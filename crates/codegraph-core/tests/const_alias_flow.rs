//! A flow carries on through a top-level JS/TS value that only hands a function on
//! (upstream 9d3abb84, #3142). An import of the value still calls the value.
//! A value that is not a function links nothing. Sync links and unlinks the same
//! way a fresh index does.
use codegraph_core::{CodeGraph, IndexOptions};
use codegraph_graph::named_symbol_flow::{resolve_named_symbol_flow, NamedSymbolFlowOptions};
use codegraph_graph::synthesized_hop::describe_synthesized_hop;
use codegraph_types::{EdgeKind, Node, NodeKind, Provenance};
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

fn display(path: &str) -> String {
    path.replace('\\', "/")
}

fn nodes_in(cg: &CodeGraph, file: &str) -> Vec<Node> {
    let want = file.replace('\\', "/");
    cg.store()
        .get_all_nodes()
        .unwrap()
        .into_iter()
        .filter(|n| {
            let path = display(&n.file_path);
            path == want || path.ends_with(&format!("/{want}"))
        })
        .collect()
}

fn alias_targets(cg: &CodeGraph, file: &str, name: &str) -> Vec<String> {
    let nodes = nodes_in(cg, file);
    let alias = nodes.iter().find(|n| n.name == name && matches!(n.kind, NodeKind::Constant | NodeKind::Variable)).unwrap_or_else(|| {
        panic!(
            "{file}: {name} missing among {}",
            nodes.iter().map(|n| format!("{} {}", n.kind, n.qualified_name)).collect::<Vec<_>>().join(", ")
        )
    });
    let mut out = Vec::new();
    for edge in cg.get_outgoing_edges(&alias.id).unwrap() {
        if edge.metadata.as_ref().and_then(|m| m.get("synthesizedBy")).and_then(|v| v.as_str()) != Some("const-alias") {
            continue;
        }
        let Some(target) = cg.get_node(&edge.target).unwrap() else { continue };
        out.push(format!("{}:{}", display(&target.file_path).rsplit('/').next().unwrap_or(""), target.qualified_name));
    }
    out.sort();
    out
}

fn flow_names(cg: &CodeGraph, query: &str) -> Vec<String> {
    let store = cg.store().as_ref();
    let flow = resolve_named_symbol_flow(store, store, query, &NamedSymbolFlowOptions::default(), None).unwrap();
    flow.chains.first().map(|chain| chain.steps.iter().map(|step| step.node.name.clone()).collect()).unwrap_or_default()
}

const PATHS: &str = r#"function validateString(value: string) { if (typeof value !== 'string') throw new Error('x'); }
export const win32 = {
  extname(path: string): string {
    validateString(path);
    return path;
  },
  sep: '\\',
};
export const posix = {
  extname(path: string): string {
    validateString(path);
    return path;
  },
  sep: '/',
};
const isWin = false;
export const extname = (isWin ? win32.extname : posix.extname);
"#;

#[test]
fn conditional_reaches_both_methods_and_the_flow_continues() {
    let (_root, cg) = index(&[
        ("path.ts", PATHS),
        ("mime.ts", "import { extname } from './path';\nexport function getMime(path: string) {\n  return extname(path);\n}\n"),
    ]);
    assert_eq!(alias_targets(&cg, "path.ts", "extname"), vec!["path.ts:posix::extname".to_string(), "path.ts:win32::extname".to_string()]);
    let chain = flow_names(&cg, "getMime extname validateString");
    assert_eq!(chain.first().map(String::as_str), Some("getMime"), "{chain:?}");
    assert_eq!(chain.last().map(String::as_str), Some("validateString"), "{chain:?}");
    assert_eq!(chain.len(), 4, "{chain:?}");
    cg.close();
}

#[test]
fn import_stays_on_the_const() {
    let (_root, cg) = index(&[
        ("path.ts", PATHS),
        ("mime.ts", "import { extname } from './path';\nexport function getMime(path: string) { return extname(path); }\n"),
    ]);
    let get_mime = nodes_in(&cg, "mime.ts").into_iter().find(|n| n.name == "getMime").unwrap();
    let called: Vec<String> = cg
        .get_outgoing_edges(&get_mime.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Calls)
        .map(|e| {
            let target = cg.get_node(&e.target).unwrap().unwrap();
            format!("{} {}", target.kind, target.qualified_name)
        })
        .collect();
    assert_eq!(called, vec!["constant extname".to_string()]);
    cg.close();
}

