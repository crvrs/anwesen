//! Anwesen: read-only HTTP daemon over a markdown vault.
//!
//! The binary in `main.rs` wires this library to a CLI; submodules implement
//! the vault scanner, the Tantivy index, the filesystem watcher, the
//! supervisor tree, and the HTTP surface as those issues land.

pub mod app;
pub mod http;
pub mod index;
pub mod query;
pub mod store;
pub mod vault;
pub mod watcher;
