pub mod advisory;
pub mod canon;
pub mod check;
pub mod db;
pub mod doc;
pub mod error;
pub mod init;
pub mod note;
pub mod root;
pub mod scan;
pub mod search;
pub mod sync;

// Pinned per plan.md section 1.3; used by the index and server in later work.
use rmcp as _;
use rusqlite as _;
use tokio as _;

pub use canon::CanonPath;
pub use error::{Error, Warning};
