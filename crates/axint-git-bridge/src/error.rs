//! Errors from the Git bridge.

use std::io;

use axint_core::CoreError;
use axint_storage::StorageError;

/// A Git import, export, or transport failure.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    /// The core library rejected a manifest.
    #[error(transparent)]
    Core(#[from] CoreError),

    /// The Axint repository rejected a read or write.
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// A filesystem call failed.
    #[error(transparent)]
    Io(#[from] io::Error),

    /// `git` is not on `PATH`.
    #[error("git is not available: {0}")]
    GitMissing(String),

    /// `git` exited with an error.
    #[error("git failed: {0}")]
    Git(String),

    /// A Git object or command result could not be parsed.
    #[error("{0}")]
    Message(String),
}
