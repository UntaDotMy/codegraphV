//! Upstream bea5e205: a Python class with PEP 695 type parameters reads its base list.
//! Ported from `__tests__/python-pep695-bases.test.ts`.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

const SOURCE: &str = r#"class Base[T]:
    def run(self):
        pass


class Other:
    def run(self):
        pass


class Mixin:
    pass


class Classic(Mixin, Base):
    pass


class Repo[T](Base[T]):
    pass


class Bounded[T: dict[str, int], *Ts, **P](Mixin, Base[T]):
    pass


class Constrained[T: (int, str)](Mixin, Base[T]):
    pass


class WrappedParams[
    K,
    V: (int, str),
](
    Mixin,
    Base[V],
):
    pass


class NoBases[T]:
    pass


current_a = Classic()
current_b = Repo()
current_c = Bounded()
current_d = Constrained()
current_e = WrappedParams()


def global_classic():
    current_a.run()


def global_repo():
    current_b.run()


def global_bounded():
    current_c.run()


def global_constrained():
    current_d.run()


def global_wrapped_params():
    current_e.run()


def class_classic(x):
    Classic.run(x)


def class_repo(x):
    Repo.run(x)


def class_bounded(x):
    Bounded.run(x)


def class_constrained(x):
    Constrained.run(x)


def class_wrapped_params(x):
    WrappedParams.run(x)


def class_no_bases(x):
    NoBases.run(x)
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
fn pep695_parameter_lists_do_not_hide_the_bases() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("app")).unwrap();
    fs::write(dir.path().join("app/__init__.py"), "").unwrap();
    fs::write(dir.path().join("app/m.py"), SOURCE).unwrap();
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    for function in [
        "global_classic",
        "global_repo",
        "global_bounded",
        "global_constrained",
        "global_wrapped_params",
        "class_classic",
        "class_repo",
        "class_bounded",
        "class_constrained",
        "class_wrapped_params",
    ] {
        assert_eq!(calls(&cg, function), ["Base::run"], "{function}");
    }
    assert!(!calls(&cg, "class_no_bases").contains(&"Base::run".to_string()));
    let base = cg.get_nodes_by_file("app/m.py").unwrap().into_iter().find(|n| n.name == "Base" && n.kind == NodeKind::Class).unwrap();
    for name in ["Repo", "Bounded", "Constrained", "WrappedParams"] {
        let cls = cg.get_nodes_by_file("app/m.py").unwrap().into_iter().find(|n| n.name == name && n.kind == NodeKind::Class).unwrap();
        assert!(
            cg.get_outgoing_edges(&cls.id)
                .unwrap()
                .into_iter()
                .any(|e| e.kind == EdgeKind::Extends && e.target == base.id),
            "{name} extends Base"
        );
    }
    cg.close();
}
