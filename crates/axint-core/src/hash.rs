//! BLAKE3 identities used by every Axint object.
//!
//! Hashing is streaming. A file is never loaded as one allocation just to be
//! digested. Inputs of at least 128 KiB use BLAKE3's parallel SIMD tree hasher.

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use zerocopy::{AsBytes, FromBytes, FromZeroes};

use crate::error::CoreError;

/// Length of an Axint digest in bytes.
pub const HASH_LEN: usize = 32;

/// Parallel tree hashing starts at this size.
pub const PARALLEL_THRESHOLD: usize = 128 * 1024;

/// Read size for the streaming hasher.
const STREAM_BUFFER: usize = 64 * 1024;

/// Files at least this large are hashed through a memory map.
const MMAP_THRESHOLD: u64 = 1024 * 1024;

/// A 256-bit BLAKE3 digest.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, AsBytes, FromBytes, FromZeroes, Serialize, Deserialize)]
#[repr(transparent)]
pub struct AxintHash(pub(crate) [u8; HASH_LEN]);

impl AxintHash {
    /// The all-zero digest. It is not the hash of empty input.
    pub const ZERO: Self = Self([0; HASH_LEN]);

    /// Wraps an existing digest.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; HASH_LEN]) -> Self {
        Self(bytes)
    }

    /// Copies a digest out of a slice.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidHashLength`] when `bytes` is not 32 bytes.
    pub fn try_from_slice(bytes: &[u8]) -> Result<Self, CoreError> {
        let array: [u8; HASH_LEN] = bytes
            .try_into()
            .map_err(|_| CoreError::InvalidHashLength { length: bytes.len() })?;
        Ok(Self(array))
    }

    /// Borrows the raw digest.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; HASH_LEN] {
        &self.0
    }

    /// Lower-case hexadecimal encoding.
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Parses a 64-digit hexadecimal digest.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidHashHex`] when the text is not a 32-byte hex digest.
    pub fn from_hex(text: &str) -> Result<Self, CoreError> {
        let decoded = hex::decode(text.trim()).map_err(|_| CoreError::InvalidHashHex)?;
        Self::try_from_slice(&decoded).map_err(|_| CoreError::InvalidHashHex)
    }

    /// Hashes `data`.
    ///
    /// Large slices use the parallel SIMD hasher. Small slices stay on one core
    /// because thread startup would dominate the digest.
    #[must_use]
    pub fn hash_bytes(data: &[u8]) -> Self {
        if data.len() >= PARALLEL_THRESHOLD {
            let mut hasher = blake3::Hasher::new();
            hasher.update_rayon(data);
            Self::from_blake(hasher.finalize())
        } else {
            Self::from_blake(blake3::hash(data))
        }
    }

    /// Hashes a reader in fixed-size chunks.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error.
    pub fn hash_reader<R: Read>(reader: &mut R) -> Result<Self, CoreError> {
        let mut hasher = StreamingHasher::new();
        let mut buffer = vec![0_u8; STREAM_BUFFER];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(hasher.finalize())
    }

    /// Hashes a filesystem path.
    ///
    /// Files of at least 1 MiB are memory-mapped and hashed in parallel.
    /// Smaller files use the streaming reader so peak memory stays bounded.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error when the path cannot be read.
    pub fn hash_path(path: &Path) -> Result<Self, CoreError> {
        let file = File::open(path)?;
        let length = file.metadata()?.len();
        if length >= MMAP_THRESHOLD {
            let mut hasher = blake3::Hasher::new();
            hasher.update_mmap_rayon(path)?;
            return Ok(Self::from_blake(hasher.finalize()));
        }
        let mut file = file;
        Self::hash_reader(&mut file)
    }

    /// Starts a streaming hasher.
    #[must_use]
    pub fn stream() -> StreamingHasher {
        StreamingHasher::new()
    }

    fn from_blake(hash: blake3::Hash) -> Self {
        Self(*hash.as_bytes())
    }
}

impl Default for AxintHash {
    fn default() -> Self {
        Self::ZERO
    }
}

impl AsRef<[u8]> for AxintHash {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl From<[u8; HASH_LEN]> for AxintHash {
    fn from(bytes: [u8; HASH_LEN]) -> Self {
        Self(bytes)
    }
}

impl TryFrom<&[u8]> for AxintHash {
    type Error = CoreError;

    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        Self::try_from_slice(bytes)
    }
}

impl fmt::Display for AxintHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

impl fmt::Debug for AxintHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "AxintHash({self})")
    }
}

impl FromStr for AxintHash {
    type Err = CoreError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::from_hex(text)
    }
}

/// Incremental BLAKE3 hasher with a bounded update buffer owned by the caller.
#[derive(Debug)]
pub struct StreamingHasher {
    inner: blake3::Hasher,
}

impl StreamingHasher {
    /// Creates an empty hasher.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: blake3::Hasher::new(),
        }
    }

    /// Absorbs the next chunk.
    pub fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    /// Finishes the digest.
    #[must_use]
    pub fn finalize(self) -> AxintHash {
        AxintHash::from_blake(self.inner.finalize())
    }
}

impl Default for StreamingHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl io::Write for StreamingHasher {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.update(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn empty_input_matches_blake3_test_vector() {
        let hash = AxintHash::hash_bytes(b"");
        assert_eq!(
            hash.to_hex(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn reader_matches_slice_hash() -> Result<(), CoreError> {
        let data = vec![7_u8; 200_000];
        let direct = AxintHash::hash_bytes(&data);
        let streamed = AxintHash::hash_reader(&mut data.as_slice())?;
        assert_eq!(direct, streamed);
        Ok(())
    }

    #[test]
    fn hex_round_trip() -> Result<(), CoreError> {
        let hash = AxintHash::hash_bytes(b"axint");
        let parsed = AxintHash::from_hex(&hash.to_hex())?;
        assert_eq!(hash, parsed);
        assert!(AxintHash::from_hex("abcd").is_err());
        assert!(AxintHash::try_from_slice(&[0; 16]).is_err());
        Ok(())
    }

    #[test]
    fn zerocopy_and_json_round_trip() -> Result<(), CoreError> {
        let hash = AxintHash::hash_bytes(b"zerocopy");
        let bytes = hash.as_bytes();
        let restored = AxintHash::read_from(bytes.as_slice()).expect("32-byte digest");
        assert_eq!(hash, restored);
        let json = serde_json::to_string(&hash).expect("serialize");
        let parsed: AxintHash = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(hash, parsed);
        Ok(())
    }

    #[test]
    fn path_hash_matches_bytes() -> Result<(), CoreError> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("blob.bin");
        {
            let mut file = File::create(&path)?;
            file.write_all(&[9_u8; 4096])?;
        }
        let from_path = AxintHash::hash_path(&path)?;
        let data = std::fs::read(&path)?;
        assert_eq!(from_path, AxintHash::hash_bytes(&data));
        Ok(())
    }

    #[test]
    fn hash_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AxintHash>();
        assert_send_sync::<StreamingHasher>();
    }
}
