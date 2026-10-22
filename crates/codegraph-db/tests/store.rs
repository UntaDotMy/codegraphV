//! Behavior of the store against a real SQLite file.

use codegraph_db::{FileBundle, OpenOptions, Store, CURRENT_SCHEMA_VERSION};
use codegraph_types::{
    Edge, EdgeKind, FileRecord, Language, Node, NodeKind, ReferenceKind, UnresolvedReference, Visibility,
};
use std::collections::HashSet;

fn tmp() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join(".codegraph").join("codegraph.db");
    (dir, db)
}

fn func(id: &str, name: &str, file: &str, line: u32) -> Node {
    let mut n = Node::new(id, NodeKind::Function, name, format!("{file}::{name}"), file, Language::TypeScript, line, line + 2, 0, 1, 1);
    n.signature = Some(format!("{name}()"));
    n.visibility = Some(Visibility::Public);
    n.is_exported = Some(true);
    n.decorators = Some(vec!["@a".into(), "@b".into()]);
    n
}

fn file_rec(path: &str, nodes: u32) -> FileRecord {
    FileRecord {
        path: path.into(),
        content_hash: "h".into(),
        language: Language::TypeScript,
        size: 10,
        modified_at: 5,
        indexed_at: 6,
        node_count: nodes,
        errors: None,
        generated: None,
    }
}

fn index_names(store: &Store) -> HashSet<String> {
    let mut stmt = store.conn().prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND name LIKE 'idx_%'").unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0)).unwrap().map(|r| r.unwrap()).collect()
}

fn trigger_count(store: &Store) -> i64 {
    store.conn().query_row("SELECT count(*) FROM sqlite_master WHERE type = 'trigger'", [], |r| r.get(0)).unwrap()
}

#[test]
fn initialize_creates_a_current_versioned_wal_database() {
    let (_d, db) = tmp();
    let store = Store::initialize(&db).unwrap();
    assert!(store.fts5_available());
    assert_eq!(store.journal_mode().unwrap(), "wal");
    assert_eq!(store.schema_version().unwrap().unwrap().version, CURRENT_SCHEMA_VERSION);
    assert_eq!(trigger_count(&store), 3);
    drop(store);
    // Re-opening is a no-op for migrations and keeps the version.
    let store = Store::open(&db, OpenOptions::default()).unwrap();
    assert_eq!(store.schema_version().unwrap().unwrap().version, CURRENT_SCHEMA_VERSION);
}

#[test]
fn open_missing_database_is_an_error() {
    let (_d, db) = tmp();
    assert!(Store::open(&db, OpenOptions::default()).is_err());
}

#[test]
fn nodes_round_trip_every_field() {
    let (_d, db) = tmp();
    let store = Store::initialize(&db).unwrap();
    let mut n = func("function:1", "greet", "src/a.ts", 3);
    n.docstring = Some("says hi".into());
    n.is_async = Some(true);
    n.type_parameters = Some(vec!["T".into()]);
    n.return_type = Some("string".into());
    store.insert_node(&n).unwrap();
    store.clear_cache();
    let got = store.get_node_by_id("function:1").unwrap().unwrap();
    assert_eq!(got.name, "greet");
    assert_eq!(got.docstring.as_deref(), Some("says hi"));
    assert_eq!(got.visibility, Some(Visibility::Public));
    assert_eq!(got.decorators, Some(vec!["@a".to_string(), "@b".to_string()]));
    assert_eq!(got.type_parameters, Some(vec!["T".to_string()]));
    assert_eq!(got.return_type.as_deref(), Some("string"));
    assert_eq!(got.is_async, Some(true));
    assert_eq!(got.is_static, Some(false));
    assert_eq!(got.is_exported, Some(true));
    assert_eq!(got.qualified_name, "src/a.ts::greet");
    assert!(store.get_node_by_id("nope").unwrap().is_none());
}

