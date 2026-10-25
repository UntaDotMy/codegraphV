//! The `CodeGraph` facade — port of `src/index.ts`.
//!
//! Wires the store, the extraction orchestrator, the reference resolver and the graph layers over
//! one project, and owns the two long-running flows built from them: `index_all` (a full build)
//! and `sync` (an incremental reconcile that converges on what a full rebuild would produce).
//!
//! The TypeScript facade is async because its work crosses worker threads; this port is
//! synchronous; live watching owns a separate engine connection on its indexing thread.
//! The in-process index mutex becomes a re-entrancy guard, and the WAL checkpoint valve — a worker thread that backfilled the WAL
//! off the writer — becomes "defer auto-checkpointing, fold once at the end".

use crate::directory::{create_directory, is_initialized, remove_directory, validate_directory};
use crate::errors::{CodeGraphError, Result};
use crate::extract::has_grammar_load_failure;
use crate::orchestrator::{ChangedFiles, ExtractionOrchestrator, IndexPhase, IndexProgress, IndexResult, ProgressFn, SyncResult};
use crate::utils::FileLock;
use codegraph_db::{database_path, remove_database_files, OpenOptions as DbOpenOptions, Store};
use codegraph_extract::languages::is_file_level_only_language;
use codegraph_extract::scan::config::load_deprioritize_patterns;
use codegraph_extract::scan::ignore::Ignore;
use codegraph_graph::{GraphQueryManager, GraphTraverser};
use codegraph_resolve::callback_synthesizer::{has_synthesis_pattern, is_jsx_child_candidate, synthesize_callback_edges};
use codegraph_resolve::module_importers::{
    reopen_name_matched_imports, reopen_references_through_added_files, reopen_references_through_barrels,
    reopen_references_through_changed_defaults, reopen_references_through_declared_packages, ChangedFile, ImportReader,
};
use codegraph_resolve::resolver::changed_routes;
use codegraph_resolve::ReferenceResolver;
use codegraph_search::query_utils::derive_project_name_tokens;
use codegraph_types::{ExtractionError, FileRecord, IndexHealth, Language, Node, NodeKind, Severity, UnresolvedReference};
use indexmap::IndexSet;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

/// The extraction version this engine stamps into an index it builds (`extraction-version.ts`).
/// Bump when a release changes extraction output enough that existing indexes should be rebuilt.
pub const EXTRACTION_VERSION: u32 = 28;

/// The package version stamped as `indexed_with_version`.
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// `project_metadata` key for the Python package-root digest (see `sync`).
const PYTHON_ROOTS_METADATA_KEY: &str = "python_root_fingerprint";
/// Metadata key: the digest of the workspace packages and path dependencies the index was
/// resolved by (see `sync`).
const WORKSPACE_METADATA_KEY: &str = "js_workspace_fingerprint";
/// The ES module family whose references a moved workspace map re-opens (ESM_IMPORT_LANGUAGES).
const ESM_IMPORT_LANGUAGES: [Language; 8] =
    [Language::TypeScript, Language::Tsx, Language::JavaScript, Language::Jsx, Language::ArkTs, Language::Svelte, Language::Vue, Language::Astro];

/// How long a recreated, not-yet-indexed database is treated as a rebuild in progress.
const RECREATE_GRACE_MS: u128 = 120_000;

/// A single word ("state") can match hundreds of names in a big repo — that is noise. Ceiling for
/// the single-word tier of segment matching; co-occurrence is exempt.
pub const SEGMENT_RARITY_CEILING: i64 = 25;

/// Options for opening an existing CodeGraph project.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenOptions {
    /// Run a sync before returning.
    pub sync: bool,
    /// Open the database read-only.
    pub read_only: bool,
}

/// Options for `index_all` / `sync`.
#[derive(Debug, Clone, Default)]
pub struct IndexOptions {
    /// Enable verbose logging (worker lifecycle, memory, timeouts).
    pub verbose: bool,
    /// Watcher fast path: reconcile ONLY these project-relative paths.
    pub paths: Option<Vec<String>>,
}

/// Completeness of the last full index run (`index_state` metadata).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexState {
    Indexing,
    Complete,
    Partial,
    Failed,
}

impl IndexState {
    pub fn as_str(self) -> &'static str {
        match self {
            IndexState::Indexing => "indexing",
            IndexState::Complete => "complete",
            IndexState::Partial => "partial",
            IndexState::Failed => "failed",
        }
    }
}

/// Which engine built the current index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexBuildInfo {
    pub version: Option<String>,
    pub extraction_version: Option<u32>,
}

fn lock_unavailable(message: String) -> CodeGraphError {
    CodeGraphError::LockUnavailable(message)
}

fn emit(progress: &mut Option<ProgressFn<'_>>, phase: IndexPhase, current: usize, total: usize) {
    if let Some(cb) = progress {
        cb(&IndexProgress::new(phase, current, total));
    }
}

fn reborrow<'a, 'b>(progress: &'a mut Option<ProgressFn<'b>>) -> Option<ProgressFn<'a>> {
    progress.as_mut().map(|cb| &mut **cb as ProgressFn<'a>)
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).map(|v| v == "1").unwrap_or(false)
}

fn timings() -> bool {
    std::env::var("CODEGRAPH_SYNTH_TIMINGS").map(|v| !v.is_empty()).unwrap_or(false)
}

fn is_python_init(path: &str) -> bool {
    path == "__init__.py" || path.ends_with("/__init__.py") || path.ends_with("\\__init__.py")
}

fn is_python_file(path: &str) -> bool {
    path.ends_with(".py") || path.ends_with(".pyi")
}

/// Releases the re-entrancy flag on drop.
struct IndexingGuard<'a>(&'a Cell<bool>);

impl Drop for IndexingGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// Main CodeGraph type: the primary interface for interacting with the code knowledge graph.
pub struct CodeGraph {
    pub(crate) watcher:Option<codegraph_sync::watcher::FileWatcher>,
    pub(crate) watch_readers:std::cell::OnceCell<crate::watch::WatchReadView>,
    pub(crate) last_watch_revision:Cell<u64>,
    project_root: String,
    store: Rc<Store>,
    orchestrator: ExtractionOrchestrator,
    /// Built on first use: detecting frameworks costs ~80 ms on a large index, and a read-only
    /// query never resolves anything.
    resolver: std::cell::OnceCell<ReferenceResolver>,
    /// Cross-process write lock (CLI, MCP, git hooks).
    file_lock: RefCell<FileLock>,
    /// True while an index/sync is on the stack — the port of the in-process index mutex.
    indexing: Cell<bool>,
    /// Set when this instance followed a database replaced on disk (#1902): the next sync that
    /// can run reconciles the whole tree.
    pending_full_reconcile: Cell<bool>,
}

impl CodeGraph {
    fn from_store(store: Rc<Store>, project_root: String) -> CodeGraph {
        let file_lock = FileLock::new(Path::new(&crate::directory::get_code_graph_dir(&project_root)).join("codegraph.lock"));
        let orchestrator = Self::wire_layers(&store, &project_root);
        CodeGraph {
            watcher:None,watch_readers:std::cell::OnceCell::new(),last_watch_revision:Cell::new(0),
            project_root,
            store,
            orchestrator,
            resolver: std::cell::OnceCell::new(),
            file_lock: RefCell::new(file_lock),
            indexing: Cell::new(false),
            pending_full_reconcile: Cell::new(false),
        }
    }

    /// (Re)build the extraction layer over `store`. Factored out of the constructor so
    /// `reopen_replaced_database` can rebuild it against a fresh connection. The resolver is built
    /// on first use (see [`CodeGraph::resolver`]).
    fn wire_layers(store: &Rc<Store>, project_root: &str) -> ExtractionOrchestrator {
        // Down-weight the project name as a query term in search ranking — it names the whole
        // repo, not a symbol, so it has no discriminative value (#720).
        store.set_project_name_tokens(derive_project_name_tokens(Path::new(project_root)));
        ExtractionOrchestrator::new(project_root, store.clone())
    }

