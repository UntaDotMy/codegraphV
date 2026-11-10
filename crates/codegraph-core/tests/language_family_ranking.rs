//! Upstream 6d3804cf: a lone candidate is judged by language family, and C, C++ and
//! Objective-C rank as one language. TypeScript and TSX stay separate for ranking, so a
//! `.ts` call keeps the `.ts` method over a nearer `.tsx` namesake.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

fn index(files: &[(&str, &str)]) -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    for (rel, source) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

fn fixture() -> (tempfile::TempDir, CodeGraph) {
    index(&[
        ("web/ui/Row.tsx", "export function Row() { return <div>{formatDate(new Date())}</div>; }\n"),
        ("web/lib/fmt.ts", "function formatDate(d: Date) { return d.toISOString(); }\n"),
        (
            "web/components/App.tsx",
            "export class App {\n  store = new Store();\n  scheduleCapture() { return 1; }\n  render() { return <div />; }\n}\n",
        ),
        (
            "web/components/App.clipboard.ts",
            "export class AppClipboard {\n  constructor(private app: any) {}\n  paste() {\n    this.app.store.scheduleCapture();\n  }\n}\n",
        ),
        ("web/store/store.ts", "export class Store {\n  scheduleCapture() { return 2; }\n}\n"),
        ("native/app/init.c", "void inputError(int code) { (void)code; }\n"),
        ("native/app/posix_thread.c", "void setTls(int *slot) { (void)slot; }\n"),
        ("native/win/win32_thread.c", "void setTls(int *slot) { (void)slot; }\n"),
        (
            "native/app/window.m",
            "#import <Foundation/Foundation.h>\n\nvoid makeCurrent(int *slot) {\n  setTls(slot);\n  inputError(1);\n}\n",
        ),
        ("native/app/clock.c", "long getTime(void) { return 1; }\n"),
        ("native/win/clock_win.c", "long getTime(void) { return 2; }\n"),
        ("native/app/main.cpp", "long run() {\n  return getTime();\n}\n"),
    ])
}

/// `file:name@confidence` of each `calls` edge leaving `name` in `file`, sorted.
fn calls_from(cg: &CodeGraph, file: &str, name: &str) -> Vec<String> {
    let from: Vec<_> = cg
        .get_nodes_by_file(file)
        .unwrap()
        .into_iter()
        .filter(|n| n.name == name && matches!(n.kind, NodeKind::Function | NodeKind::Method))
        .collect();
    assert_eq!(from.len(), 1, "{name} in {file}");
    let mut calls: Vec<String> = cg
        .get_outgoing_edges(&from[0].id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Calls)
        .map(|e| {
            let target = cg.get_node(&e.target).unwrap().expect("call target");
            let confidence = e.metadata.as_ref().and_then(|m| m.get("confidence")).and_then(|c| c.as_f64());
            format!("{}:{}@{}", target.file_path, target.name, confidence.map(|c| c.to_string()).unwrap_or_else(|| "?".into()))
        })
        .collect();
    calls.sort();
    calls
}

#[test]
fn a_tsx_file_calling_the_one_typescript_function_is_a_same_family_match() {
    let (_dir, cg) = fixture();
    assert_eq!(calls_from(&cg, "web/ui/Row.tsx", "Row"), vec!["web/lib/fmt.ts:formatDate@0.9"]);
    cg.close();
}

#[test]
fn typescript_and_tsx_are_still_ranked_by_their_own_language() {
    let (_dir, cg) = fixture();
    let targets: Vec<String> = calls_from(&cg, "web/components/App.clipboard.ts", "paste")
        .into_iter()
        .map(|call| call.split('@').next().unwrap().to_string())
        .collect();
    assert_eq!(targets, vec!["web/store/store.ts:scheduleCapture"]);
    cg.close();
}

#[test]
fn objective_c_calls_into_c_the_one_function_and_the_nearer_of_two() {
    let (_dir, cg) = fixture();
    let calls = calls_from(&cg, "native/app/window.m", "makeCurrent");
    let targets: Vec<String> = calls.iter().map(|call| call.split('@').next().unwrap().to_string()).collect();
    assert_eq!(targets, vec!["native/app/init.c:inputError", "native/app/posix_thread.c:setTls"]);
    assert!(calls.iter().any(|call| call == "native/app/init.c:inputError@0.9"), "{calls:?}");
    cg.close();
}

#[test]
fn cpp_calls_into_c_the_nearer_of_two_definitions() {
    let (_dir, cg) = fixture();
    let targets: Vec<String> = calls_from(&cg, "native/app/main.cpp", "run")
        .into_iter()
        .map(|call| call.split('@').next().unwrap().to_string())
        .collect();
    assert_eq!(targets, vec!["native/app/clock.c:getTime"]);
    cg.close();
}
