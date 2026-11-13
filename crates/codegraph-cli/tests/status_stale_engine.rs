//! `status` names the engine that built an index an older engine built, and offers only
//! `codegraphV index` (upstream 0410b310, #3123): sync never advances the extraction stamp.
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
    assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

const FULL_REBUILD: &str = "Run \"codegraphV index\" for a full rebuild";

#[test]
fn status_names_the_old_engine_and_only_codegraphv_index() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.js"), "function a() { return 1; }\n").unwrap();
    run(dir.path(), &["init", "--yes"]);
    let current = run(dir.path(), &["status"]);
    assert!(!current.contains("Index was built by"), "{current}");
    assert!(!current.contains(FULL_REBUILD), "{current}");

    let cg = CodeGraph::open(&dir.path().to_string_lossy(), Default::default()).unwrap();
    cg.store().set_metadata("indexed_with_extraction_version", "1").unwrap();
    cg.store().set_metadata("indexed_with_version", "0.0.1").unwrap();
    cg.close();

    let stale = run(dir.path(), &["status"]);
    assert!(stale.contains("Index was built by v0.0.1"), "{stale}");
    assert!(stale.contains(FULL_REBUILD), "{stale}");
    assert!(!stale.contains("codegraphV sync"), "{stale}");
}
