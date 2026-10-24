use codegraph_db::{DbError, OpenOptions, Store};
use codegraph_types::{Edge, EdgeKind, Language, Node, NodeKind};

fn setup() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    for id in ["a", "b"] {
        store
            .insert_node(&Node::new(
                id,
                NodeKind::Function,
                id,
                id,
                format!("src/{id}.rs"),
                Language::Rust,
                1,
                1,
                0,
                0,
                1,
            ))
            .unwrap();
    }
    add_edges(&store, "a", 20);
    add_edges(&store, "b", 30);
    (dir, store)
}

fn add_edges(store: &Store, id: &str, count: u32) {
    let edges: Vec<_> = (1..=count)
        .map(|line| {
            let mut e = Edge::new(id, id, EdgeKind::Calls);
            e.line = Some(line);
            e
        })
        .collect();
    store.insert_edges(&edges).unwrap();
}

#[test]
fn cached_ranking_tracks_this_connections_writes() {
    let (_dir, store) = setup();
    assert_eq!(
        store.get_dominant_file().unwrap().unwrap().file_path,
        "src/b.rs"
    );
    assert_eq!(store.get_dominant_file().unwrap().unwrap().edge_count, 30);
    add_edges(&store, "a", 60);
    let dominant = store.get_dominant_file().unwrap().unwrap();
    assert_eq!(
        (
            dominant.file_path.as_str(),
            dominant.edge_count,
            dominant.next_edge_count
        ),
        ("src/a.rs", 60, 30)
    );
    store.close().unwrap();
}

#[test]
fn cached_ranking_tracks_another_connections_commit() {
    let (dir, store) = setup();
    assert_eq!(
        store.get_dominant_file().unwrap().unwrap().file_path,
        "src/b.rs"
    );
    let writer = Store::open(dir.path().join("codegraph.db"), OpenOptions::default()).unwrap();
    add_edges(&writer, "a", 60);
    assert_eq!(
        store.get_dominant_file().unwrap().unwrap().file_path,
        "src/a.rs"
    );
    writer.close().unwrap();
    store.close().unwrap();
}

#[test]
fn a_rolled_back_ranking_is_not_memoized() {
    let (_dir, store) = setup();
    assert_eq!(
        store.get_dominant_file().unwrap().unwrap().file_path,
        "src/b.rs"
    );
    let result: codegraph_db::Result<()> = store.transaction(|| {
        add_edges(&store, "a", 60);
        assert_eq!(store.get_dominant_file()?.unwrap().file_path, "src/a.rs");
        Err(DbError::Other("rollback".into()))
    });
    assert!(result.is_err());
    assert_eq!(
        store.get_dominant_file().unwrap().unwrap().file_path,
        "src/b.rs"
    );
    store.close().unwrap();
}

/// A fresh connection (a one-shot CLI call) reuses the verdict persisted by an earlier one
/// instead of re-aggregating every edge; an edge write makes it recompute.
#[test]
fn persisted_ranking_is_reused_until_edges_change() {
    let (dir, store) = setup();
    assert_eq!(store.get_dominant_file().unwrap().unwrap().file_path, "src/b.rs");
    let stored = store.get_metadata(codegraph_db::DOMINANT_FILE_METADATA_KEY).unwrap().expect("verdict persisted");
    // Rewrite the stored verdict: only a read of the persisted value can return it.
    let mut doc: serde_json::Value = serde_json::from_str(&stored).unwrap();
    doc["value"]["filePath"] = "src/persisted.rs".into();
    store.set_metadata(codegraph_db::DOMINANT_FILE_METADATA_KEY, &doc.to_string()).unwrap();
    store.close().unwrap();

    let fresh = Store::open(dir.path().join("codegraph.db"), OpenOptions::default()).unwrap();
    assert_eq!(fresh.get_dominant_file().unwrap().unwrap().file_path, "src/persisted.rs");
    add_edges(&fresh, "a", 60);
    assert_eq!(fresh.get_dominant_file().unwrap().unwrap().file_path, "src/a.rs");
    fresh.close().unwrap();
}
