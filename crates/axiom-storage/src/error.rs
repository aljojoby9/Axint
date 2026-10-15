//! Storage-engine errors.

use std::io;

use axiom_core::CoreError;

/// Failure from chunking, the object store, or the metadata database.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// A core manifest or hash check failed.
    #[error(transparent)]
    Core(#[from] CoreError),

    /// A filesystem call failed.
    #[error(transparent)]
    Io(#[from] io::Error),

    /// `redb` rejected a transaction.
    #[error("metadata database error: {0}")]
    Database(String),

    /// No `.axiom` directory was found.
    #[error("not an axiom repository")]
    NotARepository,

    /// The named bookmark has no commit.
    #[error("bookmark `{0}` does not exist")]
    MissingBookmark(String),

    /// The repository has no commits yet.
    #[error("repository has no commits")]
    NoCommits,

    /// A caller-facing rule failed.
    #[error("{0}")]
    Message(String),

    /// A CAS frame failed its checksum or magic check.
    #[error("corrupt content block {0}")]
    CorruptBlock(String),
}

impl StorageError {
    /// Wraps a database error that implements [`Display`](std::fmt::Display).
    #[must_use]
    pub fn database(err: impl std::fmt::Display) -> Self {
        Self::Database(err.to_string())
    }

    /// Copies this error into a [`CoreError`] for the object-store trait.
    #[must_use]
    pub fn into_core(self) -> CoreError {
        match self {
            Self::Core(err) => err,
            other => CoreError::Message(other.to_string()),
        }
    }
}
