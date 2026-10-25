//! Extraction orchestrator — port of `ExtractionOrchestrator` in `src/extraction/index.ts`.
//!
//! Coordinates file scanning, parsing and database storage. [`index`] holds the full and
//! single-file indexing paths, [`sync`] the incremental reconcile; this file holds the shared
//! state, the per-file store and the re-resolution helpers they both lean on.

mod git_state;
mod index;
mod reopen;
mod sync;

use crate::errors::Result;
use crate::extract::has_grammar_load_failure;
use codegraph_db::{FileBundle, NodeIdentity, Store};
use codegraph_extract::scan::config::PROJECT_CONFIG_FILENAME;
use codegraph_extract::scan::{build_default_ignore, build_scope_ignore, ScopeIgnore};
use codegraph_resolve::ctx::Ctx;
use codegraph_resolve::frameworks::package_deps::declared_dependencies;
use codegraph_resolve::frameworks::detect_frameworks;
use codegraph_resolve::resolver::to_db_reference;
use codegraph_resolve::resurrect_ref::resurrect_ref_from_dropped_edge;
use codegraph_types::{Edge, ExtractionError, ExtractionResult, FileRecord, Language, Node, UnresolvedReference};
use indexmap::IndexMap;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

pub use git_state::{ChangedFiles, GitIndexSnapshot};

/// Number of files whose parse may be in flight per worker, before the in-order commit catches up.
/// Room enough that one slow file at the commit cursor does not stall every other worker.
pub(crate) const WINDOW_PER_WORKER: usize = 8;

/// How many files the `sync()` reconcile processes between checks of whether to keep going
/// (the TypeScript yields to its event loop here; this port has no loop to starve, but keeps the
/// constant for the progress cadence).
pub const SYNC_RECONCILE_YIELD_INTERVAL: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexPhase {
    Scanning,
    Parsing,
    Storing,
    Resolving,
    Linking,
}

/// Progress callback payload for indexing operations.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexProgress {
    pub phase: IndexPhase,
    pub current: usize,
    pub total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_file: Option<String>,
}

impl IndexProgress {
    pub fn new(phase: IndexPhase, current: usize, total: usize) -> Self {
        IndexProgress { phase, current, total, current_file: None }
    }
    pub fn with_file(phase: IndexPhase, current: usize, total: usize, file: &str) -> Self {
        IndexProgress { phase, current, total, current_file: Some(file.to_string()) }
    }
}

/// A progress sink.
pub type ProgressFn<'a> = &'a mut dyn FnMut(&IndexProgress);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnsupportedExtension {
    pub ext: String,
    pub count: usize,
}

/// Result of an indexing operation.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexResult {
    pub success: bool,
    pub files_indexed: usize,
    pub files_skipped: usize,
    pub files_errored: usize,
    /// How many indexable files the scan discovered — the ground truth the
    /// indexed/skipped/errored tallies must add up to. Only set by full-index runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_discovered: Option<usize>,
    /// Files the scan saw but has no grammar for (#1502).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_skipped_unsupported: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_unsupported_extensions: Option<Vec<UnsupportedExtension>>,
    pub nodes_created: usize,
    pub edges_created: usize,
    pub errors: Vec<ExtractionError>,
    pub duration_ms: u64,
}

/// Result of a sync operation.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_refs_processed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_refs_resolved: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_refs_unresolved: Option<usize>,
    pub files_checked: usize,
    pub files_added: usize,
    pub files_modified: usize,
    pub files_removed: usize,
    pub nodes_updated: usize,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_file_paths: Option<Vec<String>>,
    /// The part of `changed_file_paths` that was not indexed before this sync.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_file_paths: Option<Vec<String>>,
    /// Paths not absorbed because reading or extraction failed; retained for status/retry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_file_paths: Option<Vec<String>>,
    /// Symbol names whose set of definitions this sync CHANGED (CG-33).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition_delta: Option<Vec<String>>,
    /// Tracked files this sync found gone and deleted from the index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed_file_paths: Option<Vec<String>>,
}

