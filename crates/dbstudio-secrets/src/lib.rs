//! Persistent storage for connection secrets.
//!
//! Primary backend is the OS keyring (Windows Credential Manager, macOS
//! Keychain, freedesktop Secret Service). On systems where the keyring is
//! unavailable (headless Linux without a Secret Service), callers fall back to
//! a 0600 JSON file in the user config directory.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use dbstudio_core::{Error, Result};

const SERVICE: &str = "dbstudio";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Keyring,
    File,
}

#[derive(Debug)]
pub enum SecretStore {
    Keyring,
    File {
        path: PathBuf,
        cache: Mutex<BTreeMap<String, String>>,
    },
}

impl SecretStore {
    /// Keyring backend.
    pub fn keyring() -> Self {
        SecretStore::Keyring
    }

    /// File backend at `path`.
    pub fn file(path: PathBuf) -> Self {
        SecretStore::File {
            path,
            cache: Mutex::new(BTreeMap::new()),
        }
    }

    /// Keyring if it round-trips, otherwise a file under the user config dir.
    pub fn open_auto() -> Self {
        let probe = SecretStore::Keyring;
        match probe.set("__probe__", "probe") {
            Ok(()) => {
                let _ = probe.delete("__probe__");
                SecretStore::Keyring
            }
            Err(_) => SecretStore::file(default_file_path()),
        }
    }

    pub fn backend(&self) -> Backend {
        match self {
            SecretStore::Keyring => Backend::Keyring,
            SecretStore::File { .. } => Backend::File,
        }
    }

    pub fn get(&self, account: &str) -> Result<Option<String>> {
        match self {
            SecretStore::Keyring => {
                let entry = keyring::Entry::new(SERVICE, account)
                    .map_err(|e| Error::Secret(e.to_string()))?;
                match entry.get_password() {
                    Ok(secret) => Ok(Some(secret)),
                    Err(keyring::Error::NoEntry) => Ok(None),
                    Err(e) => Err(Error::Secret(e.to_string())),
                }
            }
            SecretStore::File { path, cache } => {
                let map = self.load_file(path, cache)?;
                Ok(map.get(account).cloned())
            }
        }
    }

    pub fn set(&self, account: &str, secret: &str) -> Result<()> {
        match self {
            SecretStore::Keyring => {
                let entry = keyring::Entry::new(SERVICE, account)
                    .map_err(|e| Error::Secret(e.to_string()))?;
                entry
                    .set_password(secret)
                    .map_err(|e| Error::Secret(e.to_string()))
            }
            SecretStore::File { path, cache } => {
                let mut map = self.load_file(path, cache)?;
                map.insert(account.to_string(), secret.to_string());
                self.save_file(path, &map)?;
                if let Ok(mut c) = cache.lock() {
                    *c = map;
                }
                Ok(())
            }
        }
    }

    pub fn delete(&self, account: &str) -> Result<()> {
        match self {
            SecretStore::Keyring => {
                let entry = keyring::Entry::new(SERVICE, account)
                    .map_err(|e| Error::Secret(e.to_string()))?;
                match entry.delete_credential() {
                    Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                    Err(e) => Err(Error::Secret(e.to_string())),
                }
            }
            SecretStore::File { path, cache } => {
                let mut map = self.load_file(path, cache)?;
                map.remove(account);
                self.save_file(path, &map)?;
                if let Ok(mut c) = cache.lock() {
                    *c = map;
                }
                Ok(())
            }
        }
    }

    fn load_file(
        &self,
        path: &PathBuf,
        cache: &Mutex<BTreeMap<String, String>>,
    ) -> Result<BTreeMap<String, String>> {
        if let Ok(c) = cache.lock() {
            if !c.is_empty() {
                return Ok(c.clone());
            }
        }
        if !path.exists() {
            return Ok(BTreeMap::new());
        }
        let data = std::fs::read_to_string(path)?;
        serde_json::from_str(&data).map_err(|e| Error::Secret(format!("corrupt secrets file: {e}")))
    }

    fn save_file(&self, path: &PathBuf, map: &BTreeMap<String, String>) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(map).map_err(|e| Error::Secret(e.to_string()))?;
        std::fs::write(path, json)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

pub fn default_file_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("dbstudio")
        .join("secrets.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_store_roundtrip() {
        let path = std::env::temp_dir().join(format!("dbstudio-test-{}.json", uuid::Uuid::new_v4()));
        let store = SecretStore::file(path.clone());
        assert_eq!(store.get("acct").unwrap(), None);
        store.set("acct", "s3cret").unwrap();
        assert_eq!(store.get("acct").unwrap().as_deref(), Some("s3cret"));
        // A second handle sees the same file.
        let store2 = SecretStore::file(path.clone());
        assert_eq!(store2.get("acct").unwrap().as_deref(), Some("s3cret"));
        store.delete("acct").unwrap();
        assert_eq!(store.get("acct").unwrap(), None);
        let _ = std::fs::remove_file(path);
    }
}
