use codegraph_sync::worktree::*;
use std::{fs, path::Path, time::Duration};
fn git(root: &Path, args: &[&str]) -> String {
    codegraph_extract::scan::git::run_git(root, args, Duration::from_secs(5), 1024 * 1024).unwrap()
}
fn init(root: &Path) {
    fs::create_dir_all(root).unwrap();
    git(root, &["init", "-q", "--template="]);
    git(
        root,
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
            "-q",
            "--allow-empty",
            "-m",
            "fixture",
        ],
    );
}

#[test]
fn linked_worktrees_are_distinguished_and_share_common_directory() {
    let temp_root = codegraph_types::paths::real_path(&std::env::temp_dir()).unwrap();
    let temp = tempfile::tempdir_in(temp_root).unwrap();
    let main = temp.path().join("main");
    init(&main);
    let linked = main.join("nested/worktree");
    git(
        &main,
        &[
            "-c",
            "core.hooksPath=",
            "worktree",
            "add",
            "-q",
            "-b",
            "fixture-worktree",
            linked.to_str().unwrap(),
        ],
    );
    let m =
        detect_worktree_index_mismatch(linked.to_str().unwrap(), main.to_str().unwrap()).unwrap();
    assert_ne!(m.worktree_root, m.index_root);
    assert_eq!(
        git_common_dir(linked.to_str().unwrap()),
        git_common_dir(main.to_str().unwrap())
    );
    assert_eq!(
        git_worktree_root(linked.to_str().unwrap()),
        Some(m.worktree_root.clone())
    );
    assert!(worktree_mismatch_warning(&m).contains(&m.worktree_root));
    assert!(worktree_mismatch_notice(&m).contains("codegraph init -i"));
    assert!(
        detect_worktree_index_mismatch(linked.to_str().unwrap(), linked.to_str().unwrap())
            .is_none()
    );
    assert!(nested_repository_below(linked.to_str().unwrap(), main.to_str().unwrap()).is_none());
}

#[test]
fn embedded_repositories_are_not_borrowed_worktrees() {
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("main");
    init(&main);
    let embedded = main.join("vendor/service");
    init(&embedded);
    assert_ne!(
        git_common_dir(main.to_str().unwrap()),
        git_common_dir(embedded.to_str().unwrap())
    );
    assert!(
        detect_worktree_index_mismatch(embedded.to_str().unwrap(), main.to_str().unwrap())
            .is_none()
    );
    let nested = nested_repository_below(
        embedded.join("src/new.ts").to_str().unwrap(),
        main.to_str().unwrap(),
    )
    .unwrap();
    assert_eq!(nested.rel_path, "vendor/service");
    assert_eq!(
        Some(nested.root),
        git_worktree_root(embedded.to_str().unwrap())
    );
    assert!(nested_repository_below(main.to_str().unwrap(), embedded.to_str().unwrap()).is_none());
}

#[test]
fn plain_directories_same_tree_and_subproject_indexes_do_not_warn() {
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("main");
    init(&main);
    let sub = main.join("src");
    fs::create_dir(&sub).unwrap();
    let plain = temp.path().join("plain");
    fs::create_dir(&plain).unwrap();
    assert!(
        detect_worktree_index_mismatch(sub.to_str().unwrap(), main.to_str().unwrap()).is_none()
    );
    assert!(
        detect_worktree_index_mismatch(main.to_str().unwrap(), sub.to_str().unwrap()).is_none()
    );
    assert!(
        detect_worktree_index_mismatch(main.to_str().unwrap(), plain.to_str().unwrap()).is_none()
    );
    assert!(
        detect_worktree_index_mismatch(plain.to_str().unwrap(), main.to_str().unwrap()).is_none()
    );
    assert!(git_worktree_root(plain.to_str().unwrap()).is_none());
    assert!(nested_repository_below(sub.to_str().unwrap(), main.to_str().unwrap()).is_none());
}
