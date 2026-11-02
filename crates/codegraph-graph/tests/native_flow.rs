use codegraph_db::Store;
use codegraph_graph::named_symbol_flow::{resolve_named_symbol_flow, NamedSymbolFlowOptions};
use codegraph_types::{Edge, EdgeKind, Language, Node, NodeKind};

#[test]
fn unicode_names_reach_named_flows_without_ascii_fragments() {
    use codegraph_graph::named_symbol_flow::flow_tokens;
    assert_eq!(flow_tokens("入口 末端 Ωmega état NavMeshAgent类型"), ["入口", "末端", "Ωmega", "état", "NavMeshAgent", "类型"]);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    let nodes: Vec<_> = ["入口", "末端"].into_iter().map(|name| Node::new(name, NodeKind::Function,
        name, name, "flow.rs", Language::Rust, 1, 1, 0, 0, 1)).collect();
    store.insert_nodes(&nodes).unwrap();
    store.insert_edge(&Edge::new("入口", "末端", EdgeKind::Calls)).unwrap();
    let flow = resolve_named_symbol_flow(&store, &store, "入口 末端", &Default::default(), None).unwrap();
    assert_eq!(flow.chains[0].steps.len(), 2);
}

#[test]
fn qualified_lookup_requires_a_whole_scope_even_in_a_matching_filename() {
    use codegraph_graph::symbol_lookup::matches_symbol;
    let node = Node::new("price", NodeKind::Method, "Price", "LegacyFormat::Price",
        "src/Format.cs", Language::CSharp, 1, 1, 0, 0, 1);
    assert!(!matches_symbol(&node, "Format.Price", None));
    assert!(matches_symbol(&node, "LegacyFormat.Price", None));
    let mut node = node;
    node.qualified_name = "Billing::Format::Price".into();
    assert!(matches_symbol(&node, "Format.Price", None));
}

#[test]
fn longer_named_route_survives_a_shortcut_and_never_repeats_a_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    let nodes: Vec<_> = ["entry", "mid1", "mid2", "deep", "leaf"].into_iter()
        .map(|name| Node::new(name, NodeKind::Function, name, name, "src/flow.rs",
            Language::Rust, 1, 1, 0, 0, 1)).collect();
    store.insert_nodes(&nodes).unwrap();
    store.insert_edges(&[("entry", "mid1"), ("entry", "mid2"), ("mid1", "deep"),
        ("deep", "leaf"), ("mid2", "leaf"), ("leaf", "entry")].map(|(a,b)| Edge::new(a,b,EdgeKind::Calls))).unwrap();
    let flow = resolve_named_symbol_flow(&store, &store, "entry deep leaf", &Default::default(), None).unwrap();
    let names: Vec<_> = flow.chains[0].steps.iter().map(|s| s.node.name.as_str()).collect();
    assert_eq!(names, ["entry", "mid1", "deep", "leaf"]);
    for chain in flow.chains {
        let ids: std::collections::HashSet<_> = chain.steps.iter().map(|s| &s.node.id).collect();
        assert_eq!(ids.len(), chain.steps.len());
    }
}

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
