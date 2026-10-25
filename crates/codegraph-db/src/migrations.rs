//! Schema versioning and migrations — port of `src/db/migrations.ts`.
//!
//! Version 1 is the initial schema, applied from `schema.sql`; everything after
//! it is listed here. A database created from the current `schema.sql` already
//! contains every migration's effect and is stamped at
//! [`CURRENT_SCHEMA_VERSION`], so migrations only ever run against databases an
//! older release created.

use crate::error::Result;
use crate::reference_tail::{reference_name_tail, MEMBER_TAIL_SQL, MODULE_PARKED_SQL};
use rusqlite::Connection;

/// Current schema version.
pub const CURRENT_SCHEMA_VERSION: u32 = 16;

pub struct Migration {
    pub version: u32,
    pub description: &'static str,
    up: fn(&Connection) -> Result<()>,
}

fn has_column(db: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = db.prepare("SELECT name FROM pragma_table_info(?1)")?;
    let found = stmt
        .query_map([table], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .iter()
        .any(|n| n == column);
    Ok(found)
}

/// Rewrite `name_tail` for parked rows picked by `select`, via `tail_of`.
/// Idempotent: a rewritten tail rewrites to itself.
fn retail(
    db: &Connection,
    select: &str,
    only_if_changed: bool,
    tail_of: impl Fn(&str, &str) -> String,
) -> Result<()> {
    struct Row {
        id: i64,
        name: String,
        kind: String,
        tail: String,
    }
    let rows: Vec<Row> = {
        let mut stmt = db.prepare(select)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Row { id: r.get(0)?, name: r.get(1)?, kind: r.get(2)?, tail: r.get(3)? })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };
    let mut update = db.prepare("UPDATE unresolved_refs SET name_tail = ? WHERE id = ?")?;
    for row in rows {
        let tail = tail_of(&row.name, &row.kind);
        if !only_if_changed || tail != row.tail {
            update.execute(rusqlite::params![tail, row.id])?;
        }
    }
    Ok(())
}

