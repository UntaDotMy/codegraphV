use codegraph_db::{OpenOptions, Store};
use codegraph_types::{Language, Node, NodeKind, SearchOptions};

fn nodes() -> Vec<Node> {
    ["getUser", "signInWithGoogle", "run"]
        .into_iter()
        .enumerate()
        .map(|(i, name)| {
            Node::new(
                format!("n{i}"),
                NodeKind::Function,
                name,
                format!("Api::{name}"),
                "src/api.rs",
                Language::Rust,
                i as u32 + 1,
                i as u32 + 1,
                0,
                0,
                1,
            )
        })
        .collect()
}

#[test]
fn fts_failure_falls_back_to_like_and_fuzzy_search() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    store.insert_nodes(&nodes()).unwrap();
    // A real failed FTS query, not a DB mock or a substitute search function.
    store.conn().execute_batch("DROP TABLE nodes_fts").unwrap();
    let like = store
        .search_nodes("InWith", &SearchOptions::default())
        .unwrap();
    assert_eq!(like[0].node.name, "signInWithGoogle");
    let fuzzy = store
        .search_nodes("getUsr", &SearchOptions::default())
        .unwrap();
    assert_eq!(fuzzy[0].node.name, "getUser");
    assert!(store
        .search_nodes("unrelatedName", &SearchOptions::default())
        .unwrap()
        .is_empty());
    store.close().unwrap();
}

#[test]
fn ranked_queries_work_on_a_read_only_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("codegraph.db");
    let store = Store::initialize(&path).unwrap();
    store.insert_nodes(&nodes()).unwrap();
    store.close().unwrap();
    let store = Store::open(&path, OpenOptions { read_only: true }).unwrap();
    let before = store.conn().total_changes();
    assert_eq!(
        store
            .search_nodes("getUser", &SearchOptions::default())
            .unwrap()[0]
            .node
            .name,
        "getUser"
    );
    assert_eq!(
        store
            .search_nodes("kind:function lang:rust", &SearchOptions::default())
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        store
            .find_nodes_by_exact_name(&["getUser".into()], &SearchOptions::default())
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .find_nodes_by_name_substring("InWith", &SearchOptions::default(), true)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(store.conn().total_changes(), before);
    store.close().unwrap();
}
