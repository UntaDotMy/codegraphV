//! What a full index accounts for: a file the scan lists that nothing is at, a video named `.ts`,
//! and the name-segment vocabulary a repeat run keeps (upstream #2928, #2943, #3017).
use codegraph_core::orchestrator::IndexResult;
use codegraph_core::{CodeGraph, IndexOptions, IndexState};
use codegraph_extract::languages::MPEG_TS_SNIFF_BYTES;
use codegraph_extract::scan::{git::run_git, INDEXED_AT_COMMIT_KEY};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::Duration,
};

fn git(root: &Path, args: &[&str]) -> String {
    run_git(root, args, Duration::from_secs(10), 1024 * 1024).unwrap()
}

/// A git project: `app.ts` plus whatever `files` adds, all committed.
fn git_project(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let temp_root = codegraph_types::paths::real_path(&std::env::temp_dir()).unwrap();
    let dir = tempfile::tempdir_in(temp_root).unwrap();
    let root = dir.path().join("project");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "--template="]);
    fs::write(root.join("app.ts"), "export function startApp() { return 1; }\n").unwrap();
    for (name, text) in files {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    git(&root, &["add", "-A"]);
    git(
        &root,
        &["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "-c", "core.hooksPath=", "-c", "commit.gpgSign=false", "commit", "-q", "-m", "fixture"],
    );
    (dir, root)
}

fn index(root: &Path) -> (CodeGraph, IndexResult) {
    let mut cg = CodeGraph::init(&root.to_string_lossy()).unwrap();
    let result = cg.index_all(&IndexOptions::default(), None, None).unwrap();
    (cg, result)
}

fn head(root: &Path) -> String {
    git(root, &["rev-parse", "HEAD"]).trim().to_string()
}

fn symlink_file(target: &Path, link: &Path) -> bool {
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file(target, link);
    #[cfg(not(windows))]
    let made = std::os::unix::fs::symlink(target, link);
    made.is_ok()
}

/// The run read as complete: nothing errored, nothing missing, the commit recorded.
fn assert_complete(cg: &CodeGraph, result: &IndexResult, root: &Path) {
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.files_errored, 0);
    assert!(result.success);
    assert_eq!(cg.get_index_state(), Some(IndexState::Complete));
    assert_eq!(cg.store().get_metadata(INDEXED_AT_COMMIT_KEY).unwrap().as_deref(), Some(head(root).as_str()), "the indexed commit was not recorded");
}

#[test]
fn a_tracked_file_deleted_from_the_working_tree_is_skipped() {
    let (_dir, root) = git_project(&[("gone.ts", "export const gone = 1;\n")]);
    fs::remove_file(root.join("gone.ts")).unwrap();
    let (cg, result) = index(&root);
    assert_eq!(result.files_discovered, Some(2), "git still lists the deleted file");
    assert_eq!((result.files_indexed, result.files_skipped), (1, 1));
    assert_complete(&cg, &result, &root);
}

#[test]
fn a_folder_replaced_by_a_file_is_skipped() {
    let (_dir, root) = git_project(&[("sub/mod.ts", "export const mod = 1;\n")]);
    fs::remove_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("sub"), "now a file").unwrap();
    let (cg, result) = index(&root);
    assert_eq!(result.files_discovered, Some(2), "git still lists the file under the old folder");
    assert_eq!((result.files_indexed, result.files_skipped), (1, 1));
    assert_complete(&cg, &result, &root);
}

#[test]
fn a_dangling_symlink_is_skipped() {
    let (_dir, root) = git_project(&[]);
    if !symlink_file(&root.join("nothing-here.ts"), &root.join("link.ts")) {
        eprintln!("symlinks are not permitted here: the dangling-link case was not exercised");
        return;
    }
    let (cg, result) = index(&root);
    assert_eq!(result.files_discovered, Some(2), "git lists the link");
    assert_eq!((result.files_indexed, result.files_skipped), (1, 1));
    assert_complete(&cg, &result, &root);
}

