//! Behavioural coverage for the boundary report — the graph-aware half of
//! `src/graph/dynamic-boundary-report.ts` (#687).
use codegraph_db::Store;
use codegraph_graph::dynamic_boundary_report::{
    continuations_from, find_dynamic_boundaries, shortlist_boundary_candidates, UNCERTAIN_BELOW,
};
use codegraph_types::{Edge, EdgeKind, Language, Metadata, Node, NodeKind};
use std::collections::{HashMap, HashSet};

fn node(id: &str, kind: NodeKind, name: &str, file: &str) -> Node {
    Node::new(
        id,
        kind,
        name,
        format!("{file}::{name}"),
        file,
        Language::TypeScript,
        1,
        3,
        0,
        0,
        0,
    )
}

#[test]
fn reads_a_body_from_disk_and_reports_the_absolute_site() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/router.ts"),
        [
            "export class Router {",
            "  route(a: Action) {",
            "    this.table[a.type](a.payload);",
            "  }",
            "}",
        ]
        .join("\n"),
    )
    .unwrap();

    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    let owner = node("n1", NodeKind::Method, "route", "src/router.ts");
    let boundaries = find_dynamic_boundaries(
        &store,
        dir.path().to_str().unwrap(),
        &[owner],
        4,
        &HashMap::new(),
    );
    assert_eq!(boundaries.len(), 1);
    assert_eq!(boundaries[0].sites.len(), 1);
    assert_eq!(boundaries[0].sites[0].matched.form, "computed-call");
    assert_eq!(boundaries[0].sites[0].matched.line, 3);
    store.close().unwrap();
}

#[test]
fn never_reads_a_path_that_escapes_the_project_root() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(
        outside.path().join("secret.ts"),
        "this.table['save'](p);",
    )
    .unwrap();

    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    let owner = node("n1", NodeKind::Method, "run", "../secret.ts");
    let boundaries = find_dynamic_boundaries(
        &store,
        dir.path().to_str().unwrap(),
        &[owner],
        4,
        &HashMap::new(),
    );
    assert!(boundaries.is_empty());
    store.close().unwrap();
}

#[test]
fn marks_a_named_candidate_and_sorts_it_first() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    let unnamed = node("n1", NodeKind::Function, "handleSave", "a.ts");
    let named_node = node("n2", NodeKind::Function, "onSave", "b.ts");
    store
        .insert_nodes(&[unnamed, named_node.clone()])
        .unwrap();

    let mut named = HashMap::new();
    named.insert(named_node.id.clone(), named_node.clone());
    let (candidates, note) =
        shortlist_boundary_candidates(&store, "save", false, &named, "self");
    assert!(note.is_none());
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].node.name, "onSave");
    assert!(candidates[0].named);
    assert!(!candidates[1].named);
    store.close().unwrap();
}

#[test]
fn a_constructor_named_like_its_class_does_not_duplicate_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    store
        .insert_nodes(&[
            node("c1", NodeKind::Class, "SaveAction", "a.ts"),
            node("m1", NodeKind::Method, "SaveAction", "a.ts"),
        ])
        .unwrap();

    let (candidates, _) =
        shortlist_boundary_candidates(&store, "save", false, &HashMap::new(), "self");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].node.id, "c1");
    store.close().unwrap();
}

#[test]
fn a_type_key_names_the_handler_method_instead_of_the_class() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    store
        .insert_nodes(&[
            node("c1", NodeKind::Class, "CreateCmdHandler", "a.ts"),
            node("m1", NodeKind::Method, "Handle", "a.ts"),
        ])
        .unwrap();
    store
        .insert_edges(&[Edge::new("c1", "m1", EdgeKind::Contains)])
        .unwrap();

    let (candidates, _) =
        shortlist_boundary_candidates(&store, "CreateCmd", true, &HashMap::new(), "self");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].display, "CreateCmdHandler.Handle");
    assert_eq!(candidates[0].node.id, "m1");
    store.close().unwrap();
}

#[test]
fn a_key_too_short_to_narrow_returns_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    let (candidates, note) =
        shortlist_boundary_candidates(&store, "id", false, &HashMap::new(), "self");
    assert!(candidates.is_empty());
    assert!(note.is_none());
    store.close().unwrap();
}

#[test]
fn splits_continuations_by_confidence_and_dedupes_by_target() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    store
        .insert_nodes(&[
            node("n1", NodeKind::Method, "run", "a.ts"),
            node("n2", NodeKind::Function, "sure", "b.ts"),
            node("n3", NodeKind::Function, "guess", "c.ts"),
        ])
        .unwrap();

    let mut confident = Metadata::new();
    confident.insert("confidence".into(), serde_json::json!(0.9));
    let mut shaky = Metadata::new();
    shaky.insert("confidence".into(), serde_json::json!(0.4));

    let mut first = Edge::new("n1", "n2", EdgeKind::Calls);
    first.line = Some(5);
    first.metadata = Some(confident);
    let mut duplicate = Edge::new("n1", "n2", EdgeKind::Calls);
    duplicate.line = Some(9);
    let mut uncertain = Edge::new("n1", "n3", EdgeKind::Calls);
    uncertain.line = Some(2);
    uncertain.metadata = Some(shaky);
    store
        .insert_edges(&[first, duplicate, uncertain])
        .unwrap();

    let owner = node("n1", NodeKind::Method, "run", "a.ts");
    let out = continuations_from(&store, &owner, &HashSet::new());
    assert_eq!(out.resolved.len(), 1);
    assert_eq!(out.resolved[0].node.id, "n2");
    assert_eq!(out.resolved[0].line, Some(5));
    assert_eq!(out.uncertain.len(), 1);
    assert_eq!(out.uncertain[0].node.id, "n3");
    assert!(out.uncertain[0].confidence.unwrap() < UNCERTAIN_BELOW);

    let excluded: HashSet<String> = ["n3".to_string()].into_iter().collect();
    let out = continuations_from(&store, &owner, &excluded);
    assert!(out.uncertain.is_empty());
    store.close().unwrap();
}
