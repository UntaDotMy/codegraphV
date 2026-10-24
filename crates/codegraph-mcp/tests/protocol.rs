use codegraph_mcp::{render::explore_budget, session::Session, ToolHandler};
use serde_json::{json, Value};
fn request(session: &mut Session, id: u32, method: &str, params: Value) -> Value {
    session
        .process(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string())
        .into_iter()
        .next()
        .unwrap()
}
#[test]
fn protocol_handshake_roots_and_recoverable_misses() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::new(Some(dir.path().to_string_lossy().into_owned()), false);
    assert_eq!(session.process("bad json")[0]["error"]["code"], -32700);
    assert_eq!(
        request(&mut session, 1, "initialize", json!({}))["result"]["protocolVersion"],
        "2024-11-05"
    );
    let tools = request(&mut session, 2, "tools/list", json!({}));
    assert!(!tools["result"]["tools"].as_array().unwrap().is_empty());
    let missed = request(
        &mut session,
        3,
        "tools/call",
        json!({"name":"codegraph_explore","arguments":{"query":"start finish"}}),
    );
    assert_ne!(missed["result"]["isError"], true);
    assert!(missed["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("projectPath"));
    assert_eq!(
        request(&mut session, 4, "unknown", json!({}))["error"]["code"],
        -32601
    );
    assert_eq!(
        request(&mut session, 5, "tools/call", json!({"name":"unknown"}))["error"]["code"],
        -32602
    );
    assert!(session
        .process(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
        .is_empty());
    assert_eq!(
        request(&mut session, 6, "ping", json!({}))["result"],
        json!({})
    );
}
#[test]
fn roots_response_adopts_index_without_restarting_session() {
    let dir = tempfile::tempdir().unwrap();
    let cg = codegraph_core::CodeGraph::init(dir.path().to_str().unwrap()).unwrap();
    cg.close();
    let mut session = Session::new(None, false);
    let messages=session.process(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{"roots":{}}}}).to_string());
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1]["method"], "roots/list");
    let uri = url::Url::from_directory_path(dir.path()).unwrap();
    session.process(
        &json!({"jsonrpc":"2.0","id":"codegraph-roots","result":{"roots":[{"uri":uri.as_str()}]}})
            .to_string(),
    );
    assert!(session.handler.default_root().is_some());
    assert!(
        request(&mut session, 2, "tools/list", json!({}))["result"]["tools"][0]["inputSchema"]
            ["required"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v != "projectPath")
    );
}
#[test]
fn source_node_flow_and_read_windows_are_native() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.ts"),
        "export function finish(){ return 42; }\nexport function start(){ return finish(); }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("other.ts"),
        "export function finish(value: string){ return value; }\n",
    )
    .unwrap();
    let cg = codegraph_core::CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    cg.close();
    let mut handler = ToolHandler::new(Some(dir.path().to_string_lossy().into_owned()), false);
    let node = handler.call(
        "codegraph_node",
        &json!({"symbol":"finish","includeCode":true}),
    );
    assert!(!node.is_error);
    assert!(
        node.message().contains("2 definitions"),
        "{}",
        node.message()
    );
    assert!(node.message().contains("return 42"));
    assert!(node.message().contains("return value"));
    let explore = handler.call("codegraph_explore", &json!({"query":"start finish"}));
    assert!(!explore.is_error);
    assert!(
        explore.message().contains("Flow among named symbols"),
        "{}",
        explore.message()
    );
    assert!(explore.message().contains("already Read"));
    let file = handler.call("codegraph_node", &json!({"file":"main.ts:2-2"}));
    assert!(file.message().contains("2\texport function start"));
    assert!(!file.message().contains("1\texport function finish"));
    let missing = handler.call("codegraph_node", &json!({"symbol":"missing"}));
    assert!(!missing.is_error);
    assert!(missing.message().contains("No symbol found"));
}
#[test]
fn output_budgets_never_shrink() {
    let mut previous = explore_budget(0);
    for files in [
        149, 150, 499, 500, 4999, 5000, 14999, 15000, 24999, 25000, 100000,
    ] {
        let next = explore_budget(files);
        assert!(next.calls >= previous.calls);
        assert!(next.chars >= previous.chars);
        assert!(next.per_file >= previous.per_file);
        previous = next;
    }
}
#[test]
fn missing_project_is_guidance() {
    let mut handler = ToolHandler::new(Some("does-not-exist/codegraph-project".into()), false);
    assert!(
        !handler
            .call("codegraph_explore", &json!({"query":"hello"}))
            .is_error
    );
}