#[test]
fn a_folder_where_a_file_was_is_still_a_read_error() {
    let (_dir, root) = git_project(&[("tool.ts", "export const tool = 1;\n")]);
    fs::remove_file(root.join("tool.ts")).unwrap();
    fs::create_dir(root.join("tool.ts")).unwrap();
    let (cg, result) = index(&root);
    assert_eq!(result.files_errored, 1, "{:?}", result.errors);
    let error = result.errors.iter().find(|e| e.code.as_deref() == Some("read_error")).expect("a read_error entry");
    assert_eq!(error.file_path.as_deref(), Some("tool.ts"));
    assert_ne!(cg.store().get_metadata(INDEXED_AT_COMMIT_KEY).unwrap().as_deref(), Some(head(&root).as_str()), "an errored run must not record the commit");
}

/// Take read access to `path` away; the returned closure gives it back.
fn deny_read(path: &Path) -> Box<dyn FnOnce()> {
    #[cfg(windows)]
    {
        let run = |args: &[&str]| std::process::Command::new("icacls").arg(path).args(args).output().unwrap();
        assert!(run(&["/deny", "*S-1-1-0:(R)"]).status.success(), "icacls /deny failed");
        let path = path.to_path_buf();
        Box::new(move || {
            let _ = std::process::Command::new("icacls").arg(path).args(["/remove:d", "*S-1-1-0"]).output();
        })
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
        let path = path.to_path_buf();
        Box::new(move || {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o644));
        })
    }
}

#[test]
fn an_unreadable_file_is_still_a_read_error() {
    let (_dir, root) = git_project(&[("secret.ts", "export const secret = 1;\n")]);
    let restore = deny_read(&root.join("secret.ts"));
    if fs::read(root.join("secret.ts")).is_ok() {
        restore();
        eprintln!("this account reads files it was denied: the permission case was not exercised");
        return;
    }
    let (_cg, result) = index(&root);
    restore();
    assert_eq!(result.files_errored, 1, "{:?}", result.errors);
    assert!(result.errors.iter().any(|e| e.code.as_deref() == Some("read_error") && e.file_path.as_deref() == Some("secret.ts")), "{:?}", result.errors);
}

fn transport_stream() -> Vec<u8> {
    let mut bytes = Vec::new();
    for _ in 0..(MPEG_TS_SNIFF_BYTES / 188 + 4) {
        bytes.push(0x47);
        bytes.extend_from_slice(&[0u8; 187]);
    }
    bytes
}

#[test]
fn a_video_named_ts_counts_as_skipped() {
    let (_dir, root) = git_project(&[]);
    fs::create_dir_all(root.join("testdata")).unwrap();
    fs::write(root.join("testdata").join("clip.ts"), transport_stream()).unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "-c", "core.hooksPath=", "-c", "commit.gpgSign=false", "commit", "-q", "-m", "clip"]);
    let (cg, result) = index(&root);
    assert_eq!(result.files_discovered, Some(2));
    assert_eq!((result.files_indexed, result.files_skipped), (1, 1));
    assert!(result.errors.iter().all(|e| e.code.as_deref() != Some("index_partial")), "{:?}", result.errors);
    assert_complete(&cg, &result, &root);
}

// --- single-file indexing --------------------------------------------------------------------

#[test]
fn a_single_path_with_nothing_at_it_is_skipped_not_a_read_error() {
    let (_dir, root) = git_project(&[("gone.ts", "export const gone = 1;\n"), ("sub/mod.ts", "export const mod = 1;\n")]);
    let (cg, first) = index(&root);
    assert!(first.errors.is_empty());
    fs::remove_file(root.join("gone.ts")).unwrap();
    fs::remove_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("sub"), "now a file").unwrap();
    let mut paths = vec!["gone.ts".to_string(), "sub/mod.ts".to_string(), "never-existed.ts".to_string()];
    if symlink_file(&root.join("nothing-here.ts"), &root.join("link.ts")) {
        paths.push("link.ts".to_string());
    }
    let result = cg.index_files(&paths).unwrap();
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!((result.files_errored, result.files_skipped), (0, paths.len()));
    assert!(result.success);
}

#[test]
fn a_single_folder_where_a_file_was_is_still_a_read_error() {
    let (_dir, root) = git_project(&[("tool.ts", "export const tool = 1;\n")]);
    let (cg, _) = index(&root);
    fs::remove_file(root.join("tool.ts")).unwrap();
    fs::create_dir(root.join("tool.ts")).unwrap();
    let result = cg.index_files(&["tool.ts".to_string()]).unwrap();
    assert_eq!(result.files_errored, 1, "{:?}", result.errors);
    assert!(result.errors.iter().any(|e| e.code.as_deref() == Some("read_error")));
}

