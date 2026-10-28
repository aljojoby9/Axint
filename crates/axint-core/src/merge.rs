//! Algebraic three-way merge.
//!
//! Identical subtrees are skipped. When both sides edit the same path in an
//! incompatible way, the merge still produces a tree. The contested path points
//! at a [`ConflictManifest`] instead of aborting the commit.

use crate::error::CoreError;
use crate::hash::AxintHash;
use crate::manifest::{
    entry_type_for_mode, ConflictKind, ConflictManifest, EntryType, SpanConflict, TreeEntry, TreeManifest,
    SCHEMA_VERSION,
};
use crate::store::{FileRecord, ObjectStore};

/// Maximum line count for an automatic textual merge.
///
/// Larger files stay conflicted as a single span so a wrong auto-merge is
/// impossible and memory stays bounded.
const MAX_AUTO_MERGE_LINES: usize = 512;

/// A resolved node, or a conflict stored as its own object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NodeResolution {
    /// The node has one digest.
    Resolved(AxintHash),
    /// The node is a conflict object.
    Conflicted {
        /// Ancestor digest, when the path existed.
        ancestor: Option<AxintHash>,
        /// Left and right digests.
        sides: Vec<AxintHash>,
        /// Why the node conflicted.
        conflict_type: ConflictKind,
    },
}

/// Outcome of merging two trees against their ancestor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeOutput {
    /// Tree that contains resolved entries and conflict entries.
    pub tree_hash: AxintHash,
    /// Individual conflict objects, in path order.
    pub conflicts: Vec<AxintHash>,
    /// Commit-level conflict record. Present when `conflicts` is not empty.
    pub conflict_root: Option<AxintHash>,
}

/// Merges `left` and `right` against `ancestor`.
///
/// All three arguments are tree-manifest digests.
///
/// # Errors
///
/// Returns an error when a required object is missing or a manifest is invalid.
pub fn merge_trees<S: ObjectStore>(
    store: &mut S,
    ancestor: &AxintHash,
    left: &AxintHash,
    right: &AxintHash,
) -> Result<MergeOutput, CoreError> {
    let mut conflicts = Vec::new();
    let tree_hash = merge_directory(store, "", ancestor, left, right, &mut conflicts)?;
    let conflict_root = if conflicts.is_empty() {
        None
    } else {
        let record = ConflictManifest {
            schema_version: SCHEMA_VERSION,
            ancestor: Some(ancestor.as_bytes().to_vec()),
            sides: vec![left.as_bytes().to_vec(), right.as_bytes().to_vec()],
            kind: ConflictKind::MergeSet as i32,
            path: String::new(),
            spans: Vec::new(),
            nested: conflicts.iter().map(|hash| hash.as_bytes().to_vec()).collect(),
            left_mode: 0,
            right_mode: 0,
            ancestor_mode: 0,
        };
        Some(store.put_conflict(&record)?)
    };
    Ok(MergeOutput {
        tree_hash,
        conflicts,
        conflict_root,
    })
}

fn merge_directory<S: ObjectStore>(
    store: &mut S,
    path: &str,
    ancestor: &AxintHash,
    left: &AxintHash,
    right: &AxintHash,
    conflicts: &mut Vec<AxintHash>,
) -> Result<AxintHash, CoreError> {
    if left == right || right == ancestor {
        return Ok(*left);
    }
    if left == ancestor {
        return Ok(*right);
    }
    let ancestor_tree = store.get_tree(ancestor)?;
    let left_tree = store.get_tree(left)?;
    let right_tree = store.get_tree(right)?;
    let names = union_names(&ancestor_tree, &left_tree, &right_tree);
    let mut entries = Vec::with_capacity(names.len());
    for name in names {
        let child_path = child_path(path, &name);
        if let Some(entry) = merge_named_entry(
            store,
            &child_path,
            find_entry(&ancestor_tree, &name),
            find_entry(&left_tree, &name),
            find_entry(&right_tree, &name),
            conflicts,
        )? {
            entries.push(entry);
        }
    }
    let tree = TreeManifest {
        schema_version: SCHEMA_VERSION,
        entries,
    };
    store.put_tree(&tree)
}

