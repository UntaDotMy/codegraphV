//! A package import follows the page, and stops at a monorepo's own edge
//! (upstream 3bd99495, #3166).
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

fn calls_of(cg: &CodeGraph, name: &str) -> Vec<String> {
    let nodes = cg.store().get_all_nodes().unwrap();
    let node = nodes
        .iter()
        .find(|n| n.name == name && n.kind == NodeKind::Function)
        .unwrap_or_else(|| panic!("no function {name}"));
    let mut out = Vec::new();
    for edge in cg.get_outgoing_edges(&node.id).unwrap() {
        if edge.kind != EdgeKind::Calls {
            continue;
        }
        let Some(target) = cg.get_node(&edge.target).unwrap() else { continue };
        out.push(format!("{}:{}", slash(&target.file_path), target.qualified_name));
    }
    out.sort();
    out
}

const DUAL_PKG: &str = r#"{
  "name": "dual",
  "version": "1.0.0",
  "exports": {
    "./client": {
      "workerd": "./client.edge.js",
      "node": "./client.node.js",
      "browser": "./client.browser.js",
      "default": "./client.browser.js"
    }
  }
}
"#;

fn dual() -> Vec<(&'static str, &'static str)> {
    vec![
        ("package.json", "{ \"name\": \"root\", \"private\": true, \"workspaces\": [\"packages/*\"] }\n"),
        ("packages/dual/package.json", DUAL_PKG),
        ("packages/dual/client.node.js", "export function connect() {}\n"),
        ("packages/dual/client.browser.js", "export function connect() {}\n"),
        ("packages/dual/client.edge.js", "export function connect() {}\n"),
    ]
}

fn with_dual<'a>(extra: &'a [(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut files = dual();
    files.extend_from_slice(extra);
    files
}

const NESTED: &[(&str, &str)] = &[
    ("package.json", "{ \"name\": \"root\", \"private\": true, \"workspaces\": [\"packages/*\"] }\n"),
    ("packages/core/package.json", "{ \"name\": \"core\", \"version\": \"1.0.0\", \"main\": \"src/index.js\" }\n"),
    ("packages/core/src/index.js", "export function useThing() { return 1; }\n"),
    ("scripts/jest/jest.d.ts", "declare function expect(val: any): any;\n"),
    (
        "compiler/package.json",
        "{ \"name\": \"compiler-root\", \"private\": true, \"workspaces\": { \"packages\": [\"packages/*\"] } }\n",
    ),
    ("compiler/packages/n/package.json", "{ \"name\": \"n\", \"version\": \"1.0.0\", \"devDependencies\": { \"core\": \"9.9.9\" } }\n"),
];

fn with_nested<'a>(extra: &'a [(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut files = NESTED.to_vec();
    files.extend_from_slice(extra);
    files
}

#[test]
fn a_file_reading_the_page_imports_the_browser_condition() {
    let (_root, cg) = index(&with_dual(&[(
        "app/page.js",
        "import {connect} from 'dual/client';\nexport function boot() {\n  document.title = 'x';\n  connect();\n}\n",
    )]));
    assert_eq!(calls_of(&cg, "boot"), vec!["packages/dual/client.browser.js:connect".to_string()]);
    cg.close();
}

#[test]
fn a_file_that_never_touches_the_page_imports_the_node_condition() {
    let (_root, cg) = index(&with_dual(&[(
        "app/server.js",
        "import {connect} from 'dual/client';\nexport function serve() {\n  connect();\n}\n",
    )]));
    assert_eq!(calls_of(&cg, "serve"), vec!["packages/dual/client.node.js:connect".to_string()]);
    cg.close();
}

#[test]
fn a_comment_a_string_or_a_property_is_not_the_page() {
    let (_root, cg) = index(&with_dual(&[(
        "app/server.js",
        "// document.title is the browser's\nimport {connect} from 'dual/client';\nexport function serve(res, page) {\n  res.document.x = 1;\n  page.evaluate('window.scrollTo(0, 0)');\n  connect();\n}\n",
    )]));
    assert_eq!(calls_of(&cg, "serve"), vec!["packages/dual/client.node.js:connect".to_string()]);
    cg.close();
}

#[test]
fn a_nested_workspace_keeps_the_outer_packages_out() {
    let (_root, cg) = index(&with_nested(&[(
        "compiler/packages/n/src/a.ts",
        "import { useThing } from 'core';\nexport function a() { return useThing(); }\n",
    )]));
    assert_eq!(calls_of(&cg, "a"), Vec::<String>::new());
    cg.close();
}

#[test]
fn a_nested_workspace_keeps_the_outer_globals_out() {
    let (_root, cg) = index(&with_nested(&[(
        "compiler/packages/n/src/a.test.ts",
        "export function a() { return expect(1); }\n",
    )]));
    assert_eq!(calls_of(&cg, "a"), Vec::<String>::new());
    cg.close();
}

#[test]
fn files_outside_a_nested_workspace_stay_linked() {
    let (_root, cg) = index(&with_nested(&[
        (
            "packages/core/src/index.test.ts",
            "import { useThing } from './index';\nexport function a() { return expect(useThing()); }\n",
        ),
        ("fixtures/app/src/index.ts", "import { useThing } from 'core';\nexport function b() { return useThing(); }\n"),
    ]));
    assert_eq!(
        calls_of(&cg, "a"),
        vec!["packages/core/src/index.js:useThing".to_string(), "scripts/jest/jest.d.ts:expect".to_string()]
    );
    assert_eq!(calls_of(&cg, "b"), vec!["packages/core/src/index.js:useThing".to_string()]);
    cg.close();
}

#[test]
fn a_member_of_the_outer_workspace_still_reaches_it() {
    let (_root, cg) = index(&with_nested(&[
        ("packages/sub/package.json", "{ \"name\": \"sub\", \"version\": \"1.0.0\", \"workspaces\": [\"inner/*\"] }\n"),
        ("packages/sub/src/a.ts", "import { useThing } from 'core';\nexport function a() { return useThing(); }\n"),
    ]));
    assert_eq!(calls_of(&cg, "a"), vec!["packages/core/src/index.js:useThing".to_string()]);
    cg.close();
}

#[test]
fn a_nested_pnpm_workspace_keeps_the_outer_packages_out() {
    let (_root, cg) = index(&with_nested(&[
        ("compiler/package.json", "{ \"name\": \"compiler-root\", \"private\": true }\n"),
        ("compiler/pnpm-workspace.yaml", "packages:\n  - packages/*\n"),
        ("compiler/packages/n/src/a.ts", "import { useThing } from 'core';\nexport function a() { return useThing(); }\n"),
    ]));
    assert_eq!(calls_of(&cg, "a"), Vec::<String>::new());
    cg.close();
}
