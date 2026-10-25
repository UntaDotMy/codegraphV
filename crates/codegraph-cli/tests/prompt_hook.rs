//! `codegraph prompt-hook` never breaks a prompt (DEV-65): every failure path exits 0 and prints
//! nothing, and the kill-switches silence it; a structural prompt on an indexed project injects.
use std::{
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};

fn hook(cwd: &Path, stdin: &str, env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codegraphV"));
    command
        .arg("prompt-hook")
        .current_dir(cwd)
        .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
        .env_remove("CODEGRAPHV_NO_PROMPT_HOOK")
        .env_remove("CODEGRAPHV_PROMPT_HOOK")
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
    let init = Command::new(env!("CARGO_BIN_EXE_codegraphV"))
        .args(["init", "."])
        .current_dir(project.path())
        .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    assert!(init.status.success());

    let ok = hook(project.path(), &payload("how does login work?", project.path()), &[]);
    assert!(ok.status.success());
    let text = String::from_utf8(ok.stdout).unwrap();
    assert!(text.starts_with("<codegraphV_context note=\"Structural context from CodeGraphV"), "{text}");
    assert!(text.contains("login"), "{text}");

    let silent = |out: Output, case: &str| {
        assert!(out.status.success(), "{case}: exit {:?}", out.status.code());
        assert!(out.stdout.is_empty(), "{case}: {}", String::from_utf8_lossy(&out.stdout));
    };
    let structural = payload("how does login work?", project.path());
    silent(hook(project.path(), &structural, &[("CODEGRAPHV_NO_PROMPT_HOOK", "1")]), "kill-switch");
    silent(hook(project.path(), &structural, &[("CODEGRAPHV_PROMPT_HOOK", "0")]), "kill-switch 2");
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
    std::fs::create_dir_all(broken.path().join(".codegraphV")).unwrap();
    std::fs::write(broken.path().join(".codegraphV/codegraph.db"), "not a database").unwrap();
    silent(hook(broken.path(), &payload("how does login work?", broken.path()), &[]), "corrupt index");
}

/// BUG-95: the hook `install` writes runs from the hook shell even when the agent's PATH does
/// not hold the install folder (an agent started before the install, or a folder never added).
#[test]
fn installed_hook_runs_without_the_install_folder_on_path() {
    let home = tempfile::tempdir().unwrap();
    let install = Command::new(env!("CARGO_BIN_EXE_codegraphV"))
        .args(["install", "--yes", "--target", "claude"])
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("LOCALAPPDATA", home.path())
        .env("APPDATA", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
        .env_remove("CLAUDE_CONFIG_DIR")
        .output()
        .unwrap();
    assert!(install.status.success(), "{}", String::from_utf8_lossy(&install.stderr));
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.path().join(".claude/settings.json")).unwrap()).unwrap();
    let command = settings["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"].as_str().unwrap().to_string();

    // Claude Code runs hooks with Git Bash on Windows and sh elsewhere.
    let shell = if cfg!(windows) {
        let exec = Command::new("git").arg("--exec-path").output().expect("git is installed");
        let exec = String::from_utf8(exec.stdout).unwrap();
        // <git>/mingw64/libexec/git-core -> <git>/bin/bash.exe
        let bash = Path::new(exec.trim()).ancestors().nth(3).unwrap().join("bin").join("bash.exe");
        assert!(bash.is_file(), "{}", bash.display());
        bash
    } else {
        Path::new("/bin/sh").to_path_buf()
    };
    let cwd = tempfile::tempdir().unwrap();
    let mut child = Command::new(shell)
        .arg("-c")
        .arg(&command)
        .current_dir(cwd.path())
        .env("PATH", "/usr/bin:/bin")
        .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(payload("how does login work?", cwd.path()).as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{command}: {}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stderr.is_empty(), "{command}: {}", String::from_utf8_lossy(&out.stderr));
}
