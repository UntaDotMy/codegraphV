//! Native stack-safety and large-query tests beyond the oracle's recursive
//! implementation. All graphs use real SQLite and close before temp cleanup.
use codegraph_db::Store;
use codegraph_graph::{GraphQueryManager, GraphTraverser};
use codegraph_types::{Edge, EdgeKind, Language, Node, NodeKind, TraversalOptions};

#[test]
fn impact_budgets_preserve_endpoints_and_report_only_incomplete_walks() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    let nodes: Vec<_> = (0..8).map(|i| Node::new(format!("n{i}"),
        if i == 0 { NodeKind::Class } else { NodeKind::Function },
        format!("n{i}"), format!("n{i}"), "a.rs", Language::Rust, 1, 1, 0, 0, 1)).collect();
    store.insert_nodes(&nodes).unwrap();
    let edges: Vec<_> = (1..8).map(|i| Edge::new(format!("n{i}"), "n0", EdgeKind::Calls)).collect();
    store.insert_edges(&edges).unwrap();
    let t = GraphTraverser::new(&store);
    for (n, e, work) in [(3, 50, 100), (10, 2, 100), (10, 50, 2), (1, 0, 0)] {
        let result = t.get_impact_radius_bounded("n0", 3, n, e, work).unwrap();
        assert_eq!(result.truncated, Some(true));
        assert!(result.nodes.len() <= n && result.edges.len() <= e);
        assert!(result.edges.iter().all(|e| result.nodes.contains_key(&e.source) && result.nodes.contains_key(&e.target)));
    }
    let exact = t.get_impact_radius_bounded("n0", 1, 8, 7, 7).unwrap();
    assert_eq!(exact.nodes.len(), 8);
    assert_eq!(exact.truncated, None);
    assert_eq!(t.get_impact_radius_bounded("n0", 0, 1, 0, 0).unwrap().truncated, None);
    // A container's children expand at the same depth, including containment cycles.
    store.insert_edges(&[Edge::new("n0", "n1", EdgeKind::Contains), Edge::new("n1", "n0", EdgeKind::Calls)]).unwrap();
    let bounded = t.get_impact_radius_bounded("n0", u32::MAX, 3, 3, 4).unwrap();
    assert_eq!(bounded.truncated, Some(true));
    assert!(bounded.nodes.len() <= 3 && bounded.edges.len() <= 3);
}

#[test]
fn long_chains_use_heap_stacks_and_preserve_order() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    let count = 3000;
    let nodes: Vec<_> = (0..count)
        .map(|i| {
            Node::new(
                format!("n{i:04}"),
                NodeKind::Function,
                format!("n{i}"),
                format!("n{i}"),
                "chain.rs",
                Language::Rust,
                i + 1,
                i + 1,
                0,
                10,
                1,
            )
        })
        .collect();
    let edges: Vec<_> = nodes
        .windows(2)
        .map(|pair| Edge::new(&pair[0].id, &pair[1].id, EdgeKind::Calls))
        .collect();
    store.insert_nodes(&nodes).unwrap();
    store.insert_edges(&edges).unwrap();
    let t = GraphTraverser::new(&store);
    let graph = t
        .traverse_dfs(
            &nodes[0].id,
            &TraversalOptions {
                limit: Some(count as usize),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(graph.nodes.len(), count as usize);
    assert_eq!(graph.edges.len(), count as usize - 1);
    assert_eq!(graph.nodes.last().unwrap().0, &nodes.last().unwrap().id);
    assert_eq!(
        t.get_callees(&nodes[0].id, count).unwrap().len(),
        count as usize - 1
    );
    assert_eq!(
        t.get_callers(&nodes.last().unwrap().id, count)
            .unwrap()
            .len(),
        count as usize - 1
    );
    assert_eq!(
        t.get_impact_radius(&nodes.last().unwrap().id, count)
            .unwrap()
            .nodes
            .len(),
        count as usize
    );
    let path = t
        .find_path(&nodes[0].id, &nodes.last().unwrap().id, &[])
        .unwrap()
        .unwrap();
    assert_eq!(path.len(), count as usize);
    let ids: Vec<_> = nodes.iter().map(|n| n.id.clone()).collect();
    assert_eq!(
        store.find_edges_between_nodes(&ids, &[]).unwrap().len(),
        edges.len()
    );
    store.close().unwrap();
}

#[test]
fn high_fanout_adjacency_is_bounded_and_reported() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    let fanout: usize = 10_050;
    let mut nodes = vec![Node::new(
        "hub",
        NodeKind::Function,
        "hub",
        "hub",
        "hub.rs",
        Language::Rust,
        1,
        1,
        0,
        1,
        1,
    )];
    nodes.extend((0..fanout).map(|i| {
        Node::new(
            format!("leaf{i}"),
            NodeKind::Function,
            format!("leaf{i}"),
            format!("leaf{i}"),
            "hub.rs",
            Language::Rust,
            (i + 2) as u32,
            (i + 2) as u32,
            0,
            1,
            1,
        )
    }));
    store.insert_nodes(&nodes).unwrap();
    let edges: Vec<_> = (0..fanout)
        .map(|i| Edge::new("hub", format!("leaf{i}"), EdgeKind::Calls))
        .collect();
    store.insert_edges(&edges).unwrap();
    let t = GraphTraverser::new(&store);
    // The walk's node limit is far above the fanout: only the adjacency budget can stop it.
    let graph = t
        .traverse_bfs(
            "hub",
            &TraversalOptions {
                limit: Some(fanout * 2),
                max_depth: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(graph.truncated, Some(true));
    assert!(graph.nodes.len() <= 10_001, "{}", graph.nodes.len());
    assert!(graph.edges.len() <= 10_000, "{}", graph.edges.len());
    assert!(graph.nodes.contains_key("hub"));
    assert!(graph.nodes.keys().all(|id| id == "hub" || id.starts_with("leaf")));
    // An explicit call budget also reports the cut instead of silently returning all edges.
    let (refs, cut) = t.get_callees_bounded("hub", 1, 100).unwrap();
    assert_eq!(refs.len(), 100);
    assert!(cut);
    let (all, cut) = t.get_callees_bounded("hub", 1, fanout).unwrap();
    assert_eq!(all.len(), 10_000);
    assert!(cut, "the adjacency budget, not the relationship budget, is the ceiling");
    store.close().unwrap();
}

#[test]
fn structural_containment_does_not_create_file_dependencies() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    let nodes = [
        Node::new(
            "a",
            NodeKind::Class,
            "A",
            "A",
            "a.rs",
            Language::Rust,
            1,
            2,
            0,
            0,
            1,
        ),
        Node::new(
            "b",
            NodeKind::Method,
            "b",
            "B::b",
            "b.rs",
            Language::Rust,
            1,
            2,
            0,
            0,
            1,
        ),
    ];
    store.insert_nodes(&nodes).unwrap();
    store
        .insert_edge(&Edge::new("a", "b", EdgeKind::Contains))
        .unwrap();
    let q = GraphQueryManager::new(&store);
    assert!(q.get_file_dependencies("a.rs").unwrap().is_empty());
    assert!(q.get_file_dependents("b.rs").unwrap().is_empty());
    store
        .insert_edge(&Edge::new("a", "b", EdgeKind::Navigates))
        .unwrap();
    assert_eq!(q.get_file_dependencies("a.rs").unwrap(), ["b.rs"]);
    assert_eq!(q.get_file_dependents("b.rs").unwrap(), ["a.rs"]);
    store.close().unwrap();
}
