//! Normalized FastCDC.
//!
//! Chunk sizes follow the design: 16 KiB minimum, 64 KiB average, 128 KiB
//! maximum. The gear table is a fixed SplitMix64 stream so every build cuts
//! the same bytes at the same places.
//!
//! The cut length is the index `i` from the design note. The byte that tripped
//! the mask starts the next chunk. A short tail is emitted whole.

use std::io::Read;



/// Minimum chunk size.
pub const MIN_SIZE: usize = 16 * 1024;
/// Target average chunk size.
pub const AVG_SIZE: usize = 64 * 1024;
/// Maximum chunk size.
pub const MAX_SIZE: usize = 128 * 1024;
/// Mask used before the midpoint. The design pins this value.
pub const NORMAL_MASK: u64 = 0x0000_DFFF;
/// Mask used after the midpoint. The design pins this value.
pub const HARD_MASK: u64 = 0x0000_7FFF;

const GEAR: [u64; 256] = gear_table();

const fn mix(mut state: u64) -> (u64, u64) {
    state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut mixed = state;
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    mixed ^= mixed >> 31;
    (state, mixed)
}

const fn gear_table() -> [u64; 256] {
    let mut table = [0_u64; 256];
    let mut state = 0xA410_FCDC_0000_0001_u64;
    let mut index = 0;
    while index < 256 {
        let (next, value) = mix(state);
        state = next;
        table[index] = value;
        index += 1;
    }
    table
}

/// Returns the next chunk length inside `data`.
#[must_use]
pub fn cut_point(data: &[u8]) -> usize {
    let len = data.len();
    if len <= MIN_SIZE {
        return len;
    }
    let limit = len.min(MAX_SIZE);
    let midpoint = AVG_SIZE - (MIN_SIZE / 2);
    let mut fingerprint = 0_u64;
    let mut index = MIN_SIZE;
    while index < limit {
        let byte = data[index];
        fingerprint = (fingerprint << 1).wrapping_add(GEAR[usize::from(byte)]);
        let mask = if index < midpoint { NORMAL_MASK } else { HARD_MASK };
        if fingerprint & mask == 0 {
            return index;
        }
        index += 1;
    }
    limit
}

/// One chunk inside a larger buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChunkSpan {
    /// Start offset in the original buffer.
    pub offset: u64,
    /// Chunk length in bytes.
    pub length: usize,
}

/// Splits `data` into FastCDC spans. The spans cover every byte once.
#[must_use]
pub fn chunk_slice(data: &[u8]) -> Vec<ChunkSpan> {
    let mut spans = Vec::new();
    let mut offset = 0_usize;
    while offset < data.len() {
        let length = cut_point(&data[offset..]).clamp(1, data.len() - offset);
        let start = u64::try_from(offset).unwrap_or(u64::MAX);
        spans.push(ChunkSpan { offset: start, length });
        offset += length;
    }
    spans
}

/// Streams `reader` through FastCDC without holding more than one maximum chunk.
///
/// # Errors
///
/// Returns the reader error, or the error produced by `emit`.
pub fn chunk_reader<R, E>(reader: &mut R, mut emit: impl FnMut(&[u8]) -> Result<(), E>) -> Result<(), E>
where
    R: Read,
    E: From<std::io::Error>,
{
    let mut pending = Vec::with_capacity(MAX_SIZE);
    let mut incoming = [0_u8; 64 * 1024];
    loop {
        while pending.len() < MAX_SIZE {
            let read = reader.read(&mut incoming)?;
            if read == 0 {
                break;
            }
            pending.extend_from_slice(&incoming[..read]);
        }
        if pending.is_empty() {
            return Ok(());
        }
        let length = cut_point(&pending).clamp(1, pending.len());
        emit(&pending[..length])?;
        pending.drain(..length);
        if pending.is_empty() && reader_exhausted(reader, &mut incoming, &mut pending)? {
            return Ok(());
        }
    }
}

