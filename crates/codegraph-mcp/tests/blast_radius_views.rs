//! codegraphV_explore lists the views that depend on a file in their own slot, so same-language
//! dependents can't crowd them out (upstream 97492056, which does this for the blast radius;
//! codegraphV's explore reports dependents on its "Used by" line).
use codegraph_core::{CodeGraph, IndexOptions};
use codegraph_mcp::render::explore;
use serde_json::json;
use std::collections::HashMap;
use std::fs;

fn write(root: &std::path::Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn project() -> (CodeGraph, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/helper.js", "export function fmt(x) { return x + 1; }\n");
    // More code dependents than the line lists: under one flat cap, no view would show.
    for n in ["a", "b", "c", "d", "e", "f", "g", "h", "i"] {
        write(dir.path(), &format!("src/{n}.js"), &format!("import {{ fmt }} from './helper';\nexport function use_{n}() {{ return fmt(1); }}\n"));
    }
    for n in ["Index", "Edit"] {
        write(dir.path(), &format!("Views/{n}.cshtml"), "<script>\nimport { fmt } from '../src/helper';\nfmt(2);\n</script>\n");
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (cg, dir)
}

fn text(cg: &CodeGraph, query: &str) -> String {
    explore(cg, &json!({ "query": query }), &mut HashMap::new()).unwrap().message()
}

#[test]
fn lists_views_in_their_own_slot_past_the_cap_on_code_dependents() {
    let (cg, _dir) = project();
    let out = text(&cg, "fmt");
    let views = codegraph_jsre::re!(r"Used by 11 files: src/[a-i]\.js(?:, src/[a-i]\.js){7}; 2 views: Views/(?:Edit|Index)\.cshtml, Views/(?:Edit|Index)\.cshtml\n");
    assert!(views.is_match(&out), "{out}");
    cg.close();
}

#[test]
fn reads_the_same_when_only_views_depend_on_a_file() {
    let (mut cg, dir) = project();
    write(dir.path(), "src/only.js", "export function onlyViews() { return 1; }\n");
    write(dir.path(), "Views/Solo.cshtml", "<script>\nimport { onlyViews } from '../src/only';\nonlyViews();\n</script>\n");
    cg.sync(&IndexOptions::default(), None).unwrap();
    let out = text(&cg, "onlyViews");
    assert!(out.contains("Used by 1 files: 1 view: Views/Solo.cshtml\n"), "{out}");
    cg.close();
}

#[test]
fn a_file_no_view_uses_reads_as_before() {
    let (cg, _dir) = project();
    let out = text(&cg, "use_a");
    assert!(!out.contains(" view"), "{out}");
    cg.close();
}
