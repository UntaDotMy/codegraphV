//! Port of `__tests__/explore-cobol-copybook.test.ts`'s derivation-level
//! assertions (#2342). The explore/context renderers are not ported yet; this
//! pins what they both read.
use codegraph_db::Store;
use codegraph_graph::cobol_copybooks::find_named_copybooks;
use codegraph_types::{Edge, EdgeKind, FileRecord, Language, Node, NodeKind};

fn file_node(path: &str, name: &str) -> Node {
    Node::new(
        format!("file:{path}"),
        NodeKind::File,
        name,
        path,
        path,
        Language::Cobol,
        1,
        1,
        0,
        0,
        0,
    )
}

fn include_node(path: &str, member: &str, line: u32) -> Node {
    Node::new(
        format!("include:{path}:{line}"),
        NodeKind::Import,
        member,
        format!("{path}::{member}"),
        path,
        Language::Cobol,
        line,
        line,
        0,
        0,
        0,
    )
}

fn record(path: &str) -> FileRecord {
    FileRecord {
        path: path.into(),
        content_hash: "h".into(),
        language: Language::Cobol,
        size: 10,
        modified_at: 5,
        indexed_at: 6,
        node_count: 1,
        errors: None,
        generated: None,
    }
}

#[test]
fn finds_an_indexed_copybook_and_every_include_site() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    for path in ["cpy/CVACT01Y.cpy", "cbl/ACCTUPDT.cbl", "cbl/ACCTVIEW.cbl"] {
        store.upsert_file(&record(path)).unwrap();
    }
    store
        .insert_nodes(&[
            file_node("cpy/CVACT01Y.cpy", "CVACT01Y.cpy"),
            include_node("cbl/ACCTUPDT.cbl", "CVACT01Y", 5),
            include_node("cbl/ACCTVIEW.cbl", "CVACT01Y", 5),
        ])
        .unwrap();

    let found = find_named_copybooks(&store, "CVACT01Y").unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].member, "CVACT01Y");
    assert_eq!(found[0].files.len(), 1);
    assert_eq!(found[0].files[0].file_path, "cpy/CVACT01Y.cpy");
    let sites: Vec<&str> = found[0]
        .includes
        .iter()
        .map(|n| n.file_path.as_str())
        .collect();
    assert_eq!(sites, ["cbl/ACCTUPDT.cbl", "cbl/ACCTVIEW.cbl"]);
    store.close().unwrap();
}

#[test]
fn reports_an_include_whose_copybook_is_not_indexed() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    store.upsert_file(&record("TESTPROG.cbl")).unwrap();
    store
        .insert_nodes(&[include_node("TESTPROG.cbl", "MYCOPYBOOK", 6)])
        .unwrap();

    let found = find_named_copybooks(&store, "MYCOPYBOOK").unwrap();
    assert_eq!(found.len(), 1);
    assert!(found[0].files.is_empty());
    assert_eq!(found[0].includes.len(), 1);
    assert_eq!(found[0].includes[0].start_line, 6);
    store.close().unwrap();
}

#[test]
fn does_not_treat_a_same_named_program_as_a_copybook() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    store.upsert_file(&record("cbl/CVACT01Y.cbl")).unwrap();
    store
        .insert_nodes(&[file_node("cbl/CVACT01Y.cbl", "CVACT01Y.cbl")])
        .unwrap();

    assert!(find_named_copybooks(&store, "CVACT01Y").unwrap().is_empty());
    store.close().unwrap();
}

#[test]
fn accepts_a_file_an_include_actually_resolved_to() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    store.upsert_file(&record("shared/SHARED.CBL")).unwrap();
    store.upsert_file(&record("cbl/USER.cbl")).unwrap();
    store
        .insert_nodes(&[
            file_node("shared/SHARED.CBL", "SHARED.CBL"),
            include_node("cbl/USER.cbl", "SHARED", 3),
        ])
        .unwrap();
    store
        .insert_edges(&[Edge::new(
            "include:cbl/USER.cbl:3",
            "file:shared/SHARED.CBL",
            EdgeKind::Imports,
        )])
        .unwrap();

    let found = find_named_copybooks(&store, "SHARED").unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].files.len(), 1);
    assert_eq!(found[0].files[0].file_path, "shared/SHARED.CBL");
    store.close().unwrap();
}

#[test]
fn answers_nothing_in_a_project_without_cobol() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    assert!(find_named_copybooks(&store, "CVACT01Y").unwrap().is_empty());
    store.close().unwrap();
}
