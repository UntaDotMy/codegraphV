//! Upstream 0d7480f8: an overload chosen by an argument that calls the same overloaded method
//! must not re-enter the chooser forever. Ported from `__tests__/cpp-overload-argument-reentry.test.ts`.
use codegraph_core::CodeGraph;
use codegraph_types::EdgeKind;
use std::fs;

#[test]
fn cpp_overload_argument_reentry_indexes_and_links_the_calls() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/builder.h"),
        r#"#pragma once

class Builder {
 public:
  struct Extra {
    int prev() const { return prev_; }
    int next() const { return next_; }
    void set_prev(int prev) { prev_ = prev; }
    void set_next(int next) { next_ = next; }
    int prev_;
    int next_;
  };

  void unlink(int id) {
    extras(extras(id).prev()).set_next(extras(id).next());
    extras(extras(id).next()).set_prev(extras(id).prev());
  }

 private:
  const Extra &extras(int id) const { return extras_[id]; }
  Extra &extras(int id) { return extras_[id]; }

  Extra extras_[16];
};
"#,
    )
    .unwrap();
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let nodes = cg.store().get_all_nodes().unwrap();
    let unlink = nodes.iter().find(|n| n.file_path == "src/builder.h" && n.name == "unlink").expect("unlink is indexed");
    let called: Vec<_> = cg.store().get_outgoing_edges(&unlink.id, &[EdgeKind::Calls], None).unwrap()
        .into_iter()
        .filter_map(|e| nodes.iter().find(|n| n.id == e.target))
        .map(|n| n.name.clone())
        .collect();
    for name in ["extras", "set_next", "set_prev"] {
        assert!(called.iter().any(|n| n == name), "{name} missing from {called:?}");
    }
    cg.close();
}
