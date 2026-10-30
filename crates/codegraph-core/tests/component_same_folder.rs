//! The folder preference of the React, Svelte, Astro and Vue component resolvers, which pick among
//! several definitions of a component name (upstream 8bd55fa7).
//!
//! They preferred a definition whose path started with the referencing file's folder, compared as
//! text: `src/ui` also starts `src/ui-kit/Widget.tsx`, so `src/ui/Card.tsx`'s `Widget` could land
//! in the sibling folder. A file at the project root has the folder `""`, which every path starts
//! with, so its pick was whatever definition came first, skipping the rule that a name with several
//! definitions and nothing to tell them apart links nothing (#764).
//!
//! The folder is now compared whole, a folder below it still counts, and a root file has no folder
//! to prefer.
use codegraph_core::CodeGraph;
use std::fs;

fn index(files: &[(&str, &str)]) -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    for (relative, content) in files {
        let path = dir.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

/// Every edge the framework resolvers made into a node named `name`, as the `file` of its target.
fn framework_picks(cg: &CodeGraph, name: &str) -> Vec<String> {
    let mut picks = Vec::new();
    for target in cg.get_nodes_by_name(name).unwrap() {
        for edge in cg.get_incoming_edges(&target.id).unwrap() {
            let resolved_by = edge.metadata.as_ref().and_then(|m| m.get("resolvedBy")).and_then(|v| v.as_str());
            if resolved_by == Some("framework") {
                picks.push(target.file_path.clone());
            }
        }
    }
    picks.sort();
    picks
}

const REACT: &str = r#"{"name":"app","dependencies":{"react":"^18.0.0"}}"#;
const SVELTE: &str = r#"{"name":"app","devDependencies":{"svelte":"^4.0.0"}}"#;
const ASTRO: &str = r#"{"name":"app","dependencies":{"astro":"^4.0.0"}}"#;
const VUE: &str = r#"{"name":"app","dependencies":{"vue":"^3.0.0"}}"#;
const REACT_WIDGET: &str = "export function Widget(props: {}) { return null; }\n";

#[test]
fn react_a_sibling_folder_that_only_starts_with_the_same_name_is_not_the_folder() {
    let (_dir, cg) = index(&[
        ("package.json", REACT),
        ("src/ui/Card.tsx", "export function Card() {\n  return Widget({});\n}\n"),
        ("src/ui-kit/Widget.tsx", REACT_WIDGET),
        ("lib/Widget.tsx", REACT_WIDGET),
    ]);
    // Two definitions, neither in `src/ui/`: the framework has nothing to choose by.
    assert_eq!(framework_picks(&cg, "Widget"), Vec::<String>::new());
}

#[test]
fn react_the_folder_itself_and_a_folder_below_it_still_come_first() {
    let (_dir, cg) = index(&[
        ("package.json", REACT),
        ("src/ui/Card.tsx", "export function Card() {\n  return Widget({});\n}\n"),
        ("src/ui/Banner.tsx", "export function Banner() {\n  return Field({});\n}\n"),
        ("src/ui-kit/Widget.tsx", REACT_WIDGET),
        ("src/ui/Widget.tsx", REACT_WIDGET),
        ("src/ui/forms/Field.tsx", "export function Field(props: {}) { return null; }\n"),
        ("lib/Field.tsx", "export function Field(props: {}) { return null; }\n"),
    ]);
    assert_eq!(framework_picks(&cg, "Widget"), ["src/ui/Widget.tsx"]);
    assert_eq!(framework_picks(&cg, "Field"), ["src/ui/forms/Field.tsx"]);
}

#[test]
fn react_a_file_at_the_project_root_does_not_take_whichever_definition_comes_first() {
    let (_dir, cg) = index(&[
        ("package.json", REACT),
        ("App.tsx", "export function App() {\n  return Panel({});\n}\n"),
        ("src/a/Panel.tsx", "export function Panel(props: {}) { return null; }\n"),
        ("src/b/Panel.tsx", "export function Panel(props: {}) { return null; }\n"),
    ]);
    assert_eq!(framework_picks(&cg, "Panel"), Vec::<String>::new());
}

#[test]
fn svelte_a_sibling_folder_that_only_starts_with_the_same_name_is_not_the_folder() {
    let (_dir, cg) = index(&[
        ("package.json", SVELTE),
        ("src/ui/mount.ts", "export function mount() {\n  return Widget({});\n}\n"),
        ("src/ui-kit/Widget.svelte", "<span>kit</span>\n"),
        ("lib/Widget.svelte", "<span>lib</span>\n"),
    ]);
    assert_eq!(framework_picks(&cg, "Widget"), Vec::<String>::new());
}

#[test]
fn astro_a_sibling_folder_that_only_starts_with_the_same_name_is_not_the_folder() {
    let (_dir, cg) = index(&[
        ("package.json", ASTRO),
        ("src/ui/Card.astro", "---\n---\n<div>\n  <Widget />\n</div>\n"),
        ("src/ui-kit/Widget.astro", "---\n---\n<span>kit</span>\n"),
        ("lib/Widget.astro", "---\n---\n<span>lib</span>\n"),
    ]);
    assert_eq!(framework_picks(&cg, "Widget"), Vec::<String>::new());
}

#[test]
fn vue_a_sibling_folder_that_only_starts_with_the_same_name_is_not_the_folder() {
    let (_dir, cg) = index(&[
        ("package.json", VUE),
        ("src/ui/mount.ts", "export function mount() {\n  return Widget({});\n}\n"),
        ("src/ui-kit/Widget.vue", "<template><span>kit</span></template>\n"),
        ("lib/Widget.vue", "<template><span>lib</span></template>\n"),
    ]);
    assert_eq!(framework_picks(&cg, "Widget"), Vec::<String>::new());
}
