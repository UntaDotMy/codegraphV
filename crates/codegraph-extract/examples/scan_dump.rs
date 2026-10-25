//! `scan_dump <root> [sinceCommit]` — what the Rust scanner finds, as one JSON
//! document (`scripts/rust-golden/scan.mjs` prints the same from the TypeScript build).

use codegraph_extract::scan::*;
use std::path::Path;

fn main() {
    let mut args = std::env::args().skip(1);
    let root = args.next().expect("usage: scan_dump <root> [sinceCommit]");
    let since = args.next();
    let root = Path::new(&root);
    let mut stats = ScanSkipStats::default();
    let files = scan_directory(root, None, Some(&mut stats)).map_err(|e| format!("ERROR: {e}"));
    let changes = get_git_changed_files(root, since.as_deref());
    let out = serde_json::json!({
        "files": match &files { Ok(f) => serde_json::json!(f), Err(e) => serde_json::json!(e) },
        "skipped": stats.unsupported_by_extension.iter().collect::<Vec<_>>(),
        "changes": changes.map(|c| serde_json::json!({ "modified": c.modified, "added": c.added, "deleted": c.deleted })),
        "embedded": discover_embedded_repo_roots(root).ok(),
        "unindexedIgnored": find_unindexed_ignored_repos(root).ok(),
        "head": get_git_head_sha(root),
        "trust": can_trust_git_fast_path(root, since.as_deref()),
        "preload": preload_languages_for_files(files.as_ref().map(|f| f.as_slice()).unwrap_or(&[]), Some(&codegraph_extract::scan::config::load_extension_overrides(root)), Some(root)).iter().map(|l| l.as_str()).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string(&out).unwrap());
}
