//! The store: connection lifecycle (port of `src/db/index.ts`) together with the
//! write paths and the core read queries (port of the first half of
//! `QueryBuilder` in `src/db/queries.ts`).
//!
//! One type owns both because the TypeScript split between
//! `DatabaseConnection` and `QueryBuilder` existed to share a connection across
//! worker threads. Here a `Store` is a single connection plus its caches; a
//! thread that needs the database opens its own (read-only) `Store`.
//!
//! Resolution-phase and ranking queries live in sibling modules as they are
//! ported, on the same type.

use crate::error::{DbError, Result};
use crate::migrations::{get_current_version, run_migrations, CURRENT_SCHEMA_VERSION};
use crate::rows::*;
use crate::schema;
use codegraph_search::split_identifier_segments;
use codegraph_types::{
    Edge, EdgeKind, FileRecord, GraphStats, IndexMap, Language, Node, NodeKind, SchemaVersion,
    UnresolvedReference,
};
use lru::LruCache;
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OpenFlags, ToSql};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

pub use rusqlite::Connection as RawConnection;

/// Default database filename.
pub const DATABASE_FILENAME: &str = "codegraph.db";

/// SQLite's bound-parameter budget per statement is generous, but a chunk this
/// size keeps every `IN (…)` query plan simple across SQLite builds.
pub const SQLITE_PARAM_CHUNK_SIZE: usize = 500;

/// Multi-row INSERT sizes the bulk path decomposes into. Each size's statement
/// is prepared once and reused; row order within and across chunks is the input
/// order, so rowid assignment (and resolution's insertion-order disambiguation)
/// is identical to the one-row-per-run path.
const BATCH_SIZES: [usize; 4] = [128, 32, 8, 1];

const NODE_CACHE_SIZE: usize = 1000;
const MAX_SEGMENTED_NAMES: usize = 65536;

/// WAL size past which opening heals the file (checkpoint + truncate), and to
/// which `journal_size_limit` clips the WAL after any resetting checkpoint. A
/// killed process (watchdog, OOM, crash) can leave an arbitrarily large WAL
/// behind and, before #1431, no later session ever shrank it. 64 MB is far above
/// anything a healthy open sees yet small enough to cap the leak. Override with
/// `CODEGRAPH_WAL_HEAL_MB`.
pub fn wal_heal_threshold_bytes() -> u64 {
    resolve_wal_heal_bytes(std::env::var("CODEGRAPHV_WAL_HEAL_MB").ok().as_deref())
}

/// Resolve the heal threshold from the env override (MB); invalid ⇒ 64 MB.
pub fn resolve_wal_heal_bytes(env_val: Option<&str>) -> u64 {
    if let Some(v) = env_val.filter(|v| !v.is_empty()) {
        if let Ok(n) = v.trim().parse::<f64>() {
            if n.is_finite() && n > 0.0 {
                return (n * 1024.0 * 1024.0).floor() as u64;
            }
        }
    }
    64 * 1024 * 1024
}

/// Options for [`Store::open`].
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenOptions {
    pub read_only: bool,
}

/// The fields that tell one node of a file from another across a re-index of
/// that file, whose node ids change with every line shift (#2276).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeIdentity {
    pub id: String,
    pub kind: NodeKind,
    pub name: String,
    pub qualified_name: String,
    pub signature: Option<String>,
    pub start_line: u32,
    pub start_column: u32,
}

/// One file's extraction output, stored in a single transaction.
pub struct FileBundle<'a> {
    pub nodes: &'a [Node],
    pub edges: &'a [Edge],
    pub refs: &'a [UnresolvedReference],
    pub file: &'a FileRecord,
}

type SqlRow = Vec<Value>;

pub struct Store {
    conn: Connection,
    path: PathBuf,
    read_only: bool,
    fts5_available: bool,
    /// `dev:ino` of the DB file at open (None where the platform has no usable
    /// inode). Lets us notice the file we hold open was unlinked and REPLACED at
    /// the same path — a worktree removed and re-added, or `.codegraph/` deleted
    /// and re-`init`ed under a long-lived server (#925).
    opened_inode: Option<String>,
    tx_depth: Cell<u32>,
    node_cache: RefCell<LruCache<String, Node>>,
    /// Names whose segments were already written this session — skips
    /// re-splitting for the same-named nodes that repeat across files. Purely a
    /// write-path fast path; INSERT OR IGNORE is the correctness backstop.
    segmented_names: RefCell<HashSet<String>>,
    batch_sql: RefCell<HashMap<(&'static str, usize), String>>,
    project_name_tokens: RefCell<HashSet<String>>,
    pub(crate) dominant_file_memo: RefCell<Option<crate::graph_queries::DominantMemo>>,
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

impl Store {
    fn wrap(conn: Connection, path: &Path, read_only: bool, fts5_available: bool) -> Store {
        conn.set_prepared_statement_cache_capacity(256);
        Store {
            conn,
            path: path.to_path_buf(),
            read_only,
            fts5_available,
            opened_inode: stat_inode(path),
            tx_depth: Cell::new(0),
            node_cache: RefCell::new(LruCache::new(NonZeroUsize::new(NODE_CACHE_SIZE).unwrap())),
            segmented_names: RefCell::new(HashSet::new()),
            batch_sql: RefCell::new(HashMap::new()),
            project_name_tokens: RefCell::new(HashSet::new()),
            dominant_file_memo: RefCell::new(None),
        }
    }

    /// Run a PRAGMA, discarding any row it returns (`mmap_size`, `journal_mode`
    /// and friends answer even when set).
    fn pragma(conn: &Connection, sql: &str) -> Result<()> {
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query([])?;
        while rows.next()?.is_some() {}
        Ok(())
    }

    /// Apply connection-level PRAGMAs. Shared by `initialize` and `open` so the
    /// two paths can't drift.
    ///
    /// `busy_timeout` is set FIRST, before any pragma that might touch the
    /// database file (notably `journal_mode`). If another process holds a write
    /// lock at open time, the later pragmas — and the connection's first query —
    /// then wait out the lock instead of failing immediately (#238). The 5s
    /// window rides out a normal incremental sync; with WAL, reads never block on
    /// a writer, so this only governs cross-process write contention.
    fn configure_connection(conn: &Connection, read_only: bool) -> Result<()> {
        Self::pragma(conn, "PRAGMA busy_timeout = 5000")?;
        Self::pragma(conn, "PRAGMA foreign_keys = ON")?;
        if !read_only {
            Self::pragma(conn, "PRAGMA journal_mode = WAL")?;
        }
        Self::pragma(conn, "PRAGMA synchronous = NORMAL")?; // safe with WAL
        Self::pragma(conn, "PRAGMA cache_size = -64000")?; // 64 MB page cache
        Self::pragma(conn, "PRAGMA temp_store = MEMORY")?;
        Self::pragma(conn, "PRAGMA mmap_size = 268435456")?; // 256 MB memory-mapped I/O
        // Without a journal_size_limit the -wal file never shrinks below its
        // high-water mark while a connection lives (#1431).
        Self::pragma(conn, &format!("PRAGMA journal_size_limit = {}", wal_heal_threshold_bytes()))?;
        Ok(())
    }

    /// Initialize a new database at `db_path`, creating parent directories.
    pub fn initialize(db_path: impl AsRef<Path>) -> Result<Store> {
        let db_path = db_path.as_ref();
        if let Some(dir) = db_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        configure_sqlite();
        let conn = Connection::open(db_path)?;
        Self::configure_connection(&conn, false)?;

        // Run schema initialization, splitting FTS5 from the rest so codegraph
        // still works when SQLite was built without FTS5 (#1532).
        let fts5_available = schema::apply_schema(&conn)?;

        // Record the current schema version so migrations aren't re-applied on open.
        if get_current_version(&conn) < CURRENT_SCHEMA_VERSION {
            conn.execute(
                "INSERT OR IGNORE INTO schema_versions (version, applied_at, description) VALUES (?1, ?2, ?3)",
                params![CURRENT_SCHEMA_VERSION, crate::now_ms(), "Initial schema includes all migrations"],
            )?;
        }
        Ok(Self::wrap(conn, db_path, false, fts5_available))
    }

    /// Open an existing database.
    pub fn open(db_path: impl AsRef<Path>, options: OpenOptions) -> Result<Store> {
        let db_path = db_path.as_ref();
        if !db_path.exists() {
            return Err(DbError::NotFound(db_path.to_path_buf()));
        }
        configure_sqlite();
        let conn = if options.read_only {
            Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?
        } else {
            Connection::open(db_path)?
        };
        Self::configure_connection(&conn, options.read_only)?;

        // Detect FTS5 availability for search fallback (#1532).
        let fts5_available = conn.prepare("SELECT * FROM nodes_fts LIMIT 0").is_ok();

        let store = Self::wrap(conn, db_path, options.read_only, fts5_available);
        // A concurrent reader must leave migrations, bulk-load repair, and WAL
        // maintenance to the writer, including when versions differ (#1963).
        if options.read_only {
            return Ok(store);
        }
        let current = get_current_version(&store.conn);
        if current < CURRENT_SCHEMA_VERSION {
            run_migrations(&store.conn, current)?;
        }

        // Self-heal a bulk-load window that never closed (crash between
        // begin_bulk_node_load and end_bulk_node_load): the FTS triggers are
        // missing and nodes_fts is stale. Rebuild + recreate so search stays in sync.
        store.heal_bulk_node_load()?;
        store.heal_bulk_secondary_indexes()?;
        // Self-heal a killed session's leftover oversized WAL (#1431).
        store.heal_oversized_wal();
        Ok(store)
    }

    /// The underlying SQLite connection.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Whether FTS5 is available in this SQLite build. When false, search falls
    /// back to LIKE + fuzzy matching (#1532).
    pub fn fts5_available(&self) -> bool {
        self.fts5_available
    }

    /// The SQLite backend serving this connection (reported by `codegraph status`).
    pub fn backend(&self) -> &'static str {
        "rusqlite-bundled"
    }

    /// True when the DB file at our path has been REPLACED on disk since we
    /// opened it — a different inode now lives at the same path, so the fd we
    /// still hold points at a now-unlinked inode that can never receive new
    /// writes (#925). False when the inode is unchanged, when the file is
    /// momentarily absent (mid-recreate), or when the platform doesn't report a
    /// usable inode (Windows can't unlink an open file).
    pub fn is_replaced_on_disk(&self) -> bool {
        match (&self.opened_inode, stat_inode(&self.path)) {
            (Some(opened), Some(current)) => *opened != current,
            _ => false,
        }
    }

    /// The journal mode actually in effect (e.g. 'wal', 'delete'). SQLite
    /// silently keeps the prior mode if WAL can't be enabled — filesystems
    /// without shared-memory support (some network mounts, WSL2 `/mnt`) — so
    /// this can differ from what `configure_connection` requested (#238).
    pub fn journal_mode(&self) -> Result<String> {
        let mode: String = self.conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        Ok(mode.to_lowercase())
    }

    pub fn schema_version(&self) -> Result<Option<SchemaVersion>> {
        let row = self
            .conn
            .query_row(
                "SELECT version, applied_at, description FROM schema_versions ORDER BY version DESC LIMIT 1",
                [],
                |r| {
                    Ok(SchemaVersion {
                        version: r.get(0)?,
                        applied_at: r.get(1)?,
                        description: r.get::<_, Option<String>>(2)?,
                    })
                },
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })?;
        Ok(row)
    }

