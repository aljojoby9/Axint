//! Local repository: init, status, commit, merge, switch, undo, and checkout.
//!
//! Large files are chunked from a bounded read buffer. The index lives in
//! `redb`. Object manifests live under `.axiom/objects`. Undo state lives in
//! the core operation journal.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use axiom_core::{
    entry_type_for_mode, generate_signing_key, merge_trees, seal_commit, signing_key_from_bytes, unix_millis,
    AxiomHash, ChangeId, CommitDraft, CommitGraph, CommitManifest, CommitNode, ConflictManifest, EntryType,
    FileManifest, FileRecord, Identity, MergeOutput, ObjectStore, OpLog, OperationEntry, OperationKind, TreeEntry,
    TreeManifest, MODE_DIR, MODE_FILE, SCHEMA_VERSION,
};
use ed25519_dalek::SigningKey;
use prost::Message;
use serde::{Deserialize, Serialize};

use crate::cas::{read_chunk, write_chunk};
use crate::chunking::chunk_reader;
use crate::compression::COMMIT_LEVEL;
use crate::db::{InodeRecord, MetaDb};
use crate::error::StorageError;

/// A path that differs from the index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkChange {
    /// Repository-relative path using `/`.
    pub path: String,
    /// How it differs.
    pub kind: ChangeKind,
}

/// Kind of working-tree change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeKind {
    /// The path is not in the index.
    Added,
    /// The path is in the index and its bytes changed.
    Modified,
    /// The path is in the index and missing on disk.
    Deleted,
}

/// One line of `axiom log`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitSummary {
    /// Commit digest.
    pub id: AxiomHash,
    /// Stable change id.
    pub change_id: ChangeId,
    /// First line of the message.
    pub summary: String,
    /// Parent count.
    pub parents: usize,
}

/// User and remote configuration stored in `.axiom/config.toml`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepoConfig {
    /// Commit identity.
    pub user: UserConfig,
    /// Named Git or Axiom remotes.
    #[serde(default)]
    pub remotes: BTreeMap<String, RemoteConfig>,
}

/// Name and mailbox stored in the repository config.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserConfig {
    /// Display name.
    pub name: String,
    /// Mailbox.
    pub email: String,
}

/// A remote URL.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteConfig {
    /// Fetch and push URL.
    pub url: String,
}

#[derive(Default)]
struct DirBuilder {
    files: BTreeMap<String, LeafInput>,
    dirs: BTreeMap<String, DirBuilder>,
}

/// An open Axiom repository.
pub struct Repository {
    root: PathBuf,
    db: MetaDb,
    config: RepoConfig,
    key: SigningKey,
}

impl Repository {
    /// Creates a new repository at `path`.
    ///
    /// # Errors
    ///
    /// Returns an I/O or identity error.
    pub fn init(path: &Path, name: &str, email: &str) -> Result<Self, StorageError> {
        let axiom = path.join(".axiom");
        fs::create_dir_all(axiom.join("cas").join("blocks"))?;
        fs::create_dir_all(axiom.join("cas").join("packs"))?;
        fs::create_dir_all(axiom.join("objects").join("commits"))?;
        fs::create_dir_all(axiom.join("objects").join("trees"))?;
        fs::create_dir_all(axiom.join("objects").join("files"))?;
        fs::create_dir_all(axiom.join("objects").join("conflicts"))?;
        fs::create_dir_all(axiom.join("oplog"))?;
        fs::create_dir_all(axiom.join("refs").join("bookmarks"))?;
        fs::create_dir_all(axiom.join("refs").join("remotes"))?;
        fs::create_dir_all(axiom.join("vfs"))?;
        let config = RepoConfig {
            user: UserConfig {
                name: name.to_owned(),
                email: email.to_owned(),
            },
            remotes: BTreeMap::new(),
        };
        let config_text = toml::to_string(&config).map_err(|err| StorageError::Message(err.to_string()))?;
        fs::write(axiom.join("config.toml"), config_text)?;
        let key = generate_signing_key();
        fs::write(axiom.join("identity.key"), key.to_bytes())?;
        fs::write(axiom.join("HEAD"), "bookmark main\n")?;
        MetaDb::open(&axiom.join("index.db"))?;
        OpLog::open(&axiom.join("oplog").join("journal.log"), AxiomHash::ZERO)?;
        Self::open(path)
    }

