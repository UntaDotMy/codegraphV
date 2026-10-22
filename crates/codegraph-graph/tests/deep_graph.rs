//! Native stack-safety and large-query tests beyond the oracle's recursive
//! implementation. All graphs use real SQLite and close before temp cleanup.
use codegraph_db::Store;
use codegraph_graph::{GraphQueryManager, GraphTraverser};
use codegraph_types::{Edge, EdgeKind, Language, Node, NodeKind, TraversalOptions};

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
