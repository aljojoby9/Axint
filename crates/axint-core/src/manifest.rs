//! Merkle manifests.
//!
//! Directory, file, commit, and conflict objects use the protobuf schema in
//! `proto/axint.proto`. Commit identity is the BLAKE3 digest of a preimage:
//! the manifest encoded with an empty commit id and empty signature bytes.
//! Ed25519 signatures then sign that digest. A [`ChangeId`] is a separate
//! UUIDv7 that stays constant when a change is rebased.

use std::fmt;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use prost::Message;
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use crate::error::CoreError;
use crate::hash::AxintHash;

/// Protobuf schema version stored in tree and conflict manifests.
pub const SCHEMA_VERSION: u32 = 1;

/// POSIX mode for a regular file.
pub const MODE_FILE: u32 = 0o100_644;
/// POSIX mode for an executable file.
pub const MODE_EXEC: u32 = 0o100_755;
/// POSIX mode for a symbolic link.
pub const MODE_SYMLINK: u32 = 0o120_000;
/// POSIX mode for a directory.
pub const MODE_DIR: u32 = 0o040_000;

const FILE_TYPE_MASK: u32 = 0o170_000;
const EXEC_BITS: u32 = 0o111;

/// Permanent identity of a logical change.
///
/// New changes use UUIDv7 so identifiers sort by creation time. Imports from
/// Git may carry a UUIDv5 derived from the Git object id so a second import
/// of the same commit keeps the same identity.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChangeId(Uuid);

impl ChangeId {
    /// Creates a new time-ordered change id.
    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::now_v7())
    }

    /// Wraps an existing UUID.
    #[must_use]
    pub const fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    /// Borrows the inner UUID.
    #[must_use]
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// Parses a hyphenated UUID.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidChangeId`] when `text` is not a UUID.
    pub fn parse(text: &str) -> Result<Self, CoreError> {
        Uuid::parse_str(text)
            .map(Self)
            .map_err(|_| CoreError::InvalidChangeId {
                value: text.to_owned(),
            })
    }
}

impl fmt::Display for ChangeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}

impl fmt::Debug for ChangeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ChangeId({self})")
    }
}

impl FromStr for ChangeId {
    type Err = CoreError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

/// Person and time recorded on a commit, before a signature is attached.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Identity {
    /// Display name.
    pub name: String,
    /// Mailbox. Used as the stable author key alongside the signing key.
    pub email: String,
    /// Unix seconds.
    pub timestamp_seconds: i64,
    /// Minutes east of UTC.
    pub timezone_offset_minutes: i32,
}

impl Identity {
    /// Builds an identity after checking the name, mailbox, and timezone.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ManifestInvariant`] when the name, email, or
    /// timezone is not usable.
    pub fn new(
        name: impl Into<String>,
        email: impl Into<String>,
        timestamp_seconds: i64,
        timezone_offset_minutes: i32,
    ) -> Result<Self, CoreError> {
        let identity = Self {
            name: name.into(),
            email: email.into(),
            timestamp_seconds,
            timezone_offset_minutes,
        };
        identity.check()?;
        Ok(identity)
    }

    /// Current time at offset zero.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ManifestInvariant`] when the clock is before the
    /// Unix epoch or the timestamp does not fit in `i64`.
    pub fn now(name: impl Into<String>, email: impl Into<String>) -> Result<Self, CoreError> {
        Self::new(name, email, unix_timestamp()?, 0)
    }

    fn check(&self) -> Result<(), CoreError> {
        if self.name.is_empty() || self.name.contains(['\0', '\n']) {
            return Err(CoreError::ManifestInvariant("identity name is empty or unsafe"));
        }
        if self.email.is_empty() || !self.email.contains('@') || self.email.contains(['\0', '\n', ' ']) {
            return Err(CoreError::ManifestInvariant("identity email is empty or unsafe"));
        }
        if !(-12 * 60..=14 * 60).contains(&self.timezone_offset_minutes) {
            return Err(CoreError::ManifestInvariant("timezone offset is out of range"));
        }
        Ok(())
    }
}

