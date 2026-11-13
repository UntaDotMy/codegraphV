//! A receiver typed by a type alias of a class has that class's methods
//! (upstream 2393ea2f, #3164).
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;
use std::path::Path;

const BRIDGE: &str = r#"export class Bridge<A, B> {
  send(event: string) { return event; }
}
class Base { shutdown() {} }
export class Derived extends Base {}
export type BackendBridge = Bridge<string, number>;
export type Maybe = Bridge<string, number> | null;
export type Chained = BackendBridge;
export type WithDefault<T = string> = Bridge<T, number>;
export type Inherited = Derived;
"#;

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

fn callees(cg: &CodeGraph, qualified: &str) -> Vec<String> {
    let nodes = cg.store().get_all_nodes().unwrap();
    let node = nodes.iter().find(|n| n.qualified_name == qualified).unwrap_or_else(|| panic!("no {qualified}"));
    let mut out = Vec::new();
    for edge in cg.get_outgoing_edges(&node.id).unwrap() {
        if edge.kind != EdgeKind::Calls {
            continue;
        }
        let Some(target) = cg.get_node(&edge.target).unwrap() else { continue };
        out.push(target.qualified_name.clone());
    }
    out.sort();
    out
}

fn call_file(cg: &CodeGraph, name: &str) -> String {
    let nodes = cg.store().get_all_nodes().unwrap();
    let node = nodes.iter().find(|n| n.name == name && n.kind == NodeKind::Function).unwrap_or_else(|| panic!("no {name}"));
    let edge = cg
        .get_outgoing_edges(&node.id)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == EdgeKind::Calls)
        .unwrap_or_else(|| panic!("{name} calls nothing"));
    let target = cg.get_node(&edge.target).unwrap().unwrap();
    slash(&target.file_path)
}

#[test]
fn an_alias_calls_the_class_it_names() {
    let shapes = [
        ("BackendBridge", "Bridge::send"),
        ("Maybe", "Bridge::send"),
        ("Chained", "Bridge::send"),
        ("WithDefault<number>", "Bridge::send"),
        ("Inherited", "Base::shutdown"),
    ];
    for (ty, expect) in shapes {
        let imported = ty.split('<').next().unwrap();
        let body = if expect == "Base::shutdown" {
            format!("import type {{ {imported} }} from './bridge';\nexport function run(link: {ty}) {{ link.shutdown(); }}\n")
        } else {
            format!("import type {{ {imported} }} from './bridge';\nexport function run(link: {ty}) {{ link.send('a'); }}\n")
        };
        let (_root, cg) = index(&[("bridge.ts", BRIDGE), ("use.ts", &body)]);
        assert_eq!(callees(&cg, "run"), vec![expect.to_string()], "{ty}");
        cg.close();
    }
}

#[test]
fn a_parameter_a_local_and_a_field_use_the_alias() {
    let use_ts = r#"import type { BackendBridge } from './bridge';
declare function make(): BackendBridge;
export function outer(bridge: BackendBridge) {
  function nested() { bridge.send('a'); }
  return nested;
}
export function local() {
  const link: BackendBridge = make();
  link.send('b');
}
export class Holder {
  private readonly port: BackendBridge;
  constructor(port: BackendBridge) { this.port = port; }
  go() { this.port.send('c'); }
}
"#;
    let (_root, cg) = index(&[("bridge.ts", BRIDGE), ("use.ts", use_ts)]);
    assert_eq!(callees(&cg, "outer::nested"), vec!["Bridge::send".to_string()]);
    assert!(callees(&cg, "local").contains(&"Bridge::send".to_string()));
    assert_eq!(callees(&cg, "Holder::go"), vec!["Bridge::send".to_string()]);
    cg.close();
}

#[test]
fn the_alias_file_picks_the_class() {
    let (_root, cg) = index(&[
        ("one/bridge.ts", "export class Bridge { send() {} }\n"),
        ("two/bridge.ts", "export class Bridge { send() {} }\n"),
        ("two/types.ts", "import { Bridge } from './bridge';\nexport type Link = Bridge;\n"),
        ("use.ts", "import type { Link } from './two/types';\nexport function run(link: Link) { link.send(); }\n"),
    ]);
    assert_eq!(call_file(&cg, "run"), "two/bridge.ts");
    cg.close();
}

#[test]
fn a_union_a_function_and_an_array_name_nothing() {
    let types = r#"export class One { run() {} }
export class Two { run() {} }
export type Either = One | Two;
export type Fn = (a: One) => One;
export type Many = One[];
"#;
    let use_ts = r#"import type { Either, Fn, Many } from './types';
export function a(x: Either) { x.run(); }
export function b(x: Fn) { x.run(); }
export function c(x: Many) { x.run(); }
"#;
    let (_root, cg) = index(&[("types.ts", types), ("use.ts", use_ts)]);
    for name in ["a", "b", "c"] {
        assert_eq!(callees(&cg, name), Vec::<String>::new(), "{name}");
    }
    cg.close();
}
