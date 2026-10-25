//! `schema.sql` handling.
//!
//! The DDL is the TypeScript engine's own `src/db/schema.sql`, embedded verbatim
//! so a Rust-created database and a TypeScript-created one are the same file
//! format — either engine opens the other's index. Bulk-load windows recreate
//! the indexes and triggers they drop by extracting their DDL from this text
//! rather than duplicating it, so the DDL cannot drift from the schema.
//! (Re-executing the whole schema is not an option: it holds data INSERTs that
//! are not idempotent, e.g. `schema_versions`.)

use crate::error::{DbError, Result};
use regex::Regex;
use rusqlite::Connection;
use std::sync::LazyLock;

pub const SCHEMA_SQL: &str = include_str!("../../../src/db/schema.sql");

const FTS5_MARKER: &str = "-- Full-text search index on node names, docstrings, and signatures";

/// FTS ends after the update trigger; required tables and indexes follow it in
/// schema.sql and must still be created when FTS5 is unavailable.
static FTS_SECTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\s\S]*?CREATE TRIGGER IF NOT EXISTS nodes_au\b[\s\S]*?END;").unwrap());

static FTS_TRIGGER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"CREATE TRIGGER IF NOT EXISTS nodes_a[idu]\b[\s\S]*?END;").unwrap());

/// Create the schema on a fresh connection. Returns whether FTS5 is available:
/// the FTS section is split out and tried on its own so codegraph still works
/// when SQLite was built without FTS5 (#1532) — search then falls back to
/// LIKE + fuzzy matching.
pub fn apply_schema(conn: &Connection) -> Result<bool> {
    let Some(fts_idx) = SCHEMA_SQL.find(FTS5_MARKER) else {
        conn.execute_batch(SCHEMA_SQL)?;
        return Ok(true);
    };
    let pre_fts = &SCHEMA_SQL[..fts_idx];
    let fts_section = FTS_SECTION
        .find(&SCHEMA_SQL[fts_idx..])
        .ok_or_else(|| DbError::other("schema.sql: FTS5 update trigger not found"))?
        .as_str();
    conn.execute_batch(pre_fts)?;
    let fts5_available = match conn.execute_batch(fts_section) {
        Ok(()) => true,
        Err(e) => {
            eprintln!(
                "[codegraphV] FTS5 not available in this SQLite build ({e}). \
                 Search will fall back to LIKE + fuzzy matching."
            );
            false
        }
    };
    conn.execute_batch(&SCHEMA_SQL[fts_idx + fts_section.len()..])?;
    Ok(fts5_available)
}

/// The `CREATE INDEX IF NOT EXISTS <name> …;` statement for a (non-unique)
/// index of schema.sql.
pub fn create_index_ddl(name: &str) -> Option<String> {
    let re = Regex::new(&format!(r"CREATE INDEX IF NOT EXISTS {}\b[^;]*;", regex::escape(name))).ok()?;
    re.find(SCHEMA_SQL).map(|m| m.as_str().to_string())
}

/// The three FTS sync triggers (`nodes_ai`, `nodes_ad`, `nodes_au`).
pub fn fts_trigger_ddls() -> Result<Vec<String>> {
    let ddls: Vec<String> = FTS_TRIGGER.find_iter(SCHEMA_SQL).map(|m| m.as_str().to_string()).collect();
    if ddls.len() != 3 {
        return Err(DbError::other(format!("schema.sql: expected 3 nodes FTS triggers, found {}", ddls.len())));
    }
    Ok(ddls)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_index_ddl() {
        let ddl = create_index_ddl("idx_nodes_kind").unwrap();
        assert!(ddl.starts_with("CREATE INDEX IF NOT EXISTS idx_nodes_kind ON nodes("));
        assert!(ddl.ends_with(';'));
        // A name that is only a prefix of another index's name must not match it.
        assert!(create_index_ddl("idx_edges_synthesis").is_none());
        let site = create_index_ddl("idx_edges_synthesis_site").unwrap();
        assert!(site.contains("json_valid(metadata)") && site.contains("WHERE CASE"));
    }

    #[test]
    fn extracts_three_triggers() {
        let ddls = fts_trigger_ddls().unwrap();
        assert_eq!(ddls.len(), 3);
        assert!(ddls[0].contains("nodes_ai") && ddls[1].contains("nodes_ad") && ddls[2].contains("nodes_au"));
    }

    #[test]
    fn schema_applies_cleanly_with_fts() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(apply_schema(&conn).unwrap());
        let n: i64 =
            conn.query_row("SELECT count(*) FROM sqlite_master WHERE name = 'nodes_fts'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }
}