/// Fields required to seal a commit. The commit id and signatures are filled
/// by [`seal_commit`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitDraft {
    /// Digest of the root [`TreeManifest`].
    pub root_tree_hash: AxintHash,
    /// Parent commit ids. Empty for a root commit.
    pub parents: Vec<AxintHash>,
    /// Stable change identity.
    pub change_id: ChangeId,
    /// Generation of `change_id`. Rebases increment this.
    pub sequence_number: u64,
    /// Original author.
    pub author: Identity,
    /// Person who recorded this commit object.
    pub committer: Identity,
    /// Commit message.
    pub message: String,
    /// Conflict objects recorded by this commit.
    pub conflict_records: Vec<AxintHash>,
}

/// Creates an Ed25519 signing key from the operating-system generator.
#[must_use]
pub fn generate_signing_key() -> SigningKey {
    SigningKey::generate(&mut OsRng)
}

/// Loads a signing key from its 32-byte seed.
#[must_use]
pub fn signing_key_from_bytes(bytes: &[u8; 32]) -> SigningKey {
    SigningKey::from_bytes(bytes)
}

/// Unix time in seconds.
///
/// # Errors
///
/// Returns [`CoreError::ManifestInvariant`] when the clock cannot be read as
/// a positive `i64` timestamp.
pub fn unix_timestamp() -> Result<i64, CoreError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CoreError::ManifestInvariant("system clock is before the unix epoch"))?;
    i64::try_from(elapsed.as_secs()).map_err(|_| CoreError::ManifestInvariant("timestamp does not fit in i64"))
}

/// Unix time in milliseconds for journal records.
///
/// # Errors
///
/// Returns [`CoreError::ManifestInvariant`] when the clock cannot be read.
pub fn unix_millis() -> Result<u64, CoreError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CoreError::ManifestInvariant("system clock is before the unix epoch"))?;
    u64::try_from(elapsed.as_millis()).map_err(|_| CoreError::ManifestInvariant("millisecond clock overflow"))
}

/// Seals a draft into a signed commit manifest.
///
/// # Errors
///
/// Returns an error when an identity is invalid or a parent digest cannot be encoded.
pub fn seal_commit(
    draft: &CommitDraft,
    author_key: &SigningKey,
    committer_key: &SigningKey,
) -> Result<CommitManifest, CoreError> {
    draft.author.check()?;
    draft.committer.check()?;
    let mut commit = CommitManifest {
        commit_id: Vec::new(),
        root_tree_hash: draft.root_tree_hash.as_bytes().to_vec(),
        parent_ids: draft
            .parents
            .iter()
            .map(|parent| parent.as_bytes().to_vec())
            .collect(),
        change_id: draft.change_id.to_string(),
        sequence_number: draft.sequence_number,
        author: Some(signature_shell(&draft.author, author_key)),
        committer: Some(signature_shell(&draft.committer, committer_key)),
        message: draft.message.clone(),
        conflict_records: draft
            .conflict_records
            .iter()
            .map(|hash| hash.as_bytes().to_vec())
            .collect(),
    };
    let commit_id = AxintHash::hash_bytes(&commit.preimage_bytes());
    sign_shell(commit.author.as_mut(), author_key, &commit_id)?;
    sign_shell(commit.committer.as_mut(), committer_key, &commit_id)?;
    commit.commit_id = commit_id.as_bytes().to_vec();
    commit.verify()?;
    Ok(commit)
}

fn signature_shell(identity: &Identity, key: &SigningKey) -> Signature {
    Signature {
        name: identity.name.clone(),
        email: identity.email.clone(),
        timestamp_seconds: identity.timestamp_seconds,
        timezone_offset_minutes: identity.timezone_offset_minutes,
        ed25519_signature: Vec::new(),
        public_key: key.verifying_key().to_bytes().to_vec(),
    }
}

fn sign_shell(slot: Option<&mut Signature>, key: &SigningKey, commit_id: &AxintHash) -> Result<(), CoreError> {
    let Some(signature) = slot else {
        return Err(CoreError::ManifestInvariant("commit is missing a signature slot"));
    };
    let signed = key.sign(commit_id.as_bytes());
    signature.ed25519_signature = signed.to_bytes().to_vec();
    Ok(())
}

/// Maps a POSIX mode to an entry kind.
#[must_use]
pub fn entry_type_for_mode(mode: u32) -> EntryType {
    match mode & FILE_TYPE_MASK {
        MODE_DIR => EntryType::Directory,
        MODE_SYMLINK => EntryType::Symlink,
        _ if mode & EXEC_BITS != 0 => EntryType::Executable,
        _ => EntryType::File,
    }
}

