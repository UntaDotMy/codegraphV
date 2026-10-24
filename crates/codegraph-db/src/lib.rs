//! SQLite store for the CodeGraph knowledge graph — port of `src/db/*`.
//!
//! The on-disk format is identical to the TypeScript engine's (same
//! `schema.sql`, same migrations, same pragmas), so either engine opens an index
//! the other wrote.

mod error;
pub mod dead_code_queries;
pub mod graph_queries;
pub mod migrations;
pub mod reference_tail;
pub mod resolver_queries;
pub mod rows;
pub mod schema;
pub mod search_queries;
pub mod segment_queries;
pub mod sync_queries;
mod store;

pub use error::{DbError, Result};
pub use graph_queries::DOMINANT_FILE_METADATA_KEY;
pub use migrations::CURRENT_SCHEMA_VERSION;
pub use store::{
    database_path, remove_database_files, resolve_wal_heal_bytes, wal_heal_threshold_bytes, CheckpointMode,
    FileBundle, NodeIdentity, OpenOptions, RawConnection, Store, DATABASE_FILENAME, SQLITE_PARAM_CHUNK_SIZE,
};

/// Milliseconds since the Unix epoch (`Date.now()`).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
