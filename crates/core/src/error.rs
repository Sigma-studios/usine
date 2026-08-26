//! Centralized error type for `usine-core`.
//!
//! Every fallible operation in the core returns [`Result`]. The UI converts
//! these into user-facing toasts — the core itself never panics across its
//! public boundary.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    // Boxed: `native_db`'s error is ~176 bytes and would otherwise bloat every
    // `Result<T, CoreError>` in the crate (clippy::result_large_err).
    #[error("database error: {0}")]
    Db(Box<native_db::db_type::Error>),

    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("git error: {0}")]
    Git(#[from] git2::Error),

    #[error("provider error: {0}")]
    Provider(String),

    #[error("forge error: {0}")]
    Forge(String),

    #[error("illegal transition: {0}")]
    IllegalTransition(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("{0}")]
    Other(String),
}

impl From<native_db::db_type::Error> for CoreError {
    fn from(e: native_db::db_type::Error) -> Self {
        CoreError::Db(Box::new(e))
    }
}

pub type Result<T> = std::result::Result<T, CoreError>;

impl CoreError {
    pub fn provider(msg: impl Into<String>) -> Self {
        CoreError::Provider(msg.into())
    }
    pub fn forge(msg: impl Into<String>) -> Self {
        CoreError::Forge(msg.into())
    }
    pub fn other(msg: impl Into<String>) -> Self {
        CoreError::Other(msg.into())
    }

    /// True when the database file is locked by another writer — redb allows
    /// only one, so a second Usine (or the CLI) fails to open the store.
    pub fn is_db_locked(&self) -> bool {
        matches!(self, CoreError::Db(e) if matches!(
            **e,
            native_db::db_type::Error::RedbDatabaseError(redb::DatabaseError::DatabaseAlreadyOpen)
        ))
    }
}
