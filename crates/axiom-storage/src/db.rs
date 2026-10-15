//! Transactional metadata stored in `redb`.
//!
//! Readers use a read transaction. Writers take the single write transaction,
//! update one table, and commit. Records are fixed-width byte structs.

use std::fs;
use std::path::Path;

use axiom_core::AxiomHash;
use redb::{Database, ReadableTable, TableDefinition};
use zerocopy::{AsBytes, FromBytes, FromZeroes};

use crate::error::StorageError;

const PATHS: TableDefinition<&str, &[u8]> = TableDefinition::new("paths_to_inodes");
const BOOKMARKS: TableDefinition<&str, &[u8]> = TableDefinition::new("bookmarks");
const OPLOG: TableDefinition<u64, &[u8]> = TableDefinition::new("oplog");

/// The working tree matches the index.
pub const STATUS_CLEAN: u8 = 0;
/// The path was edited since the index was written.
pub const STATUS_MODIFIED: u8 = 1;
/// The path is not in HEAD.
pub const STATUS_UNTRACKED: u8 = 2;
/// The path was filled in by on-demand hydration.
pub const STATUS_HYDRATED: u8 = 3;

/// Fixed-width inode stored in `paths_to_inodes`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, FromBytes, AsBytes, FromZeroes)]
pub struct InodeRecord {
    /// File-manifest digest.
    pub blake3: [u8; 32],
    /// Modified time, seconds since the Unix epoch.
    pub mtime_sec: u64,
    /// Modified time, nanoseconds.
    pub mtime_nsec: u32,
    /// POSIX mode.
    pub mode: u32,
    /// File length in bytes.
    pub file_size: u64,
    /// [`STATUS_CLEAN`], [`STATUS_MODIFIED`], [`STATUS_UNTRACKED`], or [`STATUS_HYDRATED`].
    pub status_flag: u8,
    pad: [u8; 7],
}

impl InodeRecord {
    /// Builds a clean inode.
    #[must_use]
    pub fn clean(hash: AxiomHash, mtime_sec: u64, mtime_nsec: u32, file_size: u64, mode: u32) -> Self {
        Self {
            blake3: *hash.as_bytes(),
            mtime_sec,
            mtime_nsec,
            mode,
            file_size,
            status_flag: STATUS_CLEAN,
            pad: [0; 7],
        }
    }

    /// Manifest digest stored in this record.
    #[must_use]
    pub fn hash(&self) -> AxiomHash {
        AxiomHash::from_bytes(self.blake3)
    }
}

/// Open metadata database.
pub struct MetaDb {
    db: Database,
}

