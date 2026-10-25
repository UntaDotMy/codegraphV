//! Portable expectations captured from the TypeScript oracle, never the port.
mod support;
use codegraph_db::Store;
use codegraph_types::{Edge, FileRecord, Node};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Deserialize)]
struct Scenario {
    name: String,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    files: Vec<FileRecord>,
    sources: HashMap<String, String>,
    cases: Vec<Case>,
}
#[derive(Deserialize)]
struct Case {
    query: Value,
    expected: Value,
}

#[test]
fn context_matches_the_current_reference_engine() {
    let scenarios: Vec<Scenario> = serde_json::from_str(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/golden.json"
        ))
        .unwrap(),
    )
    .unwrap();
    for s in scenarios {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
        store.insert_nodes(&s.nodes).unwrap();
        store.insert_edges(&s.edges).unwrap();
        for f in &s.files {
            store.upsert_file(f).unwrap();
        }
        for (path, text) in &s.sources {
            let p = dir.path().join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        for c in &s.cases {
            let actual = support::run(&store, dir.path().to_str().unwrap(), &c.query);
            let expected = serde_json::from_str::<Value>(&codegraph_types::branding::from_original(&c.expected.to_string())).unwrap(); // the TS output, in codegraphV's names
            assert_eq!(actual, expected, "{}: {}", s.name, c.query);
        }
        store.close().unwrap();
    }
}
