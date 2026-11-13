//! Swift overloads that differ only in whether they take a function (upstream 45ea6801):
//! `didStop.write(true)` is `write(_ value:)`, `write { $0 = value }` is `write(_ closure:)`,
//! a trailing closure is read after the labels before it, the `{` after a condition is the
//! statement's, and an `@autoclosure` parameter takes a value.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

const PROTECTED: &str = "final class Protected<Value> {
    private var value: Value
    init(_ value: Value) { self.value = value }
    @discardableResult
    func write<U>(_ closure: (inout Value) throws -> U) rethrows -> U {
        try closure(&self.value)
    }
    func write(_ value: Value) {
        write { $0 = value }
    }
}
";

const CACHE: &str = "final class Cache {
    func clear(handler: (@Sendable () -> Void)? = nil) {}
    func clear() async {
        await withCheckedContinuation { continuation in
            clear { continuation.resume() }
        }
    }
    func store(_ image: String, forKey key: String, options: Int, done: @escaping (Int) -> Void) {}
    func store(_ image: String, forKey key: String, done: @escaping (Int) -> Void) {}
    func accept(contentType: @autoclosure () -> String) {}
    func accept(statusCode: Int) {}
    func take(_ items: [() -> Void]) {}
    func take(_ count: Int) {}
    func has(_ key: String, then done: () -> Void) -> Bool { true }
    func has(_ key: String) -> Bool { true }
}
";

const USE: &str = "final class Use {
    func value() {
        let completed = Protected<Int?>(nil)
        completed.write(3)
    }
    func closure() {
        let completed = Protected<Int?>(nil)
        completed.write { $0 = 1 }
    }
    func closureInParens() {
        let completed = Protected<Int?>(nil)
        completed.write({ $0 = 1 })
    }
    func trailingWithLabels(cache: Cache) {
        cache.store(\"a\", forKey: \"k\") { _ in }
    }
    func trailingWithAllLabels(cache: Cache) {
        cache.store(\"a\", forKey: \"k\", options: 1) { _ in }
    }
    func autoclosure(cache: Cache) {
        cache.accept(contentType: \"text\")
    }
    func condition(cache: Cache) {
        if cache.has(\"k\") {
            print(\"yes\")
        }
    }
}
";

fn source(file: &str) -> &'static str {
    match file {
        "Protected.swift" => PROTECTED,
        "Cache.swift" => CACHE,
        _ => USE,
    }
}

/// A declaration's first two lines (its attribute and its `func` line).
fn declaration(file: &str, start_line: u32) -> String {
    source(file).lines().skip(start_line as usize - 1).take(2).collect::<Vec<_>>().join(" ")
}

/// The declarations of `callee` that the method `name` (in `file`, matched by `pick`) calls.
fn called(cg: &CodeGraph, name: &str, file: &str, pick: impl Fn(&str) -> bool, callee: &str) -> String {
    let from = cg
        .get_nodes_by_kind(NodeKind::Method)
        .unwrap()
        .into_iter()
        .find(|n| n.name == name && n.file_path == file && pick(&declaration(file, n.start_line)))
        .unwrap_or_else(|| panic!("no method {name}"));
    cg.get_outgoing_edges(&from.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Calls)
        .filter_map(|e| cg.get_node(&e.target).unwrap())
        .filter(|n| n.name == callee)
        .map(|n| declaration(&n.file_path, n.start_line))
        .collect::<Vec<_>>()
        .join(",")
}

#[test]
fn a_swift_closure_goes_to_the_overload_that_takes_a_function() {
    let dir = tempfile::tempdir().unwrap();
    for (rel, text) in [("Protected.swift", PROTECTED), ("Cache.swift", CACHE), ("Use.swift", USE)] {
        fs::write(dir.path().join(rel), text).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let any = |_: &str| true;
    let use_ = |name: &str, callee: &str| called(&cg, name, "Use.swift", any, callee);

    assert!(use_("value", "write").contains("_ value: Value"), "value: {}", use_("value", "write"));
    assert!(use_("closure", "write").contains("_ closure:"), "closure: {}", use_("closure", "write"));
    assert!(use_("closureInParens", "write").contains("_ closure:"));

    let in_value = called(&cg, "write", "Protected.swift", |d| d.contains("_ value:"), "write");
    assert!(in_value.contains("_ closure:"), "inside write(_ value:): {in_value}");
    let in_async = called(&cg, "clear", "Cache.swift", |d| d.trim_start().starts_with("func clear() async"), "clear");
    assert!(in_async.contains("handler:"), "inside clear() async: {in_async}");

    assert!(!use_("trailingWithLabels", "store").contains("options:"));
    assert!(use_("trailingWithAllLabels", "store").contains("options:"));
    assert!(!use_("condition", "has").contains("then done"));
    assert!(use_("autoclosure", "accept").contains("contentType:"));
    cg.close();
}
