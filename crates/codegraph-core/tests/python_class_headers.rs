//! Upstream 27c904b0 / bea5e205: wrapped bases and PEP 695 parameters.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

fn check(header: &str, name: &str, expected: &[&str]) {
    let dir = tempfile::tempdir().unwrap();
    let source = format!(r#"class Base:
    def run(self): pass
class Other:
    def run(self): pass
class Mixin: pass
class User:
    def save(self): pass
def keep(cls): return cls
{header}
    pass
current = {name}()
def through_global():
    current.run()
def through_class(x):
    {name}.run(x)
def not_type_argument(x):
    {name}.save(x)
"#);
    fs::write(dir.path().join("main.py"), source).unwrap();
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let nodes = cg.get_nodes_by_file("main.py").unwrap();
    for function in ["through_global", "through_class", "not_type_argument"] {
        let node = nodes.iter().find(|n| n.name == function && n.kind == NodeKind::Function).unwrap();
        let calls: Vec<_> = cg.get_outgoing_edges(&node.id).unwrap().into_iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .filter_map(|e| cg.get_node(&e.target).unwrap())
            .filter(|n| n.kind == NodeKind::Method)
            .map(|n| n.qualified_name).collect();
        let want = if function == "not_type_argument" { &[][..] } else { expected };
        assert_eq!(calls, want, "{header}: {function}");
    }
    if !expected.is_empty() {
        let class = nodes.iter().find(|n| n.name == name && n.kind == NodeKind::Class).unwrap();
        let base = nodes.iter().find(|n| n.name == "Base" && n.kind == NodeKind::Class).unwrap();
        assert!(cg.get_outgoing_edges(&class.id).unwrap().iter()
            .any(|e| e.kind == EdgeKind::Extends && e.target == base.id), "{header}: extends Base");
    }
    cg.close();
}

#[test]
fn wrapped_and_decorated_base_lists() {
    for header in [
        "class Repo(Mixin, Base):",
        "class Repo(\n    Mixin,\n    Base,\n):",
        "class Repo(\n    Mixin, # misleading ):\n    # comment before base\n    Base,\n):",
        "class Repo(\n    Mixin,\n    Base[\n        dict[str, User]\n    ],\n):",
        "@keep\nclass Repo(Mixin, Base):",
        "@keep\nclass Repo(\n    Mixin,\n    Base[User],\n):",
    ] { check(header, "Repo", &["Base::run"]); }
}

#[test]
fn pep695_parameters_are_not_base_lists() {
    for header in [
        "class Repo[T](Base[T]):",
        "class Repo[T: dict[str, int], *Ts, **P](Mixin, Base[T]):",
        "class Repo[T: (int, str)](Mixin, Base[T]):",
        "class Repo[\n    K,\n    V: (int, str),\n](\n    Mixin,\n    Base[V],\n):",
    ] { check(header, "Repo", &["Base::run"]); }
    check("class NoBases[T]:", "NoBases", &[]);
}

#[test]
fn external_bases_do_not_link_project_namesakes() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("namesakes.py"), "class Enum:\n    def run(self): pass\nclass Sequence:\n    def run(self): pass\nclass Mapping:\n    def run(self): pass\n").unwrap();
    fs::write(dir.path().join("outside.py"), "from enum import Enum\nfrom typing import Sequence\nimport enum\nimport enum as e\nimport collections.abc\nclass Color(Enum): pass\nclass Items(Sequence[int]): pass\nclass Dotted(enum.Enum): pass\nclass Aliased(e.Enum): pass\nclass Deep(collections.abc.Mapping): pass\ndef use():\n    Color.run()\n    Items.run()\n    Dotted.run()\n    Aliased.run()\n    Deep.run()\n").unwrap();
    fs::write(dir.path().join("inside.py"), "from namesakes import Enum\nclass Local(Enum): pass\ndef use():\n    Local.run()\n").unwrap();
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    for node in cg.get_nodes_by_file("outside.py").unwrap() {
        let edges = cg.get_outgoing_edges(&node.id).unwrap();
        for edge in edges.iter().filter(|e| matches!(e.kind, EdgeKind::Extends | EdgeKind::Calls)) {
            let target = cg.get_node(&edge.target).unwrap().unwrap();
            assert_ne!(target.file_path, "namesakes.py", "{} -> {}", node.name, target.qualified_name);
        }
    }
    let nodes = cg.get_nodes_by_file("inside.py").unwrap();
    for (name, kind) in [("Local", EdgeKind::Extends), ("use", EdgeKind::Calls)] {
        let node = nodes.iter().find(|n| n.name == name).unwrap();
        assert!(cg.get_outgoing_edges(&node.id).unwrap().iter().any(|e| e.kind == kind));
    }
    cg.close();
}

