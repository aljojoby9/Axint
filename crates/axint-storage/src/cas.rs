//! Content-addressed blocks.
//!
//! A loose block is stored at `.axint/cas/blocks/xx/<rest>.axc`. The frame is
//! magic, a compression flag, the raw size, the BLAKE3 digest, the payload,
//! and a CRC32C of everything before the checksum.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use axint_core::{AxintHash, ChunkReference};

use crate::compression::{compress, decompress};
use crate::error::StorageError;

const MAGIC: &[u8; 4] = b"AXCK";
const FLAG_RAW: u8 = 0;
const FLAG_ZSTD: u8 = 1;
const HEADER_LEN: usize = 4 + 1 + 4 + 32;

/// Writes one uncompressed chunk and returns its manifest reference.
///
/// An existing block for the same digest is left in place.
///
/// # Errors
///
/// Returns an I/O, compression, or framing error.
pub fn write_chunk(root: &Path, chunk: &[u8], level: i32) -> Result<ChunkReference, StorageError> {
    let hash = AxintHash::hash_bytes(chunk);
    let path = loose_path(root, &hash);
    if path.exists() {
        return reference_for(&hash, chunk.len(), chunk.len());
    }
    let compressed = compress(chunk, level)?;
    let (flag, payload) = if compressed.len() < chunk.len() {
        (FLAG_ZSTD, compressed)
    } else {
        (FLAG_RAW, chunk.to_vec())
    };
    let frame = encode_frame(flag, chunk.len(), &hash, &payload)?;
    write_new(&path, &frame)?;
    reference_for(&hash, payload.len(), chunk.len())
}

/// Reads a chunk from the loose store or from a pack.
///
/// # Errors
///
/// Returns an error when the block is missing or corrupt.
pub fn read_chunk(root: &Path, hash: &AxintHash) -> Result<Vec<u8>, StorageError> {
    let loose = loose_path(root, hash);
    if loose.exists() {
        let frame = fs::read(&loose)?;
        return decode_frame(&frame, hash);
    }
    if let Some(bytes) = read_packed(root, hash)? {
        return Ok(bytes);
    }
    Err(StorageError::CorruptBlock(format!("missing block {hash}")))
}

/// Copies loose blocks into a new `.axp` pack and a sorted `.idx`.
///
/// # Errors
///
/// Returns an I/O or framing error. Existing loose blocks are removed only
/// after the pack and index have been written.
pub fn pack_loose(root: &Path) -> Result<Option<PathBuf>, StorageError> {
    let blocks = loose_path(root, &AxintHash::ZERO);
    let blocks_root = blocks
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| StorageError::Message("cas block path has no parent".to_owned()))?;
    if !blocks_root.exists() {
        return Ok(None);
    }
    let mut frames = Vec::new();
    collect_loose(&blocks_root, &mut frames)?;
    if frames.is_empty() {
        return Ok(None);
    }
    frames.sort_by_key(|left| left.0);
    let packs = root.join(".axint").join("cas").join("packs");
    fs::create_dir_all(&packs)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| StorageError::Message("system clock is invalid".to_owned()))?
        .as_millis();
    let stem = format!("pack-{stamp}");
    let pack_path = packs.join(format!("{stem}.axp"));
    let index_path = packs.join(format!("{stem}.idx"));
    write_pack(&pack_path, &index_path, &frames)?;
    for (hash, _) in &frames {
        let path = loose_path(root, hash);
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    Ok(Some(pack_path))
}

fn reference_for(hash: &AxintHash, compressed: usize, raw: usize) -> Result<ChunkReference, StorageError> {
    let compressed_size = u32::try_from(compressed)
        .map_err(|_| StorageError::Message("compressed chunk exceeds 4 GiB".to_owned()))?;
    let uncompressed_size =
        u32::try_from(raw).map_err(|_| StorageError::Message("chunk exceeds 4 GiB".to_owned()))?;
    Ok(ChunkReference {
        chunk_blake3: hash.as_bytes().to_vec(),
        compressed_size,
        uncompressed_size,
    })
}