/// SHA-256 of file contents, hex.
pub fn hash_content(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// A file's stat, as the index records it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FileStat {
    pub size: u64,
    /// Fractional milliseconds since the epoch (`stat.mtimeMs`).
    pub mtime_ms: f64,
}

impl FileStat {
    pub fn from_metadata(meta: &std::fs::Metadata) -> FileStat {
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as f64 * 1000.0 + f64::from(d.subsec_nanos()) / 1e6)
            .unwrap_or(0.0);
        FileStat { size: meta.len(), mtime_ms }
    }
    pub fn of(path: &Path) -> std::io::Result<FileStat> {
        Ok(FileStat::from_metadata(&std::fs::metadata(path)?))
    }
}

// -- Re-index node pairing --------------------------------------------------------------------

type IdentityTier = fn(&NodeIdentity) -> String;

/// What tells a file's nodes apart across a re-index, coarsest first. (kind, name) is the #899 key
/// and settles every name a file defines once; a name it defines more than once — the same method
/// on two classes, a method's overloads — is split further by qualified name, then by signature
/// (#2276).
const REINDEX_IDENTITY_TIERS: [IdentityTier; 3] = [
    |n| format!("{}\0{}", n.kind.as_str(), n.name),
    |n| n.qualified_name.clone(),
    |n| n.signature.clone().unwrap_or_default(),
];

fn group_node_identities<'a>(nodes: &[&'a NodeIdentity], key: IdentityTier) -> IndexMap<String, Vec<&'a NodeIdentity>> {
    let mut groups: IndexMap<String, Vec<&NodeIdentity>> = IndexMap::new();
    for n in nodes {
        groups.entry(key(n)).or_default().push(n);
    }
    groups
}

/// Pair each node a file had before a re-index with the node that replaces it, old id → new id.
/// Ids embed the start line, so they can't be compared across an edit. Each tier of
/// [`REINDEX_IDENTITY_TIERS`] splits the groups the one before left ambiguous, and a group down to
/// one node on each side is a pair. Nodes still identical after every tier (overloads in a language
/// that records no signature) pair by position, but only when both sides have the same number of
/// them. Anything else stays unpaired: the caller re-resolves its edges from their original
/// reference instead of handing them all to whichever same-named node happens to come last (#2276).
pub fn pair_reindexed_nodes(prior: &[NodeIdentity], next: &[NodeIdentity]) -> HashMap<String, String> {
    fn pair_groups<'a>(before: &[&'a NodeIdentity], after: &[&'a NodeIdentity], tier: usize, pairs: &mut HashMap<String, String>) {
        let key = REINDEX_IDENTITY_TIERS[tier];
        let after_groups = group_node_identities(after, key);
        for (k, group) in group_node_identities(before, key) {
            let Some(matched) = after_groups.get(&k) else { continue };
            if group.len() == 1 && matched.len() == 1 {
                pairs.insert(group[0].id.clone(), matched[0].id.clone());
            } else if tier + 1 < REINDEX_IDENTITY_TIERS.len() {
                pair_groups(&group, matched, tier + 1, pairs);
            } else if group.len() == matched.len() {
                let by_position = |a: &&NodeIdentity, b: &&NodeIdentity| a.start_line.cmp(&b.start_line).then(a.start_column.cmp(&b.start_column));
                let mut a = group.clone();
                let mut b = matched.clone();
                a.sort_by(by_position);
                b.sort_by(by_position);
                for (x, y) in a.iter().zip(b.iter()) {
                    pairs.insert(x.id.clone(), y.id.clone());
                }
            }
        }
    }
    let mut pairs = HashMap::new();
    let prior_refs: Vec<&NodeIdentity> = prior.iter().collect();
    let next_refs: Vec<&NodeIdentity> = next.iter().collect();
    pair_groups(&prior_refs, &next_refs, 0, &mut pairs);
    pairs
}

fn identity_of(n: &Node) -> NodeIdentity {
    NodeIdentity {
        id: n.id.clone(),
        kind: n.kind,
        name: n.name.clone(),
        qualified_name: n.qualified_name.clone(),
        signature: n.signature.clone(),
        start_line: n.start_line,
        start_column: n.start_column,
    }
}

// -- Framework gating -------------------------------------------------------------------------

