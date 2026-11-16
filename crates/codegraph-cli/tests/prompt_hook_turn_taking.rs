//! A prompt that is only a turn-taking reply ("continue", "ok, go ahead") is not a request about
//! code: the hook injected unrelated symbols such as `ident_continue` for it, because the word
//! is a rare segment of indexed names. Such a prompt gets nothing; a request that merely contains
//! one of those words still does.
use std::{
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};

fn hook(cwd: &Path, prompt: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_codegraphV"))
        .arg("prompt-hook")
        .current_dir(cwd)
        .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
        .env("CODEGRAPHV_DAEMON", "0")
        .env_remove("CODEGRAPHV_NO_PROMPT_HOOK")
        .env_remove("CODEGRAPHV_PROMPT_HOOK")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let payload = serde_json::json!({ "prompt": prompt, "cwd": cwd }).to_string();
    child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
    let out: Output = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{prompt}: exit {:?}", out.status.code());
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn a_turn_taking_prompt_injects_nothing_and_a_request_still_does() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("lib.rs"),
        "pub fn ident_continue(c: char) -> bool { c.is_alphanumeric() }\n\
         pub fn continue_loop(n: u32) -> u32 { n + 1 }\n\
         pub fn refund_order(id: u32) -> u32 { id }\n\
         pub fn refund_total(id: u32) -> u32 { refund_order(id) }\n",
    )
    .unwrap();
    let init = Command::new(env!("CARGO_BIN_EXE_codegraphV"))
        .args(["init", "."])
        .current_dir(project.path())
        .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
        .env("CODEGRAPHV_DAEMON", "0")
        .output()
        .unwrap();
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));

    for prompt in ["continue", "Continue.", "ok, continue please", "yes go ahead", "  proceed!\n", "keep going"] {
        let out = hook(project.path(), prompt);
        assert!(out.is_empty(), "{prompt:?} injected: {out}");
    }

    // Requests: a prose request (MEDIUM), one that contains "continue" with a real subject, and a
    // structural question (HIGH) all still inject.
    let out = hook(project.path(), "the refund amount looks off");
    assert!(out.contains("refund_"), "{out}");
    let out = hook(project.path(), "continue the refund work");
    assert!(out.contains("refund_"), "{out}");
    let out = hook(project.path(), "how does continue_loop work?");
    assert!(out.contains("continue_loop"), "{out}");
}
