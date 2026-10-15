//! Axiom storage engine.
//!
//! FastCDC splits file bytes, Zstandard compresses each chunk, and `redb`
//! stores the working-tree index. [`repo::Repository`] is the local command API.

#![deny(clippy::all, clippy::pedantic, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

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
