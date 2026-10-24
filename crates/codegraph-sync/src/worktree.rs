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
            Some(root) if resolved(&root) == resolved(index) => return None,
            Some(_) => {}
        }
    }
    let worktree_root = git_worktree_root(start)?;
    let index_root = resolved(index);
    if worktree_root == index_root || git_worktree_root(&index_root).as_ref() != Some(&index_root) {
        return None;
    }
    let worktree_common = git_common_dir(&worktree_root);
    let index_common = git_common_dir(&index_root);
    if worktree_common.is_some() && index_common.is_some() && worktree_common != index_common {
        return None;
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
    let native = |p: &str| {
        codegraph_types::paths::real_path(Path::new(&nodepath::resolve(&[p])))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| resolved(p))
    };
    let rel = nodepath::relative(&native(index), &native(&root));
    if rel.is_empty()
        || rel == ".."
        || rel.starts_with(&format!("..{}", nodepath::SEP))
        || Path::new(&rel).is_absolute()
    {
        return None;
    }
    let common = git_common_dir(&root)?;
    if Some(common) == git_common_dir(&resolved(index)) {
        return None;
    }
    Some(NestedRepository {
        root,
        rel_path: rel.replace(nodepath::SEP, "/"),
    })
}

pub fn worktree_mismatch_warning(m: &WorktreeIndexMismatch) -> String {
    format!("This CodeGraph index belongs to a different git working tree.\n  Running in: {}\n  Index from: {}\nResults reflect that tree's code (often a different branch), not this worktree — symbols changed only here are missing. Run \"codegraph init -i\" in this worktree for a worktree-local index.",m.worktree_root,m.index_root)
}
pub fn worktree_mismatch_notice(m: &WorktreeIndexMismatch) -> String {
    format!("⚠ CodeGraph results below come from a different git worktree ({}), not where you're working ({}) — they may reflect another branch, and symbols changed only here are missing. Run \"codegraph init -i\" here for a worktree-local index.",m.index_root,m.worktree_root)
}
