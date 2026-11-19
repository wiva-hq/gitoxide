//! Native support for [Git LFS](https://git-lfs.com), compatible with repositories that the `git-lfs` program also uses.
//!
//! * [`Pointer`] parses and writes version 1 pointer files.
//! * [`Store`] is the local object store at `<git-common-dir>/lfs`, laid out as `git-lfs` lays it out.
//! * [`filter`] implements the `clean` and `smudge` operations of the `lfs` filter driver.
//! * [`Endpoint`] locates the LFS API of a remote, possibly through `git-lfs-authenticate` over SSH.
//! * [`Client`] talks to the batch API and transfers objects with the `basic` transfer adapter.
//!
//! ## Not supported
//!
//! File locking, custom transfer adapters (including `lfs-standalone-file` for local remotes), resumable uploads via
//! `tus`, the pure SSH transfer protocol (`git-lfs-transfer`), and applying pointer extensions while smudging.
#![deny(missing_docs)]
#![forbid(unsafe_code)]

mod oid;
pub use oid::Oid;

///
pub mod pointer;
pub use pointer::Pointer;

///
pub mod store;
pub use store::Store;

///
pub mod filter;

///
pub mod endpoint;
pub use endpoint::{Endpoint, Operation};

///
pub mod http;

///
pub mod batch;

///
pub mod client;
pub use client::Client;

mod time;
