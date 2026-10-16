use std::fmt;

/// A string that redacts itself in `Debug` output and never prints its contents
/// accidentally (logs, `{:?}` in errors, panic messages).
#[derive(Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl From<String> for SecretString {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for SecretString {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_debug_and_display() {
        let s = SecretString::new("hunter2");
        assert!(!format!("{s:?}").contains("hunter2"));
        assert!(!format!("{s}").contains("hunter2"));
        assert_eq!(s.expose(), "hunter2");
    }

    #[test]
    fn serde_roundtrip() {
        let s = SecretString::new("hunter2");
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"hunter2\"");
        let back: SecretString = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }
}
