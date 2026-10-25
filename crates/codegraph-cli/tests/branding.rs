//! DEV-83: the Rust build is codegraphV everywhere a user sees it, and never collides with the
//! original codegraph (its `.codegraph/` index, its `codegraph` MCP entry, its env vars).
use serde_json::{json, Value};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_codegraphV")
}

/// A command with a throwaway home and none of the user's settings.
fn cmd(home: &Path) -> Command {
    let mut c = Command::new(bin());
    c.env("HOME", home)
        .env("USERPROFILE", home)
        .env("LOCALAPPDATA", home)
        .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
        .env("CODEGRAPHV_DAEMON", "0")
        .env_remove("CODEGRAPHV_DIR")
        .env_remove("CODEGRAPH_DIR")
        .env_remove("CLAUDE_CONFIG_DIR");
    c
}

fn ok(out: std::process::Output) -> String {
    assert!(out.status.success(), "{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The original's user-visible names: tool names, the product name, its MCP permission.
fn old_names(text: &str) -> Vec<String> {
    let re = regex::Regex::new(r"(^|[^\w/@.-])codegraph_(explore|node|search|callers|callees|impact|files|status)\b|CodeGraph[^V]|CodeGraph$|mcp__codegraph__").unwrap();
    re.find_iter(text).map(|m| m.as_str().to_string()).collect()
}

#[test]
fn binary_name_and_version() {
    let name = Path::new(bin()).file_stem().unwrap().to_string_lossy().into_owned();
    assert_eq!(name, "codegraphV");
    let home = tempfile::tempdir().unwrap();
    let version = ok(cmd(home.path()).arg("--version").output().unwrap());
    assert_eq!(version.trim(), format!("codegraphV {}", env!("CARGO_PKG_VERSION")));
    let help = ok(cmd(home.path()).arg("--help").output().unwrap());
    assert!(old_names(&help).is_empty(), "{help}");
}

#[test]
fn index_lives_in_dot_codegraphv_and_the_originals_folder_is_never_indexed() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project.path().join("src")).unwrap();
    std::fs::write(project.path().join("src/app.ts"), "export function start() { return 1; }\n").unwrap();
    // The original codegraph's index folder, with a source-looking file in it.
    std::fs::create_dir_all(project.path().join(".codegraph")).unwrap();
    std::fs::write(project.path().join(".codegraph/stale.ts"), "export function stale() {}\n").unwrap();
    ok(cmd(home.path()).args(["init", "."]).current_dir(project.path()).output().unwrap());
    assert!(project.path().join(".codegraphV").join("codegraph.db").is_file());
    assert!(!project.path().join(".codegraph").join("codegraph.db").exists(), "the original's folder is left alone");
    let files = ok(cmd(home.path()).args(["files", "--json", "-p"]).arg(project.path()).output().unwrap());
    assert!(files.contains("src/app.ts"), "{files}");
    assert!(!files.contains("stale.ts"), "{files}");

    // CODEGRAPHV_DIR names the folder; the original's CODEGRAPH_DIR is not ours.
    let other = tempfile::tempdir().unwrap();
    std::fs::write(other.path().join("a.ts"), "export const a = 1;\n").unwrap();
    ok(cmd(home.path()).env("CODEGRAPHV_DIR", ".cgv-custom").env("CODEGRAPH_DIR", ".cg-original").args(["init", "."]).current_dir(other.path()).output().unwrap());
    assert!(other.path().join(".cgv-custom").join("codegraph.db").is_file());
    assert!(!other.path().join(".cg-original").exists());
}

#[test]
fn mcp_server_and_tools_are_codegraphv() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("app.ts"), "export function start() { return 1; }\n").unwrap();
    ok(cmd(home.path()).args(["init", "."]).current_dir(project.path()).output().unwrap());
    let mut child = cmd(home.path())
        .args(["serve", "--mcp", "--no-watch", "--path"])
        .arg(project.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let requests = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"codegraphV_explore","arguments":{"query":"start"}}}),
    ];
    let text: String = requests.iter().map(|r| format!("{r}\n")).collect();
    child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    let replies: Vec<Value> =
        String::from_utf8_lossy(&out.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let by_id = |id: u64| replies.iter().find(|r| r["id"] == id).cloned().unwrap();
    let init = by_id(1);
    assert_eq!(init["result"]["serverInfo"]["name"], "codegraphV");
    let instructions = init["result"]["instructions"].as_str().unwrap();
    assert!(old_names(instructions).is_empty(), "{:?}", old_names(instructions));
    let tools = by_id(2)["result"]["tools"].as_array().unwrap().clone();
    assert!(!tools.is_empty());
    for tool in &tools {
        let name = tool["name"].as_str().unwrap();
        assert!(name.starts_with("codegraphV_"), "{name}");
        let described = tool.to_string();
        assert!(old_names(&described).is_empty(), "{name}: {:?}", old_names(&described));
    }
    let explore = by_id(3);
    assert_ne!(explore["result"]["isError"], true, "{explore}");
    let body = explore["result"]["content"][0]["text"].as_str().unwrap();
    assert!(body.contains("start"), "{body}");
    assert!(old_names(body).is_empty(), "{:?}", old_names(body));
}

