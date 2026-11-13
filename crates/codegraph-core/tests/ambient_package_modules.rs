//! A `declare module` answers its own package, and a module this repository
//! implements comes first (upstream 4499d007, #3157).
use codegraph_core::{CodeGraph, IndexOptions};
use codegraph_types::EdgeKind;
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

fn edges_of(cg: &CodeGraph) -> Vec<String> {
    let nodes = cg.store().get_all_nodes().unwrap();
    let mut out = Vec::new();
    for node in &nodes {
        for edge in cg.get_outgoing_edges(&node.id).unwrap() {
            let kind = match edge.kind {
                EdgeKind::Calls => "calls",
                EdgeKind::Imports => "imports",
                EdgeKind::Extends => "extends",
                EdgeKind::Instantiates => "instantiates",
                EdgeKind::References => "references",
                _ => continue,
            };
            let Some(target) = cg.get_node(&edge.target).unwrap() else { continue };
            out.push(format!(
                "{kind} {}@{} -> {}@{}",
                node.qualified_name, node.file_path, target.qualified_name, target.file_path
            ));
        }
    }
    out.sort();
    out.dedup();
    out
}

fn touching(edges: &[String], file: &str) -> Vec<String> {
    edges.iter().filter(|e| e.contains(file)).cloned().collect()
}

fn has(edges: &[String], needle: &str) -> bool {
    edges.iter().any(|e| e.contains(needle))
}