#[test]
fn hop_names_the_alias_and_where_it_was_written() {
    let (_root, cg) = index(&[("path.ts", PATHS)]);
    let alias = nodes_in(&cg, "path.ts").into_iter().find(|n| n.name == "extname" && n.kind == NodeKind::Constant).unwrap();
    let edge = cg
        .get_outgoing_edges(&alias.id)
        .unwrap()
        .into_iter()
        .find(|e| e.provenance == Some(Provenance::Heuristic))
        .expect("heuristic edge");
    assert_eq!(edge.kind, EdgeKind::Calls);
    assert_eq!(edge.line, Some(alias.start_line));
    let registered = edge.metadata.as_ref().unwrap().get("registeredAt").and_then(|v| v.as_str()).unwrap().replace('\\', "/");
    assert!(registered.ends_with(&format!("path.ts:{}", alias.start_line)), "{registered}");
    assert_eq!(edge.metadata.as_ref().unwrap().get("synthesizedBy").and_then(|v| v.as_str()), Some("const-alias"));
    let summary = describe_synthesized_hop(Some(&edge)).unwrap().summary;
    assert!(summary == "const alias of `win32.extname`" || summary == "const alias of `posix.extname`", "{summary}");
    cg.close();
}

#[test]
fn flow_crosses_the_const_when_the_name_has_many_definitions() {
    let others = (0..4).map(|i| format!("export class Other{i} {{ extname() {{}} }}\n")).collect::<String>();
    let (_root, cg) = index(&[
        ("path.ts", PATHS),
        ("others.ts", &others),
        ("mime.ts", "import { extname } from './path';\nexport function getMime(path: string) { return extname(path); }\n"),
    ]);
    let chain = flow_names(&cg, "getMime extname validateString");
    assert_eq!(chain.first().map(String::as_str), Some("getMime"), "{chain:?}");
    assert_eq!(chain.last().map(String::as_str), Some("validateString"), "{chain:?}");
    cg.close();
}

#[test]
fn plain_nullish_either_and_cast_link_the_members() {
    let (_root, cg) = index(&[(
        "m.ts",
        "export const a = { f() {}, g: () => 1, h: function () {} };\nexport const b = { f() {} };\nexport const plain = a.f;\nexport const nullish = a.g ?? b.f;\nexport const either = a.h || b.f;\nexport const cast = a.f as () => void;\n",
    )]);
    assert_eq!(alias_targets(&cg, "m.ts", "plain"), vec!["m.ts:a::f"]);
    assert_eq!(alias_targets(&cg, "m.ts", "nullish"), vec!["m.ts:a::g", "m.ts:b::f"]);
    assert_eq!(alias_targets(&cg, "m.ts", "either"), vec!["m.ts:a::h", "m.ts:b::f"]);
    assert_eq!(alias_targets(&cg, "m.ts", "cast"), vec!["m.ts:a::f"]);
    cg.close();
}

#[test]
fn imported_namespace_members_link_however_deep() {
    let (_root, cg) = index(&[
        ("path.ts", PATHS),
        ("fns.ts", "export function join() {}\n"),
        (
            "use.ts",
            "import * as fns from './fns';\nimport * as paths from './path';\nexport const direct = fns.join;\nexport const deep = paths.win32.extname;\nexport const chosen = paths.extname;\nexport function run() { chosen('x'); }\n",
        ),
    ]);
    assert_eq!(alias_targets(&cg, "use.ts", "direct"), vec!["fns.ts:join"]);
    assert_eq!(alias_targets(&cg, "use.ts", "deep"), vec!["path.ts:win32::extname"]);
    assert_eq!(alias_targets(&cg, "use.ts", "chosen"), vec!["path.ts:extname"]);
    let chain = flow_names(&cg, "run validateString");
    assert_eq!(chain.last().map(String::as_str), Some("validateString"), "{chain:?}");
    cg.close();
}

#[test]
fn named_import_default_import_and_barrel() {
    let (_root, cg) = index(&[
        ("path.ts", PATHS),
        ("barrel.ts", "export * from './path';\n"),
        ("d.ts", "const api = { run() {} };\nexport default api;\n"),
        ("use.ts", "import { win32 as w } from './barrel';\nimport api from './d';\nexport const named = w.extname;\nexport const fromDefault = api.run;\n"),
    ]);
    assert_eq!(alias_targets(&cg, "use.ts", "named"), vec!["path.ts:win32::extname"]);
    assert_eq!(alias_targets(&cg, "use.ts", "fromDefault"), vec!["d.ts:api::run"]);
    cg.close();
}

