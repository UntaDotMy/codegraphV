//! Upstream 274b43c4: a Python base imported from outside the project is that package's class,
//! not a project class of the same name. Ported from `__tests__/python-outside-bases.test.ts`.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

const FILES: &[(&str, &str)] = &[
    ("app/__init__.py", ""),
    (
        "app/schema.py",
        "class Enum:\n    def name_of(self):\n        pass\n\n\nclass Sequence:\n    pass\n\n\nclass Generic:\n    pass\n\n\nclass Protocol:\n    pass\n\n\nclass BaseModel:\n    pass\n\n\nclass Thing:\n    pass\n\n\nclass Mapping:\n    def name_of(self):\n        pass\n\n\nclass Other:\n    def name_of(self):\n        pass\n",
    ),
    (
        "app/outside.py",
        "from enum import Enum\nfrom typing import Generic, Protocol, Sequence, TypeVar\n\nfrom pydantic import BaseModel\n\nT = TypeVar(\"T\")\n\n\nclass Color(Enum):\n    pass\n\n\nclass Plain(Sequence):\n    pass\n\n\nclass Items(Sequence[int]):\n    pass\n\n\nclass Box(Generic[T]):\n    pass\n\n\nclass Greeter(Protocol[T]):\n    pass\n\n\nclass Item(BaseModel):\n    pass\n\n\nclass Row(BaseModel, Enum):\n    pass\n",
    ),
    (
        "app/inside.py",
        "from app.schema import Sequence\n\nfrom .schema import Enum\n\n\nclass Mine(Enum):\n    pass\n\n\nclass Seq(Sequence[int]):\n    pass\n",
    ),
    ("app/shadow.py", "from enum import Enum\n\n\nclass Enum(Enum):\n    pass\n\n\nclass Own(Enum):\n    pass\n"),
    (
        "app/fallback.py",
        "try:\n    from pydantic import BaseModel\nexcept ImportError:\n    from .schema import BaseModel\n\n\nclass Either(BaseModel):\n    pass\n",
    ),
    ("app/starred.py", "from .schema import *\n\n\nclass Starred(Enum):\n    pass\n"),
    ("app/relative.py", "from .moved import Thing\n\n\nclass Keep(Thing):\n    pass\n"),
    (
        "lib/support/helpers/base.py",
        "class Base:\n    def helper(self):\n        pass\n\n\nclass Unrelated:\n    def helper(self):\n        pass\n",
    ),
    (
        "tests/test_base.py",
        "from helpers.base import Base\n\n\nclass TestBase(Base):\n    pass\n\n\ndef call_helper():\n    TestBase.helper()\n",
    ),
    (
        "app/calls.py",
        "import collections.abc\nimport enum\nimport enum as e\nfrom enum import Enum\n\nfrom .inside import Mine\n\n\nclass Color(Enum):\n    pass\n\n\nclass Dotted(enum.Enum):\n    pass\n\n\nclass Aliased(e.Enum):\n    pass\n\n\nclass Deep(collections.abc.Mapping):\n    pass\n\n\ndef named_import():\n    Color.name_of()\n\n\ndef on_a_receiver(color):\n    color.name_of()\n\n\ndef dotted():\n    Dotted.name_of()\n\n\ndef aliased():\n    Aliased.name_of()\n\n\ndef deep():\n    Deep.name_of()\n\n\ndef from_the_project():\n    Mine.name_of()\n",
    ),
];

fn index() -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    for (rel, content) in FILES {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

fn bases(cg: &CodeGraph, file: &str, name: &str) -> Vec<String> {
    let node = cg.get_nodes_by_file(file).unwrap().into_iter().find(|n| n.name == name && n.kind == NodeKind::Class).unwrap();
    let mut out: Vec<String> = cg
        .get_outgoing_edges(&node.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Extends)
        .filter_map(|e| cg.get_node(&e.target).unwrap())
        .map(|n| format!("{}:{}", n.file_path, n.name))
        .collect();
    out.sort();
    out
}

fn calls(cg: &CodeGraph, file: &str, function: &str) -> Vec<String> {
    let node = cg
        .get_nodes_by_file(file)
        .unwrap()
        .into_iter()
        .find(|n| n.name == function && n.kind == NodeKind::Function)
        .unwrap();
    let mut out: Vec<String> = cg
        .get_outgoing_edges(&node.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Calls)
        .filter_map(|e| cg.get_node(&e.target).unwrap())
        .filter(|n| n.kind == NodeKind::Method)
        .map(|n| n.qualified_name)
        .collect();
    out.sort();
    out
}

#[test]
fn outside_bases_do_not_link_project_namesakes() {
    let (_dir, cg) = index();
    for class in ["Color", "Plain", "Items", "Box", "Greeter", "Item", "Row"] {
        assert_eq!(bases(&cg, "app/outside.py", class), Vec::<String>::new(), "{class}");
    }
    assert_eq!(bases(&cg, "app/inside.py", "Mine"), ["app/schema.py:Enum"]);
    assert_eq!(bases(&cg, "app/inside.py", "Seq"), ["app/schema.py:Sequence"]);
    assert_eq!(bases(&cg, "app/shadow.py", "Own"), ["app/shadow.py:Enum"]);
    assert_eq!(bases(&cg, "app/fallback.py", "Either"), ["app/schema.py:BaseModel"]);
    assert_eq!(bases(&cg, "app/starred.py", "Starred"), ["app/schema.py:Enum"]);
    assert_eq!(bases(&cg, "app/relative.py", "Keep"), ["app/schema.py:Thing"]);
    assert_eq!(bases(&cg, "tests/test_base.py", "TestBase"), ["lib/support/helpers/base.py:Base"]);
    cg.close();
}

#[test]
fn calls_through_outside_bases_reach_nothing() {
    let (_dir, cg) = index();
    for function in ["named_import", "on_a_receiver", "dotted", "aliased", "deep"] {
        assert_eq!(calls(&cg, "app/calls.py", function), Vec::<String>::new(), "{function}");
    }
    assert_eq!(calls(&cg, "app/calls.py", "from_the_project"), ["Enum::name_of"]);
    assert_eq!(calls(&cg, "tests/test_base.py", "call_helper"), ["Base::helper"]);
    cg.close();
}
