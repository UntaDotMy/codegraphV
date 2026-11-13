//! `status` recommends a re-index when a framework arrives or leaves through `package.json` alone
//! (upstream 5526e33d, #3007). A sync detects frameworks again only when it extracts a file, so a
//! manifest-only change recorded nothing; the resolver detects on every open, so its set is
//! compared with the one the last full index stamped.
use codegraph_core::{CodeGraph, IndexOptions};
use std::path::Path;

const KIT: &str = r#"{"name":"app","devDependencies":{"@sveltejs/kit":"^2.0.0","svelte":"^4.0.0"}}"#;
const NO_KIT: &str = r#"{"name":"app","devDependencies":{"svelte":"^4.0.0"}}"#;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn svelte_app() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/routes/+layout.svelte", "<script>\n  import { goto } from \"$app/navigation\";\n  function out() { goto(\"/login\"); }\n</script>\n");
    write(dir.path(), "src/routes/login/+page.svelte", "<h1>login</h1>\n");
    dir
}

fn open(root: &Path) -> CodeGraph {
    CodeGraph::open(&root.to_string_lossy(), Default::default()).unwrap()
}

#[test]
fn names_the_sveltekit_router_when_kit_is_added_after_the_index() {
    let dir = svelte_app();
    write(dir.path(), "package.json", NO_KIT);
    let cg = CodeGraph::init_and_index(&dir.path().to_string_lossy(), None).unwrap();
    assert_eq!(cg.get_framework_changes_since_index(), None);
    cg.close();

    write(dir.path(), "package.json", KIT);
    let mut cg = open(dir.path());
    cg.sync(&IndexOptions::default(), None).unwrap();
    let changes = cg.get_framework_changes_since_index().expect("a manifest-only change is reported");
    assert!(changes.added.iter().any(|n| n == "sveltekit-router"), "{changes:?}");

    cg.index_all(&IndexOptions::default(), None, None).unwrap();
    assert_eq!(cg.get_framework_changes_since_index(), None, "a full index clears it");
}

#[test]
fn names_it_as_removed_when_the_dependency_goes() {
    let dir = svelte_app();
    write(dir.path(), "package.json", KIT);
    CodeGraph::init_and_index(&dir.path().to_string_lossy(), None).unwrap().close();
    write(dir.path(), "package.json", NO_KIT);
    let cg = open(dir.path());
    let changes = cg.get_framework_changes_since_index().expect("removal is reported without a sync");
    assert!(changes.removed.iter().any(|n| n == "sveltekit-router"), "{changes:?}");
}

#[test]
fn names_express_when_it_is_added_with_no_source_file_changed() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "package.json", r#"{"name":"app","dependencies":{}}"#);
    write(dir.path(), "src/server.js", "const express = require('express');\nconst app = express();\napp.get('/hello', (req, res) => res.send('hi'));\n");
    let cg = CodeGraph::init_and_index(&dir.path().to_string_lossy(), None).unwrap();
    assert_eq!(cg.get_framework_changes_since_index(), None);
    cg.close();
    write(dir.path(), "package.json", r#"{"name":"app","dependencies":{"express":"^4.0.0"}}"#);
    let mut cg = open(dir.path());
    cg.sync(&IndexOptions::default(), None).unwrap();
    let changes = cg.get_framework_changes_since_index().expect("express is reported");
    assert!(changes.added.iter().any(|n| n == "express"), "{changes:?}");
}

#[test]
fn says_nothing_when_the_project_is_as_it_was_indexed() {
    let dir = svelte_app();
    write(dir.path(), "package.json", KIT);
    CodeGraph::init_and_index(&dir.path().to_string_lossy(), None).unwrap().close();
    let mut cg = open(dir.path());
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(cg.get_framework_changes_since_index(), None);
}
