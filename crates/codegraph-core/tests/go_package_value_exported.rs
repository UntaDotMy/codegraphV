//! Upstream 37052b18: a Go package-level variable or constant is exported by its own name's case.
//! Ported from `__tests__/go-package-value-exported.test.ts`.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

const PACKAGE_A: &str = "package a\n\nvar Hook = func() int { return 1 }\n\nvar (\n\tGroupedHook = func() int { return 2 }\n\tgrouped     = 0\n)\n\nvar hidden = func() int { return 3 }\n\nvar Typed, second int\n\nconst Limit = 3\n\nconst (\n\tModeA = iota\n\tmodeB\n)\n\ntype Client struct{}\n\nfunc (c *Client) Do() int { return 4 }\n\nfunc helper() int { return Hook() + hidden() + grouped }\n";
const PACKAGE_B: &str = "package b\n\nimport \"example.com/app/a\"\n\nfunc Use() int {\n\treturn a.Hook() + a.GroupedHook() + a.Limit + a.hidden()\n}\n";

fn calls_from(cg: &CodeGraph, file: &str, function: &str) -> Vec<String> {
    let nodes = cg.store().get_all_nodes().unwrap();
    let src = nodes.iter().find(|n| n.file_path == file && n.name == function).unwrap();
    let mut out: Vec<_> = cg.store().get_outgoing_edges(&src.id, &[EdgeKind::Calls], None).unwrap()
        .into_iter()
        .filter_map(|e| nodes.iter().find(|n| n.id == e.target))
        .map(|n| format!("{}:{}", n.file_path, n.name))
        .collect();
    out.sort();
    out
}

#[test]
fn go_package_values_are_exported_by_their_name() {
    let dir = tempfile::tempdir().unwrap();
    for (path, source) in [("go.mod", "module example.com/app\n\ngo 1.21\n"), ("a/a.go", PACKAGE_A), ("b/b.go", PACKAGE_B)] {
        let path = dir.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let nodes = cg.store().get_all_nodes().unwrap();
    let flag = |name: &str| nodes.iter().find(|n| n.file_path == "a/a.go" && n.name == name).and_then(|n| n.is_exported);
    for (name, expected) in [
        ("Hook", true), ("GroupedHook", true), ("grouped", false), ("hidden", false),
        ("Typed", true), ("Limit", true), ("ModeA", true), ("modeB", false),
    ] {
        assert_eq!(flag(name), Some(expected), "{name}");
    }
    assert_eq!(nodes.iter().find(|n| n.file_path == "a/a.go" && n.name == "Client").unwrap().is_exported, Some(true));
    assert_eq!(nodes.iter().find(|n| n.file_path == "a/a.go" && n.name == "Do").unwrap().is_exported, Some(true));
    assert_eq!(nodes.iter().find(|n| n.file_path == "a/a.go" && n.name == "helper").unwrap().is_exported, Some(false));

    assert_eq!(calls_from(&cg, "b/b.go", "Use"), ["a/a.go:GroupedHook", "a/a.go:Hook"]);
    let hidden_unresolved: Vec<_> = cg.store().get_unresolved_references().unwrap().iter()
        .filter(|r| r.file_path.as_deref() == Some("b/b.go"))
        .map(|r| r.reference_name.clone()).collect();
    assert_eq!(hidden_unresolved, ["example.com/app/a", "a.hidden"]);
    assert_eq!(calls_from(&cg, "a/a.go", "helper"), ["a/a.go:Hook", "a/a.go:hidden"]);
    // No constant is callable; the point above is the two function-valued variables.
    assert!(nodes.iter().any(|n| n.kind == NodeKind::Constant && n.name == "Limit"));
    cg.close();
}
