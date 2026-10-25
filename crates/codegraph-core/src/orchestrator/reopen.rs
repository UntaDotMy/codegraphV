//! Re-opening resolution edges whose answer a sync may have changed — the `resurrect*` half of
//! `ExtractionOrchestrator`. An edge goes back to the reference that created it (the
//! refName/refKind stamp), `pending`, for the sync's resolution sweep to bind against the
//! post-sync graph — the same input a full rebuild resolves from, which is what makes the two
//! converge (CG-33).

use super::ExtractionOrchestrator;
use crate::errors::Result;
use codegraph_db::sync_queries::ResolutionEdge;
use codegraph_resolve::resolver::to_db_reference;
use codegraph_resolve::resurrect_ref::resurrect_ref_from_dropped_edge;
use codegraph_types::{Language, UnresolvedReference};
use indexmap::{IndexMap, IndexSet};
use std::collections::{HashMap, HashSet};

/// The per-name ceiling `getResolutionEdgesByTargetName` defaults to.
const DEFAULT_PER_NAME_CEILING: i64 = 500;

fn resurrect(e: &ResolutionEdge) -> Option<UnresolvedReference> {
    resurrect_ref_from_dropped_edge(&e.edge, &e.source_file_path, e.source_language).map(|r| to_db_reference(&r))
}

impl ExtractionOrchestrator {
    /// Re-open, for re-resolution, every resolution edge whose answer this sync may have changed —
    /// the fix for index drift (CG-33).
    ///
    /// Incremental sync re-resolves only the references IN the changed files, but resolution's
    /// answer is a function of the WHOLE graph: a reference binds to one of the same-named
    /// definitions project-wide, so adding or removing a definition of `pct` can change which `pct`
    /// every other file's `pct(...)` should bind to. Those other files are never revisited, and
    /// their references resolved successfully once and were deleted from `unresolved_refs`, so
    /// nothing existed to revisit them with.
    ///
    /// Deliberately conservative in three ways, because a wrong deletion is a permanent edge loss
    /// while a missed rebind is only residual drift:
    /// - an edge with no refName stamp (synthesized, or built by an engine older than the stamp)
    ///   is left ALONE rather than reconstructed from the target's plain name;
    /// - edges whose source is in a file this sync already re-extracted are skipped — their
    ///   references were re-resolved from scratch moments ago;
    /// - very common names are skipped by the per-name ceiling in
    ///   `get_resolution_edges_by_target_name`, counted over the edges up to `counted_up_to_edge_id`
    ///   when given (those before the sync's failed-ref retry).
    ///
    /// Returns the number of references resurrected.
    pub fn resurrect_stale_resolution_edges(&self, definition_delta: &[String], changed_file_paths: &[String], counted_up_to_edge_id: Option<i64>) -> Result<usize> {
        if definition_delta.is_empty() {
            return Ok(0);
        }
        let already_fresh: HashSet<&str> = changed_file_paths.iter().map(String::as_str).collect();
        let candidates = self.store.get_resolution_edges_by_target_name(definition_delta, DEFAULT_PER_NAME_CEILING, counted_up_to_edge_id)?;

        let mut edge_ids: Vec<i64> = Vec::new();
        let mut refs: Vec<UnresolvedReference> = Vec::new();
        for e in &candidates {
            if already_fresh.contains(e.source_file_path.as_str()) {
                continue;
            }
            let Some(r) = resurrect(e) else { continue }; // no stamp — never delete what we cannot restore
            edge_ids.push(e.edge_id);
            refs.push(r);
        }
        if refs.is_empty() {
            return Ok(0);
        }
        // Delete first. The sweep re-inserts whichever edge resolution now picks, and
        // `insert_edges` is INSERT OR IGNORE against idx_edges_identity — so a rebind to the same
        // target is a clean no-op, but leaving the old row in place for a rebind ELSEWHERE would
        // keep both, turning drift into duplication.
        self.store.replace_resolution_edges_with_unresolved_refs(&edge_ids, &refs)?;
        Ok(refs.len())
    }

    /// Re-open resolution edges whose answer was READ from a file this sync touched, although
    /// neither end of the edge changed: a python attribute typed through a factory or a base class
    /// in another file records those files as `metadata.typeFrom`. The definition delta cannot see
    /// these — editing what a factory returns, or what a base `__init__` assigns, adds or removes
    /// no definition. The edges go back to pending refs; returns the files they come from, whose
    /// pending rows the caller resolves.
    pub fn resurrect_type_dependent_edges(&self, touched_paths: &[String], changed_file_paths: &[String]) -> Result<Vec<String>> {
        let touched: HashSet<&str> = touched_paths.iter().map(String::as_str).filter(|p| p.ends_with(".py") || p.ends_with(".pyi")).collect();
        if touched.is_empty() {
            return Ok(Vec::new());
        }
        let already_fresh: HashSet<&str> = changed_file_paths.iter().map(String::as_str).collect();

        let mut edge_ids: Vec<i64> = Vec::new();
        let mut refs: Vec<UnresolvedReference> = Vec::new();
        let mut files: IndexSet<String> = IndexSet::new();
        for e in self.store.get_edges_with_type_from()? {
            if already_fresh.contains(e.source_file_path.as_str()) {
                continue;
            }
            let from = e.edge.metadata.as_ref().and_then(|m| m.get("typeFrom")).and_then(|v| v.as_array());
            let Some(from) = from else { continue };
            if !from.iter().any(|f| f.as_str().is_some_and(|s| touched.contains(s))) {
                continue;
            }
            let Some(r) = resurrect(&e) else { continue };
            edge_ids.push(e.edge_id);
            refs.push(r);
            files.insert(e.source_file_path.clone());
        }
        if refs.is_empty() {
            return Ok(Vec::new());
        }
        self.store.replace_resolution_edges_with_unresolved_refs(&edge_ids, &refs)?;
        Ok(files.into_iter().collect())
    }

