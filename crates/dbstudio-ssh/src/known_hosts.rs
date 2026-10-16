//! OpenSSH `known_hosts` parsing and verification.
//!
//! Supports plain entries, `host:port` entries (for non-standard SSH ports),
//! `*`/`?` wildcards, hashed (`|1|salt|hash`) entries, and `@revoked`
//! markers. Unknown hosts are classified so the caller can apply its
//! `HostKeyPolicy`.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use dbstudio_core::{Error, HostKeyPolicy, Result};
use hmac::{Hmac, Mac};
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownHost {
    pub patterns: Vec<String>,
    pub key_type: String,
    /// Raw wire-format key blob (decoded from base64).
    pub key: Vec<u8>,
    pub revoked: bool,
    /// Original line, used to re-emit on AcceptNew writes.
    pub raw: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyCheck {
    Match,
    Revoked,
    Unknown,
    Changed,
}

pub fn default_known_hosts_path() -> Option<PathBuf> {
    dirs_home().map(|h| h.join(".ssh").join("known_hosts"))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

pub struct KnownHostsFile {
    pub path: Option<PathBuf>,
    pub entries: Vec<KnownHost>,
}

impl KnownHostsFile {
    pub fn load(path: Option<PathBuf>) -> Result<Self> {
        let entries = match &path {
            Some(p) if p.exists() => parse(&std::fs::read_to_string(p)?)?,
            _ => Vec::new(),
        };
        Ok(Self { path, entries })
    }

    /// Verify `key` (wire format) against entries for `host` at `port`.
    pub fn check(&self, host: &str, port: u16, key: &[u8]) -> HostKeyCheck {
        let mut saw_plain_match = false;
        for entry in &self.entries {
            if !host_matches(&entry.patterns, host, port) {
                continue;
            }
            if entry.revoked {
                return HostKeyCheck::Revoked;
            }
            if entry.key == key {
                return HostKeyCheck::Match;
            }
            saw_plain_match = true;
        }
        if saw_plain_match {
            HostKeyCheck::Changed
        } else {
            HostKeyCheck::Unknown
        }
    }

    /// Append a plain (unhashed) entry for `host` so future connections pin
    /// the key. No-op when there is no known_hosts path.
    pub fn trust(&self, host: &str, port: u16, key_type: &str, key: &[u8]) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let host_pattern = if port == 22 {
            host.to_string()
        } else {
            format!("[{host}]:{port}")
        };
        let b64 = base64::engine::general_purpose::STANDARD.encode(key);
        let line = format!("{host_pattern} {key_type} {b64}\n");
        use std::io::Write;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        file.write_all(line.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

/// Resolve `HostKeyPolicy` into a decision given a check result.
pub fn apply_policy(
    policy: HostKeyPolicy,
    check: HostKeyCheck,
) -> std::result::Result<(), String> {
    match (policy, check) {
        (_, HostKeyCheck::Match) => Ok(()),
        (_, HostKeyCheck::Revoked) => Err("host key is marked @revoked in known_hosts".into()),
        (HostKeyPolicy::Strict, HostKeyCheck::Unknown) => {
            Err("host key not found in known_hosts (strict policy)".into())
        }
        (HostKeyPolicy::Strict, HostKeyCheck::Changed) => {
            Err("REMOTE HOST IDENTIFICATION HAS CHANGED — possible man-in-the-middle".into())
        }
        (HostKeyPolicy::AcceptNew, HostKeyCheck::Unknown) => Ok(()),
        (HostKeyPolicy::AcceptNew, HostKeyCheck::Changed) => {
            Err("REMOTE HOST IDENTIFICATION HAS CHANGED — possible man-in-the-middle".into())
        }
        (HostKeyPolicy::Insecure, _) => Ok(()),
    }
}

pub fn parse(content: &str) -> Result<Vec<KnownHost>> {
    let mut out = Vec::new();
    for raw_line in content.lines() {
        let line = raw_line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (revoked, rest) = if line.starts_with("@revoked ") {
            (true, &line["@revoked ".len()..])
        } else if line.starts_with("@cert-authority ") {
            // Certificates are not used for host auth here; skip.
            continue;
        } else if line.starts_with('@') {
            continue;
        } else {
            (false, line)
        };
        let mut parts = rest.split_whitespace();
        let (Some(patterns), Some(key_type), Some(key_b64)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let Ok(key) = base64::engine::general_purpose::STANDARD.decode(key_b64) else {
            continue;
        };
        out.push(KnownHost {
            patterns: patterns.split(',').map(str::to_string).collect(),
            key_type: key_type.to_string(),
            key,
            revoked,
            raw: line.to_string(),
        });
    }
    Ok(out)
}

fn host_matches(patterns: &[String], host: &str, port: u16) -> bool {
    patterns.iter().any(|p| pattern_matches(p, host, port))
}

fn pattern_matches(pattern: &str, host: &str, port: u16) -> bool {
    // Bracketed [host]:port form used for non-default ports.
    if let Some(rest) = pattern.strip_prefix('[') {
        if let Some((h, p)) = rest.split_once("]:") {
            if p.parse::<u16>() == Ok(port) {
                return glob_match(h, host);
            }
            return false;
        }
    }
    if port != 22 {
        return false;
    }
    // A bare pattern may also be an address literal or wildcard.
    glob_match(pattern, host)
}

/// Minimal `*`/`?` glob matcher for known_hosts patterns.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi].to_ascii_lowercase() == t[ti].to_ascii_lowercase())
        {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Verify an OpenSSH-hashed host entry (`|1|salt|hash`) against `host`.
pub fn hashed_host_matches(hash_field: &str, host: &str) -> bool {
    let mut parts = hash_field.split('|');
    if parts.next() != Some("") || parts.next() != Some("1") {
        return false;
    }
    let (Some(salt_b64), Some(hash_b64), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let (Ok(salt), Ok(expected)) = (
        base64::engine::general_purpose::STANDARD.decode(salt_b64),
        base64::engine::general_purpose::STANDARD.decode(hash_b64),
    ) else {
        return false;
    };
    let Ok(mut mac) = HmacSha1::new_from_slice(&salt) else {
        return false;
    };
    mac.update(host.as_bytes());
    mac.verify_slice(&expected).is_ok()
}

pub fn path_string(path: &Path) -> String {
    path.display().to_string()
}

pub fn load_or_default(explicit: Option<PathBuf>) -> Result<KnownHostsFile> {
    let path = explicit.or_else(default_known_hosts_path);
    KnownHostsFile::load(path).map_err(|e| Error::Ssh(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(entries: Vec<KnownHost>) -> KnownHostsFile {
        KnownHostsFile {
            path: None,
            entries,
        }
    }

    #[test]
    fn plain_entry_match() {
        let key = b"fakekeydata".to_vec();
        let line = format!(
            "example.com ssh-ed25519 {}\n",
            base64::engine::general_purpose::STANDARD.encode(&key)
        );
        let kh = file(parse(&line).unwrap());
        assert_eq!(kh.check("example.com", 22, &key), HostKeyCheck::Match);
        assert_eq!(kh.check("other.com", 22, &key), HostKeyCheck::Unknown);
        assert_eq!(
            kh.check("example.com", 22, b"other"),
            HostKeyCheck::Changed
        );
    }

    #[test]
    fn port_entry_match() {
        let line = format!(
            "[db.internal]:2222 ssh-rsa {}\n",
            base64::engine::general_purpose::STANDARD.encode(b"fake")
        );
        let kh = file(parse(&line).unwrap());
        assert_eq!(kh.check("db.internal", 2222, b"fake"), HostKeyCheck::Match);
        assert_eq!(kh.check("db.internal", 22, b"fake"), HostKeyCheck::Unknown);
    }

    #[test]
    fn wildcard_match() {
        let line = format!(
            "*.example.com ssh-ed25519 {}\n",
            base64::engine::general_purpose::STANDARD.encode(b"fake")
        );
        let kh = file(parse(&line).unwrap());
        assert_eq!(kh.check("db.example.com", 22, b"fake"), HostKeyCheck::Match);
        assert_eq!(kh.check("example.com", 22, b"fake"), HostKeyCheck::Unknown);
    }

    #[test]
    fn revoked_rejected_in_any_policy() {
        let line = format!(
            "@revoked example.com ssh-ed25519 {}\n",
            base64::engine::general_purpose::STANDARD.encode(b"fake")
        );
        let kh = file(parse(&line).unwrap());
        assert_eq!(kh.check("example.com", 22, b"fake"), HostKeyCheck::Revoked);
        assert!(apply_policy(HostKeyPolicy::Insecure, HostKeyCheck::Revoked).is_err());
    }

    #[test]
    fn hashed_entry_roundtrip() {
        let salt = b"0123456789abcdef";
        let mut mac = HmacSha1::new_from_slice(salt).unwrap();
        mac.update(b"secret.example.com");
        let hash = mac.finalize().into_bytes();
        let line = format!(
            "|1|{}|{} ssh-ed25519 AAAAFake==",
            base64::engine::general_purpose::STANDARD.encode(salt),
            base64::engine::general_purpose::STANDARD.encode(hash)
        );
        assert!(hashed_host_matches(
            &format!(
                "|1|{}|{}",
                base64::engine::general_purpose::STANDARD.encode(salt),
                base64::engine::general_purpose::STANDARD.encode(hash)
            ),
            "secret.example.com"
        ));
        let _ = line;
        assert!(!hashed_host_matches(
            &format!(
                "|1|{}|{}",
                base64::engine::general_purpose::STANDARD.encode(salt),
                base64::engine::general_purpose::STANDARD.encode(hash)
            ),
            "evil.example.com"
        ));
    }

    #[test]
    fn glob() {
        assert!(glob_match("*.example.com", "db.example.com"));
        assert!(glob_match("db?.example.com", "db1.example.com"));
        assert!(!glob_match("db?.example.com", "db12.example.com"));
        assert!(glob_match("exact.com", "exact.com"));
    }
}