#[test]
fn stated_default_and_commonjs_require() {
    let (_root, cg) = index(&[
        ("stated.ts", "export function other() {}\nexport default function parse() {}\n"),
        ("barrel.ts", "export { default } from './stated';\n"),
        ("impl.js", "function impl() {}\nmodule.exports = impl;\n"),
        ("klass.js", "class K { static make() {} }\nmodule.exports = K;\n"),
        ("use.ts", "import parse from './stated';\nimport forwarded from './barrel';\nexport const direct = parse;\nexport const through = forwarded;\n"),
        ("use.js", "const impl = require('./impl');\nconst K = require('./klass');\nconst alias = impl;\nconst make = K.make;\nmodule.exports = { alias, make };\n"),
    ]);
    assert_eq!(alias_targets(&cg, "use.ts", "direct"), vec!["stated.ts:parse"]);
    assert_eq!(alias_targets(&cg, "use.ts", "through"), vec!["stated.ts:parse"]);
    assert_eq!(alias_targets(&cg, "use.js", "alias"), vec!["impl.js:impl"]);
    assert_eq!(alias_targets(&cg, "use.js", "make"), vec!["klass.js:K::make"]);
    cg.close();
}

#[test]
fn object_that_only_names_a_function() {
    let (_root, cg) = index(&[
        ("impl.ts", "export function load() {}\n"),
        ("api.ts", "import { load } from './impl';\nfunction save() {}\nexport const api = { load, write: save };\nexport const fromImport = api.load;\nexport const fromFile = api.write;\n"),
    ]);
    assert_eq!(alias_targets(&cg, "api.ts", "fromImport"), vec!["impl.ts:load"]);
    assert_eq!(alias_targets(&cg, "api.ts", "fromFile"), vec!["api.ts:save"]);
    cg.close();
}

#[test]
fn class_static_method_and_arrow_not_an_instance_method() {
    let (_root, cg) = index(&[
        (
            "m.ts",
            "export class C {\n  static make() {}\n  static arrow = () => 1;\n  run() {}\n}\nexport const make = C.make;\nexport const arrow = C.arrow;\nexport const run = C.run;\n",
        ),
        ("use.ts", "import { C } from './m';\nexport const imported = C.make;\n"),
    ]);
    assert_eq!(alias_targets(&cg, "m.ts", "make"), vec!["m.ts:C::make"]);
    assert_eq!(alias_targets(&cg, "m.ts", "arrow"), vec!["m.ts:C::arrow"]);
    assert_eq!(alias_targets(&cg, "m.ts", "run"), Vec::<String>::new());
    assert_eq!(alias_targets(&cg, "use.ts", "imported"), vec!["m.ts:C::make"]);
    cg.close();
}

#[test]
fn bare_function_names_and_a_chain_through_a_live_const() {
    let (_root, cg) = index(&[
        ("dev.ts", "export function devHandler() {}\n"),
        (
            "m.ts",
            "import { devHandler } from './dev';\nfunction prodHandler() {}\nconst isDev = false;\nexport const handler = isDev ? devHandler : prodHandler;\nexport const renamed = prodHandler;\nexport const again = renamed;\n",
        ),
    ]);
    assert_eq!(alias_targets(&cg, "m.ts", "handler"), vec!["dev.ts:devHandler", "m.ts:prodHandler"]);
    assert_eq!(alias_targets(&cg, "m.ts", "renamed"), vec!["m.ts:prodHandler"]);
    assert_eq!(alias_targets(&cg, "m.ts", "again"), vec!["m.ts:renamed"]);
    cg.close();
}

#[test]
fn let_var_and_an_initializer_longer_than_the_kept_signature() {
    let long = "a_condition_with_a_name_long_enough_to_push_the_initializer_past_what_the_index_keeps_of_it";
    let source = format!(
        "export const a = {{ f() {{}} }};\nexport const b = {{ g() {{}} }};\nlet viaLet = a.f;\nvar viaVar = b.g;\ndeclare const {long}: boolean;\nexport const long = {long} ? a.f : b.g;\nexport const wrapped = {long}\n  ? a.f\n  : b.g;\nexport const closed = check(1) ? a.f : {long} ? b.g : a.f;\ndeclare function check(n: number): boolean;\n"
    );
    let (_root, cg) = index(&[("m.ts", &source)]);
    assert_eq!(alias_targets(&cg, "m.ts", "viaLet"), vec!["m.ts:a::f"]);
    assert_eq!(alias_targets(&cg, "m.ts", "viaVar"), vec!["m.ts:b::g"]);
    assert_eq!(alias_targets(&cg, "m.ts", "long"), vec!["m.ts:a::f", "m.ts:b::g"]);
    assert_eq!(alias_targets(&cg, "m.ts", "wrapped"), vec!["m.ts:a::f", "m.ts:b::g"]);
    assert_eq!(alias_targets(&cg, "m.ts", "closed"), vec!["m.ts:a::f", "m.ts:b::g"]);
    cg.close();
}