#[test]
fn batched_inserts_keep_input_order_across_chunk_sizes() {
    let (_d, db) = tmp();
    let store = Store::initialize(&db).unwrap();
    // 128 + 32 + 8 + 1 + a remainder: exercises every cached statement size.
    let nodes: Vec<Node> = (0..200).map(|i| func(&format!("function:{i:03}"), &format!("f{i}"), "src/a.ts", i + 1)).collect();
    store.insert_nodes(&nodes).unwrap();
    let rowids: Vec<String> = {
        let mut stmt = store.conn().prepare("SELECT id FROM nodes ORDER BY rowid").unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0)).unwrap().map(|r| r.unwrap()).collect()
    };
    let expected: Vec<String> = nodes.iter().map(|n| n.id.clone()).collect();
    assert_eq!(rowids, expected);
    assert_eq!(store.get_nodes_by_file("src/a.ts").unwrap().len(), 200);
}

#[test]
fn fts_triggers_index_inserts_updates_and_deletes() {
    let (_d, db) = tmp();
    let store = Store::initialize(&db).unwrap();
    let mut n = func("function:1", "frobnicate", "src/a.ts", 1);
    store.insert_node(&n).unwrap();
    let hits = |q: &str| -> i64 {
        store.conn().query_row("SELECT count(*) FROM nodes_fts WHERE nodes_fts MATCH ?", [q], |r| r.get(0)).unwrap()
    };
    assert_eq!(hits("frobnicate"), 1);
    // qualified_name and signature are FTS columns too, so a rename carries them.
    n.name = "quuxify".into();
    n.qualified_name = "src/a.ts::quuxify".into();
    n.signature = Some("quuxify()".into());
    store.update_node(&n).unwrap();
    assert_eq!(hits("frobnicate"), 0);
    assert_eq!(hits("quuxify"), 1);
    store.delete_node("function:1").unwrap();
    assert_eq!(hits("quuxify"), 0);
}

#[test]
fn bulk_node_load_window_rebuilds_fts_and_restores_triggers() {
    let (_d, db) = tmp();
    let store = Store::initialize(&db).unwrap();
    store.begin_bulk_node_load().unwrap();
    assert_eq!(trigger_count(&store), 0);
    store.insert_node(&func("function:1", "needle", "src/a.ts", 1)).unwrap();
    store.end_bulk_node_load().unwrap();
    assert_eq!(trigger_count(&store), 3);
    let hits: i64 = store
        .conn()
        .query_row("SELECT count(*) FROM nodes_fts WHERE nodes_fts MATCH 'needle'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(hits, 1);
}

#[test]
fn a_bulk_window_left_open_by_a_crash_is_healed_on_open() {
    let (_d, db) = tmp();
    let before = {
        let store = Store::initialize(&db).unwrap();
        let before = index_names(&store);
        store.begin_bulk_parse_load().unwrap();
        store.begin_bulk_node_load().unwrap();
        store.begin_bulk_ref_load().unwrap();
        assert!(index_names(&store).len() < before.len());
        before
        // dropped without the matching end_* calls: the "crash".
    };
    let store = Store::open(&db, OpenOptions::default()).unwrap();
    assert_eq!(index_names(&store), before);
    assert_eq!(trigger_count(&store), 3);
}

#[test]
fn parse_load_window_round_trips_the_index_set() {
    let (_d, db) = tmp();
    let store = Store::initialize(&db).unwrap();
    let before = index_names(&store);
    store.begin_bulk_parse_load().unwrap();
    store.begin_bulk_ref_load().unwrap();
    store.end_bulk_ref_load().unwrap();
    store.end_bulk_parse_load().unwrap();
    assert_eq!(index_names(&store), before);
    // The synthesis-site index can be deferred, then created on its own.
    store.begin_bulk_edge_load().unwrap();
    store.end_bulk_edge_load(true).unwrap();
    assert!(!index_names(&store).contains("idx_edges_synthesis_site"));
    store.create_synthesis_site_index().unwrap();
    assert_eq!(index_names(&store), before);
}

#[test]
fn edges_are_deduped_and_filtered_to_existing_endpoints() {
    let (_d, db) = tmp();
    let store = Store::initialize(&db).unwrap();
    store.insert_nodes(&[func("function:a", "a", "src/a.ts", 1), func("function:b", "b", "src/a.ts", 5)]).unwrap();
    let mut e = Edge::new("function:a", "function:b", EdgeKind::Calls);
    e.line = Some(2);
    e.column = Some(4);
    store.insert_edges(&[e.clone(), e.clone(), Edge::new("function:a", "function:missing", EdgeKind::Calls)]).unwrap();
    let out = store.get_outgoing_edges("function:a", &[], None).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0], e);
    // Coordinate-less edges dedup too (IFNULL in the identity index).
    let bare = Edge::new("function:a", "function:b", EdgeKind::References);
    store.insert_edge(&bare).unwrap();
    store.insert_edge(&bare).unwrap();
    assert_eq!(store.get_outgoing_edges("function:a", &[EdgeKind::References], None).unwrap().len(), 1);
    assert_eq!(store.get_incoming_edges("function:b", &[]).unwrap().len(), 2);
    let counts = store.count_incoming_edges(&["function:b".to_string(), "function:a".to_string()]).unwrap();
    assert_eq!(counts.get("function:b"), Some(&2));
    assert!(!counts.contains_key("function:a"));
}

