//! Task context retrieval and rendering, ported from `src/context/`.
pub mod builder;
pub mod formatter;
pub mod retrieval;
pub mod segments;
pub use builder::{BuildOutput, ContextBuilder};

/// Shared with MCP when deciding whether the response is comprehensive.
pub const LOW_CONFIDENCE_MARKER: &str = "### ⚠️ Low-confidence match";