#[test]
fn data_members_link_nothing() {
    let (_root, cg) = index(&[
        ("path.ts", PATHS),
        (
            "m.ts",
            "import { win32, posix } from './path';\nexport class C { static size = 3; width = 1; }\nexport enum Color { Red, Blue }\nexport const data = { count: 1, label: 'x', nested: { value: 2 } };\nconst isWin = false;\nexport const sep = isWin ? win32.sep : posix.sep;\nexport const size = C.size;\nexport const width = C.width;\nexport const red = Color.Red;\nexport const count = data.count ?? 0;\nexport const nested = data.nested.value;\nexport const missing = data.nothing;\nexport const flag = isWin;\n",
        ),
    ]);
    for name in ["sep", "size", "width", "red", "count", "nested", "missing", "flag"] {
        assert_eq!(alias_targets(&cg, "m.ts", name), Vec::<String>::new(), "{name}");
    }
    cg.close();
}

#[test]
fn a_const_holding_classes_links_nothing() {
    let (_root, cg) = index(&[("m.ts", "export class A { run() {} }\nexport class B { run() {} }\nconst isA = false;\nexport const Chosen = isA ? A : B;\n")]);
    assert_eq!(alias_targets(&cg, "m.ts", "Chosen"), Vec::<String>::new());
    cg.close();
}

#[test]
fn a_const_chain_that_never_reaches_a_function_links_nothing() {
    let (_root, cg) = index(&[(
        "m.ts",
        "export const size = 3;\nexport const data = { count: 1 };\nexport const first = data.count;\nexport const second = first;\nexport const third = size;\n",
    )]);
    for name in ["first", "second", "third"] {
        assert_eq!(alias_targets(&cg, "m.ts", name), Vec::<String>::new(), "{name}");
    }
    cg.close();
}

#[test]
fn two_consts_that_only_name_each_other_link_nothing() {
    let (_root, cg) = index(&[("m.ts", "export const a = b;\nexport const b = a;\n")]);
    assert_eq!(alias_targets(&cg, "m.ts", "a"), Vec::<String>::new());
    assert_eq!(alias_targets(&cg, "m.ts", "b"), Vec::<String>::new());
    cg.close();
}

#[test]
fn an_unbound_receiver_links_nothing() {
    let (_root, cg) = index(&[
        ("other.ts", "export function keys() {}\nexport class Service { handler() {} }\nexport const config = { handler() {} };\n"),
        (
            "m.ts",
            "export const fromGlobal = Object.keys;\nexport const fromUnknown = settings.handler;\ndeclare const settings: { handler(): void };\nexport const fromWindow = window.config.handler;\n",
        ),
    ]);
    for name in ["fromGlobal", "fromUnknown", "fromWindow"] {
        assert_eq!(alias_targets(&cg, "m.ts", name), Vec::<String>::new(), "{name}");
    }
    cg.close();
}

#[test]
fn an_unstated_default_links_nothing() {
    let (_root, cg) = index(&[
        ("first-class.ts", "export class K { static make() {} }\ndeclare function build(): unknown;\nexport default build();\n"),
        ("first-function.ts", "export function helper() {}\ndeclare function build(): unknown;\nexport default build();\n"),
        ("exports.js", "exports.first = function () {};\nexports.second = function second() {};\n"),
        (
            "use.ts",
            "import client from './first-function';\nimport made from './first-class';\nexport const bare = client;\nexport const member = made.make;\n",
        ),
        ("use.js", "const named = require('./exports');\nconst whole = named;\nconst second = named.second;\nmodule.exports = { whole, second };\n"),
    ]);
    assert_eq!(alias_targets(&cg, "use.ts", "bare"), Vec::<String>::new());
    assert_eq!(alias_targets(&cg, "use.ts", "member"), Vec::<String>::new());
    assert_eq!(alias_targets(&cg, "use.js", "whole"), Vec::<String>::new());
    assert_eq!(alias_targets(&cg, "use.js", "second"), Vec::<String>::new());
    cg.close();
}