/// Normalizes one path component to NFC and rejects separators.
///
/// # Errors
///
/// Returns [`CoreError::InvalidPathComponent`] for empty names, `.`, `..`,
/// or names that contain a separator or NUL.
pub fn normalize_component(name: &str) -> Result<String, CoreError> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        return Err(CoreError::InvalidPathComponent {
            name: name.to_owned(),
        });
    }
    let normalized: String = name.nfc().collect();
    if normalized.is_empty() {
        return Err(CoreError::InvalidPathComponent {
            name: name.to_owned(),
        });
    }
    Ok(normalized)
}

/// Kind of node stored in a tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum EntryType {
    /// Regular file.
    File = 0,
    /// Directory tree.
    Directory = 1,
    /// Symbolic link. The file bytes are the link target.
    Symlink = 2,
    /// Executable file.
    Executable = 3,
    /// First-class conflict object.
    Conflict = 4,
}

/// One sorted directory entry.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct TreeEntry {
    /// NFC file or directory name. No separators.
    #[prost(string, tag = "1")]
    pub name: String,
    /// [`EntryType`] stored as its protobuf ordinal.
    #[prost(enumeration = "EntryType", tag = "2")]
    pub entry_type: i32,
    /// BLAKE3 digest of the target object.
    #[prost(bytes = "vec", tag = "3")]
    pub blake3_hash: Vec<u8>,
    /// Logical size in bytes. Directories use `0`.
    #[prost(uint64, tag = "4")]
    pub size: u64,
    /// POSIX permission bits.
    #[prost(uint32, tag = "5")]
    pub permissions: u32,
}

impl TreeEntry {
    /// Builds an entry from a checked component name.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidPathComponent`] when `name` is not a single
    /// NFC-safe path component.
    pub fn new(
        name: &str,
        kind: EntryType,
        hash: AxintHash,
        size: u64,
        permissions: u32,
    ) -> Result<Self, CoreError> {
        Ok(Self {
            name: normalize_component(name)?,
            entry_type: kind as i32,
            blake3_hash: hash.as_bytes().to_vec(),
            size,
            permissions,
        })
    }

    /// Parsed entry kind.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ManifestInvariant`] when the ordinal is unknown.
    pub fn kind(&self) -> Result<EntryType, CoreError> {
        EntryType::try_from(self.entry_type).map_err(|_| CoreError::ManifestInvariant("unknown entry type"))
    }

    /// Parsed target digest.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidHashLength`] when the field is not 32 bytes.
    pub fn hash(&self) -> Result<AxintHash, CoreError> {
        AxintHash::try_from_slice(&self.blake3_hash)
    }
}

/// Deterministic directory listing.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct TreeManifest {
    /// Schema version. Currently [`SCHEMA_VERSION`].
    #[prost(uint32, tag = "1")]
    pub schema_version: u32,
    /// Children sorted by NFC name.
    #[prost(message, repeated, tag = "2")]
    pub entries: Vec<TreeEntry>,
}

impl TreeManifest {
    /// Sorts and checks entries, then returns the canonical manifest.
    ///
    /// # Errors
    ///
    /// Returns an error when the schema is unsupported, a name is illegal, or
    /// two entries share a name.
    pub fn canonicalize(mut self) -> Result<Self, CoreError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(CoreError::ManifestInvariant("unsupported tree schema"));
        }
        for entry in &mut self.entries {
            entry.name = normalize_component(&entry.name)?;
            let _ = entry.hash()?;
            let _ = entry.kind()?;
        }
        self.entries.sort_by(|left, right| left.name.cmp(&right.name));
        for window in self.entries.windows(2) {
            if window[0].name == window[1].name {
                return Err(CoreError::ManifestInvariant("duplicate tree entry name"));
            }
        }
        Ok(self)
    }

    /// Checks that this value is already in canonical form.
    ///
    /// # Errors
    ///
    /// Returns an error when normalization or sorting would change the manifest.
    pub fn check_canonical(&self) -> Result<(), CoreError> {
        let canonical = self.clone().canonicalize()?;
        if canonical == *self {
            Ok(())
        } else {
            Err(CoreError::ManifestInvariant("tree manifest is not canonical"))
        }
    }

    /// BLAKE3 of the canonical protobuf encoding.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest is not canonical.
    pub fn digest(&self) -> Result<AxintHash, CoreError> {
        self.check_canonical()?;
        Ok(AxintHash::hash_bytes(&self.encode_to_vec()))
    }

    /// Decodes a canonical tree.
    ///
    /// # Errors
    ///
    /// Returns a decode error, or a manifest error when the bytes are not canonical.
    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, CoreError> {
        let tree = Self::decode(bytes)?;
        tree.check_canonical()?;
        Ok(tree)
    }
}

