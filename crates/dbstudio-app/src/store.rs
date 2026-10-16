//! Saved connections: a JSON list under the user config dir, passwords
//! kept out of the file and persisted in the OS keyring instead.

use std::path::PathBuf;

use dbstudio_core::{ConnectionConfig, SecretString, SshAuth};
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
            let ssh_account = format!("{}-ssh", cfg.secret_account());
            if let Ok(Some(pw)) = self.secrets.get(&cfg.secret_account()) {
                cfg.password = Some(pw.into());
            }
            if let Some(ssh) = &mut cfg.ssh {
                match &mut ssh.auth {
                    // Stripped secrets are persisted as "" — the keyring holds
                    // the real value.
                    SshAuth::Password { password } => {
                        if password.is_empty() {
                            if let Ok(Some(stored)) = self.secrets.get(&ssh_account) {
                                *password = stored.into();
                            }
                        }
                    }
                    SshAuth::PrivateKey { passphrase, .. } => {
                        if passphrase.as_ref().is_some_and(|p| p.is_empty()) {
                            if let Ok(Some(stored)) = self.secrets.get(&format!("{ssh_account}-pp"))
                            {
                                *passphrase = Some(stored.into());
                            }
                        }
                    }
                    SshAuth::Agent => {}
                }
            }
        }
        cfgs
    }

    /// Persist configs: passwords go to the secret store, never to disk.
    pub fn save(&self, cfgs: &[ConnectionConfig]) -> Result<(), String> {
        let mut stripped = cfgs.to_vec();
        for cfg in &mut stripped {
            let ssh_account = format!("{}-ssh", cfg.secret_account());
            if let Some(pw) = cfg.password.take() {
                self.secrets
                    .set(&cfg.secret_account(), pw.expose())
                    .map_err(|e| e.to_string())?;
            }
            if let Some(ssh) = &mut cfg.ssh {
                match &mut ssh.auth {
                    SshAuth::Password { password } => {
                        if !password.is_empty() {
                            self.secrets
                                .set(&ssh_account, password.expose())
                                .map_err(|e| e.to_string())?;
                        }
                        *password = SecretString::new(String::new());
                    }
                    SshAuth::PrivateKey { passphrase, .. } => {
                        if let Some(pp) = passphrase.take() {
                            if !pp.is_empty() {
                                self.secrets
                                    .set(&format!("{ssh_account}-pp"), pp.expose())
                                    .map_err(|e| e.to_string())?;
                            }
                            // Keep the option as an empty marker so load()
                            // knows a passphrase existed.
                            *passphrase = Some(SecretString::new(String::new()));
                        }
                    }
                    SshAuth::Agent => {}
                }
            }
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_string_pretty(&stripped).map_err(|e| e.to_string())?;
        std::fs::write(&self.path, json).map_err(|e| e.to_string())
    }

    /// Drop every stored secret belonging to `cfg` (best-effort).
    pub fn forget(&self, cfg: &ConnectionConfig) {
        let _ = self.secrets.delete(&cfg.secret_account());
        let ssh_account = format!("{}-ssh", cfg.secret_account());
        let _ = self.secrets.delete(&ssh_account);
        let _ = self.secrets.delete(&format!("{ssh_account}-pp"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::{Engine, SshConfig};

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

        let mut ssh = SshConfig::new("bastion.example.com", "deploy");
        ssh.auth = SshAuth::Password {
            password: SecretString::new("tun3l-s3cret"),
        };
        let cfg = ConnectionConfig::new("pg-local", Engine::Postgres)
            .with_host("db.local")
            .with_username("admin")
            .with_password("s3cret")
            .with_ssh(ssh);
        store.save(std::slice::from_ref(&cfg)).unwrap();

        let raw = std::fs::read_to_string(&conn_path).unwrap();
        assert!(!raw.contains("s3cret"), "password leaked to connections.json");
        assert!(
            !raw.contains("tun3l-s3cret"),
            "ssh password leaked to connections.json"
        );

        let loaded = store.load();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "pg-local");
        assert_eq!(
            loaded[0].password.as_ref().map(|p| p.expose()),
            Some("s3cret"),
            "password should be restored from the secret store"
        );
        let ssh = loaded[0].ssh.as_ref().expect("ssh config roundtripped");
        assert_eq!(ssh.host, "bastion.example.com");
        match &ssh.auth {
            SshAuth::Password { password } => assert_eq!(password.expose(), "tun3l-s3cret"),
            other => panic!("expected password auth, got {other:?}"),
        }

        store.forget(&cfg);
        let after = store.load();
        assert!(after[0].password.is_none(), "password secret should be gone");
        match &after[0].ssh.as_ref().unwrap().auth {
            SshAuth::Password { password } => assert!(password.is_empty()),
            other => panic!("expected password auth, got {other:?}"),
        }

        let _ = std::fs::remove_file(conn_path);
        let _ = std::fs::remove_file(sec_path);
    }
}