#[test]
fn a_package_member_links_nothing() {
    let (_root, cg) = index(&[
        ("other.ts", "export function readFile() {}\n"),
        ("m.ts", "import * as fs from 'fs';\nimport { promises } from 'fs';\nexport const read = fs.readFile;\nexport const readAsync = promises.readFile;\n"),
    ]);
    assert_eq!(alias_targets(&cg, "m.ts", "read"), Vec::<String>::new());
    assert_eq!(alias_targets(&cg, "m.ts", "readAsync"), Vec::<String>::new());
    cg.close();
}

#[test]
fn a_call_a_bind_or_a_new_links_nothing() {
    let (_root, cg) = index(&[(
        "m.ts",
        "export const a = { f() {} };\nfunction make() { return a.f; }\nexport const called = a.f();\nexport const bound = a.f.bind(a);\nexport const built = make();\n",
    )]);
    for name in ["called", "bound", "built"] {
        assert_eq!(alias_targets(&cg, "m.ts", name), Vec::<String>::new(), "{name}");
    }
    cg.close();
}

#[test]
fn a_function_local_const_is_not_linked() {
    let (_root, cg) = index(&[("m.ts", "export const a = { f() {} };\nexport function run(a: { f(): void }) {\n  const f = a.f;\n  return f();\n}\n")]);
    let run = nodes_in(&cg, "m.ts").into_iter().find(|n| n.name == "run").unwrap();
    let heuristic: Vec<_> = cg.get_outgoing_edges(&run.id).unwrap().into_iter().filter(|e| e.provenance == Some(Provenance::Heuristic)).collect();
    assert!(heuristic.is_empty(), "{heuristic:?}");
    cg.close();
}

#[test]
fn a_generated_bundle_links_nothing() {
    let (_root, cg) = index(&[("app.min.js", "function b() {}\nvar a = b;\n")]);
    assert_eq!(alias_targets(&cg, "app.min.js", "a"), Vec::<String>::new());
    cg.close();
}

#[test]
fn sync_links_an_alias_a_file_gains() {
    let (root, mut cg) = index(&[("a.ts", "export const a = { f() {} };\n"), ("b.ts", "export const b = 1;\n")]);
    write_files(root.path(), &[("b.ts", "import { a } from './a';\nexport const b = a.f;\n")]);
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(alias_targets(&cg, "b.ts", "b"), vec!["a.ts:a::f"]);
    cg.close();
}

#[test]
fn sync_links_a_target_added_after_the_alias() {
    let (root, mut cg) = index(&[
        ("fns.ts", "export function other() {}\n"),
        ("use.ts", "import * as fns from './fns';\nexport const late = fns.join;\n"),
    ]);
    assert_eq!(alias_targets(&cg, "use.ts", "late"), Vec::<String>::new());
    write_files(root.path(), &[("fns.ts", "export function other() {}\nexport function join() {}\n")]);
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(alias_targets(&cg, "use.ts", "late"), vec!["fns.ts:join"]);
    cg.close();
}

#[test]
fn sync_links_a_target_among_more_definitions_than_the_lookup_cap() {
    let (root, mut cg) = index(&[
        ("fns.ts", "export function other() {}\n"),
        ("use.ts", "import * as fns from './fns';\nexport const late = fns.join;\n"),
    ]);
    let many = (0..80).map(|i| format!("export function f{i}() {{}}\n")).collect::<String>();
    write_files(root.path(), &[("fns.ts", &format!("export function other() {{}}\nexport function join() {{}}\n{many}"))]);
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(alias_targets(&cg, "use.ts", "late"), vec!["fns.ts:join"]);
    cg.close();
}

#[test]
fn sync_links_a_target_in_a_file_it_adds() {
    let (root, mut cg) = index(&[("use.ts", "import * as fns from './fns';\nexport const late = fns.join;\n")]);
    write_files(root.path(), &[("fns.ts", "export function join() {}\n")]);
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(alias_targets(&cg, "use.ts", "late"), vec!["fns.ts:join"]);
    cg.close();
}

#[test]
fn sync_drops_the_link_when_the_target_goes_away() {
    let (root, mut cg) = index(&[
        ("fns.ts", "export function join() {}\nexport function other() {}\n"),
        ("use.ts", "import * as fns from './fns';\nexport const alias = fns.join;\n"),
    ]);
    assert_eq!(alias_targets(&cg, "use.ts", "alias"), vec!["fns.ts:join"]);
    write_files(root.path(), &[("fns.ts", "export function other() {}\n")]);
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(alias_targets(&cg, "use.ts", "alias"), Vec::<String>::new());
    cg.close();
}
