//! Zstandard helpers.
//!
//! Interactive commits use level 3. Pack files use level 9.

use crate::error::StorageError;

/// Compression level for commits.
pub const COMMIT_LEVEL: i32 = 3;
/// Compression level for pack files.
pub const PACK_LEVEL: i32 = 9;

/// Compresses `bytes` at `level`.
///
/// # Errors
///
/// Returns the codec error.
pub fn compress(bytes: &[u8], level: i32) -> Result<Vec<u8>, StorageError> {
    zstd::bulk::compress(bytes, level).map_err(StorageError::Io)
}

/// Decompresses a frame into a buffer of `uncompressed_len` bytes.
///
/// # Errors
///
/// Returns an error when the frame is invalid or the size does not match.
pub fn decompress(bytes: &[u8], uncompressed_len: usize) -> Result<Vec<u8>, StorageError> {
    let payload = zstd::bulk::decompress(bytes, uncompressed_len).map_err(StorageError::Io)?;
    if payload.len() != uncompressed_len {
        return Err(StorageError::CorruptBlock("decompressed size mismatch".to_owned()));
    }
    Ok(payload)
}
