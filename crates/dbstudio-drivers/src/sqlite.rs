//! SQLite driver on `rusqlite` (bundled, no system library needed).
//!
//! `ConnectionConfig::host` holds the file path (`:memory:` works too).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dbstudio_core::{
    CellValue, ColumnMeta, ConnectionConfig, Engine, Error, ExecuteRequest, ObjectKind, ObjectNode,
    ObjectRef, QueryResult, Result,
};
use rusqlite::types::ValueRef;
use rusqlite::Connection;

use crate::helpers::data_type_from_name;
use crate::{Capabilities, DatabaseClient, ForeignKeyMeta, TableDetail};

pub struct SqliteClient {
    conn: Mutex<Connection>,
    file_label: String,
}

pub async fn connect(cfg: &ConnectionConfig) -> Result<Arc<dyn DatabaseClient>> {
    let path = if cfg.host.is_empty() { ":memory:" } else { &cfg.host };
    let conn = Connection::open(path).map_err(|e| Error::Connection(format!("sqlite: {e}")))?;
    let file_label = if path == ":memory:" {
        ":memory:".to_string()
    } else {
        std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string())
    };
    Ok(Arc::new(SqliteClient {
        conn: Mutex::new(conn),
        file_label,
    }))
}

fn lock(conn: &Mutex<Connection>) -> Result<std::sync::MutexGuard<'_, Connection>> {
    conn.lock()
        .map_err(|_| Error::Other("sqlite connection mutex poisoned".into()))
}

#[async_trait]
impl DatabaseClient for SqliteClient {
    fn engine(&self) -> Engine {
        Engine::Sqlite
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::SQL_BASIC
    }

    async fn ping(&self) -> Result<()> {
        let conn = lock(&self.conn)?;
        conn.query_row("SELECT 1", [], |_| Ok(()))
            .map_err(|e| Error::Query(e.to_string()))
    }

