//! SQL Server driver on `tiberius`.
//!
//! TLS uses the platform default (`native-tls`); over an SSH tunnel the
//! connection is plaintext inside the encrypted tunnel unless encryption is
//! demanded by the server.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use dbstudio_core::{
    CellValue, ColumnMeta, ConnectionConfig, DataType, Engine, Error, ExecuteRequest, ObjectKind,
    ObjectNode, ObjectRef, QueryResult, Result, TypeFamily,
};
use dbstudio_ssh::Tunnel;
use futures::StreamExt;
use tiberius::{AuthMethod, QueryItem};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_util::compat::TokioAsyncWriteCompatExt;

use crate::helpers::{data_type_from_name, sql_literal};
use crate::{Capabilities, DatabaseClient, ForeignKeyMeta, TableDetail};

type TiberiusClient = tiberius::Client<tokio_util::compat::Compat<TcpStream>>;

pub struct MssqlClient {
    client: Mutex<TiberiusClient>,
}

pub async fn connect(
    cfg: &ConnectionConfig,
    tunnel: Option<&Tunnel>,
) -> Result<Arc<dyn DatabaseClient>> {
    let mut config = tiberius::Config::new();
    config.host(&cfg.host);
    config.port(cfg.port);
    if !cfg.database.is_empty() {
        config.database(&cfg.database);
    }
    config.authentication(AuthMethod::sql_server(
        cfg.username.clone(),
        cfg.password
            .as_ref()
            .map(|p| p.expose().to_string())
            .unwrap_or_default(),
    ));
    // Self-signed certs are common behind tunnels; trust them for MVP.
    config.trust_cert();

    let stream = match tunnel {
        Some(t) => {
            let (_, port) = t.local_endpoint();
            TcpStream::connect(("127.0.0.1", port))
                .await
                .map_err(|e| Error::Connection(format!("mssql (over SSH): {e}")))?
        }
        None => TcpStream::connect((cfg.host.as_str(), cfg.port))
            .await
            .map_err(|e| Error::Connection(format!("mssql: {e}")))?,
    };

    let client = tiberius::Client::connect(config, stream.compat_write())
        .await
        .map_err(|e| Error::Connection(format!("mssql: {e}")))?;

    Ok(Arc::new(MssqlClient {
        client: Mutex::new(client),
    }))
}

#[async_trait]
impl DatabaseClient for MssqlClient {
    fn engine(&self) -> Engine {
        Engine::SqlServer
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::SQL_BASIC
    }

    async fn ping(&self) -> Result<()> {
        let mut client = self.client.lock().await;
        client
            .query("SELECT 1", &[])
            .await
            .map_err(|e| Error::Query(e.to_string()))?
            .into_first_result()
            .await
            .map_err(|e| Error::Query(e.to_string()))?;
        Ok(())
    }

    async fn list_children(&self, parent: &ObjectRef) -> Result<Vec<ObjectNode>> {
        let mut client = self.client.lock().await;
        match parent.path.as_slice() {
            [] => {
                let rows = client
                    .query("SELECT name FROM sys.databases ORDER BY name", &[])
                    .await
                    .map_err(|e| Error::Query(e.to_string()))?
                    .into_first_result()
                    .await
                    .map_err(|e| Error::Query(e.to_string()))?;
                Ok(rows
                    .iter()
                    .filter_map(|r| r.try_get::<&str, usize>(0).ok().flatten())
                    .map(|n| ObjectNode::branch(parent.child(n), ObjectKind::Database, n))
                    .collect())
            }
            [db] => {
                let sql = format!(
                    "SELECT TABLE_NAME, TABLE_TYPE FROM {db}.INFORMATION_SCHEMA.TABLES ORDER BY TABLE_NAME"
                );
                let rows = client
                    .query(sql, &[])
                    .await
                    .map_err(|e| Error::Query(e.to_string()))?
                    .into_first_result()
                    .await
                    .map_err(|e| Error::Query(e.to_string()))?;
                Ok(rows
                    .iter()
                    .filter_map(|r| {
                        let name = r.try_get::<&str, usize>(0).ok().flatten()?;
                        let kind = r.try_get::<&str, usize>(1).ok().flatten()?;
                        let kind = if kind.eq_ignore_ascii_case("VIEW") {
                            ObjectKind::View
                        } else {
                            ObjectKind::Table
                        };
                        Some(ObjectNode::branch(parent.child(name), kind, name))
                    })
                    .collect())
            }
            [db, table] => {
                let sql = format!(
                    "SELECT COLUMN_NAME, DATA_TYPE, IS_NULLABLE FROM {db}.INFORMATION_SCHEMA.COLUMNS WHERE TABLE_NAME = '{table}' ORDER BY ORDINAL_POSITION"
                );
                let rows = client
                    .query(sql, &[])
                    .await
                    .map_err(|e| Error::Query(e.to_string()))?
                    .into_first_result()
                    .await
                    .map_err(|e| Error::Query(e.to_string()))?;
                Ok(rows
                    .iter()
                    .filter_map(|r| {
                        let name = r.try_get::<&str, usize>(0).ok().flatten()?;
                        let type_name = r.try_get::<&str, usize>(1).ok().flatten()?;
                        let nullable = r.try_get::<&str, usize>(2).ok().flatten()?;
                        Some(
                            ObjectNode::leaf(parent.child(name), ObjectKind::Column, name)
                                .with_detail(format!(
                                    "{type_name}{}",
                                    if nullable.eq_ignore_ascii_case("YES") {
                                        ""
                                    } else {
                                        " not null"
                                    }
                                )),
                        )
                    })
                    .collect())
            }
            _ => Ok(Vec::new()),
        }
    }

