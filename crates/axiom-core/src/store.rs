//! Object store interface shared by merge and the on-disk CAS.
//!
//! [`MemoryStore`] keeps manifests and chunk payloads in process memory so
//! unit tests can exercise commits and merges without touching disk.

use std::collections::HashMap;

use prost::Message;

use crate::error::CoreError;
use crate::hash::AxiomHash;
use crate::manifest::{ConflictManifest, CommitManifest, FileManifest, TreeManifest};

/// Result of storing a file's bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileRecord {
    /// Digest of the file manifest.
    pub manifest_hash: AxiomHash,
    /// Digest of the raw bytes.
    pub aggregate_hash: AxiomHash,
    /// Uncompressed length.
    pub size: u64,
}

/// Read and write path for Merkle objects.
pub trait ObjectStore {
    /// Loads a tree manifest.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::MissingObject`] or [`CoreError::ObjectKind`] when
    /// the digest is absent or is not a tree.
    fn get_tree(&self, hash: &AxiomHash) -> Result<TreeManifest, CoreError>;

    /// Loads a file manifest.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::MissingObject`] or [`CoreError::ObjectKind`].
    fn get_file(&self, hash: &AxiomHash) -> Result<FileManifest, CoreError>;

    /// Loads a conflict manifest.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::MissingObject`] or [`CoreError::ObjectKind`].
    fn get_conflict(&self, hash: &AxiomHash) -> Result<ConflictManifest, CoreError>;

    /// Loads a verified commit.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::MissingObject`], [`CoreError::ObjectKind`], or a
    /// verification error.
    fn get_commit(&self, hash: &AxiomHash) -> Result<CommitManifest, CoreError>;

    /// Reassembles the raw bytes of a file manifest.
    ///
    /// # Errors
    ///
    /// Returns an error when a chunk is missing or the aggregate digest does
    /// not match the payload.
    fn read_file_bytes(&self, manifest_hash: &AxiomHash) -> Result<Vec<u8>, CoreError>;

    /// Stores a canonical tree and returns its digest.
    ///
    /// # Errors
    ///
    /// Returns an error when the tree is not valid.
    fn put_tree(&mut self, tree: &TreeManifest) -> Result<AxiomHash, CoreError>;

    /// Stores a file manifest and returns its digest.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest is not valid.
    fn put_file(&mut self, manifest: &FileManifest) -> Result<AxiomHash, CoreError>;

    /// Stores a conflict manifest and returns its digest.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest is not valid.
    fn put_conflict(&mut self, conflict: &ConflictManifest) -> Result<AxiomHash, CoreError>;

    /// Stores a verified commit under its commit id.
    ///
    /// # Errors
    ///
    /// Returns an error when verification fails.
    fn put_commit(&mut self, commit: &CommitManifest) -> Result<AxiomHash, CoreError>;

    /// Stores raw file bytes and the manifest that points at them.
    ///
    /// The memory store keeps a single chunk. The disk store chunks with FastCDC.
    ///
    /// # Errors
    ///
    /// Returns an error when the bytes cannot be recorded.
    fn put_file_bytes(&mut self, bytes: &[u8]) -> Result<FileRecord, CoreError>;
}

#[derive(Clone, Debug)]
enum Object {
    Tree(TreeManifest),
    File(FileManifest),
    Conflict(ConflictManifest),
    Commit(CommitManifest),
}

impl Object {
    fn kind_name(&self) -> &'static str {
        match self {
            Self::Tree(_) => "tree",
            Self::File(_) => "file",
            Self::Conflict(_) => "conflict",
            Self::Commit(_) => "commit",
        }
    }
}

/// In-memory object store.
#[derive(Clone, Debug, Default)]
pub struct MemoryStore {
    objects: HashMap<AxiomHash, Object>,
    blobs: HashMap<AxiomHash, Vec<u8>>,
}

impl MemoryStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn insert(&mut self, hash: AxiomHash, object: Object) -> AxiomHash {
        self.objects.insert(hash, object);
        hash
    }

    fn lookup<'a>(&'a self, hash: &AxiomHash, expected: &'static str) -> Result<&'a Object, CoreError> {
        let object = self.objects.get(hash).ok_or_else(|| CoreError::MissingObject {
            hash: hash.to_string(),
        })?;
        if object.kind_name() == expected {
            Ok(object)
        } else {
            Err(CoreError::ObjectKind {
                hash: hash.to_string(),
                expected,
                found: object.kind_name(),
            })
        }
    }
}