/// Which detected frameworks' extractors apply to a file. A framework with `appDependencies` runs
/// only inside its own app: the nearest `package.json` at or above the file that names ANY gated
/// framework decides. Shared with the parse workers, so its memos are locked.
pub struct FrameworkGate {
    root_dir: String,
    gated: HashMap<String, Vec<&'static str>>,
    app_memo: Mutex<HashMap<String, bool>>,
    /// Directory → the dependency names its package.json declares, `None` without one.
    manifests: Mutex<HashMap<String, Option<Arc<HashSet<String>>>>>,
}

impl FrameworkGate {
    fn new(root_dir: &str, gated: HashMap<String, Vec<&'static str>>) -> FrameworkGate {
        FrameworkGate { root_dir: root_dir.to_string(), gated, app_memo: Mutex::new(HashMap::new()), manifests: Mutex::new(HashMap::new()) }
    }

    /// The detected frameworks whose extractors apply to `file_path`: all of `names`, except one
    /// with `appDependencies` when neither the file's package.json nor an enclosing one declares
    /// any of them.
    pub fn for_file(&self, file_path: &str, names: &[String]) -> Vec<String> {
        if self.gated.is_empty() {
            return names.to_vec();
        }
        let dir = match file_path.rfind('/') {
            None => "",
            Some(slash) => &file_path[..slash],
        };
        names.iter().filter(|name| self.applies_in(dir, name)).cloned().collect()
    }

    fn applies_in(&self, dir: &str, name: &str) -> bool {
        let Some(deps) = self.gated.get(name) else { return true };
        let key = format!("{dir}|{name}");
        if let Some(memo) = self.app_memo.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
            return *memo;
        }
        // The nearest manifest that names ANY gated framework decides: true-sheet's root
        // package.json declares expo-router for its example app, and its `docs/` Next.js app —
        // whose own package.json declares `next` — is not an Expo app for it.
        let mut applies = false;
        let mut current: Option<String> = Some(dir.to_string());
        while let Some(d) = current {
            if let Some(declared) = self.dependencies_declared_in(&d) {
                if deps.iter().any(|dep| declared.contains(*dep)) {
                    applies = true;
                    break;
                }
                if self.gated.iter().any(|(other, other_deps)| other != name && other_deps.iter().any(|dep| declared.contains(*dep))) {
                    break;
                }
            }
            current = if d.is_empty() {
                None
            } else if let Some(i) = d.rfind('/') {
                Some(d[..i].to_string())
            } else {
                Some(String::new())
            };
        }
        self.app_memo.lock().unwrap_or_else(|e| e.into_inner()).insert(key, applies);
        applies
    }

    fn dependencies_declared_in(&self, dir: &str) -> Option<Arc<HashSet<String>>> {
        if let Some(hit) = self.manifests.lock().unwrap_or_else(|e| e.into_inner()).get(dir) {
            return hit.clone();
        }
        let declared = (|| {
            let text = std::fs::read_to_string(Path::new(&self.root_dir).join(dir).join("package.json")).ok()?;
            let pkg: serde_json::Value = serde_json::from_str(&text).ok()?;
            let mut names = HashSet::new();
            for field in ["dependencies", "devDependencies", "peerDependencies"] {
                if let Some(group) = pkg.get(field).and_then(|g| g.as_object()) {
                    names.extend(group.keys().cloned());
                }
            }
            Some(Arc::new(names))
        })();
        self.manifests.lock().unwrap_or_else(|e| e.into_inner()).insert(dir.to_string(), declared.clone());
        declared
    }
}

// -- The orchestrator -------------------------------------------------------------------------

/// Extraction orchestrator.
pub struct ExtractionOrchestrator {
    pub(crate) root_dir: String,
    pub(crate) store: Rc<Store>,
    /// Names of frameworks detected for this project, populated by `index_all()`. Cleared if
    /// detection hasn't run yet so single-file re-index paths can detect on the spot.
    detected_framework_names: RefCell<Option<Vec<String>>>,
    gate: RefCell<Arc<FrameworkGate>>,
    /// Scope matcher for SCOPED syncs, memoized on the mtimes of the files it is derived from.
    scoped_matcher: RefCell<Option<(String, Rc<ScopeIgnore>)>>,
    scratch: RefCell<Option<Rc<Store>>>,
}

