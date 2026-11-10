//! Upstream 66449255: compare complete native nodes/edges after sync with a fresh index.
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

#[test]
fn retargeted_barrel_sync_matches_full_index_with_negative_control() {
    for scoped in [false, true] {
        for clause in ["export { foo }", "export *"] {
            let dir = tempfile::tempdir().unwrap();
            for (file, source) in [
                ("a.ts", "export function foo() { return 1; }\n"),
                ("b.ts", "export function foo() { return 2; }\n"),
                ("client.ts", "import { foo } from './index'; export function caller() { return foo(); }\n"),
                ("control.ts", "import { foo } from './a'; export function control() { return foo(); }\n"),
            ] { fs::write(dir.path().join(file), source).unwrap(); }
            fs::write(dir.path().join("index.ts"), format!("{clause} from './a';\n")).unwrap();
            let mut cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
            // Different size defeats coarse filesystem timestamp equality.
            fs::write(dir.path().join("index.ts"), format!("{clause} from './b';\n// moved\n")).unwrap();
            cg.sync(&IndexOptions { paths: scoped.then(|| vec!["index.ts".into()]), ..Default::default() }, None).unwrap();
            let nodes = cg.store().get_all_nodes().unwrap();
            for (name, target_file) in [("caller", "b.ts"), ("control", "a.ts")] {
                let caller = nodes.iter().find(|n| n.name == name).unwrap();
                let calls = cg.store().get_outgoing_edges(&caller.id, &[EdgeKind::Calls], None).unwrap();
                let targets: Vec<_> = calls.iter().filter_map(|e| nodes.iter().find(|n| n.id == e.target)).map(|n| n.file_path.as_str()).collect();
                assert_eq!(targets, [target_file], "{clause}, scoped={scoped}, {name}");
            }
            let incremental = snapshot(&cg);
            cg.index_all(&IndexOptions::default(), None, None).unwrap();
            assert_eq!(incremental, snapshot(&cg), "{clause}, scoped={scoped}");
        }
    }
}

#[test]
fn config_only_alias_sync_matches_full_index() {
    for scoped in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        for (file, source) in [
            ("a.ts", "export function foo() { return 1; }\n"),
            ("b.ts", "export function foo() { return 2; }\n"),
            ("client.ts", "import { foo } from '@impl'; export function caller() { return foo(); }\n"),
        ] { fs::write(dir.path().join(file), source).unwrap(); }
        let config = |target: &str, strict: bool| format!(r#"{{"compilerOptions":{{"baseUrl":".","paths":{{"@impl":["./{target}"]}},"strict":{strict}}}}}"#);
        fs::write(dir.path().join("tsconfig.json"), config("a", false)).unwrap();
        let mut cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
        let options = IndexOptions { paths: scoped.then(|| vec!["tsconfig.json".into()]), ..Default::default() };
        let unchanged = snapshot(&cg);
        fs::write(dir.path().join("tsconfig.json"), config("a", true)).unwrap();
        cg.sync(&options, None).unwrap();
        assert_eq!(unchanged, snapshot(&cg), "non-resolution config edit");
        fs::write(dir.path().join("tsconfig.json"), config("b", true)).unwrap();
        cg.sync(&options, None).unwrap();
        let nodes = cg.store().get_all_nodes().unwrap();
        let caller = nodes.iter().find(|n| n.name == "caller").unwrap();
        let calls = cg.store().get_outgoing_edges(&caller.id, &[EdgeKind::Calls], None).unwrap();
        let targets: Vec<_> = calls.iter().filter_map(|e| nodes.iter().find(|n| n.id == e.target)).map(|n| n.file_path.as_str()).collect();
        assert_eq!(targets, ["b.ts"], "scoped={scoped}");
        let incremental = snapshot(&cg);
        cg.index_all(&IndexOptions::default(), None, None).unwrap();
        assert_eq!(incremental, snapshot(&cg));
    }
}

#[test]
fn inherited_alias_edits_survive_reopening_and_go_module_edits_rebind() {
    let dir = tempfile::tempdir().unwrap();
    for (file, source) in [
        ("a.ts", "export function foo() { return 1; }\n"),
        ("b.ts", "export function foo() { return 2; }\n"),
        ("client.ts", "import { foo } from '@impl'; export function caller() { return foo(); }\n"),
        ("tsconfig.json", r#"{"extends":"./base.json"}"#),
        ("base.json", r#"{"compilerOptions":{"baseUrl":".","paths":{"@impl":["./a"]}}}"#),
    ] { fs::write(dir.path().join(file), source).unwrap(); }
    let mut cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    cg.close();
    fs::write(dir.path().join("base.json"), r#"{"compilerOptions":{"baseUrl":".","paths":{"@impl":["./b"]}}}"#).unwrap();
    let mut cg = CodeGraph::open(dir.path().to_str().unwrap(), Default::default()).unwrap();
    cg.sync(&IndexOptions::default(), None).unwrap();
    let incremental = snapshot(&cg);
    cg.index_all(&IndexOptions::default(), None, None).unwrap();
    assert_eq!(incremental, snapshot(&cg), "inherited aliases after reopening");

    let go = tempfile::tempdir().unwrap();
    fs::create_dir_all(go.path().join("lib")).unwrap();
    fs::write(go.path().join("go.mod"), "module example.test/old\n\ngo 1.22\n").unwrap();
    fs::write(go.path().join("lib/lib.go"), "package lib\nfunc Work() int { return 1 }\n").unwrap();
    fs::write(go.path().join("main.go"), "package main\nimport \"example.test/old/lib\"\nfunc Main() int { return lib.Work() }\n").unwrap();
    let mut cg = CodeGraph::init_and_index(go.path().to_str().unwrap(), None).unwrap();
    let before = snapshot(&cg);
    fs::write(go.path().join("go.mod"), "module example.test/old\n\ngo 1.23\n").unwrap();
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(before, snapshot(&cg), "go version is not a module mapping");
    fs::write(go.path().join("go.mod"), "module example.test/new\n\ngo 1.23\n").unwrap();
    cg.sync(&IndexOptions::default(), None).unwrap();
    let incremental = snapshot(&cg);
    cg.index_all(&IndexOptions::default(), None, None).unwrap();
    assert_eq!(incremental, snapshot(&cg), "Go module rename");
}