impl ObjectStore for MemoryStore {
    fn get_tree(&self, hash: &AxiomHash) -> Result<TreeManifest, CoreError> {
        match self.lookup(hash, "tree")? {
            Object::Tree(tree) => Ok(tree.clone()),
            _ => Err(CoreError::ObjectKind {
                hash: hash.to_string(),
                expected: "tree",
                found: "other",
            }),
        }
    }

    fn get_file(&self, hash: &AxiomHash) -> Result<FileManifest, CoreError> {
        match self.lookup(hash, "file")? {
            Object::File(file) => Ok(file.clone()),
            _ => Err(CoreError::ObjectKind {
                hash: hash.to_string(),
                expected: "file",
                found: "other",
            }),
        }
    }

    fn get_conflict(&self, hash: &AxiomHash) -> Result<ConflictManifest, CoreError> {
        match self.lookup(hash, "conflict")? {
            Object::Conflict(conflict) => Ok(conflict.clone()),
            _ => Err(CoreError::ObjectKind {
                hash: hash.to_string(),
                expected: "conflict",
                found: "other",
            }),
        }
    }

    fn get_commit(&self, hash: &AxiomHash) -> Result<CommitManifest, CoreError> {
        match self.lookup(hash, "commit")? {
            Object::Commit(commit) => {
                commit.verify()?;
                Ok(commit.clone())
            }
            _ => Err(CoreError::ObjectKind {
                hash: hash.to_string(),
                expected: "commit",
                found: "other",
            }),
        }
    }

    fn read_file_bytes(&self, manifest_hash: &AxiomHash) -> Result<Vec<u8>, CoreError> {
        let manifest = self.get_file(manifest_hash)?;
        let mut payload = Vec::new();
        for chunk in &manifest.chunks {
            let chunk_hash = AxiomHash::try_from_slice(&chunk.chunk_blake3)?;
            let data = self.blobs.get(&chunk_hash).ok_or_else(|| CoreError::MissingObject {
                hash: chunk_hash.to_string(),
            })?;
            payload.extend_from_slice(data);
        }
        let aggregate = AxiomHash::hash_bytes(&payload);
        if aggregate != manifest.aggregate()? {
            return Err(CoreError::ManifestInvariant("aggregate hash mismatch while reading file"));
        }
        Ok(payload)
    }

    fn put_tree(&mut self, tree: &TreeManifest) -> Result<AxiomHash, CoreError> {
        let canonical = tree.clone().canonicalize()?;
        let hash = canonical.digest()?;
        Ok(self.insert(hash, Object::Tree(canonical)))
    }

    fn put_file(&mut self, manifest: &FileManifest) -> Result<AxiomHash, CoreError> {
        manifest.check()?;
        let hash = manifest.digest()?;
        Ok(self.insert(hash, Object::File(manifest.clone())))
    }

    fn put_conflict(&mut self, conflict: &ConflictManifest) -> Result<AxiomHash, CoreError> {
        conflict.check()?;
        let hash = conflict.digest()?;
        Ok(self.insert(hash, Object::Conflict(conflict.clone())))
    }

    fn put_commit(&mut self, commit: &CommitManifest) -> Result<AxiomHash, CoreError> {
        commit.verify()?;
        let hash = commit.id()?;
        Ok(self.insert(hash, Object::Commit(commit.clone())))
    }

    fn put_file_bytes(&mut self, bytes: &[u8]) -> Result<FileRecord, CoreError> {
        let aggregate = AxiomHash::hash_bytes(bytes);
        let length = u32::try_from(bytes.len())
            .map_err(|_| CoreError::ManifestInvariant("in-memory file exceeds 4 GiB"))?;
        if length > 0 {
            self.blobs.insert(aggregate, bytes.to_vec());
        }
        let manifest = FileManifest::single_chunk(aggregate, length, aggregate)?;
        let manifest_hash = self.put_file(&manifest)?;
        Ok(FileRecord {
            manifest_hash,
            aggregate_hash: aggregate,
            size: u64::from(length),
        })
    }
}

/// Encodes any prost message. Used by callers that want the stored bytes.
#[must_use]
pub fn encode_message(message: &impl Message) -> Vec<u8> {
    message.encode_to_vec()
}
