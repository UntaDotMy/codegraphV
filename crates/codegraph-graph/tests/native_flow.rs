use codegraph_db::Store;
use codegraph_graph::named_symbol_flow::{resolve_named_symbol_flow, NamedSymbolFlowOptions};
use codegraph_types::{Edge, EdgeKind, Language, Node, NodeKind};

#[test]
fn flow_uses_native_search_and_keeps_a_miss_distinct_from_fuzzy_suggestions() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    let nodes: Vec<_> = ["loadUsers", "dispatchWork", "saveUsers"]
        .into_iter()
        .enumerate()
        .map(|(i, name)| {
            Node::new(
                format!("n{i}"),
                NodeKind::Function,
                name,
                name,
                "src/store.rs",
                Language::Rust,
                i as u32 + 1,
                i as u32 + 1,
                0,
                0,
                1,
            )
        })
        .collect();
    store.insert_nodes(&nodes).unwrap();
    store
        .insert_edges(&[
            Edge::new("n0", "n1", EdgeKind::Calls),
            Edge::new("n1", "n2", EdgeKind::Calls),
        ])
        .unwrap();
    // Both the graph and symbol lookup are supplied by the real native Store.
    let flow = resolve_named_symbol_flow(
        &store,
        &store,
        "loadUsers saveUsers",
        &NamedSymbolFlowOptions::default(),
        None,
    )
    .unwrap();
    assert_eq!(
        flow.chains[0]
            .steps
            .iter()
            .map(|s| s.node.name.as_str())
            .collect::<Vec<_>>(),
        ["loadUsers", "dispatchWork", "saveUsers"]
    );
    assert!(!store
        .search_nodes("loadUsrs", &Default::default())
        .unwrap()
        .is_empty());
    let miss = resolve_named_symbol_flow(
        &store,
        &store,
        "loadUsrs saveUsers",
        &Default::default(),
        None,
    )
    .unwrap();
    assert!(miss.chains.is_empty());
    assert_eq!(miss.named.len(), 1);
    store.close().unwrap();
}
