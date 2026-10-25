use codegraph_sync::git_hooks::*;
use std::{fs, path::Path, time::Duration};

fn git(root: &Path, args: &[&str]) -> String {
    codegraph_extract::scan::git::run_git(root, args, Duration::from_secs(5), 1024 * 1024).unwrap()
}
fn repo() -> tempfile::TempDir {
    let temp_root = codegraph_types::paths::real_path(&std::env::temp_dir()).unwrap();
    let dir = tempfile::tempdir_in(temp_root).unwrap();
    git(dir.path(), &["init", "-q", "--template="]);
    git(
        dir.path(),
        &[
            "config",
            "core.hooksPath",
            dir.path().join(".git/hooks").to_str().unwrap(),
        ],
    );
    fs::create_dir_all(dir.path().join(".git/hooks")).unwrap();
    dir
}
const COMMIT: &[GitHookName] = &[GitHookName::PostCommit];

#[test]
fn default_install_is_executable_idempotent_and_removable() {
    let dir = repo();
    let root = dir.path().to_str().unwrap();
    let result = install_git_sync_hook(root, None).unwrap();
    assert_eq!(result.installed, DEFAULT_SYNC_HOOKS);
    assert!(result.skipped.is_none());
    assert!(is_git_repo(root));
    assert!(is_sync_hook_installed(root, None).unwrap());
    for h in DEFAULT_SYNC_HOOKS {
        let p = dir.path().join(".git/hooks").join(h.as_str());
        let original = fs::read_to_string(&p).unwrap();
        assert!(original.contains("command -v codegraphV"));
        assert!(original.contains("codegraphV sync"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(fs::metadata(&p).unwrap().permissions().mode() & 0o111, 0);
        }
        install_git_sync_hook(root, Some(&[*h])).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), original);
    }
    assert_eq!(
        remove_git_sync_hook(root, None).unwrap().installed,
        DEFAULT_SYNC_HOOKS
    );
    for h in DEFAULT_SYNC_HOOKS {
        assert!(!dir.path().join(".git/hooks").join(h.as_str()).exists());
    }
}

#[test]
fn preserves_user_shell_content_and_puts_sync_before_exit() {
    let dir = repo();
    let root = dir.path().to_str().unwrap();
    let p = dir.path().join(".git/hooks/post-commit");
    let original = "#!/bin/sh\necho user-content\nexit 0\n";
    fs::write(&p, original).unwrap();
    install_git_sync_hook(root, Some(COMMIT)).unwrap();
    let body = fs::read_to_string(&p).unwrap();
    assert!(body.starts_with("#!/bin/sh\n# >>>"));
    assert!(body.find("codegraphV sync").unwrap() < body.find("exit 0").unwrap());
    install_git_sync_hook(root, Some(COMMIT)).unwrap();
    assert_eq!(fs::read_to_string(&p).unwrap(), body);
    remove_git_sync_hook(root, Some(COMMIT)).unwrap();
    assert_eq!(fs::read_to_string(p).unwrap(), original);
}

#[test]
fn one_block_per_project_and_removal_preserves_sibling_roots() {
    let dir = repo();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    fs::create_dir(&a).unwrap();
    fs::create_dir(&b).unwrap();
    let a = a.to_str().unwrap();
    let b = b.to_str().unwrap();
    install_git_sync_hook(a, Some(COMMIT)).unwrap();
    assert!(!is_sync_hook_installed(b, Some(COMMIT)).unwrap());
    install_git_sync_hook(b, Some(COMMIT)).unwrap();
    install_git_sync_hook(a, Some(COMMIT)).unwrap();
    let p = dir.path().join(".git/hooks/post-commit");
    assert_eq!(
        fs::read_to_string(&p)
            .unwrap()
            .matches("# >>> codegraphV sync hook >>>")
            .count(),
        2
    );
    remove_git_sync_hook(a, Some(COMMIT)).unwrap();
    assert!(!is_sync_hook_installed(a, Some(COMMIT)).unwrap());
    assert!(is_sync_hook_installed(b, Some(COMMIT)).unwrap());
}

