//! JSON-lines oracle probe for git hooks and worktree diagnostics.
use codegraph_sync::{git_hooks::*, worktree::*};
use serde_json::{json, Value};
use std::io::{self, BufRead};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    for line in io::stdin().lock().lines() {
        let q: Value = serde_json::from_str(&line?)?;
        let root = q["root"].as_str().unwrap_or("");
        let hooks: Option<Vec<GitHookName>> = q
            .get("hooks")
            .map(|v| serde_json::from_value(v.clone()).unwrap());
        let out = match q["op"].as_str().unwrap() {
            "install" => json!(install_git_sync_hook(root, hooks.as_deref())?),
            "remove" => json!(remove_git_sync_hook(root, hooks.as_deref())?),
            "is" => json!(is_sync_hook_installed(root, hooks.as_deref())?),
            "repo" => json!(is_git_repo(root)),
            "worktree" => json!(git_worktree_root(root)),
            "common" => json!(git_common_dir(root)),
            "mismatch" => json!(detect_worktree_index_mismatch(
                root,
                q["index"].as_str().unwrap()
            )),
            "nested" => json!(nested_repository_below(root, q["index"].as_str().unwrap())),
            "warning" | "notice" => {
                let m = WorktreeIndexMismatch {
                    worktree_root: root.into(),
                    index_root: q["index"].as_str().unwrap().into(),
                };
                json!(if q["op"] == "warning" {
                    worktree_mismatch_warning(&m)
                } else {
                    worktree_mismatch_notice(&m)
                })
            }
            _ => panic!("unknown operation"),
        };
        println!("{out}");
    }
    Ok(())
}
