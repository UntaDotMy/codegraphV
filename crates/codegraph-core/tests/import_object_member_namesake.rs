//! Upstream 59eb0d28: an import names the module's own binding, never a method of an object in
//! it. Ported from `__tests__/import-object-member-namesake.test.ts`.
use codegraph_core::CodeGraph;
use codegraph_types::EdgeKind;
use std::fs;

fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, source) in files {
        let path = dir.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    dir
}

fn calls_from(cg: &CodeGraph, file: &str, function: &str) -> Vec<String> {
    let nodes = cg.store().get_all_nodes().unwrap();
    let src = nodes.iter().find(|n| n.file_path == file && n.name == function).unwrap();
    let mut out: Vec<_> = cg.store().get_outgoing_edges(&src.id, &[EdgeKind::Calls], None).unwrap()
        .into_iter()
        .filter_map(|e| nodes.iter().find(|n| n.id == e.target))
        .map(|n| format!("{}:{}:{}", n.file_path, n.qualified_name, n.start_line))
        .collect();
    out.sort();
    out
}

fn index(files: &[(&str, &str)]) -> (tempfile::TempDir, CodeGraph) {
    let dir = project(files);
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

const WALK: &str = "import { f } from './m';\nexport function walk() { return f(); }\n";

#[test]
fn an_exported_object_method_does_not_take_a_named_import() {
    for (source, target) in [
        (
            "export interface Driver { f(id: string): string; }\nexport const d: Driver = {\n  f(id) { return '#' + id; },\n};\nlet driver: Driver = d;\nexport function f(id: string): string { return driver.f(id); }\n",
            "m.ts:f:6",
        ),
        (
            "export function f(): string { return '#'; }\nexport const d = {\n  f() { return f(); },\n};\n",
            "m.ts:f:1",
        ),
    ] {
        let (_dir, cg) = index(&[("m.ts", source), ("walk.ts", WALK)]);
        assert_eq!(calls_from(&cg, "walk.ts", "walk"), [target]);
        cg.close();
    }
}

#[test]
fn export_clauses_reexports_and_namespaces_name_the_modules_binding() {
    let (_dir, cg) = index(&[
        ("m.ts", "export const d = { f() {} };\nfunction f() {}\nfunction impl() {}\nexport { f, impl as g };\n"),
        ("walk.ts", "import { f, g } from './m';\nexport function walk() { f(); g(); }\n"),
    ]);
    assert_eq!(calls_from(&cg, "walk.ts", "walk"), ["m.ts:f:2", "m.ts:impl:3"]);
    cg.close();

    let (_dir, cg) = index(&[
        ("m.ts", "export const d = { f() {} };\nexport { f } from './other';\n"),
        ("other.ts", "export function f() {}\n"),
        ("walk.ts", WALK),
    ]);
    assert_eq!(calls_from(&cg, "walk.ts", "walk"), ["other.ts:f:1"]);
    cg.close();

    let (_dir, cg) = index(&[
        ("m.ts", "export const d = { f() {} };\nexport function f() {}\n"),
        ("walk.ts", "import * as m from './m';\nexport function walk() { return m.f(); }\n"),
    ]);
    assert_eq!(calls_from(&cg, "walk.ts", "walk"), ["m.ts:f:2"]);
    cg.close();
}

#[test]
fn a_default_binding_is_the_module_binding_and_object_calls_still_reach_their_method() {
    let (_dir, cg) = index(&[
        ("m.ts", "const d = { f() { return 1; } };\nfunction f() { return d; }\nexport default f;\n"),
        ("walk.ts", "import X from './m';\nexport function walk() { return X(); }\n"),
    ]);
    assert_eq!(calls_from(&cg, "walk.ts", "walk"), ["m.ts:f:2"]);
    cg.close();

    // Negative controls: a method reached through its object, and a class method that no module
    // binding exports under that name.
    let (_dir, cg) = index(&[
        ("m.ts", "export const d = { f() {} };\n"),
        ("walk.ts", "import { d } from './m';\nexport function walk() { return d.f(); }\n"),
    ]);
    assert_eq!(calls_from(&cg, "walk.ts", "walk"), ["m.ts:d::f:1"]);
    cg.close();

    let (_dir, cg) = index(&[
        ("m.ts", "export class C { render() {} }\n"),
        ("walk.ts", "import { render } from './m';\nexport function walk() { return render(); }\n"),
    ]);
    assert_eq!(calls_from(&cg, "walk.ts", "walk"), Vec::<String>::new());
    cg.close();

    // An explicit destructuring export does bind the object's method under the new name.
    let (_dir, cg) = index(&[
        ("m.ts", "export const d = { f() {} };\nexport const { f } = d;\n"),
        ("walk.ts", WALK),
    ]);
    assert_eq!(calls_from(&cg, "walk.ts", "walk"), ["m.ts:d::f:1"]);
    cg.close();
}
