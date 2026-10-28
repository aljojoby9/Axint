//! Axint storage engine.
//!
//! FastCDC splits file bytes, Zstandard compresses each chunk, and `redb`
//! stores the working-tree index. [`repo::Repository`] is the local command API.

#![deny(clippy::all)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::match_same_arms)]
#![allow(clippy::map_unwrap_or)]
#![allow(clippy::similar_names)]

pub mod cas;
pub mod chunking;
pub mod compression;
pub mod db;
pub mod error;
pub mod repo;

pub use error::StorageError;
pub use repo::{
    ChangeKind, CommitSummary, LeafInput, RemoteConfig, RepoConfig, Repository, UserConfig, WorkChange,
};
