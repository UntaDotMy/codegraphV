//! explore connects a flow through a C++ template parameter (upstream 01a4931): the hop names the
//! argument, the parameter and the site that writes it.
use codegraph_mcp::render::explore;
use serde_json::json;
use std::collections::HashMap;

#[test]
fn connects_a_flow_through_the_template() {
    let dir = tempfile::tempdir().unwrap();
    let files = [
        ("txn.h", "class BaseTxn {\n public:\n  void Commit() {}\n};\nclass PlainTxn : public BaseTxn {\n public:\n  void Put(int k) {}\n  void Put(int k, int v) {}\n  static void Make() {}\n};\n"),
        ("mixin.h", "#include \"txn.h\"\ntemplate <typename Txn>\nclass Mixin : public Txn {\n public:\n  void Add(int k) { Txn::Put(k); }\n  void Finish() { Txn::Commit(); }\n  static void Build() { Txn::Make(); }\n};\n"),
        ("use.cc", "#include \"mixin.h\"\nvoid Start() { Mixin<PlainTxn> m; m.Add(1); }\n"),
    ];
    for (rel, text) in files {
        std::fs::write(dir.path().join(rel), text).unwrap();
    }
    let mut cg = codegraph_core::CodeGraph::init(dir.path().to_str().unwrap()).unwrap();
    cg.index_all(&Default::default(), None, None).unwrap();
    let result = explore(&cg, &json!({ "query": "Start Mixin::Add PlainTxn::Put" }), &mut HashMap::new()).unwrap();
    let text = result.content[0]["text"].as_str().unwrap().to_string();
    assert!(text.contains("template argument `PlainTxn` for `Txn`"), "{text}");
    assert!(text.contains("use.cc:2"), "{text}");
}
