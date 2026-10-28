//! Append-only operation journal.
//!
//! Every mutating command records the head it replaced and the head it
//! installed. `undo` and `redo` move a cursor and patch an 8-byte header
//! field. They do not rewrite the log body, so the transition stays a
//! pointer update.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::CoreError;
use crate::hash::AxintHash;
use crate::manifest::unix_millis;

const MAGIC: &[u8; 4] = b"AXOL";
const VERSION: u32 = 1;
const HEADER_LEN: u64 = 24;
const MAX_TEXT: usize = 1024 * 1024;

/// Kind of repository mutation stored in the journal.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[repr(u8)]
pub enum OperationKind {
    /// A new commit was recorded.
    Commit = 1,
    /// History was rebased.
    Rebase = 2,
    /// Two bookmarks were merged.
    Merge = 3,
    /// A tree was restored.
    Restore = 4,
    /// The working bookmark changed.
    Switch = 5,
    /// The user undid an operation. The undo itself is not journaled.
    Undo = 6,
    /// The user redid an operation. The redo itself is not journaled.
    Redo = 7,
}

impl OperationKind {
    fn from_byte(byte: u8) -> Result<Self, CoreError> {
        Ok(match byte {
            1 => Self::Commit,
            2 => Self::Rebase,
            3 => Self::Merge,
            4 => Self::Restore,
            5 => Self::Switch,
            6 => Self::Undo,
            7 => Self::Redo,
            _ => return Err(CoreError::CorruptJournal("unknown operation kind")),
        })
    }
}

/// One journal record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationEntry {
    /// Unix milliseconds.
    pub timestamp_unix_ms: u64,
    /// What changed.
    pub op_type: OperationKind,
    /// Head before the operation.
    pub previous_head: AxintHash,
    /// Head after the operation.
    pub target_head: AxintHash,
    /// Head restored by undo. This is the reverse pointer.
    pub reverse_head: AxintHash,
    /// Bookmark the operation moved, when it moved one.
    pub bookmark: String,
    /// Human-readable summary.
    pub description: String,
}

impl OperationEntry {
    /// Builds a record whose undo target is `previous_head`.
    #[must_use]
    pub fn new(
        timestamp_unix_ms: u64,
        op_type: OperationKind,
        previous_head: AxintHash,
        target_head: AxintHash,
        bookmark: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            timestamp_unix_ms,
            op_type,
            previous_head,
            target_head,
            reverse_head: previous_head,
            bookmark: bookmark.into(),
            description: description.into(),
        }
    }
}

/// Cursor over an operation journal.
///
/// `cursor` is the number of applied entries. The current head is
/// `entries[cursor - 1].target_head`, or `base_head` when the cursor is zero.
#[derive(Debug)]
pub struct OpLog {
    base_head: AxintHash,
    entries: Vec<OperationEntry>,
    cursor: usize,
    file: Option<File>,
}

impl OpLog {
    /// Creates an in-memory journal.
    #[must_use]
    pub fn in_memory(base_head: AxintHash) -> Self {
        Self {
            base_head,
            entries: Vec::new(),
            cursor: 0,
            file: None,
        }
    }