#[test]
fn deleting_a_file_cascades_to_edges_and_refs() {
    let (_d, db) = tmp();
    let store = Store::initialize(&db).unwrap();
    let nodes = [func("function:a", "a", "src/a.ts", 1), func("function:b", "b", "src/b.ts", 1)];
    let edges = [Edge::new("function:a", "function:b", EdgeKind::Calls)];
    let refs = [UnresolvedReference {
        from_node_id: "function:a".into(),
        reference_name: "ghost".into(),
        reference_kind: ReferenceKind::Edge(EdgeKind::Calls),
        line: 3,
        column: 1,
        file_path: Some("src/a.ts".into()),
        language: Some(Language::TypeScript),
        candidates: Some(vec!["x".into()]),
        row_id: None,
    }];
    store
        .store_file_bundle(&FileBundle { nodes: &nodes[..1], edges: &[], refs: &refs, file: &file_rec("src/a.ts", 1) })
        .unwrap();
    store
        .store_file_bundle(&FileBundle { nodes: &nodes[1..], edges: &edges, refs: &[], file: &file_rec("src/b.ts", 1) })
        .unwrap();
    assert_eq!(store.get_unresolved_references().unwrap().len(), 1);
    let r = &store.get_unresolved_by_name("ghost").unwrap()[0];
    assert_eq!(r.candidates, Some(vec!["x".to_string()]));
    assert!(r.row_id.is_some());
    assert_eq!(store.get_node_and_edge_count().unwrap(), (2, 1));

    store.delete_file("src/a.ts").unwrap();
    assert_eq!(store.get_node_and_edge_count().unwrap(), (1, 0));
    assert!(store.get_unresolved_references().unwrap().is_empty());
    assert!(store.get_file_by_path("src/a.ts").unwrap().is_none());
    assert!(store.get_file_by_path("src/b.ts").unwrap().is_some());
}

#[test]
fn upsert_file_rewrites_the_generated_flag() {
    let (_d, db) = tmp();
    let store = Store::initialize(&db).unwrap();
    let mut f = file_rec("gen.go", 0);
    f.generated = Some(true);
    store.upsert_file(&f).unwrap();
    assert_eq!(store.count_generated_files().unwrap(), 1);
    assert!(store.get_generated_paths_among(&["gen.go", "x.go"]).unwrap().contains("gen.go"));
    f.generated = None;
    store.upsert_file(&f).unwrap();
    assert_eq!(store.count_generated_files().unwrap(), 0);
}

#[test]
fn transactions_flatten_and_roll_back_as_one() {
    let (_d, db) = tmp();
    let store = Store::initialize(&db).unwrap();
    let r: Result<(), _> = store.transaction(|| {
        store.insert_node(&func("function:1", "a", "src/a.ts", 1))?;
        store.transaction(|| store.insert_node(&func("function:2", "b", "src/a.ts", 2)))?;
        assert!(store.in_transaction());
        Err(codegraph_db::DbError::Other("boom".into()))
    });
    assert!(r.is_err());
    assert!(!store.in_transaction());
    assert_eq!(store.get_node_and_edge_count().unwrap(), (0, 0));
    store.transaction(|| store.insert_node(&func("function:1", "a", "src/a.ts", 1))).unwrap();
    assert_eq!(store.get_node_and_edge_count().unwrap(), (1, 0));
}

