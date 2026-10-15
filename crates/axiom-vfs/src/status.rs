//! O(delta) status classification.
//!
//! The caller supplies only the paths the watcher marked dirty. Unchanged
//! size and mtime skip the content hash.

use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use crate::error::VfsError;

/// Cached inode fields needed to decide whether a path changed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IndexStat {
    /// Length recorded in the index.
    pub size: u64,
    /// Modified time in seconds.
    pub mtime_sec: u64,
    /// Modified time in nanoseconds.
    pub mtime_nsec: u32,
}

/// Result of comparing one dirty path with the index.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathStatus {
    /// The path is not in the index.
    Added,
    /// Size or mtime changed. The caller should hash the bytes.
    NeedsHash,
    /// The caller already proved the bytes differ.
    Modified,
    /// The indexed path is gone.
    Deleted,
    /// Size and mtime still match the index.
    Clean,
}

/// Compares `path` with the index entry.
///
/// # Errors
///
/// Returns an I/O error when the path exists but its metadata cannot be read.
pub fn compare_path(path: &Path, indexed: Option<IndexStat>) -> Result<PathStatus, VfsError> {
    let exists = path.exists();
    match (exists, indexed) {
        (false, None) => Ok(PathStatus::Clean),
        (false, Some(_)) => Ok(PathStatus::Deleted),
        (true, None) => Ok(PathStatus::Added),
        (true, Some(stat)) => {
            let meta = fs::symlink_metadata(path)?;
            let (seconds, nanos) = stamp(&meta)?;
            if meta.len() != stat.size {
                return Ok(PathStatus::NeedsHash);
            }
            if seconds == stat.mtime_sec && nanos == stat.mtime_nsec {
                Ok(PathStatus::Clean)
            } else {
                Ok(PathStatus::NeedsHash)
            }
        }
    }
}

fn stamp(meta: &fs::Metadata) -> Result<(u64, u32), VfsError> {
    let modified = meta.modified()?;
    let Ok(elapsed) = modified.duration_since(UNIX_EPOCH) else {
        return Ok((0, 0));
    };
    Ok((elapsed.as_secs(), elapsed.subsec_nanos()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_indexed_path_is_deleted() -> Result<(), VfsError> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("gone.txt");
        let status = compare_path(
            &path,
            Some(IndexStat {
                size: 1,
                mtime_sec: 1,
                mtime_nsec: 0,
            }),
        )?;
        assert_eq!(status, PathStatus::Deleted);
        Ok(())
    }

    #[test]
    fn new_file_is_added() -> Result<(), VfsError> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("new.txt");
        fs::write(&path, b"hi")?;
        assert_eq!(compare_path(&path, None)?, PathStatus::Added);
        Ok(())
    }
}