    /// Opens or creates a journal file.
    ///
    /// # Errors
    ///
    /// Returns an I/O or corruption error.
    pub fn open(path: &Path, base_head: AxintHash) -> Result<Self, CoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        if file.metadata()?.len() == 0 {
            let fresh = Self {
                base_head,
                entries: Vec::new(),
                cursor: 0,
                file: Some(file),
            };
            // Rewrite needs the file in the struct. Split the write to avoid a partial move.
            drop(fresh.file);
            file = OpenOptions::new().read(true).write(true).open(path)?;
            let mut created = Self {
                base_head,
                entries: Vec::new(),
                cursor: 0,
                file: Some(file),
            };
            created.rewrite()?;
            return Ok(created);
        }
        let mut header = [0_u8; HEADER_LEN as usize];
        file.read_exact(&mut header)?;
        if &header[0..4] != MAGIC {
            return Err(CoreError::CorruptJournal("bad magic"));
        }
        let version = u32::from_le_bytes(copy_array(&header[4..8])?);
        if version != VERSION {
            return Err(CoreError::CorruptJournal("unsupported version"));
        }
        let cursor = usize_from_u64(u64::from_le_bytes(copy_array(&header[8..16])?))?;
        let count = usize_from_u64(u64::from_le_bytes(copy_array(&header[16..24])?))?;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            entries.push(read_entry(&mut file)?);
        }
        if cursor > entries.len() {
            return Err(CoreError::CorruptJournal("cursor past the last entry"));
        }
        Ok(Self {
            base_head,
            entries,
            cursor,
            file: Some(file),
        })
    }

    /// Head currently selected by the cursor.
    #[must_use]
    pub fn head(&self) -> AxintHash {
        if self.cursor == 0 {
            self.base_head
        } else {
            self.entries[self.cursor - 1].target_head
        }
    }

    /// Applied-entry count.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Number of stored entries, including ones undone.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Reports whether the journal has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Borrows the entry that `undo` would reverse.
    #[must_use]
    pub fn current(&self) -> Option<&OperationEntry> {
        self.cursor.checked_sub(1).and_then(|index| self.entries.get(index))
    }

    /// Appends an operation at the current time.
    ///
    /// Entries after the cursor are discarded. That is the standard undo stack.
    ///
    /// # Errors
    ///
    /// Returns an I/O or clock error.
    pub fn record_now(&mut self, entry: OperationEntry) -> Result<(), CoreError> {
        let mut stamped = entry;
        stamped.timestamp_unix_ms = unix_millis()?;
        self.record(stamped)
    }

    /// Appends an operation with a caller-supplied timestamp.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the journal file cannot be updated.
    pub fn record(&mut self, entry: OperationEntry) -> Result<(), CoreError> {
        self.entries.truncate(self.cursor);
        self.entries.push(entry);
        self.cursor = self.entries.len();
        self.rewrite()
    }

    /// Moves the cursor backward.
    ///
    /// The returned entry is the operation that was undone. Its `reverse_head`
    /// is the head to restore. Only the 8-byte cursor field is written.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::OplogBoundary`] when nothing is applied, or an I/O error.
    pub fn undo(&mut self) -> Result<&OperationEntry, CoreError> {
        if self.cursor == 0 {
            return Err(CoreError::OplogBoundary { action: "undo" });
        }
        self.cursor -= 1;
        self.persist_cursor()?;
        Ok(&self.entries[self.cursor])
    }

    /// Moves the cursor forward.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::OplogBoundary`] when there is nothing to redo, or an I/O error.
    pub fn redo(&mut self) -> Result<&OperationEntry, CoreError> {
        if self.cursor >= self.entries.len() {
            return Err(CoreError::OplogBoundary { action: "redo" });
        }
        self.cursor += 1;
        self.persist_cursor()?;
        let index = self.cursor - 1;
        Ok(&self.entries[index])
    }

    fn persist_cursor(&mut self) -> Result<(), CoreError> {
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        file.seek(SeekFrom::Start(8))?;
        file.write_all(&u64::try_from(self.cursor).unwrap_or(0).to_le_bytes())?;
        file.flush()?;
        Ok(())
    }

    fn rewrite(&mut self) -> Result<(), CoreError> {
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        file.seek(SeekFrom::Start(0))?;
        file.write_all(MAGIC)?;
        file.write_all(&VERSION.to_le_bytes())?;
        file.write_all(&u64_len(self.cursor)?.to_le_bytes())?;
        file.write_all(&u64_len(self.entries.len())?.to_le_bytes())?;
        for entry in &self.entries {
            write_entry(file, entry)?;
        }
        let end = file.stream_position()?;
        file.set_len(end)?;
        file.flush()?;
        Ok(())
    }
}

fn u64_len(value: usize) -> Result<u64, CoreError> {
    u64::try_from(value).map_err(|_| CoreError::CorruptJournal("journal length does not fit in u64"))
}

fn usize_from_u64(value: u64) -> Result<usize, CoreError> {
    usize::try_from(value).map_err(|_| CoreError::CorruptJournal("journal length does not fit in usize"))
}

fn copy_array<const N: usize>(bytes: &[u8]) -> Result<[u8; N], CoreError> {
    let slice = bytes
        .get(..N)
        .ok_or(CoreError::CorruptJournal("field is shorter than expected"))?;
    slice
        .try_into()
        .map_err(|_| CoreError::CorruptJournal("field is shorter than expected"))
}

fn write_entry(file: &mut File, entry: &OperationEntry) -> Result<(), CoreError> {
    let bytes = encode_entry(entry)?;
    let length = u32::try_from(bytes.len()).map_err(|_| CoreError::CorruptJournal("entry is larger than 4 GiB"))?;
    file.write_all(&length.to_le_bytes())?;
    file.write_all(&bytes)?;
    Ok(())
}

fn read_entry(file: &mut File) -> Result<OperationEntry, CoreError> {
    let mut length_bytes = [0_u8; 4];
    file.read_exact(&mut length_bytes)?;
    let length = usize::try_from(u32::from_le_bytes(length_bytes))
        .map_err(|_| CoreError::CorruptJournal("entry length does not fit"))?;
    if length > MAX_TEXT.saturating_mul(4) {
        return Err(CoreError::CorruptJournal("entry length is unreasonable"));
    }
    let mut bytes = vec![0_u8; length];
    file.read_exact(&mut bytes)?;
    decode_entry(&bytes)
}

