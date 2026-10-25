//! The update notice reaches the MCP surfaces (DEV-63): a fresh `~/.codegraph/update-check-rust.json`
//! naming a newer release puts one sentence in the initialize instructions and in
//! codegraph_status; CODEGRAPH_NO_UPDATE_CHECK=1 removes it. The cache is fresh, so no test
//! here touches the network.
use serde_json::Value;
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

fn binary(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codegraph"));
    command
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("CODEGRAPH_DAEMON", "0")
        .env_remove("CODEGRAPH_NO_UPDATE_CHECK")
        .env_remove("DO_NOT_TRACK")
        .env_remove("CODEGRAPH_MCP_TOOLS");
    command
}

/// initialize + codegraph_status over stdio; returns (instructions, status text).
fn handshake(home: &Path, project: &Path, opt_out: bool) -> (String, String) {
    let mut command = binary(home);
    if opt_out {
        command.env("CODEGRAPH_NO_UPDATE_CHECK", "1");
    }
    let mut child = command
        .args(["serve", "--mcp", "--no-watch", "--path"])
        .arg(project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let requests = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"codegraph_status","arguments":{}}}"#,
        "\n"
    );
    child.stdin.take().unwrap().write_all(requests.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    let mut instructions = String::new();
    let mut status = String::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let msg: Value = serde_json::from_str(line).unwrap();
        match msg["id"].as_u64() {
            Some(1) => instructions = msg["result"]["instructions"].as_str().unwrap_or_default().to_string(),
            Some(2) => status = msg["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string(),
            _ => {}
        }
    }
    (instructions, status)
}

#[test]
fn fresh_cache_with_newer_release_reaches_initialize_and_status() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("app.ts"), "export function start() { return 1; }\n").unwrap();
    let init = binary(home.path()).args(["init", "."]).current_dir(project.path()).output().unwrap();
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as f64;
    std::fs::create_dir_all(home.path().join(".codegraph")).unwrap();
    std::fs::write(
        home.path().join(".codegraph/update-check-rust.json"),
        format!(r#"{{"lastAttemptAt":{now},"lastSuccessAt":{now},"latest":"v9.9.9"}}"#),
    )
    .unwrap();
    let notice = format!(
        "CodeGraph v9.9.9 is available (this server is running v{}). Update with `codegraph upgrade`.",
        env!("CARGO_PKG_VERSION")
    );

    let (instructions, status) = handshake(home.path(), project.path(), false);
    assert!(instructions.contains(&format!("---\n{notice} This server keeps running the old version")), "{instructions}");
    let status: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(status["updateAvailable"], notice.as_str());

    let (instructions, status) = handshake(home.path(), project.path(), true);
    assert!(!instructions.contains("is available"), "{instructions}");
    let status: Value = serde_json::from_str(&status).unwrap();
    assert!(status.get("updateAvailable").is_none());
}

/// BUG-71: the npm (TypeScript) codegraph keeps its own cache in `~/.codegraph/update-check.json`,
/// naming npm versions (v1.6.x). A machine switched to the Rust build still has it; it must not
/// turn into "v1.6.2 is available".
#[test]
fn the_typescript_versions_cache_is_not_read() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("app.ts"), "export function start() { return 1; }\n").unwrap();
    let init = binary(home.path()).args(["init", "."]).current_dir(project.path()).output().unwrap();
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as f64;
    std::fs::create_dir_all(home.path().join(".codegraph")).unwrap();
    let ts_cache = home.path().join(".codegraph/update-check.json");
    let ts_text = format!(r#"{{"lastAttemptAt":{now},"lastSuccessAt":{now},"latest":"v1.6.2"}}"#);
    std::fs::write(&ts_cache, &ts_text).unwrap();

    let (instructions, status) = handshake(home.path(), project.path(), false);
    assert!(!instructions.contains("v1.6.2"), "{instructions}");
    let status: Value = serde_json::from_str(&status).unwrap();
    assert!(!status["updateAvailable"].as_str().unwrap_or_default().contains("v1.6.2"), "{status}");
    assert_eq!(std::fs::read_to_string(&ts_cache).unwrap(), ts_text, "the TS cache is left as it was");
}
