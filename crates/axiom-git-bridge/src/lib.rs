//! Git interoperability.
//!
//! Axiom commits are translated into Git commits, trees, and blobs. Publishing
//! to GitHub uses the local `git` binary so SSH keys and credential helpers
//! keep working. `Change-Id` trailers preserve Axiom's stable change identity.

#![deny(clippy::all)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::format_push_string)]
#![allow(clippy::zero_sized_map_values)]
#![allow(clippy::case_sensitive_file_extension_comparisons)]
#![allow(clippy::collapsible_if)]

pub mod error;
pub mod object;
pub mod sync;

pub use error::BridgeError;
pub use object::GitOid;
pub use sync::{
    clone_repository, export_bookmark, normalize_git_url, pull_bookmark, push_bookmark, push_bookmark_opts,
};
