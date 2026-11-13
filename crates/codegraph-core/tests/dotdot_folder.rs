//! Upstream a38a1ec0: a folder whose name starts with two dots (`..generated/`) is inside the
//! project. Only a `..` segment or an absolute path leaves the root. Ported from
//! `__tests__/dotdot-folder-names.test.ts`.
use codegraph_core::CodeGraph;
use codegraph_resolve::path_aliases::{alias_and_base_url_candidates, apply_aliases, load_project_aliases};
use serde_json::json;
use std::fs;

fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (rel, content) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    dir
}

#[test]
fn aliases_into_a_dotdot_folder_link_calls_but_outside_targets_are_dropped() {
    let dir = project(&[
        (
            "tsconfig.json",
            &json!({"compilerOptions":{"baseUrl":".","paths":{"@generated/*":["..generated/*","../outside/*"]}}}).to_string(),
        ),
        ("..generated/client.ts", "export function makeClient(): number { return 1; }\n"),
        ("src/app.ts", "import { makeClient as mk } from '@generated/client';\nexport function run(): number { return mk(); }\n"),
    ]);
    let root = dir.path().to_str().unwrap();
    let aliases = load_project_aliases(root).unwrap();
    assert_eq!(apply_aliases("@generated/client", &aliases, root), ["..generated/client"]);

    let cg = CodeGraph::init_and_index(root, None).unwrap();
    let run = cg.get_nodes_by_name("run").unwrap().into_iter().next().unwrap();
    let callees: Vec<String> = cg
        .get_callees(&run.id, 1)
        .unwrap()
        .into_iter()
        .map(|r| format!("{}@{}", r.node.name, r.node.file_path))
        .collect();
    assert_eq!(callees, ["makeClient@..generated/client.ts"]);
    cg.close();
}

#[test]
fn a_base_url_in_a_dotdot_folder_is_inside_the_project() {
    let dir = project(&[("jsconfig.json", &json!({"compilerOptions":{"baseUrl":"..generated"}}).to_string())]);
    let root = dir.path().to_str().unwrap();
    let aliases = load_project_aliases(root).unwrap();
    assert_eq!(alias_and_base_url_candidates("client", &aliases, root), ["..generated/client"]);
}
