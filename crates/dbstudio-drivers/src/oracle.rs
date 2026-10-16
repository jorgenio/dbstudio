//! Oracle is not supported yet.
//!
//! The pure-Rust Oracle client (`oacle`) is not production-ready; options for
//! a future phase are an out-of-process bridge or an optional C client. The
//! engine exists in the UI so connections can be modeled, but connecting
//! returns an explicit error.

use std::sync::Arc;

use async_trait::async_trait;
use dbstudio_core::{
    ConnectionConfig, Engine, Error, ExecuteRequest, ObjectNode, ObjectRef, QueryResult, Result,
};

use crate::{Capabilities, DatabaseClient, TableDetail};

#[allow(dead_code)]
struct OracleClient;

pub async fn connect(_cfg: &ConnectionConfig) -> Result<Arc<dyn DatabaseClient>> {
    Err(Error::Unsupported(
        "Oracle is not supported yet (no production-quality Rust driver). \
         Use a community bridge or contribute to the oacle crate."
            .into(),
    ))
}

#[async_trait]
impl DatabaseClient for OracleClient {
    fn engine(&self) -> Engine {
        Engine::Oracle
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::SQL_BASIC
    }

    async fn ping(&self) -> Result<()> {
        unreachable!("connect() never yields OracleClient")
    }

    async fn list_children(&self, _parent: &ObjectRef) -> Result<Vec<ObjectNode>> {
        unreachable!()
    }

    async fn describe_table(&self, _table: &ObjectRef) -> Result<TableDetail> {
        unreachable!()
    }

    async fn execute(&self, _req: ExecuteRequest) -> Result<QueryResult> {
        unreachable!()
    }
}