fn merge_named_entry<S: ObjectStore>(
    store: &mut S,
    path: &str,
    ancestor: Option<&TreeEntry>,
    left: Option<&TreeEntry>,
    right: Option<&TreeEntry>,
    conflicts: &mut Vec<AxintHash>,
) -> Result<Option<TreeEntry>, CoreError> {
    match (left, right) {
        (None, None) => Ok(None),
        (Some(kept), None) => one_side(store, path, ancestor, kept, true, conflicts),
        (None, Some(kept)) => one_side(store, path, ancestor, kept, false, conflicts),
        (Some(left_entry), Some(right_entry)) => {
            merge_both(store, path, ancestor, left_entry, right_entry, conflicts)
        }
    }
}

fn one_side<S: ObjectStore>(
    store: &mut S,
    path: &str,
    ancestor: Option<&TreeEntry>,
    kept: &TreeEntry,
    left_survived: bool,
    conflicts: &mut Vec<AxintHash>,
) -> Result<Option<TreeEntry>, CoreError> {
    let Some(base) = ancestor else {
        return Ok(Some(kept.clone()));
    };
    if entries_match(base, kept)? {
        return Ok(None);
    }
    let sides = if left_survived {
        (Some(kept), None)
    } else {
        (None, Some(kept))
    };
    let conflict = store_conflict(store, path, ConflictKind::DeleteModify, Some(base), sides, Vec::new())?;
    conflicts.push(conflict.hash()?);
    Ok(Some(conflict))
}

fn entries_match(left: &TreeEntry, right: &TreeEntry) -> Result<bool, CoreError> {
    Ok(left.hash()? == right.hash()? && left.kind()? == right.kind()? && left.permissions == right.permissions)
}

fn merge_both<S: ObjectStore>(
    store: &mut S,
    path: &str,
    ancestor: Option<&TreeEntry>,
    left: &TreeEntry,
    right: &TreeEntry,
    conflicts: &mut Vec<AxintHash>,
) -> Result<Option<TreeEntry>, CoreError> {
    if entries_match(left, right)? {
        return Ok(Some(left.clone()));
    }
    if let Some(base) = ancestor {
        if entries_match(base, left)? {
            return Ok(Some(right.clone()));
        }
        if entries_match(base, right)? {
            return Ok(Some(left.clone()));
        }
    }
    let left_kind = left.kind()?;
    let right_kind = right.kind()?;
    if is_directory(left_kind) != is_directory(right_kind) {
        return conflict_entry(store, path, ancestor, left, right, ConflictKind::FileDirectory, Vec::new(), conflicts);
    }
    if is_directory(left_kind) && is_directory(right_kind) {
        return merge_child_directories(store, path, ancestor, left, right, conflicts);
    }
    if left_kind == EntryType::Conflict || right_kind == EntryType::Conflict {
        return conflict_entry(store, path, ancestor, left, right, ConflictKind::Content, Vec::new(), conflicts);
    }
    merge_files(store, path, ancestor, left, right, conflicts)
}

fn merge_child_directories<S: ObjectStore>(
    store: &mut S,
    path: &str,
    ancestor: Option<&TreeEntry>,
    left: &TreeEntry,
    right: &TreeEntry,
    conflicts: &mut Vec<AxintHash>,
) -> Result<Option<TreeEntry>, CoreError> {
    let empty = empty_tree_hash(store)?;
    let ancestor_hash = match ancestor {
        Some(entry) if is_directory(entry.kind()?) => entry.hash()?,
        _ => empty,
    };
    let merged = merge_directory(store, path, &ancestor_hash, &left.hash()?, &right.hash()?, conflicts)?;
    let permissions = resolve_mode(ancestor.map(|entry| entry.permissions), left.permissions, right.permissions);
    let mode = permissions.unwrap_or(left.permissions);
    if permissions.is_none() {
        return conflict_entry(store, path, ancestor, left, right, ConflictKind::ModePermission, Vec::new(), conflicts);
    }
    Ok(Some(TreeEntry::new(
        name_of(path),
        EntryType::Directory,
        merged,
        0,
        mode,
    )?))
}

