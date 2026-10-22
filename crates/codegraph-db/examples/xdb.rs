//! Cross-engine harness (driven by `scripts/rust-golden/xdb.mjs`).
//!
//!   xdb write <db> <fixture.json>   store the fixture into a fresh database
//!   xdb read  <db>                  print the database's graph as JSON
//!
//! A database written here must open and read back identically through the
//! TypeScript engine, and vice versa.

use codegraph_db::{FileBundle, OpenOptions, Store};
use codegraph_types::{Edge, FileRecord, Node, UnresolvedReference};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("write") => write(&args[2], &args[3]),
        Some("read") => read(&args[2]),
        // Open read-write (running migrations and the open-time heals), then close.
        Some("migrate") => Store::open(&args[2], OpenOptions::default()).unwrap().close().unwrap(),
        _ => {
            eprintln!("usage: xdb write <db> <fixture.json> | xdb read <db> | xdb migrate <db>");
            std::process::exit(2);
        }
    }
}

fn write(db: &str, fixture: &str) {
    let fx: Value = serde_json::from_str(&std::fs::read_to_string(fixture).unwrap()).unwrap();
    let nodes: Vec<Node> = serde_json::from_value(fx["nodes"].clone()).unwrap();
    let edges: Vec<Edge> = serde_json::from_value(fx["edges"].clone()).unwrap();
    let files: Vec<FileRecord> = serde_json::from_value(fx["files"].clone()).unwrap();
    let refs: Vec<UnresolvedReference> = serde_json::from_value(fx["refs"].clone()).unwrap();

    let store = Store::initialize(db).unwrap();
    for file in &files {
        let f_nodes: Vec<Node> = nodes.iter().filter(|n| n.file_path == file.path).cloned().collect();
        let ids: std::collections::HashSet<&str> = f_nodes.iter().map(|n| n.id.as_str()).collect();
        // The bundle path's edges are pre-filtered by the caller to the file's own nodes.
        let f_edges: Vec<Edge> = edges.iter().filter(|e| ids.contains(e.source.as_str())).cloned().collect();
        let f_refs: Vec<UnresolvedReference> =
            refs.iter().filter(|r| ids.contains(r.from_node_id.as_str())).cloned().collect();
        // Cross-file edge targets don't exist yet; FK enforcement would reject them, so
        // keep only intra-file edges here and add the rest after every file is stored.
        let f_edges: Vec<Edge> = f_edges.into_iter().filter(|e| ids.contains(e.target.as_str())).collect();
        store
            .store_file_bundle(&FileBundle { nodes: &f_nodes, edges: &f_edges, refs: &f_refs, file })
            .unwrap();
    }
    let cross: Vec<Edge> = edges
        .iter()
        .filter(|e| {
            let src = nodes.iter().find(|n| n.id == e.source).map(|n| &n.file_path);
            let dst = nodes.iter().find(|n| n.id == e.target).map(|n| &n.file_path);
            src != dst
        })
        .cloned()
        .collect();
    store.insert_edges(&cross).unwrap();
    store.close().unwrap();
}

fn read(db: &str) {
    let store = Store::open(db, OpenOptions { read_only: true }).unwrap();
    let mut nodes = store.get_all_nodes().unwrap();
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    let mut edges = Vec::new();
    for n in &nodes {
        edges.extend(store.get_outgoing_edges(&n.id, &[], None).unwrap());
    }
    let files = store.get_all_files().unwrap();
    let mut refs = store.get_unresolved_references().unwrap();
    for r in &mut refs {
        r.row_id = None;
    }
    let status: BTreeMap<&str, i64> = BTreeMap::new();
    let _ = status;
    println!(
        "{}",
        json!({
            "nodes": nodes,
            "edges": edges,
            "files": files,
            "refs": refs,
            "version": store.schema_version().unwrap().map(|v| v.version),
        })
    );
}
