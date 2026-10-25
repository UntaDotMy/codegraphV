//! DEV-105: the source-size limit is 10 MB by default, `CODEGRAPHV_MAX_FILE_MB` changes it
//! (`0` removes it), and a file crossing the limit in either direction is picked up by `sync`.
use serde_json::Value;
use std::{path::Path, process::Command};

/// A hand-written-looking TypeScript file of about `bytes` bytes holding `marker`.
fn big_source(marker: &str, bytes: usize) -> String {
    let mut text = format!("export function {marker}() {{ return 1; }}\n");
    let mut i = 0;
    while text.len() < bytes {
        text.push_str(&format!("export function filler{i:06}(a: number) {{ return a + {i}; }}\n"));
        i += 1;
    }
    text
}

fn project(file_bytes: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("big.ts"), big_source("limitMarkerFn", file_bytes)).unwrap();
    std::fs::write(dir.path().join("small.ts"), "export function smallOne() { return 2; }\n").unwrap();
    dir
}

/// Run the CLI in `root`; `limit_mb` is the value of CODEGRAPHV_MAX_FILE_MB, or unset for `None`.
fn run(root: &Path, limit_mb: Option<&str>, args: &[&str]) -> String {
    let home = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_codegraphV"));
    command
        .args(args)
        .current_dir(root)
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("LOCALAPPDATA", home.path())
        .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
        .env_remove("CODEGRAPHV_MAX_FILE_MB");
    if let Some(mb) = limit_mb {
        command.env("CODEGRAPHV_MAX_FILE_MB", mb);
    }
    let out = command.output().unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn init(root: &Path, limit_mb: Option<&str>) -> String {
    run(root, limit_mb, &["init", "--yes"])
}

/// Is `limitMarkerFn` in the index?
fn marker_indexed(root: &Path, limit_mb: Option<&str>) -> bool {
    let found: Value = serde_json::from_str(&run(root, limit_mb, &["query", "limitMarkerFn", "--json"])).unwrap();
    found.as_array().is_some_and(|hits| hits.iter().any(|h| h["node"]["name"] == "limitMarkerFn"))
}

#[test]
fn a_file_over_one_megabyte_is_indexed_by_default() {
    let dir = project(1_400_000);
    let out = init(dir.path(), None);
    assert!(!out.contains("size_exceeded"), "default limit must not skip a 1.4 MB file: {out}");
    assert!(marker_indexed(dir.path(), None));
}

#[test]
fn the_env_var_sets_the_limit_and_zero_removes_it() {
    let dir = project(600_000);
    let skipped = init(dir.path(), Some("0.5"));
    assert!(skipped.contains("size_exceeded"), "a 0.6 MB file is over 0.5 MB: {skipped}");
    assert!(!marker_indexed(dir.path(), Some("0.5")));

    let again = tempfile::tempdir().unwrap();
    std::fs::copy(dir.path().join("big.ts"), again.path().join("big.ts")).unwrap();
    let out = init(again.path(), Some("0"));
    assert!(!out.contains("size_exceeded"), "0 means no limit: {out}");
    assert!(marker_indexed(again.path(), Some("0")));
}

#[test]
fn sync_picks_up_a_file_that_crosses_the_limit_in_either_direction() {
    let dir = project(600_000);
    // Indexed while over the limit: skipped.
    init(dir.path(), Some("0.5"));
    assert!(!marker_indexed(dir.path(), Some("0.5")));
    // The limit is raised (an upgrade): the unchanged file is now within it, so sync indexes it.
    run(dir.path(), None, &["sync", "--quiet"]);
    assert!(marker_indexed(dir.path(), None), "raising the limit must index the skipped file");
    // The limit is lowered again: the file is over it, so sync drops its symbols.
    run(dir.path(), Some("0.5"), &["sync", "--quiet"]);
    assert!(!marker_indexed(dir.path(), Some("0.5")), "lowering the limit must drop the file");
}
