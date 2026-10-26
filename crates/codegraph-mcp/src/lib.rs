//! Native MCP tools and newline-delimited JSON-RPC sessions.
pub mod daemon;
pub mod explore;
pub mod handoff;
pub mod render;
pub mod session;
pub mod tools;
pub use tools::{ToolHandler, ToolResult};