fn merge_files<S: ObjectStore>(
    store: &mut S,
    path: &str,
    ancestor: Option<&TreeEntry>,
    left: &TreeEntry,
    right: &TreeEntry,
    conflicts: &mut Vec<AxintHash>,
) -> Result<Option<TreeEntry>, CoreError> {
    let left_content = content_hash(store, left)?;
    let right_content = content_hash(store, right)?;
    let ancestor_content = ancestor.map(|entry| content_hash(store, entry)).transpose()?;
    let mode = resolve_mode(ancestor.map(|entry| entry.permissions), left.permissions, right.permissions);
    if left_content == right_content {
        return finish_mode(store, path, ancestor, left, right, left, mode, conflicts);
    }
    if let Some(base_content) = ancestor_content {
        if base_content == left_content {
            return finish_mode(store, path, ancestor, left, right, right, mode, conflicts);
        }
        if base_content == right_content {
            return finish_mode(store, path, ancestor, left, right, left, mode, conflicts);
        }
    }
    match try_text_merge(store, ancestor, left, right)? {
        TextMerge::Merged(record) => {
            let Some(mode) = mode else {
                return conflict_entry(
                    store,
                    path,
                    ancestor,
                    left,
                    right,
                    ConflictKind::ModePermission,
                    Vec::new(),
                    conflicts,
                );
            };
            let kind = entry_type_for_mode(mode);
            Ok(Some(TreeEntry::new(name_of(path), kind, record.manifest_hash, record.size, mode)?))
        }
        TextMerge::Conflict(spans) => {
            conflict_entry(store, path, ancestor, left, right, ConflictKind::Content, spans, conflicts)
        }
    }
}

fn finish_mode<S: ObjectStore>(
    store: &mut S,
    path: &str,
    ancestor: Option<&TreeEntry>,
    left: &TreeEntry,
    right: &TreeEntry,
    chosen: &TreeEntry,
    mode: Option<u32>,
    conflicts: &mut Vec<AxintHash>,
) -> Result<Option<TreeEntry>, CoreError> {
    let Some(mode) = mode else {
        return conflict_entry(
            store,
            path,
            ancestor,
            left,
            right,
            ConflictKind::ModePermission,
            Vec::new(),
            conflicts,
        );
    };
    let mut entry = chosen.clone();
    entry.name = name_of(path).to_owned();
    entry.permissions = mode;
    entry.entry_type = entry_type_for_mode(mode) as i32;
    Ok(Some(entry))
}

enum TextMerge {
    Merged(FileRecord),
    Conflict(Vec<SpanConflict>),
}

fn try_text_merge<S: ObjectStore>(
    store: &mut S,
    ancestor: Option<&TreeEntry>,
    left: &TreeEntry,
    right: &TreeEntry,
) -> Result<TextMerge, CoreError> {
    let Some(ancestor) = ancestor else {
        return Ok(TextMerge::Conflict(Vec::new()));
    };
    let ancestor_bytes = read_entry_bytes(store, ancestor)?;
    let left_bytes = read_entry_bytes(store, left)?;
    let right_bytes = read_entry_bytes(store, right)?;
    let (Some(ancestor_text), Some(left_text), Some(right_text)) = (
        as_merge_text(&ancestor_bytes),
        as_merge_text(&left_bytes),
        as_merge_text(&right_bytes),
    ) else {
        return Ok(TextMerge::Conflict(full_span(&ancestor_bytes, &left_bytes, &right_bytes)));
    };
    if ancestor_text.len() > MAX_AUTO_MERGE_LINES
        || left_text.len() > MAX_AUTO_MERGE_LINES
        || right_text.len() > MAX_AUTO_MERGE_LINES
    {
        return Ok(TextMerge::Conflict(full_span(&ancestor_bytes, &left_bytes, &right_bytes)));
    }
    let left_hunks = hunks_against(&ancestor_text, &left_text);
    let right_hunks = hunks_against(&ancestor_text, &right_text);
    match combine_hunks(&ancestor_text, &left_text, &right_text, &left_hunks, &right_hunks) {
        Ok(merged) => {
            let record = store.put_file_bytes(merged.as_bytes())?;
            Ok(TextMerge::Merged(record))
        }
        Err(spans) => Ok(TextMerge::Conflict(spans)),
    }
}

fn read_entry_bytes<S: ObjectStore>(store: &S, entry: &TreeEntry) -> Result<Vec<u8>, CoreError> {
    store.read_file_bytes(&entry.hash()?)
}

fn as_merge_text(bytes: &[u8]) -> Option<Vec<String>> {
    if bytes.contains(&0) {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    Some(split_lines(text))
}

fn split_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut rest = text;
    while let Some(index) = rest.find('\n') {
        lines.push(rest[..=index].to_owned());
        rest = &rest[index + 1..];
    }
    if !rest.is_empty() {
        lines.push(rest.to_owned());
    }
    lines
}

