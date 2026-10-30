//! A name-only method match never binds a delegating call to its own caller (upstream 0b194448).
//!
//! With one method of a name in the project, the resolver's by-name guess took it outright, so a
//! wrapper handing its call on (`wrapped.unbind(value)` inside `unbind`) linked to the method
//! making the call and looked recursive. A receiver that is another object is not the enclosing
//! method's own instance, so that guess is dropped; `this`, a bare call and Go's typed receivers
//! are real recursion and stay.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

fn index(files: &[(&str, &str)]) -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    for (relative, content) in files {
        let path = dir.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

/// The names of the methods and functions in `file` that call themselves.
fn self_loops(cg: &CodeGraph, file: &str) -> Vec<String> {
    let mut names = Vec::new();
    for node in cg.get_nodes_by_file(file).unwrap() {
        if !matches!(node.kind, NodeKind::Method | NodeKind::Function) {
            continue;
        }
        if cg.get_outgoing_edges(&node.id).unwrap().iter().any(|e| e.kind == EdgeKind::Calls && e.target == node.id) {
            names.push(node.name.clone());
        }
    }
    names.sort();
    names
}

#[test]
fn a_scala_delegating_wrapper_has_no_self_loop() {
    let (_dir, cg) = index(&[(
        "Form.scala",
        "package play.api.data\nclass WrappedMapping(wrapped: Mapping) {\n  def unbind(value: String): String = wrapped.unbind(value)\n}\n",
    )]);
    assert_eq!(self_loops(&cg, "Form.scala"), Vec::<String>::new());
}

#[test]
fn a_typescript_wrapper_handing_its_call_to_a_parameter_has_no_self_loop() {
    let (_dir, cg) = index(&[(
        "wrapped.ts",
        "export class WrappedMapping {\n  unbind(wrapped, value: string): string {\n    return wrapped.unbind(value);\n  }\n}\n",
    )]);
    assert_eq!(self_loops(&cg, "wrapped.ts"), Vec::<String>::new());
}

#[test]
fn real_recursion_through_this_a_bare_call_and_a_go_named_receiver_stays() {
    let (_dir, cg) = index(&[
        (
            "Tree.java",
            "public class Tree {\n  public int depth(int n) { return n == 0 ? 0 : this.depth(n - 1); }\n  public int size(int n) { return n == 0 ? 0 : size(n - 1); }\n}\n",
        ),
        (
            "relay.go",
            "package relay\ntype relayClient struct{}\nfunc (r *relayClient) sendBatchAttempt(retry bool) error {\n\tif retry {\n\t\treturn r.sendBatchAttempt(false)\n\t}\n\treturn nil\n}\n",
        ),
    ]);
    assert_eq!(self_loops(&cg, "Tree.java"), ["depth", "size"]);
    assert_eq!(self_loops(&cg, "relay.go"), ["sendBatchAttempt"]);
}

#[test]
fn go_recursion_through_a_range_variable_over_the_types_own_children_stays() {
    // cobra's `sub.checkCommandGroups()` over `c.commands`, etcd's `child.Repr(...)`: the loop
    // variable is untyped, and it is the caller's type.
    let (_dir, cg) = index(&[(
        "node.go",
        "package tree\ntype Node struct{ children []*Node }\nfunc (n *Node) Size() int {\n\ttotal := 1\n\tfor _, child := range n.children {\n\t\ttotal += child.Size()\n\t}\n\treturn total\n}\n",
    )]);
    assert_eq!(self_loops(&cg, "node.go"), ["Size"]);
}
