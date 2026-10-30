//! A standard-library method called on a receiver of unknown type (Python `options.setdefault(...)`,
//! Scala `m.getOrElse(...)`, Java `m.put(...)` on a `var`) does not bind by name to the one project
//! method that shares the name: the receiver's own words must name the method's type (upstream
//! 7cceba52).
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

const FILES: [(&str, &str); 6] = [
    (
        "py/globals.py",
        "class Globals:\n    def setdefault(self, key, value):\n        return value\n    def reset(self):\n        return self.setdefault(\"a\", 1)\n\ndef build(options):\n    options.setdefault(\"x\", 1)\n    return options.copy().setdefault(\"y\", 2)\n",
    ),
    (
        "java/com/app/Registry.java",
        "package com.app;\npublic class Registry {\n    public static final String KEY = \"k\";\n    public Object get(String k) { return null; }\n    public int size() { return 0; }\n    public int hash() { return 0; }\n    public boolean equals(Object o) { return false; }\n    public String toString() { return \"r\"; }\n    int count() { return size(); }\n    Object own(String k) { return this.get(k); }\n}\n",
    ),
    ("sc/Registry.scala", "package app\nclass Registry {\n  def getOrElse(d: Int): Int = d\n}\n"),
    (
        "sc/Use.scala",
        "package app\nobject Use {\n  def go(): Int = {\n    val m = compute()\n    m.getOrElse(0)\n  }\n  def compute() = Option(1)\n  def viaName(): Int = { val registry = make(); registry.getOrElse(0) }\n  def make() = new Registry\n}\n",
    ),
    (
        "java/com/app/Use.java",
        "package com.app;\nimport java.util.*;\npublic class Use {\n    private Registry registry;\n    Object untyped(String a) {\n        var data = load();\n        data.put(a, 1);\n        return data.get(a);\n    }\n    Map<String, Object> load() { return null; }\n    Object typed(String k) { return registry.get(k); }\n    int named() { var userRegistry = make(); return userRegistry.size(); }\n    Registry make() { return null; }\n}\n",
    ),
    (
        "cs/App/Money.cs",
        "namespace App {\n  public class Money {\n    public override string ToString() { return \"m\"; }\n  }\n  public class Use {\n    object Make() { return null; }\n    public string Look() { var cls = Make(); return cls.ToString(); }\n  }\n}\n",
    ),
];

fn index() -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    for (relative, content) in FILES {
        let path = dir.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

fn callees_of(cg: &CodeGraph, name: &str, file: &str) -> Vec<String> {
    let node = cg
        .get_nodes_by_name(name)
        .unwrap()
        .into_iter()
        .find(|n| n.file_path.ends_with(file) && matches!(n.kind, NodeKind::Function | NodeKind::Method))
        .unwrap_or_else(|| panic!("{name} in {file}"));
    let mut callees: Vec<String> = cg
        .get_callees(&node.id, 1)
        .unwrap()
        .into_iter()
        .filter(|c| c.edge.kind == EdgeKind::Calls)
        .map(|c| {
            let segments: Vec<&str> = c.node.qualified_name.split("::").collect();
            segments[segments.len().saturating_sub(2)..].join("::")
        })
        .collect();
    callees.sort();
    callees
}

#[test]
fn python_setdefault_on_an_untyped_receiver_declines_but_self_keeps_its_method() {
    let (_dir, cg) = index();
    assert_eq!(callees_of(&cg, "build", "globals.py"), Vec::<String>::new());
    assert_eq!(callees_of(&cg, "reset", "globals.py"), ["Globals::setdefault"]);
}

#[test]
fn scala_get_or_else_on_an_untyped_value_declines_but_a_receiver_named_after_the_type_keeps_its_method() {
    let (_dir, cg) = index();
    assert_eq!(callees_of(&cg, "go", "Use.scala"), ["Use::compute"]);
    assert_eq!(callees_of(&cg, "viaName", "Use.scala"), ["Registry::getOrElse", "Use::make"]);
}

#[test]
fn java_put_and_get_on_an_untyped_var_do_not_bind_to_a_lone_project_method() {
    let (_dir, cg) = index();
    assert_eq!(callees_of(&cg, "untyped", "Use.java"), ["Use::load"]);
}

#[test]
fn java_a_typed_field_an_own_class_call_and_a_receiver_named_after_the_type_keep_their_method() {
    let (_dir, cg) = index();
    assert_eq!(callees_of(&cg, "typed", "Use.java"), ["Registry::get"]);
    assert_eq!(callees_of(&cg, "count", "Registry.java"), ["Registry::size"]);
    assert_eq!(callees_of(&cg, "own", "Registry.java"), ["Registry::get"]);
    assert_eq!(callees_of(&cg, "named", "Use.java"), ["Registry::size", "Use::make"]);
}

#[test]
fn csharp_a_receiver_named_cls_is_a_value_like_any_other() {
    let (_dir, cg) = index();
    assert_eq!(callees_of(&cg, "Look", "Money.cs"), ["Use::Make"]);
}
