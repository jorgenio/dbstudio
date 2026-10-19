//! Saved connections: a JSON list under the user config dir, passwords
//! kept out of the file and persisted in the OS keyring instead.

use std::path::PathBuf;

use dbstudio_core::{ConnectionConfig, SecretString, SshAuth};
use dbstudio_secrets::SecretStore;
use serde::{Deserialize, Serialize};

/// App-level settings stored next to `connections.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub theme_dark: bool,
}

fn settings_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("dbstudio")
        .join("settings.json")
}

/// Load persisted app settings; defaults (light theme) on any problem.
pub fn load_settings() -> Settings {
    load_settings_from(&settings_path())
}

/// Persist app settings (best-effort; callers may ignore errors).
pub fn save_settings(settings: &Settings) {
    save_settings_to(&settings_path(), settings);
}

/// Same as [`load_settings`] but from an explicit path (tests).
pub fn load_settings_from(path: &std::path::Path) -> Settings {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Same as [`save_settings`] but to an explicit path (tests).
pub fn save_settings_to(path: &std::path::Path, settings: &Settings) {
    let Ok(json) = serde_json::to_string_pretty(settings) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, json);
}

/// One executed statement kept for reuse: newest first in `history.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub sql: String,
    /// Connection name at execution time (display only).
    #[serde(default)]
    pub conn: Option<String>,
    /// Local stamp `dd/MM HH:MM` recorded when the statement ran.
    #[serde(default)]
    pub when: String,
}

/// Statements kept on disk; older ones fall off the tail.
pub const HISTORY_CAP: usize = 300;

fn history_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("dbstudio")
        .join("history.json")
}

/// Load the query history; a missing or corrupt file reads as empty.
pub fn load_history() -> Vec<HistoryEntry> {
    load_history_from(&history_path())
}

/// Persist the history (best-effort; callers may ignore errors).
pub fn save_history(entries: &[HistoryEntry]) {
    save_history_to(&history_path(), entries);
}

/// Same as [`load_history`] but from an explicit path (tests).
pub fn load_history_from(path: &std::path::Path) -> Vec<HistoryEntry> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<HistoryEntry>>(&s).ok())
        .map(|mut v| {
            v.truncate(HISTORY_CAP);
            v
        })
        .unwrap_or_default()
}

/// Same as [`save_history`] but to an explicit path (tests).
pub fn save_history_to(path: &std::path::Path, entries: &[HistoryEntry]) {
    let Ok(json) = serde_json::to_string_pretty(entries) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, json);
}

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

    #[test]
    fn settings_theme_roundtrips_and_defaults_to_light() {
        let path =
            std::env::temp_dir().join(format!("dbstudio-test-settings-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // Missing file → defaults (light).
        assert!(!load_settings_from(&path).theme_dark);

        save_settings_to(&path, &Settings { theme_dark: true });
        assert!(load_settings_from(&path).theme_dark);

        save_settings_to(&path, &Settings { theme_dark: false });
        assert!(!load_settings_from(&path).theme_dark);

        // Corrupt file → defaults again, no panic.
        std::fs::write(&path, "{not json").unwrap();
        assert!(!load_settings_from(&path).theme_dark);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn history_roundtrips_trims_cap_and_reads_corrupt_as_empty() {
        let path =
            std::env::temp_dir().join(format!("dbstudio-test-hist-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // Missing file → empty history.
        assert!(load_history_from(&path).is_empty());

        let entries = vec![
            HistoryEntry {
                sql: "SELECT 1".into(),
                conn: Some("demo".into()),
                when: "01/01 10:00".into(),
            },
            HistoryEntry {
                sql: "SELECT 2".into(),
                conn: None,
                when: "01/01 10:01".into(),
            },
        ];
        save_history_to(&path, &entries);
        assert_eq!(load_history_from(&path), entries);

        // Over-cap file → trimmed on load.
        let big: Vec<HistoryEntry> = (0..HISTORY_CAP + 7)
            .map(|i| HistoryEntry {
                sql: format!("SELECT {i}"),
                conn: None,
                when: String::new(),
            })
            .collect();
        save_history_to(&path, &big);
        assert_eq!(load_history_from(&path).len(), HISTORY_CAP);

        // Corrupt file → empty again, no panic.
        std::fs::write(&path, "{nope").unwrap();
        assert!(load_history_from(&path).is_empty());

        let _ = std::fs::remove_file(&path);
    }
}
