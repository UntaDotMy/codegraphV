//! `codegraphV guard-hook`: Claude Code's `PreToolUse` guard. In an indexed project a search that
//! names indexed symbols, or a file listing over indexed files, is denied until the agent asked
//! codegraphV this session; everything else passes, and every failure path allows (prints
//! nothing, exits 0). The switch `CODEGRAPHV_GUARD=0` turns it off.
use serde_json::{json, Value};
use std::{
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};

fn run(cwd: &Path, stdin: &str, env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codegraphV"));
    command
        .arg("guard-hook")
        .current_dir(cwd)
        .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
        .env_remove("CODEGRAPHV_GUARD")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        command.env(k, v);
    }
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

/// None when the call is allowed, the deny reason otherwise.
fn decide(session: &str, cwd: &Path, tool: &str, input: Value) -> Option<String> {
    let event = json!({"session_id": session, "cwd": cwd, "hook_event_name": "PreToolUse", "tool_name": tool, "tool_input": input});
    let out = run(cwd, &event.to_string(), &[]);
    assert!(out.status.success(), "{tool} {input}: exit {:?}", out.status.code());
    if out.stdout.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    Some(v["hookSpecificOutput"]["permissionDecisionReason"].as_str().unwrap().to_owned())
}

fn indexed_project() -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project.path().join("src")).unwrap();
    std::fs::write(
        project.path().join("src/auth.ts"),
        "export function parseToken(raw: string) { return raw; }\nexport function login(u: string) { return parseToken(u); }\n",
    )
    .unwrap();
    let init = Command::new(env!("CARGO_BIN_EXE_codegraphV"))
        .args(["init", "."])
        .current_dir(project.path())
        .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));
    project
}

fn session(tag: &str) -> String {
    format!("guard-test-{tag}-{}-{:?}", std::process::id(), std::time::SystemTime::now())
}

fn bash(command: &str) -> Value {
    json!({ "command": command })
}

#[test]
fn searches_naming_indexed_symbols_are_denied_until_codegraphv_was_asked() {
    let project = indexed_project();
    let p = project.path();
    let s = session("deny");
    let denied = [
        ("Grep", json!({"pattern": "parseToken", "path": p})),
        ("Grep", json!({"pattern": "parseToken"})),
        ("Bash", bash("rg parseToken src")),
        ("Bash", bash("cd src && grep -n parseToken auth.ts")),
        ("PowerShell", bash("Select-String -Path src/*.ts -Pattern parseToken")),
        // A searcher after a pipe that names its own files reads them, not the pipe.
        ("Bash", bash("true | rg parseToken src")),
        // Command substitution and backticks run their commands too.
        ("Bash", bash("echo $(rg parseToken src)")),
        ("Bash", bash("echo `grep -rn parseToken .`")),
        // Mentioning a lookup is not making one.
        ("Bash", bash("echo codegraphV explore parseToken && rg parseToken src")),
        ("Glob", json!({"pattern": "**/*.ts", "path": p})),
        ("Bash", bash("find . -name '*.ts'")),
        ("Bash", bash("git ls-files")),
    ];
    for (tool, input) in &denied {
        let reason = decide(&s, p, tool, input.clone());
        assert!(reason.as_deref().is_some_and(|r| r.contains("codegraphV_explore")), "{tool} {input} should be denied: {reason:?}");
    }
    let allowed = [
        // Plain text, a pipe filter, a non-code word, a file read, and commands that search nothing.
        ("Grep", json!({"pattern": "failed to open", "path": p})),
        ("Bash", bash("git log --oneline | grep parseToken")),
        ("Bash", bash("cargo build --release")),
        ("Bash", bash("ls")),
        ("Read", json!({"file_path": p.join("src/auth.ts")})),
        ("Edit", json!({"file_path": p.join("src/auth.ts"), "old_string": "raw", "new_string": "text"})),
    ];
    for (tool, input) in &allowed {
        assert_eq!(decide(&s, p, tool, input.clone()), None, "{tool} {input} should pass");
    }
}

#[test]
fn a_codegraphv_lookup_unlocks_its_names_and_file_finding() {
    let project = indexed_project();
    let p = project.path();
    // Through the MCP tool.
    let s = session("mcp");
    assert!(decide(&s, p, "Grep", json!({"pattern": "parseToken"})).is_some());
    assert_eq!(decide(&s, p, "mcp__codegraphV__codegraphV_explore", json!({"query": "parseToken login"})), None);
    assert_eq!(decide(&s, p, "Grep", json!({"pattern": "parseToken"})), None);
    assert_eq!(decide(&s, p, "Bash", bash("rg -n 'login\\(' src")), None);
    assert_eq!(decide(&s, p, "Glob", json!({"pattern": "**/*.ts"})), None);
    // Through the CLI, as a command of its own.
    let s = session("cli");
    assert_eq!(decide(&s, p, "Bash", bash("codegraphV explore parseToken")), None);
    assert_eq!(decide(&s, p, "Bash", bash("rg parseToken src")), None);
    // Unlocked names do not unlock others.
    assert!(decide(&s, p, "Bash", bash("rg login src")).is_some());
}

#[test]
fn unindexed_folders_the_switch_and_bad_payloads_all_pass() {
    let plain = tempfile::tempdir().unwrap();
    std::fs::write(plain.path().join("a.ts"), "export function parseToken() {}\n").unwrap();
    let s = session("plain");
    assert_eq!(decide(&s, plain.path(), "Bash", bash("rg parseToken .")), None);
    assert_eq!(decide(&s, plain.path(), "Glob", json!({"pattern": "**/*.ts"})), None);

    let project = indexed_project();
    let p = project.path();
    let event = json!({"session_id": session("off"), "cwd": p, "tool_name": "Grep", "tool_input": {"pattern": "parseToken"}});
    let off = run(p, &event.to_string(), &[("CODEGRAPHV_GUARD", "0")]);
    assert!(off.status.success() && off.stdout.is_empty());
    for bad in ["", "not json {", "[]", "{\"tool_name\": 5}"] {
        let out = run(p, bad, &[]);
        assert!(out.status.success() && out.stdout.is_empty(), "{bad:?}");
    }
    // A corrupt index is an engine error: allow.
    let broken = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(broken.path().join(".codegraphV")).unwrap();
    std::fs::write(broken.path().join(".codegraphV/codegraph.db"), "not a database").unwrap();
    assert_eq!(decide(&session("broken"), broken.path(), "Bash", bash("rg parseToken .")), None);
}
