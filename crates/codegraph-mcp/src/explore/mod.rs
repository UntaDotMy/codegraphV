//! `codegraph_explore` — a parity port of `ToolHandler.handleExplore`
//! (`src/mcp/tools.ts`). The TypeScript handler is the oracle:
//! `scripts/rust-golden/explore.mjs` compares both on the same index.
pub mod helpers;
pub mod rank;
pub mod sections;

use crate::tools::ToolResult;
use crate::render::Result;
use codegraph_core::CodeGraph;
use serde_json::Value;
use std::collections::HashMap;

/// One explore call with no prior session state (cross-call dedup off).
pub fn handle(cg: &CodeGraph, args: &Value) -> Result<ToolResult> {
    crate::render::explore(cg, args, &mut HashMap::new())
}
