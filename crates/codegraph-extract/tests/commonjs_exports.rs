//! Upstream 8e854d11 (1675): `exports.name = …` / `exports["name"] = …` /
//! `module.exports.name = …` name the function they hold. Ported from
//! `__tests__/commonjs-exports.test.ts`.
use codegraph_extract::extract_source;
use codegraph_types::{Language, NodeKind};

#[test]
fn dotted_commonjs_exports_name_and_own_the_function() {
    let code = "\nconst { findItems, removeItem } = require('./db');\n\nexports.getItems = async (req, res) => {\n  res.json(await findItems());\n};\n\nmodule.exports.deleteItem = function (req, res) {\n  removeItem(req.params.id);\n  res.end();\n};\n\nexports.plain = 42;\nmodule.exports = { legacy: 1 };\n";
    let result = extract_source("src/controller.js", code, Language::JavaScript).unwrap();
    let names: Vec<_> = result.nodes.iter().filter(|n| n.kind == NodeKind::Function).map(|n| n.name.as_str()).collect();
    assert_eq!(names, ["getItems", "deleteItem"]);
    for node in result.nodes.iter().filter(|n| n.kind == NodeKind::Function) {
        assert_eq!(node.is_exported, Some(true), "{}", node.name);
        let refs: Vec<_> = result
            .unresolved_references
            .iter()
            .filter(|r| r.from_node_id == node.id)
            .map(|r| r.reference_name.as_str())
            .collect();
        let want = if node.name == "getItems" { "findItems" } else { "removeItem" };
        assert!(refs.contains(&want), "{} refs {refs:?}", node.name);
        let file = result.nodes.iter().find(|n| n.kind == NodeKind::File).unwrap();
        assert!(!result
            .unresolved_references
            .iter()
            .any(|r| r.from_node_id == file.id && r.reference_name == want), "file owns {want}");
    }
    assert!(!result.nodes.iter().any(|n| n.name == "<anonymous>"));
}

#[test]
fn quoted_commonjs_exports_name_the_function_and_leave_computed_ones_alone() {
    let code = "\nconst { findItems, removeItem } = require('./db');\n\nexports[\"getItems\"] = async (req, res) => {\n  res.json(await findItems());\n};\n\nmodule.exports['deleteItem'] = function (req, res) {\n  removeItem(req.params.id);\n  res.end();\n};\n\nexports[\"not-a-name\"] = () => { skipped(); };\nexports[key] = () => { computed(); };\nexports[`tpl`] = () => { template(); };\nhandlers[\"onSave\"] = () => { persist(); };\n";
    for (file, language) in [("src/controller.js", Language::JavaScript), ("src/controller.ts", Language::TypeScript)] {
        let result = extract_source(file, code, language).unwrap();
        let mut got: Vec<_> = result
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Function)
            .map(|n| format!("{}@{} exported={}", n.name, n.start_line, n.is_exported == Some(true)))
            .collect();
        got.sort();
        assert_eq!(got, ["deleteItem@8 exported=true", "getItems@4 exported=true"], "{file}");
        let file_node = result.nodes.iter().find(|n| n.kind == NodeKind::File).unwrap();
        for name in ["findItems", "removeItem"] {
            assert!(!result
                .unresolved_references
                .iter()
                .any(|r| r.from_node_id == file_node.id && r.reference_name == name), "{file}: {name}");
        }
    }
}
