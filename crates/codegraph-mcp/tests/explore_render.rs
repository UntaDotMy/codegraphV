use codegraph_mcp::render::explore;
use serde_json::json;
use std::collections::HashMap;

fn explore_text(root: &std::path::Path, query: &str) -> String {
    let mut cg = codegraph_core::CodeGraph::init(root.to_str().unwrap()).unwrap();
    cg.index_all(&Default::default(), None, None).unwrap();
    let result = explore(&cg, &json!({ "query": query }), &mut HashMap::new()).unwrap();
    cg.close();
    result.content[0]["text"].as_str().unwrap().to_string()
}

/// A named definition is shown even when a long body the graph relates to it (a caller) comes
/// first in its file: that body used to spend the file's budget in line order.
#[test]
fn named_definitions_come_before_unrelated_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let filler: String = (0..400).map(|i| format!("    let v{i} = {i};\n")).collect();
    let source = format!(
        "pub fn unrelated_long() -> u32 {{\n{filler}    helper_alpha()\n}}\n\npub fn helper_alpha() -> u32 {{\n    1\n}}\n\npub fn helper_beta() -> u32 {{\n    helper_alpha() + 1\n}}\n"
    );
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), source).unwrap();

    let text = explore_text(dir.path(), "helper_alpha helper_beta");
    assert!(text.contains("pub fn helper_alpha() -> u32 {"), "{text}");
    assert!(text.contains("pub fn helper_beta() -> u32 {"), "{text}");
    assert!(!text.contains("let v200 = 200;"), "unrelated body printed:\n{text}");
}
