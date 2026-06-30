use thiserror::Error;

/// Library error type. Boundaries (`main`) wrap these with `anyhow` context.
#[derive(Debug, Error)]
pub enum Error {
    #[error("config: {0}")]
    Config(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("github: {0}")]
    Github(String),
}

pub type Result<T> = std::result::Result<T, Error>;