    /// Database file size in bytes.
    pub fn size(&self) -> Result<u64> {
        Ok(std::fs::metadata(&self.path)?.len())
    }

    /// Size of the `-wal` sidecar file in bytes. 0 when it doesn't exist.
    pub fn wal_size_bytes(&self) -> u64 {
        let mut p = self.path.clone().into_os_string();
        p.push("-wal");
        std::fs::metadata(PathBuf::from(p)).map(|m| m.len()).unwrap_or(0)
    }

    /// Current `wal_autocheckpoint` interval in pages (0 = disabled).
    pub fn wal_autocheckpoint(&self) -> Result<i64> {
        Ok(self.conn.query_row("PRAGMA wal_autocheckpoint", [], |r| r.get(0))?)
    }

    /// Set `wal_autocheckpoint` (pages; 0 disables). Bulk indexing can defer
    /// checkpoints entirely (#1231): the default 1000-page auto-checkpoint
    /// re-writes hot B-tree/FTS pages into the main file over and over.
    pub fn set_wal_autocheckpoint(&self, pages: u32) -> Result<()> {
        Self::pragma(&self.conn, &format!("PRAGMA wal_autocheckpoint = {pages}"))
    }

    /// `PRAGMA wal_checkpoint(mode)`; the result row is `(busy, log, checkpointed)`.
    pub fn checkpoint_wal(&self, mode: CheckpointMode) -> Option<(i64, i64, i64)> {
        let sql = format!("PRAGMA wal_checkpoint({})", mode.as_str());
        self.conn.query_row(&sql, [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).ok()
    }

    /// Shrink a leftover oversized WAL (#1431). Cost is one `stat` when the WAL
    /// is small (the overwhelmingly common case). Past the threshold it folds the
    /// log and truncates the file; a racing writer degrades this to a no-op that
    /// the next open retries.
    fn heal_oversized_wal(&self) -> (bool, u64, u64) {
        let before = self.wal_size_bytes();
        if before <= wal_heal_threshold_bytes() {
            return (false, before, before);
        }
        let _ = Self::pragma(&self.conn, "PRAGMA busy_timeout = 2000");
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
            self.checkpoint_wal(CheckpointMode::Passive);
            self.checkpoint_wal(CheckpointMode::Truncate);
            if self.wal_size_bytes() <= wal_heal_threshold_bytes() {
                break;
            }
        }
        let _ = Self::pragma(&self.conn, "PRAGMA busy_timeout = 5000");
        let after = self.wal_size_bytes();
        (after < before, before, after)
    }

    /// Lightweight maintenance to run after bulk writes (indexAll, sync):
    /// `PRAGMA optimize` (incremental ANALYZE, so the planner has statistics on
    /// freshly bulk-loaded tables) and a passive WAL checkpoint (so the WAL
    /// doesn't grow unboundedly between automatic checkpoints). Best-effort —
    /// never load-bearing for correctness.
    pub fn run_maintenance(&self) {
        let _ = Self::pragma(&self.conn, "PRAGMA analysis_limit=1000");
        let _ = Self::pragma(&self.conn, "PRAGMA optimize");
        self.checkpoint_wal(CheckpointMode::Passive);
    }

    /// `VACUUM` + `ANALYZE`.
    pub fn optimize(&self) -> Result<()> {
        self.conn.execute_batch("VACUUM")?;
        self.conn.execute_batch("ANALYZE")?;
        Ok(())
    }

    /// Run `f` as one transaction. A nested call joins the enclosing transaction
    /// (BEGIN would fail inside one), so no caller relies on nested rollback
    /// granularity — flattening is behavior-preserving.
    pub fn transaction<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let depth = self.tx_depth.get();
        if depth > 0 {
            self.tx_depth.set(depth + 1);
            let r = f();
            self.tx_depth.set(depth);
            return r;
        }
        self.conn.execute_batch("BEGIN")?;
        self.tx_depth.set(1);
        let result = f();
        self.tx_depth.set(0);
        match result {
            Ok(v) => match self.conn.execute_batch("COMMIT") {
                Ok(()) => Ok(v),
                Err(e) => {
                    let _ = self.conn.execute_batch("ROLLBACK");
                    Err(e.into())
                }
            },
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Open a transaction that [`Store::transaction`] calls then join, until
    /// [`Store::commit_batch`] or [`Store::rollback_batch`]. For a writer storing many small units:
    /// each unit stays atomic, and the batch saves a commit (and its log writes) per unit. No-op
    /// when a transaction is already open.
    pub fn begin_batch(&self) -> Result<()> {
        if self.tx_depth.get() == 0 {
            self.conn.execute_batch("BEGIN")?;
            self.tx_depth.set(1);
        }
        Ok(())
    }

    /// Commit the batch [`Store::begin_batch`] opened.
    pub fn commit_batch(&self) -> Result<()> {
        if self.tx_depth.get() != 1 {
            return Ok(());
        }
        self.tx_depth.set(0);
        if let Err(e) = self.conn.execute_batch("COMMIT") {
            let _ = self.conn.execute_batch("ROLLBACK");
            return Err(e.into());
        }
        Ok(())
    }

    /// Drop everything the open batch wrote: a unit inside it failed part-way (a joined
    /// transaction cannot roll back on its own).
    pub fn rollback_batch(&self) {
        if self.tx_depth.get() == 1 {
            self.tx_depth.set(0);
            let _ = self.conn.execute_batch("ROLLBACK");
        }
    }

    /// Alias kept for parity with `QueryBuilder.runInTransaction`.
    pub fn run_in_transaction<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        self.transaction(f)
    }

    /// Whether this store is currently inside a transaction opened through
    /// [`Store::transaction`].
    pub fn in_transaction(&self) -> bool {
        self.tx_depth.get() > 0
    }

    /// Close the connection, surfacing the error SQLite reports (a plain drop
    /// swallows it).
    pub fn close(self) -> Result<()> {
        self.conn.close().map_err(|(_, e)| DbError::from(e))
    }
}

/// `wal_checkpoint` mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointMode {
    Passive,
    Truncate,
}

impl CheckpointMode {
    fn as_str(self) -> &'static str {
        match self {
            CheckpointMode::Passive => "PASSIVE",
            CheckpointMode::Truncate => "TRUNCATE",
        }
    }
}

#[cfg(unix)]
fn stat_inode(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| format!("{}:{}", m.dev(), m.ino()))
}

/// Windows can't unlink an open file and its inode numbers are unreliable, so
/// replacement detection never fires there.
#[cfg(not(unix))]
fn stat_inode(_path: &Path) -> Option<String> {
    None
}

// ---------------------------------------------------------------------------
// Bulk-load windows
// ---------------------------------------------------------------------------

/// FTS maintenance triggers dropped/recreated around a bulk load. Names must
/// match schema.sql.
const FTS_TRIGGER_NAMES: [&str; 3] = ["nodes_ai", "nodes_ad", "nodes_au"];

/// NON-UNIQUE secondary indexes maintained per-row during the parse phase's
/// bulk inserts. A fresh init writes every row of nodes/unresolved_refs/files
/// exactly once and reads none of them until resolution, so the parse window
/// can drop all of these and rebuild each in one table scan afterwards.
/// Primary keys and UNIQUE constraints stay (upserts and OR-IGNORE dedup
/// conflict on them).
const BULK_PARSE_INDEX_NAMES: [&str; 15] = [
    "idx_nodes_kind",
    "idx_nodes_name",
    "idx_nodes_qualified_name",
    "idx_nodes_file_path",
    "idx_nodes_language",
    "idx_nodes_file_line",
    "idx_nodes_lower_name",
    "idx_unresolved_from_node",
    "idx_unresolved_name",
    "idx_unresolved_file_path",
    "idx_unresolved_from_name",
    "idx_unresolved_status",
    "idx_unresolved_failed_tail",
    "idx_files_language",
    "idx_files_modified_at",
];

