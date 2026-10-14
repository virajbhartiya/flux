use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum Error {
    #[error("invalid path {path:?}: {reason}")]
    PathInvalid { path: String, reason: &'static str },
    #[error("path conflict at {0:?}: a path cannot be both a file and a directory")]
    PathConflict(String),
    #[error("invalid id: {0}")]
    InvalidId(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("non-canonical encoding")]
    NonCanonical,
    #[error("invalid object: {0}")]
    InvalidObject(String),
}
