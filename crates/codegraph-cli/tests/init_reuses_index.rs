//! `init` on a project that already has a complete index built by this engine syncs the changed
//! files instead of rebuilding everything; the watcher and git hooks keep it current from there.
//! A missing, unfinished or older-engine index still gets a full index.
use codegraph_core::CodeGraph;
use std::{path::Path, process::Command};

fn run(root: &Path, args: &[&str]) -> String {
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
    assert!(out.status.success(), "{args:?}: {stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    stdout
}

fn meta(root: &Path, key: &str, value: Option<&str>) -> Option<String> {
    let cg = CodeGraph::open(&root.to_string_lossy(), Default::default()).unwrap();
    if let Some(v) = value {
        cg.store().set_metadata(key, v).unwrap();
    }
    cg.store().get_metadata(key).unwrap()
}

/// A marker only a full index overwrites: it re-stamps the engine version, a sync never does.
const STAMP: &str = "indexed_with_version";
const MARK: &str = "marker-only-a-full-index-replaces";

fn has_symbol(root: &Path, name: &str) -> bool {
    let cg = CodeGraph::open(&root.to_string_lossy(), Default::default()).unwrap();
    cg.store().get_nodes_by_name(name).map(|n| !n.is_empty()).unwrap_or(false)
}

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.js"), "function a() { return 1; }\n").unwrap();
    run(dir.path(), &["init", "--yes"]);
    meta(dir.path(), STAMP, Some(MARK));
    dir
}

#[test]
fn a_second_init_on_a_current_index_syncs_instead_of_rebuilding() {
    let dir = project();
    std::fs::write(dir.path().join("b.js"), "function addedLater() { return 2; }\n").unwrap();
    let out = run(dir.path(), &["init", "--yes"]);
    assert!(out.contains("Already indexed"), "stdout: {out}");
    assert_eq!(meta(dir.path(), STAMP, None).as_deref(), Some(MARK), "no full rebuild");
    assert!(has_symbol(dir.path(), "addedLater"), "the changed file is synced");
}

#[test]
fn an_older_engine_index_is_rebuilt() {
    let dir = project();
    meta(dir.path(), "indexed_with_extraction_version", Some("1"));
    run(dir.path(), &["init", "--yes"]);
    assert_ne!(meta(dir.path(), STAMP, None).as_deref(), Some(MARK));
}

#[test]
fn an_unfinished_index_is_rebuilt() {
    let dir = project();
    meta(dir.path(), "index_state", Some("partial"));
    run(dir.path(), &["init", "--yes"]);
    assert_ne!(meta(dir.path(), STAMP, None).as_deref(), Some(MARK));
    assert_eq!(meta(dir.path(), "index_state", None).as_deref(), Some("complete"));
}

#[test]
fn a_first_init_indexes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.js"), "function firstOne() { return 1; }\n").unwrap();
    let out = run(dir.path(), &["init", "--yes"]);
    assert!(!out.contains("Already indexed"), "stdout: {out}");
    assert!(has_symbol(dir.path(), "firstOne"));
}
