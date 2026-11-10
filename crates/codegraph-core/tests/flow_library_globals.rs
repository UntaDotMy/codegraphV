//! A Flow library's declarations are globals of the files Flow reads
//! (upstream 6473f609, #3165).
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;
use std::path::Path;

fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (relative, content) in files {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

fn index(files: &[(&str, &str)]) -> (tempfile::TempDir, CodeGraph) {
    let root = tempfile::tempdir().unwrap();
    write_files(root.path(), files);
    let cg = CodeGraph::init_and_index(root.path().to_str().unwrap(), None).unwrap();
    (root, cg)
}

fn slash(path: &str) -> String {
    path.replace('\\', "/")
}

fn files() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "flow-typed/environments/streams.js",
            "declare class ReadableStream {\n  locked: boolean;\n}\ndeclare module 'fsx' {\n  declare module.exports: { read(): void };\n}\n",
        ),
        (
            "scripts/flow/environment.js",
            "/* @flow */\ndeclare interface ConsoleTask {\n  run<T>(f: () => T): T;\n}\ndeclare class Holder {\n  stream: ReadableStream;\n}\n",
        ),
        ("src/other.js", "export function other(a: any) { return a; }\n"),
        (
            "src/client.js",
            "// @flow\nimport {other} from './other';\nexport function start(stream: ReadableStream, task: ConsoleTask): void {\n  other(stream);\n  other(task);\n}\n",
        ),
        (
            "ts/client.ts",
            "import { other } from '../src/other';\nexport function typed(stream: ReadableStream): void {\n  other(stream);\n}\n",
        ),
        ("src/sample.js", "class Sample {}\n"),
        (
            "src/uses-sample.js",
            "import {other} from './other';\nexport function sampled(x: Sample) { return other(x); }\n",
        ),
        ("src/not-a-lib.js", "declare(function side() {});\nclass Marker {}\n"),
        (
            "src/uses-marker.js",
            "// @flow\nimport {other} from './other';\nexport function marked(x: Marker) { return other(x); }\n",
        ),
        ("src/decoy.js", "export class ReadableStream { leak() { return 1; } }\n"),
    ]
}

fn type_refs(cg: &CodeGraph, name: &str) -> Vec<String> {
    let nodes = cg.store().get_all_nodes().unwrap();
    let node = nodes
        .iter()
        .find(|n| n.name == name && matches!(n.kind, NodeKind::Function | NodeKind::Method))
        .unwrap_or_else(|| panic!("no function {name}"));
    let mut out = Vec::new();
    for edge in cg.get_outgoing_edges(&node.id).unwrap() {
        if edge.kind != EdgeKind::References {
            continue;
        }
        let Some(target) = cg.get_node(&edge.target).unwrap() else { continue };
        out.push(format!("{}:{}", slash(&target.file_path), target.name));
    }
    out.sort();
    out
}

fn field_refs(cg: &CodeGraph, file_suffix: &str, name: &str) -> Vec<String> {
    let nodes = cg.store().get_all_nodes().unwrap();
    let node = nodes
        .iter()
        .find(|n| n.name == name && slash(&n.file_path).ends_with(file_suffix) && matches!(n.kind, NodeKind::Field | NodeKind::Property))
        .unwrap_or_else(|| panic!("no field {name} in {file_suffix}"));
    let mut out = Vec::new();
    for edge in cg.get_outgoing_edges(&node.id).unwrap() {
        if edge.kind != EdgeKind::References {
            continue;
        }
        let Some(target) = cg.get_node(&edge.target).unwrap() else { continue };
        out.push(format!("{}:{}", slash(&target.file_path), target.name));
    }
    out.sort();
    out
}

#[test]
fn a_flow_file_names_the_libraries_it_does_not_import() {
    let (_root, cg) = index(&files());
    assert_eq!(
        type_refs(&cg, "start"),
        vec![
            "flow-typed/environments/streams.js:ReadableStream".to_string(),
            "scripts/flow/environment.js:ConsoleTask".to_string(),
        ]
    );
    cg.close();
}

#[test]
fn a_typescript_file_does_not_take_the_library() {
    let (_root, cg) = index(&files());
    assert_eq!(type_refs(&cg, "typed"), Vec::<String>::new());
    cg.close();
}

#[test]
fn a_plain_script_and_a_declare_call_are_not_libraries() {
    let (_root, cg) = index(&files());
    assert_eq!(type_refs(&cg, "sampled"), Vec::<String>::new());
    assert_eq!(type_refs(&cg, "marked"), Vec::<String>::new());
    cg.close();
}

#[test]
fn the_library_keeps_its_own_name() {
    let (_root, cg) = index(&files());
    assert_eq!(
        type_refs(&cg, "start"),
        vec![
            "flow-typed/environments/streams.js:ReadableStream".to_string(),
            "scripts/flow/environment.js:ConsoleTask".to_string(),
        ]
    );
    assert_eq!(
        field_refs(&cg, "scripts/flow/environment.js", "stream"),
        vec!["flow-typed/environments/streams.js:ReadableStream".to_string()]
    );
    cg.close();
}
