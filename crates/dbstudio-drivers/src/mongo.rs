//! MongoDB driver on the official `mongodb` crate.
//!
//! The "editor" input is Extended JSON:
//! - `{"find": "coll", "filter": {...}, "limit": n, "sort": {...}}`
//! - `{"aggregate": "coll", "pipeline": [...]}`
//! - a bare `{...}` filter (or `{ "aggregate": ... }`-less pipeline via the
//!   command form) applied to the collection in `ExecuteRequest::context`
//!   (`"db.collection"` or just `"collection"`).

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use dbstudio_core::{
    CellValue, ColumnMeta, ConnectionConfig, DataType, Engine, Error, ExecuteRequest, ObjectKind,
    ObjectNode, ObjectRef, QueryResult, Result, TypeFamily,
};
use dbstudio_ssh::Tunnel;
use futures::StreamExt;
use mongodb::bson::{self, Bson, Document};
use mongodb::options::{AggregateOptions, FindOptions};
use mongodb::{Client, Database};

use crate::{Capabilities, DatabaseClient, TableDetail};

pub struct MongoClient {
    client: Client,
    database: String,
}

pub async fn connect(
    cfg: &ConnectionConfig,
    tunnel: Option<&Tunnel>,
) -> Result<Arc<dyn DatabaseClient>> {
    let (host, port) = match tunnel {
        Some(t) => t.local_endpoint(),
        None => (cfg.host.clone(), cfg.port),
    };

    let mut uri = String::from("mongodb://");
    if !cfg.username.is_empty() {
        uri.push_str(&url_encode(&cfg.username));
        if let Some(password) = &cfg.password {
            uri.push(':');
            uri.push_str(&url_encode(password.expose()));
        }
        uri.push('@');
    }
    uri.push_str(&host);
    uri.push(':');
    uri.push_str(&port.to_string());
    uri.push('/');
    uri.push_str(&cfg.database);
    if cfg.tls.wants_tls() && tunnel.is_none() {
        uri.push_str("?tls=true");
    }

    let client = Client::with_uri_str(&uri)
        .await
        .map_err(|e| Error::Connection(format!("mongodb: {e}")))?;

    Ok(Arc::new(MongoClient {
        client,
        database: cfg.database.clone(),
    }))
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl MongoClient {
    fn db(&self, name: Option<&str>) -> Database {
        self.client.database(name.unwrap_or(&self.database))
    }
}

#[async_trait]
impl DatabaseClient for MongoClient {
    fn engine(&self) -> Engine {
        Engine::Mongodb
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            sql: false,
            explain: false,
            transactions: false,
            schemas: false,
        }
    }

    async fn ping(&self) -> Result<()> {
        self.client
            .database("admin")
            .run_command(bson::doc! { "ping": 1 })
            .await
            .map_err(|e| Error::Query(e.to_string()))?;
        Ok(())
    }

    async fn list_children(&self, parent: &ObjectRef) -> Result<Vec<ObjectNode>> {
        match parent.path.as_slice() {
            [] => {
                let names = self
                    .client
                    .list_database_names()
                    .await
                    .map_err(|e| Error::Query(e.to_string()))?;
                Ok(names
                    .into_iter()
                    .map(|n| ObjectNode::branch(parent.child(&n), ObjectKind::Database, &n))
                    .collect())
            }
            [db] => {
                let names = self
                    .db(Some(db))
                    .list_collection_names()
                    .await
                    .map_err(|e| Error::Query(e.to_string()))?;
                Ok(names
                    .into_iter()
                    .map(|n| ObjectNode::branch(parent.child(&n), ObjectKind::Collection, &n))
                    .collect())
            }
            _ => Ok(Vec::new()),
        }
    }

    async fn describe_table(&self, _table: &ObjectRef) -> Result<TableDetail> {
        Ok(TableDetail::default())
    }

    async fn execute(&self, req: ExecuteRequest) -> Result<QueryResult> {
        let start = Instant::now();
        let text = req.sql.trim();
        let context = split_context(req.context.as_deref())?;

        if text.starts_with('{') {
            let cmd: serde_json::Value = serde_json::from_str(text)
                .map_err(|e| Error::query(format!("invalid Extended JSON: {e}")))?;
            let Some(obj) = cmd.as_object() else {
                return Err(Error::query("query must be a JSON object"));
            };

            if let Some(coll) = obj.get("find").and_then(|v| v.as_str()) {
                let filter = obj
                    .get("filter")
                    .cloned()
                    .unwrap_or(serde_json::Value::Object(Default::default()));
                let limit = obj.get("limit").and_then(|v| v.as_u64());
                let sort = obj.get("sort").cloned();
                return run_find(&self.db(context.db), coll, filter, limit, sort, req.max_rows, start)
                    .await;
            }

            if let Some(coll) = obj.get("aggregate").and_then(|v| v.as_str()) {
                let pipeline = obj
                    .get("pipeline")
                    .cloned()
                    .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
                return run_aggregate(&self.db(context.db), coll, pipeline, req.max_rows, start).await;
            }

            let coll = context
                .collection
                .ok_or_else(|| Error::query("select a collection or use {\"find\": ...}"))?;
            return run_find(&self.db(context.db), &coll, cmd, None, None, req.max_rows, start).await;
        }

        let coll = context
            .collection
            .ok_or_else(|| Error::query("select a collection or use {\"find\": ...}"))?;
        let filter: serde_json::Value = serde_json::from_str(text)
            .map_err(|e| Error::query(format!("filter must be Extended JSON: {e}")))?;
        run_find(&self.db(context.db), &coll, filter, None, None, req.max_rows, start).await
    }
}