fn migrations() -> Vec<Migration> {
    vec![
        Migration {
            version: 2,
            description: "Add project metadata, provenance tracking, and unresolved ref context",
            up: |db| {
                db.execute_batch(
                    "
                    CREATE TABLE IF NOT EXISTS project_metadata (
                      key TEXT PRIMARY KEY,
                      value TEXT NOT NULL,
                      updated_at INTEGER NOT NULL
                    );
                    ALTER TABLE unresolved_refs ADD COLUMN file_path TEXT NOT NULL DEFAULT '';
                    ALTER TABLE unresolved_refs ADD COLUMN language TEXT NOT NULL DEFAULT 'unknown';
                    ALTER TABLE edges ADD COLUMN provenance TEXT DEFAULT NULL;
                    CREATE INDEX IF NOT EXISTS idx_unresolved_file_path ON unresolved_refs(file_path);
                    CREATE INDEX IF NOT EXISTS idx_edges_provenance ON edges(provenance);
                    ",
                )?;
                Ok(())
            },
        },
        Migration {
            version: 3,
            description: "Add lower(name) expression index for memory-efficient case-insensitive lookups",
            up: |db| {
                db.execute_batch("CREATE INDEX IF NOT EXISTS idx_nodes_lower_name ON nodes(lower(name));")?;
                Ok(())
            },
        },
        Migration {
            version: 4,
            description: "Drop redundant idx_edges_source / idx_edges_target (covered by source_kind / target_kind composites)",
            up: |db| {
                db.execute_batch("DROP INDEX IF EXISTS idx_edges_source; DROP INDEX IF EXISTS idx_edges_target;")?;
                Ok(())
            },
        },
        Migration {
            version: 5,
            description: "Add nodes.return_type — normalized return/result type for receiver-type inference (C++ singletons/factories, #645)",
            up: |db| {
                db.execute_batch("ALTER TABLE nodes ADD COLUMN return_type TEXT;")?;
                Ok(())
            },
        },
        Migration {
            version: 6,
            description: "Dedup duplicate edge rows and add a UNIQUE identity index so INSERT OR IGNORE actually dedups (#1034)",
            // `insertEdge` has always used `INSERT OR IGNORE`, but the edges table had no
            // UNIQUE constraint, so nothing conflicted and byte-identical rows accumulated
            // whenever two passes emitted the same edge. Collapse each identity group to its
            // lowest id, then add the constraint that makes `OR IGNORE` keep its promise.
            // IFNULL folds nullable line/col so coordinate-less edges dedup too — and it MUST
            // match the GROUP BY exactly, or the index creation would fail on a pair the
            // DELETE left behind. Idempotent.
            up: |db| {
                db.execute_batch(
                    "
                    DELETE FROM edges
                    WHERE id NOT IN (
                      SELECT MIN(id) FROM edges
                      GROUP BY source, target, kind, IFNULL(line, -1), IFNULL(col, -1)
                    );
                    CREATE UNIQUE INDEX IF NOT EXISTS idx_edges_identity
                      ON edges(source, target, kind, IFNULL(line, -1), IFNULL(col, -1));
                    ",
                )?;
                Ok(())
            },
        },
        Migration {
            version: 7,
            description: "Add name_segment_vocab — prose-word → symbol-name lookup for the prompt hook’s graph-derived gate",
            // DDL only — instant on any size database. The table starts EMPTY on migrated
            // databases; `sync` detects that over a populated graph and backfills, and any
            // full index rebuilds it from scratch. Keep in lockstep with schema.sql.
            up: |db| {
                db.execute_batch(
                    "
                    CREATE TABLE IF NOT EXISTS name_segment_vocab (
                      segment TEXT NOT NULL,
                      name TEXT NOT NULL,
                      PRIMARY KEY (segment, name)
                    ) WITHOUT ROWID;
                    ",
                )?;
                Ok(())
            },
        },
        Migration {
            version: 8,
            description: "Track attempted-but-unresolvable refs as status=failed so sync can retry them when a changed file adds a matching symbol (#1240)",
            // DDL only. ALTER TABLE has no IF NOT EXISTS, so each column is guarded for
            // idempotency — a database created from current schema.sql already has both.
            up: |db| {
                if !has_column(db, "unresolved_refs", "status")? {
                    db.execute_batch("ALTER TABLE unresolved_refs ADD COLUMN status TEXT NOT NULL DEFAULT 'pending'")?;
                }
                if !has_column(db, "unresolved_refs", "name_tail")? {
                    db.execute_batch("ALTER TABLE unresolved_refs ADD COLUMN name_tail TEXT NOT NULL DEFAULT ''")?;
                }
                db.execute_batch(
                    "
                    CREATE INDEX IF NOT EXISTS idx_unresolved_status ON unresolved_refs(status);
                    CREATE INDEX IF NOT EXISTS idx_unresolved_failed_tail ON unresolved_refs(name_tail) WHERE status = 'failed';
                    ",
                )?;
                Ok(())
            },
        },
        Migration {
            version: 9,
            description: "Add files.generated — index-time content-header generated-file detection for ranking (#1500)",
            // DDL only, and NO backfill: the flag is derived from file CONTENT, which this
            // migration has no access to. Migrated rows stay 0 until the next full index
            // re-extracts them, and every reader unions the flag with the path-only check.
            up: |db| {
                if !has_column(db, "files", "generated")? {
                    db.execute_batch("ALTER TABLE files ADD COLUMN generated INTEGER NOT NULL DEFAULT 0")?;
                }
                db.execute_batch("CREATE INDEX IF NOT EXISTS idx_files_generated ON files(path) WHERE generated = 1")?;
                Ok(())
            },
        },
        Migration {
            version: 10,
            description: "Track synthesis inputs and stabilize synthesis traversal for incremental refresh (#1988)",
            up: |db| {
                db.execute_batch(
                    "
                    DROP INDEX IF EXISTS idx_nodes_kind;
                    CREATE INDEX idx_nodes_kind ON nodes(kind, file_path, start_line, id);
                    CREATE TABLE IF NOT EXISTS synthesis_inputs (
                      file_path TEXT PRIMARY KEY REFERENCES files(path) ON DELETE CASCADE
                    );
                    CREATE INDEX IF NOT EXISTS idx_edges_synthesis_site ON edges(CASE WHEN json_valid(metadata) THEN json_extract(metadata, '$.registeredAt') END)
                      WHERE CASE WHEN json_valid(metadata) THEN json_extract(metadata, '$.synthesizedBy') END IS NOT NULL;
                    UPDATE edges SET metadata = json_set(CASE WHEN json_valid(metadata) THEN metadata ELSE '{}' END, '$.synthesizedBy', 'go-method-contains')
                      WHERE kind = 'contains' AND provenance IS NULL AND EXISTS (
                        SELECT 1 FROM nodes s JOIN nodes t ON t.id = edges.target
                        WHERE s.id = edges.source AND s.language = 'go' AND t.language = 'go'
                          AND s.kind IN ('struct', 'class', 'interface', 'enum', 'type_alias') AND t.kind = 'method'
                          AND s.file_path != t.file_path
                      );
                    INSERT OR REPLACE INTO project_metadata(key, value, updated_at)
                      VALUES ('synthesis_pending', '1', 0);
                    ",
                )?;
                Ok(())
            },
        },
        Migration {
            version: 11,
            description: "Guard synthesis metadata lookups against malformed JSON",
            up: |db| {
                db.execute_batch(
                    "
                    DROP INDEX IF EXISTS idx_edges_synthesis_site;
                    CREATE INDEX idx_edges_synthesis_site
                      ON edges(CASE WHEN json_valid(metadata) THEN json_extract(metadata, '$.registeredAt') END)
                      WHERE CASE WHEN json_valid(metadata) THEN json_extract(metadata, '$.synthesizedBy') END IS NOT NULL;
                    ",
                )?;
                Ok(())
            },
        },
        Migration {
            version: 12,
            description: "Retry a failed import when sync adds the file it names: path tails and failed-import indexes",
            up: |db| {
                db.execute_batch(
                    "
                    CREATE INDEX IF NOT EXISTS idx_unresolved_failed_import_tail ON unresolved_refs(reference_kind, name_tail) WHERE status = 'failed' AND reference_kind = 'imports';
                    CREATE INDEX IF NOT EXISTS idx_unresolved_failed_import_name ON unresolved_refs(reference_kind, reference_name) WHERE status = 'failed' AND reference_kind = 'imports';
                    ",
                )?;
                // A path import parked before this version carries its dotted tail — the
                // extension or a path fragment — which no file's keys match. Rewrite it to
                // the path tail a failed import is parked under now.
                retail(
                    db,
                    "SELECT id, reference_name, reference_kind, name_tail FROM unresolved_refs WHERE status = 'failed' AND reference_kind = 'imports' AND reference_name LIKE '%/%'",
                    false,
                    |name, _| reference_name_tail(name, Some("imports")),
                )
            },
        },
        Migration {
            version: 13,
            description: "Retry a failed path reference when sync adds the file it names: file-name tails",
            up: |db| {
                // A path reference parked before this version — a Liquid
                // `snippets/price.liquid` — carries its extension as its tail, which no node
                // is named. Rewrite it to the file name a failed path reference is parked
                // under now.
                retail(
                    db,
                    "SELECT id, reference_name, reference_kind, name_tail FROM unresolved_refs WHERE status = 'failed' AND reference_kind = 'references' AND reference_name LIKE '%/%.%'",
                    true,
                    |name, _| reference_name_tail(name, Some("references")),
                )
            },
        },
        Migration {
            version: 14,
            description: "Retry a failed route module reference when sync adds or changes its file: module tails",
            up: |db| {
                // A route's reference to the module it lazily loads, parked before this
                // version, carries a fragment of its path as its tail. Rewrite it to the
                // module tail it is parked under now. The `+` keeps the planner off
                // idx_unresolved_status, which it otherwise picks although 'failed' is nearly
                // every row.
                retail(
                    db,
                    "SELECT id, reference_name, reference_kind, name_tail FROM unresolved_refs
                      WHERE (reference_name GLOB 'lazy-import:*' OR reference_name GLOB 'import:*' OR reference_name GLOB 'layout:*')
                        AND +status = 'failed' AND +reference_kind IN ('references', 'calls')",
                    true,
                    |name, kind| reference_name_tail(name, Some(kind)),
                )
            },
        },
        Migration {
            version: 15,
            description: "Retry a failed reference through an import binding by its whole name: module-tail name index",
            up: |db| {
                db.execute_batch(
                    "CREATE INDEX IF NOT EXISTS idx_unresolved_failed_module_name ON unresolved_refs(status, reference_name) WHERE status = 'failed' AND name_tail GLOB 'module:*';",
                )?;
                Ok(())
            },
        },
        Migration {
            version: 16,
            description: "Retry a failed member reference through an import binding by its member: module-tail member index",
            up: |db| {
                db.execute_batch(&format!(
                    "CREATE INDEX IF NOT EXISTS idx_unresolved_failed_module_member ON unresolved_refs(status, {MEMBER_TAIL_SQL}) WHERE {MODULE_PARKED_SQL};"
                ))?;
                Ok(())
            },
        },
    ]
}