fn reader_exhausted<R: Read>(
    reader: &mut R,
    incoming: &mut [u8],
    pending: &mut Vec<u8>,
) -> Result<bool, std::io::Error> {
    let read = reader.read(incoming)?;
    if read == 0 {
        Ok(pending.is_empty())
    } else {
        pending.extend_from_slice(&incoming[..read]);
        Ok(false)
    }
}

/// Fills `output` with a deterministic byte stream.
pub fn fill_pattern(output: &mut [u8], seed: u64) {
    let mut state = seed;
    for byte in output {
        let (next, value) = {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut mixed = state;
            mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            mixed ^= mixed >> 31;
            (state, mixed)
        };
        state = next;
        *byte = u8::try_from(value & 0xFF).unwrap_or(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::StorageError;
    use std::collections::HashSet;
    use std::io::Cursor;
    use std::time::Instant;

    #[test]
    fn spans_cover_the_buffer_exactly() {
        let mut data = vec![0_u8; 300 * 1024];
        fill_pattern(&mut data, 7);
        let spans = chunk_slice(&data);
        let mut cursor = 0_u64;
        for span in &spans {
            assert_eq!(span.offset, cursor);
            assert!(span.length >= 1);
            assert!(span.length <= MAX_SIZE);
            if data.len() > MIN_SIZE {
                assert!(span.length >= MIN_SIZE || span.offset + u64::try_from(span.length).unwrap_or(0) == data.len() as u64);
            }
            cursor += u64::try_from(span.length).unwrap_or(0);
        }
        assert_eq!(cursor, data.len() as u64);
    }

    #[test]
    fn reader_matches_slice_chunking() -> Result<(), StorageError> {
        let mut data = vec![0_u8; 200 * 1024];
        fill_pattern(&mut data, 99);
        let expected: Vec<Vec<u8>> = chunk_slice(&data)
            .into_iter()
            .map(|span| data[span.offset as usize..span.offset as usize + span.length].to_vec())
            .collect();
        let mut got = Vec::new();
        chunk_reader(&mut Cursor::new(data), |chunk| {
            got.push(chunk.to_vec());
            Ok::<(), StorageError>(())
        })?;
        assert_eq!(expected, got);
        Ok(())
    }

    #[test]
    fn local_edit_preserves_most_chunks() {
        let mut original = vec![0_u8; 1024 * 1024];
        fill_pattern(&mut original, 42);
        let mut edited = original.clone();
        let start = 400 * 1024;
        for byte in &mut edited[start..start + 20 * 1024] {
            *byte ^= 0x5A;
        }
        let left: HashSet<_> = chunk_slice(&original)
            .into_iter()
            .map(|span| {
                axiom_core::AxiomHash::hash_bytes(&original[span.offset as usize..span.offset as usize + span.length])
            })
            .collect();
        let right: HashSet<_> = chunk_slice(&edited)
            .into_iter()
            .map(|span| {
                axiom_core::AxiomHash::hash_bytes(&edited[span.offset as usize..span.offset as usize + span.length])
            })
            .collect();
        let shared = left.intersection(&right).count();
        assert!(
            shared.saturating_mul(100) > left.len().saturating_mul(70),
            "shared chunks {shared} of {}",
            left.len()
        );
    }

    #[test]
    fn chunking_throughput_is_in_the_right_order() {
        let mut data = vec![0_u8; 8 * 1024 * 1024];
        fill_pattern(&mut data, 1);
        let started = Instant::now();
        let spans = chunk_slice(&data);
        let elapsed = started.elapsed();
        assert!(!spans.is_empty());
        // Debug builds are far from the 4 GB/s release budget. Eight mebibytes
        // finishing inside four seconds is enough to catch a broken loop.
        assert!(elapsed.as_secs() < 4, "chunking 8 MiB took {elapsed:?}");
    }
}
