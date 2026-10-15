//! Network errors.

/// A reconciliation failure.
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum NetError {
    /// The sketch could not be peeled. It was too small for the difference.
    #[error("set difference did not fit in the lookup table")]
    NotPeeled,

    /// A key was not 32 bytes.
    #[error("reconciliation key must be 32 bytes")]
    KeyLength,
}
