//! Upstream 27c904b0: a Python base list wrapped over several lines is read whole by the
//! resolver's own header reader. Ported from `__tests__/python-multiline-bases.test.ts`.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

const SOURCE: &str = r#"from typing import Generic, TypeVar

T = TypeVar("T")


class Base(Generic[T]):
    def run(self):
        pass


class Other:
    def run(self):
        pass


class Mixin:
    pass


class User:
    def save(self):
        pass


class OneLine(Mixin, Base):
    pass


class Wrapped(
    Mixin,
    Base,
):
    pass


class Commented(
    Mixin,  # no run here :)
    # the base that declares run:
    Base,
):
    pass


class WrappedGeneric(
    Mixin,
    Base[
        dict[str, int]
    ],
):
    pass

current_a = OneLine()
current_b = Wrapped()
current_c = Commented()
current_d = WrappedGeneric()


def use_one_line():
    current_a.run()


def use_wrapped():
    current_b.run()


def use_commented():
    current_c.run()


def use_wrapped_generic():
    current_d.run()


def keep(cls):
    return cls


@keep
class DecoratedOneLine(Mixin, Base):
    pass


@keep
class DecoratedWrapped(
    Mixin,
    Base,
):
    pass


def class_one_line(x):
    OneLine.run(x)


def class_wrapped(x):
    Wrapped.run(x)


def class_commented(x):
    Commented.run(x)


def class_decorated_one_line(x):
    DecoratedOneLine.run(x)


def class_decorated_wrapped(x):
    DecoratedWrapped.run(x)


class WrappedRepo(
    Mixin,
    Base[User],
):
    pass


def class_wrapped_base_method(x):
    WrappedRepo.run(x)


def class_wrapped_type_argument(x):
    WrappedRepo.save(x)
"#;

fn calls(cg: &CodeGraph, function: &str) -> Vec<String> {
    let node = cg
        .get_nodes_by_file("app/m.py")
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
fn wrapped_and_commented_base_lists_resolve_like_one_line_lists() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("app")).unwrap();
    fs::write(dir.path().join("app/__init__.py"), "").unwrap();
    fs::write(dir.path().join("app/m.py"), SOURCE).unwrap();
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    for function in [
        "use_one_line",
        "use_wrapped",
        "use_commented",
        "use_wrapped_generic",
        "class_one_line",
        "class_wrapped",
        "class_commented",
        "class_decorated_one_line",
        "class_decorated_wrapped",
        "class_wrapped_base_method",
    ] {
        assert_eq!(calls(&cg, function), ["Base::run"], "{function}");
    }
    assert_eq!(calls(&cg, "class_wrapped_type_argument"), Vec::<String>::new());
    cg.close();
}
