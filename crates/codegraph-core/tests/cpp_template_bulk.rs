//! The C++ template-argument edges (upstream 01a4931) are the same, in the same order, whether a
//! resolution run takes its bulk path or not (upstream's "with the resolver pool and without";
//! native has no pool, and `CODEGRAPHV_PARALLEL_RESOLVE_MIN` picks the bulk path instead). One test
//! in its own binary: it sets that variable for the process.
use codegraph_core::CodeGraph;

fn edges(files: &[(&str, &str)]) -> Vec<(String, String, Option<String>)> {
    let dir = tempfile::tempdir().unwrap();
    for (rel, text) in files {
        std::fs::write(dir.path().join(rel), text).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let conn = cg.store().conn();
    let mut stmt = conn
        .prepare(
            "SELECT s.qualified_name, t.qualified_name, json_extract(e.metadata, '$.registeredAt')
               FROM edges e JOIN nodes s ON s.id = e.source JOIN nodes t ON t.id = e.target
              WHERE json_extract(e.metadata, '$.synthesizedBy') = 'cpp-template-argument' ORDER BY e.id",
        )
        .unwrap();
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(Result::unwrap).collect();
    rows
}

#[test]
fn gives_the_same_edges_in_the_same_order_on_the_bulk_path_and_off_it() {
    let files = [
        ("txn.h", "class BaseTxn {\n public:\n  void Commit() {}\n};\nclass PlainTxn : public BaseTxn {\n public:\n  void Put(int k) {}\n  static void Make() {}\n};\nclass FancyTxn : public BaseTxn {\n public:\n  void Put(int k) {}\n  static void Make() {}\n};\n"),
        ("mixin.h", "#include \"txn.h\"\ntemplate <typename Txn>\nclass Mixin : public Txn {\n public:\n  void Add(int k) { Txn::Put(k); }\n  void Finish() { Txn::Commit(); }\n  static void Build() { Txn::Make(); }\n};\n"),
        ("b.cc", "#include \"mixin.h\"\nMixin<FancyTxn> b;\nMixin<PlainTxn> again;\n"),
        ("a.cc", "#include \"mixin.h\"\nMixin<PlainTxn> a;\n"),
    ];
    std::env::set_var("CODEGRAPHV_PARALLEL_RESOLVE_MIN", "999999999");
    let plain = edges(&files);
    std::env::set_var("CODEGRAPHV_PARALLEL_RESOLVE_MIN", "0");
    let bulk = edges(&files);
    std::env::remove_var("CODEGRAPHV_PARALLEL_RESOLVE_MIN");
    assert!(!plain.is_empty());
    assert_eq!(bulk, plain);
}