struct MongoContext<'a> {
    db: Option<&'a str>,
    collection: Option<String>,
}

fn split_context<'a>(context: Option<&'a str>) -> Result<MongoContext<'a>> {
    Ok(match context {
        None => MongoContext {
            db: None,
            collection: None,
        },
        Some(ctx) => match ctx.split_once('.') {
            Some((db, coll)) => MongoContext {
                db: Some(db),
                collection: Some(coll.to_string()),
            },
            None => MongoContext {
                db: None,
                collection: Some(ctx.to_string()),
            },
        },
    })
}

async fn run_find(
    db: &Database,
    collection: &str,
    filter: serde_json::Value,
    limit: Option<u64>,
    sort: Option<serde_json::Value>,
    max_rows: usize,
    start: Instant,
) -> Result<QueryResult> {
    let filter_doc = json_to_document(filter)?;
    let mut options = FindOptions::default();
    let take = limit.unwrap_or(max_rows as u64).min(max_rows as u64);
    options.limit = Some(take as i64);
    if let Some(sort) = sort {
        options.sort = Some(json_to_document(sort)?);
    }

    let mut cursor = db
        .collection::<Document>(collection)
        .find(filter_doc)
        .with_options(options)
        .await
        .map_err(|e| Error::query(e))?;

    let mut columns: Vec<ColumnMeta> = Vec::new();
    let mut rows: Vec<Vec<CellValue>> = Vec::new();
    let mut truncated = false;

    while let Some(doc) = cursor.next().await {
        let doc = doc.map_err(|e| Error::query(e))?;
        if rows.len() >= max_rows {
            truncated = true;
            break;
        }
        collect_doc(&doc, &mut columns, &mut rows);
    }

    Ok(QueryResult {
        columns,
        rows,
        affected: None,
        truncated,
        elapsed: start.elapsed(),
    })
}

async fn run_aggregate(
    db: &Database,
    collection: &str,
    pipeline: serde_json::Value,
    max_rows: usize,
    start: Instant,
) -> Result<QueryResult> {
    let pipeline_bson = json_to_bson(pipeline)?;
    let stages: Vec<Document> = match pipeline_bson {
        Bson::Array(items) => items
            .into_iter()
            .map(|item| match item {
                Bson::Document(d) => Ok(d),
                _ => Err(Error::query("pipeline stages must be objects")),
            })
            .collect::<Result<_>>()?,
        _ => return Err(Error::query("`pipeline` must be an array")),
    };

    let mut cursor = db
        .collection::<Document>(collection)
        .aggregate(stages)
        .with_options(AggregateOptions::default())
        .await
        .map_err(|e| Error::query(e))?;

    let mut columns: Vec<ColumnMeta> = Vec::new();
    let mut rows: Vec<Vec<CellValue>> = Vec::new();
    let mut truncated = false;

    while let Some(doc) = cursor.next().await {
        let doc = doc.map_err(|e| Error::query(e))?;
        if rows.len() >= max_rows {
            truncated = true;
            break;
        }
        collect_doc(&doc, &mut columns, &mut rows);
    }

    Ok(QueryResult {
        columns,
        rows,
        affected: None,
        truncated,
        elapsed: start.elapsed(),
    })
}

