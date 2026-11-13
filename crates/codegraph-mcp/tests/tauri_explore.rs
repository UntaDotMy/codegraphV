//! codegraphV_explore carries a flow across the Tauri seam (upstream 2008e2f3). codegraphV's Flow
//! line names a hop by its summary only (no `@wiring site`, for any hop), so that is what's pinned.
use codegraph_core::CodeGraph;
use codegraph_mcp::render::explore;
use serde_json::json;
use std::collections::HashMap;
use std::fs;

#[test]
fn carries_a_flow_across_the_seam() {
    let dir = tempfile::tempdir().unwrap();
    let files = [
        ("src-tauri/src/lib.rs", "fn persist() {}\n#[tauri::command]\nfn save_file() { persist(); }\nfn run() { tauri::Builder::default().invoke_handler(tauri::generate_handler![save_file]); }"),
        ("src/a.ts", "import { invoke } from \"@tauri-apps/api/core\";\nexport function onSaveClick() { return invoke(\"save_file\"); }"),
    ];
    for (rel, body) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let out = explore(&cg, &json!({ "query": "onSaveClick save_file persist" }), &mut HashMap::new()).unwrap().message();
    assert!(out.contains("- onSaveClick (function) — src/a.ts:2
  → save_file (function) — src-tauri/src/lib.rs:3 — Tauri command `save_file`
  → persist (function)"), "{out}");
    cg.close();
}