impl MetaDb {
    /// Opens `path`, creating it when missing.
    ///
    /// # Errors
    ///
    /// Returns an I/O or database error.
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let db = if path.exists() {
            Database::open(path).map_err(StorageError::database)?
        } else {
            Database::create(path).map_err(StorageError::database)?
        };
        let store = Self { db };
        store.ensure_tables()?;
        Ok(store)
    }

    fn ensure_tables(&self) -> Result<(), StorageError> {
        let write = self.db.begin_write().map_err(StorageError::database)?;
        {
            let _paths = write.open_table(PATHS).map_err(StorageError::database)?;
            let _bookmarks = write.open_table(BOOKMARKS).map_err(StorageError::database)?;
            let _oplog = write.open_table(OPLOG).map_err(StorageError::database)?;
        }
        write.commit().map_err(StorageError::database)?;
        Ok(())
    }

    /// Reads one inode.
    ///
    /// # Errors
    ///
    /// Returns a database error, or a corrupt-record error.
    pub fn inode(&self, path: &str) -> Result<Option<InodeRecord>, StorageError> {
        let read = self.db.begin_read().map_err(StorageError::database)?;
        let table = read.open_table(PATHS).map_err(StorageError::database)?;
        let Some(value) = table.get(path).map_err(StorageError::database)? else {
            return Ok(None);
        };
        decode_inode(value.value())
    }

    /// Inserts or replaces an inode.
    ///
    /// # Errors
    ///
    /// Returns a database error.
    pub fn put_inode(&self, path: &str, record: &InodeRecord) -> Result<(), StorageError> {
        let write = self.db.begin_write().map_err(StorageError::database)?;
        {
            let mut table = write.open_table(PATHS).map_err(StorageError::database)?;
            table.insert(path, record.as_bytes()).map_err(StorageError::database)?;
        }
        write.commit().map_err(StorageError::database)?;
        Ok(())
    }

    /// Removes an inode.
    ///
    /// # Errors
    ///
    /// Returns a database error.
    pub fn delete_inode(&self, path: &str) -> Result<(), StorageError> {
        let write = self.db.begin_write().map_err(StorageError::database)?;
        {
            let mut table = write.open_table(PATHS).map_err(StorageError::database)?;
            table.remove(path).map_err(StorageError::database)?;
        }
        write.commit().map_err(StorageError::database)?;
        Ok(())
    }

    /// Lists every indexed path.
    ///
    /// # Errors
    ///
    /// Returns a database error.
    pub fn inodes(&self) -> Result<Vec<(String, InodeRecord)>, StorageError> {
        let read = self.db.begin_read().map_err(StorageError::database)?;
        let table = read.open_table(PATHS).map_err(StorageError::database)?;
        let mut rows = Vec::new();
        for row in table.iter().map_err(StorageError::database)? {
            let (key, value) = row.map_err(StorageError::database)?;
            let record = decode_inode(value.value())?.ok_or_else(|| StorageError::Message("empty inode".to_owned()))?;
            rows.push((key.value().to_owned(), record));
        }
        Ok(rows)
    }

    /// Removes every inode and writes `records` in one transaction.
    ///
    /// # Errors
    ///
    /// Returns a database error.
    pub fn replace_inodes(&self, records: &[(String, InodeRecord)]) -> Result<(), StorageError> {
        let write = self.db.begin_write().map_err(StorageError::database)?;
        {
            let mut table = write.open_table(PATHS).map_err(StorageError::database)?;
            let existing: Vec<String> = table
                .iter()
                .map_err(StorageError::database)?
                .map(|row| row.map(|(key, _)| key.value().to_owned()))
                .collect::<Result<_, _>>()
                .map_err(StorageError::database)?;
            for path in existing {
                table.remove(path.as_str()).map_err(StorageError::database)?;
            }
            for (path, record) in records {
                table.insert(path.as_str(), record.as_bytes()).map_err(StorageError::database)?;
            }
        }
        write.commit().map_err(StorageError::database)?;
        Ok(())
    }

    /// Reads a bookmark.
    ///
    /// # Errors
    ///
    /// Returns a database error or a hash-width error.
    pub fn bookmark(&self, name: &str) -> Result<Option<AxiomHash>, StorageError> {
        let read = self.db.begin_read().map_err(StorageError::database)?;
        let table = read.open_table(BOOKMARKS).map_err(StorageError::database)?;
        let Some(value) = table.get(name).map_err(StorageError::database)? else {
            return Ok(None);
        };
        Ok(Some(AxiomHash::try_from_slice(value.value())?))
    }

    /// Removes a bookmark.
    ///
    /// # Errors
    ///
    /// Returns a database error.
    pub fn delete_bookmark(&self, name: &str) -> Result<(), StorageError> {
        let write = self.db.begin_write().map_err(StorageError::database)?;
        {
            let mut table = write.open_table(BOOKMARKS).map_err(StorageError::database)?;
            table.remove(name).map_err(StorageError::database)?;
        }
        write.commit().map_err(StorageError::database)?;
        Ok(())
    }

    /// Writes a bookmark.
    ///
    /// # Errors
    ///
    /// Returns a database error.
    pub fn put_bookmark(&self, name: &str, commit: &AxiomHash) -> Result<(), StorageError> {
        let write = self.db.begin_write().map_err(StorageError::database)?;
        {
            let mut table = write.open_table(BOOKMARKS).map_err(StorageError::database)?;
            table.insert(name, commit.as_bytes().as_slice()).map_err(StorageError::database)?;
        }
        write.commit().map_err(StorageError::database)?;
        Ok(())
    }

    /// Lists bookmarks.
    ///
    /// # Errors
    ///
    /// Returns a database error.
    pub fn bookmarks(&self) -> Result<Vec<(String, AxiomHash)>, StorageError> {
        let read = self.db.begin_read().map_err(StorageError::database)?;
        let table = read.open_table(BOOKMARKS).map_err(StorageError::database)?;
        let mut rows = Vec::new();
        for row in table.iter().map_err(StorageError::database)? {
            let (key, value) = row.map_err(StorageError::database)?;
            rows.push((key.value().to_owned(), AxiomHash::try_from_slice(value.value())?));
        }
        Ok(rows)
    }

    /// Appends an operation-log payload under `sequence`.
    ///
    /// # Errors
    ///
    /// Returns a database error.
    pub fn put_oplog(&self, sequence: u64, bytes: &[u8]) -> Result<(), StorageError> {
        let write = self.db.begin_write().map_err(StorageError::database)?;
        {
            let mut table = write.open_table(OPLOG).map_err(StorageError::database)?;
            table.insert(sequence, bytes).map_err(StorageError::database)?;
        }
        write.commit().map_err(StorageError::database)?;
        Ok(())
    }
}

fn decode_inode(bytes: &[u8]) -> Result<Option<InodeRecord>, StorageError> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let record = InodeRecord::read_from(bytes)
        .ok_or_else(|| StorageError::Message("inode record has the wrong width".to_owned()))?;
    Ok(Some(record))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inode_and_bookmark_round_trip() -> Result<(), StorageError> {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = MetaDb::open(&dir.path().join("index.db"))?;
        let hash = AxiomHash::hash_bytes(b"file");
        db.put_inode("src/main.rs", &InodeRecord::clean(hash, 10, 20, 4, 0o100_644))?;
        let loaded = db.inode("src/main.rs")?.expect("inode");
        assert_eq!(loaded.hash(), hash);
        assert_eq!(loaded.mtime_nsec, 20);
        db.put_bookmark("main", &hash)?;
        assert_eq!(db.bookmark("main")?, Some(hash));
        db.put_oplog(1, b"entry")?;
        Ok(())
    }
}