fn encode_frame(flag: u8, raw_len: usize, hash: &AxintHash, payload: &[u8]) -> Result<Vec<u8>, StorageError> {
    let raw_len = u32::try_from(raw_len).map_err(|_| StorageError::Message("chunk exceeds 4 GiB".to_owned()))?;
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len() + 4);
    frame.extend_from_slice(MAGIC);
    frame.push(flag);
    frame.extend_from_slice(&raw_len.to_le_bytes());
    frame.extend_from_slice(hash.as_bytes());
    frame.extend_from_slice(payload);
    let checksum = crc32c::crc32c(&frame);
    frame.extend_from_slice(&checksum.to_le_bytes());
    Ok(frame)
}

fn decode_frame(frame: &[u8], expected: &AxintHash) -> Result<Vec<u8>, StorageError> {
    if frame.len() < HEADER_LEN + 4 || &frame[0..4] != MAGIC {
        return Err(StorageError::CorruptBlock(expected.to_string()));
    }
    let (body, checksum_bytes) = frame.split_at(frame.len() - 4);
    let stored = u32::from_le_bytes(copy4(checksum_bytes)?);
    if crc32c::crc32c(body) != stored {
        return Err(StorageError::CorruptBlock(format!("{expected} checksum")));
    }
    let flag = body[4];
    let raw_len = u32::from_le_bytes(copy4(&body[5..9])?);
    let hash = AxintHash::try_from_slice(&body[9..41])?;
    if hash != *expected {
        return Err(StorageError::CorruptBlock(format!("frame hash {hash} != {expected}")));
    }
    let payload = &body[41..];
    let bytes = match flag {
        FLAG_RAW => payload.to_vec(),
        FLAG_ZSTD => decompress(payload, usize::try_from(raw_len).unwrap_or(0))?,
        _ => return Err(StorageError::CorruptBlock(format!("unknown flag {flag}"))),
    };
    if AxintHash::hash_bytes(&bytes) != hash {
        return Err(StorageError::CorruptBlock(format!("{expected} content mismatch")));
    }
    Ok(bytes)
}

fn copy4(bytes: &[u8]) -> Result<[u8; 4], StorageError> {
    let slice = bytes
        .get(..4)
        .ok_or_else(|| StorageError::CorruptBlock("short frame field".to_owned()))?;
    slice
        .try_into()
        .map_err(|_| StorageError::CorruptBlock("short frame field".to_owned()))
}

pub(crate) fn loose_path(root: &Path, hash: &AxintHash) -> PathBuf {
    let hex = hash.to_hex();
    root.join(".axint")
        .join("cas")
        .join("blocks")
        .join(&hex[..2])
        .join(format!("{}.axc", &hex[2..]))
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), StorageError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() {
        return Ok(());
    }
    let temporary = path.with_extension("tmp");
    {
        let mut file = File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(temporary, path)?;
    Ok(())
}

fn collect_loose(dir: &Path, out: &mut Vec<(AxintHash, Vec<u8>)>) -> Result<(), StorageError> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_loose(&path, out)?;
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("axc") {
            continue;
        }
        let frame = fs::read(&path)?;
        if frame.len() < HEADER_LEN {
            continue;
        }
        let hash = AxintHash::try_from_slice(&frame[9..41])?;
        out.push((hash, frame));
    }
    Ok(())
}

fn write_pack(pack_path: &Path, index_path: &Path, frames: &[(AxintHash, Vec<u8>)]) -> Result<(), StorageError> {
    let mut pack = File::create(pack_path)?;
    let mut index = Vec::new();
    index.extend_from_slice(b"AXIX");
    index.extend_from_slice(&1_u32.to_le_bytes());
    let count = u64::try_from(frames.len()).unwrap_or(0);
    index.extend_from_slice(&count.to_le_bytes());
    let mut offset = 0_u64;
    for (hash, frame) in frames {
        pack.write_all(frame)?;
        let length = u64::try_from(frame.len()).unwrap_or(0);
        index.extend_from_slice(hash.as_bytes());
        index.extend_from_slice(&offset.to_le_bytes());
        index.extend_from_slice(&length.to_le_bytes());
        offset = offset.saturating_add(length);
    }
    pack.sync_all()?;
    fs::write(index_path, index)?;
    Ok(())
}

