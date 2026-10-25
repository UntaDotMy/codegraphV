use serde_json::{json, Value};
use std::{
    io::Write,
    process::{Command, Stdio},
};
fn binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codegraphV"));
    command.env_remove("CODEGRAPHV_MCP_TOOLS");
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
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"codegraphV_explore","arguments":{"query":"start finish"}}}),
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
    assert!(!dir.path().join(".codegraphV").exists());
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
    assert_eq!(config["mcpServers"]["codegraphV"]["args"][1], "--mcp");
    assert!(!dir.path().join(".cursor").exists());
}

/// BUG-99: the one-shot CLI tool commands answer from the index as it is (like the TypeScript
/// CLI) instead of reconciling the whole project first; the MCP server still catches up on open.
#[test]
fn cli_explore_reads_the_index_as_is_and_the_server_catches_up() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let sandboxed = |args: &[&str]| {
        let mut c = binary();
        c.args(args).current_dir(dir.path());
        for var in ["HOME", "USERPROFILE", "LOCALAPPDATA", "APPDATA", "XDG_CONFIG_HOME"] {
            c.env(var, home.path());
        }
        c.env("CODEGRAPHV_NO_UPDATE_CHECK", "1");
        c
    };
    let ok = |c: &mut Command| {
        let out = c.output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    std::fs::write(dir.path().join("app.ts"), "export function start(){return 1;}\n").unwrap();
    ok(sandboxed(&["init", "--yes"]).stdout(Stdio::piped()));
    std::fs::write(dir.path().join("later.ts"), "export function later(){return 2;}\n").unwrap();

    ok(&mut sandboxed(&["explore", "later start"]));
    let query = ok(&mut sandboxed(&["query", "later", "--json"]));
    assert!(!query.contains("later.ts"), "CLI explore synced the project: {query}");

    let mut child = sandboxed(&["serve", "--mcp", "--no-watch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        for request in [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"codegraphV_explore","arguments":{"query":"later"}}}),
        ] {
            writeln!(stdin, "{request}").unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let query = ok(&mut sandboxed(&["query", "later", "--json"]));
    assert!(query.contains("later.ts"), "the MCP server did not catch up on open: {query}");
}
