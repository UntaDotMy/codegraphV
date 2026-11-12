//! `init --local-only` keeps `.codegraphV/` out of git entirely (upstream fb71a9f4, #910): its
//! `.gitignore` becomes a lone `*`, which ignores itself too.
use std::{path::Path, process::Command};

const LOCAL_ONLY: &str = "*\n";

fn run(root: &Path, args: &[&str]) -> String {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_codegraphV"))
        .args(args)
        .current_dir(root)
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("LOCALAPPDATA", home.path())
        .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
        .env("CODEGRAPHV_NO_DAEMON", "1")
        .output()
        .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{args:?}: {text}");
    text
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid", "-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    std::fs::write(dir.path().join("a.js"), "function a() { return 1; }\n").unwrap();
    git(dir.path(), &["add", "a.js"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

fn gitignore(root: &Path) -> String {
    std::fs::read_to_string(root.join(".codegraphV/.gitignore")).unwrap().replace("\r\n", "\n")
}

#[test]
fn a_new_project_shows_nothing_in_git() {
    let dir = repo();
    let out = run(dir.path(), &["init", "--local-only"]);
    assert!(out.contains("now ignores itself"), "{out}");
    assert_eq!(gitignore(dir.path()), LOCAL_ONLY);
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

#[test]
fn plain_init_keeps_the_shared_default() {
    let dir = repo();
    run(dir.path(), &["init"]);
    assert!(gitignore(dir.path()).contains("!.gitignore"));
    assert!(git(dir.path(), &["status", "--porcelain"]).contains(".codegraphV/"));
}

#[test]
fn an_initialized_project_converts_without_indexing_again_and_a_second_run_says_so() {
    let dir = repo();
    run(dir.path(), &["init"]);
    let db = dir.path().join(".codegraphV/codegraph.db");
    let before = std::fs::read(&db).unwrap();
    let out = run(dir.path(), &["init", "--local-only"]);
    assert!(out.contains("now ignores itself"), "{out}");
    assert_eq!(gitignore(dir.path()), LOCAL_ONLY);
    assert_eq!(std::fs::read(&db).unwrap(), before, "the database is untouched");
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
    let again = run(dir.path(), &["init", "--local-only"]);
    assert!(again.contains("already ignores itself"), "{again}");
}

#[test]
fn a_customized_file_is_left_alone() {
    let dir = repo();
    run(dir.path(), &["init"]);
    let custom = "codegraph.db\n";
    std::fs::write(dir.path().join(".codegraphV/.gitignore"), custom).unwrap();
    let out = run(dir.path(), &["init", "--local-only"]);
    assert!(out.contains("your own rules"), "{out}");
    assert_eq!(gitignore(dir.path()), custom);
}

#[test]
fn a_committed_default_is_left_alone() {
    let dir = repo();
    run(dir.path(), &["init"]);
    git(dir.path(), &["add", ".codegraphV/.gitignore"]);
    git(dir.path(), &["commit", "-q", "-m", "share"]);
    let before = gitignore(dir.path());
    let out = run(dir.path(), &["init", "--local-only"]);
    assert!(out.contains("committed"), "{out}");
    assert_eq!(gitignore(dir.path()), before);
}

#[test]
fn the_local_only_file_survives_sync_status_index_and_plain_init() {
    let dir = repo();
    run(dir.path(), &["init", "--local-only"]);
    std::fs::write(dir.path().join("b.js"), "function b() { return 2; }\n").unwrap();
    run(dir.path(), &["sync"]);
    run(dir.path(), &["status"]);
    run(dir.path(), &["index"]);
    run(dir.path(), &["init"]);
    assert_eq!(gitignore(dir.path()), LOCAL_ONLY);
}