    /// Opens a repository rooted at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::NotARepository`] when `.axiom` is missing.
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        let axiom = path.join(".axiom");
        if !axiom.is_dir() {
            return Err(StorageError::NotARepository);
        }
        let config: RepoConfig = toml::from_str(&fs::read_to_string(axiom.join("config.toml"))?)
            .map_err(|err| StorageError::Message(err.to_string()))?;
        let key_bytes = fs::read(axiom.join("identity.key"))?;
        let key_array: [u8; 32] = key_bytes
            .as_slice()
            .try_into()
            .map_err(|_| StorageError::Message("identity key must be 32 bytes".to_owned()))?;
        Ok(Self {
            root: path.to_path_buf(),
            db: MetaDb::open(&axiom.join("index.db"))?,
            config,
            key: signing_key_from_bytes(&key_array),
        })
    }

    /// Walks parents from `start` until an `.axiom` directory is found.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::NotARepository`] when none exists.
    pub fn discover(start: &Path) -> Result<Self, StorageError> {
        let mut cursor = if start.is_file() { start.parent() } else { Some(start) };
        while let Some(dir) = cursor {
            if dir.join(".axiom").is_dir() {
                return Self::open(dir);
            }
            cursor = dir.parent();
        }
        Err(StorageError::NotARepository)
    }

    /// Repository root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Borrowed configuration.
    #[must_use]
    pub fn config(&self) -> &RepoConfig {
        &self.config
    }

    /// Signing key generated by [`Self::init`].
    #[must_use]
    pub fn signing_key(&self) -> &SigningKey {
        &self.key
    }

    /// Records a remote URL in `config.toml`.
    ///
    /// # Errors
    ///
    /// Returns an I/O error.
    pub fn add_remote(&mut self, name: &str, url: &str) -> Result<(), StorageError> {
        self.config.remotes.insert(
            name.to_owned(),
            RemoteConfig { url: url.to_owned() },
        );
        self.save_config()
    }

    /// Looks up a remote URL.
    #[must_use]
    pub fn remote_url(&self, name: &str) -> Option<&str> {
        self.config.remotes.get(name).map(|remote| remote.url.as_str())
    }

    /// Current bookmark name from `HEAD`.
    ///
    /// # Errors
    ///
    /// Returns an error when `HEAD` is missing or detached.
    pub fn current_bookmark(&self) -> Result<String, StorageError> {
        let text = fs::read_to_string(self.root.join(".axiom").join("HEAD"))?;
        text.trim()
            .strip_prefix("bookmark ")
            .map(ToOwned::to_owned)
            .ok_or_else(|| StorageError::Message("HEAD does not name a bookmark".to_owned()))
    }

    /// Commit currently pointed at by `name`.
    ///
    /// # Errors
    ///
    /// Returns a database error.
    pub fn bookmark(&self, name: &str) -> Result<Option<AxiomHash>, StorageError> {
        self.db.bookmark(name)
    }

    /// Head commit, when the current bookmark has one.
    ///
    /// # Errors
    ///
    /// Returns an I/O or database error.
    pub fn head(&self) -> Result<Option<AxiomHash>, StorageError> {
        self.bookmark(&self.current_bookmark()?)
    }

    /// Creates `name` at the current head.
    ///
    /// # Errors
    ///
    /// Returns an error when there is no commit yet.
    pub fn create_bookmark(&mut self, name: &str) -> Result<(), StorageError> {
        let head = self.head()?.ok_or(StorageError::NoCommits)?;
        self.db.put_bookmark(name, &head)
    }

    /// Working-tree changes found by comparing the index to the filesystem.
    ///
    /// # Errors
    ///
    /// Returns an I/O or database error.
    pub fn status_scan(&self) -> Result<Vec<WorkChange>, StorageError> {
        let mut indexed: BTreeMap<String, InodeRecord> = self.db.inodes()?.into_iter().collect();
        let mut changes = Vec::new();
        for path in walk_files(&self.root)? {
            let relative = relative_path(&self.root, &path)?;
            match indexed.remove(&relative) {
                None => changes.push(WorkChange {
                    path: relative,
                    kind: ChangeKind::Added,
                }),
                Some(record) => {
                    if !self.file_matches(&path, &record)? {
                        changes.push(WorkChange {
                            path: relative,
                            kind: ChangeKind::Modified,
                        });
                    }
                }
            }
        }
        for (path, _) in indexed {
            changes.push(WorkChange {
                path,
                kind: ChangeKind::Deleted,
            });
        }
        Ok(changes)
    }

    /// Compares only `paths` against the index. Paths are repository-relative.
    ///
    /// # Errors
    ///
    /// Returns an I/O or database error.
    pub fn status_paths(&self, paths: &[String]) -> Result<Vec<WorkChange>, StorageError> {
        let mut changes = Vec::new();
        for path in paths {
            let absolute = self.absolute(path);
            let indexed = self.db.inode(path)?;
            match (absolute.exists(), indexed) {
                (false, None) => {}
                (false, Some(_)) => changes.push(WorkChange {
                    path: path.clone(),
                    kind: ChangeKind::Deleted,
                }),
                (true, None) => changes.push(WorkChange {
                    path: path.clone(),
                    kind: ChangeKind::Added,
                }),
                (true, Some(record)) if !self.file_matches(&absolute, &record)? => changes.push(WorkChange {
                    path: path.clone(),
                    kind: ChangeKind::Modified,
                }),
                (true, Some(_)) => {}
            }
        }
        Ok(changes)
    }

    /// Records a commit from the current working tree.
    ///
    /// # Errors
    ///
    /// Returns an error when there is nothing to commit or a write fails.
    pub fn commit(&mut self, message: &str) -> Result<AxiomHash, StorageError> {
        if message.trim().is_empty() {
            return Err(StorageError::Message("commit message is empty".to_owned()));
        }
        let changes = self.status_scan()?;
        if changes.is_empty() {
            return Err(StorageError::Message("nothing to commit".to_owned()));
        }
        let mut leaves = self.index_leaves()?;
        self.apply_work_changes(&mut leaves, &changes)?;
        let tree = self.tree_from_leaves(&leaves)?;
        let parents = self.head()?.into_iter().collect();
        let identity = self.identity_now()?;
        let draft = CommitDraft {
            root_tree_hash: tree,
            parents,
            change_id: ChangeId::generate(),
            sequence_number: 0,
            author: identity.clone(),
            committer: identity,
            message: message.to_owned(),
            conflict_records: Vec::new(),
        };
        let bookmark = self.current_bookmark()?;
        let previous = self.head()?.unwrap_or(AxiomHash::ZERO);
        let id = self.store_draft(draft)?;
        self.point_bookmark(&bookmark, &id)?;
        self.rewrite_index(&leaves)?;
        self.record_op(OperationKind::Commit, previous, id, &bookmark, message)?;
        Ok(id)
    }

    /// Merges `bookmark` into the current bookmark.
    ///
    /// Conflicts are stored in the merge commit. The command still succeeds.
    ///
    /// # Errors
    ///
    /// Returns an error when the working tree is dirty or a bookmark is missing.
    pub fn merge(&mut self, bookmark: &str, message: Option<&str>) -> Result<MergeOutput, StorageError> {
        self.require_clean("merge")?;
        let current_name = self.current_bookmark()?;
        let head = self.require_bookmark(&current_name)?;
        let other = self.require_bookmark(bookmark)?;
        if head == other {
            return Err(StorageError::Message("already up to date".to_owned()));
        }
        let ancestor_tree = self.ancestor_tree(&head, &other)?;
        let left_tree = self.load_commit(&head)?.root()?;
        let right_tree = self.load_commit(&other)?.root()?;
        let outcome = merge_trees(self, &ancestor_tree, &left_tree, &right_tree)?;
        let mut conflicts = outcome.conflicts.clone();
        if let Some(root) = outcome.conflict_root {
            conflicts.push(root);
        }
        let text = message
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Merge {bookmark} into {current_name}"));
        let identity = self.identity_now()?;
        let draft = CommitDraft {
            root_tree_hash: outcome.tree_hash,
            parents: vec![head, other],
            change_id: ChangeId::generate(),
            sequence_number: 0,
            author: identity.clone(),
            committer: identity,
            message: text.clone(),
            conflict_records: conflicts,
        };
        let id = self.store_draft(draft)?;
        self.point_bookmark(&current_name, &id)?;
        self.checkout_tree(&outcome.tree_hash)?;
        self.record_op(OperationKind::Merge, head, id, &current_name, &text)?;
        Ok(outcome)
    }

    /// Moves the current bookmark to `bookmark` when tracked files are clean.
    ///
    /// Untracked files stay in the worktree unless the target wants that path.
    ///
    /// # Errors
    ///
    /// Returns an error when tracked edits would be overwritten.
    pub fn switch(&mut self, bookmark: &str) -> Result<(), StorageError> {
        let target = self.require_bookmark(bookmark)?;
        let target_files = self.list_files(&self.load_commit(&target)?.root()?)?;
        for change in self.status_scan()? {
            let blocks = match change.kind {
                ChangeKind::Added => target_files.contains_key(&change.path),
                ChangeKind::Modified | ChangeKind::Deleted => true,
            };
            if blocks {
                return Err(StorageError::Message(format!(
                    "local change in `{}` would be overwritten by switch",
                    change.path
                )));
            }
        }
        let previous = self.head()?.unwrap_or(AxiomHash::ZERO);
        self.checkout_tree(&self.load_commit(&target)?.root()?)?;
        self.point_bookmark(bookmark, &target)?;
        self.record_op(OperationKind::Switch, previous, target, bookmark, "switch")?;
        Ok(())
    }

    /// Restores the previous journal head.
    ///
    /// # Errors
    ///
    /// Returns an error when there is nothing to undo.
    pub fn undo(&mut self) -> Result<AxiomHash, StorageError> {
        let mut log = self.journal()?;
        let entry = log.undo()?.clone();
        self.restore_head(&entry.reverse_head, &entry.bookmark)?;
        Ok(entry.reverse_head)
    }

    /// Reapplies an undone journal entry.
    ///
    /// # Errors
    ///
    /// Returns an error when there is nothing to redo.
    pub fn redo(&mut self) -> Result<AxiomHash, StorageError> {
        let mut log = self.journal()?;
        let entry = log.redo()?.clone();
        self.restore_head(&entry.target_head, &entry.bookmark)?;
        Ok(entry.target_head)
    }

    /// First-parent history, newest first.
    ///
    /// # Errors
    ///
    /// Returns an error when a commit object is missing.
    pub fn log(&self) -> Result<Vec<CommitSummary>, StorageError> {
        let mut history = Vec::new();
        let mut cursor = self.head()?;
        let mut seen = HashSet::new();
        while let Some(id) = cursor {
            if !seen.insert(id) {
                break;
            }
            let commit = self.load_commit(&id)?;
            let summary = commit.message.lines().next().unwrap_or("").to_owned();
            let parents = commit.parents()?;
            history.push(CommitSummary {
                id,
                change_id: commit.change()?,
                summary,
                parents: parents.len(),
            });
            cursor = parents.into_iter().next();
        }
        Ok(history)
    }

    /// Stores raw bytes as a chunked file manifest.
    ///
    /// # Errors
    ///
    /// Returns an I/O or manifest error.
    pub fn ingest_bytes(&mut self, mut bytes: &[u8]) -> Result<FileRecord, StorageError> {
        self.store_reader(&mut bytes)
    }

    /// Builds a tree from repository-relative files and returns its digest.
    ///
    /// # Errors
    ///
    /// Returns an error when a path or manifest is invalid.
    pub fn tree_from_files(&mut self, files: &BTreeMap<String, LeafInput>) -> Result<AxiomHash, StorageError> {
        let leaves = files.iter().map(|(path, leaf)| (path.clone(), *leaf)).collect();
        self.tree_from_leaves(&leaves)
    }

    /// Seals `draft` with the repository key and optionally moves a bookmark.
    ///
    /// # Errors
    ///
    /// Returns a signing or I/O error.
    pub fn import_commit(&mut self, draft: CommitDraft, bookmark: Option<&str>) -> Result<AxiomHash, StorageError> {
        let id = self.store_draft(draft)?;
        if let Some(name) = bookmark {
            let previous = self.bookmark(name)?.unwrap_or(AxiomHash::ZERO);
            self.point_bookmark(name, &id)?;
            self.record_op(OperationKind::Commit, previous, id, name, "import")?;
        }
        Ok(id)
    }

    /// Writes the tree of `commit` into the working tree and refreshes the index.
    ///
    /// # Errors
    ///
    /// Returns an I/O error.
    pub fn checkout_commit(&mut self, commit: &AxiomHash) -> Result<(), StorageError> {
        let tree = self.load_commit(commit)?.root()?;
        self.checkout_tree(&tree)
    }

    /// Lists files in a tree. The hash is the file or conflict object.
    ///
    /// # Errors
    ///
    /// Returns an error when the tree cannot be read.
    pub fn list_files(&self, tree: &AxiomHash) -> Result<BTreeMap<String, LeafInput>, StorageError> {
        let mut files = BTreeMap::new();
        self.collect_files(tree, "", &mut files)?;
        Ok(files)
    }

    fn save_config(&self) -> Result<(), StorageError> {
        let text = toml::to_string(&self.config).map_err(|err| StorageError::Message(err.to_string()))?;
        fs::write(self.root.join(".axiom").join("config.toml"), text)?;
        Ok(())
    }

    fn identity_now(&self) -> Result<Identity, StorageError> {
        Identity::now(&self.config.user.name, &self.config.user.email).map_err(StorageError::from)
    }

    fn require_bookmark(&self, name: &str) -> Result<AxiomHash, StorageError> {
        self.bookmark(name)?.ok_or_else(|| StorageError::MissingBookmark(name.to_owned()))
    }

    fn require_clean(&self, action: &str) -> Result<(), StorageError> {
        let dirty = self.status_scan()?;
        if dirty.is_empty() {
            Ok(())
        } else {
            Err(StorageError::Message(format!(
                "cannot {action} with local changes in {}",
                dirty[0].path
            )))
        }
    }

    fn journal(&self) -> Result<OpLog, StorageError> {
        OpLog::open(
            &self.root.join(".axiom").join("oplog").join("journal.log"),
            AxiomHash::ZERO,
        )
        .map_err(StorageError::from)
    }

    fn record_op(
        &self,
        kind: OperationKind,
        previous: AxiomHash,
        target: AxiomHash,
        bookmark: &str,
        description: &str,
    ) -> Result<(), StorageError> {
        let mut log = self.journal()?;
        let entry = OperationEntry::new(unix_millis()?, kind, previous, target, bookmark, description);
        let sequence = u64::try_from(log.len()).unwrap_or(u64::MAX);
        log.record(entry)?;
        self.db.put_oplog(sequence, description.as_bytes())?;
        Ok(())
    }

    fn restore_head(&mut self, head: &AxiomHash, bookmark: &str) -> Result<(), StorageError> {
        if *head == AxiomHash::ZERO {
            let empty = self.empty_tree()?;
            self.checkout_tree(&empty)?;
            self.db.delete_bookmark(bookmark)?;
            fs::write(self.root.join(".axiom").join("HEAD"), format!("bookmark {bookmark}\n"))?;
            return Ok(());
        }
        self.checkout_commit(head)?;
        self.point_bookmark(bookmark, head)?;
        Ok(())
    }

    fn point_bookmark(&self, name: &str, commit: &AxiomHash) -> Result<(), StorageError> {
        self.db.put_bookmark(name, commit)?;
        fs::create_dir_all(self.root.join(".axiom").join("refs").join("bookmarks"))?;
        fs::write(
            self.root.join(".axiom").join("refs").join("bookmarks").join(name),
            format!("{commit}\n"),
        )?;
        fs::write(self.root.join(".axiom").join("HEAD"), format!("bookmark {name}\n"))?;
        Ok(())
    }

    fn store_draft(&mut self, draft: CommitDraft) -> Result<AxiomHash, StorageError> {
        let commit = seal_commit(&draft, &self.key, &self.key)?;
        let id = commit.id()?;
        self.save_bytes("commits", &id, &commit.encode_to_vec())?;
        Ok(id)
    }

    fn ancestor_tree(&mut self, left: &AxiomHash, right: &AxiomHash) -> Result<AxiomHash, StorageError> {
        let graph = self.graph_from(&[*left, *right])?;
        if let Some(base) = graph.merge_base(left, right)? {
            return Ok(self.load_commit(&base)?.root()?);
        }
        self.empty_tree()
    }

    fn graph_from(&self, seeds: &[AxiomHash]) -> Result<CommitGraph, StorageError> {
        let mut graph = CommitGraph::new();
        let mut queue = VecDeque::from(seeds.to_vec());
        let mut seen = HashSet::new();
        while let Some(id) = queue.pop_front() {
            if id == AxiomHash::ZERO || !seen.insert(id) {
                continue;
            }
            let manifest = self.load_commit(&id)?;
            let node = CommitNode::from_manifest(&manifest)?;
            let parents: Vec<AxiomHash> = node.parents.iter().copied().collect();
            graph.insert(node)?;
            queue.extend(parents);
        }
        Ok(graph)
    }

    fn empty_tree(&mut self) -> Result<AxiomHash, StorageError> {
        self.put_tree(&TreeManifest {
            schema_version: SCHEMA_VERSION,
            entries: Vec::new(),
        })
        .map_err(StorageError::from)
    }

    fn index_leaves(&self) -> Result<BTreeMap<String, LeafInput>, StorageError> {
        let mut leaves = BTreeMap::new();
        for (path, record) in self.db.inodes()? {
            leaves.insert(
                path,
                LeafInput {
                    manifest: record.hash(),
                    size: record.file_size,
                    mode: record.mode,
                },
            );
        }
        Ok(leaves)
    }

    fn apply_work_changes(
        &mut self,
        leaves: &mut BTreeMap<String, LeafInput>,
        changes: &[WorkChange],
    ) -> Result<(), StorageError> {
        for change in changes {
            match change.kind {
                ChangeKind::Deleted => {
                    leaves.remove(&change.path);
                }
                ChangeKind::Added | ChangeKind::Modified => {
                    let absolute = self.absolute(&change.path);
                    let meta = fs::symlink_metadata(&absolute)?;
                    let record = if meta.file_type().is_symlink() {
                        let target = fs::read_link(&absolute)?;
                        self.ingest_bytes(target.to_string_lossy().as_bytes())?
                    } else {
                        let mut file = File::open(&absolute)?;
                        self.store_reader(&mut file)?
                    };
                    leaves.insert(
                        change.path.clone(),
                        LeafInput {
                            manifest: record.manifest_hash,
                            size: record.size,
                            mode: file_mode(&meta),
                        },
                    );
                }
            }
        }
        Ok(())
    }

    fn store_reader<R: std::io::Read>(&mut self, reader: &mut R) -> Result<FileRecord, StorageError> {
        let mut hasher = AxiomHash::stream();
        let mut chunks = Vec::new();
        let mut total = 0_u64;
        chunk_reader(reader, |chunk| {
            hasher.update(chunk);
            total = total.saturating_add(u64::try_from(chunk.len()).unwrap_or(0));
            chunks.push(write_chunk(&self.root, chunk, COMMIT_LEVEL)?);
            Ok::<(), StorageError>(())
        })?;
        let aggregate = hasher.finalize();
        let manifest = FileManifest::from_parts(total, aggregate, chunks)?;
        let manifest_hash = self.put_file(&manifest)?;
        Ok(FileRecord {
            manifest_hash,
            aggregate_hash: aggregate,
            size: total,
        })
    }

    fn tree_from_leaves(&mut self, leaves: &BTreeMap<String, LeafInput>) -> Result<AxiomHash, StorageError> {
        let mut root = DirBuilder::default();
        for (path, leaf) in leaves {
            insert_leaf(&mut root, path, leaf.clone())?;
        }
        self.write_dir(&root)
    }

    fn write_dir(&mut self, dir: &DirBuilder) -> Result<AxiomHash, StorageError> {
        let mut entries = Vec::new();
        for (name, child) in &dir.dirs {
            let hash = self.write_dir(child)?;
            entries.push(TreeEntry::new(name, EntryType::Directory, hash, 0, MODE_DIR)?);
        }
        for (name, leaf) in &dir.files {
            entries.push(TreeEntry::new(
                name,
                entry_type_for_mode(leaf.mode),
                leaf.manifest,
                leaf.size,
                leaf.mode,
            )?);
        }
        self.put_tree(&TreeManifest {
            schema_version: SCHEMA_VERSION,
            entries,
        })
        .map_err(StorageError::from)
    }

    fn checkout_tree(&mut self, tree: &AxiomHash) -> Result<(), StorageError> {
        let old = self.db.inodes()?;
        let files = self.list_files(tree)?;
        let mut records = Vec::new();
        for (path, leaf) in &files {
            let absolute = self.absolute(path);
            self.write_leaf(&absolute, leaf)?;
            if let Ok(meta) = fs::symlink_metadata(&absolute) {
                let stamp = file_stamp(&meta)?;
                records.push((
                    path.clone(),
                    InodeRecord::clean(leaf.manifest, stamp.0, stamp.1, leaf.size, leaf.mode),
                ));
            }
        }
        for (path, _) in &old {
            if !files.contains_key(path) {
                let absolute = self.absolute(path);
                if absolute.is_file() {
                    fs::remove_file(absolute)?;
                }
            }
        }
        self.db.replace_inodes(&records)?;
        Ok(())
    }

    fn write_leaf(&self, path: &Path, leaf: &LeafInput) -> Result<(), StorageError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if self.load_kind(&leaf.manifest)? == ObjectKind::Conflict {
            let conflict = self.get_conflict(&leaf.manifest)?;
            fs::write(path, conflict_marker(&conflict))?;
            return Ok(());
        }
        let manifest = self.get_file(&leaf.manifest)?;
        let mut output = File::create(path)?;
        for chunk in &manifest.chunks {
            let hash = AxiomHash::try_from_slice(&chunk.chunk_blake3)?;
            let bytes = read_chunk(&self.root, &hash)?;
            output.write_all(&bytes)?;
        }
        Ok(())
    }

    fn collect_files(
        &self,
        tree: &AxiomHash,
        prefix: &str,
        out: &mut BTreeMap<String, LeafInput>,
    ) -> Result<(), StorageError> {
        let manifest = self.get_tree(tree)?;
        for entry in manifest.entries {
            let name = entry.name.clone();
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            match entry.kind()? {
                EntryType::Directory => self.collect_files(&entry.hash()?, &path, out)?,
                EntryType::Conflict => {
                    let conflict = self.get_conflict(&entry.hash()?)?;
                    let marker_len = u64::try_from(conflict_marker(&conflict).len()).unwrap_or(0);
                    out.insert(
                        path,
                        LeafInput {
                            manifest: entry.hash()?,
                            size: marker_len,
                            mode: MODE_FILE,
                        },
                    );
                }
                EntryType::File | EntryType::Executable | EntryType::Symlink => {
                    out.insert(
                        path,
                        LeafInput {
                            manifest: entry.hash()?,
                            size: entry.size,
                            mode: entry.permissions,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    fn rewrite_index(&self, leaves: &BTreeMap<String, LeafInput>) -> Result<(), StorageError> {
        let mut records = Vec::new();
        for (path, leaf) in leaves {
            let absolute = self.absolute(path);
            let meta = fs::symlink_metadata(&absolute)?;
            let stamp = file_stamp(&meta)?;
            records.push((
                path.clone(),
                InodeRecord::clean(leaf.manifest, stamp.0, stamp.1, leaf.size, leaf.mode),
            ));
        }
        self.db.replace_inodes(&records)
    }

    fn file_matches(&self, path: &Path, record: &InodeRecord) -> Result<bool, StorageError> {
        let meta = fs::symlink_metadata(path)?;
        let stamp = file_stamp(&meta)?;
        if stamp.0 == record.mtime_sec && stamp.1 == record.mtime_nsec && meta.len() == record.file_size {
            return Ok(true);
        }
        if meta.len() != record.file_size {
            return Ok(false);
        }
        let current = if meta.file_type().is_symlink() {
            let target = fs::read_link(path)?;
            AxiomHash::hash_bytes(target.to_string_lossy().as_bytes())
        } else {
            let mut file = File::open(path)?;
            AxiomHash::hash_reader(&mut file)?
        };
        Ok(current == self.aggregate_of(&record.hash())?)
    }

    fn aggregate_of(&self, manifest_hash: &AxiomHash) -> Result<AxiomHash, StorageError> {
        if self.load_kind(manifest_hash)? == ObjectKind::Conflict {
            let conflict = self.get_conflict(manifest_hash)?;
            return Ok(AxiomHash::hash_bytes(&conflict_marker(&conflict)));
        }
        self.get_file(manifest_hash)?.aggregate().map_err(StorageError::from)
    }

    fn load_commit(&self, hash: &AxiomHash) -> Result<CommitManifest, StorageError> {
        let bytes = self.load_bytes("commits", hash)?;
        CommitManifest::decode_verified(&bytes).map_err(StorageError::from)
    }

    fn load_kind(&self, hash: &AxiomHash) -> Result<ObjectKind, StorageError> {
        for (kind, value) in [
            ("trees", ObjectKind::Tree),
            ("files", ObjectKind::File),
            ("conflicts", ObjectKind::Conflict),
            ("commits", ObjectKind::Commit),
        ] {
            if object_path(&self.root, kind, hash).is_file() {
                return Ok(value);
            }
        }
        Err(StorageError::Core(axiom_core::CoreError::MissingObject {
            hash: hash.to_string(),
        }))
    }

    fn save_bytes(&self, kind: &str, hash: &AxiomHash, bytes: &[u8]) -> Result<(), StorageError> {
        let path = object_path(&self.root, kind, hash);
        if path.exists() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, bytes)?;
        Ok(())
    }

    fn load_bytes(&self, kind: &str, hash: &AxiomHash) -> Result<Vec<u8>, StorageError> {
        let path = object_path(&self.root, kind, hash);
        fs::read(&path).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                StorageError::Core(axiom_core::CoreError::MissingObject {
                    hash: hash.to_string(),
                })
            } else {
                StorageError::Io(err)
            }
        })
    }

    fn absolute(&self, relative: &str) -> PathBuf {
        relative.split('/').filter(|part| !part.is_empty()).fold(self.root.clone(), |path, part| path.join(part))
    }
}

/// File description accepted by [`Repository::tree_from_files`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LeafInput {
    /// File-manifest digest.
    pub manifest: AxiomHash,
    /// Uncompressed size.
    pub size: u64,
    /// POSIX mode.
    pub mode: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObjectKind {
    Tree,
    File,
    Conflict,
    Commit,
}

impl ObjectStore for Repository {
    fn get_tree(&self, hash: &AxiomHash) -> Result<TreeManifest, axiom_core::CoreError> {
        let bytes = self.load_bytes("trees", hash).map_err(StorageError::into_core)?;
        TreeManifest::decode_canonical(&bytes)
    }

    fn get_file(&self, hash: &AxiomHash) -> Result<FileManifest, axiom_core::CoreError> {
        let bytes = self.load_bytes("files", hash).map_err(StorageError::into_core)?;
        FileManifest::decode_canonical(&bytes)
    }

    fn get_conflict(&self, hash: &AxiomHash) -> Result<ConflictManifest, axiom_core::CoreError> {
        let bytes = self.load_bytes("conflicts", hash).map_err(StorageError::into_core)?;
        ConflictManifest::decode_canonical(&bytes)
    }

    fn get_commit(&self, hash: &AxiomHash) -> Result<CommitManifest, axiom_core::CoreError> {
        self.load_commit(hash).map_err(StorageError::into_core)
    }

    fn read_file_bytes(&self, manifest_hash: &AxiomHash) -> Result<Vec<u8>, axiom_core::CoreError> {
        let manifest = self.get_file(manifest_hash)?;
        let mut payload = Vec::new();
        for chunk in &manifest.chunks {
            let hash = AxiomHash::try_from_slice(&chunk.chunk_blake3)?;
            let bytes = read_chunk(&self.root, &hash).map_err(StorageError::into_core)?;
            payload.extend_from_slice(&bytes);
        }
        let aggregate = AxiomHash::hash_bytes(&payload);
        if aggregate != manifest.aggregate()? {
            return Err(axiom_core::CoreError::ManifestInvariant("aggregate hash mismatch"));
        }
        Ok(payload)
    }

    fn put_tree(&mut self, tree: &TreeManifest) -> Result<AxiomHash, axiom_core::CoreError> {
        let canonical = tree.clone().canonicalize()?;
        let hash = canonical.digest()?;
        self.save_bytes("trees", &hash, &canonical.encode_to_vec())
            .map_err(StorageError::into_core)?;
        Ok(hash)
    }

    fn put_file(&mut self, manifest: &FileManifest) -> Result<AxiomHash, axiom_core::CoreError> {
        let hash = manifest.digest()?;
        self.save_bytes("files", &hash, &manifest.encode_to_vec())
            .map_err(StorageError::into_core)?;
        Ok(hash)
    }

    fn put_conflict(&mut self, conflict: &ConflictManifest) -> Result<AxiomHash, axiom_core::CoreError> {
        let hash = conflict.digest()?;
        self.save_bytes("conflicts", &hash, &conflict.encode_to_vec())
            .map_err(StorageError::into_core)?;
        Ok(hash)
    }

    fn put_commit(&mut self, commit: &CommitManifest) -> Result<AxiomHash, axiom_core::CoreError> {
        commit.verify()?;
        let hash = commit.id()?;
        self.save_bytes("commits", &hash, &commit.encode_to_vec())
            .map_err(StorageError::into_core)?;
        Ok(hash)
    }

    fn put_file_bytes(&mut self, bytes: &[u8]) -> Result<FileRecord, axiom_core::CoreError> {
        self.ingest_bytes(bytes).map_err(StorageError::into_core)
    }
}

fn insert_leaf(dir: &mut DirBuilder, path: &str, leaf: LeafInput) -> Result<(), StorageError> {
    let (name, rest) = split_once_path(path)?;
    if rest.is_empty() {
        dir.files.insert(name.to_owned(), leaf);
        return Ok(());
    }
    let child = dir.dirs.entry(name.to_owned()).or_default();
    insert_leaf(child, rest, leaf)
}

fn split_once_path(path: &str) -> Result<(&str, &str), StorageError> {
    if path.is_empty() || path.starts_with('/') || path.contains("//") {
        return Err(StorageError::Message(format!("invalid repository path `{path}`")));
    }
    match path.split_once('/') {
        Some((name, rest)) => Ok((name, rest)),
        None => Ok((path, "")),
    }
}

fn object_path(root: &Path, kind: &str, hash: &AxiomHash) -> PathBuf {
    let hex = hash.to_hex();
    root.join(".axiom")
        .join("objects")
        .join(kind)
        .join(&hex[..2])
        .join(&hex[2..])
}

fn conflict_marker(conflict: &ConflictManifest) -> Vec<u8> {
    format!(
        "Axiom-Conflict\npath: {}\nkind: {}\n",
        conflict.path,
        conflict.kind
    )
    .into_bytes()
}

fn file_mode(meta: &std::fs::Metadata) -> u32 {
    if meta.file_type().is_symlink() {
        return axiom_core::MODE_SYMLINK;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            return axiom_core::MODE_EXEC;
        }
    }
    MODE_FILE
}

