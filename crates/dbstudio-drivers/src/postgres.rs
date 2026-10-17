//! PostgreSQL driver on `tokio-postgres`.
//!
//! Direct connections negotiate TLS with webpki roots when the config wants
//! TLS. Over an SSH tunnel the stream is injected with `connect_raw` (no
//! local TCP hop); TLS is used only when explicitly required.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use dbstudio_core::{
    CellValue, ColumnMeta, ConnectionConfig, Engine, Error, ExecuteRequest, ObjectKind, ObjectNode,
    ObjectRef, QueryResult, Result, TypeFamily, DataType, TlsMode,
};
use dbstudio_ssh::Tunnel;
use tokio_postgres::types::Type;
use tokio_postgres::{Client, Config, NoTls, Row};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::helpers::data_type_from_name;
use crate::{Capabilities, DatabaseClient, ForeignKeyMeta, TableDetail};

pub struct PostgresClient {
    client: Client,
}

pub async fn connect(
    cfg: &ConnectionConfig,
    tunnel: Option<&Tunnel>,
) -> Result<Arc<dyn DatabaseClient>> {
    // Over an SSH tunnel we dial the loopback endpoint; TLS then runs
    // end-to-end inside the tunnel when requested.
    let (host, port) = match tunnel {
        Some(t) => t.local_endpoint(),
        None => (cfg.host.clone(), cfg.port),
    };
    let mut pg = Config::new();
    pg.host(&host).port(port).user(&cfg.username);
    if !cfg.database.is_empty() {
        pg.dbname(&cfg.database);
    }
    if let Some(password) = &cfg.password {
        pg.password(password.expose());
    }
    pg.connect_timeout(std::time::Duration::from_secs(15));

    // Strict modes must actually get TLS; prefer keeps the library default
    // (try SSL, fall back to cleartext if the server declines the request).
    if matches!(cfg.tls, TlsMode::Require | TlsMode::VerifyCa | TlsMode::VerifyFull) {
        pg.ssl_mode(tokio_postgres::config::SslMode::Require);
    }

    let client = if cfg.tls.wants_tls() {
        let tls = MakeRustlsConnect::new(rustls_config()?);
        match pg.connect(tls).await {
            Ok((client, connection)) => {
                tokio::spawn(async move {
                    let _ = connection.await;
                });
                client
            }
            // sslmode=prefer semantics: the handshake failed (typically a
            // self-signed or hostname-mismatched cert on a LAN/dev server),
            // so retry in cleartext instead of refusing the connection.
            Err(e) if cfg.tls == TlsMode::Prefer => {
                tracing::warn!("postgres: TLS handshake failed ({e}); retrying without TLS (mode=prefer)");
                let (client, connection) = pg
                    .connect(NoTls)
                    .await
                    .map_err(|e| Error::Connection(format!("postgres: {e}")))?;
                tokio::spawn(async move {
                    let _ = connection.await;
                });
                client
            }
            Err(e) => return Err(Error::Connection(format!("postgres: {e}"))),
        }
    } else {
        let (client, connection) = pg
            .connect(NoTls)
            .await
            .map_err(|e| Error::Connection(format!("postgres: {e}")))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        client
    };

    Ok(Arc::new(PostgresClient { client }))
}

fn rustls_config() -> Result<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    Ok(rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth())
}