#[test]
fn outside_base_gate_preserves_local_relative_and_custom_root_bindings() {
    let dir = tempfile::tempdir().unwrap();
    for (file, source) in [
        ("app/__init__.py", ""),
        ("app/schema.py", "class Enum: pass\nclass BaseModel: pass\nclass Thing: pass\n"),
        ("app/shadow.py", "from enum import Enum\nclass Enum(Enum): pass\nclass Own(Enum): pass\n"),
        ("app/fallback.py", "try:\n    from pydantic import BaseModel\nexcept ImportError:\n    from .schema import BaseModel\nclass Either(BaseModel): pass\n"),
        ("app/starred.py", "from .schema import *\nclass Starred(Enum): pass\n"),
        ("app/relative.py", "from .moved import Thing\nclass Keep(Thing): pass\n"),
        ("lib/support/helpers/base.py", "class Base: pass\n"),
        ("tests/test_base.py", "from helpers.base import Base\nclass TestBase(Base): pass\n"),
    ] {
        let path = dir.path().join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    for (file, name, target_file) in [
        ("app/shadow.py", "Own", "app/shadow.py"),
        ("app/fallback.py", "Either", "app/schema.py"),
        ("app/starred.py", "Starred", "app/schema.py"),
        ("app/relative.py", "Keep", "app/schema.py"),
        ("tests/test_base.py", "TestBase", "lib/support/helpers/base.py"),
    ] {
        let nodes = cg.get_nodes_by_file(file).unwrap();
        let node = nodes.iter().find(|n| n.name == name).unwrap();
        let targets: Vec<_> = cg.get_outgoing_edges(&node.id).unwrap().into_iter()
            .filter(|e| e.kind == EdgeKind::Extends)
            .map(|e| cg.get_node(&e.target).unwrap().unwrap().file_path).collect();
        assert_eq!(targets, [target_file], "{file}:{name}");
    }
    cg.close();
}

#[test]
fn header_edits_sync_to_the_complete_fresh_graph() {
    use codegraph_core::IndexOptions;
    fn snapshot(cg: &CodeGraph) -> (Vec<String>, Vec<String>, Vec<String>) {
        let nodes = cg.store().get_all_nodes().unwrap();
        let mut ns: Vec<_> = nodes.iter().map(|n| {
            let mut v = serde_json::to_value(n).unwrap();
            v.as_object_mut().unwrap().remove("updatedAt");
            v.to_string()
        }).collect();
        let mut es: Vec<_> = nodes.iter().flat_map(|n| cg.get_outgoing_edges(&n.id).unwrap())
            .map(|e| serde_json::to_string(&e).unwrap()).collect();
        let mut refs: Vec<_> = cg.store().get_unresolved_references().unwrap().iter().map(|r| {
            let mut v = serde_json::to_value(r).unwrap();
            v.as_object_mut().unwrap().remove("id");
            v.to_string()
        }).collect();
        ns.sort(); es.sort(); refs.sort();
        (ns, es, refs)
    }
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.py"), "class Repo: pass\n").unwrap();
    let mut cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    for header in ["class Repo(Base):", "class Repo[T](\n    Base[T],\n):", "class Repo[T]:"] {
        fs::write(dir.path().join("main.py"), format!("class Base:\n    def run(self): pass\n{header}\n    pass\ncurrent = Repo()\ndef use():\n    current.run()\n")).unwrap();
        cg.sync(&IndexOptions::default(), None).unwrap();
        let incremental = snapshot(&cg);
        cg.index_all(&IndexOptions::default(), None, None).unwrap();
        assert_eq!(incremental, snapshot(&cg), "{header}");
    }
    cg.close();
}