fn file_stamp(meta: &std::fs::Metadata) -> Result<(u64, u32), StorageError> {
    let modified = meta.modified()?;
    let Ok(elapsed) = modified.duration_since(UNIX_EPOCH) else {
        return Ok((0, 0));
    };
    Ok((elapsed.as_secs(), elapsed.subsec_nanos()))
}

fn relative_path(root: &Path, path: &Path) -> Result<String, StorageError> {
    let stripped = path
        .strip_prefix(root)
        .map_err(|_| StorageError::Message("path escapes the repository".to_owned()))?;
    let mut parts = Vec::new();
    for component in stripped.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            _ => return Err(StorageError::Message("unsupported path component".to_owned())),
        }
    }
    Ok(parts.join("/"))
}

fn walk_files(root: &Path) -> Result<Vec<PathBuf>, StorageError> {
    let mut files = Vec::new();
    walk_into(root, root, &mut files)?;
    Ok(files)
}

fn walk_into(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), StorageError> {
    let _ = root;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".axiom" || name == ".git" || name == "target" {
            continue;
        }
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            walk_into(root, &path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_status_and_undo() -> Result<(), StorageError> {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut repo = Repository::init(dir.path(), "Ada Lovelace", "ada@axiom.dev")?;
        fs::write(dir.path().join("hello.txt"), b"hello\n")?;
        fs::create_dir(dir.path().join("src"))?;
        fs::write(dir.path().join("src").join("lib.rs"), b"fn main() {}\n")?;
        let first = repo.commit("first")?;
        assert!(repo.status_scan()?.is_empty());
        fs::write(dir.path().join("hello.txt"), b"hello!\n")?;
        assert_eq!(repo.status_scan()?.len(), 1);
        repo.commit("second")?;
        assert_eq!(repo.log()?.len(), 2);
        let restored = repo.undo()?;
        assert_eq!(restored, first);
        assert_eq!(fs::read(dir.path().join("hello.txt"))?, b"hello\n");
        repo.redo()?;
        assert_eq!(fs::read(dir.path().join("hello.txt"))?, b"hello!\n");
        Ok(())
    }

    #[test]
    fn disjoint_bookmark_merge() -> Result<(), StorageError> {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut repo = Repository::init(dir.path(), "Ada Lovelace", "ada@axiom.dev")?;
        fs::write(dir.path().join("a.txt"), b"a\n")?;
        fs::write(dir.path().join("b.txt"), b"b\n")?;
        repo.commit("base")?;
        repo.create_bookmark("side")?;
        fs::write(dir.path().join("a.txt"), b"A\n")?;
        repo.commit("edit a")?;
        repo.switch("side")?;
        fs::write(dir.path().join("b.txt"), b"B\n")?;
        repo.commit("edit b")?;
        let merged = repo.merge("main", None)?;
        assert!(merged.conflicts.is_empty());
        assert_eq!(fs::read(dir.path().join("a.txt"))?, b"A\n");
        assert_eq!(fs::read(dir.path().join("b.txt"))?, b"B\n");
        Ok(())
    }
}
