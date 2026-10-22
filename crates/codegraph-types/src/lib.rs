//! Core graph types shared by every CodeGraph crate.
//!
//! This is the Rust port of `src/types.ts` plus the node-ID scheme. Everything
//! else — the store, extractors, resolver, MCP server — speaks these types.

mod ids;
pub mod jscompat;
mod kinds;
pub mod log;
mod model;
pub mod nodepath;
pub mod paths;

pub use ids::{file_node_id, node_id, node_id_str, NodeIdAllocator};
pub use indexmap::IndexMap;
pub use kinds::{
    Confidence, ContextFormat, Direction, EdgeKind, Language, NodeKind, Provenance, ReferenceKind,
    Severity, Visibility,
};
pub use model::*;
