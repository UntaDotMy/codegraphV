//! Which files changed since the index was last brought up to date — the git half of
//! `ExtractionOrchestrator` (`beginGitIndexState`, `finishGitIndexState`, `getChangedFiles`).

use super::{hash_content, ExtractionOrchestrator};
use crate::file_limits::{oversize_stamp, read_bounded_source};
use codegraph_extract::languages::{has_mpeg_ts_extension, is_mpeg_transport_stream, is_source_file, MPEG_TS_SNIFF_BYTES};
use codegraph_extract::scan::config::load_extension_overrides;
use codegraph_extract::scan::{can_trust_git_fast_path, get_git_changed_files, get_git_head_sha, scan_directory, INDEXED_AT_COMMIT_KEY};
use codegraph_types::log::log_debug;
use indexmap::IndexSet;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;

const INDEXED_DIRTY_PATHS_KEY: &str = "indexed_dirty_paths";

/// Files that differ from the index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangedFiles {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub removed: Vec<String>,
}

/// The git state captured before a write, so a crashed or partial write can't claim freshness.
#[derive(Debug, Clone)]
pub struct GitIndexSnapshot {
    pub head: String,
    pub stamp: String,
    pub dirty: Option<Vec<String>>,
}

/// Whether these bytes, read from `file_path`, are a `.ts` MPEG transport stream (#1910).
pub(crate) fn is_mpeg_ts_bytes(file_path: &str, bytes: &[u8]) -> bool {
    if !has_mpeg_ts_extension(file_path) || !is_mpeg_transport_stream(&bytes[..bytes.len().min(MPEG_TS_SNIFF_BYTES)]) {
        return false;
    }
    log_debug("Skipping MPEG transport stream named .ts — not TypeScript", &[("filePath", file_path.to_string())]);
    true
}

/// What change detection hashes for a file: its text when it is under the size limit, the size
/// stamp when it is over — an oversize file is never decoded. `None` when the bytes show a `.ts`
/// file is an MPEG transport stream, not TypeScript (#1910).
pub(crate) fn read_source_or_stamp(full_path: &Path, rel_path: &str) -> std::io::Result<Option<String>> {
    let read = read_bounded_source(full_path)?;
    match read.bytes {
        None => Ok(Some(oversize_stamp(read.size))),
        Some(bytes) => {
            if is_mpeg_ts_bytes(rel_path, &bytes) {
                Ok(None)
            } else {
                Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
            }
        }
    }
}

impl ExtractionOrchestrator {
    fn indexed_dirty_paths(&self, stamp: Option<&str>) -> Option<Vec<String>> {
        let raw = self.store.get_metadata(INDEXED_DIRTY_PATHS_KEY).ok().flatten();
        let state: Value = serde_json::from_str(raw.as_deref().unwrap_or("null")).ok()?;
        let object = state.as_object()?;
        if object.get("commit")?.as_str()? != stamp.unwrap_or("") {
            return None;
        }
        let paths = object.get("paths")?.as_array()?;
        let mut out = Vec::with_capacity(paths.len());
        for p in paths {
            let p = p.as_str()?;
            if p.is_empty() || Path::new(p).is_absolute() || p.starts_with('/') || p.split('/').any(|s| s == "..") {
                return None;
            }
            out.push(p.to_string());
        }
        Some(out)
    }

    /// Capture before file reads. In-flight/failed full writes must not claim freshness.
    pub fn begin_git_index_state(&self, full: bool) -> crate::errors::Result<GitIndexSnapshot> {
        let root = Path::new(&self.root_dir);
        let head = get_git_head_sha(root).unwrap_or_default();
        let stamp = self.store.get_metadata(INDEXED_AT_COMMIT_KEY)?.unwrap_or_default();
        let prior = self.indexed_dirty_paths(Some(&stamp));
        let status = get_git_changed_files(root, None);
        let dirty = status.map(|s| {
            let mut set: IndexSet<String> = IndexSet::new();
            if !full {
                set.extend(prior.iter().flatten().cloned());
            }
            set.extend(s.added);
            set.extend(s.modified);
            set.extend(s.deleted);
            set.into_iter().collect::<Vec<_>>()
        });
        if full || prior.is_none() || dirty.is_none() {
            self.store.set_metadata(INDEXED_AT_COMMIT_KEY, "")?;
            self.store.set_metadata(INDEXED_DIRTY_PATHS_KEY, "")?;
        } else {
            // Scoped writes leave the commit alone and retain dirty paths before the first write,
            // so a crash cannot forget an indexed uncommitted edit.
            self.store.set_metadata(INDEXED_DIRTY_PATHS_KEY, &json!({ "commit": stamp, "paths": dirty }).to_string())?;
        }
        Ok(GitIndexSnapshot { head, stamp, dirty })
    }

