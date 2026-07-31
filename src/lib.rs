//! Anwesen: read-only HTTP daemon over a markdown vault.
//!
//! The binary in `main.rs` wires this library to a CLI; submodules implement
//! the vault scanner, the in-memory note store, the filesystem watcher, the
//! supervisor tree, and the HTTP surface. See
//! [[ADR-009 Reverse ADR-002 In-Memory Evaluation No Tantivy]] for why v1
//! ships without a search index.

pub mod app;
pub mod doctor;
pub mod health;
pub mod http;
pub mod oneshot;
pub mod query;
pub mod store;
pub mod telemetry;
pub mod vault;
pub mod watcher;
