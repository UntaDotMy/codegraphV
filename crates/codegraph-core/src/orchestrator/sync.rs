//! Incremental sync — `ExtractionOrchestrator.sync`.
//!
//! Change detection is filesystem-based, never git: a (size, mtime) stat pre-filter skips unchanged
//! files, then a content-hash compare confirms real changes. This works in non-git projects and
//! catches committed changes from `git pull`/`checkout`/`merge`/`rebase` that `git status` cannot
//! see.

use crate::file_limits::{max_source_file_bytes, oversize_stamp};
use super::git_state::read_source_or_stamp;
use super::{hash_content, ExtractionOrchestrator, FileStat, IndexPhase, IndexProgress, ProgressFn, SyncResult};
use crate::errors::Result;
use crate::extract::has_grammar_load_failure;
use codegraph_extract::languages::is_source_file;
use codegraph_extract::scan::config::load_extension_overrides;
use codegraph_extract::scan::scan_directory;
use codegraph_resolve::module_importers::{reopen_references_through_removed_files, ChangedFile, ImportReader};
use codegraph_resolve::resurrect_ref::resurrect_ref_from_dropped_edge;
use codegraph_resolve::resolver::to_db_reference;
use codegraph_types::log::log_debug;
use codegraph_types::{FileRecord, Severity, UnresolvedReference};
use indexmap::IndexSet;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

/// Called with a path (and its new content, when there is any) before the sync replaces or deletes
/// what the index holds for it — so a caller can inspect the inputs before their edges cascade.
pub type FileChangeFn<'a> = &'a mut dyn FnMut(&str, Option<&str>);

#[derive(Default)]
struct Reconcile {
    pairs_before: IndexSet<String>,
    removed_file_paths: Vec<String>,
    files_removed: usize,
}

impl ExtractionOrchestrator {
    /// Delete a tracked file the sync found gone, first resurrecting the references other files
    /// made INTO it.
    fn remove_tracked(&self, tracked: &FileRecord, state: &mut Reconcile, on_file_change: &mut Option<FileChangeFn<'_>>) -> Result<()> {
        // Before the cascade deletes them, resurrect incoming cross-file resolution edges as their
        // original refs (#1240 removal case): the callers live in files this sync will NOT revisit,
        // so this is their only chance to rebind to an alternative definition — or to park as
        // failed until the symbol reappears somewhere. (A deleted file whose CALLERS are also being
        // deleted is fine: their nodes cascade later in this loop and take the resurrected rows
        // with them.) Every name this file defined is about to stop existing here, which narrows
        // the candidate set for that name repo-wide (CG-33).
        state.pairs_before.extend(self.store.get_node_name_pairs_by_files(std::slice::from_ref(&tracked.path))?);
        let incoming = self.store.get_cross_file_incoming_edges_with_target(&tracked.path)?;
        if !incoming.is_empty() {
            let resurrected: Vec<UnresolvedReference> = incoming
                .iter()
                .filter_map(|e| resurrect_ref_from_dropped_edge(&e.edge, &e.source_file_path, e.source_language))
                .map(|r| to_db_reference(&r))
                .collect();
            if !resurrected.is_empty() {
                self.store.insert_unresolved_refs_batch(&resurrected)?;
            }
        }
        if let Some(cb) = on_file_change.as_deref_mut() {
            cb(&tracked.path, None);
        }
        self.store.delete_file(&tracked.path)?;
        state.removed_file_paths.push(tracked.path.clone());
        state.files_removed += 1;
        Ok(())
    }

