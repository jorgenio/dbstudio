//! MySQL / MariaDB driver on `mysql_async` (rustls backend).

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::{NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use dbstudio_core::{
    CellValue, ColumnMeta, ConnectionConfig, DataType, Engine, Error, ExecuteRequest, ObjectKind,
    ObjectNode, ObjectRef, QueryResult, Result, TypeFamily, TlsMode,
};
use dbstudio_ssh::Tunnel;
use mysql_async::consts::ColumnType;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts, OptsBuilder, Row, SslOpts, Value};
use tokio::sync::Mutex;

use crate::helpers::{backtick_ident, data_type_from_name};
use crate::{Capabilities, DatabaseClient, TableDetail};

pub struct MysqlClient {
    conn: Mutex<Conn>,
}

pub async fn connect(
    cfg: &ConnectionConfig,
    tunnel: Option<&Tunnel>,
) -> Result<Arc<dyn DatabaseClient>> {
    let (host, port) = match tunnel {
        Some(t) => t.local_endpoint(),
        None => (cfg.host.clone(), cfg.port),
    };

    let want_tls = cfg.tls.wants_tls() && tunnel.is_none();

    let build_opts = |with_tls: bool| -> Opts {
        let mut builder = OptsBuilder::default()
            .ip_or_hostname(host.clone())
            .tcp_port(port)
            .user(Some(cfg.username.clone()))
            .pass(cfg.password.as_ref().map(|p| p.expose().to_string()));
        if !cfg.database.is_empty() {
            builder = builder.db_name(Some(cfg.database.clone()));
        }
        if with_tls {
            builder = builder.ssl_opts(Some(SslOpts::default()));
        }
        builder.into()
    };

    let conn = match Conn::new(build_opts(want_tls)).await {
        Ok(conn) => conn,
        // sslmode=prefer: handshake/cert failure → retry in cleartext.
        Err(e) if want_tls && cfg.tls == TlsMode::Prefer => {
            tracing::warn!("mysql: TLS attempt failed ({e}); retrying without TLS (mode=prefer)");
            Conn::new(build_opts(false))
                .await
                .map_err(|e| Error::Connection(format!("mysql: {e}")))?
        }
        Err(e) => return Err(Error::Connection(format!("mysql: {e}"))),
    };

    Ok(Arc::new(MysqlClient {
        conn: Mutex::new(conn),
    }))
}

#[async_trait]
impl DatabaseClient for MysqlClient {
    fn engine(&self) -> Engine {
        Engine::Mysql
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::SQL_BASIC
    }

    async fn ping(&self) -> Result<()> {
        // Conn is not Sync; use a cloned connection for the health check.
        let mut conn = self.conn.lock().await;
        conn.query_drop("SELECT 1")
            .await
            .map_err(|e| Error::Query(e.to_string()))?;
        Ok(())
    }

