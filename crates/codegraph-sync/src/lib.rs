//! Native live watching, git sync hooks and worktree awareness.
pub mod git_hooks;
pub mod watch_policy;
pub mod watcher;
pub mod worktree;

pub(crate) fn git(root: &str, args: &[&str]) -> Option<String> {
    codegraph_extract::scan::git::run_git(
        std::path::Path::new(root),
        args,
        std::time::Duration::from_secs(5),
        1024 * 1024,
    )
    .ok()
    .map(|s| codegraph_types::jscompat::js_trim(&s).to_string())
}

pub(crate) fn resolved(root: &str) -> String {
    let lexical = codegraph_types::nodepath::resolve(&[root]);
    codegraph_types::paths::real_path_preserving_spelling(std::path::Path::new(&lexical))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or(lexical)
}
