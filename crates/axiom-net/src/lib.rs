//! Remote set reconciliation.
//!
//! An invertible Bloom lookup table lets two peers exchange a fixed-size
//! sketch and recover the commits that differ, without walking both histories.

#![deny(clippy::all, clippy::pedantic, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

pub mod error;
pub mod iblt;

pub use error::NetError;
pub use iblt::Iblt;
