//! On-demand hydration.
//!
//! A projected file is a list of chunks. `read_range` copies only the chunks
//! that overlap the requested bytes, which is the same request a ProjFS or
//! FUSE callback receives. `hydrate_bytes` streams those chunks to a writer
//! without building one buffer for the whole file.

use std::io::Write;

use crate::error::VfsError;

/// One chunk of a projected file, already decompressed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChunkSlice<'a> {
    /// Offset of this chunk in the logical file.
    pub offset: u64,
    /// Uncompressed bytes.
    pub bytes: &'a [u8],
}

/// Copies `buf`'s worth of logical bytes starting at `offset`.
///
/// # Errors
///
/// Returns [`VfsError::Range`] when `offset` is past the end of the file.
pub fn read_range(chunks: &[ChunkSlice<'_>], offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
    if buf.is_empty() {
        return Ok(0);
    }
    let mut filled = 0_usize;
    let mut saw_file = false;
    for chunk in chunks {
        let end = chunk.offset.saturating_add(u64::try_from(chunk.bytes.len()).unwrap_or(0));
        saw_file = true;
        if end <= offset || chunk.offset >= offset.saturating_add(u64::try_from(buf.len()).unwrap_or(0)) {
            continue;
        }
        let start_in_chunk = usize::try_from(offset.saturating_sub(chunk.offset)).unwrap_or(0);
        let start_in_buf = usize::try_from(chunk.offset.saturating_sub(offset)).unwrap_or(0);
        if start_in_chunk >= chunk.bytes.len() || start_in_buf >= buf.len() {
            continue;
        }
        let available = chunk.bytes.len() - start_in_chunk;
        let space = buf.len() - start_in_buf;
        let count = available.min(space);
        buf[start_in_buf..start_in_buf + count].copy_from_slice(&chunk.bytes[start_in_chunk..start_in_chunk + count]);
        filled = filled.max(start_in_buf + count);
    }
    if !saw_file && offset > 0 {
        return Err(VfsError::Range);
    }
    Ok(filled)
}

/// Writes every chunk, in order, to `output`.
///
/// # Errors
///
/// Returns the writer error.
pub fn hydrate_bytes<W: Write>(chunks: &[ChunkSlice<'_>], output: &mut W) -> Result<u64, VfsError> {
    let mut total = 0_u64;
    for chunk in chunks {
        output.write_all(chunk.bytes)?;
        total = total.saturating_add(u64::try_from(chunk.bytes.len()).unwrap_or(0));
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_reads_span_two_chunks() -> Result<(), VfsError> {
        let first = b"abcdefgh";
        let second = b"ijklmnop";
        let chunks = [
            ChunkSlice { offset: 0, bytes: first },
            ChunkSlice { offset: 8, bytes: second },
        ];
        let mut buf = [0_u8; 4];
        let count = read_range(&chunks, 6, &mut buf)?;
        assert_eq!(count, 4);
        assert_eq!(&buf, b"ghij");
        let mut whole = Vec::new();
        assert_eq!(hydrate_bytes(&chunks, &mut whole)?, 16);
        assert_eq!(whole, b"abcdefghijklmnop");
        Ok(())
    }
}
