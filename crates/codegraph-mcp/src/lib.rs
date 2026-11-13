//! Native MCP tools and newline-delimited JSON-RPC sessions.
pub mod daemon;
pub mod explore;
pub mod handoff;
pub mod owning_package;
pub mod render;
mod requests;
pub mod session;
pub mod tools;
pub use tools::{ToolHandler, ToolResult};