fn copy_sources(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() == ".codegraphV" {
            continue;
        }
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir_all(&dest).unwrap();
            copy_sources(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}

fn fresh_edges(root: &Path) -> Vec<String> {
    let copy = tempfile::tempdir().unwrap();
    copy_sources(root, copy.path());
    let cg = CodeGraph::init_and_index(copy.path().to_str().unwrap(), None).unwrap();
    let edges = edges_of(&cg);
    cg.close();
    edges
}

const ROOT: &str = r#"{"name":"root","private":true,"workspaces":["packages/*"]}"#;

fn flags() -> Vec<(&'static str, &'static str)> {
    vec![
        ("package.json", ROOT),
        ("packages/eslint/package.json", r#"{"name":"eslint-plugin"}"#),
        (
            "packages/eslint/src/shared/ReactFeatureFlags.d.ts",
            "declare module 'shared/ReactFeatureFlags' {\n  export const enableFoo: boolean;\n  export const onlyDeclared: boolean;\n}\n",
        ),
        (
            "packages/eslint/src/rule.ts",
            "import { enableFoo, onlyDeclared } from 'shared/ReactFeatureFlags';\nexport const on = enableFoo && onlyDeclared;\n",
        ),
        ("packages/reconciler/package.json", r#"{"name":"reconciler"}"#),
        (
            "packages/reconciler/src/work.js",
            "import { enableFoo, flagName, onlyDeclared } from 'shared/ReactFeatureFlags';\nexport function work() {\n  return enableFoo && onlyDeclared ? flagName() : null;\n}\n",
        ),
        ("packages/shared/package.json", r#"{"name":"shared"}"#),
        ("packages/shared/ReactFeatureFlags.js", "export const enableFoo = true;\nexport function flagName() { return 'foo'; }\n"),
    ]
}

#[test]
fn real_module_wins_over_another_packages_stub() {
    let files = flags();
    let (_root, cg) = index(&files);
    let got = edges_of(&cg);
    let work = touching(&got, "packages/reconciler/src/work.js");
    assert!(has(&work, "enableFoo@packages/shared/ReactFeatureFlags.js"), "{work:?}");
    assert!(has(&work, "flagName@packages/shared/ReactFeatureFlags.js"), "{work:?}");
    assert!(!work.iter().any(|e| e.contains("packages/eslint/")), "{work:?}");
    let rule = touching(&got, "packages/eslint/src/rule.ts");
    assert!(has(&rule, "enableFoo@packages/shared/ReactFeatureFlags.js"), "{rule:?}");
    assert!(!has(&rule, "enableFoo@packages/eslint/src/shared/ReactFeatureFlags.d.ts"), "{rule:?}");
    assert!(has(&rule, "onlyDeclared@packages/eslint/src/shared/ReactFeatureFlags.d.ts"), "{rule:?}");
    cg.close();

    let (_root, cg) = index(&[
        ("package.json", ROOT),
        ("packages/types-only/package.json", r#"{"name":"types-only","types":"index.d.ts"}"#),
        ("packages/types-only/index.d.ts", "export function make(): string;\n"),
        ("packages/app/package.json", r#"{"name":"app"}"#),
        ("packages/app/typings/own.d.ts", "declare module 'types-only' {\n  export function make(): number;\n}\n"),
        ("packages/app/src/use.ts", "import { make } from 'types-only';\nexport const made = make();\n"),
    ]);
    let app = touching(&edges_of(&cg), "packages/app/src/use.ts");
    assert!(has(&app, "make@packages/app/typings/own.d.ts"), "{app:?}");
    assert!(!has(&app, "packages/types-only/"), "{app:?}");
    cg.close();
}

#[test]
fn a_declare_module_answers_the_packages_that_can_see_it() {
    let stub = "declare module 'untyped-lib' {\n  export function make(): string;\n  export interface Made { id: string }\n}\n";
    let source = "import { make, Made } from 'untyped-lib';\nexport const made: Made = make();\n";
    let (_root, cg) = index(&[
        ("package.json", r#"{"name":"mono","private":true,"workspaces":["packages/*"]}"#),
        ("packages/typed/package.json", r#"{"name":"typed","dependencies":{"untyped-lib":"^1.0.0"}}"#),
        ("packages/typed/typings/lib.d.ts", stub),
        ("packages/typed/src/use.ts", source),
        ("packages/typed/src/deep/nested/use.ts", source),
        ("packages/listed/package.json", r#"{"name":"listed","dependencies":{"untyped-lib":"^1.0.0"}}"#),
        ("packages/listed/src/use.ts", source),
        ("packages/unlisted/package.json", r#"{"name":"unlisted"}"#),
        ("packages/unlisted/src/use.ts", source),
    ]);
    let got = edges_of(&cg);
    for file in ["packages/typed/src/use.ts", "packages/typed/src/deep/nested/use.ts"] {
        let edges = touching(&got, file);
        assert!(has(&edges, "make@packages/typed/typings/lib.d.ts"), "{file}: {edges:?}");
        assert!(has(&edges, "Made@packages/typed/typings/lib.d.ts"), "{file}: {edges:?}");
    }
    assert!(touching(&got, "packages/listed/src/use.ts").is_empty(), "{got:?}");
    assert!(touching(&got, "packages/unlisted/src/use.ts").is_empty(), "{got:?}");
    cg.close();

    let source = "import { getImage } from 'astro:assets';\nexport const image = getImage();\n";
    let (_root, cg) = index(&[
        ("package.json", r#"{"name":"astro-monorepo","private":true,"workspaces":["packages/*","examples/*"]}"#),
        ("packages/astro/package.json", r#"{"name":"astro"}"#),
        ("packages/astro/client.d.ts", "declare module 'astro:assets' {\n  export function getImage(): string;\n}\n"),
        ("packages/astro/src/own.ts", source),
        ("packages/mdx/package.json", r#"{"name":"@astrojs/mdx","peerDependencies":{"astro":"^7.0.0"}}"#),
        ("packages/mdx/src/server.ts", source),
        ("examples/blog/package.json", r#"{"name":"@example/blog","dependencies":{"astro":"^7.0.0"}}"#),
        ("examples/blog/src/pages/deep/index.ts", source),
        ("examples/plain/package.json", r#"{"name":"@example/plain","dependencies":{"other":"^1.0.0"}}"#),
        ("examples/plain/src/index.ts", source),
    ]);
    let got = edges_of(&cg);
    for file in ["packages/astro/src/own.ts", "packages/mdx/src/server.ts", "examples/blog/src/pages/deep/index.ts"] {
        assert!(has(&touching(&got, file), "getImage@packages/astro/client.d.ts"), "{file}");
    }
    assert!(touching(&got, "examples/plain/src/index.ts").is_empty());
    cg.close();

    let (_root, cg) = index(&[
        ("package.json", r#"{"name":"code-oss"}"#),
        (
            "src/vscode-dts/vscode.d.ts",
            "declare module 'vscode' {\n  export class Disposable { dispose(): void; }\n}\n",
        ),
        ("extensions/git/package.json", r#"{"name":"git"}"#),
        ("extensions/git/src/main.ts", "import { Disposable } from 'vscode';\nexport const held = new Disposable();\n"),
        ("extensions/git/test/fixtures/app/package.json", r#"{"name":"fixture-app"}"#),
        ("extensions/git/test/fixtures/app/use.ts", "import { Disposable } from 'vscode';\nexport const held = new Disposable();\n"),
    ]);
    let got = edges_of(&cg);
    for file in ["extensions/git/src/main.ts", "extensions/git/test/fixtures/app/use.ts"] {
        let edges = touching(&got, file);
        assert!(has(&edges, "Disposable@src/vscode-dts/vscode.d.ts"), "{file}: {edges:?}");
    }
    cg.close();

    let (_root, cg) = index(&[
        ("package.json", r#"{"name":"app","dependencies":{"untyped-lib":"^1.0.0"}}"#),
        ("src/esm/package.json", r#"{"type":"module"}"#),
        ("src/esm/lib.d.ts", "declare module 'untyped-lib' {\n  export function make(): string;\n}\n"),
        ("src/use.ts", "import { make } from 'untyped-lib';\nexport const made = make();\n"),
    ]);
    assert!(has(&edges_of(&cg), "make@src/esm/lib.d.ts"));
    cg.close();

    let (_root, cg) = index(&[
        ("package.json", r#"{"name":"mono","private":true,"workspaces":["apps/*"]}"#),
        ("typings/lib.d.ts", "declare module 'untyped-lib' {\n  export function make(): string;\n}\n"),
        ("apps/a/package.json", r#"{"name":"a"}"#),
        ("apps/a/src/deep/lib.d.ts", "declare module 'untyped-lib' {\n  export function make(): number;\n}\n"),
        ("apps/a/src/deep/use.ts", "import { make } from 'untyped-lib';\nexport const made = make();\n"),
        ("apps/b/package.json", r#"{"name":"b"}"#),
        ("apps/b/src/deep/use.ts", "import { make } from 'untyped-lib';\nexport const made = make();\n"),
    ]);
    let got = edges_of(&cg);
    assert!(has(&touching(&got, "apps/a/src/deep/use.ts"), "make@apps/a/src/deep/lib.d.ts"));
    assert!(has(&touching(&got, "apps/b/src/deep/use.ts"), "make@typings/lib.d.ts"));
    cg.close();

    let (_root, cg) = index(&[
        ("package.json", ROOT),
        ("packages/a/package.json", r#"{"name":"a"}"#),
        ("packages/a/lib.d.ts", "declare module 'untyped-lib' {\n  export interface Made extends Base {}\n  export class Built extends Core {}\n}\n"),
        ("packages/b/package.json", r#"{"name":"b"}"#),
        ("packages/b/lib.d.ts", "declare module 'untyped-lib' {\n  export interface Base { id: string }\n  export class Core {}\n}\n"),
    ]);
    assert!(touching(&edges_of(&cg), "packages/a/lib.d.ts").is_empty());
    cg.close();

    let (_root, cg) = index(&[
        ("package.json", ROOT),
        ("packages/a/package.json", r#"{"name":"a"}"#),
        ("packages/a/lib.d.ts", "declare module 'untyped-lib' {\n  export interface Made { id: string }\n}\n"),
        ("packages/b/package.json", r#"{"name":"b"}"#),
        ("packages/b/src/use.ts", "import type { Made } from '../../a/lib';\nexport function id(made: Made): string { return made.id; }\n"),
    ]);
    assert!(has(&touching(&edges_of(&cg), "packages/b/src/use.ts"), "Made@packages/a/lib.d.ts"));
    cg.close();

    let (_root, cg) = index(&[
        ("package.json", ROOT),
        ("packages/react/package.json", r#"{"name":"react","main":"index.js"}"#),
        ("packages/react/index.js", "export function createElement() { return null; }\n"),
        (
            "packages/react/src/__tests__/testDefinitions/React.d.ts",
            "declare module 'react' {\n  export function createElement(): unknown;\n}\n",
        ),
        (
            "packages/react/src/__tests__/testDefinitions/PropTypes.d.ts",
            "declare module 'prop-types' {\n  export function string(): unknown;\n}\n",
        ),
        (
            "packages/react/src/__tests__/Own-test.ts",
            "import { createElement } from 'react';\nimport { string } from 'prop-types';\nexport const made = [createElement(), string()];\n",
        ),
        ("packages/dom/package.json", r#"{"name":"dom","peerDependencies":{"react":"*"}}"#),
        (
            "packages/dom/src/__tests__/Dom-test.js",
            "import { createElement } from 'react';\nimport { string } from 'prop-types';\nexport const made = [createElement(), string()];\n",
        ),
        ("packages/dom/src/render.js", "import { string } from 'prop-types';\nexport const made = string();\n"),
    ]);
    let got = edges_of(&cg);
    let own = touching(&got, "packages/react/src/__tests__/Own-test.ts");
    assert!(has(&own, "createElement@packages/react/index.js"), "{own:?}");
    assert!(has(&own, "string@packages/react/src/__tests__/testDefinitions/PropTypes.d.ts"), "{own:?}");
    assert!(!own.iter().any(|e| e.contains("React.d.ts")), "{own:?}");
    let other = touching(&got, "packages/dom/src/__tests__/Dom-test.js");
    assert!(has(&other, "createElement@packages/react/index.js"), "{other:?}");
    assert!(!other.iter().any(|e| e.contains("testDefinitions")), "{other:?}");
    assert!(touching(&got, "packages/dom/src/render.js").is_empty(), "{got:?}");
    cg.close();
}

#[test]
fn a_stub_is_no_rival_where_it_does_not_count() {
    let (_root, cg) = index(&[
        ("package.json", ROOT),
        ("packages/react/package.json", r#"{"name":"react","main":"index.js"}"#),
        ("packages/react/index.js", "export { createRef } from './src/ReactClient';\n"),
        ("packages/react/src/ReactClient.js", "import { createRef } from './ReactCreateRef';\nexport { createRef };\n"),
        ("packages/react/src/ReactCreateRef.js", "/** @flow */\nexport function createRef() { return { current: null }; }\n"),
        (
            "packages/react/src/__tests__/testDefinitions/React.d.ts",
            "declare module 'react' {\n  export function createRef(): unknown;\n}\n",
        ),
        ("fixtures/clock/package.json", r#"{"name":"clock","dependencies":{"react":"^19.0.0"}}"#),
        ("fixtures/clock/src/Clock.js", "import { createRef } from 'react';\nexport const faceRef = createRef();\n"),
    ]);
    let edges = touching(&edges_of(&cg), "fixtures/clock/src/Clock.js");
    assert!(has(&edges, "createRef@packages/react/src/ReactCreateRef.js"), "{edges:?}");
    assert!(!edges.iter().any(|e| e.contains(".d.ts")), "{edges:?}");
    cg.close();
}

#[test]
fn sync_moves_importers_onto_a_real_module() {
    let packages = [
        ("package.json", ROOT),
        ("packages/eslint/package.json", r#"{"name":"eslint-plugin"}"#),
        ("packages/eslint/src/flags.d.ts", "declare module 'shared/flags' {\n  export const enableFoo: boolean;\n}\n"),
        ("packages/eslint/src/rule.ts", "import { enableFoo } from 'shared/flags';\nexport const on = enableFoo;\n"),
        ("packages/reconciler/package.json", r#"{"name":"reconciler"}"#),
        ("packages/reconciler/src/work.js", "import { enableFoo } from 'shared/flags';\nexport const on = enableFoo;\n"),
        ("packages/shared/package.json", r#"{"name":"shared"}"#),
        ("packages/shared/other.js", "export const other = 1;\n"),
    ];
    let root = tempfile::tempdir().unwrap();
    write_files(root.path(), &packages);
    let mut cg = CodeGraph::init_and_index(root.path().to_str().unwrap(), None).unwrap();
    assert!(has(&edges_of(&cg), "enableFoo@packages/eslint/src/flags.d.ts"));
    assert!(touching(&edges_of(&cg), "packages/reconciler/src/work.js").is_empty());

    write_files(root.path(), &[("packages/shared/flags.js", "export const enableFoo = true;\n")]);
    cg.sync(&IndexOptions::default(), None).unwrap();
    let synced = edges_of(&cg);
    assert_eq!(synced, fresh_edges(root.path()));
    assert!(has(&touching(&synced, "packages/eslint/src/rule.ts"), "enableFoo@packages/shared/flags.js"), "{synced:?}");
    assert!(has(&touching(&synced, "packages/reconciler/src/work.js"), "enableFoo@packages/shared/flags.js"), "{synced:?}");

    fs::remove_file(root.path().join("packages/shared/flags.js")).unwrap();
    cg.sync(&IndexOptions::default(), None).unwrap();
    let synced = edges_of(&cg);
    assert_eq!(synced, fresh_edges(root.path()));
    assert!(has(&touching(&synced, "packages/eslint/src/rule.ts"), "enableFoo@packages/eslint/src/flags.d.ts"), "{synced:?}");
    assert!(touching(&synced, "packages/reconciler/src/work.js").is_empty(), "{synced:?}");
    cg.close();

    let mut without_stub: Vec<(&str, &str)> = packages.into_iter().filter(|(p, _)| *p != "packages/eslint/src/flags.d.ts").collect();
    without_stub.push(("packages/shared/flags.js", "export const enableFoo = true;\n"));
    let root = tempfile::tempdir().unwrap();
    write_files(root.path(), &without_stub);
    let mut cg = CodeGraph::init_and_index(root.path().to_str().unwrap(), None).unwrap();
    write_files(root.path(), &[("packages/eslint/src/flags.d.ts", "declare module 'shared/flags' {\n  export const enableFoo: boolean;\n}\n")]);
    cg.sync(&IndexOptions::default(), None).unwrap();
    let synced = edges_of(&cg);
    assert_eq!(synced, fresh_edges(root.path()));
    assert!(has(&touching(&synced, "packages/eslint/src/rule.ts"), "enableFoo@packages/shared/flags.js"), "{synced:?}");
    assert!(has(&touching(&synced, "packages/reconciler/src/work.js"), "enableFoo@packages/shared/flags.js"), "{synced:?}");
    cg.close();
}

#[test]
fn a_single_package_declaration_still_links() {
    let (_root, cg) = index(&[
        ("package.json", r#"{"name":"app"}"#),
        ("typings/lib.d.ts", "declare module 'untyped-lib' {\n  export function make(): string;\n}\n"),
        ("src/use.ts", "import { make } from 'untyped-lib';\nexport const made = make();\n"),
    ]);
    assert!(has(&edges_of(&cg), "make@typings/lib.d.ts"));
    cg.close();
}