    // ===========================================================================
    // Lifecycle
    // ===========================================================================

    fn resolve_root(project_root: &str) -> String {
        let p = Path::new(project_root);
        let abs = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(p) };
        // `path.resolve` normalizes `.` and `..` lexically without touching the filesystem.
        let mut out = std::path::PathBuf::new();
        for comp in abs.components() {
            match comp {
                std::path::Component::ParentDir => {
                    out.pop();
                }
                std::path::Component::CurDir => {}
                other => out.push(other.as_os_str()),
            }
        }
        out.to_string_lossy().into_owned()
    }

    /// Initialize a new CodeGraph project: creates the `.codegraph` directory and database.
    pub fn init(project_root: &str) -> Result<CodeGraph> {
        let root = Self::resolve_root(project_root);
        if is_initialized(&root) {
            return Err(CodeGraphError::other(format!("CodeGraph already initialized in {root}")));
        }
        create_directory(&root).map_err(CodeGraphError::other)?;
        let store = Store::initialize(database_path(Path::new(&root)))?;
        Ok(Self::from_store(Rc::new(store), root))
    }

    /// Initialize, then index every file, reporting progress.
    pub fn init_and_index(project_root: &str, on_progress: Option<ProgressFn<'_>>) -> Result<CodeGraph> {
        let mut cg = Self::init(project_root)?;
        cg.index_all(&IndexOptions::default(), on_progress, None)?;
        Ok(cg)
    }

    /// Open an existing CodeGraph project.
    pub fn open(project_root: &str, options: OpenOptions) -> Result<CodeGraph> {
        let root = Self::resolve_root(project_root);
        if !is_initialized(&root) {
            return Err(CodeGraphError::other(format!("CodeGraph not initialized in {root}. Run init() first.")));
        }
        let validation = validate_directory(&root);
        if !validation.valid {
            return Err(CodeGraphError::other(format!("Invalid CodeGraph directory: {}", validation.errors.join(", "))));
        }
        let store = Store::open(database_path(Path::new(&root)), DbOpenOptions { read_only: options.read_only, ..Default::default() })?;
        let mut instance = Self::from_store(Rc::new(store), root);
        if options.sync && !options.read_only {
            instance.sync(&IndexOptions::default(), None)?;
        }
        Ok(instance)
    }

    /// Rebuild the project's database from scratch and return a fresh, empty instance — the "same
    /// result as a fresh init" semantics `codegraph index` documents. Discards the existing
    /// database (and its `-wal`/`-shm` sidecars) rather than deleting every row (#1067).
    pub fn recreate(project_root: &str) -> Result<CodeGraph> {
        let root = Self::resolve_root(project_root);
        if !is_initialized(&root) {
            return Err(CodeGraphError::other(format!("CodeGraph not initialized in {root}. Run init() first.")));
        }
        let db_path = database_path(Path::new(&root));
        if let Err(err) = remove_database_files(&db_path) {
            return Err(CodeGraphError::other(format!(
                "Could not rebuild the index — the database file is in use ({err}). Stop any running CodeGraph MCP server/daemon for this project and retry, or remove the {} directory and run \"codegraph init\".",
                crate::directory::get_code_graph_dir(&root)
            )));
        }
        let store = Store::initialize(&db_path)?;
        Ok(Self::from_store(Rc::new(store), root))
    }

    /// Check if a directory has been initialized as a CodeGraph project.
    pub fn is_initialized(project_root: &str) -> bool {
        is_initialized(&Self::resolve_root(project_root))
    }

    /// Close the CodeGraph instance and release resources.
    pub fn close(mut self) {
        self.unwatch();
        self.file_lock.borrow_mut().release();
        drop(self.orchestrator);
        drop(self.resolver);
        if let Ok(store) = Rc::try_unwrap(self.store) {
            let _ = store.close();
        }
    }

    /// The project root directory.
    pub fn project_root(&self) -> &str {
        &self.project_root
    }

    /// The underlying store, for the query surfaces (CLI, MCP, viewer) that read it directly.
    pub fn store(&self) -> &Rc<Store> {
        let store=if self.watcher.is_some(){self.watch_readers.get_or_init(||crate::watch::WatchReadView::new(self.store.clone())).current()}else{&self.store};
        let revision=self.watcher.as_ref().map(|w|w.revision()).unwrap_or(0);
        if self.last_watch_revision.replace(revision)!=revision{store.clear_cache();}
        store
    }

    pub fn resolver(&self) -> &ReferenceResolver {
        self.resolver.get_or_init(|| ReferenceResolver::create(&self.project_root, self.store.clone()))
    }

    pub fn orchestrator(&self) -> &ExtractionOrchestrator {
        &self.orchestrator
    }

    pub fn graph_manager(&self) -> GraphQueryManager<'_> {
        GraphQueryManager::new(self.store())
    }

    pub fn traverser(&self) -> GraphTraverser<'_> {
        GraphTraverser::new(self.store())
    }

    /// A `(path) => boolean` test for the paths the project named in `codegraph.json`
    /// `deprioritize` — indexed and findable, but never outranking real code (#982). Read lazily
    /// (the config loader is mtime-cached) so an edit to `codegraph.json` applies to a long-lived
    /// instance; ranking-only, so a bad pattern costs relevance, never recall.
    pub fn deprioritized_path_matcher(&self) -> impl Fn(&str) -> bool + '_ {
        let cached: RefCell<Option<(Vec<String>, Ignore)>> = RefCell::new(None);
        move |file_path: &str| -> bool {
            let patterns = load_deprioritize_patterns(Path::new(&self.project_root));
            if patterns.is_empty() {
                return false;
            }
            let mut slot = cached.borrow_mut();
            if slot.as_ref().map(|(p, _)| p != &patterns).unwrap_or(true) {
                let mut ig = Ignore::new();
                ig.add_patterns(&patterns);
                *slot = Some((patterns, ig));
            }
            let rel = if Path::new(file_path).is_absolute() {
                match Path::new(file_path).strip_prefix(&self.project_root) {
                    Ok(r) => r.to_string_lossy().replace('\\', "/"),
                    Err(_) => return false,
                }
            } else {
                file_path.replace('\\', "/")
            };
            if rel.is_empty() || rel.starts_with("..") {
                return false;
            }
            slot.as_ref().map(|(_, ig)| ig.ignores(&rel).unwrap_or(false)).unwrap_or(false)
        }
    }

    // ===========================================================================
    // Database replaced on disk (#925, #1902)
    // ===========================================================================

    /// The database file at the path was written within the recreate grace window.
    fn is_freshly_recreated(&self) -> bool {
        std::fs::metadata(self.store.path())
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|age| age.as_millis() < RECREATE_GRACE_MS)
            .unwrap_or(false)
    }

    /// Heal a stale database handle in place. If `.codegraph/` was removed and recreated at the
    /// SAME path while this instance held the DB open, our open handle points at the unlinked
    /// inode and can never see the new index (#925). When detected, open the live file at the same
    /// path, rebuild the layers and swap them in. Returns true iff it reopened. Refuses (false)
    /// while an index/sync is running.
    pub fn reopen_if_replaced(&mut self) -> bool {
        if self.indexing.get() {
            return false;
        }
        let followed=self.watch_readers.get().is_some_and(|r|r.followed());
        let reopened=self.reopen_replaced_database().unwrap_or(false);
        reopened&&!followed
    }

    fn reopen_replaced_database(&mut self) -> Result<bool> {
        if !self.store.is_replaced_on_disk() {
            return Ok(false);
        }
        // Open the live file FIRST — if that throws (e.g. mid-recreate), the old handle stays in
        // place and the caller retries on the next query.
        let fresh = Rc::new(Store::open(self.store.path(), DbOpenOptions { read_only: self.store.is_read_only(), ..Default::default() })?);
        self.orchestrator = Self::wire_layers(&fresh, &self.project_root);
        self.store = fresh;
        self.resolver = std::cell::OnceCell::new();
        self.watch_readers.take();
        // Whoever reopened — a sync, or a tool call's self-heal — the next sync must reconcile the
        // whole tree (#1902).
        self.pending_full_reconcile.set(true);
        Ok(true)
    }

    // ===========================================================================
    // Indexing
    // ===========================================================================

    fn failed_index_result(message: &str) -> IndexResult {
        IndexResult {
            success: false,
            errors: vec![ExtractionError { message: message.to_string(), file_path: None, line: None, column: None, severity: Severity::Error, code: None }],
            ..Default::default()
        }
    }

    fn begin_indexing(&self) -> Result<IndexingGuard<'_>> {
        if self.indexing.replace(true) {
            return Err(CodeGraphError::other("An index or sync is already running on this CodeGraph instance"));
        }
        Ok(IndexingGuard(&self.indexing))
    }

    /// Check if an indexing operation is currently in progress.
    pub fn is_indexing(&self) -> bool {
        self.indexing.get()||self.watcher.as_ref().is_some_and(|w|w.is_syncing())
    }

    /// Index all files in the project.
    pub fn index_all(&mut self, options: &IndexOptions, mut on_progress: Option<ProgressFn<'_>>, signal: Option<&AtomicBool>) -> Result<IndexResult> {
        let _guard = self.begin_indexing()?;
        let started = Instant::now();
        if self.file_lock.borrow_mut().acquire().is_err() {
            return Ok(Self::failed_index_result("Could not acquire file lock - another process may be indexing"));
        }

        // Fast-init: on a COMPLETELY fresh DB, trade crash-durability for speed during the bulk
        // build (journal in memory, no fsync). Safe because the DB is disposable until the index
        // completes — index_state stays 'indexing' and a crashed init is re-run from scratch.
        // Kill switch: CODEGRAPH_NO_FAST_INIT=1.
        let (nodes_before, edges_before) = self.store.get_node_and_edge_count()?;
        let fresh_db = nodes_before == 0;
        let fast_init = !env_flag("CODEGRAPH_NO_FAST_INIT") && fresh_db;
        if fast_init {
            let _ = self.store.conn().execute_batch("PRAGMA journal_mode = MEMORY; PRAGMA synchronous = OFF;");
        }
        // Defer WAL auto-checkpointing for the whole bulk run (#1231); runMaintenance does the
        // final fold-up before the interval is restored. Kill switch: CODEGRAPH_NO_WAL_DEFER=1.
        let defer_wal = !fast_init && !env_flag("CODEGRAPH_NO_WAL_DEFER") && self.store.journal_mode().map(|m| m == "wal").unwrap_or(false);
        let mut prior_autocheckpoint = 1000i64;
        if defer_wal {
            prior_autocheckpoint = self.store.wal_autocheckpoint().unwrap_or(1000);
            let _ = self.store.set_wal_autocheckpoint(0);
        }

        let outcome = self.index_all_inner(options, &mut on_progress, signal, fresh_db, fast_init, (nodes_before, edges_before), started);

        if defer_wal {
            let _ = self.store.set_wal_autocheckpoint(prior_autocheckpoint.max(0) as u32);
        }
        if fast_init {
            // Back to the durable defaults; journal_mode=WAL folds the MEMORY journal state into a
            // normal WAL-mode database file.
            let _ = self.store.conn().execute_batch("PRAGMA synchronous = NORMAL; PRAGMA journal_mode = WAL;");
        }
        self.file_lock.borrow_mut().release();
        outcome
    }

    #[allow(clippy::too_many_arguments)]
    fn index_all_inner(
        &self,
        options: &IndexOptions,
        on_progress: &mut Option<ProgressFn<'_>>,
        signal: Option<&AtomicBool>,
        fresh_db: bool,
        fast_init: bool,
        before: (i64, i64),
        started: Instant,
    ) -> Result<IndexResult> {
        let _ = fast_init;
        // Detect frameworks from the index as it stands before this run writes (as at open).
        self.resolver();
        let git_state = self.orchestrator.begin_git_index_state(true)?;
        // Mark the index as in-flight BEFORE any writes: a run killed mid-index leaves this marker
        // behind, so `codegraph status` can tell a truncated index from a completed one.
        let _ = self.store.set_metadata("index_state", "indexing");
        // The segment vocabulary starts empty and is repopulated by the node write path as every
        // file (re-)indexes below — so a full index is also the orphan-cleanup pass.
        let _ = self.store.clear_name_segment_vocab();
        // Bulk FTS mode for the mass-insert phase: drop the per-row FTS sync triggers, rebuild
        // nodes_fts once from the nodes table afterwards.
        self.store.begin_bulk_node_load()?;
        if fresh_db {
            self.store.begin_bulk_parse_load()?;
        }
        let indexed = self.orchestrator.index_all(reborrow(on_progress), signal, options.verbose);
        if fresh_db {
            let t = Instant::now();
            self.store.end_bulk_parse_load()?;
            if timings() {
                eprintln!("[phase-timing] parse-index-rebuild: {}ms", t.elapsed().as_millis());
            }
        }
        let t = Instant::now();
        self.store.end_bulk_node_load()?;
        if timings() {
            eprintln!("[phase-timing] fts-rebuild: {}ms", t.elapsed().as_millis());
        }
        let mut result = indexed?;

        // Re-detect frameworks now that the index is populated: framework resolvers whose
        // detect() consults the indexed file list return false on the initial pass.
        if result.success && result.files_indexed > 0 {
            let t = Instant::now();
            self.resolver().initialize();
            // Cross-file finalization (e.g. NestJS RouterModule prefixes). Runs before resolution
            // so updated names show up in subsequent reads.
            self.resolver().run_post_extract();
            if timings() {
                eprintln!("[phase-timing] resolver-reinit: {}ms", t.elapsed().as_millis());
            }

            let unresolved_count = self.store.get_unresolved_references_count()? as usize;
            emit(on_progress, IndexPhase::Resolving, 0, unresolved_count);
            let t = Instant::now();
            self.resolve_references_batched(on_progress, true)?;
            if timings() {
                eprintln!("[phase-timing] resolution: {}ms", t.elapsed().as_millis());
            }
            // Second pass: chained calls whose method lives on a supertype the receiver conforms to
            // (needs the implements/extends edges the main pass just built, #750), and `this.<member>`
            // callback registrations whose member is inherited (#808).
            self.resolver().resolve_chained_calls_via_conformance()?;
            self.resolver().resolve_deferred_this_member_refs()?;

            // Refresh planner stats + checkpoint the WAL after bulk writes.
            let t = Instant::now();
            self.store.run_maintenance();
            if timings() {
                eprintln!("[phase-timing] maintenance: {}ms", t.elapsed().as_millis());
            }

            // The orchestrator only sees extraction-phase counts; resolution and synthesizer edges
            // come later. Recompute against the DB so the summary reports the true totals.
            let (nodes_after, edges_after) = self.store.get_node_and_edge_count()?;
            result.nodes_created = (nodes_after - before.0).max(0) as usize;
            result.edges_created = (edges_after - before.1).max(0) as usize;
        }

        if result.success {
            self.stamp_python_root_fingerprint(None);
            self.stamp_workspace_fingerprint(None);
        }
        if result.success && result.files_indexed > 0 {
            let _ = self.store.set_metadata("indexed_with_version", PACKAGE_VERSION);
            let _ = self.store.set_metadata("indexed_with_extraction_version", &EXTRACTION_VERSION.to_string());
        }
        if result.success
            && result.files_errored == 0
            && result.files_discovered.is_none_or(|d| result.files_indexed + result.files_skipped >= d)
        {
            self.orchestrator.finish_git_index_state(&git_state, true, &[])?;
        }

        // Reconcile the scan's ground truth against what the pipeline accounted for. A shortfall
        // means files were silently dropped — record it and tell the user, don't let the index
        // pass as complete.
        if !result.success {
            let _ = self.store.set_metadata("index_state", "failed");
        } else {
            let accounted = result.files_indexed + result.files_skipped + result.files_errored;
            let shortfall = result.files_discovered.map(|d| d as i64 - accounted as i64).unwrap_or(0);
            if let (Some(discovered), true) = (result.files_discovered, shortfall > 0) {
                let _ = self.store.set_metadata("index_state", "partial");
                let _ = self.store.set_metadata("index_files_discovered", &discovered.to_string());
                let _ = self.store.set_metadata("index_files_accounted", &accounted.to_string());
                result.errors.push(ExtractionError {
                    message: format!(
                        "Index is missing {shortfall} of {discovered} discovered files (indexed {}, skipped {}, errored {}). The index is PARTIAL — re-run `codegraph index`.",
                        result.files_indexed, result.files_skipped, result.files_errored
                    ),
                    file_path: None,
                    line: None,
                    column: None,
                    severity: Severity::Warning,
                    code: Some("index_partial".to_string()),
                });
            } else {
                let _ = self.store.set_metadata("index_state", "complete");
                if let Some(discovered) = result.files_discovered {
                    let _ = self.store.set_metadata("index_files_discovered", &discovered.to_string());
                    let _ = self.store.set_metadata("index_files_accounted", &accounted.to_string());
                }
            }
        }

        // The time covers the whole run, like the totals above (#2334).
        result.duration_ms = started.elapsed().as_millis() as u64;
        Ok(result)
    }

    /// Index specific files.
    pub fn index_files(&self, file_paths: &[String]) -> Result<IndexResult> {
        let _guard = self.begin_indexing()?;
        if self.file_lock.borrow_mut().acquire().is_err() {
            return Ok(Self::failed_index_result("Could not acquire file lock - another process may be indexing"));
        }
        let out = self.orchestrator.index_files(file_paths);
        // The files' nodes are new to the resolver: a resolution after this reloads its caches,
        // the set of node names included, as index_all and sync do before they resolve (#2829).
        // A resolver not built yet has none to drop.
        if let Some(resolver) = self.resolver.get() {
            resolver.clear_caches();
        }
        self.file_lock.borrow_mut().release();
        out
    }

    /// Indexed Python file paths — the before/after sets a sync diffs.
    fn python_file_paths(&self) -> Result<HashSet<String>> {
        Ok(self.store.get_all_file_paths()?.into_iter().filter(|f| f.ends_with(".py")).collect())
    }

    /// Record the Python package-root digest a later sync compares against.
    fn stamp_python_root_fingerprint(&self, fingerprint: Option<String>) {
        let fingerprint = fingerprint.unwrap_or_else(|| self.resolver().get_python_root_fingerprint());
        let _ = self.store.set_metadata(PYTHON_ROOTS_METADATA_KEY, &fingerprint);
    }

    /// Record the workspace-package digest a later sync compares against.
    fn stamp_workspace_fingerprint(&self, fingerprint: Option<String>) {
        let fingerprint = fingerprint.unwrap_or_else(|| self.resolver().get_workspace_fingerprint());
        let _ = self.store.set_metadata(WORKSPACE_METADATA_KEY, &fingerprint);
    }

    /// Resolve every pending reference in batches (memory-bounded), then synthesize.
    fn resolve_references_batched(&self, on_progress: &mut Option<ProgressFn<'_>>, synthesize: bool) -> Result<codegraph_resolve::types::ResolutionStats> {
        let progress = RefCell::new(reborrow(on_progress));
        let report = |current: usize, total: usize| {
            if let Some(cb) = progress.borrow_mut().as_mut() {
                cb(&IndexProgress::new(IndexPhase::Resolving, current, total));
            }
        };
        let total = self.store.get_unresolved_references_count()? as usize;
        // On big runs, drop the non-unique edge/ref indexes for the batch loop and recreate them
        // before synthesis (which reads kind-keyed).
        let bulk = total >= min_refs_for_bulk();
        if bulk {
            self.store.begin_bulk_edge_load()?;
            self.store.begin_bulk_ref_load()?;
        }
        let t = Instant::now();
        let stats = self.resolver().resolve_and_persist_batched(Some(&report), 5000, false);
        if timings() {
            eprintln!("[phase-timing]   resolve-batches: {}ms ({total} refs)", t.elapsed().as_millis());
        }
        let t = Instant::now();
        if bulk {
            self.store.end_bulk_ref_load()?;
            self.store.end_bulk_edge_load(true)?;
            self.store.create_synthesis_site_index()?;
        }
        if timings() {
            eprintln!("[phase-timing]   bulk-index-rebuild: {}ms", t.elapsed().as_millis());
        }
        let mut stats = stats?;
        if synthesize {
            let t = Instant::now();
            let count = synthesize_callback_edges(&self.store, self.resolver().context()).unwrap_or(0);
            stats.by_method.insert("callback-synthesis".to_string(), count);
            if timings() {
                eprintln!("[phase-timing]   synthesis: {}ms", t.elapsed().as_millis());
            }
        }
        Ok(stats)
    }

    /// Sync with the current file state (incremental update). Errors with
    /// [`CodeGraphError::LockUnavailable`] if the cross-process write lock is unavailable.
    pub fn sync(&mut self, options: &IndexOptions, mut on_progress: Option<ProgressFn<'_>>) -> Result<SyncResult> {
        if self.indexing.get() {
            return Err(CodeGraphError::other("An index or sync is already running on this CodeGraph instance"));
        }
        let started = Instant::now();
        if let Err(err) = self.file_lock.borrow_mut().acquire() {
            return Err(lock_unavailable(format!("Sync could not acquire the file lock; retry when the index is available. {err}")));
        }
        // A full rebuild in another process unlinks the database and creates a new file at the same
        // path. Follow the path before writing, and widen a scoped sync to a full one (#1902). If the
        // reopen fails (the rebuild is mid-way), report lock contention so the watcher retries.
        if let Err(err) = self.reopen_replaced_database() {
            self.file_lock.borrow_mut().release();
            return Err(lock_unavailable(format!("Sync could not open the rebuilt index yet; retry when the rebuild finishes. {err}")));
        }
        let mut options = options.clone();
        if self.pending_full_reconcile.get() {
            // `codegraph index` recreates the file, THEN takes the write lock in index_all. A sync
            // landing in that gap would run a full reconcile of the empty file and hold the lock the
            // rebuild is about to ask for: step aside and reconcile in full once it is done.
            if self.get_index_state().is_none() && self.is_freshly_recreated() {
                self.file_lock.borrow_mut().release();
                return Err(lock_unavailable("A rebuild of this index is in progress; retry when it finishes.".to_string()));
            }
            options.paths = None;
        }
        self.indexing.set(true);
        let outcome = self.sync_inner(&options, &mut on_progress, started);
        self.indexing.set(false);
        self.file_lock.borrow_mut().release();
        outcome
    }

    fn sync_inner(&self, options: &IndexOptions, on_progress: &mut Option<ProgressFn<'_>>, started: Instant) -> Result<SyncResult> {
        // Detect frameworks from the index as it stands before this sync writes (as at open).
        self.resolver();
        // Defer WAL auto-checkpointing for the whole incremental run, exactly as index_all does for
        // the bulk path (#1231); runMaintenance at the end folds the WAL.
        let defer_wal = !env_flag("CODEGRAPH_NO_WAL_DEFER") && self.store.journal_mode().map(|m| m == "wal").unwrap_or(false);
        let mut prior_autocheckpoint = 1000i64;
        if defer_wal {
            prior_autocheckpoint = self.store.wal_autocheckpoint().unwrap_or(1000);
            let _ = self.store.set_wal_autocheckpoint(0);
        }
        let outcome = self.sync_body(options, on_progress, started);
        if defer_wal {
            let _ = self.store.set_wal_autocheckpoint(prior_autocheckpoint.max(0) as u32);
        }
        outcome
    }

    fn sync_body(&self, options: &IndexOptions, on_progress: &mut Option<ProgressFn<'_>>, started: Instant) -> Result<SyncResult> {
        let store = &self.store;
        // Captured BEFORE the sync runs: the sync's own incremental writes populate vocab rows for
        // the files it touches, so an end-of-sync emptiness check would see "non-empty" and skip the
        // backfill forever.
        let vocab_was_empty = store.is_name_segment_vocab_empty().unwrap_or(false);

        let scoped: Option<&[String]> = options.paths.as_deref().filter(|p| !p.is_empty());
        let full_reconcile = scoped.is_none();
        let git_state = self.orchestrator.begin_git_index_state(full_reconcile)?;

        let python_files_before = self.python_file_paths()?;
        // An interrupted index may have absorbed its changed files before resolution/synthesis.
        // Detect those orphans BEFORE this sync adds refs.
        let mut refresh_synthesis = store.get_metadata("synthesis_pending")?.as_deref() == Some("1") || store.get_unresolved_references_count()? > 0;
        if refresh_synthesis {
            store.set_metadata("synthesis_pending", "1")?;
        }
        // The route nodes as they were before this sync replaced or removed a file — read at the
        // first change, so a sync that changes nothing reads nothing, and only where a router binds
        // navigation calls to routes.
        let watch_routes = self.resolver().has_navigation_routers();
        let mut routes_before: Option<Vec<Node>> = None;
        let mut result = {
            let mut on_file_change = |file_path: &str, content: Option<&str>| {
                if watch_routes && routes_before.is_none() {
                    routes_before = store.get_nodes_by_kind(NodeKind::Route).ok();
                }
                if !refresh_synthesis
                    && (store.has_synthesized_edges_touching_file(file_path).unwrap_or(false)
                        || store.was_synthesis_input(file_path).unwrap_or(false)
                        || content.is_some_and(|c| has_synthesis_pattern(file_path, c)))
                {
                    refresh_synthesis = true;
                    let _ = store.set_metadata("synthesis_pending", "1");
                }
            };
            self.orchestrator.sync(reborrow(on_progress), scoped, None, Some(&mut on_file_change))?
        };

        // A component a tag in an unchanged file may render: `<Team />` links to the `Team` this
        // sync adds. Removing one needs no check: a component a tag rendered had synthesized edges
        // into its file.
        if !refresh_synthesis {
            if let (Some(delta), Some(changed)) = (&result.definition_delta, &result.changed_file_paths) {
                let delta: HashSet<&str> = delta.iter().map(String::as_str).collect();
                if store.get_nodes_by_files(changed)?.iter().any(|n| delta.contains(n.name.as_str()) && is_jsx_child_candidate(n.kind, &n.name, n.language)) {
                    refresh_synthesis = true;
                    store.set_metadata("synthesis_pending", "1")?;
                }
            }
        }

        // Cross-file finalization (e.g. NestJS RouterModule prefixes). Runs on every sync that
        // touched files so edits to `app.module.ts` propagate to controllers in unchanged files. A
        // removal counts too (a pure-removal sync also drops the resolver's warmed name caches).
        if result.files_added > 0 || result.files_modified > 0 || result.files_removed > 0 {
            self.resolver().run_post_extract();
        }

        // A route that appeared, went away or was renamed changes what a navigation call in an
        // UNCHANGED file resolves to. Put those calls back in the pending set for the sweep below,
        // and let the synthesizers redraw the links markup makes to routes.
        if let Some(before) = &routes_before {
            let t = Instant::now();
            let routes = changed_routes(before, &store.get_nodes_by_kind(NodeKind::Route)?);
            let reopened = self.resolver().reopen_navigations_for(&routes, result.changed_file_paths.as_deref().unwrap_or(&[]), 500)?;
            if !routes.is_empty() && !refresh_synthesis {
                refresh_synthesis = true;
                store.set_metadata("synthesis_pending", "1")?;
            }
            if timings() {
                eprintln!("[phase-timing] sync-navigation-retry: {}ms ({} routes changed, {} refs re-opened)", t.elapsed().as_millis(), routes.len(), reopened);
            }
        }

        // Workspace packages and path dependencies (#2456, #2502). A package.json edit moves what
        // an import of the package resolves to in files it never touched. A full reconcile reads
        // the manifests again; when what they declare moved, every JavaScript-family reference is
        // re-opened and the orphan sweep resolves them by the new map, as a fresh index would. The
        // watcher's scoped syncs skip the read. An index stamped by an older engine has no
        // digest: record one.
        if full_reconcile {
            let t = Instant::now();
            let before = store.get_metadata(WORKSPACE_METADATA_KEY)?;
            let now = self.resolver().refresh_workspace_packages();
            if before.as_deref().is_some_and(|b| b != now) {
                let mut reopened = 0;
                for language in ESM_IMPORT_LANGUAGES {
                    reopened += self.orchestrator.resurrect_resolution_edges_for_language(language)?;
                }
                refresh_synthesis = true;
                store.set_metadata("synthesis_pending", "1")?;
                if timings() {
                    eprintln!("[phase-timing] sync-workspace-packages: {}ms ({} refs re-opened)", t.elapsed().as_millis(), reopened);
                }
            }
            if before.as_deref() != Some(now.as_str()) {
                self.stamp_workspace_fingerprint(Some(now));
            }
        }

        // Resolve references if files were updated.
        let files_changed = result.files_added > 0 || result.files_modified > 0;
        // The last edge before the failed-ref retry below: the rebind's per-name ceiling counts
        // only edges up to it (see resurrect_stale_resolution_edges).
        let mut edges_before_retry: Option<i64> = None;
        // And the last edge before this sync resolved anything: an importer's edge up to it was
        // bound before a module this sync adds was there.
        let edges_before_resolution = store.get_max_edge_id()?;
        if files_changed {
            if let Some(changed) = result.changed_file_paths.clone() {
                self.resolve_changed_files(&result, &changed, edges_before_resolution, &mut edges_before_retry, on_progress)?;
            } else {
                // No git info — use batched resolution to avoid OOM.
                let unresolved_count = store.get_unresolved_references_count()? as usize;
                emit(on_progress, IndexPhase::Resolving, 0, unresolved_count);
                self.resolve_references_batched(on_progress, false)?;
            }
        }

        // Re-open resolution edges this sync may have invalidated ELSEWHERE in the repo (CG-33):
        // references in files the sync never touched whose answer depended on a definition that
        // just appeared or disappeared. The resurrected refs are pending rows, so the orphan sweep
        // below resolves them. `definition_delta` is empty for a body-only edit.
        // CODEGRAPH_NO_REBIND=1 disables it.
        if let Some(delta) = &result.definition_delta {
            if !env_flag("CODEGRAPH_NO_REBIND") {
                let t = Instant::now();
                let rebound = self.orchestrator.resurrect_stale_resolution_edges(delta, result.changed_file_paths.as_deref().unwrap_or(&[]), edges_before_retry)?;
                if timings() {
                    eprintln!("[phase-timing] sync-rebind: {}ms ({} changed names, {} edges re-opened)", t.elapsed().as_millis(), delta.len(), rebound);
                }
            }
        }

        // The same for a route whose answer is read from a module: one that renders
        // `const Docs = lazy(() => import('./pages/Docs'))` binds to the component the module
        // exports, so it moves when the module is added or edited.
        if files_changed {
            if let Some(changed) = &result.changed_file_paths {
                let t = Instant::now();
                let reopened = self.resolver().reopen_route_module_readers(changed)?;
                if timings() {
                    eprintln!("[phase-timing] sync-route-modules: {}ms ({} refs re-opened)", t.elapsed().as_millis(), reopened);
                }
            }
        }

        // Python module answers this sync may have moved in files it never touched.
        {
            let before = store.get_metadata(PYTHON_ROOTS_METADATA_KEY)?;
            let now = self.resolver().get_python_root_fingerprint();
            if let Some(before_value) = &before {
                if *before_value != now {
                    self.resolver().clear_caches();
                    self.orchestrator.resurrect_resolution_edges_for_language(Language::Python)?;
                    // Any Python edge may move, so any synthesis built on one may too.
                    refresh_synthesis = true;
                    store.set_metadata("synthesis_pending", "1")?;
                } else {
                    let after = self.python_file_paths()?;
                    let mut moved: Vec<String> = after.iter().filter(|f| !python_files_before.contains(*f)).cloned().collect();
                    for f in &python_files_before {
                        if !after.contains(f) {
                            moved.push(f.clone());
                        }
                    }
                    let mut reopened: IndexSet<String> = IndexSet::new();
                    if !moved.is_empty() {
                        self.resolver().clear_caches();
                        let scope = self.resolver().get_python_reopen_scope(&moved);
                        reopened.extend(self.orchestrator.resurrect_resolution_edges_touching(Language::Python, &scope.target_files, &scope.source_dirs, &scope.module_leaves)?);
                    }
                    // An edited `__init__.py` can add, drop or repoint what its package binds, which
                    // moves what its importers' names resolve to.
                    let edited_inits: Vec<String> = result
                        .changed_file_paths
                        .iter()
                        .flatten()
                        .filter(|f| python_files_before.contains(*f) && after.contains(*f) && is_python_init(f))
                        .cloned()
                        .collect();
                    if !edited_inits.is_empty() {
                        self.resolver().clear_caches();
                        let bindings = self.resolver().get_python_package_importers(&edited_inits);
                        reopened.extend(self.orchestrator.resurrect_python_package_importers(&bindings)?);
                    }
                    // Resolve the narrow set now, the way the changed files' own references were
                    // resolved above, rather than leaving a handful of refs to the orphan sweep.
                    if !reopened.is_empty() {
                        let files: Vec<String> = reopened.iter().cloned().collect();
                        self.resolver().resolve_and_persist(&store.get_unresolved_references_by_files(&files)?)?;
                        // A moved edge in a file synthesis read or wrote means synthesis must be rebuilt.
                        if !refresh_synthesis
                            && files.iter().any(|f| store.has_synthesized_edges_touching_file(f).unwrap_or(false) || store.was_synthesis_input(f).unwrap_or(false))
                        {
                            refresh_synthesis = true;
                            store.set_metadata("synthesis_pending", "1")?;
                        }
                    }
                }
            }
            if before.as_deref() != Some(now.as_str()) {
                self.stamp_python_root_fingerprint(Some(now));
            }
        }

        // Orphan sweep (#1187). A resolution pass that dies mid-run leaves the refs it never
        // reached pending; any pending row now is such an orphan (or a row from an older engine's
        // scoped pass). Grind them down with the batched resolver; on a healthy index this is one
        // COUNT query.
        result.pending_refs_processed = Some(0);
        result.pending_refs_resolved = Some(0);
        result.pending_refs_unresolved = Some(0);
        let orphan_count = store.get_unresolved_references_count()? as usize;
        if orphan_count > 0 {
            emit(on_progress, IndexPhase::Resolving, 0, orphan_count);
            let recovery = self.resolve_references_batched(on_progress, false)?;
            result.pending_refs_processed = Some(recovery.total);
            result.pending_refs_resolved = Some(recovery.resolved);
            result.pending_refs_unresolved = Some(recovery.unresolved);
        }

        // Python attribute types read from OTHER files — a factory's return, a base class's
        // `__init__` — change with a body-only edit the definition delta cannot see.
        if !env_flag("CODEGRAPH_NO_REBIND") {
            let touched: Vec<String> = result.changed_file_paths.iter().flatten().chain(result.removed_file_paths.iter().flatten()).cloned().collect();
            let touched_py: Vec<String> = touched.into_iter().filter(|p| is_python_file(p)).collect();
            if !touched_py.is_empty() {
                let t = Instant::now();
                let reopened_files = self.orchestrator.resurrect_type_dependent_edges(&touched_py, result.changed_file_paths.as_deref().unwrap_or(&[]))?;
                // CODEGRAPH_PY_ATTR_RETRY_CEILING overrides the candidate ceiling (tests).
                let ceiling = std::env::var("CODEGRAPH_PY_ATTR_RETRY_CEILING").ok().and_then(|v| v.parse::<usize>().ok()).filter(|n| *n > 0).unwrap_or(2000);
                let failed = self.resolver().python_attr_retry_candidates(&touched_py, ceiling)?;
                let mut retry = store.get_unresolved_references_by_files(&reopened_files)?;
                let over_ceiling = failed.is_none();
                retry.extend(failed.unwrap_or_default());
                if !retry.is_empty() {
                    self.resolver().resolve_and_persist(&retry)?;
                }
                if timings() {
                    eprintln!("[phase-timing] sync-type-rebind: {}ms ({} refs{})", t.elapsed().as_millis(), retry.len(), if over_ceiling { ", failed-ref fan-in over ceiling" } else { "" });
                }
            }
        }

        if files_changed || orphan_count > 0 {
            // Second pass: chained calls whose method lives on a supertype the receiver conforms to
            // (#750), and `this.<member>` callback registrations whose member is inherited (#808).
            self.resolver().resolve_chained_calls_via_conformance()?;
            self.resolver().resolve_deferred_this_member_refs()?;
        }

        if refresh_synthesis {
            self.resolver().refresh_synthesis()?;
            store.set_metadata("synthesis_pending", "0")?;
        }

        // Refresh planner stats + checkpoint the WAL after bulk writes.
        if files_changed || result.files_removed > 0 || orphan_count > 0 || refresh_synthesis {
            store.run_maintenance();
        }

        // Heal the segment vocabulary on indexes built before the table existed (upgrade path):
        // a vocab that was empty when this sync STARTED means the bulk was never segmented.
        if vocab_was_empty && store.get_node_and_edge_count().map(|(n, _)| n > 0).unwrap_or(false) {
            let _ = self.rebuild_name_segment_vocab();
        }

        // A killed full index leaves the `indexing` marker. Sync repairs missing files, pending refs
        // and dropped indexes, so a successful recovery must also close the metadata state (#1556).
        if full_reconcile && self.get_index_state() == Some(IndexState::Indexing) {
            let _ = store.set_metadata("index_state", "complete");
        }

        self.orchestrator.finish_git_index_state(&git_state, full_reconcile, result.failed_file_paths.as_deref().unwrap_or(&[]))?;

        if full_reconcile && result.files_checked > 0 {
            self.pending_full_reconcile.set(false);
        }
        // The whole sync, as for index_all: resolution and linking included (#2334).
        result.duration_ms = started.elapsed().as_millis() as u64;
        Ok(result)
    }

    /// The git-scoped half of a sync's resolution: the changed files' own references, then every
    /// reference elsewhere that the change may now satisfy (#1240) and the module-importer reopen
    /// passes.
    fn resolve_changed_files(
        &self,
        result: &SyncResult,
        changed: &[String],
        edges_before_resolution: i64,
        edges_before_retry: &mut Option<i64>,
        on_progress: &mut Option<ProgressFn<'_>>,
    ) -> Result<()> {
        let store = &self.store;
        // Scope resolution to changed files (git fast path — bounded set).
        let t = Instant::now();
        let unresolved = store.get_unresolved_references_by_files(changed)?;
        if timings() {
            eprintln!("[phase-timing] sync-ref-load: {}ms ({} refs)", t.elapsed().as_millis(), unresolved.len());
        }
        emit(on_progress, IndexPhase::Resolving, 0, unresolved.len());
        self.resolver().resolve_and_persist(&unresolved)?;
        emit(on_progress, IndexPhase::Resolving, unresolved.len(), unresolved.len());

        // Retry previously-failed refs the changed files may now satisfy (#1240). Look them up by
        // the symbol names the changed files now carry (plus each file's own keys, the keys a
        // route's lazily loaded module waits under, and the module keys of an import binding).
        let t = Instant::now();
        *edges_before_retry = Some(store.get_max_edge_id()?);
        let mut names: IndexSet<String> = store.get_node_names_by_files(changed, None)?.into_iter().collect();
        for f in changed {
            names.extend(codegraph_db::reference_tail::module_reference_keys(f));
        }
        let names: Vec<String> = names.into_iter().collect();
        let mut retryable = store.get_retryable_failed_references(&names, 500, 5000)?;
        // A failed import waits for a file, a folder or a namespace, not a symbol: look those up by
        // what the ADDED files can be imported as, and by the namespaces and modules the changed
        // files declare.
        let added_files: HashSet<String> = result.added_file_paths.iter().flatten().cloned().collect();
        let added_list: Vec<String> = result.added_file_paths.clone().unwrap_or_default();
        let resolves_to_added = |specifier: &str, from_file: &str, language: Language| -> bool {
            self.resolver().resolve_module_file(specifier, from_file, language).is_some_and(|f| added_files.contains(&f))
        };
        let resolved_modules: Vec<String> = if added_files.is_empty() {
            Vec::new()
        } else {
            store.get_failed_scoped_imports()?.into_iter().filter(|s| resolves_to_added(&s.name, &s.file_path, s.language)).map(|s| s.name).collect()
        };
        let folder_imports: Vec<UnresolvedReference> = if added_files.is_empty() {
            Vec::new()
        } else {
            store
                .get_failed_folder_imports()?
                .into_iter()
                .filter(|r| match (&r.file_path, r.language) {
                    (Some(fp), Some(lang)) => resolves_to_added(&r.reference_name, fp, lang),
                    _ => false,
                })
                .collect()
        };
        let mut retry_rows: HashSet<Option<i64>> = retryable.iter().map(|r| r.row_id).collect();
        let path_keys: Vec<String> = added_list.iter().flat_map(|f| codegraph_db::reference_tail::import_path_keys(f)).collect();
        let namespace_names = store.get_node_names_by_files(changed, Some(&[NodeKind::Namespace, NodeKind::Module]))?;
        let could_name = self.resolver().could_name_any_of(&added_list);
        let import_retry = store.get_retryable_failed_imports(&path_keys, &namespace_names, 500, &resolved_modules, 5000, Some(&*could_name))?;
        for r in import_retry.into_iter().chain(folder_imports) {
            if retry_rows.insert(r.row_id) {
                retryable.push(r);
            }
        }
        // In row order, as a full index resolves them: when two refs make the same edge, the one
        // written first names it, as it does in a fresh index.
        retryable.sort_by_key(|r| r.row_id.unwrap_or(0));
        if !retryable.is_empty() {
            emit(on_progress, IndexPhase::Resolving, 0, retryable.len());
            self.resolver().resolve_and_persist(&retryable)?;
            emit(on_progress, IndexPhase::Resolving, retryable.len(), retryable.len());
        }
        if timings() {
            eprintln!("[phase-timing] sync-failed-ref-retry: {}ms ({} refs)", t.elapsed().as_millis(), retryable.len());
        }

        // A module that appeared, such as a package `__init__.py` or an `index.ts` re-exporting what
        // it imports: what the files importing it bound by name in its absence, or failed, goes back
        // to pending for the sweep below. (The removal half runs before removed files are deleted.)
        let reader = ImportReader::new(&self.project_root);
        let module_resolver = |specifier: &str, from_file: &str, language: Language| self.resolver().resolve_module_file(specifier, from_file, language);
        let mut added: Vec<ChangedFile> = Vec::new();
        for file_path in &added_list {
            if let Some(file) = store.get_file_by_path(file_path)? {
                added.push(ChangedFile { path: file_path.clone(), language: file.language });
            }
        }
        if !added.is_empty() {
            let t = Instant::now();
            let package_dir_of = |file: &str| self.resolver().workspace_package_dir_of(file);
            let reopened =
                reopen_references_through_added_files(store, &reader, &added, edges_before_resolution, Some(&module_resolver), Some(&package_dir_of))?;
            if timings() {
                eprintln!("[phase-timing] sync-added-module-importers: {}ms ({} refs re-opened)", t.elapsed().as_millis(), reopened);
            }
        }

        // A reference that reaches a module through a barrel waits for a file below it. Those that
        // reach a file this sync adds, or one whose definitions it changed, go back to pending for
        // the sweep. A body-only edit satisfies none.
        let t = Instant::now();
        let delta: HashSet<&str> = result.definition_delta.iter().flatten().map(String::as_str).collect();
        let mut seeds: indexmap::IndexMap<String, Language> = added.iter().map(|f| (f.path.clone(), f.language)).collect();
        let modified: Vec<String> = changed.iter().filter(|f| !seeds.contains_key(*f)).cloned().collect();
        if !delta.is_empty() && !modified.is_empty() {
            for node in store.get_nodes_by_files(&modified)? {
                if delta.contains(node.name.as_str()) {
                    seeds.insert(node.file_path.clone(), node.language);
                }
            }
        }
        let seed_files: Vec<ChangedFile> = seeds.into_iter().map(|(path, language)| ChangedFile { path, language }).collect();
        let through_barrels = reopen_references_through_barrels(store, &reader, &seed_files, edges_before_resolution)?;
        if timings() {
            eprintln!("[phase-timing] sync-barrel-modules: {}ms ({} refs re-opened)", t.elapsed().as_millis(), through_barrels);
        }

        // A declaration file that declares a package itself (`declare module 'mammoth' { … }`).
        let declared = reopen_references_through_declared_packages(store, &reader, changed, edges_before_resolution)?;
        if timings() && declared > 0 {
            eprintln!("[phase-timing] sync-declared-packages: {declared} refs re-opened");
        }

        // An edited module whose default export moved to another declaration, or went away, while
        // the old one stayed declared. Read off the edges into the edited modules.
        let t = Instant::now();
        let moved_defaults = self.resolver().changed_default_exports(&modified, changed, edges_before_resolution)?;
        let through_defaults = if moved_defaults.is_empty() { 0 } else { reopen_references_through_changed_defaults(store, &reader, &moved_defaults, edges_before_resolution)? };
        if timings() {
            eprintln!("[phase-timing] sync-default-exports: {}ms ({} defaults moved, {} refs re-opened)", t.elapsed().as_millis(), moved_defaults.len(), through_defaults);
        }

        // An import a name match took while its module did not export the name, once the binding
        // resolves another way; and a name match into the edited module itself.
        let t = Instant::now();
        let mut edited_modules: Vec<ChangedFile> = Vec::new();
        for file_path in &modified {
            if let Some(file) = store.get_file_by_path(file_path)? {
                edited_modules.push(ChangedFile { path: file_path.clone(), language: file.language });
            }
        }
        let resolve_now = |r: &UnresolvedReference| self.resolver().resolve_now(r);
        let name_matched = reopen_name_matched_imports(store, &reader, &edited_modules, edges_before_resolution, &module_resolver, &resolve_now)?;
        if timings() {
            eprintln!("[phase-timing] sync-name-matched-imports: {}ms ({} refs re-opened)", t.elapsed().as_millis(), name_matched);
        }
        Ok(())
    }

    /// Rebuild the segment vocabulary from the current graph, batched — the upgrade-heal path for
    /// indexes built before the vocab table existed.
    fn rebuild_name_segment_vocab(&self) -> Result<()> {
        const BATCH: usize = 2000;
        let mut offset = 0usize;
        loop {
            let names = self.store.get_distinct_node_names(BATCH, offset)?;
            if names.is_empty() {
                break;
            }
            self.store.insert_name_segments_batch(&names)?;
            offset += BATCH;
        }
        Ok(())
    }

    /// One-shot upgrade heal for callers that open the graph WITHOUT syncing — concretely the
    /// prompt hook, whose MEDIUM tier reads the segment vocabulary (#1142). Returns true when the
    /// vocab is usable (already populated, or healed here); false when it isn't (empty graph, or
    /// another process holds the index lock — that process's own sync heals it).
    pub fn heal_segment_vocab_if_empty(&self) -> bool {
        if !self.store.is_name_segment_vocab_empty().unwrap_or(false) {
            return true;
        }
        if self.store.get_node_and_edge_count().map(|(n, _)| n == 0).unwrap_or(true) {
            return false;
        }
        if self.indexing.get() || self.file_lock.borrow_mut().acquire().is_err() {
            return false; // an index/sync is running — it backfills the vocab itself
        }
        let healed = if self.store.is_name_segment_vocab_empty().unwrap_or(false) { self.rebuild_name_segment_vocab().is_ok() } else { true };
        self.file_lock.borrow_mut().release();
        healed
    }

    // ===========================================================================
    // Index state
    // ===========================================================================

    /// Files that have changed since the last index.
    pub fn get_changed_files(&self) -> Result<ChangedFiles> {
        if self.watcher.is_some(){ExtractionOrchestrator::new(&self.project_root,self.store().clone()).get_changed_files()}else{self.orchestrator.get_changed_files()}
    }

    /// Most recent index timestamp (ms since epoch) across all tracked files, or None when nothing
    /// is indexed yet (#329).
    pub fn get_last_indexed_at(&self) -> Result<Option<i64>> {
        Ok(self.store().get_last_indexed_at()?)
    }

    /// Completeness of the last full index run. `Complete` is the only good state; `None` = the
    /// index predates this marker.
    pub fn get_index_state(&self) -> Option<IndexState> {
        match self.store().get_metadata("index_state").ok().flatten().as_deref() {
            Some("indexing") => Some(IndexState::Indexing),
            Some("complete") => Some(IndexState::Complete),
            Some("partial") => Some(IndexState::Partial),
            Some("failed") => Some(IndexState::Failed),
            _ => None,
        }
    }

    /// Which engine built the current index: the package version + extraction version stamped at
    /// the last full `index_all`.
    pub fn get_index_build_info(&self) -> IndexBuildInfo {
        let version = self.store().get_metadata("indexed_with_version").ok().flatten();
        let extraction_version = self.store().get_metadata("indexed_with_extraction_version").ok().flatten().and_then(|v| v.trim().parse::<u32>().ok());
        IndexBuildInfo { version, extraction_version }
    }

    /// True when the on-disk index was built by an engine whose extraction is older than the one
    /// now running — a re-index would add data a migration can't backfill.
    pub fn is_index_stale(&self) -> bool {
        if self.store().get_last_indexed_at().ok().flatten().is_none() {
            return false;
        }
        self.get_index_build_info().extraction_version.is_none_or(|v| v < EXTRACTION_VERSION)
    }

    /// References extracted but never attempted by a resolution pass. Zero on a healthy index.
    pub fn get_pending_reference_count(&self) -> Result<i64> {
        Ok(self.store().get_unresolved_references_count()?)
    }

    /// Detected frameworks in the project.
    pub fn get_detected_frameworks(&self) -> Vec<String> {
        if self.watcher.is_some(){ReferenceResolver::create(&self.project_root,self.store().clone()).detected_frameworks()}else{self.resolver().detected_frameworks()}
    }

    /// Re-initialize the resolver (useful after adding new files).
    pub fn reinitialize_resolver(&self) {
        self.resolver().initialize();
    }

    /// Resolve the pending references and create edges.
    pub fn resolve_references(&self) -> Result<codegraph_resolve::types::ResolutionResult> {
        let refs = self.store.get_unresolved_references()?;
        Ok(self.resolver().resolve_and_persist(&refs)?)
    }

    /// Indexed files whose symbols are missing although their content is current — rows a
    /// content-hash comparison calls up to date (#2336). `sync` repairs the `needs_reindex` group.
    pub fn get_index_health(&self) -> Result<IndexHealth> {
        let mut needs_reindex: Vec<String> = Vec::new();
        let mut parse_errors: Vec<String> = Vec::new();
        for file in self.store().get_files_without_nodes_or_with_errors()? {
            let errors: &[ExtractionError] = file.errors.as_deref().unwrap_or(&[]);
            if has_grammar_load_failure(errors) {
                // Stored without being parsed — its grammar could not load (#2335).
                needs_reindex.push(file.path.clone());
            } else if file.node_count == 0 {
                // Every parse stores at least the file node, so zero nodes means a wiped row (#1541)
                // or a recorded failure — except file-level-only languages and files over the size
                // limit, which are empty on purpose.
                if is_file_level_only_language(file.language) {
                    continue;
                }
                if errors.is_empty() {
                    needs_reindex.push(file.path.clone());
                } else if errors.iter().any(|e| e.severity == Severity::Error || e.code.as_deref() == Some("parse_error")) {
                    parse_errors.push(file.path.clone());
                }
            } else if errors.iter().any(|e| e.code.as_deref() == Some("parse_error")) {
                // Parsed, but the tree had errors and no symbols survived.
                parse_errors.push(file.path.clone());
            }
        }
        Ok(IndexHealth { needs_reindex, parse_errors })
    }

    /// Get a file record by path.
    pub fn get_file(&self, file_path: &str) -> Result<Option<FileRecord>> {
        Ok(self.store().get_file_by_path(file_path)?)
    }

    /// Get all tracked files.
    pub fn get_files(&self) -> Result<Vec<FileRecord>> {
        Ok(self.store().get_all_files()?)
    }

    /// Whether an OS watcher event points at a file whose index metadata is still current. Used on
    /// Windows to discard NTFS last-access notifications (#1451).
    pub fn is_indexed_file_state_current(&self, file_path: &str) -> bool {
        let Ok(Some(tracked)) = self.store().get_file_by_path(file_path) else { return false };
        let Ok(stat) = crate::orchestrator::FileStat::of(&Path::new(&self.project_root).join(file_path)) else { return false };
        stat.size == tracked.size as u64 && stat.mtime_ms.floor() == (tracked.modified_at as f64).floor()
    }

    /// Optimize the database (vacuum and analyze).
    pub fn optimize(&self) -> Result<()> {
        Ok(self.store.optimize()?)
    }

    /// Clear all data from the graph.
    pub fn clear(&self) -> Result<()> {
        Ok(self.store.clear()?)
    }

    /// Completely remove CodeGraph from the project: closes the database and deletes the
    /// `.codegraph` directory. WARNING: permanently deletes all CodeGraph data for the project.
    pub fn uninitialize(self) -> std::io::Result<()> {
        let root = self.project_root.clone();
        self.close();
        remove_directory(&root)
    }
}

/// `minRefsForPool`: the pending-ref count past which a resolution run drops the non-unique
/// edge/ref indexes for its batch loop.
fn min_refs_for_bulk() -> usize {
    std::env::var("CODEGRAPH_PARALLEL_RESOLVE_MIN").ok().and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(150_000)
}

#[allow(dead_code)]
fn _assert_send_free(_: &HashMap<String, String>) {}
