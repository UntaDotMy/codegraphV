//! Upstream e651b872 / d2e0389b: sparse scan and freshness, on real temporary Git repos.
use codegraph_core::{CodeGraph, IndexOptions};
use codegraph_extract::scan::{git::run_git, scan_directory};
use std::{fs, path::Path, time::Duration};
fn git(root: &Path, args: &[&str]) -> String {
    run_git(root, args, Duration::from_secs(15), 1_048_576).unwrap()
}
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "--template="]);
    for folder in ["one", "two", "kept"] {
        fs::create_dir_all(dir.path().join(folder)).unwrap();
        fs::write(
            dir.path().join(folder).join("a.ts"),
            format!("export function {folder}() {{ return 1; }}\n"),
        )
        .unwrap();
    }
    git(dir.path(), &["add", "."]);
    git(
        dir.path(),
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "core.hooksPath=",
            "-c",
            "commit.gpgSign=false",
            "commit",
            "-qm",
            "fixture",
        ],
    );
    dir
}
fn snapshot(cg: &CodeGraph) -> (Vec<String>, Vec<String>, Vec<String>) {
    let nodes = cg.store().get_all_nodes().unwrap();
    let mut ns: Vec<_> = nodes
        .iter()
        .map(|node| {
            let mut v = serde_json::to_value(node).unwrap();
            v.as_object_mut().unwrap().remove("updatedAt");
            v.to_string()
        })
        .collect();
    let mut es: Vec<_> = nodes
        .iter()
        .flat_map(|n| cg.get_outgoing_edges(&n.id).unwrap())
        .map(|e| serde_json::to_string(&e).unwrap())
        .collect();
    let mut refs: Vec<_> = cg
        .store()
        .get_unresolved_references()
        .unwrap()
        .iter()
        .map(|r| {
            let mut v = serde_json::to_value(r).unwrap();
            v.as_object_mut().unwrap().remove("id");
            v.to_string()
        })
        .collect();
    ns.sort();
    es.sort();
    refs.sort();
    (ns, es, refs)
}
#[test]
fn missing_skip_worktree_paths_are_not_scan_candidates_but_present_ones_are() {
    let dir = fixture();
    git(
        dir.path(),
        &["update-index", "--skip-worktree", "two/a.ts", "kept/a.ts"],
    );
    fs::remove_file(dir.path().join("two/a.ts")).unwrap();
    let files = scan_directory(dir.path(), None, None).unwrap();
    assert!(!files.iter().any(|file| file == "two/a.ts"), "{files:?}");
    assert!(files.iter().any(|file| file == "kept/a.ts"), "{files:?}");
}
#[test]
fn sparse_cone_changes_are_visible_before_sync_and_converge_to_a_full_index() {
    let dir = fixture();
    git(dir.path(), &["sparse-checkout", "init", "--cone"]);
    git(dir.path(), &["sparse-checkout", "set", "one"]);
    let mut cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    assert!(cg.get_changed_files().unwrap().added.is_empty());
    git(dir.path(), &["sparse-checkout", "set", "one", "two"]);
    assert!(git(
        dir.path(),
        &["status", "--porcelain", "--untracked-files=no"]
    )
    .trim()
    .is_empty());
    assert_eq!(cg.get_changed_files().unwrap().added, ["two/a.ts"]);
    cg.sync(
        &IndexOptions {
            paths: Some(vec!["one/a.ts".into()]),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    assert_eq!(
        cg.get_changed_files().unwrap().added,
        ["two/a.ts"],
        "scoped sync must not claim sparse freshness"
    );
    cg.sync(&IndexOptions::default(), None).unwrap();
    let synced = snapshot(&cg);
    cg.index_all(&IndexOptions::default(), None, None).unwrap();
    assert_eq!(synced, snapshot(&cg));
    git(dir.path(), &["sparse-checkout", "set", "two"]);
    assert_eq!(cg.get_changed_files().unwrap().removed, ["one/a.ts"]);
    cg.sync(&IndexOptions::default(), None).unwrap();
    let synced = snapshot(&cg);
    cg.index_all(&IndexOptions::default(), None, None).unwrap();
    assert_eq!(synced, snapshot(&cg));
    git(dir.path(), &["sparse-checkout", "disable"]);
    let changes = cg.get_changed_files().unwrap();
    assert!(
        changes.added.contains(&"one/a.ts".into()) && changes.added.contains(&"kept/a.ts".into()),
        "{changes:?}"
    );
    cg.close();
}

#[test]
fn linked_worktree_sparse_changes_are_scoped_to_its_git_directory() {
    let repo = fixture();
    let worktree_parent = tempfile::tempdir().unwrap();
    let worktree = worktree_parent.path().join("linked");
    git(
        repo.path(),
        &[
            "worktree",
            "add",
            "--detach",
            worktree.to_str().unwrap(),
            "HEAD",
        ],
    );
    git(&worktree, &["sparse-checkout", "init", "--cone"]);
    git(&worktree, &["sparse-checkout", "set", "one"]);
    let cg = CodeGraph::init_and_index(worktree.to_str().unwrap(), None).unwrap();
    let main = CodeGraph::init_and_index(repo.path().to_str().unwrap(), None).unwrap();
    git(&worktree, &["sparse-checkout", "set", "one", "two"]);
    assert_eq!(cg.get_changed_files().unwrap().added, ["two/a.ts"]);
    assert!(main.get_changed_files().unwrap().added.is_empty());
    assert!(main.get_changed_files().unwrap().removed.is_empty());
    cg.close();
    main.close();
}
