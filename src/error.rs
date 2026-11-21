//! Application error codes (plan.md section 11) and the warning shape shared by
//! tool results.

use std::fmt;

#[derive(Debug)]
pub enum Error {
    InvalidArgument(String),
    RootNotFound,
    NotInitialized,
    UnsafePath(String),
    IndexBusy,
    SchemaTooNew,
    IndexCorrupt,
    Fts5Unavailable,
    Io(std::io::Error),
    AtomicPublishUnavailable,
    NameCollision,
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidArgument(_) => "INVALID_ARGUMENT",
            Self::RootNotFound => "ROOT_NOT_FOUND",
            Self::NotInitialized => "NOT_INITIALIZED",
            Self::UnsafePath(_) => "UNSAFE_PATH",
            Self::IndexBusy => "INDEX_BUSY",
            Self::SchemaTooNew => "SCHEMA_TOO_NEW",
            Self::IndexCorrupt => "INDEX_CORRUPT",
            Self::Fts5Unavailable => "FTS5_UNAVAILABLE",
            Self::Io(_) => "IO_ERROR",
            Self::AtomicPublishUnavailable => "ATOMIC_PUBLISH_UNAVAILABLE",
            Self::NameCollision => "NAME_COLLISION",
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(self, Self::IndexBusy | Self::NameCollision)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidArgument(m) => write!(f, "invalid argument: {m}"),
            Self::RootNotFound => {
                write!(
                    f,
                    "no project root found; supply --root or run epocnote init"
                )
            }
            Self::NotInitialized => {
                write!(
                    f,
                    "project root has no .context directory; run epocnote init"
                )
            }
            Self::UnsafePath(m) => write!(f, "unsafe path: {m}"),
            Self::IndexBusy => write!(f, "index is busy; retry later"),
            Self::SchemaTooNew => write!(f, "a newer server owns the index format; upgrade"),
            Self::IndexCorrupt => {
                write!(
                    f,
                    "index is corrupt; stop all epocnote processes and recover"
                )
            }
            Self::Fts5Unavailable => write!(f, "SQLite lacks required FTS5 support"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::AtomicPublishUnavailable => {
                write!(f, "filesystem cannot provide atomic publication")
            }
            Self::NameCollision => write!(f, "note filename collision retries exhausted"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub code: String,
    pub path: Option<String>,
    pub message: String,
}

impl Warning {
    pub fn new(code: &str, path: Option<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            path,
            message: message.into(),
        }
    }
}
