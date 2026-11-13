//! Upstream f72c9d2e: a long-lived resolver must not resolve an edit by what the file said
//! before it. Ported from the Go case of `__tests__/sync-memo-lifetime.test.ts`.
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

fn callee_files(cg: &CodeGraph, caller: &str) -> Vec<String> {
    let nodes = cg.store().get_all_nodes().unwrap();
    let node = nodes.iter().find(|n| n.name == caller).unwrap();
    let mut files: Vec<_> = cg.store().get_outgoing_edges(&node.id, &[EdgeKind::Calls], None).unwrap()
        .iter()
        .filter_map(|e| nodes.iter().find(|n| n.id == e.target))
        .map(|n| n.file_path.clone())
        .collect();
    files.sort();
    files.dedup();
    files
}

fn built(tag: &str, value: &str) -> String {
    format!("//go:build {tag}\n\npackage app\n\nfunc sep() string {{ return \"{value}\" }}\n")
}

#[test]
fn a_vue_script_block_that_becomes_an_export_is_seen_by_a_sync() {
    let dir = tempfile::tempdir().unwrap();
    let write = |file: &str, text: &str| fs::write(dir.path().join(file), text).unwrap();
    write("package.json", "{\"name\":\"app\"}");
    write("Comp.vue", "<script setup lang=\"ts\">\nfunction helper() { return 1; }\n</script>\n<template><div/></template>\n");
    write("use.ts", "export function run() { return helper(); }\n");
    let mut cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    assert!(!calls_to_helper(&cg));

    write("Comp.vue", "<script lang=\"ts\">\nexport function helper() { return 1; }\n</script>\n<template><div/></template>\n");
    write("use.ts", "// edited\nexport function run() { return helper(); }\n");
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert!(calls_to_helper(&cg), "the exported component function is reached after sync");

    let incremental = snapshot(&cg);
    cg.index_all(&IndexOptions::default(), None, None).unwrap();
    assert_eq!(incremental, snapshot(&cg), "sync converges to a fresh index");
}

fn calls_to_helper(cg: &CodeGraph) -> bool {
    let nodes = cg.store().get_all_nodes().unwrap();
    let ids: Vec<_> = nodes.iter().filter(|n| n.file_path == "use.ts").map(|n| n.id.clone()).collect();
    ids.iter().flat_map(|id| cg.store().get_outgoing_edges(id, &[EdgeKind::Calls], None).unwrap())
        .filter_map(|e| nodes.iter().find(|n| n.id == e.target))
        .any(|n| n.file_path == "Comp.vue" && n.name == "helper")
}

#[test]
fn a_go_file_that_changes_the_target_it_builds_for_is_re_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let write = |file: &str, text: &str| fs::write(dir.path().join(file), text).unwrap();
    write("go.mod", "module example.com/app\n\ngo 1.22\n");
    write("sep_a.go", &built("!alpha", "/"));
    write("sep_b.go", &built("alpha", "x"));
    write("main.go", "package app\n\nfunc Join() string { return sep() }\n");
    let mut cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    assert_eq!(callee_files(&cg, "Join"), ["sep_a.go"]);

    // The two files trade the build target they satisfy; the caller is edited so it is
    // re-resolved either way. Only a resolver that re-reads the files can see the swap.
    write("sep_a.go", &built("alpha", "/"));
    write("sep_b.go", &built("!alpha", "x"));
    write("main.go", "package app\n\n// edited\nfunc Join() string { return sep() }\n");
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(callee_files(&cg, "Join"), ["sep_b.go"]);

    let incremental = snapshot(&cg);
    cg.index_all(&IndexOptions::default(), None, None).unwrap();
    assert_eq!(incremental, snapshot(&cg), "sync converges to a fresh index");
}
