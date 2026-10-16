//! Saved connections: a JSON list under the user config dir, passwords
//! kept out of the file and persisted in the OS keyring instead.

use std::path::PathBuf;

use dbstudio_core::ConnectionConfig;
use dbstudio_secrets::SecretStore;

pub struct Connections {
    path: PathBuf,
    secrets: SecretStore,
}

impl Connections {
    pub fn open() -> Self {
        let path = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("dbstudio")
            .join("connections.json");
        Self {
            path,
            secrets: SecretStore::open_auto(),
        }
    }

    /// Load saved configs, re-attaching passwords from the secret store.
    pub fn load(&self) -> Vec<ConnectionConfig> {
        let Ok(data) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        let Ok(mut cfgs) = serde_json::from_str::<Vec<ConnectionConfig>>(&data) else {
            return Vec::new();
        };
        for cfg in &mut cfgs {
            if let Ok(Some(pw)) = self.secrets.get(&cfg.secret_account()) {
                cfg.password = Some(pw.into());
            }
        }
        cfgs
    }

    /// Persist configs: passwords go to the secret store, never to disk.
    pub fn save(&self, cfgs: &[ConnectionConfig]) -> Result<(), String> {
        let mut stripped = cfgs.to_vec();
        for cfg in &mut stripped {
            if let Some(pw) = cfg.password.take() {
                self.secrets
                    .set(&cfg.secret_account(), pw.expose())
                    .map_err(|e| e.to_string())?;
            }
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_string_pretty(&stripped).map_err(|e| e.to_string())?;
        std::fs::write(&self.path, json).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::Engine;

    #[test]
    fn save_load_roundtrip_keeps_password_out_of_disk() {
        let conn_path =
            std::env::temp_dir().join(format!("dbstudio-test-conn-{}.json", std::process::id()));
        let sec_path =
            std::env::temp_dir().join(format!("dbstudio-test-sec-{}.json", std::process::id()));
        let store = Connections {
            path: conn_path.clone(),
            secrets: SecretStore::file(sec_path.clone()),
        };

        let cfg = ConnectionConfig::new("pg-local", Engine::Postgres)
            .with_host("db.local")
            .with_username("admin")
            .with_password("s3cret");
        store.save(std::slice::from_ref(&cfg)).unwrap();

        let raw = std::fs::read_to_string(&conn_path).unwrap();
        assert!(!raw.contains("s3cret"), "password leaked to connections.json");

        let loaded = store.load();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "pg-local");
        assert_eq!(
            loaded[0].password.as_ref().map(|p| p.expose()),
            Some("s3cret"),
            "password should be restored from the secret store"
        );

        let _ = std::fs::remove_file(conn_path);
        let _ = std::fs::remove_file(sec_path);
    }
}