/// One FastCDC chunk inside a file manifest.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ChunkReference {
    /// Digest of the uncompressed chunk payload.
    #[prost(bytes = "vec", tag = "1")]
    pub chunk_blake3: Vec<u8>,
    /// Compressed length stored in the CAS block.
    #[prost(uint32, tag = "2")]
    pub compressed_size: u32,
    /// Uncompressed payload length.
    #[prost(uint32, tag = "3")]
    pub uncompressed_size: u32,
}

/// A file as an ordered list of content-defined chunks.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct FileManifest {
    /// Sum of uncompressed chunk sizes.
    #[prost(uint64, tag = "1")]
    pub total_size: u64,
    /// BLAKE3 of the concatenated uncompressed bytes.
    #[prost(bytes = "vec", tag = "2")]
    pub aggregate_blake3: Vec<u8>,
    /// Chunks in file order.
    #[prost(message, repeated, tag = "3")]
    pub chunks: Vec<ChunkReference>,
}

impl FileManifest {
    /// Builds a manifest and checks that chunk sizes add up.
    ///
    /// # Errors
    ///
    /// Returns an error when a chunk digest is the wrong length or the sizes disagree.
    pub fn from_parts(
        total_size: u64,
        aggregate: AxintHash,
        chunks: Vec<ChunkReference>,
    ) -> Result<Self, CoreError> {
        let manifest = Self {
            total_size,
            aggregate_blake3: aggregate.as_bytes().to_vec(),
            chunks,
        };
        manifest.check()?;
        Ok(manifest)
    }

    /// One chunk that covers the whole buffer. Empty input stores no chunks.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest fails validation.
    pub fn single_chunk(aggregate: AxintHash, uncompressed: u32, chunk_hash: AxintHash) -> Result<Self, CoreError> {
        let chunks = if uncompressed == 0 {
            Vec::new()
        } else {
            vec![ChunkReference {
                chunk_blake3: chunk_hash.as_bytes().to_vec(),
                compressed_size: uncompressed,
                uncompressed_size: uncompressed,
            }]
        };
        Self::from_parts(u64::from(uncompressed), aggregate, chunks)
    }

    /// Checks chunk digests and the size sum.
    ///
    /// # Errors
    ///
    /// Returns an error when a digest or the size sum is wrong.
    pub fn check(&self) -> Result<(), CoreError> {
        let _ = self.aggregate()?;
        let mut sum = 0_u64;
        for chunk in &self.chunks {
            let _ = AxintHash::try_from_slice(&chunk.chunk_blake3)?;
            sum = sum.saturating_add(u64::from(chunk.uncompressed_size));
        }
        if sum != self.total_size {
            return Err(CoreError::ManifestInvariant("file size does not match chunk sizes"));
        }
        Ok(())
    }

    /// Aggregate content digest.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidHashLength`] when the field is not 32 bytes.
    pub fn aggregate(&self) -> Result<AxintHash, CoreError> {
        AxintHash::try_from_slice(&self.aggregate_blake3)
    }

    /// BLAKE3 of the protobuf encoding.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest fails validation.
    pub fn digest(&self) -> Result<AxintHash, CoreError> {
        self.check()?;
        Ok(AxintHash::hash_bytes(&self.encode_to_vec()))
    }

    /// Decodes and validates a file manifest.
    ///
    /// # Errors
    ///
    /// Returns a decode or validation error.
    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, CoreError> {
        let manifest = Self::decode(bytes)?;
        manifest.check()?;
        Ok(manifest)
    }
}

/// Author or committer signature block.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct Signature {
    /// Display name.
    #[prost(string, tag = "1")]
    pub name: String,
    /// Mailbox.
    #[prost(string, tag = "2")]
    pub email: String,
    /// Unix seconds.
    #[prost(int64, tag = "3")]
    pub timestamp_seconds: i64,
    /// Minutes east of UTC.
    #[prost(int32, tag = "4")]
    pub timezone_offset_minutes: i32,
    /// Ed25519 signature over the commit id. Empty inside the preimage.
    #[prost(bytes = "vec", tag = "5")]
    pub ed25519_signature: Vec<u8>,
    /// Ed25519 public key.
    #[prost(bytes = "vec", tag = "6")]
    pub public_key: Vec<u8>,
}

