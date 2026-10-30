//! A `paths` wildcard whose prefix and suffix overlap (upstream 77ee3484).
//!
//! `"@x/*/index"` splits into the prefix `@x/` and the suffix `/index`. An import of `@x/index`
//! starts with the one and ends with the other, but only by counting its single `/` twice: nothing
//! is left for `*`. tsc's `isPatternMatch` asks for a specifier at least as long as the prefix and
//! the suffix together, so it skips the pattern there. The resolver took the empty capture instead
//! and linked the import, and every call through it, to `widgets/index`, a file it never named.
use codegraph_core::CodeGraph;
use codegraph_resolve::path_aliases::{apply_aliases, load_project_aliases};
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (relative, content) in files {
        let path = dir.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    dir
}

fn tsconfig(paths: &[(&str, &str)]) -> String {
    let entries: Vec<String> = paths.iter().map(|(key, target)| format!("\"{key}\": [\"{target}\"]")).collect();
    format!("{{ \"compilerOptions\": {{ \"baseUrl\": \"./src\", \"paths\": {{ {} }} }} }}", entries.join(", "))
}

fn root(dir: &tempfile::TempDir) -> &str {
    dir.path().to_str().unwrap()
}

#[test]
fn a_specifier_that_leaves_nothing_for_the_wildcard_does_not_match() {
    let dir = project(&[("tsconfig.json", &tsconfig(&[("@x/*/index", "widgets/*/index")]))]);
    let aliases = load_project_aliases(root(&dir)).unwrap();
    assert_eq!(apply_aliases("@x/index", &aliases, root(&dir)), Vec::<String>::new());
    assert_eq!(apply_aliases("@x/button/index", &aliases, root(&dir)), ["src/widgets/button/index"]);
}

#[test]
fn a_skipped_pattern_falls_through_to_the_next_that_matches() {
    // Both prefixes are `@x/`, so the first-declared pattern is tried first, as in tsc. Skipping it
    // must not end the search.
    let dir = project(&[("tsconfig.json", &tsconfig(&[("@x/*/index", "widgets/*/index"), ("@x/*", "lib/*")]))]);
    let aliases = load_project_aliases(root(&dir)).unwrap();
    assert_eq!(apply_aliases("@x/index", &aliases, root(&dir)), ["src/lib/index"]);
    assert_eq!(apply_aliases("@x/button/index", &aliases, root(&dir)), ["src/widgets/button/index"]);
}

#[test]
fn an_empty_capture_is_accepted_at_exactly_prefix_plus_suffix_length() {
    // tsc's bound is `>=`: here the prefix and suffix meet without sharing a character, so `*`
    // matches the empty string.
    let dir = project(&[("tsconfig.json", &tsconfig(&[("@x/*/index", "widgets/*/index")]))]);
    let aliases = load_project_aliases(root(&dir)).unwrap();
    assert_eq!(apply_aliases("@x//index", &aliases, root(&dir)), ["src/widgets/index"]);
}

#[test]
fn neither_the_import_nor_its_calls_reach_the_file_the_empty_capture_named() {
    let dir = project(&[
        ("tsconfig.json", &tsconfig(&[("@x/*/index", "widgets/*/index")])),
        ("src/index.ts", "export function trap(): number { return 1; }\n"),
        ("src/widgets/index.ts", "export function trap(): number { return 0; }\n"),
        ("src/widgets/button/index.ts", "export function press(): number { return 2; }\n"),
        (
            "src/main.ts",
            "import { trap as caught } from '@x/index';\nimport { press } from '@x/button/index';\nexport function go(): number { return caught() + press(); }\n",
        ),
    ]);
    let cg = CodeGraph::init_and_index(root(&dir), None).unwrap();
    let function = |name: &str, file: &str| {
        cg.get_nodes_by_kind(NodeKind::Function).unwrap().into_iter().find(|n| n.name == name && n.file_path == file).unwrap_or_else(|| panic!("{name} in {file}"))
    };
    let go = function("go", "src/main.ts");

    // The overlapping import names no project file.
    let widgets_file = cg.get_nodes_by_file("src/widgets/index.ts").unwrap().into_iter().find(|n| n.kind == NodeKind::File).unwrap();
    let imports: Vec<_> = cg.get_incoming_edges(&widgets_file.id).unwrap().into_iter().filter(|e| e.kind == EdgeKind::Imports).collect();
    assert!(imports.is_empty(), "src/widgets/index.ts was imported through an empty capture: {imports:?}");
    let trap_callers = cg.get_callers(&function("trap", "src/widgets/index.ts").id, 1).unwrap();
    assert!(trap_callers.is_empty(), "{trap_callers:?}");

    // The pattern still serves a specifier with a segment for `*`.
    let callees: Vec<String> = cg.get_callees(&go.id, 1).unwrap().iter().map(|c| format!("{}@{}", c.node.name, c.node.file_path)).collect();
    assert_eq!(callees, ["press@src/widgets/button/index.ts"]);
}