fn read_packed(root: &Path, hash: &AxintHash) -> Result<Option<Vec<u8>>, StorageError> {
    let packs = root.join(".axint").join("cas").join("packs");
    if !packs.exists() {
        return Ok(None);
    }
    for entry in fs::read_dir(packs)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
            continue;
        }
        if let Some(frame) = read_index_hit(&path, hash)? {
            return Ok(Some(decode_frame(&frame, hash)?));
        }
    }
    Ok(None)
}

fn read_index_hit(index_path: &Path, hash: &AxintHash) -> Result<Option<Vec<u8>>, StorageError> {
    let index = fs::read(index_path)?;
    if index.len() < 16 || &index[0..4] != b"AXIX" {
        return Err(StorageError::CorruptBlock(index_path.display().to_string()));
    }
    let count = u64::from_le_bytes(copy8(&index[8..16])?);
    let count_usize = usize::try_from(count).unwrap_or(0);
    let entries = &index[16..];
    let stride = 32 + 8 + 8;
    if entries.len() < count_usize.saturating_mul(stride) {
        return Err(StorageError::CorruptBlock("short pack index".to_owned()));
    }
    let mut low = 0_usize;
    let mut high = count_usize;
    while low < high {
        let mid = low + (high - low) / 2;
        let start = mid * stride;
        let key = AxintHash::try_from_slice(&entries[start..start + 32])?;
        match key.cmp(hash) {
            std::cmp::Ordering::Equal => {
                let offset = u64::from_le_bytes(copy8(&entries[start + 32..start + 40])?);
                let length = u64::from_le_bytes(copy8(&entries[start + 40..start + 48])?);
                let pack_path = index_path.with_extension("axp");
                return read_range(&pack_path, offset, length).map(Some);
            }
            std::cmp::Ordering::Less => low = mid + 1,
            std::cmp::Ordering::Greater => high = mid,
        }
    }
    Ok(None)
}

fn copy8(bytes: &[u8]) -> Result<[u8; 8], StorageError> {
    let slice = bytes
        .get(..8)
        .ok_or_else(|| StorageError::CorruptBlock("short index field".to_owned()))?;
    slice
        .try_into()
        .map_err(|_| StorageError::CorruptBlock("short index field".to_owned()))
}

fn read_range(path: &Path, offset: u64, length: u64) -> Result<Vec<u8>, StorageError> {
    let mut file = File::open(path)?;
    let start = usize::try_from(offset).map_err(|_| StorageError::CorruptBlock("offset overflow".to_owned()))?;
    let len = usize::try_from(length).map_err(|_| StorageError::CorruptBlock("length overflow".to_owned()))?;
    let mut skipped = 0_usize;
    let mut waste = [0_u8; 8192];
    while skipped < start {
        let want = (start - skipped).min(waste.len());
        let read = file.read(&mut waste[..want])?;
        if read == 0 {
            return Err(StorageError::CorruptBlock("pack ended early".to_owned()));
        }
        skipped += read;
    }
    let mut buffer = vec![0_u8; len];
    file.read_exact(&mut buffer)?;
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loose_block_round_trip_and_pack() -> Result<(), StorageError> {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let payload = b"axint-cas-payload-0123456789";
        let reference = write_chunk(root, payload, 3)?;
        let hash = AxintHash::try_from_slice(&reference.chunk_blake3)?;
        assert_eq!(read_chunk(root, &hash)?, payload);
        let packed = pack_loose(root)?;
        assert!(packed.is_some());
        assert!(!loose_path(root, &hash).exists());
        assert_eq!(read_chunk(root, &hash)?, payload);
        Ok(())
    }
}