/// Signed commit object.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct CommitManifest {
    /// BLAKE3 of [`Self::preimage_bytes`].
    #[prost(bytes = "vec", tag = "1")]
    pub commit_id: Vec<u8>,
    /// Root tree digest.
    #[prost(bytes = "vec", tag = "2")]
    pub root_tree_hash: Vec<u8>,
    /// Zero, one, or many parent commit ids.
    #[prost(bytes = "vec", repeated, tag = "3")]
    pub parent_ids: Vec<Vec<u8>>,
    /// UUIDv7 (or UUIDv5 for a Git import), invariant across rebases.
    #[prost(string, tag = "4")]
    pub change_id: String,
    /// Generation counter for `change_id`.
    #[prost(uint64, tag = "5")]
    pub sequence_number: u64,
    /// Author signature.
    #[prost(message, optional, tag = "6")]
    pub author: Option<Signature>,
    /// Committer signature.
    #[prost(message, optional, tag = "7")]
    pub committer: Option<Signature>,
    /// Commit message.
    #[prost(string, tag = "8")]
    pub message: String,
    /// Conflict-object digests. Empty when the commit is clean.
    #[prost(bytes = "vec", repeated, tag = "9")]
    pub conflict_records: Vec<Vec<u8>>,
}

impl CommitManifest {
    /// Protobuf bytes hashed to produce [`Self::commit_id`].
    #[must_use]
    pub fn preimage_bytes(&self) -> Vec<u8> {
        let mut copy = self.clone();
        copy.commit_id.clear();
        if let Some(author) = copy.author.as_mut() {
            author.ed25519_signature.clear();
        }
        if let Some(committer) = copy.committer.as_mut() {
            committer.ed25519_signature.clear();
        }
        copy.encode_to_vec()
    }

    /// Declared commit id.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidHashLength`] when the field is not 32 bytes.
    pub fn id(&self) -> Result<AxintHash, CoreError> {
        AxintHash::try_from_slice(&self.commit_id)
    }

    /// Root tree digest.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidHashLength`] when the field is not 32 bytes.
    pub fn root(&self) -> Result<AxintHash, CoreError> {
        AxintHash::try_from_slice(&self.root_tree_hash)
    }

    /// Parent commit ids.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidHashLength`] when a parent field is not 32 bytes.
    pub fn parents(&self) -> Result<Vec<AxintHash>, CoreError> {
        self.parent_ids.iter().map(|raw| AxintHash::try_from_slice(raw)).collect()
    }

    /// Parsed change id.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidChangeId`] when the field is not a UUID.
    pub fn change(&self) -> Result<ChangeId, CoreError> {
        ChangeId::parse(&self.change_id)
    }

    /// Conflict object ids.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidHashLength`] when a record is not 32 bytes.
    pub fn conflicts(&self) -> Result<Vec<AxintHash>, CoreError> {
        self.conflict_records
            .iter()
            .map(|raw| AxintHash::try_from_slice(raw))
            .collect()
    }

    /// Checks the commit id and both signatures.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::CommitIdMismatch`] or [`CoreError::Signature`] when
    /// the manifest was mutated or signed by a different key.
    pub fn verify(&self) -> Result<(), CoreError> {
        let declared = self.id()?;
        let actual = AxintHash::hash_bytes(&self.preimage_bytes());
        if declared != actual {
            return Err(CoreError::CommitIdMismatch);
        }
        let _ = self.root()?;
        let _ = self.parents()?;
        let _ = self.change()?;
        let _ = self.conflicts()?;
        verify_signature(self.author.as_ref(), &declared)?;
        verify_signature(self.committer.as_ref(), &declared)?;
        Ok(())
    }

    /// Decodes a commit and verifies it.
    ///
    /// # Errors
    ///
    /// Returns a decode error or a verification error.
    pub fn decode_verified(bytes: &[u8]) -> Result<Self, CoreError> {
        let commit = Self::decode(bytes)?;
        commit.verify()?;
        Ok(commit)
    }
}

