//! Git interoperability.
//!
//! Axiom commits are translated into Git commits, trees, and blobs. Publishing
//! to GitHub uses the local `git` binary so SSH keys and credential helpers
//! keep working. `Change-Id` trailers preserve Axiom's stable change identity.

#![deny(clippy::all, clippy::pedantic, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

pub mod error;
pub mod object;
pub mod sync;

pub use error::BridgeError;
pub use object::GitOid;
pub use sync::{
    clone_repository, export_bookmark, normalize_git_url, pull_bookmark, push_bookmark, push_bookmark_opts,
};
