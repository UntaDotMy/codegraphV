use serde_json::{json, Value};
use std::{
    io::Write,
    process::{Command, Stdio},
};
fn binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codegraph"));
    command.env_remove("CODEGRAPH_MCP_TOOLS");
    command
}
fn run(root: &std::path::Path, args: &[&str]) -> String {
    let result = binary().args(args).current_dir(root).output().unwrap();
    assert!(
        result.status.success(),
        "{args:?}: {}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}
#[test]
fn executable_indexes_queries_syncs_and_serves_jsonrpc_without_node() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("app.ts"),
        "export function finish(){return 42;}\nexport function start(){return finish();}\n",
    )
    .unwrap();
    let initial: Value = serde_json::from_str(&run(dir.path(), &["status", "--json"])).unwrap();
    assert_eq!(initial["initialized"], false);
    let index: Value = serde_json::from_str(&run(dir.path(), &["init", "--yes"])).unwrap();
    assert_eq!(index["success"], true);
    assert_eq!(index["filesIndexed"], 1);
    let query: Value =
        serde_json::from_str(&run(dir.path(), &["query", "finish", "--json"])).unwrap();
    assert_eq!(query[0]["node"]["name"], "finish");
    assert!(run(dir.path(), &["explore", "start finish"]).contains("Flow among named symbols"));
    let callers: Value =
        serde_json::from_str(&run(dir.path(), &["callers", "finish", "--json"])).unwrap();
    assert!(callers["callers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["name"] == "start"));
    assert!(run(
        dir.path(),
        &["node", "--file", "app.ts", "--offset", "2", "--limit", "1"]
    )
    .contains("2\texport function start"));
    std::fs::write(
        dir.path().join("next.ts"),
        "export function next(){return 7;}\n",
    )
    .unwrap();
    run(dir.path(), &["sync", "--quiet"]);
    assert!(run(dir.path(), &["query", "next", "--json"]).contains("next.ts"));
    let mut child = binary()
        .args(["serve", "--mcp", "--no-watch"])
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        for request in [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"codegraph_explore","arguments":{"query":"start finish"}}}),
        ] {
            writeln!(stdin, "{request}").unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let messages: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).expect("stdout must contain only JSON-RPC"))
        .collect();
    assert_eq!(messages.len(), 3);
    assert!(messages[2]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("start"));
    run(dir.path(), &["uninit", "--force"]);
    assert!(!dir.path().join(".codegraph").exists());
    assert!(dir.path().join("app.ts").exists());
}
#[test]
fn print_config_never_writes_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let text = run(
        dir.path(),
        &["install", "--print-config", "cursor", "--location", "local"],
    );
    let config: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(config["mcpServers"]["codegraph"]["args"][1], "--mcp");
    assert!(!dir.path().join(".cursor").exists());
}