fn verify_signature(slot: Option<&Signature>, commit_id: &AxintHash) -> Result<(), CoreError> {
    let Some(signature) = slot else {
        return Err(CoreError::ManifestInvariant("commit is missing a signature"));
    };
    let key_bytes: [u8; 32] = signature
        .public_key
        .as_slice()
        .try_into()
        .map_err(|_| CoreError::signature("public key must be 32 bytes"))?;
    let verifying = VerifyingKey::from_bytes(&key_bytes).map_err(CoreError::signature)?;
    let dalek = ed25519_dalek::Signature::from_slice(&signature.ed25519_signature).map_err(CoreError::signature)?;
    verifying
        .verify(commit_id.as_bytes(), &dalek)
        .map_err(CoreError::signature)?;
    Ok(())
}

/// Why a merge node could not be resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum ConflictKind {
    /// Both sides changed the same content.
    Content = 0,
    /// A file replaced a directory, or the reverse.
    FileDirectory = 1,
    /// One side deleted a path the other side edited.
    DeleteModify = 2,
    /// Both sides changed permissions differently.
    ModePermission = 3,
    /// A commit-level record that lists nested conflict objects.
    MergeSet = 4,
}

/// Byte ranges that overlapped during a content merge.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct SpanConflict {
    /// Inclusive-exclusive start in the ancestor.
    #[prost(uint64, tag = "1")]
    pub ancestor_start: u64,
    /// Exclusive end in the ancestor.
    #[prost(uint64, tag = "2")]
    pub ancestor_end: u64,
    /// Inclusive-exclusive start on the left.
    #[prost(uint64, tag = "3")]
    pub left_start: u64,
    /// Exclusive end on the left.
    #[prost(uint64, tag = "4")]
    pub left_end: u64,
    /// Inclusive-exclusive start on the right.
    #[prost(uint64, tag = "5")]
    pub right_start: u64,
    /// Exclusive end on the right.
    #[prost(uint64, tag = "6")]
    pub right_end: u64,
}

/// First-class conflict object.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ConflictManifest {
    /// Schema version. Currently [`SCHEMA_VERSION`].
    #[prost(uint32, tag = "1")]
    pub schema_version: u32,
    /// Ancestor object digest, when an ancestor existed.
    #[prost(bytes = "vec", optional, tag = "2")]
    pub ancestor: Option<Vec<u8>>,
    /// Digests of the conflicting sides, in left-then-right order.
    #[prost(bytes = "vec", repeated, tag = "3")]
    pub sides: Vec<Vec<u8>>,
    /// [`ConflictKind`] ordinal.
    #[prost(enumeration = "ConflictKind", tag = "4")]
    pub kind: i32,
    /// Repository-relative path using `/`.
    #[prost(string, tag = "5")]
    pub path: String,
    /// Overlapping spans for a content conflict.
    #[prost(message, repeated, tag = "6")]
    pub spans: Vec<SpanConflict>,
    /// Nested conflict digests for a merge-set record.
    #[prost(bytes = "vec", repeated, tag = "7")]
    pub nested: Vec<Vec<u8>>,
    /// Left POSIX mode.
    #[prost(uint32, tag = "8")]
    pub left_mode: u32,
    /// Right POSIX mode.
    #[prost(uint32, tag = "9")]
    pub right_mode: u32,
    /// Ancestor POSIX mode.
    #[prost(uint32, tag = "10")]
    pub ancestor_mode: u32,
}

