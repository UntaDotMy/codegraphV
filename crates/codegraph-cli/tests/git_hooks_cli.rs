//! A05/A06: Git sync hooks are an explicit opt-in (`init --git-hooks`) and `uninit` removes
//! only the blocks it owns, alongside the index.
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(root).output().expect("git is installed");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}
fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_codegraphV"))
        .args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env("LOCALAPPDATA", home)
        .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap()
}

#[test]
fn init_git_hooks_is_opt_in_and_uninit_removes_only_owned_blocks() {
    let home = tempfile::tempdir().unwrap();
    git(home.path(), &["init", "-q", "--template="]);
    fs::write(home.path().join("app.ts"), "export function start(){return 1;}\n").unwrap();
    let hook_dir = home.path().join(".git/hooks");
    fs::create_dir_all(&hook_dir).unwrap();
    let post_commit = hook_dir.join("post-commit");
    let user_hook = "#!/bin/sh\necho user-content\n";
    fs::write(&post_commit, user_hook).unwrap();

    // Without the flag, init only indexes.
    let out = run(home.path(), &["init", ".", "--force"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read_to_string(&post_commit).unwrap(), user_hook);

    let out = run(home.path(), &["init", ".", "--force", "--git-hooks"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let body = fs::read_to_string(&post_commit).unwrap();
    assert!(body.contains("# >>> codegraphV sync hook >>>"), "{body}");
    assert!(body.contains("echo user-content"), "{body}");

    // A second init is idempotent for the owned block.
    let out = run(home.path(), &["init", ".", "--force", "--git-hooks"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read_to_string(&post_commit).unwrap(), body);

    let out = run(home.path(), &["uninit", ".", "--force"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read_to_string(&post_commit).unwrap(), user_hook);
    assert!(!home.path().join(".codegraphV").exists());
    // The other default hooks existed only because we created them.
    for name in ["post-merge", "post-checkout"] {
        assert!(!hook_dir.join(name).exists(), "{name}");
    }
}

#[test]
fn uninit_keeps_sibling_products_blocks_and_custom_hooks_path() {
    let home = tempfile::tempdir().unwrap();
    git(home.path(), &["init", "-q", "--template="]);
    fs::write(home.path().join("app.ts"), "export function start(){return 1;}\n").unwrap();
    git(home.path(), &["config", "core.hooksPath", "custom-hooks"]);
    let hooks = home.path().join("custom-hooks");
    fs::create_dir_all(&hooks).unwrap();
    let post_commit = hooks.join("post-commit");
    let original = "#!/bin/sh\n# >>> another-product >>>\necho other\n# <<< another-product <<<\n";
    fs::write(&post_commit, original).unwrap();
    let out = run(home.path(), &["init", ".", "--force", "--git-hooks"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(fs::read_to_string(&post_commit).unwrap().contains("another-product"));
    let out = run(home.path(), &["uninit", ".", "--force"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read_to_string(&post_commit).unwrap(), original);
}
