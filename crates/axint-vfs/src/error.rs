//! Virtual-filesystem errors.

use std::io;

/// A watcher or hydration failure.
#[derive(Debug, thiserror::Error)]
pub enum VfsError {
    /// A filesystem call failed.
    #[error(transparent)]
    Io(#[from] io::Error),

    /// The OS watcher could not be started.
    #[error("filesystem watcher failed: {0}")]
    Watch(String),

    /// The dirty ring overflowed and a full scan is required.
    #[error("dirty path ring overflowed; a full scan is required")]
    RescanRequired,

    /// A requested byte range does not overlap the file.
    #[error("read range is outside the file")]
    Range,
}