    /// Re-open every resolution a language made, for the sync's sweep to redo against the current
    /// graph — the answer when an input to that language's resolution changed project-wide rather
    /// than per definition. Python package roots are that kind of input: an added `__init__.py` or
    /// an edited `pyproject.toml` moves where modules start for importers in files the sync never
    /// touched, and CG-33's per-name rebind can't see it. Parked failures are re-queued too, since
    /// some of them now resolve.
    pub fn resurrect_resolution_edges_for_language(&self, language: Language) -> Result<usize> {
        let edges = self.store.get_resolution_edges_by_source_language(language)?;
        Ok(self.reopen_edges(&edges)? + self.store.requeue_failed_references_by_language(language, None, None)?)
    }

    /// The narrow form, for modules or packages that appeared or vanished below an unchanged set
    /// of roots: re-open the edges whose target lies in `target_files` or whose source lies under
    /// `source_dirs`, and re-queue the parked failures naming one of `module_leaves`. Returns the
    /// files whose references were re-opened.
    pub fn resurrect_resolution_edges_touching(&self, language: Language, target_files: &[String], source_dirs: &[String], module_leaves: &[String]) -> Result<Vec<String>> {
        let edges = self.store.get_resolution_edges_touching_files(language, target_files, source_dirs, module_leaves)?;
        let mut files: HashSet<String> = edges.iter().map(|e| e.source_file_path.clone()).collect();
        self.reopen_edges(&edges)?;
        self.store.requeue_failed_references_by_language(language, Some(module_leaves), Some(&mut files))?;
        // `files` is a set in the TypeScript too; its iteration order is insertion order.
        let mut ordered: IndexSet<String> = IndexSet::new();
        for e in &edges {
            ordered.insert(e.source_file_path.clone());
        }
        ordered.extend(files);
        Ok(ordered.into_iter().collect())
    }

    /// Re-open what importers bound from a Python package whose `__init__.py` was edited: in each
    /// importing file, the edges and parked failures whose reference starts with a name bound from
    /// that package (`N`, `N.run`, `pkg.N`). A def or re-export added, removed or repointed there
    /// moves exactly those answers — `from pkg import N` prefers what `__init__.py` binds over a
    /// submodule `pkg/N` — and nothing in the importer's other references. A name whose import edge
    /// already lands where the package binds it now is left alone, so an edit that moves nothing
    /// re-resolves nothing. Returns the files whose references were re-opened.
    pub fn resurrect_python_package_importers(&self, bindings: &IndexMap<String, IndexMap<String, Option<String>>>) -> Result<Vec<String>> {
        if bindings.is_empty() {
            return Ok(Vec::new());
        }
        let files: Vec<String> = bindings.keys().cloned().collect();
        let edges = self.store.get_resolution_edges_from_files(Language::Python, &files)?;
        let ref_name = |e: &ResolutionEdge| e.edge.metadata.as_ref().and_then(|m| m.get("refName")).and_then(|v| v.as_str()).map(str::to_string);
        // Where each bound name's own import edge lands today.
        let mut binds_now: HashMap<String, String> = HashMap::new();
        for e in &edges {
            let Some(name) = ref_name(e) else { continue };
            if e.edge.kind != codegraph_types::EdgeKind::Imports || name.contains('.') {
                continue;
            }
            if let Some(target) = self.store.get_node_by_id(&e.edge.target)? {
                let leaf = if target.kind == codegraph_types::NodeKind::File { "" } else { target.name.as_str() };
                binds_now.insert(format!("{}\0{}", e.source_file_path, name), format!("{}\0{}", target.file_path, leaf));
            }
        }
        let mut moved: IndexMap<String, HashSet<String>> = IndexMap::new();
        for (file, names) in bindings {
            for (name, binding) in names {
                if let Some(b) = binding {
                    if binds_now.get(&format!("{file}\0{name}")) == Some(b) {
                        continue;
                    }
                }
                moved.entry(file.clone()).or_default().insert(name.clone());
            }
        }
        if moved.is_empty() {
            return Ok(Vec::new());
        }
        let to_reopen: Vec<ResolutionEdge> = edges
            .iter()
            .filter(|e| {
                let head = ref_name(e).map(|n| n.split('.').next().unwrap_or("").to_string()).unwrap_or_default();
                moved.get(&e.source_file_path).is_some_and(|set| set.contains(&head))
            })
            .cloned()
            .collect();
        self.reopen_edges(&to_reopen)?;
        self.store.requeue_failed_references_binding(Language::Python, &moved)?;
        Ok(moved.keys().cloned().collect())
    }

    fn reopen_edges(&self, edges: &[ResolutionEdge]) -> Result<usize> {
        let mut edge_ids: Vec<i64> = Vec::new();
        let mut refs: Vec<UnresolvedReference> = Vec::new();
        for e in edges {
            let Some(r) = resurrect(e) else { continue };
            edge_ids.push(e.edge_id);
            refs.push(r);
        }
        if !refs.is_empty() {
            self.store.replace_resolution_edges_with_unresolved_refs(&edge_ids, &refs)?;
        }
        Ok(refs.len())
    }
}
