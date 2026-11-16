//! Upstream #1474 / #3260: a file edited after its last index sync has indexed line ranges that no
//! longer match its bytes. `explore` and `node` must never slice current bytes at those ranges
//! (that serves another symbol's code under the asked name): a small file is shown whole and
//! current, a big one is left out with a notice, and line references are flagged.
use codegraph_mcp::render::{explore, node};
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;

const SMALL: &str = "struct Model { name: String }\nfn plain_helper(m: &Model) -> String { m.name.clone() }\nfn direct(m: &Model) -> String { plain_helper(m) }\nfn main() { let m = Model { name: \"a\".into() }; direct(&m); }\n";

fn indexed(root: &Path, file: &str, text: &str) -> codegraph_core::CodeGraph {
    std::fs::write(root.join(file), text).unwrap();
    let mut cg = codegraph_core::CodeGraph::init(root.to_str().unwrap()).unwrap();
    cg.index_all(&Default::default(), None, None).unwrap();
    cg
}

fn text(result: codegraph_mcp::tools::ToolResult) -> String {
    result.content[0]["text"].as_str().unwrap().to_string()
}

fn big() -> String {
    let filler: String = (0..600).map(|i| format!("fn filler_{i}() -> u32 {{ {i} }}\n")).collect();
    format!("{filler}fn plain_helper() -> u32 {{ 7 }}\nfn direct() -> u32 {{ plain_helper() }}\n")
}

#[test]
fn an_unchanged_file_has_no_drift_notice() {
    let dir = tempfile::tempdir().unwrap();
    let cg = indexed(dir.path(), "main.rs", SMALL);
    let out = text(explore(&cg, &json!({"query": "plain_helper direct"}), &mut HashMap::new()).unwrap());
    assert!(!out.contains("⚠"), "{out}");
    let out = text(node(&cg, &json!({"symbol": "plain_helper", "includeCode": true})).unwrap());
    assert!(!out.contains("⚠"), "{out}");
}

#[test]
fn a_small_drifted_file_is_shown_whole_and_current() {
    let dir = tempfile::tempdir().unwrap();
    let cg = indexed(dir.path(), "main.rs", SMALL);
    std::fs::write(dir.path().join("main.rs"), format!("// a\n// b\n// c\n{SMALL}")).unwrap();

    let out = text(explore(&cg, &json!({"query": "plain_helper direct"}), &mut HashMap::new()).unwrap());
    assert!(out.contains("     5\tfn plain_helper(m: &Model)"), "{out}");
    assert!(out.contains("     6\tfn direct(m: &Model)"), "{out}");
    assert!(out.contains("⚠ Changed on disk after the last index sync: main.rs"), "{out}");

    let out = text(node(&cg, &json!({"symbol": "plain_helper", "includeCode": true})).unwrap());
    assert!(out.contains("the file has changed on disk since"), "{out}");
    assert!(out.contains("     5\tfn plain_helper(m: &Model)"), "{out}");
    assert!(out.contains("```rust\n     1\t// a"), "the whole current file, from line 1: {out}");
    assert!(!out.contains("```rust\n     2\t"), "a slice at the indexed range: {out}");
}

#[test]
fn a_big_drifted_file_is_left_out_with_a_notice_never_mis_sliced() {
    let dir = tempfile::tempdir().unwrap();
    let source = big();
    let cg = indexed(dir.path(), "main.rs", &source);
    std::fs::write(dir.path().join("main.rs"), format!("// a\n// b\n// c\n{source}")).unwrap();

    let out = text(explore(&cg, &json!({"query": "plain_helper direct"}), &mut HashMap::new()).unwrap());
    assert!(!out.contains("filler_597"), "a slice at the indexed range: {out}");
    assert!(out.contains("source omitted"), "{out}");
    assert!(out.contains("main.rs:1-"), "the notice names a range query for current source: {out}");
    assert!(!out.contains("Read this file"), "explore steers to another explore, never to Read: {out}");

    let out = text(node(&cg, &json!({"symbol": "plain_helper", "includeCode": true})).unwrap());
    assert!(!out.contains("filler_597"), "a slice at the indexed range: {out}");
    assert!(out.contains("body is omitted"), "{out}");
    assert!(out.contains("codegraphV_node"), "{out}");
}

#[test]
fn an_explicit_range_of_a_drifted_file_is_served_as_current_lines() {
    let dir = tempfile::tempdir().unwrap();
    let source = big();
    let cg = indexed(dir.path(), "main.rs", &source);
    std::fs::write(dir.path().join("main.rs"), format!("// a\n// b\n// c\n{source}")).unwrap();
    let out = text(explore(&cg, &json!({"query": "main.rs:603-605"}), &mut HashMap::new()).unwrap());
    assert!(out.contains("   604\tfn plain_helper() -> u32 { 7 }"), "{out}");
}

/// #3260: callers, callees and impact print indexed line numbers; a drifted file in the answer
/// is named so the reader knows they may be shifted.
#[test]
fn relations_and_impact_flag_a_drifted_file() {
    let dir = tempfile::tempdir().unwrap();
    let cg = indexed(dir.path(), "main.rs", SMALL);
    let before = codegraph_mcp::render::relations(&cg, &json!({"symbol": "plain_helper"}), true).unwrap();
    assert!(!before.contains("⚠"), "{before}");
    std::fs::write(dir.path().join("main.rs"), format!("// a\n// b\n// c\n{SMALL}")).unwrap();
    for out in [
        codegraph_mcp::render::relations(&cg, &json!({"symbol": "plain_helper"}), true).unwrap(),
        codegraph_mcp::render::relations(&cg, &json!({"symbol": "direct"}), false).unwrap(),
        codegraph_mcp::render::impact(&cg, &json!({"symbol": "plain_helper"})).unwrap(),
    ] {
        assert!(out.contains("⚠ Changed on disk after the last index sync: main.rs"), "{out}");
    }
}
