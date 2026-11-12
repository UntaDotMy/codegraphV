//! An index built by an older extraction engine (upstream 737ee961, #1852): the first successful
//! tool response for that project in an MCP session opens with a one-time warning naming the
//! version that built it, the running engine and `codegraphV index`, as the user's decision.
use codegraph_core::CodeGraph;
use codegraph_mcp::tools::ToolHandler;
use serde_json::json;
use std::path::Path;

const PREFIX: &str = "⚠ This project's CodeGraphV index predates the running extraction engine.";

fn indexed(name: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.js"), format!("function {name}() {{ return 1; }}\n")).unwrap();
    CodeGraph::init_and_index(&dir.path().to_string_lossy(), None).unwrap().close();
    dir
}

fn stamp(root: &Path, version: &str) {
    let cg = CodeGraph::open(&root.to_string_lossy(), Default::default()).unwrap();
    cg.store().set_metadata("indexed_with_extraction_version", version).unwrap();
    cg.store().set_metadata("indexed_with_version", "0.0.1").unwrap();
    cg.close();
}

fn handler(root: &Path) -> ToolHandler {
    let mut handler = ToolHandler::new(Some(root.to_string_lossy().into_owned()), false);
    handler.catch_up = false;
    handler
}

fn search(handler: &mut ToolHandler, query: &str, project: Option<&Path>) -> String {
    let mut args = json!({ "query": query });
    if let Some(project) = project {
        args["projectPath"] = json!(project.to_string_lossy());
    }
    let result = handler.call("codegraphV_search", &args);
    assert!(!result.is_error);
    result.content[0]["text"].as_str().unwrap().to_string()
}

#[test]
fn the_first_response_on_a_stale_index_warns_and_the_second_does_not() {
    let dir = indexed("alpha");
    stamp(dir.path(), "1");
    let mut handler = handler(dir.path());
    let first = search(&mut handler, "alpha", None);
    assert!(first.starts_with(PREFIX), "{first}");
    assert!(first.contains("v0.0.1 (extraction 1)"), "{first}");
    assert!(first.contains(&format!("(extraction {})", codegraph_core::codegraph::EXTRACTION_VERSION)), "{first}");
    assert!(first.contains("`codegraphV index`") && first.contains("user's decision"), "{first}");
    assert!(first.contains("alpha"), "the answer still follows: {first}");
    let second = search(&mut handler, "alpha", None);
    assert!(!second.contains(PREFIX), "{second}");
}

#[test]
fn each_project_warns_once_and_a_new_session_warns_again() {
    let one = indexed("alpha");
    let two = indexed("beta");
    stamp(one.path(), "1");
    stamp(two.path(), "1");
    let mut handler = handler(one.path());
    assert!(search(&mut handler, "alpha", None).starts_with(PREFIX));
    assert!(search(&mut handler, "beta", Some(two.path())).starts_with(PREFIX));
    assert!(!search(&mut handler, "beta", Some(two.path())).contains(PREFIX));
    assert!(!search(&mut handler, "alpha", Some(one.path())).contains(PREFIX));
    drop(handler);
    let mut fresh = self::handler(one.path());
    assert!(search(&mut fresh, "alpha", None).starts_with(PREFIX));
}

#[test]
fn a_current_index_never_warns() {
    let dir = indexed("alpha");
    let mut handler = handler(dir.path());
    assert!(!search(&mut handler, "alpha", None).contains(PREFIX));
}

#[test]
fn the_one_shot_cli_handler_does_not_warn() {
    let dir = indexed("alpha");
    stamp(dir.path(), "1");
    let mut handler = handler(dir.path());
    handler.read_only = true;
    assert!(!search(&mut handler, "alpha", None).contains(PREFIX));
}
