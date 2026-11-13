//! Upstream f72c9d2e (Cargo scope): a sync follows a Cargo workspace whose members moved, so a
//! Rust importer in an unchanged file links as a fresh index does.
use codegraph_core::{CodeGraph, IndexOptions};
use codegraph_types::EdgeKind;
use std::fs;

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

fn rust_calls(cg: &CodeGraph, caller_file: &str) -> Vec<String> {
    let nodes = cg.store().get_all_nodes().unwrap();
    let ids: Vec<_> = nodes.iter().filter(|n| n.file_path == caller_file).map(|n| n.id.clone()).collect();
    let mut targets: Vec<_> = ids.iter().flat_map(|id| cg.store().get_outgoing_edges(id, &[EdgeKind::Calls], None).unwrap())
        .filter_map(|e| nodes.iter().find(|n| n.id == e.target))
        .map(|n| format!("{}:{}", n.file_path, n.name))
        .collect();
    targets.sort();
    targets.dedup();
    targets
}

fn write(dir: &std::path::Path, files: &[(&str, &str)]) {
    for (path, source) in files {
        let path = dir.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
}

const A_MANIFEST: &str = "[package]\nname = \"a\"\nversion = \"0.1.0\"\n\n[dependencies]\nb = { path = \"../b\" }\n";
const A_SOURCE: &str = "use b::Thing;\npub fn go() { b::run(); let _t = Thing; }\n";
const B_MANIFEST: &str = "[package]\nname = \"b\"\nversion = \"0.1.0\"\n";
const B_SOURCE: &str = "pub fn run() {}\npub struct Thing;\n";

fn assert_converges(cg: &mut CodeGraph) {
    let incremental = snapshot(cg);
    cg.index_all(&IndexOptions::default(), None, None).unwrap();
    assert_eq!(incremental, snapshot(cg), "sync converges to a fresh index");
}

#[test]
fn a_path_dependency_that_moves_relinks_an_untouched_importer() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &[
        ("Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\", \"b2\"]\n"),
        ("a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\n\n[dependencies]\nb = { path = \"../b\" }\n"),
        ("a/src/lib.rs", "use b::Thing;\npub fn go() { b::run(); let _t = Thing; }\n"),
        ("b/Cargo.toml", B_MANIFEST),
        ("b/src/lib.rs", B_SOURCE),
        ("b2/Cargo.toml", "[package]\nname = \"b2\"\nversion = \"0.1.0\"\n"),
        ("b2/src/lib.rs", B_SOURCE),
    ]);
    let mut cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    assert!(rust_calls(&cg, "a/src/lib.rs").contains(&"b/src/lib.rs:run".to_string()), "{:?}", rust_calls(&cg, "a/src/lib.rs"));
    // Only the manifest moves: the same dependency key now names another crate. No source file
    // changes, so only the module-config snapshot can re-open the Rust imports.
    write(dir.path(), &[("a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\n\n[dependencies]\nb = { package = \"b2\", path = \"../b2\" }\n")]);
    cg.sync(&IndexOptions::default(), None).unwrap();
    let calls = rust_calls(&cg, "a/src/lib.rs");
    assert!(calls.contains(&"b2/src/lib.rs:run".to_string()), "{calls:?}");
    assert!(!calls.contains(&"b/src/lib.rs:run".to_string()), "{calls:?}");
    assert_converges(&mut cg);
}

#[test]
fn a_crate_that_appears_under_a_glob_member_is_seen() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &[
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
        ("crates/a/Cargo.toml", A_MANIFEST),
        ("crates/a/src/lib.rs", "use b::Thing;\npub fn go() { b::run(); let _t = Thing; }\n"),
    ]);
    let mut cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    write(dir.path(), &[
        ("crates/b/Cargo.toml", B_MANIFEST),
        ("crates/b/src/lib.rs", B_SOURCE),
    ]);
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert!(rust_calls(&cg, "crates/a/src/lib.rs").contains(&"crates/b/src/lib.rs:run".to_string()), "{:?}", rust_calls(&cg, "crates/a/src/lib.rs"));
    assert_converges(&mut cg);
}

#[test]
fn a_manifest_edit_that_moves_no_import_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &[
        ("Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\n"),
        ("a/Cargo.toml", A_MANIFEST),
        ("a/src/lib.rs", A_SOURCE),
        ("b/Cargo.toml", B_MANIFEST),
        ("b/src/lib.rs", B_SOURCE),
    ]);
    let mut cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let before = snapshot(&cg);
    write(dir.path(), &[("a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.2.0\"\n\n[dependencies]\nb = { path = \"../b\" }\n")]);
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(before, snapshot(&cg));
}
