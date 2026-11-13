//! Upstream 4b0f72b7: a file that shrinks back under the source-size limit links its
//! importers as a fresh index does, including the file-level import edge.
use codegraph_core::{CodeGraph, IndexOptions};
use codegraph_types::EdgeKind;
use std::fs;

const BIG: &str = "export function f1(): number { return 1; }\n// ";

fn big(padding: usize) -> String {
    format!("{BIG}{}\n", "x".repeat(padding))
}

fn snapshot(cg: &CodeGraph) -> (Vec<String>, Vec<String>, Vec<String>) {
    let nodes = cg.store().get_all_nodes().unwrap();
    let mut ns: Vec<_> = nodes.iter().map(|n| {
        let mut value = serde_json::to_value(n).unwrap();
        value.as_object_mut().unwrap().remove("updatedAt");
        value.to_string()
    }).collect();
    let mut es: Vec<_> = nodes.iter().flat_map(|n| cg.store().get_outgoing_edges(&n.id, &[], None).unwrap())
        .map(|e| serde_json::to_string(&e).unwrap()).collect();
    let mut refs: Vec<_> = cg.store().get_unresolved_references().unwrap().iter().map(|r| {
        let mut value = serde_json::to_value(r).unwrap();
        value.as_object_mut().unwrap().remove("id");
        value.to_string()
    }).collect();
    ns.sort(); es.sort(); refs.sort();
    (ns, es, refs)
}

fn imports_of(cg: &CodeGraph, file: &str) -> Vec<String> {
    let nodes = cg.store().get_all_nodes().unwrap();
    let ids: Vec<_> = nodes.iter().filter(|n| n.file_path == file).map(|n| n.id.clone()).collect();
    let mut found: Vec<_> = ids.iter().flat_map(|id| cg.store().get_outgoing_edges(id, &[EdgeKind::Imports], None).unwrap())
        .filter_map(|e| nodes.iter().find(|n| n.id == e.target))
        .map(|n| format!("{}:{}", n.kind.as_str(), n.name))
        .collect();
    found.sort();
    found
}

fn assert_converges(cg: &mut CodeGraph) {
    let incremental = snapshot(cg);
    cg.index_all(&IndexOptions::default(), None, None).unwrap();
    assert_eq!(incremental, snapshot(cg), "sync converges to a fresh index");
}

fn setup(limit_mb: &str) -> tempfile::TempDir {
    // The limit is a process-wide OnceLock: this integration test binary owns it.
    std::env::set_var("CODEGRAPHV_MAX_FILE_MB", limit_mb);
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("big.ts"), big(5_000)).unwrap();
    fs::write(dir.path().join("user.ts"), "import { f1 } from './big';\nexport const x = f1();\n").unwrap();
    dir
}

#[test]
fn a_file_that_shrinks_under_the_limit_links_its_importers() {
    let dir = setup("0.001");
    let mut cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    assert_eq!(imports_of(&cg, "user.ts"), Vec::<String>::new(), "over the limit, nothing to import");
    fs::write(dir.path().join("big.ts"), big(10)).unwrap();
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(imports_of(&cg, "user.ts"), ["file:big.ts", "function:f1"]);
    assert_converges(&mut cg);

    // Growing over the limit again removes both edges, and shrinking re-adds them.
    fs::write(dir.path().join("big.ts"), big(8_000)).unwrap();
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(imports_of(&cg, "user.ts"), Vec::<String>::new());
    fs::write(dir.path().join("big.ts"), big(20)).unwrap();
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(imports_of(&cg, "user.ts"), ["file:big.ts", "function:f1"]);
    assert_converges(&mut cg);
}

#[test]
fn a_file_stored_after_a_parse_failure_relinks_its_importers() {
    let dir = setup("0.001");
    let mut cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    // Replace the oversize record with the other node-less shape: a recorded parse failure.
    let mut record = cg.store().get_file_by_path("big.ts").unwrap().unwrap();
    cg.store().delete_file("big.ts").unwrap();
    record.node_count = 0;
    record.errors = Some(vec![codegraph_types::ExtractionError {
        message: "parse failed".into(),
        file_path: Some("big.ts".into()),
        line: None,
        column: None,
        severity: codegraph_types::Severity::Error,
        code: Some("parse_error".into()),
    }]);
    cg.store().upsert_file(&record).unwrap();
    cg.close();
    let mut cg = CodeGraph::open(dir.path().to_str().unwrap(), Default::default()).unwrap();
    fs::write(dir.path().join("big.ts"), big(10)).unwrap();
    let result = cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(result.added_file_paths.as_deref(), Some(&["big.ts".to_string()][..]));
    assert_eq!(imports_of(&cg, "user.ts"), ["file:big.ts", "function:f1"]);
    assert_converges(&mut cg);
}

#[test]
fn a_barrel_passes_on_what_a_shrunk_file_exports() {
    let dir = setup("0.001");
    fs::create_dir_all(dir.path().join("lib")).unwrap();
    fs::write(dir.path().join("lib/index.ts"), "export * from './big';\nexport * from './small';\n").unwrap();
    fs::write(dir.path().join("lib/small.ts"), "export function s1(): number { return 2; }\n").unwrap();
    fs::write(dir.path().join("lib/big.ts"), big(5_000)).unwrap();
    fs::write(dir.path().join("user.ts"), "import { f1, s1 } from './lib';\nexport const x = f1() + s1();\n").unwrap();
    let mut cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    fs::write(dir.path().join("lib/big.ts"), big(10)).unwrap();
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert!(imports_of(&cg, "user.ts").iter().any(|e| e.contains("f1")), "{:?}", imports_of(&cg, "user.ts"));
    assert_converges(&mut cg);
}
