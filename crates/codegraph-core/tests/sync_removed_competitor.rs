//! Upstream 2d98c1b6: a call that only failed because a second candidate shared its name
//! resolves once a sync removes that candidate (delete or rename), and converges to a fresh
//! index. Ported from `__tests__/sync-removed-competitor.test.ts`.
use codegraph_core::{CodeGraph, IndexOptions};
use codegraph_types::EdgeKind;
use std::fs;

fn snapshot(cg: &CodeGraph) -> (Vec<String>, Vec<String>, Vec<String>) {
    let nodes = cg.store().get_all_nodes().unwrap();
    let mut ns: Vec<_> = nodes.iter().map(|n| {
        let mut value = serde_json::to_value(n).unwrap();
        value.as_object_mut().unwrap().remove("updatedAt");
        value.to_string()
    }).collect();
    let mut es: Vec<_> = nodes.iter().flat_map(|n| cg.store().get_outgoing_edges(&n.id, &[], None).unwrap())
        .map(|e| serde_json::to_string(&e).unwrap()).collect();
    let mut refs: Vec<_> = cg.store().get_unresolved_references().unwrap().iter().map(|r| {
        let mut value = serde_json::to_value(r).unwrap();
        value.as_object_mut().unwrap().remove("id");
        value.to_string()
    }).collect();
    ns.sort(); es.sort(); refs.sort();
    (ns, es, refs)
}

/// Callee names, qualified by file so the two languages read alike.
fn call_targets(cg: &CodeGraph, caller: &str) -> Vec<String> {
    let nodes = cg.store().get_all_nodes().unwrap();
    let node = nodes.iter().find(|n| n.name == caller && n.kind == codegraph_types::NodeKind::Function).unwrap();
    let mut targets: Vec<_> = cg.store().get_outgoing_edges(&node.id, &[EdgeKind::Calls], None).unwrap().into_iter()
        .filter_map(|e| nodes.iter().find(|n| n.id == e.target))
        .map(|n| format!("{}:{}", n.file_path, n.name))
        .collect();
    targets.sort();
    targets
}

fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, source) in files {
        let path = dir.path().join(path);
        if let Some(parent) = path.parent() { fs::create_dir_all(parent).unwrap(); }
        fs::write(path, source).unwrap();
    }
    dir
}

fn assert_converges(cg: &mut CodeGraph) {
    let incremental = snapshot(cg);
    cg.index_all(&IndexOptions::default(), None, None).unwrap();
    assert_eq!(incremental, snapshot(cg), "sync converges to a fresh index");
}

#[test]
fn python_delete_of_the_other_candidate_links_the_call() {
    let dir = project(&[
        ("a.py", "class A:\n    def go(self):\n        return 1\n"),
        ("b.py", "class B:\n    def go(self):\n        return 2\n"),
        ("t.py", "from a import A\n\ndef make():\n    return A()\n\ndef use():\n    x = make()\n    x.go()\n"),
    ]);
    let mut cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    assert_eq!(call_targets(&cg, "use"), ["t.py:make"], "two candidates stay unresolved");
    fs::remove_file(dir.path().join("b.py")).unwrap();
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(call_targets(&cg, "use"), ["a.py:go", "t.py:make"]);
    assert_converges(&mut cg);
}

#[test]
fn python_rename_of_the_other_candidate_links_the_call() {
    let dir = project(&[
        ("a.py", "class A:\n    def go(self):\n        return 1\n"),
        ("b.py", "class B:\n    def go(self):\n        return 2\n"),
        ("t.py", "from a import A\n\ndef make():\n    return A()\n\ndef use():\n    x = make()\n    x.go()\n"),
    ]);
    let mut cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    fs::write(dir.path().join("b.py"), "class B:\n    def stop(self):\n        return 2\n").unwrap();
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(call_targets(&cg, "use"), ["a.py:go", "t.py:make"]);
    assert_converges(&mut cg);
    // A third candidate makes it ambiguous again; sync must not keep a stale edge.
    fs::write(dir.path().join("c.py"), "class C:\n    def go(self):\n        return 3\n").unwrap();
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(call_targets(&cg, "use"), ["t.py:make"]);
    assert_converges(&mut cg);
}

#[test]
fn python_deletion_that_leaves_two_candidates_changes_nothing() {
    let dir = project(&[
        ("a.py", "class A:\n    def go(self):\n        return 1\n"),
        ("b.py", "class B:\n    def go(self):\n        return 2\n"),
        ("c.py", "class C:\n    def go(self):\n        return 3\n"),
        ("t.py", "from a import A\n\ndef make():\n    return A()\n\ndef use():\n    x = make()\n    x.go()\n"),
    ]);
    let mut cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    fs::remove_file(dir.path().join("b.py")).unwrap();
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(call_targets(&cg, "use"), ["t.py:make"]);
    assert_converges(&mut cg);
}

#[test]
fn typescript_delete_and_rename_link_the_surviving_method() {
    for rename in [false, true] {
        let dir = project(&[
            ("a.ts", "export class A {\n  go(): number { return 1; }\n}\n"),
            ("b.ts", "export class B {\n  go(): number { return 2; }\n}\n"),
            ("t.ts", "export function use(x: any): void {\n  x.go();\n}\n"),
        ]);
        let mut cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
        assert_eq!(call_targets(&cg, "use"), Vec::<String>::new(), "two candidates stay unresolved");
        if rename {
            fs::write(dir.path().join("b.ts"), "export class B {\n  stop(): number { return 2; }\n}\n").unwrap();
        } else {
            fs::remove_file(dir.path().join("b.ts")).unwrap();
        }
        cg.sync(&IndexOptions::default(), None).unwrap();
        assert_eq!(call_targets(&cg, "use"), ["a.ts:go"], "rename={rename}");
        assert_converges(&mut cg);
    }
}