#[test]
fn install_writes_codegraphv_and_leaves_the_originals_entries() {
    let home = tempfile::tempdir().unwrap();
    let claude_json = home.path().join(".claude.json");
    let settings = home.path().join(".claude").join("settings.json");
    let claude_md = home.path().join(".claude").join("CLAUDE.md");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    // What the original codegraph wrote.
    std::fs::write(&claude_json, r#"{"mcpServers":{"codegraph":{"type":"stdio","command":"codegraph","args":["serve","--mcp"]}}}"#).unwrap();
    std::fs::write(&settings, r#"{"permissions":{"allow":["mcp__codegraph__*"]},"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"codegraph prompt-hook"}]}]}}"#).unwrap();
    std::fs::write(&claude_md, "# mine\n\n<!-- CODEGRAPH_START -->\n## CodeGraph\noriginal block\n<!-- CODEGRAPH_END -->\n").unwrap();

    ok(cmd(home.path()).args(["install", "--yes", "--target", "claude"]).output().unwrap());
    let cj: Value = serde_json::from_str(&std::fs::read_to_string(&claude_json).unwrap()).unwrap();
    assert_eq!(cj["mcpServers"]["codegraph"]["command"], "codegraph", "original entry untouched");
    assert_eq!(cj["mcpServers"]["codegraphV"]["args"], json!(["serve", "--mcp"]));
    let st: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    let allow: Vec<&str> = st["permissions"]["allow"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(allow.contains(&"mcp__codegraph__*") && allow.contains(&"mcp__codegraphV__*"), "{allow:?}");
    let hooks: Vec<&str> = st["hooks"]["UserPromptSubmit"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|g| g["hooks"][0]["command"].as_str())
        .collect();
    let ours = format!("\"{}\" prompt-hook", env!("CARGO_BIN_EXE_codegraphV").replace('\\', "/"));
    assert_eq!(hooks, vec!["codegraph prompt-hook", ours.as_str()]);
    let md = std::fs::read_to_string(&claude_md).unwrap();
    assert!(md.contains("<!-- CODEGRAPH_START -->\n## CodeGraph\noriginal block"), "{md}");
    assert!(md.contains("<!-- CODEGRAPHV_START -->") && md.contains("<!-- CODEGRAPHV_END -->"), "{md}");

    ok(cmd(home.path()).args(["uninstall", "--yes", "--target", "claude", "--keep-cli"]).output().unwrap());
    let cj: Value = serde_json::from_str(&std::fs::read_to_string(&claude_json).unwrap()).unwrap();
    assert_eq!(cj["mcpServers"]["codegraph"]["command"], "codegraph");
    assert!(cj["mcpServers"].get("codegraphV").is_none());
    let st = std::fs::read_to_string(&settings).unwrap();
    assert!(st.contains("mcp__codegraph__*") && !st.contains("mcp__codegraphV__"), "{st}");
    assert!(st.contains("\"codegraph prompt-hook\"") && !st.contains("codegraphV"), "{st}");
    let md = std::fs::read_to_string(&claude_md).unwrap();
    assert!(md.contains("original block") && !md.contains("CODEGRAPHV_START"), "{md}");
}

/// BUG-91: like the original, every per-project file follows the CODEGRAPHV_DIR override.
#[test]
fn dir_override_holds_every_per_project_file() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.ts"), "export const a = 1;\n").unwrap();
    let with = |c: &mut Command| {
        c.env("CODEGRAPHV_DIR", ".cgv-custom").current_dir(project.path());
    };
    let mut init = cmd(home.path());
    with(&mut init);
    ok(init.args(["init", "."]).output().unwrap());
    assert!(project.path().join(".cgv-custom").join("codegraph.db").is_file());
    assert!(!project.path().join(".codegraphV").exists(), "nothing goes to the default folder");
    let mut status = cmd(home.path());
    with(&mut status);
    let status: Value = serde_json::from_str(&ok(status.args(["status", "--json"]).output().unwrap())).unwrap();
    assert!(status["indexPath"].as_str().unwrap().ends_with(".cgv-custom"), "{status}");
    let mut files = cmd(home.path());
    with(&mut files);
    assert!(ok(files.args(["files", "--json"]).output().unwrap()).contains("a.ts"));
}