/// unresolved_refs secondary indexes NOT read by the batched resolution loop.
/// The loop pages pending refs by keyset (`status='pending' AND id>?` — the
/// status index + PK), deletes resolved rows by id, and parks failures with a
/// status UPDATE; every other ref index serves SYNC-time paths.
const BULK_REF_INDEX_NAMES: [&str; 5] = [
    "idx_unresolved_from_node",
    "idx_unresolved_name",
    "idx_unresolved_file_path",
    "idx_unresolved_from_name",
    "idx_unresolved_failed_tail",
];

/// Names of the NON-UNIQUE edge indexes dropped for a bulk edge load.
/// `idx_edges_identity` deliberately stays: INSERT OR IGNORE's dedup conflicts
/// on it (#1034), and its leftmost column is `source`, so source-keyed reads
/// resolution makes mid-window keep an index via its prefix.
const BULK_EDGE_INDEX_NAMES: [&str; 5] = [
    "idx_edges_kind",
    "idx_edges_source_kind",
    "idx_edges_target_kind",
    "idx_edges_provenance",
    "idx_edges_synthesis_site",
];

const SYNTHESIS_SITE_INDEX: &str = "idx_edges_synthesis_site";

impl Store {
    fn drop_indexes(&self, names: &[&str]) -> Result<()> {
        for idx in names {
            self.conn.execute_batch(&format!("DROP INDEX IF EXISTS {idx}"))?;
        }
        Ok(())
    }

    fn create_indexes(&self, names: &[&str], what: &str) -> Result<()> {
        for idx in names {
            let ddl = schema::create_index_ddl(idx)
                .ok_or_else(|| DbError::other(format!("schema.sql: {what} index {idx} not found for bulk-load recreation")))?;
            self.conn.execute_batch(&ddl)?;
        }
        Ok(())
    }

    /// Enter bulk-load mode: drop the per-row FTS sync triggers so mass node
    /// inserts skip per-row tokenization. MUST be paired with
    /// [`Store::end_bulk_node_load`]; a crash inside the window is healed on the
    /// next `open`. The window is DB-wide (triggers are schema objects), which
    /// is safe because the end rebuilds `nodes_fts` from the `nodes` table
    /// wholesale — any row written by anyone during the window is captured.
    pub fn begin_bulk_node_load(&self) -> Result<()> {
        if !self.fts5_available {
            return Ok(());
        }
        for t in FTS_TRIGGER_NAMES {
            self.conn.execute_batch(&format!("DROP TRIGGER IF EXISTS {t}"))?;
        }
        Ok(())
    }

    /// Leave bulk-load mode: rebuild the whole FTS index from the nodes table in
    /// one pass (far cheaper than per-row trigger firings), then recreate the
    /// triggers from schema.sql.
    pub fn end_bulk_node_load(&self) -> Result<()> {
        if !self.fts5_available {
            return Ok(());
        }
        self.conn.execute_batch("INSERT INTO nodes_fts(nodes_fts) VALUES('rebuild')")?;
        for ddl in schema::fts_trigger_ddls()? {
            self.conn.execute_batch(&ddl)?;
        }
        Ok(())
    }

    /// Enter bulk-parse-load mode (FRESH-INIT ONLY — an incremental index
    /// deletes per-file rows mid-phase and needs the file_path indexes): drop
    /// every parse-lane secondary index, including the non-unique edge indexes.
    pub fn begin_bulk_parse_load(&self) -> Result<()> {
        self.drop_indexes(&BULK_PARSE_INDEX_NAMES)?;
        self.begin_bulk_edge_load()
    }

    /// Leave bulk-parse-load mode: recreate everything the window dropped, one
    /// table scan per index. The edge indexes are rebuilt too, so paths that
    /// never enter the resolution phase's own bulk-edge window are left with a
    /// complete schema.
    pub fn end_bulk_parse_load(&self) -> Result<()> {
        self.create_indexes(&BULK_PARSE_INDEX_NAMES, "parse")?;
        self.end_bulk_edge_load(false)
    }

    /// Enter bulk-ref mode for the batched resolution loop.
    pub fn begin_bulk_ref_load(&self) -> Result<()> {
        self.drop_indexes(&BULK_REF_INDEX_NAMES)
    }

    /// Leave bulk-ref mode: recreate each index in one scan.
    pub fn end_bulk_ref_load(&self) -> Result<()> {
        self.create_indexes(&BULK_REF_INDEX_NAMES, "ref")
    }

    /// Enter bulk-edge-load mode: drop the non-unique edge indexes so the mass
    /// INSERT OR IGNORE stream pays one B-tree (the identity index) instead of
    /// five — measured 2.8s → 1.1s inserting a 224k-edge resolution set.
    pub fn begin_bulk_edge_load(&self) -> Result<()> {
        self.drop_indexes(&BULK_EDGE_INDEX_NAMES)
    }

    /// Leave bulk-edge-load mode: recreate the dropped indexes in one pass each
    /// over the (now fully loaded) edges table. With `defer_synthesis_site` the
    /// sync-only synthesis-site index is left for [`Store::create_synthesis_site_index`].
    pub fn end_bulk_edge_load(&self, defer_synthesis_site: bool) -> Result<()> {
        let names: Vec<&str> = BULK_EDGE_INDEX_NAMES
            .iter()
            .copied()
            .filter(|n| !(defer_synthesis_site && *n == SYNTHESIS_SITE_INDEX))
            .collect();
        self.create_indexes(&names, "edge")
    }

    /// The sync-only synthesis-site index (#1988), which `end_bulk_edge_load`
    /// can leave for later. Its partial predicate runs json_valid over the
    /// metadata of every edge, while nothing before the end of an index reads
    /// it. Idempotent.
    pub fn create_synthesis_site_index(&self) -> Result<()> {
        self.create_indexes(&[SYNTHESIS_SITE_INDEX], "edge")
    }

    /// Recreate the FTS triggers + rebuild if a bulk-load window never closed.
    fn heal_bulk_node_load(&self) -> Result<()> {
        if !self.fts5_available {
            return Ok(());
        }
        let c: i64 = self.conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'trigger' AND name IN ('nodes_ai','nodes_ad','nodes_au')",
            [],
            |r| r.get(0),
        )?;
        if c as usize >= FTS_TRIGGER_NAMES.len() {
            return Ok(());
        }
        self.end_bulk_node_load()
    }

    /// Recreate every secondary index a killed bulk parse/ref/edge window may
    /// leave dropped.
    fn heal_bulk_secondary_indexes(&self) -> Result<()> {
        let mut names: Vec<&str> = Vec::new();
        for n in BULK_PARSE_INDEX_NAMES.iter().chain(&BULK_REF_INDEX_NAMES).chain(&BULK_EDGE_INDEX_NAMES) {
            if !names.contains(n) {
                names.push(n);
            }
        }
        let placeholders = vec!["?"; names.len()].join(",");
        let c: i64 = self.conn.query_row(
            &format!("SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name IN ({placeholders})"),
            params_from_iter(names.iter()),
            |r| r.get(0),
        )?;
        if c as usize >= names.len() {
            return Ok(());
        }
        self.create_indexes(&names, "crash-recovery")
    }
}

// ---------------------------------------------------------------------------
// Value helpers
// ---------------------------------------------------------------------------

fn text(s: &str) -> Value {
    Value::Text(s.to_string())
}

fn opt_text(s: &Option<String>) -> Value {
    s.as_ref().map_or(Value::Null, |s| Value::Text(s.clone()))
}

fn opt_int(n: Option<u32>) -> Value {
    n.map_or(Value::Null, |n| Value::Integer(n as i64))
}

fn flag(b: Option<bool>) -> Value {
    Value::Integer(i64::from(b.unwrap_or(false)))
}

fn opt_json<T: serde::Serialize>(v: &Option<T>) -> Value {
    v.as_ref().map_or(Value::Null, |v| Value::Text(serde_json::to_string(v).unwrap_or_default()))
}

fn node_row(n: &Node) -> SqlRow {
    vec![
        text(&n.id),
        text(n.kind.as_str()),
        text(&n.name),
        text(if n.qualified_name.is_empty() { &n.name } else { &n.qualified_name }),
        text(&n.file_path),
        text(n.language.as_str()),
        Value::Integer(n.start_line as i64),
        Value::Integer(n.end_line as i64),
        Value::Integer(n.start_column as i64),
        Value::Integer(n.end_column as i64),
        opt_text(&n.docstring),
        opt_text(&n.signature),
        n.visibility.map_or(Value::Null, |v| text(v.as_str())),
        flag(n.is_exported),
        flag(n.is_async),
        flag(n.is_static),
        flag(n.is_abstract),
        opt_json(&n.decorators),
        opt_json(&n.type_parameters),
        opt_text(&n.return_type),
        Value::Integer(n.updated_at),
    ]
}

fn edge_row(e: &Edge) -> SqlRow {
    vec![
        text(&e.source),
        text(&e.target),
        text(e.kind.as_str()),
        opt_json(&e.metadata),
        opt_int(e.line),
        opt_int(e.column),
        e.provenance.map_or(Value::Null, |p| text(p.as_str())),
    ]
}

fn ref_row(r: &UnresolvedReference) -> SqlRow {
    vec![
        text(&r.from_node_id),
        text(&r.reference_name),
        text(r.reference_kind.as_str()),
        Value::Integer(r.line as i64),
        Value::Integer(r.column as i64),
        opt_json(&r.candidates),
        text(r.file_path.as_deref().unwrap_or("")),
        text(r.language.unwrap_or(Language::Unknown).as_str()),
    ]
}

const NODE_INSERT_HEAD: &str = "INSERT OR REPLACE INTO nodes (\
    id, kind, name, qualified_name, file_path, language, \
    start_line, end_line, start_column, end_column, \
    docstring, signature, visibility, \
    is_exported, is_async, is_static, is_abstract, \
    decorators, type_parameters, return_type, updated_at\
    ) VALUES ";