#[derive(Clone, Debug)]
struct Hunk {
    ancestor_start: usize,
    ancestor_end: usize,
    side_start: usize,
    side_end: usize,
}

fn hunks_against(ancestor: &[String], side: &[String]) -> Vec<Hunk> {
    let rows = ancestor.len() + 1;
    let cols = side.len() + 1;
    let mut scores = vec![0_u32; rows * cols];
    let index = |row: usize, col: usize| row * cols + col;
    let mut row = ancestor.len();
    while row > 0 {
        row -= 1;
        let mut col = side.len();
        while col > 0 {
            col -= 1;
            scores[index(row, col)] = if ancestor[row] == side[col] {
                scores[index(row + 1, col + 1)].saturating_add(1)
            } else {
                scores[index(row + 1, col)].max(scores[index(row, col + 1)])
            };
        }
    }
    let mut hunks = Vec::new();
    let mut current: Option<Hunk> = None;
    let mut ancestor_index = 0_usize;
    let mut side_index = 0_usize;
    while ancestor_index < ancestor.len() || side_index < side.len() {
        let matches = ancestor_index < ancestor.len()
            && side_index < side.len()
            && ancestor[ancestor_index] == side[side_index];
        if matches {
            if let Some(hunk) = current.take() {
                hunks.push(hunk);
            }
            ancestor_index += 1;
            side_index += 1;
            continue;
        }
        let insert = side_index < side.len()
            && (ancestor_index == ancestor.len()
                || scores[index(ancestor_index, side_index + 1)] >= scores[index(ancestor_index + 1, side_index)]);
        if insert {
            let hunk = current.get_or_insert(Hunk {
                ancestor_start: ancestor_index,
                ancestor_end: ancestor_index,
                side_start: side_index,
                side_end: side_index,
            });
            hunk.side_end = side_index + 1;
            side_index += 1;
        } else {
            let hunk = current.get_or_insert(Hunk {
                ancestor_start: ancestor_index,
                ancestor_end: ancestor_index,
                side_start: side_index,
                side_end: side_index,
            });
            hunk.ancestor_end = ancestor_index + 1;
            ancestor_index += 1;
        }
    }
    if let Some(hunk) = current {
        hunks.push(hunk);
    }
    hunks
}

fn combine_hunks(
    ancestor: &[String],
    left: &[String],
    right: &[String],
    left_hunks: &[Hunk],
    right_hunks: &[Hunk],
) -> Result<String, Vec<SpanConflict>> {
    let mut spans = Vec::new();
    let mut output = String::new();
    let mut cursor = 0_usize;
    let mut left_index = 0_usize;
    let mut right_index = 0_usize;
    while left_index < left_hunks.len() || right_index < right_hunks.len() {
        let left_hunk = left_hunks.get(left_index);
        let right_hunk = right_hunks.get(right_index);
        let next = match (left_hunk, right_hunk) {
            (Some(left_side), Some(right_side)) => left_side.ancestor_start.min(right_side.ancestor_start),
            (Some(left_side), None) => left_side.ancestor_start,
            (None, Some(right_side)) => right_side.ancestor_start,
            (None, None) => break,
        };
        if next < cursor {
            // A previous conflict already consumed this region.
            if left_hunk.is_some_and(|hunk| hunk.ancestor_start < cursor) {
                left_index += 1;
            }
            if right_hunk.is_some_and(|hunk| hunk.ancestor_start < cursor) {
                right_index += 1;
            }
            continue;
        }
        push_lines(&mut output, &ancestor[cursor..next]);
        let left_here = left_hunk.filter(|hunk| hunk.ancestor_start == next);
        let right_here = right_hunk.filter(|hunk| hunk.ancestor_start == next);
        match (left_here, right_here) {
            (Some(left_side), Some(right_side)) if ranges_overlap(left_side, right_side) => {
                let left_lines = &left[left_side.side_start..left_side.side_end];
                let right_lines = &right[right_side.side_start..right_side.side_end];
                let same_region = left_side.ancestor_start == right_side.ancestor_start
                    && left_side.ancestor_end == right_side.ancestor_end
                    && left_lines == right_lines;
                if same_region {
                    push_lines(&mut output, left_lines);
                    cursor = left_side.ancestor_end;
                } else {
                    spans.push(span_for(ancestor, left, right, left_side, right_side));
                    cursor = left_side.ancestor_end.max(right_side.ancestor_end);
                }
                left_index += 1;
                right_index += 1;
            }
            (Some(left_side), Some(right_side)) => {
                if left_side.ancestor_start <= right_side.ancestor_start {
                    push_lines(&mut output, &left[left_side.side_start..left_side.side_end]);
                    cursor = left_side.ancestor_end;
                    left_index += 1;
                } else {
                    push_lines(&mut output, &right[right_side.side_start..right_side.side_end]);
                    cursor = right_side.ancestor_end;
                    right_index += 1;
                }
            }
            (Some(left_side), None) => {
                push_lines(&mut output, &left[left_side.side_start..left_side.side_end]);
                cursor = left_side.ancestor_end;
                left_index += 1;
            }
            (None, Some(right_side)) => {
                push_lines(&mut output, &right[right_side.side_start..right_side.side_end]);
                cursor = right_side.ancestor_end;
                right_index += 1;
            }
            (None, None) => break,
        }
    }
    if spans.is_empty() {
        push_lines(&mut output, &ancestor[cursor..]);
        Ok(output)
    } else {
        Err(spans)
    }
}

