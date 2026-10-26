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

#[test]
fn fts_rank_iterator_preserves_weighted_bm25_scores_and_pagination() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    let mut data = nodes();
    data[0].docstring = Some("account account profile".into());
    data[1].docstring = Some("account sign in".into());
    data[2].docstring = Some("profile account account account".into());
    store.insert_nodes(&data).unwrap();
    let query = "\"account\"* OR \"profile\"*";
    let read = |sql: &str, limit: usize, offset: usize| -> Vec<(String, f64)> {
        store
            .conn()
            .prepare(sql)
            .unwrap()
            .query_map(rusqlite::params![query, limit, offset], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let old = "SELECT nodes.id, bm25(nodes_fts, 0, 20, 5, 1, 2) AS score FROM nodes_fts JOIN nodes ON nodes_fts.id = nodes.id WHERE nodes_fts MATCH ? ORDER BY score LIMIT ? OFFSET ?";
    let ranked = "SELECT nodes.id, nodes_fts.rank AS score FROM nodes_fts JOIN nodes ON nodes_fts.id = nodes.id WHERE nodes_fts MATCH ? AND nodes_fts.rank MATCH 'bm25(0, 20, 5, 1, 2)' ORDER BY nodes_fts.rank LIMIT ? OFFSET ?";
    assert!(!read(old, 10, 0).is_empty());
    for (limit, offset) in [(10, 0), (1, 0), (1, 1), (2, 1)] {
        assert_eq!(read(old, limit, offset), read(ranked, limit, offset));
    }
    // A docstring-only match cannot be supplied by the name LIKE fallback:
    // this also detects accidental FTS errors swallowed by search_nodes.
    assert_eq!(
        store
            .search_nodes("account", &SearchOptions::default())
            .unwrap()
            .len(),
        3
    );
    store.close().unwrap();
}

#[test]
fn ranked_fts_preserves_tied_results_with_kind_and_language_filters() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    let data: Vec<_> = (0..120)
        .map(|i| {
            let mut node = Node::new(
                format!("id_{i}"),
                if i % 3 == 0 {
                    NodeKind::Method
                } else {
                    NodeKind::Function
                },
                format!("item_{i:03}"),
                format!("Api::item_{i:03}"),
                "src/api.rs",
                if i % 4 == 0 {
                    Language::TypeScript
                } else {
                    Language::Rust
                },
                i + 1,
                i + 1,
                0,
                0,
                1,
            );
            node.docstring = Some("account profile".into());
            node
        })
        .collect();
    store.insert_nodes(&data).unwrap();
    let base = "FROM nodes_fts JOIN nodes ON nodes_fts.id = nodes.id WHERE nodes_fts MATCH ? AND nodes.kind = 'function' AND nodes.language = 'rust'";
    let old = format!("SELECT nodes.id, bm25(nodes_fts, 0, 20, 5, 1, 2) AS score {base} ORDER BY score LIMIT 11 OFFSET 9");
    let new = format!("SELECT nodes.id, nodes_fts.rank AS score {base} AND nodes_fts.rank MATCH 'bm25(0, 20, 5, 1, 2)' ORDER BY nodes_fts.rank LIMIT 11 OFFSET 9");
    let read = |sql: &str| -> Vec<(String, f64)> {
        store
            .conn()
            .prepare(sql)
            .unwrap()
            .query_map(["account"], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let expected = read(&old);
    assert_eq!(expected.len(), 11);
    assert_eq!(read(&new), expected);
    let results = store
        .search_nodes("account kind:function lang:rust", &SearchOptions::default())
        .unwrap();
    assert!(!results.is_empty());
    assert!(results
        .iter()
        .all(|r| r.node.kind == NodeKind::Function && r.node.language == Language::Rust));
    store.close().unwrap();
}