fn encode_entry(entry: &OperationEntry) -> Result<Vec<u8>, CoreError> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&entry.timestamp_unix_ms.to_le_bytes());
    bytes.push(entry.op_type as u8);
    bytes.extend_from_slice(entry.previous_head.as_bytes());
    bytes.extend_from_slice(entry.target_head.as_bytes());
    bytes.extend_from_slice(entry.reverse_head.as_bytes());
    write_text(&mut bytes, &entry.bookmark)?;
    write_text(&mut bytes, &entry.description)?;
    Ok(bytes)
}

fn decode_entry(bytes: &[u8]) -> Result<OperationEntry, CoreError> {
    if bytes.len() < 8 + 1 + 32 + 32 + 32 {
        return Err(CoreError::CorruptJournal("entry is truncated"));
    }
    let timestamp_unix_ms = u64::from_le_bytes(copy_array(&bytes[0..8])?);
    let op_type = OperationKind::from_byte(bytes[8])?;
    let previous_head = AxintHash::try_from_slice(&bytes[9..41])?;
    let target_head = AxintHash::try_from_slice(&bytes[41..73])?;
    let reverse_head = AxintHash::try_from_slice(&bytes[73..105])?;
    let (bookmark, rest) = read_text(&bytes[105..])?;
    let (description, rest) = read_text(rest)?;
    if !rest.is_empty() {
        return Err(CoreError::CorruptJournal("entry has trailing bytes"));
    }
    Ok(OperationEntry {
        timestamp_unix_ms,
        op_type,
        previous_head,
        target_head,
        reverse_head,
        bookmark,
        description,
    })
}

fn write_text(bytes: &mut Vec<u8>, text: &str) -> Result<(), CoreError> {
    if text.len() > MAX_TEXT {
        return Err(CoreError::CorruptJournal("journal text is too long"));
    }
    let length = u32::try_from(text.len()).map_err(|_| CoreError::CorruptJournal("text length overflow"))?;
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(text.as_bytes());
    Ok(())
}

fn read_text(bytes: &[u8]) -> Result<(String, &[u8]), CoreError> {
    if bytes.len() < 4 {
        return Err(CoreError::CorruptJournal("missing text length"));
    }
    let length = usize::try_from(u32::from_le_bytes(copy_array(&bytes[0..4])?))
        .map_err(|_| CoreError::CorruptJournal("text length overflow"))?;
    if length > MAX_TEXT || bytes.len() < 4 + length {
        return Err(CoreError::CorruptJournal("text length is out of range"));
    }
    let text = std::str::from_utf8(&bytes[4..4 + length]).map_err(|_| CoreError::CorruptJournal("text is not utf-8"))?;
    Ok((text.to_owned(), &bytes[4 + length..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn entry(previous: u8, target: u8) -> OperationEntry {
        OperationEntry::new(
            1,
            OperationKind::Commit,
            AxintHash::from_bytes([previous; 32]),
            AxintHash::from_bytes([target; 32]),
            "main",
            "commit",
        )
    }

    #[test]
    fn undo_and_redo_move_the_head() -> Result<(), CoreError> {
        let base = AxintHash::ZERO;
        let mut log = OpLog::in_memory(base);
        log.record(entry(0, 1))?;
        log.record(entry(1, 2))?;
        assert_eq!(log.head(), AxintHash::from_bytes([2; 32]));
        let undone = log.undo()?;
        assert_eq!(undone.reverse_head, AxintHash::from_bytes([1; 32]));
        assert_eq!(log.head(), AxintHash::from_bytes([1; 32]));
        let redone = log.redo()?;
        assert_eq!(redone.target_head, AxintHash::from_bytes([2; 32]));
        assert_eq!(log.head(), AxintHash::from_bytes([2; 32]));
        log.undo()?;
        log.record(entry(1, 9))?;
        assert_eq!(log.len(), 2);
        assert!(log.redo().is_err());
        Ok(())
    }

    #[test]
    fn persisted_cursor_survives_reload() -> Result<(), CoreError> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.log");
        let base = AxintHash::ZERO;
        {
            let mut log = OpLog::open(&path, base)?;
            log.record(entry(0, 4))?;
            log.record(entry(4, 5))?;
            log.undo()?;
        }
        let log = OpLog::open(&path, base)?;
        assert_eq!(log.cursor(), 1);
        assert_eq!(log.head(), AxintHash::from_bytes([4; 32]));
        assert_eq!(log.len(), 2);
        Ok(())
    }

    #[test]
    fn undo_cursor_update_stays_under_two_milliseconds() -> Result<(), CoreError> {
        let mut log = OpLog::in_memory(AxintHash::ZERO);
        for value in 1..200 {
            log.record(entry(value - 1, value))?;
        }
        let started = Instant::now();
        for _ in 0..100 {
            log.undo()?;
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed.as_micros() < 2_000,
            "100 in-memory undos took {} us",
            elapsed.as_micros()
        );
        Ok(())
    }
}