const NODE_TUPLE: &str = "(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)";
const EDGE_INSERT_HEAD: &str = "INSERT OR IGNORE INTO edges (source, target, kind, metadata, line, col, provenance) VALUES ";
const EDGE_TUPLE: &str = "(?,?,?,?,?,?,?)";
const REF_INSERT_HEAD: &str = "INSERT INTO unresolved_refs (from_node_id, reference_name, reference_kind, line, col, candidates, file_path, language) VALUES ";
const REF_TUPLE: &str = "(?,?,?,?,?,?,?,?)";
const SEGMENT_INSERT_HEAD: &str = "INSERT OR IGNORE INTO name_segment_vocab (segment, name) VALUES ";
const SEGMENT_TUPLE: &str = "(?,?)";

fn valid_node(n: &Node) -> bool {
    !n.id.is_empty() && !n.name.is_empty() && !n.file_path.is_empty()
}

fn chunked<'a, T>(items: impl IntoIterator<Item = &'a T>) -> Vec<Vec<T>>
where
    T: 'a + Clone + Eq + std::hash::Hash,
{
    let mut seen = HashSet::new();
    let unique: Vec<T> = items.into_iter().filter(|i| seen.insert((*i).clone())).cloned().collect();
    unique.chunks(SQLITE_PARAM_CHUNK_SIZE).map(<[T]>::to_vec).collect()
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

impl Store {
    /// Run `rows` through a multi-row `INSERT` built as `head + tuple,…`,
    /// decomposed greedily into the cached batch sizes. Preserves row order.
    fn run_batched(&self, kind: &'static str, head: &str, tuple: &str, rows: &[SqlRow]) -> Result<()> {
        let mut i = 0;
        for size in BATCH_SIZES {
            while rows.len() - i >= size {
                let sql = self
                    .batch_sql
                    .borrow_mut()
                    .entry((kind, size))
                    .or_insert_with(|| format!("{head}{}", vec![tuple; size].join(",")))
                    .clone();
                let mut stmt = self.conn.prepare_cached(&sql)?;
                stmt.execute(params_from_iter(rows[i..i + size].iter().flatten()))?;
                i += size;
            }
        }
        Ok(())
    }

    /// Which node kinds contribute their name to the segment vocabulary — the
    /// single gate shared by insert, update and the rebuild page query, so the
    /// write paths can't drift apart. File nodes are excluded (a file's basename
    /// duplicates the symbols inside it) and so are imports (named after module
    /// specifiers, not symbols — an import-only name can never be surfaced).
    fn is_segmentable_kind(kind: NodeKind) -> bool {
        !matches!(kind, NodeKind::File | NodeKind::Import)
    }

    /// Collect (segment, name) rows for a name, honouring the session-dedupe of
    /// the write paths.
    fn collect_name_segment_rows(&self, name: &str, out: &mut Vec<SqlRow>) {
        let mut seen = self.segmented_names.borrow_mut();
        if seen.contains(name) {
            return;
        }
        if seen.len() >= MAX_SEGMENTED_NAMES {
            seen.clear();
        }
        seen.insert(name.to_string());
        for segment in split_identifier_segments(name) {
            out.push(vec![Value::Text(segment), text(name)]);
        }
    }

    /// Write `name`'s segments into name_segment_vocab (idempotent).
    fn insert_name_segments(&self, name: &str) -> Result<()> {
        let mut rows = Vec::new();
        self.collect_name_segment_rows(name, &mut rows);
        self.run_batched("insertNameSegments", SEGMENT_INSERT_HEAD, SEGMENT_TUPLE, &rows)
    }

    /// Insert a new node (replacing one with the same id).
    pub fn insert_node(&self, node: &Node) -> Result<()> {
        if !valid_node(node) {
            eprintln!("[CodeGraphV] Skipping node with missing required fields: {:?}", node.id);
            return Ok(());
        }
        // INSERT OR REPLACE may overwrite a node we have cached; drop the stale entry.
        self.node_cache.borrow_mut().pop(&node.id);
        let mut stmt = self.conn.prepare_cached(&format!("{NODE_INSERT_HEAD}{NODE_TUPLE}"))?;
        stmt.execute(params_from_iter(node_row(node).iter()))?;
        // Segment vocabulary rides the same write path (and transaction) so it can
        // never drift ahead of the nodes it describes.
        if Self::is_segmentable_kind(node.kind) {
            self.insert_name_segments(&node.name)?;
        }
        Ok(())
    }

    /// Insert multiple nodes in a transaction. Same semantics as
    /// [`Store::insert_node`] per row, bound as multi-row INSERTs — the per-call
    /// overhead dominates the store phase on full indexes.
    pub fn insert_nodes(&self, nodes: &[Node]) -> Result<()> {
        self.transaction(|| {
            let mut rows: Vec<SqlRow> = Vec::with_capacity(nodes.len());
            let mut segment_rows: Vec<SqlRow> = Vec::new();
            for node in nodes {
                if !valid_node(node) {
                    eprintln!("[CodeGraphV] Skipping node with missing required fields: {:?}", node.id);
                    continue;
                }
                self.node_cache.borrow_mut().pop(&node.id);
                rows.push(node_row(node));
                if Self::is_segmentable_kind(node.kind) {
                    self.collect_name_segment_rows(&node.name, &mut segment_rows);
                }
            }
            self.run_batched("insertNodes", NODE_INSERT_HEAD, NODE_TUPLE, &rows)?;
            self.run_batched("insertNameSegments", SEGMENT_INSERT_HEAD, SEGMENT_TUPLE, &segment_rows)
        })
    }

    /// Store one file's whole extraction bundle — nodes, edges, unresolved refs,
    /// and the file record — in a SINGLE transaction. The bulk-index path calls
    /// this once per file instead of opening one transaction per table.
    ///
    /// Edges MUST already be endpoint-filtered by the caller (the store path
    /// filters to the file's own inserted node ids), so the per-file existence
    /// SELECT that [`Store::insert_edges`] pays is skipped here.
    pub fn store_file_bundle(&self, bundle: &FileBundle<'_>) -> Result<()> {
        self.transaction(|| {
            self.insert_nodes(bundle.nodes)?;
            if !bundle.edges.is_empty() {
                let rows: Vec<SqlRow> = bundle.edges.iter().map(edge_row).collect();
                self.run_batched("insertEdges", EDGE_INSERT_HEAD, EDGE_TUPLE, &rows)?;
            }
            if !bundle.refs.is_empty() {
                self.insert_unresolved_refs_batch(bundle.refs)?;
            }
            self.upsert_file(bundle.file)
        })
    }

    /// Update an existing node.
    pub fn update_node(&self, node: &Node) -> Result<()> {
        self.node_cache.borrow_mut().pop(&node.id);
        if !valid_node(node) {
            eprintln!("[CodeGraphV] Skipping node update with missing required fields: {:?}", node.id);
            return Ok(());
        }
        let mut stmt = self.conn.prepare_cached(
            "UPDATE nodes SET
               kind = ?2, name = ?3, qualified_name = ?4, file_path = ?5, language = ?6,
               start_line = ?7, end_line = ?8, start_column = ?9, end_column = ?10,
               docstring = ?11, signature = ?12, visibility = ?13,
               is_exported = ?14, is_async = ?15, is_static = ?16, is_abstract = ?17,
               decorators = ?18, type_parameters = ?19, return_type = ?20, updated_at = ?21
             WHERE id = ?1",
        )?;
        stmt.execute(params_from_iter(node_row(node).iter()))?;
        // updateNode is a second real write path to `nodes` — framework
        // post-extract passes rewrite names through it (NestJS route prefixing),
        // and a renamed node's new name must reach the segment vocabulary just
        // like an inserted one's (#1141). insert_name_segments is idempotent.
        if Self::is_segmentable_kind(node.kind) {
            self.insert_name_segments(&node.name)?;
        }
        Ok(())
    }

    pub fn delete_node(&self, id: &str) -> Result<()> {
        self.node_cache.borrow_mut().pop(id);
        self.conn.prepare_cached("DELETE FROM nodes WHERE id = ?")?.execute([id])?;
        Ok(())
    }

    /// Delete all nodes for a file.
    pub fn delete_nodes_by_file(&self, file_path: &str) -> Result<()> {
        {
            let mut cache = self.node_cache.borrow_mut();
            let stale: Vec<String> =
                cache.iter().filter(|(_, n)| n.file_path == file_path).map(|(id, _)| id.clone()).collect();
            for id in stale {
                cache.pop(&id);
            }
        }
        self.conn.prepare_cached("DELETE FROM nodes WHERE file_path = ?")?.execute([file_path])?;
        Ok(())
    }

    /// Insert a new edge (ignored when an identical one exists).
    pub fn insert_edge(&self, edge: &Edge) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(&format!("{EDGE_INSERT_HEAD}{EDGE_TUPLE}"))?;
        stmt.execute(params_from_iter(edge_row(edge).iter()))?;
        Ok(())
    }

    /// Insert multiple edges in a transaction, dropping any whose endpoints are
    /// not stored nodes.
    pub fn insert_edges(&self, edges: &[Edge]) -> Result<()> {
        if edges.is_empty() {
            return Ok(());
        }
        self.transaction(|| {
            let endpoint_ids: Vec<&String> = edges.iter().flat_map(|e| [&e.source, &e.target]).collect();
            let existing = self.existing_node_ids(endpoint_ids)?;
            let rows: Vec<SqlRow> = edges
                .iter()
                .filter(|e| existing.contains(&e.source) && existing.contains(&e.target))
                .map(edge_row)
                .collect();
            self.run_batched("insertEdges", EDGE_INSERT_HEAD, EDGE_TUPLE, &rows)
        })
    }

    /// Delete all edges from a source node.
    pub fn delete_edges_by_source(&self, source_id: &str) -> Result<()> {
        self.conn.prepare_cached("DELETE FROM edges WHERE source = ?")?.execute([source_id])?;
        Ok(())
    }

    fn existing_node_ids<'a>(&self, ids: impl IntoIterator<Item = &'a String>) -> Result<HashSet<String>> {
        let mut out = HashSet::new();
        for chunk in chunked(ids) {
            let sql = format!("SELECT id FROM nodes WHERE id IN ({})", placeholders(chunk.len()));
            let mut stmt = self.conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params_from_iter(chunk.iter()), |r| r.get::<_, String>(0))?;
            for row in rows {
                out.insert(row?);
            }
        }
        Ok(out)
    }

    /// Insert or update a file record. The upsert always REWRITES the
    /// `generated` flag: a file that loses its banner in an edit must lose the
    /// flag on the next sync, not keep a stale 1.
    pub fn upsert_file(&self, file: &FileRecord) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO files (path, content_hash, language, size, modified_at, indexed_at, node_count, errors, generated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(path) DO UPDATE SET
               content_hash = ?2, language = ?3, size = ?4, modified_at = ?5,
               indexed_at = ?6, node_count = ?7, errors = ?8, generated = ?9",
        )?;
        stmt.execute(params![
            file.path,
            file.content_hash,
            file.language.as_str(),
            file.size as i64,
            file.modified_at,
            file.indexed_at,
            file.node_count,
            file.errors.as_ref().map(|e| serde_json::to_string(e).unwrap_or_default()),
            i64::from(file.generated.unwrap_or(false)),
        ])?;
        Ok(())
    }

    /// Delete a file record and its nodes.
    pub fn delete_file(&self, file_path: &str) -> Result<()> {
        self.transaction(|| {
            self.delete_nodes_by_file(file_path)?;
            self.conn.prepare_cached("DELETE FROM files WHERE path = ?")?.execute([file_path])?;
            Ok(())
        })
    }

    pub fn insert_unresolved_ref(&self, r: &UnresolvedReference) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(&format!("{REF_INSERT_HEAD}{REF_TUPLE}"))?;
        stmt.execute(params_from_iter(ref_row(r).iter()))?;
        Ok(())
    }

    /// Insert multiple unresolved references in a transaction.
    pub fn insert_unresolved_refs_batch(&self, refs: &[UnresolvedReference]) -> Result<()> {
        if refs.is_empty() {
            return Ok(());
        }
        self.transaction(|| {
            let rows: Vec<SqlRow> = refs.iter().map(ref_row).collect();
            self.run_batched("insertUnresolvedRefs", REF_INSERT_HEAD, REF_TUPLE, &rows)
        })
    }

    pub fn delete_unresolved_by_node(&self, node_id: &str) -> Result<()> {
        self.conn.prepare_cached("DELETE FROM unresolved_refs WHERE from_node_id = ?")?.execute([node_id])?;
        Ok(())
    }

    // -- Name-segment vocabulary (prompt-hook graph-derived gate) -----------

    /// Wipe the segment vocabulary. A full index of a fresh database calls this
    /// at its start; the node write path repopulates it as files index, so the
    /// end state is exactly the current names with no orphan rows.
    pub fn clear_name_segment_vocab(&self) -> Result<()> {
        self.conn.execute_batch("DELETE FROM name_segment_vocab")?;
        self.segmented_names.borrow_mut().clear();
        Ok(())
    }

    /// Drop the rows of names that no symbol carries any more, deleted or renamed since they were
    /// written (file and import nodes don't count, as on the write path). A full index over an
    /// existing graph calls this once it has stored everything, instead of clearing the table up
    /// front: it never rewrites the names of its unchanged files, so a clear lost those until a
    /// rebuild (#2252). The session's set of names already written goes too, so a dropped name
    /// that comes back later is written again rather than skipped as already seen.
    pub fn prune_name_segment_vocab(&self) -> Result<()> {
        self.conn.execute_batch("DELETE FROM name_segment_vocab WHERE name NOT IN (SELECT name FROM nodes WHERE kind NOT IN ('file', 'import'))")?;
        self.segmented_names.borrow_mut().clear();
        Ok(())
    }

    /// True when the vocab has no rows — an index built before the table existed.
    pub fn is_name_segment_vocab_empty(&self) -> Result<bool> {
        let any: Option<i64> = self
            .conn
            .query_row("SELECT 1 FROM name_segment_vocab LIMIT 1", [], |r| r.get(0))
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })?;
        Ok(any.is_none())
    }

    /// One page of distinct segmentable node names, for batched vocab rebuilds.
    pub fn get_distinct_node_names(&self, limit: usize, offset: usize) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT name FROM nodes WHERE kind NOT IN ('file', 'import') ORDER BY name LIMIT ? OFFSET ?",
        )?;
        let rows = stmt
            .query_map([limit as i64, offset as i64], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Insert segments for a batch of names in one transaction (vocab heal path).
    pub fn insert_name_segments_batch(&self, names: &[String]) -> Result<()> {
        self.transaction(|| {
            let mut rows = Vec::new();
            for name in names {
                self.collect_name_segment_rows(name, &mut rows);
            }
            self.run_batched("insertNameSegments", SEGMENT_INSERT_HEAD, SEGMENT_TUPLE, &rows)
        })
    }

    /// Set the normalized project-name tokens used to down-weight
    /// non-discriminative query words in path scoring (#720).
    pub fn set_project_name_tokens(&self, tokens: HashSet<String>) {
        *self.project_name_tokens.borrow_mut() = tokens;
    }

    pub fn project_name_tokens(&self) -> HashSet<String> {
        self.project_name_tokens.borrow().clone()
    }

    /// Clear all data from the database.
    pub fn clear(&self) -> Result<()> {
        self.node_cache.borrow_mut().clear();
        self.transaction(|| {
            self.conn.execute_batch(
                "DELETE FROM unresolved_refs; DELETE FROM edges; DELETE FROM nodes; DELETE FROM files;",
            )?;
            Ok(())
        })
    }

    /// Clear the node cache.
    pub fn clear_cache(&self) {
        self.node_cache.borrow_mut().clear();
    }
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

impl Store {
    fn query_nodes(&self, sql: &str, params: &[&dyn ToSql]) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map(params, node_from_row)?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn query_edges(&self, sql: &str, params: &[&dyn ToSql]) -> Result<Vec<Edge>> {
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map(params, edge_from_row)?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn exists(&self, sql: &str, params: &[&dyn ToSql]) -> Result<bool> {
        let mut stmt = self.conn.prepare_cached(sql)?;
        Ok(stmt.exists(params)?)
    }

    fn cache_node(&self, node: &Node) {
        self.node_cache.borrow_mut().put(node.id.clone(), node.clone());
    }

    /// Get a node by ID.
    pub fn get_node_by_id(&self, id: &str) -> Result<Option<Node>> {
        if let Some(n) = self.node_cache.borrow_mut().get(id) {
            return Ok(Some(n.clone()));
        }
        let mut found = self.query_nodes(&format!("SELECT {NODE_COLS} FROM nodes WHERE id = ?"), &[&id])?;
        let node = found.pop();
        if let Some(n) = &node {
            self.cache_node(n);
        }
        Ok(node)
    }

    /// Batch lookup: fetch many nodes by ID in a single SQL round-trip.
    ///
    /// Replaces the N+1 pattern in graph traversal where every edge would
    /// trigger its own `get_node_by_id` call. Returns a map keyed by id so
    /// callers can preserve their own ordering; missing IDs are simply absent.
    /// Cache-aware: ids already in the LRU cache are served from memory and the
    /// SQL query only touches the misses.
    pub fn get_nodes_by_ids<S: AsRef<str>>(&self, ids: &[S]) -> Result<HashMap<String, Node>> {
        let mut out = HashMap::new();
        if ids.is_empty() {
            return Ok(out);
        }
        let mut misses: Vec<String> = Vec::new();
        for id in ids {
            let id = id.as_ref();
            match self.node_cache.borrow_mut().get(id) {
                Some(n) => {
                    out.insert(id.to_string(), n.clone());
                }
                None => misses.push(id.to_string()),
            }
        }
        for chunk in chunked(misses.iter()) {
            let sql = format!("SELECT {NODE_COLS} FROM nodes WHERE id IN ({})", placeholders(chunk.len()));
            let refs: Vec<&dyn ToSql> = chunk.iter().map(|s| s as &dyn ToSql).collect();
            for node in self.query_nodes(&sql, &refs)? {
                self.cache_node(&node);
                out.insert(node.id.clone(), node);
            }
        }
        Ok(out)
    }

    /// Whether any node in `file_path` is exported — one indexed probe, without
    /// decoding the file's nodes.
    pub fn file_has_exported_node(&self, file_path: &str) -> Result<bool> {
        self.exists("SELECT 1 FROM nodes WHERE file_path = ? AND is_exported = 1 LIMIT 1", &[&file_path])
    }

    /// The exported nodes of a file, in [`Store::get_nodes_by_file`] order.
    pub fn get_exported_nodes_by_file(&self, file_path: &str) -> Result<Vec<Node>> {
        self.query_nodes(
            &format!("SELECT {NODE_COLS} FROM nodes WHERE file_path = ? AND is_exported = 1 ORDER BY start_line, id"),
            &[&file_path],
        )
    }

    /// The nodes of a file with one name, in [`Store::get_nodes_by_file`] order.
    pub fn get_nodes_by_file_and_name(&self, file_path: &str, name: &str) -> Result<Vec<Node>> {
        self.query_nodes(
            &format!("SELECT {NODE_COLS} FROM nodes WHERE file_path = ? AND name = ? ORDER BY start_line, id"),
            &[&file_path, &name],
        )
    }

    /// Get all nodes in a file.
    pub fn get_nodes_by_file(&self, file_path: &str) -> Result<Vec<Node>> {
        self.query_nodes(
            &format!("SELECT {NODE_COLS} FROM nodes WHERE file_path = ? ORDER BY start_line, id"),
            &[&file_path],
        )
    }

    /// The [`NodeIdentity`] of every node in a file, without decoding whole
    /// nodes. Read before a re-index deletes the file, so each incoming
    /// cross-file edge can follow its target to the node that replaces it (#2276).
    pub fn get_node_identities_by_file(&self, file_path: &str) -> Result<Vec<NodeIdentity>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, kind, name, qualified_name, signature, start_line, start_column FROM nodes WHERE file_path = ?",
        )?;
        let rows = stmt
            .query_map([file_path], |r| {
                let kind: String = r.get(1)?;
                Ok(NodeIdentity {
                    id: r.get(0)?,
                    kind: NodeKind::parse(&kind).ok_or_else(|| {
                        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, kind.clone().into())
                    })?,
                    name: r.get(2)?,
                    qualified_name: r.get(3)?,
                    signature: r.get(4)?,
                    start_line: r.get(5)?,
                    start_column: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Get all nodes in several files at once — one chunked `IN` query rather
    /// than one [`Store::get_nodes_by_file`] per file (#1975).
    pub fn get_nodes_by_files<S: AsRef<str>>(&self, file_paths: &[S]) -> Result<Vec<Node>> {
        let paths: Vec<String> = file_paths.iter().map(|s| s.as_ref().to_string()).collect();
        let mut out = Vec::new();
        for chunk in chunked(paths.iter()) {
            let sql = format!("SELECT {NODE_COLS} FROM nodes WHERE file_path IN ({})", placeholders(chunk.len()));
            let refs: Vec<&dyn ToSql> = chunk.iter().map(|s| s as &dyn ToSql).collect();
            out.extend(self.query_nodes(&sql, &refs)?);
        }
        Ok(out)
    }

    pub fn get_nodes_by_kind(&self, kind: NodeKind) -> Result<Vec<Node>> {
        self.query_nodes(
            &format!("SELECT {NODE_COLS} FROM nodes WHERE kind = ? ORDER BY file_path, start_line, id"),
            &[&kind.as_str()],
        )
    }

    /// Every node of a kind (or kinds) in the canonical `(file_path, start_line,
    /// id)` order, restricted to some languages. Streams through `visit` so a
    /// pass over a symbol-dense project never materializes the whole table
    /// (#610); `visit` returns `false` to stop early.
    pub fn for_each_node_of_kind_in(
        &self,
        kinds: &[NodeKind],
        languages: &[Language],
        mut visit: impl FnMut(Node) -> bool,
    ) -> Result<()> {
        if kinds.is_empty() || languages.is_empty() {
            return Ok(());
        }
        let kind_test = if kinds.len() == 1 {
            "kind = ?".to_string()
        } else {
            format!("kind IN ({})", placeholders(kinds.len()))
        };
        let sql = format!(
            "SELECT {NODE_COLS} FROM nodes WHERE {kind_test} AND language IN ({}) ORDER BY file_path, start_line, id",
            placeholders(languages.len())
        );
        let params: Vec<&str> = kinds.iter().map(|k| k.as_str()).chain(languages.iter().map(|l| l.as_str())).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter(params.iter()))?;
        while let Some(row) = rows.next()? {
            if !visit(node_from_row(row)?) {
                break;
            }
        }
        Ok(())
    }

    /// Every node of one kind in the canonical `(file_path, start_line, id)` order,
    /// streamed through `visit` (`iterateNodesByKind`); `visit` returns `false` to stop early.
    pub fn for_each_node_of_kind(&self, kind: NodeKind, mut visit: impl FnMut(Node) -> bool) -> Result<()> {
        let mut stmt =
            self.conn.prepare("SELECT * FROM nodes WHERE kind = ? ORDER BY file_path, start_line, id")?;
        let mut rows = stmt.query([kind.as_str()])?;
        while let Some(row) = rows.next()? {
            if !visit(node_from_row(row)?) {
                break;
            }
        }
        Ok(())
    }

    /// Every node of a language whose decorator list names `decorator` (a substring pre-filter
    /// over the JSON column — the caller keeps the exact check), in the canonical order, streamed
    /// through `visit` (`iterateNodesByLanguageWithDecorator`).
    pub fn for_each_node_by_language_with_decorator(
        &self,
        language: Language,
        decorator: &str,
        mut visit: impl FnMut(Node) -> bool,
    ) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM nodes WHERE language = ? AND decorators LIKE '%' || ? || '%' ORDER BY file_path, start_line, id",
        )?;
        let mut rows = stmt.query(rusqlite::params![language.as_str(), format!("\"{decorator}\"")])?;
        while let Some(row) = rows.next()? {
            if !visit(node_from_row(row)?) {
                break;
            }
        }
        Ok(())
    }

    pub fn get_all_nodes(&self) -> Result<Vec<Node>> {
        self.query_nodes(&format!("SELECT {NODE_COLS} FROM nodes"), &[])
    }

    /// Distinct languages present in the files table. One indexed aggregate —
    /// lets the dynamic-edge synthesizers skip passes for languages the project
    /// doesn't contain at all (#1212).
    pub fn get_distinct_file_languages(&self) -> Result<HashSet<String>> {
        let mut stmt = self.conn.prepare_cached("SELECT DISTINCT language FROM files")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<std::result::Result<HashSet<_>, _>>()?;
        Ok(rows)
    }

    /// Nodes with an exact name (uses idx_nodes_name).
    ///
    /// This is resolution's candidate list, and the ORDER BY is load-bearing for
    /// index correctness, not cosmetic (CG-33). When a reference names a symbol
    /// that several files define and nothing disambiguates them, resolution binds
    /// to the first candidate — so without an ORDER BY the winner was decided by
    /// rowid, i.e. by the order files happened to be WRITTEN, and a long-lived
    /// synced index drifted away from a rebuild of itself. `(file_path,
    /// start_line)` is a property of the CODE, so both paths pick the same one.
    pub fn get_nodes_by_name(&self, name: &str) -> Result<Vec<Node>> {
        self.query_nodes(
            &format!("SELECT {NODE_COLS} FROM nodes WHERE name = ? ORDER BY file_path, start_line, id"),
            &[&name],
        )
    }

    /// Nodes whose name starts with `prefix`, by index range scan (a LIKE would
    /// skip idx_nodes_name under SQLite's default case-insensitive LIKE).
    pub fn get_nodes_by_name_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<Node>> {
        let upper = format!("{prefix}\u{FFFF}");
        self.query_nodes(
            &format!("SELECT {NODE_COLS} FROM nodes WHERE name >= ? AND name < ? ORDER BY name LIMIT ?"),
            &[&prefix, &upper.as_str(), &(limit as i64)],
        )
    }

    /// File nodes whose basename starts with `prefix`, without a result cap.
    pub fn get_file_nodes_by_name_prefix(&self, prefix: &str) -> Result<Vec<Node>> {
        let upper = format!("{prefix}\u{FFFF}");
        self.query_nodes(
            &format!("SELECT {NODE_COLS} FROM nodes WHERE kind = 'file' AND name >= ? AND name < ? ORDER BY name"),
            &[&prefix, &upper.as_str()],
        )
    }

    /// Nodes by exact qualified name (uses idx_nodes_qualified_name).
    pub fn get_nodes_by_qualified_name_exact(&self, qualified_name: &str) -> Result<Vec<Node>> {
        self.query_nodes(
            &format!("SELECT {NODE_COLS} FROM nodes WHERE qualified_name = ? ORDER BY file_path, start_line, id"),
            &[&qualified_name],
        )
    }

    /// Nodes by name, case-insensitively (seeks the idx_nodes_lower_name
    /// expression index). The parameter is lowered in SQL rather than trusted to
    /// arrive lowered, so the lookup means the same thing whatever casing a
    /// caller hands it.
    pub fn get_nodes_by_lower_name(&self, name: &str) -> Result<Vec<Node>> {
        self.query_nodes(&format!("SELECT {NODE_COLS} FROM nodes WHERE lower(name) = lower(?)"), &[&name])
    }

    /// The name of every node of the given kinds in one language (duplicates kept), as
    /// `getNodeNamesOfKinds` reads them: `+language` lets the kind index narrow further.
    pub fn get_node_names_of_kinds(&self, language: Language, kinds: &[NodeKind]) -> Result<Vec<String>> {
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!("SELECT name FROM nodes WHERE kind IN ({}) AND +language = ?", vec!["?"; kinds.len()].join(", "));
        let params: Vec<String> = kinds.iter().map(|k| k.as_str().to_string()).chain([language.as_str().to_string()]).collect();
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params), |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Does the index hold any file of this language? One seek on idx_files_language.
    pub fn has_files_of_language(&self, language: &str) -> Result<bool> {
        self.exists("SELECT 1 FROM files WHERE language = ? LIMIT 1", &[&language])
    }

    /// COBOL copybook include statements naming `member` — the `import` node
    /// sitting on a `COPY X` / `EXEC SQL INCLUDE X` line. Ordering matches the
    /// TypeScript query: file/line so the include list reads as the project is
    /// laid out. Case-insensitive, because COBOL code is written in capitals
    /// while a query usually is not.
    pub fn get_cobol_includes_by_member(&self, member: &str, limit: u32) -> Result<Vec<Node>> {
        self.query_nodes(
            &format!(
                "SELECT {NODE_COLS} FROM nodes WHERE lower(name) = lower(?) AND kind = 'import' AND language = 'cobol' ORDER BY file_path, start_line, id LIMIT ?"
            ),
            &[&member, &(limit as i64)],
        )
    }

    /// COBOL file nodes whose basename stem is `member`, case-insensitively —
    /// `member.cpy`, `MEMBER.CPY`, `member.cbl`. Every such basename sorts into
    /// [`member.`, `member/`) under lower(), so this is a range seek on
    /// idx_nodes_lower_name.
    pub fn get_cobol_files_by_stem(&self, member: &str) -> Result<Vec<Node>> {
        self.query_nodes(
            &format!(
                "SELECT {NODE_COLS} FROM nodes WHERE lower(name) >= lower(?) || '.' AND lower(name) < lower(?) || '/' AND kind = 'file' AND language = 'cobol' ORDER BY file_path"
            ),
            &[&member, &member],
        )
    }

    // -- Edges ---------------------------------------------------------------

    /// Outgoing edges from a node. Preserves the source/kind index order (calls
    /// before imports/references), then breaks ties deterministically.
    pub fn get_outgoing_edges(&self, source_id: &str, kinds: &[EdgeKind], provenance: Option<&str>) -> Result<Vec<Edge>> {
        if kinds.is_empty() && provenance.is_none() {
            return self.query_edges(
                &format!("SELECT {EDGE_COLS} FROM edges WHERE source = ? ORDER BY kind, target, line, col"),
                &[&source_id],
            );
        }
        let mut sql = format!("SELECT {EDGE_COLS} FROM edges WHERE source = ?");
        let mut params: Vec<&dyn ToSql> = vec![&source_id];
        let kind_strs: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
        if !kinds.is_empty() {
            sql.push_str(&format!(" AND kind IN ({})", placeholders(kinds.len())));
            params.extend(kind_strs.iter().map(|k| k as &dyn ToSql));
        }
        if let Some(p) = provenance.as_ref() {
            sql.push_str(" AND provenance = ?");
            params.push(p);
        }
        sql.push_str(" ORDER BY kind, target, line, col");
        self.query_edges(&sql, &params)
    }

    /// Incoming edges to a node. Kind must precede opaque source IDs: file IDs
    /// sort before function IDs, so source-first ordering lets imports displace
    /// actual calls in capped caller lists.
    pub fn get_incoming_edges(&self, target_id: &str, kinds: &[EdgeKind]) -> Result<Vec<Edge>> {
        if kinds.is_empty() {
            return self.query_edges(
                &format!("SELECT {EDGE_COLS} FROM edges WHERE target = ? ORDER BY kind, source, line, col"),
                &[&target_id],
            );
        }
        let sql = format!(
            "SELECT {EDGE_COLS} FROM edges WHERE target = ? AND kind IN ({}) ORDER BY kind, source, line, col",
            placeholders(kinds.len())
        );
        let kind_strs: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
        let mut params: Vec<&dyn ToSql> = vec![&target_id];
        params.extend(kind_strs.iter().map(|k| k as &dyn ToSql));
        self.query_edges(&sql, &params)
    }

    fn edges_for_many(&self, column: &str, ids: &[String], kinds: &[EdgeKind]) -> Result<Vec<Edge>> {
        let mut out = Vec::new();
        let kind_strs: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
        for chunk in chunked(ids.iter()) {
            let mut sql =
                format!("SELECT {EDGE_COLS} FROM edges WHERE {column} IN ({})", placeholders(chunk.len()));
            let mut params: Vec<&dyn ToSql> = chunk.iter().map(|s| s as &dyn ToSql).collect();
            if !kinds.is_empty() {
                sql.push_str(&format!(" AND kind IN ({})", placeholders(kinds.len())));
                params.extend(kind_strs.iter().map(|k| k as &dyn ToSql));
            }
            out.extend(self.query_edges(&sql, &params)?);
        }
        Ok(out)
    }

    /// Outgoing edges for MANY source nodes in one query — the batch form of
    /// [`Store::get_outgoing_edges`].
    pub fn get_outgoing_edges_from(&self, source_ids: &[String], kinds: &[EdgeKind]) -> Result<Vec<Edge>> {
        self.edges_for_many("source", source_ids, kinds)
    }

    /// Incoming edges for MANY target nodes in one query.
    pub fn get_incoming_edges_to(&self, target_ids: &[String], kinds: &[EdgeKind]) -> Result<Vec<Edge>> {
        self.edges_for_many("target", target_ids, kinds)
    }

    fn count_edges(&self, column: &str, ids: &[String], extra: &str) -> Result<HashMap<String, i64>> {
        let mut out = HashMap::new();
        for chunk in chunked(ids.iter()) {
            let sql = format!(
                "SELECT {column}, COUNT(*) AS count FROM edges WHERE {column} IN ({}){extra} GROUP BY {column}",
                placeholders(chunk.len())
            );
            let mut stmt = self.conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params_from_iter(chunk.iter()), |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            for row in rows {
                let (id, n) = row?;
                out.insert(id, n);
            }
        }
        Ok(out)
    }

    /// Fan-in (total incoming edge count) for MANY nodes in one query. Ids with
    /// no incoming edges are absent from the map rather than present as 0.
    ///
    /// A test's request onto a route (tier-synthesizer's `test-request`) is not a
    /// production caller: forty tests hitting one endpoint must not make it a
    /// hub the Steps walk refuses to enter.
    pub fn count_incoming_edges(&self, ids: &[String]) -> Result<HashMap<String, i64>> {
        self.count_edges(
            "target",
            ids,
            " AND (metadata IS NULL OR metadata NOT LIKE '%\"synthesizedBy\":\"test-request\"%')",
        )
    }

    /// Fan-out (total outgoing edge count) for MANY nodes in one query.
    pub fn count_outgoing_edges(&self, ids: &[String]) -> Result<HashMap<String, i64>> {
        self.count_edges("source", ids, "")
    }

    // -- Files ----------------------------------------------------------------

    /// Which of `file_paths` the index flagged as tool-generated (schema v9+).
    /// Bounded-lookup by design: every consumer already holds a short candidate
    /// list. Returns ONLY the content/index signal; callers union it with the
    /// path-only check so pre-v9 databases keep the path-only behavior.
    pub fn get_generated_paths_among<S: AsRef<str>>(&self, file_paths: &[S]) -> Result<HashSet<String>> {
        let paths: Vec<String> = file_paths.iter().map(|s| s.as_ref().to_string()).collect();
        let mut found = HashSet::new();
        for chunk in chunked(paths.iter()) {
            let sql = format!(
                "SELECT path FROM files WHERE generated = 1 AND path IN ({})",
                placeholders(chunk.len())
            );
            let mut stmt = self.conn.prepare_cached(&sql)?;
            for row in stmt.query_map(params_from_iter(chunk.iter()), |r| r.get::<_, String>(0))? {
                found.insert(row?);
            }
        }
        Ok(found)
    }

    /// Files among `file_paths` that declare nothing but types, originate no call edge, and
    /// that nothing else in the index depends on — ambient declaration shims (CG-28,
    /// `QueryBuilder.getAmbientDeclarationPathsAmong`). Interface-owned members are
    /// transparent to every condition.
    pub fn get_ambient_declaration_paths_among<S: AsRef<str>>(&self, file_paths: &[S]) -> Result<HashSet<String>> {
        const IS_INTERFACE_MEMBER_N: &str = "EXISTS (
  SELECT 1 FROM edges ce JOIN nodes owner ON owner.id = ce.source
   WHERE ce.target = n.id AND ce.kind = 'contains' AND owner.kind = 'interface'
)";
        const IS_INTERFACE_MEMBER_T: &str = "EXISTS (
  SELECT 1 FROM edges ce JOIN nodes owner ON owner.id = ce.source
   WHERE ce.target = t.id AND ce.kind = 'contains' AND owner.kind = 'interface'
)";
        let mut unique: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        for p in file_paths {
            if seen.insert(p.as_ref().to_string()) {
                unique.push(p.as_ref().to_string());
            }
        }
        let mut found = HashSet::new();
        for chunk in unique.chunks(SQLITE_PARAM_CHUNK_SIZE) {
            let sql = format!(
                "SELECT n.file_path AS file_path,
                 SUM(CASE WHEN n.kind NOT IN ('file','import','export','parameter')
                           AND NOT {IS_INTERFACE_MEMBER_N}
                          THEN 1 ELSE 0 END) AS declared,
                 SUM(CASE WHEN n.kind IN ('interface','type_alias','enum','enum_member','namespace')
                          THEN 1 ELSE 0 END) AS typeDeclared
          FROM nodes n
          WHERE n.file_path IN ({})
          GROUP BY n.file_path",
                placeholders(chunk.len())
            );
            let mut stmt = self.conn.prepare_cached(&sql)?;
            let rows = stmt
                .query_map(params_from_iter(chunk.iter()), |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let mut candidates: Vec<String> =
                rows.into_iter().filter(|(_, d, t)| *d > 0 && d == t).map(|(p, _, _)| p).collect();
            let disqualify = |candidates: &mut Vec<String>, template: &str| -> Result<()> {
                if candidates.is_empty() {
                    return Ok(());
                }
                let sql = template.replace("$IN$", &placeholders(candidates.len()));
                let mut stmt = self.conn.prepare(&sql)?;
                let hit = stmt
                    .query_map(params_from_iter(candidates.iter()), |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<HashSet<_>, _>>()?;
                candidates.retain(|p| !hit.contains(p));
                Ok(())
            };
            if candidates.is_empty() {
                continue;
            }
            disqualify(
                &mut candidates,
                &format!(
                    "SELECT DISTINCT n.file_path AS file_path
        FROM edges e JOIN nodes n ON n.id = e.source
        WHERE e.kind IN ('calls','instantiates') AND n.file_path IN ($IN$)
          AND NOT {IS_INTERFACE_MEMBER_N}"
                ),
            )?;
            disqualify(
                &mut candidates,
                &format!(
                    "SELECT DISTINCT t.file_path AS file_path
        FROM edges e JOIN nodes t ON t.id = e.target JOIN nodes s ON s.id = e.source
        WHERE t.file_path IN ($IN$) AND s.file_path <> t.file_path
          AND NOT {IS_INTERFACE_MEMBER_T}"
                ),
            )?;
            found.extend(candidates);
        }
        Ok(found)
    }

    pub fn count_generated_files(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM files WHERE generated = 1", [], |r| r.get(0))?)
    }

    pub fn get_file_by_path(&self, file_path: &str) -> Result<Option<FileRecord>> {
        let mut stmt = self.conn.prepare_cached(&format!("SELECT {FILE_COLS} FROM files WHERE path = ?"))?;
        let mut rows = stmt.query_map([file_path], file_from_row)?;
        Ok(rows.next().transpose()?)
    }

    /// Whether any tracked file lives under the project-relative POSIX directory
    /// `dir`. A range scan on the `path` primary key — `dir/` up to `dir0` ('0'
    /// is the byte after '/') — so it costs one index probe at any repo size.
    pub fn has_files_under(&self, dir: &str) -> Result<bool> {
        self.exists("SELECT 1 FROM files WHERE path >= ? AND path < ? LIMIT 1", &[&format!("{dir}/"), &format!("{dir}0")])
    }

    pub fn get_all_files(&self) -> Result<Vec<FileRecord>> {
        let mut stmt = self.conn.prepare_cached(&format!("SELECT {FILE_COLS} FROM files ORDER BY path"))?;
        let rows = stmt.query_map([], file_from_row)?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Files stored with no nodes or with recorded extraction errors — the only
    /// rows that can be missing their symbols (`CodeGraph.getIndexHealth`).
    pub fn get_files_without_nodes_or_with_errors(&self) -> Result<Vec<FileRecord>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {FILE_COLS} FROM files WHERE node_count = 0 OR errors IS NOT NULL ORDER BY path"
        ))?;
        let rows = stmt.query_map([], file_from_row)?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Most recent index timestamp (ms since epoch) across all tracked files, or
    /// None when nothing is indexed yet (#329).
    pub fn get_last_indexed_at(&self) -> Result<Option<i64>> {
        Ok(self.conn.query_row("SELECT MAX(indexed_at) FROM files", [], |r| r.get(0))?)
    }

    /// The index's revision marker: how far the last sync got, and how many files
    /// it left behind. `MAX(indexed_at)` alone is not enough: a sync that only
    /// DELETES files advances nothing, and the row count catches that case.
    pub fn get_index_revision(&self) -> Result<(Option<i64>, i64)> {
        Ok(self.conn.query_row("SELECT MAX(indexed_at), COUNT(*) FROM files", [], |r| Ok((r.get(0)?, r.get(1)?)))?)
    }

    /// Files re-indexed strictly after `since` (ms since epoch), newest first.
    /// `total` is the real count; `paths` is capped at `limit`.
    pub fn get_files_indexed_since(&self, since: i64, limit: usize) -> Result<(Vec<String>, i64)> {
        let total: i64 = self.conn.query_row("SELECT COUNT(*) FROM files WHERE indexed_at > ?", [since], |r| r.get(0))?;
        let mut stmt = self
            .conn
            .prepare_cached("SELECT path FROM files WHERE indexed_at > ? ORDER BY indexed_at DESC, path LIMIT ?")?;
        let paths = stmt
            .query_map([since, limit as i64], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok((paths, total))
    }

    /// Files whose stored hash differs from `current_hashes`.
    pub fn get_stale_files(&self, current_hashes: &HashMap<String, String>) -> Result<Vec<FileRecord>> {
        Ok(self
            .get_all_files()?
            .into_iter()
            .filter(|f| current_hashes.get(&f.path).is_some_and(|h| !h.is_empty() && *h != f.content_hash))
            .collect())
    }

    // -- Unresolved references ----------------------------------------------

    pub fn get_unresolved_by_name(&self, name: &str) -> Result<Vec<UnresolvedReference>> {
        let mut stmt =
            self.conn.prepare_cached(&format!("SELECT {REF_COLS} FROM unresolved_refs WHERE reference_name = ?"))?;
        let rows = stmt.query_map([name], ref_from_row)?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn get_unresolved_references(&self) -> Result<Vec<UnresolvedReference>> {
        let mut stmt = self.conn.prepare_cached(&format!("SELECT {REF_COLS} FROM unresolved_refs"))?;
        let rows = stmt.query_map([], ref_from_row)?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // -- Stats & metadata ----------------------------------------------------

    pub fn get_node_and_edge_count(&self) -> Result<(i64, i64)> {
        Ok(self.conn.query_row(
            "SELECT (SELECT COUNT(*) FROM nodes) AS nodes, (SELECT COUNT(*) FROM edges) AS edges",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    /// Graph statistics. `db_size_bytes` / `wal_size_bytes` are filled from the
    /// file system (the TypeScript caller set them separately).
    pub fn get_stats(&self) -> Result<GraphStats> {
        let (node_count, edge_count, file_count): (i64, i64, i64) = self.conn.query_row(
            "SELECT (SELECT COUNT(*) FROM nodes), (SELECT COUNT(*) FROM edges), (SELECT COUNT(*) FROM files)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;

        let mut nodes_by_kind = IndexMap::new();
        let mut stmt = self.conn.prepare_cached("SELECT kind, COUNT(*) FROM nodes GROUP BY kind")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
            let (kind, n) = row?;
            if let Some(k) = NodeKind::parse(&kind) {
                nodes_by_kind.insert(k, n as u64);
            }
        }
        let mut edges_by_kind = IndexMap::new();
        let mut stmt = self.conn.prepare_cached("SELECT kind, COUNT(*) FROM edges GROUP BY kind")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
            let (kind, n) = row?;
            if let Some(k) = EdgeKind::parse(&kind) {
                edges_by_kind.insert(k, n as u64);
            }
        }
        let mut files_by_language = IndexMap::new();
        let mut stmt = self.conn.prepare_cached("SELECT language, COUNT(*) FROM files GROUP BY language")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
            let (lang, n) = row?;
            if let Some(l) = Language::parse(&lang) {
                files_by_language.insert(l, n as u64);
            }
        }

        Ok(GraphStats {
            node_count: node_count as u64,
            edge_count: edge_count as u64,
            file_count: file_count as u64,
            nodes_by_kind,
            edges_by_kind,
            files_by_language,
            db_size_bytes: self.size().unwrap_or(0),
            wal_size_bytes: self.wal_size_bytes(),
            last_updated: crate::now_ms(),
        })
    }

    pub fn get_metadata(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare_cached("SELECT value FROM project_metadata WHERE key = ?")?;
        let mut rows = stmt.query_map([key], |r| r.get::<_, String>(0))?;
        Ok(rows.next().transpose()?)
    }

    /// Set a metadata key-value pair (upsert).
    pub fn set_metadata(&self, key: &str, value: &str) -> Result<()> {
        self.conn.prepare_cached(
            "INSERT INTO project_metadata (key, value, updated_at) VALUES (?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )?
        .execute(params![key, value, crate::now_ms()])?;
        Ok(())
    }

    pub fn get_all_metadata(&self) -> Result<std::collections::BTreeMap<String, String>> {
        let mut stmt = self.conn.prepare_cached("SELECT key, value FROM project_metadata")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }
}

/// The database path for a project: `<root>/.codegraph/codegraph.db`.
/// Process-wide SQLite setup, before the first connection opens (SQLite refuses it after).
/// Memory statistics off: they take one global lock on every SQLite allocation, which
/// serializes the parallel resolve workers' reads.
pub fn configure_sqlite() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: sqlite3_config takes (op, int) for SQLITE_CONFIG_MEMSTATUS; before SQLite
        // starts it only sets a flag, and after it returns SQLITE_MISUSE, which changes nothing.
        unsafe {
            rusqlite::ffi::sqlite3_config(rusqlite::ffi::SQLITE_CONFIG_MEMSTATUS, 0 as std::os::raw::c_int);
        }
    });
}

/// A consistent copy of the database at `src` written to `dest` (`VACUUM INTO`), which works
/// while another process has `src` open. `dest` must not exist yet.
pub fn copy_database(src: &Path, dest: &Path) -> Result<()> {
    configure_sqlite();
    let conn = Connection::open_with_flags(src, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    conn.execute("VACUUM INTO ?1", [dest.to_string_lossy()])?;
    Ok(())
}

/// Delete a database file and its WAL sidecars (`-wal`/`-shm`).
///
/// This is how a FULL re-index discards an existing database — rather than
/// opening the old graph and DELETE-ing every row. On a large or poisoned index
/// the per-row `nodes_fts` delete-trigger churn is prohibitive (#1067);
/// unlinking is O(1) regardless of DB size and reclaims the disk the bloated WAL
/// would otherwise keep. The main file's removal must succeed (or report why it
/// couldn't — on Windows a live holder makes the unlink fail); the sidecars are
/// best-effort, since SQLite recreates them on the next open.
pub fn remove_database_files(db_path: &Path) -> Result<()> {
    match std::fs::remove_file(db_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    for suffix in ["-wal", "-shm"] {
        let mut p = db_path.as_os_str().to_owned();
        p.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(p));
    }
    Ok(())
}