impl ExtractionOrchestrator {
    pub fn new(root_dir: &str, store: Rc<Store>) -> Self {
        ExtractionOrchestrator {
            root_dir: root_dir.to_string(),
            store,
            detected_framework_names: RefCell::new(None),
            gate: RefCell::new(Arc::new(FrameworkGate::new(root_dir, HashMap::new()))),
            scoped_matcher: RefCell::new(None),
            scratch: RefCell::new(None),
        }
    }

    pub fn root_dir(&self) -> &str {
        &self.root_dir
    }


    /// The scope matcher a scoped sync applies to the paths it was handed — the same
    /// `build_scope_ignore` the full scan uses, so an explicitly-passed path that is OUT of scope
    /// (a user `exclude` in `codegraph.json`, a `.gitignore` rule, a built-in default) is treated
    /// exactly as the full walk would treat it: absent, hence removed if tracked, never parsed
    /// (#1590). Memoized on the root config + root `.gitignore` + `info/exclude` mtimes.
    pub(crate) fn scoped_sync_matcher(&self) -> Rc<ScopeIgnore> {
        let root = Path::new(&self.root_dir);
        let git_dir = codegraph_extract::scan::git::resolve_git_dir(root);
        let sources = [
            root.join(PROJECT_CONFIG_FILENAME),
            root.join(".gitignore"),
            git_dir.map(|g| g.join("info").join("exclude")).unwrap_or_default(),
        ];
        let key = sources
            .iter()
            .map(|p| {
                if p.as_os_str().is_empty() {
                    return "-".to_string();
                }
                match FileStat::of(p) {
                    Ok(stat) => format!("{}", stat.mtime_ms),
                    Err(_) => "-".to_string(),
                }
            })
            .collect::<Vec<_>>()
            .join("|");
        if let Some((k, matcher)) = self.scoped_matcher.borrow().as_ref() {
            if *k == key {
                return Rc::clone(matcher);
            }
        }
        let matcher = Rc::new(build_scope_ignore(root, None).unwrap_or_else(|_| ScopeIgnore::new(build_default_ignore(root), Vec::new(), None, None, Vec::new())));
        *self.scoped_matcher.borrow_mut() = Some((key, Rc::clone(&matcher)));
        matcher
    }

    pub(crate) fn clear_scoped_matcher(&self) {
        *self.scoped_matcher.borrow_mut() = None;
    }

    fn scratch_store(&self) -> Result<Rc<Store>> {
        if let Some(s) = self.scratch.borrow().as_ref() {
            return Ok(Rc::clone(s));
        }
        let store = Rc::new(Store::initialize(":memory:")?);
        *self.scratch.borrow_mut() = Some(Rc::clone(&store));
        Ok(store)
    }

    /// Detect frameworks on demand using the current scanned files (or a fresh scan if none are
    /// provided). Cached on the orchestrator so repeat calls inside a single run don't re-scan.
    ///
    /// Detection reads only the file list, the manifests on disk and the file contents, so it runs
    /// against an empty scratch database rather than the (still empty) index.
    pub(crate) fn ensure_detected_frameworks(&self, files: Option<Vec<String>>) -> Vec<String> {
        if let Some(names) = self.detected_framework_names.borrow().as_ref() {
            return names.clone();
        }
        let file_list = files.unwrap_or_else(|| codegraph_extract::scan::scan_directory(Path::new(&self.root_dir), None, None).unwrap_or_default());
        let Ok(scratch) = self.scratch_store() else { return Vec::new() };
        let ctx = Ctx::with_file_list(self.root_dir.clone(), scratch, file_list);
        let detected = detect_frameworks(&ctx);
        let names: Vec<String> = detected.iter().map(|r| r.name().to_string()).collect();
        let declared = declared_dependencies(&ctx);
        let mut gated: HashMap<String, Vec<&'static str>> = HashMap::new();
        for resolver in &detected {
            // Gated only when some manifest names the framework: otherwise detection found it by
            // other evidence and every file is its app's.
            if let Some(deps) = resolver.app_dependencies() {
                if deps.iter().any(|d| declared.contains(*d)) {
                    gated.insert(resolver.name().to_string(), deps.to_vec());
                }
            }
        }
        *self.gate.borrow_mut() = Arc::new(FrameworkGate::new(&self.root_dir, gated));
        *self.detected_framework_names.borrow_mut() = Some(names.clone());
        names
    }

