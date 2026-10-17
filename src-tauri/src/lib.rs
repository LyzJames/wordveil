//! Trusted core. Worker input never carries actor, grants or root limits.
pub mod crypto;
pub mod index;
pub mod geometry;
pub mod ledger;
pub mod model_pool;
pub mod runtime;
pub mod store;
pub mod vault;
pub mod logging;
#[cfg(windows)]
pub mod installation;
#[cfg(windows)]
pub mod isolation;
#[cfg(windows)]
pub mod worker_process;
#[cfg(windows)]
pub mod local_worker;
#[cfg(windows)]
pub mod media;
#[cfg(windows)]
pub mod services;
#[cfg(windows)]
pub mod handlers;
#[cfg(windows)]
pub mod service;

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
    #[error("vault is locked")]
    Locked,
    /// A stable code reported by an isolated worker or tool; never document text.
    #[error("worker reported {0}")]
    Worker(String),
    #[error("input needed: {0}")]
    NeedsInput(&'static str),
}
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Stable machine code for UI messages and redacted logs (no paths or text).
    pub fn code(&self) -> String {
        match self {
            Error::Denied => "denied".into(),
            Error::Contract => "contract".into(),
            Error::Budget => "budget".into(),
            Error::Cancelled => "cancelled".into(),
            Error::Conflict => "conflict".into(),
            Error::Integrity => "integrity".into(),
            Error::Database(_) => "database".into(),
            Error::Io(_) => "io".into(),
            Error::Json(_) => "format".into(),
            Error::Crypto => "crypto".into(),
            Error::Locked => "locked".into(),
            Error::Worker(code) => format!("worker:{code}"),
            Error::NeedsInput(what) => format!("needs_input:{what}"),
        }
    }
}

/// Worker error codes are untrusted text; only short snake_case codes survive.
pub fn sanitize_code(code: &str) -> String {
    if !code.is_empty() && code.len() <= 64 && code.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
        code.to_owned()
    } else {
        "invalid_worker_error".into()
    }
}
