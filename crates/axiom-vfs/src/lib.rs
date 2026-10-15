//! Working-tree watcher and on-demand hydration.
//!
//! `axiom status` drains a dirty-path ring instead of walking the tree. When
//! the ring overflows, the caller must scan. Hydration writes only the chunks
//! a reader asked for.

#![deny(clippy::all)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::needless_pass_by_value)]

pub mod error;
pub mod status;
pub mod virtual_fs;
pub mod watcher;

pub use error::VfsError;
pub use status::{compare_path, IndexStat, PathStatus};
pub use virtual_fs::{hydrate_bytes, read_range};
pub use watcher::{watch, DirtyEvent, DirtyKind, DirtyRing};
