//! Axiom core data model.
//!
//! This crate holds the types every other Axiom crate shares: BLAKE3 digests,
//! protobuf manifests, the three-way merge, the commit graph, and the undo
//! journal. It does not talk to the network or the filesystem watcher.

#![deny(clippy::all)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::assigning_clones)]
#![allow(clippy::redundant_closure_for_method_calls)]
#![allow(clippy::match_same_arms)]
#![allow(clippy::map_unwrap_or)]

pub mod dag;
pub mod error;
pub mod hash;
pub mod manifest;
pub mod merge;
pub mod oplog;
pub mod store;

pub use dag::{CommitGraph, CommitNode};
pub use error::CoreError;
pub use hash::{AxiomHash, StreamingHasher, HASH_LEN};
pub use manifest::{
    entry_type_for_mode, generate_signing_key, normalize_component, seal_commit, signing_key_from_bytes,
    unix_millis, unix_timestamp, ChangeId, ChunkReference, CommitDraft, CommitManifest, ConflictKind,
    ConflictManifest, EntryType, FileManifest, Identity, Signature, SpanConflict, TreeEntry, TreeManifest,
    MODE_DIR, MODE_EXEC, MODE_FILE, MODE_SYMLINK, SCHEMA_VERSION,
};
pub use merge::{merge_trees, MergeOutput, NodeResolution};
pub use oplog::{OpLog, OperationEntry, OperationKind};
pub use store::{FileRecord, MemoryStore, ObjectStore};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_types_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AxiomHash>();
        assert_send_sync::<ChangeId>();
        assert_send_sync::<CommitManifest>();
        assert_send_sync::<TreeManifest>();
        assert_send_sync::<FileManifest>();
        assert_send_sync::<ConflictManifest>();
        assert_send_sync::<MergeOutput>();
        assert_send_sync::<CommitGraph>();
        assert_send_sync::<MemoryStore>();
    }
}
