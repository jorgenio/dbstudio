use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::secret::SecretString;

pub type ConnectionId = Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Postgres,
    Mysql,
    MariaDb,
    Mongodb,
    Sqlite,
    SqlServer,
    Oracle,
}

impl Engine {
    pub const ALL: [Engine; 7] = [
        Engine::Postgres,
        Engine::Mysql,
        Engine::MariaDb,
        Engine::Mongodb,
        Engine::Sqlite,
        Engine::SqlServer,
        Engine::Oracle,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Engine::Postgres => "PostgreSQL",
            Engine::Mysql => "MySQL",
            Engine::MariaDb => "MariaDB",
            Engine::Mongodb => "MongoDB",
            Engine::Sqlite => "SQLite",
            Engine::SqlServer => "SQL Server",
            Engine::Oracle => "Oracle",
        }
    }

    pub fn default_port(&self) -> u16 {
        match self {
            Engine::Postgres => 5432,
            Engine::Mysql | Engine::MariaDb => 3306,
            Engine::Mongodb => 27017,
            Engine::Sqlite => 0,
            Engine::SqlServer => 1433,
            Engine::Oracle => 1521,
        }
    }

    pub fn is_sql(&self) -> bool {
        !matches!(self, Engine::Mongodb)
    }
}

impl std::fmt::Display for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    Disable,
    #[default]
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl TlsMode {
    /// Every mode, in UI order (form pick-lists).
    pub const ALL: [TlsMode; 5] = [
        TlsMode::Disable,
        TlsMode::Prefer,
        TlsMode::Require,
        TlsMode::VerifyCa,
        TlsMode::VerifyFull,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            TlsMode::Disable => "Deshabilitado",
            TlsMode::Prefer => "Preferir",
            TlsMode::Require => "Requerir",
            TlsMode::VerifyCa => "Verificar CA",
            TlsMode::VerifyFull => "Verificado completo",
        }
    }

    pub fn wants_tls(&self) -> bool {
        !matches!(self, TlsMode::Disable)
    }
}

impl std::fmt::Display for TlsMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "lowercase")]
pub enum SshAuth {
    Password { password: SecretString },
    PrivateKey {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        passphrase: Option<SecretString>,
    },
    Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum HostKeyPolicy {
    /// Reject unknown and changed host keys.
    #[default]
    Strict,
    /// Accept unknown host keys and pin them; reject changed keys.
    AcceptNew,
    /// Accept any host key (insecure; for throwaway environments).
    Insecure,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SshConfig {
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    pub username: String,
    pub auth: SshAuth,
    #[serde(default)]
    pub policy: HostKeyPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_hosts_file: Option<PathBuf>,
}

fn default_ssh_port() -> u16 {
    22
}

impl SshConfig {
    pub fn new(host: impl Into<String>, username: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: 22,
            username: username.into(),
            auth: SshAuth::Agent,
            policy: HostKeyPolicy::Strict,
            known_hosts_file: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub id: ConnectionId,
    pub name: String,
    pub engine: Engine,
    /// Host name or, for SQLite, the file path.
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<SecretString>,
    #[serde(default)]
    pub tls: TlsMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshConfig>,
    #[serde(default)]
    pub options: BTreeMap<String, String>,
}

impl ConnectionConfig {
    pub fn new(name: impl Into<String>, engine: Engine) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            engine,
            host: "localhost".to_string(),
            port: engine.default_port(),
            database: String::new(),
            username: String::new(),
            password: None,
            tls: TlsMode::default(),
            ssh: None,
            options: BTreeMap::new(),
        }
    }

    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn with_database(mut self, database: impl Into<String>) -> Self {
        self.database = database.into();
        self
    }

    pub fn with_username(mut self, username: impl Into<String>) -> Self {
        self.username = username.into();
        self
    }

    pub fn with_password(mut self, password: impl Into<SecretString>) -> Self {
        self.password = Some(password.into());
        self
    }

    pub fn with_ssh(mut self, ssh: SshConfig) -> Self {
        self.ssh = Some(ssh);
        self
    }

    /// Connection identity used as account name in the OS keyring.
    pub fn secret_account(&self) -> String {
        format!("{}|{}|{}|{}", self.engine as u8, self.host, self.port, self.username)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_serde_roundtrip() {
        let cfg = ConnectionConfig::new("local pg", Engine::Postgres)
            .with_host("db.internal")
            .with_port(5432)
            .with_database("app")
            .with_username("admin")
            .with_password("secret")
            .with_ssh(SshConfig::new("bastion", "deploy"));
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let back: ConnectionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn default_ports() {
        assert_eq!(Engine::Postgres.default_port(), 5432);
        assert_eq!(Engine::Mongodb.default_port(), 27017);
        assert!(Engine::Mysql.is_sql());
        assert!(!Engine::Mongodb.is_sql());
    }
}
