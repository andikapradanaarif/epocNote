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
    Cancelled,
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
            Self::Cancelled => "REQUEST_CANCELLED",
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(self, Self::IndexBusy | Self::NameCollision)
            || matches!(self, Self::Io(e) if matches!(
                e.kind(),
                std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ))
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
            Self::Cancelled => write!(f, "request cancelled before note publication"),
        }
    }
}

/// Optional fields may be omitted, but must have their declared type when present.
pub(crate) fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct Warning {
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
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

/// Warning caps: dedupe `(code, path)`, order by path then code, at most 8
/// entries of 160 scalars inside a 4 KiB serialized array (section 8.5).
pub(crate) fn fit_warnings(mut ws: Vec<Warning>) -> (Vec<Warning>, bool) {
    const MAX_WARNINGS: usize = 8;
    const MAX_WARNING_BYTES: usize = 4 * 1024;
    const MAX_WARNING_MSG: usize = 160;
    const MAX_PATH_BYTES: usize = crate::scan::MAX_SOURCE_PATH_BYTES;

    for w in &mut ws {
        if w.message.chars().count() > MAX_WARNING_MSG {
            w.message = w.message.chars().take(MAX_WARNING_MSG).collect();
        }
        if let Some(p) = &w.path
            && p.len() > MAX_PATH_BYTES
        {
            w.path = Some(p[..p.floor_char_boundary(MAX_PATH_BYTES)].to_owned());
        }
    }
    ws.sort_by(|a, b| {
        (a.path.as_deref().unwrap_or(""), a.code.as_str())
            .cmp(&(b.path.as_deref().unwrap_or(""), b.code.as_str()))
    });
    ws.dedup_by(|a, b| a.code == b.code && a.path == b.path);
    let mut truncated = false;
    while ws.len() > MAX_WARNINGS {
        ws.pop();
        truncated = true;
    }
    while !ws.is_empty()
        && serde_json::to_string(&ws)
            .map(|s| s.len())
            .unwrap_or(usize::MAX)
            > MAX_WARNING_BYTES
    {
        ws.pop();
        truncated = true;
    }
    (ws, truncated)
}