    async fn describe_table(&self, table: &ObjectRef) -> Result<TableDetail> {
        let (Some(db), Some(name)) = (
            table.path.get(table.path.len().wrapping_sub(2)),
            table.path.last(),
        ) else {
            return Err(Error::Config(
                "table reference must be <db>.<schema-less>.<table>".into(),
            ));
        };
        let mut client = self.client.lock().await;
        let sql = format!(
            "SELECT COLUMN_NAME, DATA_TYPE, IS_NULLABLE FROM {db}.INFORMATION_SCHEMA.COLUMNS WHERE TABLE_NAME = '{name}' ORDER BY ORDINAL_POSITION"
        );
        let rows = client
            .query(sql, &[])
            .await
            .map_err(|e| Error::Query(e.to_string()))?
            .into_first_result()
            .await
            .map_err(|e| Error::Query(e.to_string()))?;
        let columns: Vec<ColumnMeta> = rows
            .iter()
            .filter_map(|r| {
                Some(ColumnMeta {
                    name: r.try_get::<&str, usize>(0).ok().flatten()?.to_string(),
                    data_type: data_type_from_name(
                        r.try_get::<&str, usize>(1).ok().flatten()?,
                    ),
                    nullable: r
                        .try_get::<&str, usize>(2)
                        .ok()
                        .flatten()
                        .map(|s| s.eq_ignore_ascii_case("YES"))
                        .unwrap_or(true),
                    comment: None,
                })
            })
            .collect();

        // Primary key columns in key order.
        let nq = sql_literal(name);
        let pk_sql = format!(
            "SELECT c.name FROM {db}.sys.indexes i \
             JOIN {db}.sys.index_columns ic \
               ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
             JOIN {db}.sys.columns c \
               ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
             JOIN {db}.sys.tables t ON t.object_id = i.object_id AND t.name = {nq} \
             WHERE i.is_primary_key = 1 \
             ORDER BY ic.key_ordinal"
        );
        let pk_rows = client
            .query(pk_sql, &[])
            .await
            .map_err(|e| Error::Query(e.to_string()))?
            .into_first_result()
            .await
            .map_err(|e| Error::Query(e.to_string()))?;
        let primary_keys: Vec<String> = pk_rows
            .iter()
            .filter_map(|r| r.try_get::<&str, usize>(0).ok().flatten().map(String::from))
            .collect();

        // Foreign keys: one row per column, ordered by constraint + column.
        let fk_sql = format!(
            "SELECT fk.name, cp.name, rp.name, rt.name, \
                    fk.delete_referential_action_desc, \
                    fk.update_referential_action_desc \
             FROM {db}.sys.foreign_keys fk \
             JOIN {db}.sys.foreign_key_columns fkc \
               ON fkc.constraint_object_id = fk.object_id \
             JOIN {db}.sys.tables t \
               ON t.object_id = fk.parent_object_id AND t.name = {nq} \
             JOIN {db}.sys.columns cp \
               ON cp.object_id = fkc.parent_object_id \
              AND cp.column_id = fkc.parent_column_id \
             JOIN {db}.sys.columns rp \
               ON rp.object_id = fkc.referenced_object_id \
              AND rp.column_id = fkc.referenced_column_id \
             JOIN {db}.sys.tables rt ON rt.object_id = fk.referenced_object_id \
             ORDER BY fk.name, fkc.constraint_column_id"
        );
        let fk_rows = client
            .query(fk_sql, &[])
            .await
            .map_err(|e| Error::Query(e.to_string()))?
            .into_first_result()
            .await
            .map_err(|e| Error::Query(e.to_string()))?;
        let mut foreign_keys: Vec<ForeignKeyMeta> = Vec::new();
        for r in &fk_rows {
            let Some(constraint) = r.try_get::<&str, usize>(0).ok().flatten() else {
                continue;
            };
            let from: String =
                r.try_get::<&str, usize>(1).ok().flatten().unwrap_or_default().into();
            let ref_col: String =
                r.try_get::<&str, usize>(2).ok().flatten().unwrap_or_default().into();
            let ref_table: String =
                r.try_get::<&str, usize>(3).ok().flatten().unwrap_or_default().into();
            let delete: String = r
                .try_get::<&str, usize>(4)
                .ok()
                .flatten()
                .unwrap_or_default()
                .replace('_', " ");
            let update: String = r
                .try_get::<&str, usize>(5)
                .ok()
                .flatten()
                .unwrap_or_default()
                .replace('_', " ");
            match foreign_keys
                .iter_mut()
                .find(|fk| fk.name.as_deref() == Some(constraint))
            {
                Some(fk) => {
                    fk.columns.push(from);
                    fk.ref_columns.push(ref_col);
                }
                None => foreign_keys.push(ForeignKeyMeta {
                    name: Some(constraint.into()),
                    columns: vec![from],
                    ref_table,
                    ref_columns: vec![ref_col],
                    on_delete: Some(delete),
                    on_update: Some(update),
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
        let mut client = self.client.lock().await;
        let mut stream = client
            .query(&req.sql, &[])
            .await
            .map_err(|e| Error::query(e))?;

        let mut columns: Vec<ColumnMeta> = Vec::new();
        let mut rows: Vec<Vec<CellValue>> = Vec::new();
        let mut truncated = false;

        loop {
            match stream.next().await {
                Some(Ok(QueryItem::Row(row))) => {
                    if columns.is_empty() {
                        columns = row
                            .columns()
                            .iter()
                            .map(|c| ColumnMeta {
                                name: c.name().to_string(),
                                data_type: DataType::new(
                                    format!("{:?}", c.column_type()),
                                    TypeFamily::Other,
                                ),
                                nullable: true,
                                comment: None,
                            })
                            .collect();
                    }
                    if rows.len() >= req.max_rows {
                        truncated = true;
                    } else {
                        rows.push(
                            (0..row.columns().len())
                                .map(|i| mssql_value(&row, i))
                                .collect(),
                        );
                    }
                }
                Some(Ok(QueryItem::Metadata(_))) => {}
                Some(Err(e)) => return Err(Error::query(e)),
                None => break,
            }
        }

        Ok(QueryResult {
            columns,
            rows,
            affected: None,
            truncated,
            elapsed: start.elapsed(),
        })
    }
}

fn mssql_value(row: &tiberius::Row, i: usize) -> CellValue {
    macro_rules! try_get {
        ($t:ty, $conv:expr) => {
            match row.try_get::<$t, usize>(i) {
                Ok(Some(v)) => return $conv(v),
                Ok(None) => return CellValue::Null,
                Err(_) => {}
            }
        };
    }
    try_get!(bool, CellValue::Bool);
    try_get!(i32, |v| CellValue::Int(v as i64));
    try_get!(i64, CellValue::Int);
    try_get!(f32, |v| CellValue::Float(v as f64));
    try_get!(f64, CellValue::Float);
    try_get!(&str, |v: &str| CellValue::Text(v.to_string()));
    try_get!(&[u8], |v: &[u8]| CellValue::Bytes(v.to_vec()));
    CellValue::Other {
        type_name: "mssql".into(),
        value: String::new(),
    }
}