impl ConflictManifest {
    /// Checks schema, kinds, and digest widths.
    ///
    /// # Errors
    ///
    /// Returns an error when a field is structurally invalid.
    pub fn check(&self) -> Result<(), CoreError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(CoreError::ManifestInvariant("unsupported conflict schema"));
        }
        let _ = self.conflict_kind()?;
        if let Some(ancestor) = &self.ancestor {
            let _ = AxintHash::try_from_slice(ancestor)?;
        }
        for side in &self.sides {
            let _ = AxintHash::try_from_slice(side)?;
        }
        for nested in &self.nested {
            let _ = AxintHash::try_from_slice(nested)?;
        }
        Ok(())
    }

    /// Parsed conflict kind.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ManifestInvariant`] when the ordinal is unknown.
    pub fn conflict_kind(&self) -> Result<ConflictKind, CoreError> {
        ConflictKind::try_from(self.kind).map_err(|_| CoreError::ManifestInvariant("unknown conflict kind"))
    }

    /// BLAKE3 of the protobuf encoding.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest fails validation.
    pub fn digest(&self) -> Result<AxintHash, CoreError> {
        self.check()?;
        Ok(AxintHash::hash_bytes(&self.encode_to_vec()))
    }

    /// Decodes and validates a conflict manifest.
    ///
    /// # Errors
    ///
    /// Returns a decode or validation error.
    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, CoreError> {
        let manifest = Self::decode(bytes)?;
        manifest.check()?;
        Ok(manifest)
    }

    /// Ancestor digest, when present.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidHashLength`] when the field has the wrong width.
    pub fn ancestor_hash(&self) -> Result<Option<AxintHash>, CoreError> {
        self.ancestor
            .as_ref()
            .map(|raw| AxintHash::try_from_slice(raw))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> Identity {
        Identity::new("Ada Lovelace", "ada@axint.dev", 1_700_000_000, 0).expect("identity")
    }

    #[test]
    fn change_id_is_uuid_v7() -> Result<(), CoreError> {
        let id = ChangeId::generate();
        let parsed = ChangeId::parse(&id.to_string())?;
        assert_eq!(id, parsed);
        assert_eq!(id.as_uuid().get_version(), Some(uuid::Version::SortRand));
        Ok(())
    }

    #[test]
    fn tree_orders_entries_and_rejects_duplicates() -> Result<(), CoreError> {
        let hash = AxintHash::hash_bytes(b"a");
        let entries = vec![
            TreeEntry::new("b.txt", EntryType::File, hash, 1, MODE_FILE)?,
            TreeEntry::new("a.txt", EntryType::File, hash, 1, MODE_FILE)?,
        ];
        let tree = TreeManifest {
            schema_version: SCHEMA_VERSION,
            entries,
        }
        .canonicalize()?;
        assert_eq!(tree.entries[0].name, "a.txt");
        assert_eq!(tree.entries[1].name, "b.txt");
        let digest = tree.digest()?;
        let encoded = tree.encode_to_vec();
        assert_eq!(TreeManifest::decode_canonical(&encoded)?.digest()?, digest);

        let duplicate = TreeManifest {
            schema_version: SCHEMA_VERSION,
            entries: vec![
                TreeEntry::new("a.txt", EntryType::File, hash, 1, MODE_FILE)?,
                TreeEntry::new("a.txt", EntryType::File, hash, 1, MODE_FILE)?,
            ],
        };
        assert!(duplicate.canonicalize().is_err());
        Ok(())
    }

    #[test]
    fn nfc_names_collapse() -> Result<(), CoreError> {
        let hash = AxintHash::hash_bytes(b"e");
        let composed = "é";
        let decomposed = "e\u{0301}";
        let left = TreeEntry::new(composed, EntryType::File, hash, 1, MODE_FILE)?;
        let right = TreeEntry::new(decomposed, EntryType::File, hash, 1, MODE_FILE)?;
        assert_eq!(left.name, right.name);
        Ok(())
    }

    #[test]
    fn sealed_commit_verifies_and_rejects_tampering() -> Result<(), CoreError> {
        let key = generate_signing_key();
        let draft = CommitDraft {
            root_tree_hash: AxintHash::hash_bytes(b"tree"),
            parents: Vec::new(),
            change_id: ChangeId::generate(),
            sequence_number: 0,
            author: identity(),
            committer: identity(),
            message: "initial".to_owned(),
            conflict_records: Vec::new(),
        };
        let commit = seal_commit(&draft, &key, &key)?;
        commit.verify()?;
        let encoded = commit.encode_to_vec();
        CommitManifest::decode_verified(&encoded)?;

        let mut tampered = commit.clone();
        tampered.message.push('!');
        assert!(matches!(tampered.verify(), Err(CoreError::CommitIdMismatch)));
        Ok(())
    }

    #[test]
    fn empty_file_manifest_round_trip() -> Result<(), CoreError> {
        let aggregate = AxintHash::hash_bytes(b"");
        let manifest = FileManifest::single_chunk(aggregate, 0, aggregate)?;
        assert!(manifest.chunks.is_empty());
        assert_eq!(manifest.digest()?, FileManifest::decode_canonical(&manifest.encode_to_vec())?.digest()?);
        Ok(())
    }
}
