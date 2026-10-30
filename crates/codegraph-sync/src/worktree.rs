//! Detect a borrowed worktree index without confusing it with an embedded repo.
use crate::{git, resolved};
use codegraph_types::nodepath;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub fn git_worktree_root(dir: &str) -> Option<String> {
    git(dir, &["rev-parse", "--show-toplevel"])
        .filter(|s| !s.is_empty())
        .map(|s| resolved(&s))
}
pub fn git_common_dir(dir: &str) -> Option<String> {
    git(dir, &["rev-parse", "--git-common-dir"])
        .filter(|s| !s.is_empty())
        .map(|s| resolved(&nodepath::resolve(&[dir, &s])))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeIndexMismatch {
    pub worktree_root: String,
    pub index_root: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedRepository {
    pub root: String,
    pub rel_path: String,
}

/// The filesystem's own name for `p` (the native realpath), falling back to the lexical resolve and
/// links-only realpath where the native call fails.
fn native_real_path(p: &str) -> String {
    codegraph_types::paths::real_path(Path::new(&nodepath::resolve(&[p])))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| resolved(p))
}

/// Whether two paths name one directory, however each is spelled. For comparing only: the paths
/// this module returns keep their own spelling.
///
/// git answers with the on-disk spelling, while callers hand in theirs: an editor passes a
/// lowercase drive letter, NTFS ignores case, a `subst` drive is a second name for a folder. Compared
/// as strings one directory looked like two, so a linked worktree spelled unlike git lost the
/// borrowed-index warning and was refused as a repository of its own (upstream #2937). The identity
/// is `canonical_project_root`'s: the native realpath, lowercased on Windows.
fn same_directory(a: &str, b: &str) -> bool {
    a == b || directory_identity(a) == directory_identity(b)
}

fn directory_identity(p: &str) -> String {
    let real = native_real_path(p);
    if cfg!(windows) {
        real.to_lowercase()
    } else {
        real
    }
}

/// The directory holding the `.git` (directory or worktree file) nearest at or above `dir`.
fn nearest_git_root(dir: &str) -> Option<String> {
    let mut cur = nodepath::resolve(&[dir]);
    loop {
        if Path::new(&cur).join(".git").exists() {
            return Some(cur);
        }
        let up = nodepath::dirname(&cur);
        if up == cur {
            return None;
        }
        cur = up;
    }
}

pub fn detect_worktree_index_mismatch(start: &str, index: &str) -> Option<WorktreeIndexMismatch> {
    // Every tool call asks this; spawning git costs ~45 ms on Windows. Without a GIT_DIR override,
    // the nearest `.git` is the worktree git would report: no repository, or the index's own
    // root, cannot mismatch.
    if std::env::var_os("GIT_DIR").is_none() {
        match nearest_git_root(start) {
            None => return None,
            Some(root) if same_directory(&root, index) => return None,
            Some(_) => {}
        }
    }
    let worktree_root = git_worktree_root(start)?;
    let index_root = resolved(index);
    if same_directory(&worktree_root, &index_root) {
        return None;
    }
    // Only a real working-tree root counts: "borrowed another worktree's index" is not "index sits
    // in a plain ancestor directory".
    if !git_worktree_root(&index_root).is_some_and(|r| same_directory(&r, &index_root)) {
        return None;
    }
    if let (Some(worktree_common), Some(index_common)) =
        (git_common_dir(&worktree_root), git_common_dir(&index_root))
    {
        if !same_directory(&worktree_common, &index_common) {
            return None;
        }
    }
    Some(WorktreeIndexMismatch {
        worktree_root,
        index_root,
    })
}

pub fn nested_repository_below(start: &str, index: &str) -> Option<NestedRepository> {
    let mut dir = nodepath::resolve(&[start]);
    while !Path::new(&dir).is_dir() {
        let up = nodepath::dirname(&dir);
        if up == dir {
            return None;
        }
        dir = up;
    }
    let root = git_worktree_root(&dir)?;
    let rel = nodepath::relative(&native_real_path(index), &native_real_path(&root));
    if rel.is_empty()
        || rel == ".."
        || rel.starts_with(&format!("..{}", nodepath::SEP))
        || Path::new(&rel).is_absolute()
    {
        return None;
    }
    let common = git_common_dir(&root)?;
    // An index root outside git has no repository of its own, so what is below it is still reported.
    if git_common_dir(&resolved(index)).is_some_and(|index_common| same_directory(&common, &index_common)) {
        return None;
    }
    Some(NestedRepository {
        root,
        rel_path: rel.replace(nodepath::SEP, "/"),
    })
}

pub fn worktree_mismatch_warning(m: &WorktreeIndexMismatch) -> String {
    format!("This CodeGraphV index belongs to a different git working tree.\n  Running in: {}\n  Index from: {}\nResults reflect that tree's code (often a different branch), not this worktree — symbols changed only here are missing. Run \"codegraphV init -i\" in this worktree for a worktree-local index.",m.worktree_root,m.index_root)
}
pub fn worktree_mismatch_notice(m: &WorktreeIndexMismatch) -> String {
    format!("⚠ CodeGraphV results below come from a different git worktree ({}), not where you're working ({}) — they may reflect another branch, and symbols changed only here are missing. Run \"codegraphV init -i\" here for a worktree-local index.",m.index_root,m.worktree_root)
}