    /// Sync the index with the current file state.
    ///
    /// `scoped_paths` is the watcher fast path: the exact project-relative paths the OS reported as
    /// changed. When provided, reconciliation runs over ONLY these paths — per-path logic identical
    /// to the full walk (stat pre-filter, hash confirm, the #1240 removal/resurrection flow) —
    /// skipping the O(repo) scan and tracked-load. Callers must pass `None` whenever the change set
    /// is not exactly known (directory removals, event overflow): the full scan-diff remains the
    /// ground truth those cases need (#1285).
    ///
    /// `backpressure` is the writer-side WAL pressure valve (#1539), called after every changed
    /// file is stored, when no extraction transaction is open.
    pub fn sync(&self, mut on_progress: Option<ProgressFn<'_>>, scoped_paths: Option<&[String]>, backpressure: Option<&dyn Fn()>, mut on_file_change: Option<FileChangeFn<'_>>) -> Result<SyncResult> {
        let start = Instant::now();
        let root = Path::new(&self.root_dir);
        let mut emit = |p: IndexProgress| {
            if let Some(cb) = on_progress.as_deref_mut() {
                cb(&p);
            }
        };
        let files_checked;
        let mut files_added = 0usize;
        let mut files_modified = 0usize;
        let mut nodes_updated = 0usize;
        let mut changed_file_paths: Vec<String> = Vec::new();
        let mut added_file_paths: Vec<String> = Vec::new();
        // `file\0name` definition pairs for the files this sync touches, sampled BEFORE their nodes
        // are replaced/deleted. Compared against the post-store pairs below to derive
        // `definition_delta` (CG-33).
        let mut state = Reconcile::default();
        // Importers' import statements, read once per sync for the files it removes.
        let import_reader = ImportReader::new(&self.root_dir);

        emit(IndexProgress::new(IndexPhase::Scanning, 0, 0));

        let mut files_to_index: Vec<String> = Vec::new();
        let mut failed_file_paths: Vec<String> = Vec::new();
        // === Filesystem reconcile (git-independent) ===
        // The source of truth for "what changed" is the filesystem vs the indexed state — never
        // git. We enumerate the current source files and reconcile each against the DB. A cheap
        // (size, mtime) stat pre-filter skips unchanged files without reading or hashing them, so
        // the expensive read+hash+parse only runs for files that actually changed.
        let timings = std::env::var_os("CODEGRAPHV_SYNTH_TIMINGS").is_some();
        let t_scan = Instant::now();
        let current_files: Vec<String>;
        let tracked_files: Vec<FileRecord>;
        match scoped_paths.filter(|p| !p.is_empty()) {
            Some(paths) => {
                // Scoped reconcile: stat only the reported paths. files_checked counts the PATHS
                // examined (not the files found) — it must stay non-zero even when every scoped
                // path was a deletion, because `CodeGraph::watch()` reads `files_checked == 0 &&
                // duration_ms == 0` as the lock-unavailable signature (#449).
                let unique: IndexSet<&String> = paths.iter().collect();
                // A scoped path is "present" only if it exists AND is in scope — the same two gates
                // the full walk applies (source extension, scope matcher). Without the scope gate a
                // caller's stale view of scope leaked straight into the index: the watcher
                // re-parsed a file the user had just excluded in `codegraph.json` while `codegraph
                // sync` removed it (#1590). Out-of-scope paths fall out of `current_files`, so a
                // tracked one takes the removal branch below, exactly as a full sync would treat
                // it. (`include`-forced paths pass: ScopeIgnore applies the include precedence.)
                let scope = self.scoped_sync_matcher();
                let overrides = load_extension_overrides(root);
                current_files = unique
                    .iter()
                    .filter(|p| is_source_file(p, Some(&overrides), Some(root)) && !scope.ignores(p).unwrap_or(false) && root.join(p.as_str()).exists())
                    .map(|p| (*p).clone())
                    .collect();
                let mut tracked = Vec::new();
                for p in &unique {
                    if let Some(rec) = self.store.get_file_by_path(p)? {
                        tracked.push(rec);
                    }
                }
                tracked_files = tracked;
                files_checked = unique.len();
                if timings {
                    eprintln!("[phase-timing] sync-scoped: {}ms ({} paths, {} tracked)", t_scan.elapsed().as_millis(), unique.len(), tracked_files.len());
                }
            }
            None => {
                // Full reconcile: drop the memoized scope matcher so a nested `.gitignore` /
                // exclude-standard change that forced this full sync is visible to the next scoped
                // sync (#1728).
                self.clear_scoped_matcher();
                current_files = scan_directory(root, None, None).map_err(crate::errors::CodeGraphError::other)?;
                if timings {
                    eprintln!("[phase-timing] sync-scan: {}ms ({} files)", t_scan.elapsed().as_millis(), current_files.len());
                }
                files_checked = current_files.len();
                // Full reconcile only (scoped syncs must not touch rows outside their scope): drop
                // zero-node rows so the wiped files re-index as adds below.
                self.heal_zero_node_rows()?;
                tracked_files = self.store.get_all_files()?;
            }
        }
        let current_set: HashSet<&str> = current_files.iter().map(String::as_str).collect();
        let tracked_map: HashMap<&str, &FileRecord> = tracked_files.iter().map(|f| (f.path.as_str(), f)).collect();

        // Removals: tracked in the DB but no longer a present source file. Check the filesystem
        // directly — `scan_directory` (via `git ls-files`) still lists a file deleted from disk but
        // not yet staged, so set membership alone misses it.
        let removed: Vec<&FileRecord> = tracked_files.iter().filter(|t| !current_set.contains(t.path.as_str()) || !root.join(&t.path).exists()).collect();
        // The references other files make THROUGH the removed ones to files that stay (a package
        // `__init__.py` or an `index.ts` re-exporting what it imports) go back to pending too: the
        // cascade keeps their edges, and a fresh index of the tree without these files binds them
        // differently or not at all. Before any of them is deleted, and for all of them at once.
        if !removed.is_empty() {
            let t_importers = Instant::now();
            let gone: Vec<ChangedFile> = removed.iter().map(|t| ChangedFile { path: t.path.clone(), language: t.language }).collect();
            let reopened = reopen_references_through_removed_files(&self.store, &import_reader, &gone)?;
            if timings {
                eprintln!("[phase-timing] sync-removed-module-importers: {}ms ({reopened} refs re-opened)", t_importers.elapsed().as_millis());
            }
        }
        for tracked in &removed {
            self.remove_tracked(tracked, &mut state, &mut on_file_change)?;
        }

        // Adds / modifications.
        for file_path in &current_files {
            let full_path = root.join(file_path);
            let tracked = tracked_map.get(file_path.as_str()).copied();
            // A row an older engine stored while the file's grammar could not load (#2335) holds no
            // parse of these bytes: re-index it even though its size, mtime and hash all match.
            // Rows with a real parse error are deterministic and are not retried.
            let never_parsed = tracked.is_some_and(|t| t.errors.as_deref().is_some_and(has_grammar_load_failure));

            // Cheap pre-filter: an already-indexed file whose size AND mtime both match the DB is
            // unchanged — skip it without reading or hashing. (A content change that preserves both
            // exactly is the blind spot every mtime-based incremental tool accepts; `index --force`
            // is the escape hatch. Git bumps mtime on every file it writes during checkout/merge,
            // so pulls are caught.)
            if let Some(t) = tracked.filter(|_| !never_parsed) {
                match FileStat::of(&full_path) {
                    // The row must also agree with the CURRENT size limit: a file stored as skipped
                    // (oversize stamp) is re-read once the limit is raised past it, and an indexed
                    // one once the limit drops below it, even though its size and mtime match.
                    Ok(stat)
                        if stat.size == t.size
                            && stat.mtime_ms.floor() as i64 == t.modified_at
                            && (t.content_hash == hash_content(&oversize_stamp(t.size))) == (stat.size > max_source_file_bytes()) =>
                    {
                        continue
                    }
                    Ok(_) => {}
                    Err(error) => {
                        log_debug("Skipping unstattable file during sync", &[("filePath", file_path.clone()), ("error", error.to_string())]);
                        failed_file_paths.push(file_path.clone());
                        continue;
                    }
                }
            }

            // New, or size/mtime changed — read + hash to confirm a real content change. (An
            // oversize file hashes as its size stamp, unread — #1910.)
            let content = match read_source_or_stamp(&full_path, file_path) {
                Ok(c) => c,
                Err(error) => {
                    log_debug("Skipping unreadable file during sync", &[("filePath", file_path.clone()), ("error", error.to_string())]);
                    failed_file_paths.push(file_path.clone());
                    continue;
                }
            };
            // Not source after all — an MPEG transport stream named `.ts` (#1910): a new one is
            // ignored, a tracked file that became one is removed.
            let Some(content) = content else {
                if let Some(t) = tracked {
                    reopen_references_through_removed_files(&self.store, &import_reader, &[ChangedFile { path: t.path.clone(), language: t.language }])?;
                    self.remove_tracked(t, &mut state, &mut on_file_change)?;
                }
                continue;
            };
            let content_hash = hash_content(&content);

            match tracked {
                None => {
                    if let Some(cb) = on_file_change.as_deref_mut() {
                        cb(file_path, Some(&content));
                    }
                    files_to_index.push(file_path.clone());
                    changed_file_paths.push(file_path.clone());
                    added_file_paths.push(file_path.clone());
                    files_added += 1;
                }
                Some(t) if t.content_hash != content_hash || never_parsed => {
                    if let Some(cb) = on_file_change.as_deref_mut() {
                        cb(file_path, Some(&content));
                    }
                    files_to_index.push(file_path.clone());
                    changed_file_paths.push(file_path.clone());
                    files_modified += 1;
                }
                Some(_) => {}
            }
        }

        // Sampled here — after the add/modify classification, before any file is re-extracted —
        // because `store_extraction_result` deletes a file's nodes before inserting the new ones,
        // so this is the last point the pre-edit definition set is readable (CG-33).
        if !files_to_index.is_empty() {
            state.pairs_before.extend(self.store.get_node_name_pairs_by_files(&files_to_index)?);
        }

        // The frameworks every file below is extracted with, current (#2516).
        if !added_file_paths.is_empty() || !state.removed_file_paths.is_empty() {
            self.detection_file_list_changed.set(true);
        }
        let t_detect = Instant::now();
        let detected_frameworks = if files_to_index.is_empty() {
            None
        } else {
            let scanned = if scoped_paths.is_some_and(|p| !p.is_empty()) { None } else { Some(current_files.as_slice()) };
            self.refresh_framework_detection(scanned, &files_to_index, &added_file_paths)
        };
        if timings && !files_to_index.is_empty() {
            eprintln!("[phase-timing] sync-framework-detect: {}ms ({})", t_detect.elapsed().as_millis(), if detected_frameworks.is_some() { "detected again" } else { "unchanged" });
        }

        // Index changed files
        let total = files_to_index.len();
        for (i, file_path) in files_to_index.iter().enumerate() {
            emit(IndexProgress::with_file(IndexPhase::Parsing, i + 1, total, file_path));
            let result = self.index_file(file_path)?;
            if result.errors.iter().any(|e| e.severity == Severity::Error) {
                failed_file_paths.push(file_path.clone());
            }
            if !has_grammar_load_failure(&result.errors) {
                nodes_updated += result.nodes.len(); // else nothing was stored (#2335)
            }
            if let Some(pause) = backpressure {
                pause();
            }
        }

        // Names whose definition set this sync changed: a `file\0name` pair present before but not
        // after (removed/renamed away) or after but not before (added). A pair on both sides is
        // untouched as far as resolution's candidate set is concerned — only its node id moved,
        // which `reattach_cross_file_edges` already follows — so an edit that only changes bodies
        // yields an empty delta and no downstream rebind work (CG-33).
        //
        // Compared per FILE, not as one name set over the whole batch: a commit that adds `collect`
        // to a new file while an unrelated changed file already defined `collect` must still flag
        // the name, and a bare name set cancels exactly that case out.
        let pairs_after: IndexSet<String> = self.store.get_node_name_pairs_by_files(&files_to_index)?.into_iter().collect();
        let mut delta_names: IndexSet<String> = IndexSet::new();
        let name_of = |pair: &str| pair[pair.find('\0').map_or(0, |i| i + 1)..].to_string();
        for pair in &state.pairs_before {
            if !pairs_after.contains(pair) {
                delta_names.insert(name_of(pair));
            }
        }
        for pair in &pairs_after {
            if !state.pairs_before.contains(pair) {
                delta_names.insert(name_of(pair));
            }
        }

        let non_empty = |v: Vec<String>| if v.is_empty() { None } else { Some(v) };
        Ok(SyncResult {
            files_checked,
            files_added,
            files_modified,
            files_removed: state.files_removed,
            nodes_updated,
            duration_ms: start.elapsed().as_millis() as u64,
            changed_file_paths: non_empty(changed_file_paths),
            added_file_paths: non_empty(added_file_paths),
            failed_file_paths: non_empty(failed_file_paths),
            definition_delta: non_empty(delta_names.into_iter().collect()),
            removed_file_paths: non_empty(state.removed_file_paths),
            detected_frameworks,
            ..SyncResult::default()
        })
    }
}