fn ranges_overlap(left: &Hunk, right: &Hunk) -> bool {
    left.ancestor_start < right.ancestor_end && right.ancestor_start < left.ancestor_end
        || left.ancestor_start == right.ancestor_start
}

fn push_lines(output: &mut String, lines: &[String]) {
    for line in lines {
        output.push_str(line);
    }
}

fn span_for(ancestor: &[String], left: &[String], right: &[String], left_hunk: &Hunk, right_hunk: &Hunk) -> SpanConflict {
    SpanConflict {
        ancestor_start: byte_offset(ancestor, left_hunk.ancestor_start.min(right_hunk.ancestor_start)),
        ancestor_end: byte_offset(ancestor, left_hunk.ancestor_end.max(right_hunk.ancestor_end)),
        left_start: byte_offset(left, left_hunk.side_start),
        left_end: byte_offset(left, left_hunk.side_end),
        right_start: byte_offset(right, right_hunk.side_start),
        right_end: byte_offset(right, right_hunk.side_end),
    }
}

fn byte_offset(lines: &[String], index: usize) -> u64 {
    let bytes = lines.iter().take(index).map(String::len).sum::<usize>();
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

fn full_span(ancestor: &[u8], left: &[u8], right: &[u8]) -> Vec<SpanConflict> {
    vec![SpanConflict {
        ancestor_start: 0,
        ancestor_end: u64::try_from(ancestor.len()).unwrap_or(u64::MAX),
        left_start: 0,
        left_end: u64::try_from(left.len()).unwrap_or(u64::MAX),
        right_start: 0,
        right_end: u64::try_from(right.len()).unwrap_or(u64::MAX),
    }]
}

fn content_hash<S: ObjectStore>(store: &S, entry: &TreeEntry) -> Result<AxintHash, CoreError> {
    match entry.kind()? {
        EntryType::File | EntryType::Executable | EntryType::Symlink => {
            if let Ok(manifest) = store.get_file(&entry.hash()?) {
                return manifest.aggregate();
            }
            Ok(entry.hash()?)
        }
        EntryType::Directory | EntryType::Conflict => entry.hash(),
    }
}

fn resolve_mode(ancestor: Option<u32>, left: u32, right: u32) -> Option<u32> {
    if left == right {
        return Some(left);
    }
    match ancestor {
        Some(base) if left == base => Some(right),
        Some(base) if right == base => Some(left),
        None => None,
        Some(_) => None,
    }
}

fn is_directory(kind: EntryType) -> bool {
    kind == EntryType::Directory
}

fn conflict_entry<S: ObjectStore>(
    store: &mut S,
    path: &str,
    ancestor: Option<&TreeEntry>,
    left: &TreeEntry,
    right: &TreeEntry,
    kind: ConflictKind,
    spans: Vec<SpanConflict>,
    conflicts: &mut Vec<AxintHash>,
) -> Result<Option<TreeEntry>, CoreError> {
    let entry = store_conflict(store, path, kind, ancestor, (Some(left), Some(right)), spans)?;
    conflicts.push(entry.hash()?);
    Ok(Some(entry))
}

fn store_conflict<S: ObjectStore>(
    store: &mut S,
    path: &str,
    kind: ConflictKind,
    ancestor: Option<&TreeEntry>,
    sides: (Option<&TreeEntry>, Option<&TreeEntry>),
    spans: Vec<SpanConflict>,
) -> Result<TreeEntry, CoreError> {
    let mut side_hashes = Vec::new();
    if let Some(left) = sides.0 {
        side_hashes.push(left.hash()?.as_bytes().to_vec());
    }
    if let Some(right) = sides.1 {
        side_hashes.push(right.hash()?.as_bytes().to_vec());
    }
    let manifest = ConflictManifest {
        schema_version: SCHEMA_VERSION,
        ancestor: ancestor.map(|entry| entry.hash()).transpose()?.map(|hash| hash.as_bytes().to_vec()),
        sides: side_hashes,
        kind: kind as i32,
        path: path.to_owned(),
        spans,
        nested: Vec::new(),
        left_mode: sides.0.map(|entry| entry.permissions).unwrap_or(0),
        right_mode: sides.1.map(|entry| entry.permissions).unwrap_or(0),
        ancestor_mode: ancestor.map(|entry| entry.permissions).unwrap_or(0),
    };
    let hash = store.put_conflict(&manifest)?;
    let name = name_of(path);
    TreeEntry::new(name, EntryType::Conflict, hash, 0, 0)
}

fn empty_tree_hash<S: ObjectStore>(store: &mut S) -> Result<AxintHash, CoreError> {
    store.put_tree(&TreeManifest {
        schema_version: SCHEMA_VERSION,
        entries: Vec::new(),
    })
}

fn union_names(ancestor: &TreeManifest, left: &TreeManifest, right: &TreeManifest) -> Vec<String> {
    let mut names = Vec::new();
    for tree in [ancestor, left, right] {
        for entry in &tree.entries {
            if !names.iter().any(|name| name == &entry.name) {
                names.push(entry.name.clone());
            }
        }
    }
    names.sort();
    names
}

fn find_entry<'a>(tree: &'a TreeManifest, name: &str) -> Option<&'a TreeEntry> {
    tree.entries.iter().find(|entry| entry.name == name)
}