    async fn list_children(&self, parent: &ObjectRef) -> Result<Vec<ObjectNode>> {
        let conn = lock(&self.conn)?;
        match parent.path.as_slice() {
            [] => Ok(vec![ObjectNode::branch(
                parent.child(&self.file_label),
                ObjectKind::Database,
                &self.file_label,
            )]),
            [_] => {
                let mut stmt = conn
                    .prepare(
                        "SELECT name, type FROM sqlite_master \
                         WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%' \
                         ORDER BY name",
                    )
                    .map_err(|e| Error::Query(e.to_string()))?;
                let rows = stmt
                    .query_map([], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })
                    .map_err(|e| Error::Query(e.to_string()))?;
                let mut out = Vec::new();
                for row in rows {
                    let (name, kind) = row.map_err(|e| Error::Query(e.to_string()))?;
                    let kind = if kind == "view" {
                        ObjectKind::View
                    } else {
                        ObjectKind::Table
                    };
                    out.push(ObjectNode::branch(parent.child(&name), kind, &name));
                }
                Ok(out)
            }
            [_, table] => {
                let mut stmt = conn
                    .prepare(&format!("PRAGMA table_info({})", quote_ident(table)))
                    .map_err(|e| Error::Query(e.to_string()))?;
                let rows = stmt
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(3)?,
                        ))
                    })
                    .map_err(|e| Error::Query(e.to_string()))?;
                let mut out = Vec::new();
                for row in rows {
                    let (name, type_name, notnull) =
                        row.map_err(|e| Error::Query(e.to_string()))?;
                    out.push(
                        ObjectNode::leaf(parent.child(&name), ObjectKind::Column, &name).with_detail(
                            format!(
                                "{type_name}{}",
                                if notnull != 0 { " not null" } else { "" }
                            ),
                        ),
                    );
                }
                Ok(out)
            }
            _ => Ok(Vec::new()),
        }
    }

    async fn describe_table(&self, table: &ObjectRef) -> Result<TableDetail> {
        let Some(name) = table.path.last() else {
            return Err(Error::Config("table reference must be <file>.<table>".into()));
        };
        let conn = lock(&self.conn)?;
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({})", quote_ident(name)))
            .map_err(|e| Error::Query(e.to_string()))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    ColumnMeta {
                        name: r.get(1)?,
                        data_type: data_type_from_name(r.get::<_, String>(2)?),
                        nullable: r.get::<_, i64>(3)? == 0,
                        comment: None,
                    },
                    r.get::<_, i64>(5)? != 0, // pk flag (position in PK)
                ))
            })
            .map_err(|e| Error::Query(e.to_string()))?;
        let mut columns = Vec::new();
        let mut primary_keys = Vec::new();
        for row in rows {
            let (col, pk) = row.map_err(|e| Error::Query(e.to_string()))?;
            if pk {
                primary_keys.push(col.name.clone());
            }
            columns.push(col);
        }

        // PRAGMA foreign_key_list: rows grouped by `id`, one row per column.
        // The pragma exposes no constraint name → keep `name: None`.
        let mut stmt = conn
            .prepare(&format!("PRAGMA foreign_key_list({})", quote_ident(name)))
            .map_err(|e| Error::Query(e.to_string()))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,    // id
                    r.get::<_, i64>(1)?,    // seq
                    r.get::<_, String>(2)?, // parent table
                    r.get::<_, String>(3)?, // from (local column)
                    r.get::<_, Option<String>>(4)?, // to (ref column)
                    r.get::<_, String>(5)?, // on_update
                    r.get::<_, String>(6)?, // on_delete
                ))
            })
            .map_err(|e| Error::Query(e.to_string()))?;
        let mut grouped: Vec<(i64, ForeignKeyMeta)> = Vec::new();
        for row in rows {
            let (id, _seq, ref_table, from, to, on_update, on_delete) =
                row.map_err(|e| Error::Query(e.to_string()))?;
            match grouped.iter_mut().find(|(gid, _)| *gid == id) {
                Some((_, fk)) => {
                    fk.columns.push(from);
                    if let Some(to) = to {
                        fk.ref_columns.push(to);
                    }
                }
                None => grouped.push((
                    id,
                    ForeignKeyMeta {
                        name: None,
                        columns: vec![from],
                        ref_table,
                        ref_columns: to.into_iter().collect(),
                        on_delete: Some(on_delete),
                        on_update: Some(on_update),
                    },
                )),
            }
        }
        let foreign_keys = grouped.into_iter().map(|(_, fk)| fk).collect();
        Ok(TableDetail {
            columns,
            primary_keys,
            foreign_keys,
        })
    }

    async fn execute(&self, req: ExecuteRequest) -> Result<QueryResult> {
        let start = std::time::Instant::now();
        let conn = lock(&self.conn)?;

        let mut stmt = match conn.prepare(&req.sql) {
            Ok(stmt) => stmt,
            Err(e) => {
                // Maybe a script of DML statements without result sets.
                return match conn.execute_batch(&req.sql) {
                    Ok(()) => Ok(QueryResult::statement(conn.changes(), start.elapsed())),
                    Err(_) => Err(Error::query(e.to_string())),
                };
            }
        };

        if stmt.column_count() == 0 {
            let affected = stmt.execute([]).map_err(|e| Error::query(e))?;
            return Ok(QueryResult::statement(affected as u64, start.elapsed()));
        }

        let columns: Vec<ColumnMeta> = stmt
            .columns()
            .iter()
            .map(|c| ColumnMeta {
                name: c.name().to_string(),
                data_type: data_type_from_name(c.decl_type().unwrap_or("text")),
                nullable: true,
                comment: None,
            })
            .collect();

        let mut rows_stmt = stmt.query([]).map_err(|e| Error::query(e))?;
        let width = columns.len();
        let mut rows = Vec::new();
        let mut truncated = false;
        while let Some(row) = rows_stmt.next().map_err(|e| Error::query(e))? {
            if rows.len() >= req.max_rows {
                truncated = true;
                break;
            }
            let mut cells = Vec::with_capacity(width);
            for i in 0..width {
                cells.push(sqlite_value(row.get_ref(i).map_err(|e| Error::query(e))?));
            }
            rows.push(cells);
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

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn sqlite_value(v: ValueRef<'_>) -> CellValue {
    match v {
        ValueRef::Null => CellValue::Null,
        ValueRef::Integer(i) => CellValue::Int(i),
        ValueRef::Real(f) => CellValue::Float(f),
        ValueRef::Text(t) => CellValue::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => CellValue::Bytes(b.to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> SqliteClient {
        let conn = Connection::open_in_memory().expect(":memory:");
        conn.execute_batch(
            "CREATE TABLE clientes (id INTEGER PRIMARY KEY, nombre TEXT NOT NULL);
             CREATE TABLE pedidos (
                 id INTEGER PRIMARY KEY,
                 cliente_id INTEGER NOT NULL,
                 ord_no INTEGER,
                 fecha TEXT,
                 FOREIGN KEY (cliente_id) REFERENCES clientes(id)
                     ON DELETE CASCADE ON UPDATE SET NULL
             );
             CREATE TABLE lineas (
                 id INTEGER PRIMARY KEY,
                 pedido_id INTEGER NOT NULL,
                 ord INTEGER NOT NULL,
                 producto TEXT,
                 FOREIGN KEY (pedido_id, ord) REFERENCES pedidos(id, ord_no)
             );",
        )
        .expect("schema");
        SqliteClient {
            conn: Mutex::new(conn),
            file_label: "test.db".into(),
        }
    }

    fn obj(path: &[&str]) -> ObjectRef {
        let mut r = ObjectRef::root(Default::default());
        for seg in path {
            r = r.child(*seg);
        }
        r
    }

    #[tokio::test]
    async fn describe_reports_columns_primary_key_and_foreign_keys() {
        let c = client();
        let detail = c
            .describe_table(&obj(&["test.db", "pedidos"]))
            .await
            .expect("describe");
        assert_eq!(
            detail.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["id", "cliente_id", "ord_no", "fecha"]
        );
        assert_eq!(detail.primary_keys, vec!["id"]);

        assert_eq!(detail.foreign_keys.len(), 1);
        let fk = &detail.foreign_keys[0];
        assert_eq!(fk.columns, vec!["cliente_id"]);
        assert_eq!(fk.ref_table, "clientes");
        assert_eq!(fk.ref_columns, vec!["id"]);
        assert_eq!(fk.on_delete.as_deref(), Some("CASCADE"));
        assert_eq!(fk.on_update.as_deref(), Some("SET NULL"));
    }

    #[tokio::test]
    async fn describe_groups_multi_column_foreign_keys() {
        let c = client();
        let detail = c
            .describe_table(&obj(&["test.db", "lineas"]))
            .await
            .expect("describe");
        assert_eq!(detail.foreign_keys.len(), 1, "one constraint, two rows");
        let fk = &detail.foreign_keys[0];
        assert_eq!(fk.columns, vec!["pedido_id", "ord"]);
        assert_eq!(fk.ref_columns, vec!["id", "ord_no"]);
        assert_eq!(fk.ref_table, "pedidos");
        assert_eq!(fk.name, None, "PRAGMA exposes no constraint name");
    }

    #[tokio::test]
    async fn describe_table_without_constraints_is_empty() {
        let c = client();
        let detail = c
            .describe_table(&obj(&["test.db", "clientes"]))
            .await
            .expect("describe");
        assert_eq!(detail.primary_keys, vec!["id"]);
        assert!(detail.foreign_keys.is_empty());
    }
}
