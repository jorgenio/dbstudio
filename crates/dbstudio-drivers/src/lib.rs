//! Database drivers behind a single async trait.
//!
//! `connect` opens an SSH tunnel first when the config asks for one; each
//! driver then connects either to the real host (direct) or to the tunnel's
//! loopback endpoint / injected stream.

mod helpers;
mod mongo;
mod mssql;
mod mysql;
mod oracle;
mod postgres;
mod sqlite;

use std::sync::Arc;

use async_trait::async_trait;
use dbstudio_core::{
    ColumnMeta, ConnectionConfig, Engine, Error, ExecuteRequest, ObjectNode, ObjectRef, QueryResult,
    Result,
};
use dbstudio_ssh::Tunnel;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Statements go through a SQL parser (vs. document queries).
    pub sql: bool,
    pub explain: bool,
    pub transactions: bool,
    /// Object tree has schemas between database and tables.
    pub schemas: bool,
}

impl Capabilities {
    pub const SQL_BASIC: Self = Self {
        sql: true,
        explain: false,
        transactions: true,
        schemas: false,
    };
}

#[derive(Debug, Clone, Default)]
pub struct TableDetail {
    pub columns: Vec<ColumnMeta>,
}

#[async_trait]
pub trait DatabaseClient: Send + Sync {
    fn engine(&self) -> Engine;
    fn capabilities(&self) -> Capabilities;
    /// Cheap health check used by the "Test connection" button.
    async fn ping(&self) -> Result<()>;
    /// Lazy children for the object navigator. `parent` is the connection
    /// root when `path` is empty.
    async fn list_children(&self, parent: &ObjectRef) -> Result<Vec<ObjectNode>>;
    async fn describe_table(&self, table: &ObjectRef) -> Result<TableDetail>;
    /// Run a statement (or script / document query) and collect at most
    /// `max_rows` rows.
    async fn execute(&self, req: ExecuteRequest) -> Result<QueryResult>;
}

/// A live client plus (when configured) the tunnel keeping it reachable.
pub struct Session {
    pub client: Arc<dyn DatabaseClient>,
    pub tunnel: Option<Tunnel>,
}

/// Open a connection described by `cfg`, establishing an SSH tunnel first if
/// configured. The tunnel stays alive inside the returned `Session`.
pub async fn connect(cfg: &ConnectionConfig) -> Result<Session> {
    let tunnel = match (&cfg.ssh, cfg.engine) {
        (Some(_), Engine::Sqlite) => {
            return Err(Error::Config(
                "SQLite connects to a local file; SSH tunneling does not apply".into(),
            ));
        }
        (Some(ssh_cfg), _) => Some(
            Tunnel::open(ssh_cfg, &cfg.host, cfg.port)
                .await
                .map_err(|e| Error::Ssh(e.to_string()))?,
        ),
        (None, _) => None,
    };

    let client: Arc<dyn DatabaseClient> = match cfg.engine {
        Engine::Postgres => postgres::connect(cfg, tunnel.as_ref()).await?,
        Engine::Mysql | Engine::MariaDb => mysql::connect(cfg, tunnel.as_ref()).await?,
        Engine::Mongodb => mongo::connect(cfg, tunnel.as_ref()).await?,
        Engine::Sqlite => sqlite::connect(cfg).await?,
        Engine::SqlServer => mssql::connect(cfg, tunnel.as_ref()).await?,
        Engine::Oracle => oracle::connect(cfg).await?,
    };

    Ok(Session { client, tunnel })
}