fn child_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

fn name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{EntryType, MODE_FILE};
    use crate::store::MemoryStore;

    fn file_entry(store: &mut MemoryStore, name: &str, bytes: &[u8], mode: u32) -> TreeEntry {
        let record = store.put_file_bytes(bytes).expect("store bytes");
        TreeEntry::new(name, entry_type_for_mode(mode), record.manifest_hash, record.size, mode).expect("entry")
    }

    fn tree_of(store: &mut MemoryStore, entries: Vec<TreeEntry>) -> AxintHash {
        store
            .put_tree(&TreeManifest {
                schema_version: SCHEMA_VERSION,
                entries,
            })
            .expect("tree")
    }

    #[test]
    fn disjoint_edits_merge_cleanly() -> Result<(), CoreError> {
        let mut store = MemoryStore::new();
        let ancestor_a = file_entry(&mut store, "a.txt", b"alpha\n", MODE_FILE);
        let ancestor_b = file_entry(&mut store, "b.txt", b"beta\n", MODE_FILE);
        let ancestor = tree_of(&mut store, vec![ancestor_a, ancestor_b]);
        let left_a = file_entry(&mut store, "a.txt", b"ALPHA\n", MODE_FILE);
        let left_b = file_entry(&mut store, "b.txt", b"beta\n", MODE_FILE);
        let left = tree_of(&mut store, vec![left_a, left_b]);
        let right_a = file_entry(&mut store, "a.txt", b"alpha\n", MODE_FILE);
        let right_b = file_entry(&mut store, "b.txt", b"BETA\n", MODE_FILE);
        let right = tree_of(&mut store, vec![right_a, right_b]);
        let merged = merge_trees(&mut store, &ancestor, &left, &right)?;
        assert!(merged.conflicts.is_empty());
        assert!(merged.conflict_root.is_none());
        let tree = store.get_tree(&merged.tree_hash)?;
        let a = store.read_file_bytes(&tree.entries[0].hash()?)?;
        let b = store.read_file_bytes(&tree.entries[1].hash()?)?;
        assert_eq!(a, b"ALPHA\n");
        assert_eq!(b, b"BETA\n");
        Ok(())
    }

    #[test]
    fn non_overlapping_lines_auto_merge() -> Result<(), CoreError> {
        let mut store = MemoryStore::new();
        let base = b"one\ntwo\nthree\n";
        let ancestor_file = file_entry(&mut store, "note.txt", base, MODE_FILE);
        let ancestor = tree_of(&mut store, vec![ancestor_file]);
        let left_file = file_entry(&mut store, "note.txt", b"ONE\ntwo\nthree\n", MODE_FILE);
        let left = tree_of(&mut store, vec![left_file]);
        let right_file = file_entry(&mut store, "note.txt", b"one\ntwo\nTHREE\n", MODE_FILE);
        let right = tree_of(&mut store, vec![right_file]);
        let merged = merge_trees(&mut store, &ancestor, &left, &right)?;
        assert!(merged.conflicts.is_empty(), "conflicts: {}", merged.conflicts.len());
        let tree = store.get_tree(&merged.tree_hash)?;
        let bytes = store.read_file_bytes(&tree.entries[0].hash()?)?;
        assert_eq!(bytes, b"ONE\ntwo\nTHREE\n");
        Ok(())
    }

    #[test]
    fn overlapping_lines_become_a_conflict_object() -> Result<(), CoreError> {
        let mut store = MemoryStore::new();
        let ancestor_file = file_entry(&mut store, "note.txt", b"same\n", MODE_FILE);
        let ancestor = tree_of(&mut store, vec![ancestor_file]);
        let left_file = file_entry(&mut store, "note.txt", b"left\n", MODE_FILE);
        let left = tree_of(&mut store, vec![left_file]);
        let right_file = file_entry(&mut store, "note.txt", b"right\n", MODE_FILE);
        let right = tree_of(&mut store, vec![right_file]);
        let merged = merge_trees(&mut store, &ancestor, &left, &right)?;
        assert_eq!(merged.conflicts.len(), 1);
        assert!(merged.conflict_root.is_some());
        let tree = store.get_tree(&merged.tree_hash)?;
        assert_eq!(tree.entries[0].kind()?, EntryType::Conflict);
        let conflict = store.get_conflict(&tree.entries[0].hash()?)?;
        assert_eq!(conflict.conflict_kind()?, ConflictKind::Content);
        assert_eq!(conflict.path, "note.txt");
        Ok(())
    }

    #[test]
    fn delete_modify_is_a_conflict() -> Result<(), CoreError> {
        let mut store = MemoryStore::new();
        let ancestor_file = file_entry(&mut store, "note.txt", b"same\n", MODE_FILE);
        let ancestor = tree_of(&mut store, vec![ancestor_file]);
        let left = tree_of(&mut store, Vec::new());
        let right_file = file_entry(&mut store, "note.txt", b"changed\n", MODE_FILE);
        let right = tree_of(&mut store, vec![right_file]);
        let merged = merge_trees(&mut store, &ancestor, &left, &right)?;
        assert_eq!(merged.conflicts.len(), 1);
        let conflict = store.get_conflict(&merged.conflicts[0])?;
        assert_eq!(conflict.conflict_kind()?, ConflictKind::DeleteModify);
        Ok(())
    }

    #[test]
    fn file_versus_directory_conflicts() -> Result<(), CoreError> {
        let mut store = MemoryStore::new();
        let ancestor_file = file_entry(&mut store, "item", b"file\n", MODE_FILE);
        let ancestor = tree_of(&mut store, vec![ancestor_file]);
        let left_file = file_entry(&mut store, "item", b"file\n", MODE_FILE);
        let left = tree_of(&mut store, vec![left_file]);
        let nested_file = file_entry(&mut store, "child.txt", b"x\n", MODE_FILE);
        let nested = tree_of(&mut store, vec![nested_file]);
        let directory = TreeEntry::new("item", EntryType::Directory, nested, 0, 0o040_000)?;
        let right = tree_of(&mut store, vec![directory]);
        let merged = merge_trees(&mut store, &ancestor, &left, &right)?;
        // Left matches the ancestor, so the directory addition on the right wins.
        let tree = store.get_tree(&merged.tree_hash)?;
        assert_eq!(tree.entries[0].kind()?, EntryType::Directory);

        let edited = file_entry(&mut store, "item", b"edited\n", MODE_FILE);
        let left_changed = tree_of(&mut store, vec![edited]);
        let merged = merge_trees(&mut store, &ancestor, &left_changed, &right)?;
        assert_eq!(merged.conflicts.len(), 1);
        assert_eq!(
            store.get_conflict(&merged.conflicts[0])?.conflict_kind()?,
            ConflictKind::FileDirectory
        );
        Ok(())
    }
}