    pub fn finish_git_index_state(&self, snapshot: &GitIndexSnapshot, full: bool, retries: &[String]) -> crate::errors::Result<()> {
        let after = get_git_changed_files(Path::new(&self.root_dir), None);
        let (Some(dirty), Some(after)) = (&snapshot.dirty, after) else { return Ok(()) };
        let commit = if full { &snapshot.head } else { &snapshot.stamp };
        let mut paths: Vec<String> = dirty
            .iter()
            .chain(after.added.iter())
            .chain(after.modified.iter())
            .chain(after.deleted.iter())
            .chain(retries.iter())
            .cloned()
            .collect::<IndexSet<_>>()
            .into_iter()
            .collect();
        // JavaScript's default sort compares UTF-16 code units.
        paths.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        // The embedded commit makes a torn pair fail closed: readers reject a dirty set that
        // doesn't match the separately stored commit. Write the set first.
        self.store.set_metadata(INDEXED_DIRTY_PATHS_KEY, &json!({ "commit": commit, "paths": paths }).to_string())?;
        if full {
            self.store.set_metadata(INDEXED_AT_COMMIT_KEY, commit)?;
        }
        Ok(())
    }

    /// Get files that have changed since last index. Uses git status as a fast path when
    /// available, falling back to a full scan.
    pub fn get_changed_files(&self) -> crate::errors::Result<ChangedFiles> {
        let root = Path::new(&self.root_dir);
        // The commit this index was last brought up to date at. Absent on an index built before
        // stamping existed — `get_git_changed_files` then declines the fast path and the full scan
        // below answers correctly, once, until a sync or a full index writes the stamp (#1829).
        let since_commit: Option<String> = self.store.get_metadata(INDEXED_AT_COMMIT_KEY).ok().flatten();
        let dirty_paths = self.indexed_dirty_paths(since_commit.as_deref());
        let git_changes = match &dirty_paths {
            Some(_) if can_trust_git_fast_path(root, since_commit.as_deref()) => get_git_changed_files(root, since_commit.as_deref()),
            _ => None,
        };

        if let (Some(git_changes), Some(dirty_paths)) = (git_changes, dirty_paths) {
            // === Git fast path ===
            let mut out = ChangedFiles::default();
            // Git supplies candidates, never the verdict. A committed deletion may have been
            // recreated locally; a previously indexed dirty path may have vanished from git status
            // after restore. Classify current disk vs DB once.
            let candidates: IndexSet<String> = git_changes.deleted.into_iter().chain(git_changes.modified).chain(git_changes.added).chain(dirty_paths).collect();
            let scope = self.scoped_sync_matcher();
            let overrides = load_extension_overrides(root);
            for file_path in candidates {
                let tracked = self.store.get_file_by_path(&file_path)?;
                let full_path = root.join(&file_path);
                if !is_source_file(&file_path, Some(&overrides), Some(root)) || scope.ignores(&file_path).unwrap_or(false) || !full_path.exists() {
                    if tracked.is_some() {
                        out.removed.push(file_path);
                    }
                    continue;
                }
                let content = match read_source_or_stamp(&full_path, &file_path) {
                    Ok(c) => c,
                    Err(error) => {
                        log_debug("Skipping unreadable file while detecting changes", &[("filePath", file_path.clone()), ("error", error.to_string())]);
                        continue;
                    }
                };
                // A `.ts` that is an MPEG transport stream is not source (#1910), so an untracked
                // clip is never pending and a tracked file that became one is gone.
                let Some(content) = content else {
                    if tracked.is_some() {
                        out.removed.push(file_path);
                    }
                    continue;
                };
                match tracked {
                    None => out.added.push(file_path),
                    Some(t) if t.content_hash != hash_content(&content) => out.modified.push(file_path),
                    Some(_) => {}
                }
            }
            return Ok(out);
        }

        // === Fallback: full scan (non-git project or git failure) ===
        let current_files: IndexSet<String> = scan_directory(root, None, None).map_err(crate::errors::CodeGraphError::other)?.into_iter().collect();
        let tracked_files = self.store.get_all_files()?;
        let tracked_map: HashMap<&str, &codegraph_types::FileRecord> = tracked_files.iter().map(|f| (f.path.as_str(), f)).collect();
        let current_set: HashSet<&str> = current_files.iter().map(String::as_str).collect();

        let mut out = ChangedFiles::default();
        // Find removed files
        for tracked in &tracked_files {
            if !current_set.contains(tracked.path.as_str()) {
                out.removed.push(tracked.path.clone());
            }
        }
        // Find added and modified files
        for file_path in &current_files {
            let full_path = root.join(file_path);
            let content = match read_source_or_stamp(&full_path, file_path) {
                Ok(c) => c,
                Err(error) => {
                    log_debug("Skipping unreadable file while detecting changes", &[("filePath", file_path.clone()), ("error", error.to_string())]);
                    continue;
                }
            };
            let tracked = tracked_map.get(file_path.as_str());
            // An MPEG transport stream named `.ts` (#1910) is not source.
            let Some(content) = content else {
                if tracked.is_some() {
                    out.removed.push(file_path.clone());
                }
                continue;
            };
            match tracked {
                None => out.added.push(file_path.clone()),
                Some(t) if t.content_hash != hash_content(&content) => out.modified.push(file_path.clone()),
                Some(_) => {}
            }
        }
        Ok(out)
    }
}