    async fn list_children(&self, parent: &ObjectRef) -> Result<Vec<ObjectNode>> {
        let mut conn = self.conn.lock().await;
        match parent.path.as_slice() {
            [] => {
                let names: Vec<String> = conn
                    .query("SHOW DATABASES")
                    .await
                    .map_err(|e| Error::Query(e.to_string()))?;
                Ok(names
                    .into_iter()
                    .filter(|n| !matches!(n.as_str(), "information_schema" | "performance_schema" | "mysql" | "sys"))
                    .map(|n| ObjectNode::branch(parent.child(&n), ObjectKind::Database, &n))
                    .collect())
            }
            [db] => {
                let rows: Vec<(String, String)> = conn
                    .exec(
                        "SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES \
                         WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME",
                        (db,),
                    )
                    .await
                    .map_err(|e| Error::Query(e.to_string()))?;
                Ok(rows
                    .into_iter()
                    .map(|(name, kind)| {
                        let kind = if kind.eq_ignore_ascii_case("VIEW") {
                            ObjectKind::View
                        } else {
                            ObjectKind::Table
                        };
                        ObjectNode::branch(parent.child(&name), kind, &name)
                    })
                    .collect())
            }
            [db, table] => {
                let sql = format!(
                    "SHOW COLUMNS FROM {} FROM {}",
                    backtick_ident(table),
                    backtick_ident(db)
                );
                let rows: Vec<mysql_async::Row> =
                    conn.query(sql).await.map_err(|e| Error::Query(e.to_string()))?;
                Ok(rows
                    .into_iter()
                    .map(|r| {
                        let name: String = r.get::<String, usize>(0).unwrap_or_default();
                        let type_name: String = r.get::<String, usize>(1).unwrap_or_default();
                        let nullable: String = r.get::<String, usize>(2).unwrap_or_default();
                        ObjectNode::leaf(parent.child(&name), ObjectKind::Column, &name).with_detail(
                            format!(
                                "{type_name}{}",
                                if nullable.eq_ignore_ascii_case("YES") { "" } else { " not null" }
                            ),
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
            return Err(Error::Config("table reference must be <db>.<table>".into()));
        };
        let mut conn = self.conn.lock().await;
        let sql = format!(
            "SHOW COLUMNS FROM {} FROM {}",
            backtick_ident(name),
            backtick_ident(db)
        );
        let rows: Vec<mysql_async::Row> =
            conn.query(sql).await.map_err(|e| Error::Query(e.to_string()))?;
        Ok(TableDetail {
            columns: rows
                .into_iter()
                .map(|r| ColumnMeta {
                    name: r.get::<String, usize>(0).unwrap_or_default(),
                    data_type: data_type_from_name(r.get::<String, usize>(1).unwrap_or_default()),
                    nullable: r
                        .get::<String, usize>(2)
                        .map(|s| s.eq_ignore_ascii_case("YES"))
                        .unwrap_or(true),
                    comment: r.get::<String, usize>(8),
                })
                .collect(),
        })
    }

    async fn execute(&self, req: ExecuteRequest) -> Result<QueryResult> {
        let start = Instant::now();
        let mut conn = self.conn.lock().await;
        let mut result = conn
            .query_iter(&req.sql)
            .await
            .map_err(|e| Error::query(e))?;

        let mut columns: Vec<ColumnMeta> = Vec::new();
        let mut rows: Vec<Vec<CellValue>> = Vec::new();
        let mut truncated = false;

        while let Some(row) = result.next().await.map_err(|e| Error::query(e))? {
            if columns.is_empty() {
                if let Some(cols) = result.columns() {
                    columns = cols.iter().map(mysql_column).collect();
                }
            }
            if rows.len() >= req.max_rows {
                truncated = true;
                break;
            }
            rows.push(mysql_row_values(row));
        }

        // Consume any remaining statement outputs so the connection is clean.
        let affected = result.affected_rows();
        let _ = result.drop_result().await;

        Ok(QueryResult {
            columns,
            rows,
            affected: if affected > 0 { Some(affected) } else { None },
            truncated,
            elapsed: start.elapsed(),
        })
    }
}

fn mysql_column(c: &mysql_async::Column) -> ColumnMeta {
    let family = match c.column_type() {
        ColumnType::MYSQL_TYPE_TINY
        | ColumnType::MYSQL_TYPE_SHORT
        | ColumnType::MYSQL_TYPE_LONG
        | ColumnType::MYSQL_TYPE_LONGLONG
        | ColumnType::MYSQL_TYPE_INT24 => {
            if c.flags().contains(mysql_async::consts::ColumnFlags::UNSIGNED_FLAG) {
                TypeFamily::UInt
            } else {
                TypeFamily::Int
            }
        }
        ColumnType::MYSQL_TYPE_FLOAT | ColumnType::MYSQL_TYPE_DOUBLE => TypeFamily::Float,
        ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL => TypeFamily::Decimal,
        ColumnType::MYSQL_TYPE_VARCHAR
        | ColumnType::MYSQL_TYPE_VAR_STRING
        | ColumnType::MYSQL_TYPE_STRING
        | ColumnType::MYSQL_TYPE_ENUM
        | ColumnType::MYSQL_TYPE_SET
        | ColumnType::MYSQL_TYPE_TINY_BLOB
        | ColumnType::MYSQL_TYPE_BLOB
        | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
        | ColumnType::MYSQL_TYPE_LONG_BLOB => TypeFamily::Text,
        ColumnType::MYSQL_TYPE_BIT => TypeFamily::Bytes,
        ColumnType::MYSQL_TYPE_DATE
        | ColumnType::MYSQL_TYPE_NEWDATE
        | ColumnType::MYSQL_TYPE_TIME
        | ColumnType::MYSQL_TYPE_DATETIME
        | ColumnType::MYSQL_TYPE_TIMESTAMP
        | ColumnType::MYSQL_TYPE_TIME2
        | ColumnType::MYSQL_TYPE_DATETIME2
        | ColumnType::MYSQL_TYPE_TIMESTAMP2 => TypeFamily::DateTime,
        ColumnType::MYSQL_TYPE_JSON => TypeFamily::Json,
        _ => TypeFamily::Other,
    };
    let type_name = format!("{:?}", c.column_type())
        .trim_start_matches("MYSQL_TYPE_")
        .to_string();
    ColumnMeta {
        name: c.name_str().to_string(),
        data_type: DataType::new(type_name, family),
        nullable: !c.flags().contains(mysql_async::consts::ColumnFlags::NOT_NULL_FLAG),
        comment: None,
    }
}

fn mysql_row_values(row: Row) -> Vec<CellValue> {
    row.unwrap().into_iter().map(mysql_value).collect()
}

fn mysql_value(v: Value) -> CellValue {
    match v {
        Value::NULL => CellValue::Null,
        Value::Bytes(b) => match String::from_utf8(b) {
            Ok(s) => CellValue::Text(s),
            Err(e) => CellValue::Bytes(e.into_bytes()),
        },
        Value::Int(i) => CellValue::Int(i),
        Value::UInt(u) => CellValue::UInt(u),
        Value::Float(f) => CellValue::Float(f as f64),
        Value::Double(d) => CellValue::Float(d),
        Value::Date(y, m, d, h, mi, s, us) => {
            let date = NaiveDate::from_ymd_opt(y as i32, m as u32, d as u32);
            let time = NaiveTime::from_hms_micro_opt(h as u32, mi as u32, s as u32, us);
            match (date, time) {
                (Some(date), Some(time)) => {
                    let naive = NaiveDateTime::new(date, time);
                    CellValue::DateTime(Utc.from_utc_datetime(&naive))
                }
                _ => CellValue::Text(format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")),
            }
        }
        Value::Time(neg, days, h, mi, s, us) => CellValue::Other {
            type_name: "TIME".into(),
            value: format!(
                "{sign}{days} {h:02}:{mi:02}:{s:02}.{us:06}",
                sign = if neg { "-" } else { "" }
            ),
        },
    }
}

