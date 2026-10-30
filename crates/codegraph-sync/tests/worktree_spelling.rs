//! Worktree detection when the caller spells a path unlike git does (upstream #2937).
//!
//! On Windows one directory has several spellings: an editor hands over a lowercase drive letter,
//! NTFS ignores case. git answers with the spelling on disk, so two paths naming one directory have
//! to be recognised as one — and two directories must still not be.
use codegraph_sync::worktree::*;
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

fn git(root: &Path, args: &[&str]) -> String {
    codegraph_extract::scan::git::run_git(root, args, Duration::from_secs(5), 1024 * 1024).unwrap()
}

fn init(root: &Path) {
    fs::create_dir_all(root).unwrap();
    git(root, &["init", "-q", "--template="]);
    git(
        root,
        &[
            "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "-c", "core.hooksPath=",
            "-c", "commit.gpgSign=false", "commit", "-q", "--allow-empty", "-m", "fixture",
        ],
    );
}

fn text(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// A checkout with a linked worktree and an embedded clone below it, and an unrelated repository
/// beside it.
struct Fixture {
    _temp: tempfile::TempDir,
    main: PathBuf,
    linked: PathBuf,
    embedded: PathBuf,
    other: PathBuf,
}

fn fixture() -> Fixture {
    let temp_root = codegraph_types::paths::real_path(&std::env::temp_dir()).unwrap();
    let temp = tempfile::tempdir_in(temp_root).unwrap();
    let main = temp.path().join("main");
    init(&main);
    let linked = main.join(".claude").join("worktrees").join("feature");
    git(&main, &["-c", "core.hooksPath=", "worktree", "add", "-q", "-b", "fixture-worktree", text(&linked)]);
    let embedded = main.join("vendor").join("service");
    init(&embedded);
    let other = temp.path().join("other");
    init(&other);
    Fixture { _temp: temp, main, linked, embedded, other }
}

/// The spellings an editor on Windows may hand over for `path`: the drive letter lowercased, and
/// the whole path lowercased. Neither is the spelling git reports.
#[cfg(windows)]
fn spellings(path: &Path) -> Vec<String> {
    let on_disk = text(path).to_string();
    let drive_only = format!("{}{}", on_disk[..1].to_lowercase(), &on_disk[1..]);
    let all_lower = on_disk.to_lowercase();
    assert_ne!(drive_only, on_disk, "the fixture must start from an uppercase drive letter");
    vec![drive_only, all_lower]
}

/// `path` rewritten the way `from` (a spelling of its ancestor `base`) is spelled.
#[cfg(windows)]
fn respell(path: &Path, base: &Path, spelled_base: &str) -> String {
    format!("{spelled_base}{}", &text(path)[text(base).len()..])
}

#[cfg(windows)]
#[test]
fn lowercase_drive_worktree_is_a_mismatch() {
    let f = fixture();
    for spelled_main in spellings(&f.main) {
        let spelled_linked = respell(&f.linked, &f.main, &spelled_main);
        let m = detect_worktree_index_mismatch(&spelled_linked, &spelled_main)
            .unwrap_or_else(|| panic!("no mismatch for {spelled_linked} against {spelled_main}"));
        assert!(m.worktree_root.eq_ignore_ascii_case(text(&f.linked)), "{m:?} vs {}", text(&f.linked));
        // The index root keeps the caller's spelling for display; only the comparison is spelling-blind.
        assert_eq!(m.index_root, spelled_main, "{m:?}");
        assert!(worktree_mismatch_warning(&m).contains(&spelled_main));
    }
}

#[cfg(windows)]
#[test]
fn lowercase_drive_checkout_itself_is_not_a_mismatch() {
    let f = fixture();
    for spelled_main in spellings(&f.main) {
        assert!(
            detect_worktree_index_mismatch(&spelled_main, text(&f.main)).is_none(),
            "{spelled_main} is the indexed checkout itself"
        );
        assert!(detect_worktree_index_mismatch(text(&f.main), &spelled_main).is_none());
    }
}

#[cfg(windows)]
#[test]
fn lowercase_drive_linked_worktree_is_not_a_separate_repository() {
    let f = fixture();
    for spelled_main in spellings(&f.main) {
        let spelled_linked = respell(&f.linked, &f.main, &spelled_main);
        assert!(
            nested_repository_below(&spelled_linked, &spelled_main).is_none(),
            "the checkout's own worktree {spelled_linked} was refused as a repository of its own"
        );
        let file_below = format!("{spelled_linked}\\src\\new.ts");
        assert!(nested_repository_below(&file_below, &spelled_main).is_none(), "{file_below}");
    }
}

#[cfg(windows)]
#[test]
fn lowercase_drive_embedded_clone_is_still_a_separate_repository() {
    let f = fixture();
    for spelled_main in spellings(&f.main) {
        let spelled_embedded = respell(&f.embedded, &f.main, &spelled_main);
        let nested = nested_repository_below(&format!("{spelled_embedded}\\src\\new.ts"), &spelled_main)
            .unwrap_or_else(|| panic!("the embedded clone {spelled_embedded} went unnoticed"));
        assert_eq!(nested.rel_path, "vendor/service");
        assert!(nested.root.eq_ignore_ascii_case(text(&f.embedded)), "{nested:?} vs {}", text(&f.embedded));
        assert!(
            detect_worktree_index_mismatch(&spelled_embedded, &spelled_main).is_none(),
            "an embedded clone is not a borrowed worktree index"
        );
    }
}

#[cfg(windows)]
#[test]
fn lowercase_drive_unrelated_repository_stays_unrelated() {
    let f = fixture();
    for spelled_other in spellings(&f.other) {
        assert!(detect_worktree_index_mismatch(text(&f.linked), &spelled_other).is_none());
        assert!(detect_worktree_index_mismatch(&spelled_other, text(&f.main)).is_none());
        assert!(nested_repository_below(text(&f.main), &spelled_other).is_none());
    }
}

#[test]
fn a_repository_below_a_plain_directory_is_still_reported() {
    let temp_root = codegraph_types::paths::real_path(&std::env::temp_dir()).unwrap();
    let temp = tempfile::tempdir_in(temp_root).unwrap();
    let plain = temp.path().join("plain");
    let repo = plain.join("repo");
    init(&repo);
    let nested = nested_repository_below(text(&repo.join("src/new.ts")), text(&plain))
        .expect("an index root outside git has no repository of its own to match");
    assert_eq!(nested.rel_path, "repo");
    assert_eq!(Some(nested.root), git_worktree_root(text(&repo)));
}

#[test]
fn unspelled_fixture_keeps_its_verdicts() {
    let f = fixture();
    let m = detect_worktree_index_mismatch(text(&f.linked), text(&f.main)).expect("a linked worktree borrows the checkout's index");
    assert_eq!(m.index_root, text(&f.main));
    assert!(nested_repository_below(text(&f.linked), text(&f.main)).is_none());
    assert!(nested_repository_below(text(&f.embedded), text(&f.main)).is_some());
    assert!(detect_worktree_index_mismatch(text(&f.linked), text(&f.other)).is_none());
    assert!(nested_repository_below(text(&f.other), text(&f.main)).is_none(), "a repository beside the checkout is not below it");
}