/// The current schema version recorded in the database; 0 when the table
/// doesn't exist yet.
pub fn get_current_version(db: &Connection) -> u32 {
    db.query_row("SELECT MAX(version) FROM schema_versions", [], |r| r.get::<_, Option<u32>>(0))
        .ok()
        .flatten()
        .unwrap_or(0)
}

fn record_migration(db: &Connection, version: u32, description: &str) -> Result<()> {
    db.execute(
        "INSERT INTO schema_versions (version, applied_at, description) VALUES (?1, ?2, ?3)",
        rusqlite::params![version, crate::now_ms(), description],
    )?;
    Ok(())
}

/// Run every migration newer than `from_version`, each in its own transaction.
pub fn run_migrations(db: &Connection, from_version: u32) -> Result<()> {
    let mut pending: Vec<Migration> = migrations().into_iter().filter(|m| m.version > from_version).collect();
    pending.sort_by_key(|m| m.version);
    for m in pending {
        db.execute_batch("BEGIN")?;
        let result = (m.up)(db).and_then(|_| record_migration(db, m.version, m.description));
        match result {
            Ok(()) => db.execute_batch("COMMIT")?,
            Err(e) => {
                let _ = db.execute_batch("ROLLBACK");
                return Err(e);
            }
        }
    }
    Ok(())
}

/// Whether the database needs migration.
pub fn needs_migration(db: &Connection) -> bool {
    get_current_version(db) < CURRENT_SCHEMA_VERSION
}

/// The migrations newer than the database's recorded version, oldest first.
pub fn get_pending_migrations(db: &Connection) -> Vec<(u32, &'static str)> {
    let current = get_current_version(db);
    let mut v: Vec<_> =
        migrations().into_iter().filter(|m| m.version > current).map(|m| (m.version, m.description)).collect();
    v.sort_by_key(|m| m.0);
    v
}

/// One row of the migration history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationRecord {
    pub version: u32,
    pub applied_at: i64,
    pub description: Option<String>,
}

pub fn get_migration_history(db: &Connection) -> Result<Vec<MigrationRecord>> {
    let mut stmt = db.prepare("SELECT version, applied_at, description FROM schema_versions ORDER BY version")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(MigrationRecord { version: r.get(0)?, applied_at: r.get(1)?, description: r.get(2)? })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}
