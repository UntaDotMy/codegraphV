//! A JS file that gives an object a member itself, `Sha256.prototype.update = function…`,
//! holds what the index has no node for (upstream 5cd8a9a8). The one method of that name
//! elsewhere is no likelier then.
use codegraph_core::CodeGraph;
use codegraph_types::EdgeKind;
use std::fs;

fn calls_in(cg: &CodeGraph, file: &str) -> Vec<String> {
    let mut out = Vec::new();
    for node in cg.get_nodes_by_file(file).unwrap() {
        for edge in cg.get_outgoing_edges(&node.id).unwrap() {
            if edge.kind == EdgeKind::Calls {
                if let Some(target) = cg.get_node(&edge.target).unwrap() {
                    out.push(target.qualified_name);
                }
            }
        }
    }
    out
}

#[test]
fn a_lone_method_name_is_no_guess_in_a_file_that_assigns_it_itself() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("guides")).unwrap();
    fs::create_dir_all(dir.path().join("app")).unwrap();
    fs::write(
        dir.path().join("guides/turbo.js"),
        "export class History {\n  update(method, location) {\n    return method;\n  }\n}\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("app/bundle.js"),
        "(function () {\n  function Sha256() {}\n  Sha256.prototype.update = function (message) {\n    return this;\n  };\n  window.sha = Sha256;\n})();\n\nexport const sha256Algorithm = {\n  append: (buffer, data) => buffer.update(data),\n};\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("app/other.js"),
        "export const plain = {\n  append: (buffer, data) => buffer.update(data),\n};\n",
    )
    .unwrap();
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();

    assert!(!calls_in(&cg, "app/bundle.js").iter().any(|q| q == "History::update"));
    assert!(calls_in(&cg, "app/other.js").iter().any(|q| q == "History::update"));
    cg.close();
}