#[test]
fn name_segment_vocab_follows_inserts_but_skips_file_and_import_nodes() {
    let (_d, db) = tmp();
    let store = Store::initialize(&db).unwrap();
    assert!(store.is_name_segment_vocab_empty().unwrap());
    store.insert_node(&func("function:1", "OrderStateMachine", "src/a.ts", 1)).unwrap();
    let mut file = func("file:1", "OrderFile", "src/a.ts", 1);
    file.kind = NodeKind::File;
    store.insert_node(&file).unwrap();
    let segs: Vec<(String, String)> = {
        let mut stmt = store.conn().prepare("SELECT segment, name FROM name_segment_vocab ORDER BY segment").unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|r| r.unwrap()).collect()
    };
    let names: HashSet<&str> = segs.iter().map(|(_, n)| n.as_str()).collect();
    assert_eq!(names, HashSet::from(["OrderStateMachine"]));
    let seg_words: Vec<&str> = segs.iter().map(|(s, _)| s.as_str()).collect();
    assert_eq!(seg_words, ["machine", "order", "state"]);
}

#[test]
fn read_only_open_skips_writes_and_maintenance() {
    let (_d, db) = tmp();
    {
        let store = Store::initialize(&db).unwrap();
        store.insert_node(&func("function:1", "a", "src/a.ts", 1)).unwrap();
    }
    let ro = Store::open(&db, OpenOptions { read_only: true }).unwrap();
    assert!(ro.is_read_only());
    assert!(ro.get_node_by_id("function:1").unwrap().is_some());
    assert!(ro.insert_node(&func("function:2", "b", "src/a.ts", 2)).is_err());
}

#[test]
fn metadata_and_stats() {
    let (_d, db) = tmp();
    let store = Store::initialize(&db).unwrap();
    assert_eq!(store.get_metadata("k").unwrap(), None);
    store.set_metadata("k", "v1").unwrap();
    store.set_metadata("k", "v2").unwrap();
    assert_eq!(store.get_metadata("k").unwrap().as_deref(), Some("v2"));
    store.insert_node(&func("function:1", "a", "src/a.ts", 1)).unwrap();
    store.upsert_file(&file_rec("src/a.ts", 1)).unwrap();
    let stats = store.get_stats().unwrap();
    assert_eq!((stats.node_count, stats.edge_count, stats.file_count), (1, 0, 1));
    assert_eq!(stats.nodes_by_kind.get(&NodeKind::Function), Some(&1));
    assert_eq!(stats.files_by_language.get(&Language::TypeScript), Some(&1));
    assert!(stats.db_size_bytes > 0);
}

#[test]
fn name_queries_are_ordered_by_code_position() {
    let (_d, db) = tmp();
    let store = Store::initialize(&db).unwrap();
    // Insert in the "wrong" order: resolution must see a code-determined order.
    store
        .insert_nodes(&[
            func("function:z", "dup", "src/z.ts", 1),
            func("function:a2", "dup", "src/a.ts", 9),
            func("function:a1", "dup", "src/a.ts", 2),
        ])
        .unwrap();
    let ids: Vec<String> = store.get_nodes_by_name("dup").unwrap().into_iter().map(|n| n.id).collect();
    assert_eq!(ids, ["function:a1", "function:a2", "function:z"]);
    assert_eq!(store.get_nodes_by_lower_name("DUP").unwrap().len(), 3);
    assert_eq!(store.get_nodes_by_name_prefix("du", 10).unwrap().len(), 3);
}

#[test]
fn removing_database_files_takes_the_wal_sidecars_too() {
    let (_d, db) = tmp();
    {
        let store = Store::initialize(&db).unwrap();
        store.insert_node(&func("function:1", "a", "src/a.ts", 1)).unwrap();
    }
    codegraph_db::remove_database_files(&db).unwrap();
    assert!(!db.exists());
    assert!(!db.with_extension("db-wal").exists());
    // Removing again is fine.
    codegraph_db::remove_database_files(&db).unwrap();
}
