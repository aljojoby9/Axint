//! Errors returned by the Axint core library.

use std::io;

/// Failure produced by hashing, manifests, merge, or the operation log.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// A digest slice was not 32 bytes long.
    #[error("invalid axint hash length: expected 32 bytes, got {length}")]
    InvalidHashLength {
        /// Number of bytes that were supplied.
        length: usize,
    },

    /// Hex input was not a 64-digit BLAKE3 digest.
    #[error("invalid hexadecimal axint hash")]
    InvalidHashHex,

    /// A manifest broke a structural rule.
    #[error("manifest invariant violated: {0}")]
    ManifestInvariant(&'static str),

    /// Protobuf decoding failed.
    #[error("protobuf decode failed: {0}")]
    Decode(#[from] prost::DecodeError),

    /// An underlying read or write failed.
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    /// A change-id string was not a UUID.
    #[error("invalid change id `{value}`")]
    InvalidChangeId {
        /// The rejected text.
        value: String,
    },

    /// Undo or redo moved past the ends of the journal.
    #[error("operation log has no entry to {action}")]
    OplogBoundary {
        /// Either `undo` or `redo`.
        action: &'static str,
    },

    /// The on-disk journal could not be parsed.
    #[error("operation journal is corrupt: {0}")]
    CorruptJournal(&'static str),

    /// An Ed25519 key or signature was rejected.
    #[error("signature error: {0}")]
    Signature(String),

    /// The stored commit id does not match the signed preimage.
    #[error("commit id does not match the manifest preimage")]
    CommitIdMismatch,

    /// The object store does not contain this digest.
    #[error("missing object {hash}")]
    MissingObject {
        /// Hex digest that was requested.
        hash: String,
    },

    /// An object was found, but it was a different kind.
    #[error("object {hash} has kind {found}, expected {expected}")]
    ObjectKind {
        /// Hex digest of the object.
        hash: String,
        /// Kind the caller asked for.
        expected: &'static str,
        /// Kind that is actually stored.
        found: &'static str,
    },

    /// A single path component cannot live in a tree manifest.
    #[error("invalid path component `{name}`")]
    InvalidPathComponent {
        /// The rejected component.
        name: String,
    },

    /// Two different commits tried to occupy one digest.
    #[error("commit {hash} is already recorded with a different payload")]
    DuplicateCommit {
        /// Hex digest of the conflicting commit.
        hash: String,
    },

    /// A caller-supplied failure that still has to cross the core trait boundary.
    #[error("{0}")]
    Message(String),
}

impl CoreError {
    /// Wraps a signature-crate failure.
    #[must_use]
    pub fn signature(err: impl std::fmt::Display) -> Self {
        Self::Signature(err.to_string())
    }
}