#[test]
fn legacy_blocks_belong_to_the_worktree_root_and_are_upgraded() {
    let dir = repo();
    let root = dir.path().to_str().unwrap();
    let p = dir.path().join(".git/hooks/post-commit");
    fs::write(
        &p,
        "#!/bin/sh\n# >>> codegraphV sync hook >>>\ncodegraphV sync\n# <<< codegraphV sync hook <<<\n",
    )
    .unwrap();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    assert!(is_sync_hook_installed(root, Some(COMMIT)).unwrap());
    assert!(!is_sync_hook_installed(sub.to_str().unwrap(), Some(COMMIT)).unwrap());
    install_git_sync_hook(root, Some(COMMIT)).unwrap();
    let body = fs::read_to_string(p).unwrap();
    assert_eq!(body.matches("# >>> codegraphV sync hook >>>").count(), 1);
    assert!(body.contains("# codegraphV-root: "));
}

#[test]
fn honors_custom_hooks_path_and_non_shell_interpreters() {
    let dir = repo();
    git(dir.path(), &["config", "core.hooksPath", "custom-hooks"]);
    let custom = dir.path().join("custom-hooks");
    fs::create_dir(&custom).unwrap();
    let p = custom.join("post-commit");
    let python = "#!/usr/bin/env python3\nprint('user-hook')\n";
    fs::write(&p, python).unwrap();
    let result = install_git_sync_hook(dir.path().to_str().unwrap(), Some(COMMIT)).unwrap();
    assert_eq!(result.unsupported, Some(COMMIT.to_vec()));
    assert!(result.installed.is_empty());
    assert_eq!(fs::read_to_string(&p).unwrap(), python);
    fs::write(&p, "#!/usr/bin/env -S bash\necho user-hook\n").unwrap();
    assert_eq!(
        install_git_sync_hook(dir.path().to_str().unwrap(), Some(COMMIT))
            .unwrap()
            .installed,
        COMMIT
    );
    assert!(is_sync_hook_installed(dir.path().to_str().unwrap(), Some(COMMIT)).unwrap());
}

#[test]
fn non_git_and_newline_roots_are_success_shaped_skips() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_str().unwrap();
    assert!(!is_git_repo(root));
    assert_eq!(
        install_git_sync_hook(root, None)
            .unwrap()
            .skipped
            .as_deref(),
        Some("not a git repository")
    );
    assert_eq!(
        remove_git_sync_hook(root, None).unwrap().skipped.as_deref(),
        Some("not a git repository")
    );
    assert!(!is_sync_hook_installed(root, None).unwrap());
    let bad = format!("{root}/bad\npath");
    assert_eq!(
        install_git_sync_hook(&bad, None)
            .unwrap()
            .skipped
            .as_deref(),
        Some("the project path contains a line break")
    );
}

#[test]
fn quotes_spaces_and_apostrophes_without_shell_interpolation() {
    let dir = repo();
    let sub = dir.path().join("app's code");
    fs::create_dir(&sub).unwrap();
    install_git_sync_hook(sub.to_str().unwrap(), Some(COMMIT)).unwrap();
    let body = fs::read_to_string(dir.path().join(".git/hooks/post-commit")).unwrap();
    assert!(body.contains("app'\\''s code'"));
}

#[cfg(windows)]
#[test]
fn junction_roots_resolve_to_the_same_hook_owner() {
    use std::os::windows::process::CommandExt;
    let dir = repo();
    let links = tempfile::tempdir().unwrap();
    let alias = links.path().join("repo-link");
    let out = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(&alias)
        .arg(dir.path())
        .creation_flags(0x08000000)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    install_git_sync_hook(alias.to_str().unwrap(), Some(COMMIT)).unwrap();
    assert!(is_sync_hook_installed(dir.path().to_str().unwrap(), Some(COMMIT)).unwrap());
    remove_git_sync_hook(alias.to_str().unwrap(), Some(COMMIT)).unwrap();
    assert!(!is_sync_hook_installed(dir.path().to_str().unwrap(), Some(COMMIT)).unwrap());
    fs::remove_dir(alias).unwrap();
}
