//! Upstream #3093: a `workspaces` entry that leaves the project (`../sibling`) names no member.
//! It made an outside folder a workspace package, and an import of that package linked the call
//! to an unrelated same-named function inside the project.
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
        out.push(format!("{}:{}", target.file_path.replace('\\', "/"), target.qualified_name));
    }
    out.sort();
    out
}

/// The indexed project is `<tmp>/app`; `left-pad2` lives in `<tmp>/sibling` (or in the project).
fn index_app(workspaces: &str, extra: &[(&str, &str)]) -> (tempfile::TempDir, CodeGraph) {
    let tmp = tempfile::tempdir().unwrap();
    let package = format!(r#"{{"name":"app"{workspaces},"dependencies":{{"left-pad2":"^1.0.0"}}}}"#);
    let mut files = vec![
        ("app/package.json", package.as_str()),
        ("app/src/util.ts", "export function pad(s) { return s; }\n"),
        ("app/src/main.ts", "import { pad } from 'left-pad2';\nexport function run() { return pad('x'); }\n"),
        ("sibling/package.json", r#"{"name":"left-pad2","main":"index.ts"}"#),
        ("sibling/index.ts", "export function pad(s) { return s; }\n"),
    ];
    files.extend_from_slice(extra);
    write_files(tmp.path(), &files);
    let cg = CodeGraph::init_and_index(tmp.path().join("app").to_str().unwrap(), None).unwrap();
    (tmp, cg)
}

#[test]
fn a_workspace_entry_outside_the_project_links_nothing() {
    let (_tmp, control) = index_app("", &[]);
    assert_eq!(calls_of(&control, "run"), Vec::<String>::new());
    for outside in [r#","workspaces":["../sibling"]"#, r#","workspaces":["./../sibling"]"#, r#","workspaces":{"packages":["../*"]}"#] {
        let (_tmp, cg) = index_app(outside, &[]);
        assert_eq!(calls_of(&cg, "run"), Vec::<String>::new(), "{outside}");
    }
}

#[test]
fn an_in_project_workspace_still_links_its_package() {
    let (_tmp, cg) = index_app(
        r#","workspaces":["packages/*"]"#,
        &[
            ("app/packages/lp/package.json", r#"{"name":"left-pad2","main":"index.ts"}"#),
            ("app/packages/lp/index.ts", "export function pad(s) { return s; }\n"),
        ],
    );
    assert_eq!(calls_of(&cg, "run"), vec!["packages/lp/index.ts:pad".to_string()]);
}