/// Grow the column set from document keys and push one row.
fn collect_doc(
    doc: &Document,
    columns: &mut Vec<ColumnMeta>,
    rows: &mut Vec<Vec<CellValue>>,
) {
    for key in doc.keys() {
        if !columns.iter().any(|c| &c.name == key) {
            columns.push(ColumnMeta {
                name: key.clone(),
                data_type: DataType::new("bson", TypeFamily::Other),
                nullable: true,
                comment: None,
            });
        }
    }
    let cells = columns
        .iter()
        .map(|col| match doc.get(&col.name) {
            Some(b) => bson_to_cell(b),
            None => CellValue::Null,
        })
        .collect();
    rows.push(cells);
}

fn json_to_bson(value: serde_json::Value) -> Result<Bson> {
    bson::to_bson(&value).map_err(|e| Error::query(format!("invalid Extended JSON: {e}")))
}

fn json_to_document(value: serde_json::Value) -> Result<Document> {
    match json_to_bson(value)? {
        Bson::Document(doc) => Ok(doc),
        _ => Err(Error::query("expected a JSON object")),
    }
}

fn bson_to_cell(b: &Bson) -> CellValue {
    match b {
        Bson::Null => CellValue::Null,
        Bson::Boolean(v) => CellValue::Bool(*v),
        Bson::Int32(v) => CellValue::Int(*v as i64),
        Bson::Int64(v) => CellValue::Int(*v),
        Bson::Double(v) => CellValue::Float(*v),
        Bson::String(s) => CellValue::Text(s.clone()),
        Bson::Binary(bin) => CellValue::Bytes(bin.bytes.clone()),
        Bson::DateTime(dt) => {
            let millis = dt.timestamp_millis();
            match chrono::DateTime::from_timestamp_millis(millis) {
                Some(utc) => CellValue::DateTime(utc),
                None => CellValue::Text(dt.to_string()),
            }
        }
        Bson::RegularExpression(re) => CellValue::Other {
            type_name: "regex".into(),
            value: format!("/{}/{}", re.pattern, re.options),
        },
        Bson::JavaScriptCode(code) => CellValue::Other {
            type_name: "javascript".into(),
            value: code.clone(),
        },
        Bson::JavaScriptCodeWithScope(j) => CellValue::Other {
            type_name: "javascriptWithScope".into(),
            value: j.code.clone(),
        },
        Bson::Timestamp(ts) => CellValue::Other {
            type_name: "timestamp".into(),
            value: format!("{}:{}", ts.time, ts.increment),
        },
        Bson::ObjectId(oid) => CellValue::Text(oid.to_hex()),
        Bson::Decimal128(d) => CellValue::Text(d.to_string()),
        Bson::Symbol(s) => CellValue::Text(s.clone()),
        Bson::Undefined => CellValue::Null,
        Bson::MaxKey => CellValue::Other {
            type_name: "maxKey".into(),
            value: String::new(),
        },
        Bson::MinKey => CellValue::Other {
            type_name: "minKey".into(),
            value: String::new(),
        },
        Bson::DbPointer(p) => CellValue::Other {
            type_name: "dbPointer".into(),
            value: format!("{p:?}"),
        },
        Bson::Array(items) => {
            let json: Vec<serde_json::Value> = items.iter().map(bson_to_json).collect();
            CellValue::Json(serde_json::Value::Array(json))
        }
        Bson::Document(_) => CellValue::Json(bson_to_json(b)),
    }
}

fn bson_to_json(b: &Bson) -> serde_json::Value {
    serde_json::to_value(b).unwrap_or(serde_json::Value::Null)
}
