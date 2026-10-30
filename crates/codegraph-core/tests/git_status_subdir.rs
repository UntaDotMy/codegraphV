//! Change detection for a project that is a folder of its git repository (upstream #2932): `git
//! status` prints every path from the repository's root, so the project's own paths have to be taken
//! back out of them, and anything outside the folder left out.
use codegraph_core::{CodeGraph, IndexOptions};
use codegraph_extract::scan::{get_git_changed_files, git::run_git};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

fn git(root: &Path, args: &[&str]) -> String {
    run_git(root, args, Duration::from_secs(10), 1024 * 1024).unwrap()
}

fn commit_all(root: &Path, message: &str) {
    git(root, &["add", "-A"]);
    git(root, &["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "-c", "core.hooksPath=", "-c", "commit.gpgSign=false", "commit", "-q", "-m", message]);
}

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn init_repo(root: &Path) {
    fs::create_dir_all(root).unwrap();
    git(root, &["init", "-q", "--template="]);
    git(root, &["config", "core.autocrlf", "false"]);
}

/// A repository holding an `app/` project beside a `server/` one, everything committed.
fn monorepo() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp_root = codegraph_types::paths::real_path(&std::env::temp_dir()).unwrap();
    let dir = tempfile::tempdir_in(temp_root).unwrap();
    let repo = dir.path().join("repo");
    init_repo(&repo);
    write(&repo, "app/src/index.ts", "export function startApp() { return 1; }\n");
    write(&repo, "app/src/keep.ts", "export function keepMe() { return 2; }\n");
    write(&repo, "server/src/main.ts", "export function serve() { return 3; }\n");
    commit_all(&repo, "fixture");
    let app = repo.join("app");
    (dir, repo, app)
}

fn sorted(mut paths: Vec<String>) -> Vec<String> {
    paths.sort();
    paths
}

#[test]
fn changes_below_the_repository_root_are_reported_project_relative() {
    let (_dir, repo, app) = monorepo();
    write(&repo, "app/src/index.ts", "export function startApp() { return 10; }\n");
    write(&repo, "app/src/new.ts", "export function fresh() { return 4; }\n");
    fs::remove_file(repo.join("app/src/keep.ts")).unwrap();
    write(&repo, "server/src/main.ts", "export function serve() { return 30; }\n");
    write(&repo, "server/src/extra.ts", "export function extra() { return 5; }\n");
    let changes = get_git_changed_files(&app, None).expect("git answers for a project below its root");
    assert_eq!(changes.modified, ["src/index.ts"], "{changes:?}");
    assert_eq!(changes.added, ["src/new.ts"], "{changes:?}");
    assert_eq!(changes.deleted, ["src/keep.ts"], "{changes:?}");
}

#[test]
fn a_project_its_repository_ignores_declines_the_git_path() {
    let (_dir, repo, _app) = monorepo();
    write(&repo, ".gitignore", "ignored-app/\n");
    commit_all(&repo, "ignore a folder");
    write(&repo, "ignored-app/src/a.ts", "export const a = 1;\n");
    let ignored = repo.join("ignored-app");
    assert!(get_git_changed_files(&ignored, None).is_none(), "git says nothing about a folder its repository ignores");
}

#[test]
fn a_project_at_its_repository_root_reports_repository_relative_paths() {
    let (_dir, repo, _app) = monorepo();
    write(&repo, "app/src/index.ts", "export function startApp() { return 10; }\n");
    let changes = get_git_changed_files(&repo, None).unwrap();
    assert_eq!(changes.modified, ["app/src/index.ts"], "{changes:?}");
}

#[test]
fn a_folder_with_its_own_repository_is_a_root() {
    let (_dir, repo, _app) = monorepo();
    let nested = repo.join("nested");
    init_repo(&nested);
    write(&nested, "x.ts", "export const x = 1;\n");
    commit_all(&nested, "nested");
    write(&nested, "x.ts", "export const x = 2;\n");
    let changes = get_git_changed_files(&nested, None).unwrap();
    assert_eq!(changes.modified, ["x.ts"], "{changes:?}");
}

#[test]
fn the_projects_own_anchored_rule_applies_to_project_relative_paths() {
    let (_dir, repo, app) = monorepo();
    // A tracked file under an ignored folder still shows in `git status`; the full index never
    // tracks it, so change detection must not report it either. The rule is anchored to the
    // project: `generated/` at its root, not `src/generated/`.
    write(&repo, "app/.gitignore", "/generated/\n");
    write(&repo, "app/generated/gen.ts", "export const gen = 1;\n");
    write(&repo, "app/src/generated/gen.ts", "export const nested = 1;\n");
    git(&repo, &["add", "-f", "app/generated/gen.ts"]);
    commit_all(&repo, "tracked under an ignored folder");
    write(&repo, "app/generated/gen.ts", "export const gen = 2;\n");
    write(&repo, "app/src/generated/gen.ts", "export const nested = 2;\n");
    let changes = get_git_changed_files(&app, None).unwrap();
    assert_eq!(changes.modified, ["src/generated/gen.ts"], "{changes:?}");
}

#[test]
fn an_embedded_repository_below_the_project_is_reported_project_relative() {
    let (_dir, repo, app) = monorepo();
    let embedded = repo.join("app/embedded");
    init_repo(&embedded);
    write(&embedded, "x.ts", "export const x = 1;\n");
    commit_all(&embedded, "embedded");
    write(&embedded, "x.ts", "export const x = 2;\n");
    write(&repo, "app/src/index.ts", "export function startApp() { return 10; }\n");
    let changes = get_git_changed_files(&app, None).unwrap();
    assert_eq!(sorted(changes.modified.clone()), ["embedded/x.ts", "src/index.ts"], "{changes:?}");
}

// --- through CodeGraph ----------------------------------------------------------------------

fn indexed_app() -> (tempfile::TempDir, PathBuf, PathBuf, CodeGraph) {
    let (dir, repo, app) = monorepo();
    let mut cg = CodeGraph::init(&app.to_string_lossy()).unwrap();
    cg.index_all(&IndexOptions::default(), None, None).unwrap();
    (dir, repo, app, cg)
}

#[test]
fn status_sees_an_edit_and_an_added_file_in_a_project_below_the_root() {
    let (_dir, repo, _app, cg) = indexed_app();
    assert_eq!(cg.get_changed_files().unwrap(), Default::default(), "nothing changed yet");
    write(&repo, "app/src/index.ts", "export function startApp() { return 10; }\n");
    write(&repo, "app/src/new.ts", "export function fresh() { return 4; }\n");
    let changed = cg.get_changed_files().unwrap();
    assert_eq!(changed.modified, ["src/index.ts"], "{changed:?}");
    assert_eq!(changed.added, ["src/new.ts"], "{changed:?}");
    assert!(changed.removed.is_empty(), "{changed:?}");
}

#[test]
fn a_file_put_back_with_git_after_a_sync_is_still_seen_across_a_reopen() {
    let (_dir, repo, app, mut cg) = indexed_app();
    write(&repo, "app/src/index.ts", "export function startApp() { return 10; }\n");
    let synced = cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(synced.files_modified, 1, "sync should have taken the edit: {synced:?}");
    assert_eq!(cg.get_changed_files().unwrap(), Default::default(), "the sync caught up");
    // Put the committed content back with git: `git status` is clean again, but the index holds the edit.
    git(&repo, &["checkout", "--", "app/src/index.ts"]);
    let before = cg.get_changed_files().unwrap();
    assert_eq!(before.modified, ["src/index.ts"], "the restore was missed: {before:?}");
    cg.close();
    let reopened = CodeGraph::open(&app.to_string_lossy(), Default::default()).unwrap();
    let after = reopened.get_changed_files().unwrap();
    assert_eq!(after.modified, ["src/index.ts"], "the restore was missed after a reopen: {after:?}");
}
