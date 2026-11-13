//! `sync` on an index built by an older engine (upstream 0603ad9e, #1842): sync re-reads changed
//! files only, so it still syncs and exits 0 (the git hooks run it with output discarded), but it
//! warns that a full `codegraphV index` is needed. `--quiet` stays silent.
use codegraph_core::CodeGraph;
use std::{path::Path, process::Command};

struct Out {
    stdout: String,
    stderr: String,
}

fn run(root: &Path, args: &[&str]) -> Out {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_codegraphV"))
        .args(args)
        .current_dir(root)
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("LOCALAPPDATA", home.path())
        .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{args:?}: {stdout}\n{stderr}");
    Out { stdout, stderr }
}

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.js"), "function a() { return 1; }\n").unwrap();
    run(dir.path(), &["init", "--yes"]);
    dir
}

/// Stamp the index as built by `version` (None blanks the stamp, which reads as missing).
fn stamp(root: &Path, version: Option<&str>) {
    let cg = CodeGraph::open(&root.to_string_lossy(), Default::default()).unwrap();
    cg.store().set_metadata("indexed_with_extraction_version", version.unwrap_or("")).unwrap();
}

fn has_symbol(root: &Path, name: &str) -> bool {
    let cg = CodeGraph::open(&root.to_string_lossy(), Default::default()).unwrap();
    cg.store().get_nodes_by_name(name).map(|n| !n.is_empty()).unwrap_or(false)
}

const WARNING: &str = "codegraphV index";

#[test]
fn an_old_stamp_warns_and_still_syncs_a_changed_file() {
    let dir = project();
    stamp(dir.path(), Some("1"));
    std::fs::write(dir.path().join("b.js"), "function freshlyAdded() { return 2; }\n").unwrap();
    let out = run(dir.path(), &["sync"]);
    assert!(out.stderr.contains(WARNING), "stderr: {}", out.stderr);
    assert!(out.stderr.contains("older"), "stderr: {}", out.stderr);
    serde_json::from_str::<serde_json::Value>(&out.stdout).expect("stdout stays JSON");
    assert!(has_symbol(dir.path(), "freshlyAdded"));
}

#[test]
fn a_missing_stamp_warns_with_nothing_changed() {
    let dir = project();
    stamp(dir.path(), None);
    let out = run(dir.path(), &["sync"]);
    assert!(out.stderr.contains(WARNING), "stderr: {}", out.stderr);
}

#[test]
fn quiet_stays_silent_on_a_stale_index_and_still_syncs() {
    let dir = project();
    stamp(dir.path(), Some("1"));
    std::fs::write(dir.path().join("b.js"), "function quietAdded() { return 2; }\n").unwrap();
    let out = run(dir.path(), &["sync", "--quiet"]);
    assert_eq!(out.stdout, "");
    assert_eq!(out.stderr, "");
    assert!(has_symbol(dir.path(), "quietAdded"));
}

#[test]
fn a_current_index_does_not_warn_and_a_rebuild_clears_the_warning() {
    let dir = project();
    assert!(!run(dir.path(), &["sync"]).stderr.contains(WARNING));
    stamp(dir.path(), Some("1"));
    assert!(run(dir.path(), &["sync"]).stderr.contains(WARNING));
    run(dir.path(), &["index"]);
    assert!(!run(dir.path(), &["sync"]).stderr.contains(WARNING));
}
