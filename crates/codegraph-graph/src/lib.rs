//! Query-time graph algorithms. All queries operate on the shared SQLite
//! format and never mutate the index or invent edges.

pub mod cobol_copybooks;
pub mod dead_code;
pub mod dynamic_boundaries;
pub mod dynamic_boundary_report;
pub mod go_package;
pub mod named_symbol_flow;
pub mod queries;
pub mod symbol_lookup;
pub mod synthesized_hop;
pub mod traversal;
pub mod type_hierarchy;

pub use queries::{GraphQueryManager, NodeMetrics};
pub use traversal::{GraphTraverser, PathStep};

#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error(transparent)]
    Database(#[from] codegraph_db::DbError),
    #[error("Node not found: {0}")]
    NodeNotFound(String),
}

pub type Result<T> = std::result::Result<T, GraphError>;
