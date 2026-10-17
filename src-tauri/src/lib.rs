//! Trusted M0 foundation. Worker input never carries actor, grants or root limits.
pub mod crypto;
pub mod index;
pub mod geometry;
pub mod ledger;
pub mod model_pool;
pub mod runtime;
#[cfg(windows)]
pub mod installation;
#[cfg(windows)]
pub mod isolation;
#[cfg(windows)]
pub mod worker_process;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("permission denied")]
    Denied,
    #[error("invalid business contract")]
    Contract,
    #[error("root budget exhausted")]
    Budget,
    #[error("cancelled, revoked or locked")]
    Cancelled,
    #[error("immutable identity conflict")]
    Conflict,
    #[error("installation resource integrity failure")]
    Integrity,
    #[error("database failure")]
    Database(#[from] rusqlite::Error),
    #[error("resource I/O failure")]
    Io(#[from] std::io::Error),
    #[error("resource format failure")]
    Json(#[from] serde_json::Error),
    #[error("authenticated encryption failure")]
    Crypto,
}
pub type Result<T> = std::result::Result<T, Error>;
