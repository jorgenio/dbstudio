use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("connection failed: {0}")]
    Connection(String),

    #[error("authentication failed: {0}")]
    Auth(String),

    #[error("SSH error: {0}")]
    Ssh(String),

    #[error("query error: {0}")]
    Query(String),

    #[error("not supported: {0}")]
    Unsupported(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("secret store error: {0}")]
    Secret(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn connection(e: impl ToString) -> Self {
        Error::Connection(e.to_string())
    }

    pub fn query(e: impl ToString) -> Self {
        Error::Query(e.to_string())
    }

    pub fn ssh(e: impl ToString) -> Self {
        Error::Ssh(e.to_string())
    }

    pub fn other(e: impl ToString) -> Self {
        Error::Other(e.to_string())
    }
}
