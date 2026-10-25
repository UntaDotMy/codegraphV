//! `codegraph prompt-hook` never breaks a prompt (DEV-65): every failure path exits 0 and prints
//! nothing, and the kill-switches silence it; a structural prompt on an indexed project injects.
use std::{
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};

fn hook(cwd: &Path, stdin: &str, env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codegraph"));
    command
        .arg("prompt-hook")
        .current_dir(cwd)
        .env("CODEGRAPH_NO_UPDATE_CHECK", "1")
        .env_remove("CODEGRAPH_NO_PROMPT_HOOK")
        .env_remove("CODEGRAPH_PROMPT_HOOK")
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

fn payload(prompt: &str, cwd: &Path) -> String {
    serde_json::json!({ "prompt": prompt, "cwd": cwd }).to_string()
}

#[test]
fn injects_for_structural_prompts_and_is_silent_on_every_failure_path() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("auth.ts"),
        "export function parseToken(raw: string) { return raw; }\nexport function login(u: string) { return parseToken(u); }\n",
    )
    .unwrap();
    let init = Command::new(env!("CARGO_BIN_EXE_codegraph"))
        .args(["init", "."])
        .current_dir(project.path())
        .env("CODEGRAPH_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    assert!(init.status.success());

    let ok = hook(project.path(), &payload("how does login work?", project.path()), &[]);
    assert!(ok.status.success());
    let text = String::from_utf8(ok.stdout).unwrap();
    assert!(text.starts_with("<codegraph_context note=\"Structural context from CodeGraph"), "{text}");
    assert!(text.contains("login"), "{text}");

    let silent = |out: Output, case: &str| {
        assert!(out.status.success(), "{case}: exit {:?}", out.status.code());
        assert!(out.stdout.is_empty(), "{case}: {}", String::from_utf8_lossy(&out.stdout));
    };
    let structural = payload("how does login work?", project.path());
    silent(hook(project.path(), &structural, &[("CODEGRAPH_NO_PROMPT_HOOK", "1")]), "kill-switch");
    silent(hook(project.path(), &structural, &[("CODEGRAPH_PROMPT_HOOK", "0")]), "kill-switch 2");
    silent(hook(project.path(), "not json {", &[]), "malformed payload");
    silent(hook(project.path(), "", &[]), "empty payload");
    silent(hook(project.path(), &payload("fix this typo", project.path()), &[]), "non-structural");
    // A missing cwd outside any index (one inside an indexed project walks up to it, as in TS).
    let unindexed = tempfile::tempdir().unwrap();
    let missing = unindexed.path().join("does-not-exist");
    silent(hook(unindexed.path(), &payload("how does login work?", &missing), &[]), "missing cwd");
    silent(hook(unindexed.path(), &payload("how does login work?", unindexed.path()), &[]), "no index");
    // A corrupt index is an engine error: still silent.
    let broken = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(broken.path().join(".codegraph")).unwrap();
    std::fs::write(broken.path().join(".codegraph/codegraph.db"), "not a database").unwrap();
    silent(hook(broken.path(), &payload("how does login work?", broken.path()), &[]), "corrupt index");
}