#[async_trait]
impl DatabaseClient for PostgresClient {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            schemas: true,
            ..Capabilities::SQL_BASIC
        }
    }

    async fn ping(&self) -> Result<()> {
        self.client
            .query("SELECT 1", &[])
            .await
            .map_err(|e| Error::Query(e.to_string()))?;
        Ok(())
    }

    async fn list_children(&self, parent: &ObjectRef) -> Result<Vec<ObjectNode>> {
        match parent.path.as_slice() {
            [] => {
                let rows = self
                    .client
                    .query("SELECT current_database()", &[])
                    .await
                    .map_err(|e| Error::Query(e.to_string()))?;
                let name: String = rows[0].get(0);
                Ok(vec![ObjectNode::branch(
                    parent.child(&name),
                    ObjectKind::Database,
                    &name,
                )])
            }
            [_db] => {
                let rows = self
                    .client
                    .query(
                        "SELECT nspname FROM pg_namespace \
                         WHERE nspname NOT LIKE 'pg\\_%' AND nspname <> 'information_schema' \
                         ORDER BY nspname",
                        &[],
                    )
                    .await
                    .map_err(|e| Error::Query(e.to_string()))?;
                Ok(rows
                    .iter()
                    .map(|r| {
                        let name: String = r.get(0);
                        ObjectNode::branch(parent.child(&name), ObjectKind::Schema, &name)
                    })
                    .collect())
            }
            [_, schema] => {
                let rows = self
                    .client
                    .query(
                        "SELECT table_name, table_type FROM information_schema.tables \
                         WHERE table_schema = $1 ORDER BY table_name",
                        &[schema],
                    )
                    .await
                    .map_err(|e| Error::Query(e.to_string()))?;
                Ok(rows
                    .iter()
                    .map(|r| {
                        let name: String = r.get(0);
                        let kind: String = r.get(1);
                        let kind = if kind.eq_ignore_ascii_case("VIEW") {
                            ObjectKind::View
                        } else {
                            ObjectKind::Table
                        };
                        ObjectNode::branch(parent.child(&name), kind, &name)
                    })
                    .collect())
            }
            [_, schema, table] => {
                let rows = self
                    .client
                    .query(
                        "SELECT column_name, data_type, is_nullable \
                         FROM information_schema.columns \
                         WHERE table_schema = $1 AND table_name = $2 \
                         ORDER BY ordinal_position",
                        &[schema, table],
                    )
                    .await
                    .map_err(|e| Error::Query(e.to_string()))?;
                Ok(rows
                    .iter()
                    .map(|r| {
                        let name: String = r.get(0);
                        let data_type: String = r.get(1);
                        let nullable: String = r.get(2);
                        ObjectNode::leaf(
                            parent.child(&name),
                            ObjectKind::Column,
                            &name,
                        )
                        .with_detail(format!(
                            "{data_type}{}",
                            if nullable.eq_ignore_ascii_case("YES") {
                                ""
                            } else {
                                " not null"
                            }
                        ))
                    })
                    .collect())
            }
            _ => Ok(Vec::new()),
        }
    }

    async fn describe_table(&self, table: &ObjectRef) -> Result<TableDetail> {
        let (Some(schema), Some(name)) = (
            table.path.get(table.path.len().wrapping_sub(2)),
            table.path.last(),
        ) else {
            return Err(Error::Config(
                "table reference must be <db>.<schema>.<table>".into(),
            ));
        };
        let rows = self
            .client
            .query(
                "SELECT column_name, data_type, is_nullable \
                 FROM information_schema.columns \
                 WHERE table_schema = $1 AND table_name = $2 \
                 ORDER BY ordinal_position",
                &[schema, name],
            )
            .await
            .map_err(|e| Error::Query(e.to_string()))?;
        let columns: Vec<ColumnMeta> = rows
            .iter()
            .map(|r| ColumnMeta {
                name: r.get::<_, String>(0),
                data_type: data_type_from_name(r.get::<_, String>(1)),
                nullable: r.get::<_, String>(2).eq_ignore_ascii_case("YES"),
                comment: None,
            })
            .collect();

        // Primary key columns, in key order.
        let pk_rows = self
            .client
            .query(
                "SELECT kcu.column_name \
                 FROM information_schema.table_constraints tc \
                 JOIN information_schema.key_column_usage kcu \
                   ON kcu.constraint_name = tc.constraint_name \
                  AND kcu.table_schema = tc.table_schema \
                 WHERE tc.constraint_type = 'PRIMARY KEY' \
                   AND tc.table_schema = $1 AND tc.table_name = $2 \
                 ORDER BY kcu.ordinal_position",
                &[schema, name],
            )
            .await
            .map_err(|e| Error::Query(e.to_string()))?;
        let primary_keys: Vec<String> = pk_rows
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect();

        // Foreign keys. `position_in_unique_constraint` pairs each local
        // column with its referenced column (information_schema's
        // `constraint_column_usage` join is a cross product for composites).
        let fk_rows = self
            .client
            .query(
                "SELECT tc.constraint_name, kcu.column_name, kcu.ordinal_position, \
                        kcu2.table_schema, kcu2.table_name, kcu2.column_name, \
                        rc.delete_rule, rc.update_rule \
                 FROM information_schema.table_constraints tc \
                 JOIN information_schema.key_column_usage kcu \
                   ON kcu.constraint_name = tc.constraint_name \
                  AND kcu.table_schema = tc.table_schema \
                 JOIN information_schema.key_column_usage kcu2 \
                   ON kcu2.constraint_name = kcu.unique_constraint_name \
                  AND kcu2.constraint_schema = kcu.unique_constraint_schema \
                  AND kcu2.ordinal_position = kcu.position_in_unique_constraint \
                 JOIN information_schema.referential_constraints rc \
                   ON rc.constraint_name = tc.constraint_name \
                  AND rc.constraint_schema = tc.table_schema \
                 WHERE tc.constraint_type = 'FOREIGN KEY' \
                   AND tc.table_schema = $1 AND tc.table_name = $2 \
                 ORDER BY tc.constraint_name, kcu.ordinal_position",
                &[schema, name],
            )
            .await
            .map_err(|e| Error::Query(e.to_string()))?;
        let mut foreign_keys: Vec<ForeignKeyMeta> = Vec::new();
        for r in &fk_rows {
            let constraint: String = r.get(0);
            let column: String = r.get(1);
            let ref_table: String = r.get(4);
            let ref_column: String = r.get(5);
            let on_delete: String = r.get(6);
            let on_update: String = r.get(7);
            match foreign_keys
                .iter_mut()
                .find(|fk| fk.name.as_deref() == Some(constraint.as_str()))
            {
                Some(fk) => {
                    fk.columns.push(column);
                    fk.ref_columns.push(ref_column);
                }
                None => foreign_keys.push(ForeignKeyMeta {
                    name: Some(constraint),
                    columns: vec![column],
                    ref_table,
                    ref_columns: vec![ref_column],
                    on_delete: Some(on_delete),
                    on_update: Some(on_update),
                }),
            }
        }

        Ok(TableDetail {
            columns,
            primary_keys,
            foreign_keys,
        })
    }

    async fn execute(&self, req: ExecuteRequest) -> Result<QueryResult> {
        let start = Instant::now();
        match self.client.query(&req.sql, &[]).await {
            Ok(rows) => {
                let columns: Vec<ColumnMeta> = rows
                    .first()
                    .map(|row| {
                        row.columns()
                            .iter()
                            .map(|c| ColumnMeta {
                                name: c.name().to_string(),
                                data_type: pg_type(c.type_()),
                                nullable: true,
                                comment: None,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let mut out = Vec::new();
                let mut truncated = false;
                for row in &rows {
                    if out.len() >= req.max_rows {
                        truncated = true;
                        break;
                    }
                    out.push(pg_row_values(row));
                }
                Ok(QueryResult {
                    columns,
                    rows: out,
                    affected: None,
                    truncated,
                    elapsed: start.elapsed(),
                })
            }
            Err(e) if is_multi_statement_error(&e) => {
                let msgs = self
                    .client
                    .simple_query(&req.sql)
                    .await
                    .map_err(|e| Error::query(e))?;
                let mut columns: Vec<ColumnMeta> = Vec::new();
                let mut rows: Vec<Vec<CellValue>> = Vec::new();
                let mut affected: u64 = 0;
                let mut truncated = false;
                for msg in msgs {
                    match msg {
                        tokio_postgres::SimpleQueryMessage::Row(row) => {
                            if columns.is_empty() {
                                columns = (0..row.len())
                                    .map(|i| ColumnMeta {
                                        name: row.columns()[i].name().to_string(),
                                        data_type: DataType::new("text", TypeFamily::Text),
                                        nullable: true,
                                        comment: None,
                                    })
                                    .collect();
                            }
                            if rows.len() >= req.max_rows {
                                truncated = true;
                                continue;
                            }
                            rows.push(
                                (0..row.len())
                                    .map(|i| match row.get(i) {
                                        Some(s) => CellValue::Text(s.to_string()),
                                        None => CellValue::Null,
                                    })
                                    .collect(),
                            );
                        }
                        tokio_postgres::SimpleQueryMessage::CommandComplete(n) => affected += n,
                        _ => {}
                    }
                }
                Ok(QueryResult {
                    columns,
                    rows,
                    affected: if affected > 0 { Some(affected) } else { None },
                    truncated,
                    elapsed: start.elapsed(),
                })
            }
            Err(e) => Err(Error::query(e)),
        }
    }
}

fn is_multi_statement_error(e: &tokio_postgres::Error) -> bool {
    e.code()
        .map(|c| c.code() == "42601")
        .unwrap_or(false)
        || e.to_string()
            .contains("cannot insert multiple commands into a prepared statement")
}

fn pg_type(ty: &Type) -> DataType {
    let family = match *ty {
        Type::BOOL => TypeFamily::Bool,
        Type::INT2 | Type::INT4 => TypeFamily::Int,
        Type::INT8 => TypeFamily::Int,
        Type::FLOAT4 | Type::FLOAT8 => TypeFamily::Float,
        Type::NUMERIC => TypeFamily::Decimal,
        Type::TEXT
        | Type::VARCHAR
        | Type::BPCHAR
        | Type::NAME
        | Type::UUID
        | Type::CHAR
        | Type::XML => TypeFamily::Text,
        Type::BYTEA => TypeFamily::Bytes,
        Type::TIMESTAMP | Type::TIMESTAMPTZ | Type::DATE | Type::TIME => TypeFamily::DateTime,
        Type::JSON | Type::JSONB => TypeFamily::Json,
        _ => TypeFamily::Other,
    };
    DataType::new(ty.name(), family)
}

fn pg_row_values(row: &Row) -> Vec<CellValue> {
    row.columns()
        .iter()
        .enumerate()
        .map(|(i, col)| pg_value(row, i, col.type_()))
        .collect()
}

fn pg_value(row: &Row, i: usize, ty: &Type) -> CellValue {
    macro_rules! opt {
        ($t:ty) => {
            if let Ok(v) = row.try_get::<_, Option<$t>>(i) {
                return match v {
                    Some(v) => v.into(),
                    None => CellValue::Null,
                };
            }
        };
    }

    match *ty {
        Type::BOOL => opt!(bool),
        Type::INT2 | Type::INT4 => {
            if let Ok(v) = row.try_get::<_, Option<i32>>(i) {
                return match v {
                    Some(v) => CellValue::Int(v as i64),
                    None => CellValue::Null,
                };
            }
        }
        Type::INT8 => opt!(i64),
        Type::FLOAT4 => {
            if let Ok(v) = row.try_get::<_, Option<f32>>(i) {
                return match v {
                    Some(v) => CellValue::Float(v as f64),
                    None => CellValue::Null,
                };
            }
        }
        Type::FLOAT8 => opt!(f64),
        Type::NUMERIC => {
            if let Ok(v) = row.try_get::<_, Option<rust_decimal::Decimal>>(i) {
                return match v {
                    Some(v) => CellValue::Text(v.to_string()),
                    None => CellValue::Null,
                };
            }
        }
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UUID | Type::CHAR => {
            opt!(String)
        }
        Type::BYTEA => opt!(Vec<u8>),
        Type::TIMESTAMP => {
            if let Ok(v) = row.try_get::<_, Option<NaiveDateTime>>(i) {
                return match v {
                    Some(v) => CellValue::DateTime(Utc.from_utc_datetime(&v)),
                    None => CellValue::Null,
                };
            }
        }
        Type::TIMESTAMPTZ => opt!(DateTime<Utc>),
        Type::JSON | Type::JSONB => opt!(serde_json::Value),
        _ => {}
    }

    // Fallbacks for anything not covered above.
    if let Ok(v) = row.try_get::<_, Option<String>>(i) {
        return match v {
            Some(v) => CellValue::Text(v),
            None => CellValue::Null,
        };
    }
    if let Ok(v) = row.try_get::<_, Option<serde_json::Value>>(i) {
        return match v {
            Some(v) => CellValue::Json(v),
            None => CellValue::Null,
        };
    }
    CellValue::Other {
        type_name: ty.name().to_string(),
        value: String::new(),
    }
}