    pub(crate) fn reset_detected_frameworks(&self) {
        *self.detected_framework_names.borrow_mut() = None;
    }

    pub(crate) fn framework_gate(&self) -> Arc<FrameworkGate> {
        Arc::clone(&self.gate.borrow())
    }

    /// Delete file rows recorded with ZERO nodes so their files re-index.
    ///
    /// No extraction path stores an empty, error-free result for a symbol-bearing language — even
    /// an empty file keeps its file node — so a zero-node row is a wiped one (#1541: an
    /// interrupted parse's retry stored an undecoded kernel transport). The wiped row's content
    /// hash matches the on-disk bytes, so every hash-based reconcile skips the file forever;
    /// deleting the row lets the normal add path repair it. File-level-only languages (yaml,
    /// twig, properties) are left alone.
    pub(crate) fn heal_zero_node_rows(&self) -> Result<()> {
        for f in self.store.get_all_files()? {
            // A zero-node row WITH recorded errors is a deliberate skip marker (#1557: oversized /
            // repeatedly-unparseable files are persisted with their reason so syncs stop retrying
            // them) — leave those alone. The #1541 wipe rows are the error-FREE zero-node rows.
            if f.node_count == 0 && !codegraph_extract::languages::is_file_level_only_language(f.language) && f.errors.as_ref().is_none_or(|e| e.is_empty()) {
                self.store.delete_file(&f.path)?;
            }
        }
        Ok(())
    }

