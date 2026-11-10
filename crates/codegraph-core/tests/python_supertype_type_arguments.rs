//! Upstream cd04b21a: a Python base's type argument is no supertype, so `Repo.save()` is not
//! `User.save`. Ported from `__tests__/python-supertype-type-arguments.test.ts`.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

const SOURCE: &str = r#"from typing import Generic, Optional, TypeVar

T = TypeVar("T")
U = TypeVar("U")


class Base(Generic[T]):
    def load(self):
        pass


class Pair(Generic[T, U]):
    def load(self):
        pass


class User:
    def save(self):
        pass


class Other:
    def save(self):
        pass


class Repo(Base[User]):
    pass


class Store(Pair[str, dict[str, User]]):
    pass


class Keyed(Base[User], metaclass=type):
    pass


class Forward(Base["Pair[str, User]"]):
    pass


class Mixin:
    def save(self):
        pass


class Mixed(Base[Optional[User]], Mixin):
    pass


def save_by_class():
    Repo.save()


def save_by_receiver(repo):
    repo.save()


def save_nested():
    Store.save()


def save_keyed():
    Keyed.save()


def save_forward():
    Forward.save()


def save_mixed():
    Mixed.save()


def load_by_class():
    Repo.load()


def load_nested():
    Store.load()
"#;

fn calls(cg: &CodeGraph, function: &str) -> Vec<String> {
    let node = cg
        .get_nodes_by_file("app/m.py")
        .unwrap()
        .into_iter()
        .find(|n| n.name == function && n.kind == NodeKind::Function)
        .unwrap_or_else(|| panic!("{function}"));
    cg.get_outgoing_edges(&node.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Calls)
        .filter_map(|e| cg.get_node(&e.target).unwrap())
        .filter(|n| n.kind == NodeKind::Method)
        .map(|n| n.qualified_name)
        .collect()
}

#[test]
fn type_arguments_are_not_supertypes_but_real_bases_are() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("app")).unwrap();
    fs::write(dir.path().join("app/__init__.py"), "").unwrap();
    fs::write(dir.path().join("app/m.py"), SOURCE).unwrap();
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    for function in ["save_by_class", "save_by_receiver", "save_nested", "save_keyed", "save_forward"] {
        let got = calls(&cg, function);
        assert!(!got.contains(&"User::save".to_string()), "{function}: {got:?}");
    }
    assert_eq!(calls(&cg, "load_by_class"), ["Base::load"]);
    assert_eq!(calls(&cg, "load_nested"), ["Pair::load"]);
    assert_eq!(calls(&cg, "save_mixed"), ["Mixin::save"]);

    // A parameterized base still links its class (upstream 3fca009e).
    let repo = cg.get_nodes_by_name("Repo").unwrap().into_iter().find(|n| n.kind == NodeKind::Class).unwrap();
    let bases: Vec<String> = cg
        .get_outgoing_edges(&repo.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Extends)
        .filter_map(|e| cg.get_node(&e.target).unwrap())
        .map(|n| n.qualified_name)
        .collect();
    assert_eq!(bases, ["Base"]);
    cg.close();
}