// --- the name-segment vocabulary --------------------------------------------------------------

fn vocab_names(cg: &CodeGraph) -> Vec<String> {
    let conn = cg.store().conn();
    let mut stmt = conn.prepare("SELECT DISTINCT name FROM name_segment_vocab ORDER BY name").unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0)).unwrap().map(Result::unwrap).collect()
}

fn vocab_rows(cg: &CodeGraph) -> i64 {
    cg.store().conn().query_row("SELECT COUNT(*) FROM name_segment_vocab", [], |r| r.get(0)).unwrap()
}

fn plain_project() -> (tempfile::TempDir, PathBuf) {
    let temp_root = codegraph_types::paths::real_path(&std::env::temp_dir()).unwrap();
    let dir = tempfile::tempdir_in(temp_root).unwrap();
    let root = dir.path().join("project");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.ts"), "export class AlphaWidget {}\n").unwrap();
    fs::write(root.join("b.ts"), "export function betaHelper() { return 1; }\n").unwrap();
    (dir, root)
}

#[test]
fn a_repeat_index_keeps_the_vocabulary_of_unchanged_files() {
    let (_dir, root) = plain_project();
    let (mut cg, _) = index(&root);
    let before = vocab_rows(&cg);
    assert!(before > 0 && vocab_names(&cg).iter().any(|n| n == "AlphaWidget"));
    cg.index_all(&IndexOptions::default(), None, None).unwrap();
    assert_eq!(vocab_rows(&cg), before, "a second run with no edits lost vocabulary rows");
}

#[test]
fn an_edit_swaps_the_rows_of_a_renamed_symbol_and_keeps_the_rest() {
    let (_dir, root) = plain_project();
    let (mut cg, _) = index(&root);
    fs::write(root.join("a.ts"), "export class GammaPanel {}\n").unwrap();
    cg.index_all(&IndexOptions::default(), None, None).unwrap();
    let names = vocab_names(&cg);
    assert!(names.iter().any(|n| n == "GammaPanel"), "{names:?}");
    assert!(names.iter().any(|n| n == "betaHelper"), "the unchanged file's names were lost: {names:?}");
    assert!(!names.iter().any(|n| n == "AlphaWidget"), "a name no symbol carries survived: {names:?}");
}

#[test]
fn a_pruned_name_that_comes_back_in_the_same_session_is_written_again() {
    let (_dir, root) = plain_project();
    let (mut cg, _) = index(&root);
    fs::write(root.join("a.ts"), "export class GammaPanel {}\n").unwrap();
    cg.index_all(&IndexOptions::default(), None, None).unwrap();
    fs::write(root.join("a.ts"), "export class AlphaWidget {}\n").unwrap();
    cg.index_all(&IndexOptions::default(), None, None).unwrap();
    let names = vocab_names(&cg);
    assert!(names.iter().any(|n| n == "AlphaWidget"), "{names:?}");
    assert!(!names.iter().any(|n| n == "GammaPanel"), "{names:?}");
}

#[test]
fn a_cancelled_run_keeps_the_vocabulary() {
    let (_dir, root) = plain_project();
    let (mut cg, _) = index(&root);
    let before = vocab_rows(&cg);
    let cancelled = AtomicBool::new(true);
    let result = cg.index_all(&IndexOptions::default(), None, Some(&cancelled)).unwrap();
    assert!(!result.success, "the run was meant to be cancelled");
    assert_eq!(vocab_rows(&cg), before, "a cancelled run emptied the vocabulary");
}

#[test]
fn a_fresh_database_starts_with_an_empty_vocabulary() {
    let (_dir, root) = plain_project();
    let mut cg = CodeGraph::init(&root.to_string_lossy()).unwrap();
    cg.store().conn().execute("INSERT INTO name_segment_vocab (segment, name) VALUES ('stale', 'StaleOrphan')", []).unwrap();
    cg.index_all(&IndexOptions::default(), None, None).unwrap();
    let names = vocab_names(&cg);
    assert!(!names.iter().any(|n| n == "StaleOrphan"), "{names:?}");
    assert!(names.iter().any(|n| n == "AlphaWidget"), "{names:?}");
}