    /// Store extraction result in database.
    pub(crate) fn store_extraction_result(&self, file_path: &str, content: &str, language: Language, stats: FileStat, result: ExtractionResult) -> Result<()> {
        // The file was never parsed: its grammar failed to load (#2335). Storing this would
        // replace the file's symbols with an empty row under the new content hash, which no
        // hash-based sync revisits. Keep whatever the index has; the stale (or missing) row makes
        // the next sync retry the file.
        if has_grammar_load_failure(&result.errors) {
            return Ok(());
        }
        let content_hash = hash_content(content);

        // Check if file already exists and hasn't changed. A skip/failure MARKER row (zero nodes +
        // recorded errors, #1557) never blocks a store carrying real content: markers are written
        // BEFORE the retry pass under the same content hash, so treating them as "no changes"
        // would silently discard a successful retry's symbols.
        let existing = self.store.get_file_by_path(file_path)?;
        if let Some(existing_file) = &existing {
            if existing_file.content_hash == content_hash {
                let existing_is_marker = existing_file.node_count == 0 && existing_file.errors.as_ref().is_some_and(|e| !e.is_empty());
                let incoming_has_content = !result.nodes.is_empty();
                // A row an older engine stored while the grammar could not load (#2335) records
                // no parse at all: any real result replaces it.
                let existing_never_parsed = existing_file.errors.as_deref().is_some_and(has_grammar_load_failure);
                if !existing_never_parsed && (!existing_is_marker || !incoming_has_content) {
                    return Ok(()); // No changes
                }
            }
        }

        // Re-decided on every re-index of a changed file, so a banner added (or removed) by an edit
        // is reflected on the next sync (#1500).
        let generated = codegraph_search::generated::detect_generated_file(file_path, content);

        // Snapshot incoming cross-file edges BEFORE deleting this file's nodes (#899):
        // `delete_file` cascades to every edge whose source OR target is a node in this file, and
        // the edges whose SOURCE is in a *different* (unchanged) file are not re-emitted by the
        // extractor. Each edge follows its old target to the re-indexed node that replaces it.
        let cross_file_incoming = if existing.is_some() { self.store.get_cross_file_incoming_edges_with_target(file_path)? } else { Vec::new() };
        let prior_nodes = if cross_file_incoming.is_empty() { Vec::new() } else { self.store.get_node_identities_by_file(file_path)? };

        // Filter out nodes with missing required fields before insertion. This prevents FK
        // violations when edges reference nodes that would be silently skipped (issue #42).
        let node_count = result.nodes.len();
        let errors = result.errors;
        let valid_nodes: Vec<Node> = result.nodes.into_iter().filter(|n| !n.id.is_empty() && !n.name.is_empty() && !n.file_path.is_empty()).collect();
        let inserted_ids: HashSet<&str> = valid_nodes.iter().map(|n| n.id.as_str()).collect();
        let valid_edges: Vec<Edge> = result.edges.into_iter().filter(|e| inserted_ids.contains(e.source.as_str()) && inserted_ids.contains(e.target.as_str())).collect();
        let valid_refs: Vec<UnresolvedReference> = result
            .unresolved_references
            .into_iter()
            .filter(|r| inserted_ids.contains(r.from_node_id.as_str()))
            .map(|mut r| {
                if r.file_path.is_none() {
                    r.file_path = Some(file_path.to_string());
                }
                if r.language.is_none() {
                    r.language = Some(language);
                }
                r
            })
            .collect();

        let record = FileRecord {
            path: file_path.to_string(),
            content_hash,
            language,
            size: stats.size,
            modified_at: stats.mtime_ms.floor() as i64,
            indexed_at: codegraph_db::now_ms(),
            node_count: node_count as u32,
            errors: if errors.is_empty() { None } else { Some(errors) },
            generated: Some(generated),
        };

        // The whole re-store — deleting the old rows, the new nodes, edges, refs and file record,
        // and the re-attached incoming edges — lands in ONE transaction. Committed separately, a
        // crash after the delete left the file looking new (no snapshot next time) and a crash
        // before the re-attach left it looking current: either way other files' edges into it were
        // lost for good.
        self.store.run_in_transaction(|| {
            if existing.is_some() {
                self.store.delete_file(file_path)?;
            }
            self.store.store_file_bundle(&FileBundle { nodes: &valid_nodes, edges: &valid_edges, refs: &valid_refs, file: &record })?;
            if !cross_file_incoming.is_empty() {
                self.reattach_cross_file_edges(&cross_file_incoming, &prior_nodes, &valid_nodes)?;
            }
            Ok(())
        })?;
        Ok(())
    }

    /// Re-attach cross-file incoming edges snapshotted before a re-index delete (#899): move each
    /// edge's target to the re-indexed node that replaces it ([`pair_reindexed_nodes`]). Targets
    /// that vanished, or that can't be told apart from a same-named sibling (#2276), are
    /// resurrected as their original unresolved ref (#1240's removal-side counterpart) when the
    /// edge carries its refName stamp, so resolution picks the target afresh.
    fn reattach_cross_file_edges(&self, incoming: &[codegraph_db::resolver_queries::IncomingEdge], prior_nodes: &[NodeIdentity], valid_nodes: &[Node]) -> codegraph_db::Result<()> {
        let next: Vec<NodeIdentity> = valid_nodes.iter().map(identity_of).collect();
        let replacement_of = pair_reindexed_nodes(prior_nodes, &next);
        let mut reinserted: Vec<Edge> = Vec::new();
        let mut resurrected: Vec<UnresolvedReference> = Vec::new();
        for e in incoming {
            if let Some(new_target) = replacement_of.get(&e.edge.target) {
                reinserted.push(Edge {
                    source: e.edge.source.clone(),
                    target: new_target.clone(),
                    kind: e.edge.kind,
                    metadata: e.edge.metadata.clone(),
                    line: e.edge.line,
                    column: e.edge.column,
                    provenance: e.edge.provenance,
                });
            } else if let Some(r) = resurrect_ref_from_dropped_edge(&e.edge, &e.source_file_path, e.source_language) {
                resurrected.push(to_db_reference(&r));
            }
        }
        if !reinserted.is_empty() {
            self.store.insert_edges(&reinserted)?;
        }
        if !resurrected.is_empty() {
            self.store.insert_unresolved_refs_batch(&resurrected)?;
        }
        Ok(())
    }
}
