//! `files --json` marks test files, `--tests`/`--no-tests` filter every format, and the node file
//! view names a test file (upstream b6ea6179, #1877).
use serde_json::Value;
use std::{path::Path, process::Command};

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
    assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/app.ts"), "export function app() { return 1; }\n").unwrap();
    std::fs::write(dir.path().join("src/app.test.ts"), "import { app } from \"./app\";\napp();\n").unwrap();
    run(dir.path(), &["init", "--yes"]);
    dir
}

fn paths(json: &str) -> Vec<String> {
    serde_json::from_str::<Vec<Value>>(json).unwrap().iter().map(|f| f["path"].as_str().unwrap().to_string()).collect()
}

#[test]
fn files_json_reports_is_test_and_filters_by_it() {
    let dir = project();
    let all: Vec<Value> = serde_json::from_str(&run(dir.path(), &["files", "--json"])).unwrap();
    let is_test = |p: &str| all.iter().find(|f| f["path"] == p).unwrap()["isTest"].clone();
    assert_eq!(is_test("src/app.ts"), Value::Bool(false));
    assert_eq!(is_test("src/app.test.ts"), Value::Bool(true));
    assert_eq!(paths(&run(dir.path(), &["files", "--json", "--tests"])), vec!["src/app.test.ts"]);
    assert_eq!(paths(&run(dir.path(), &["files", "--json", "--no-tests"])), vec!["src/app.ts"]);
}

#[test]
fn the_filter_holds_in_every_output_format() {
    let dir = project();
    for format in ["tree", "flat", "grouped", "package"] {
        let tests = run(dir.path(), &["files", "--tests", "--format", format]);
        assert!(tests.contains("app.test.ts"), "{format}: {tests}");
        assert!(!tests.contains("app.ts "), "{format}: {tests}");
        assert!(!tests.lines().any(|l| l.trim_end().ends_with("app.ts") || l.contains("app.ts (")), "{format}: {tests}");
        let source = run(dir.path(), &["files", "--no-tests", "--format", format]);
        assert!(source.contains("app.ts"), "{format}: {source}");
        assert!(!source.contains("app.test.ts"), "{format}: {source}");
    }
}

#[test]
fn says_so_when_a_filter_leaves_no_files() {
    let dir = project();
    assert!(run(dir.path(), &["files", "--tests", "--pattern", "src/app.ts"]).contains("No files found"));
}

#[test]
fn the_node_file_header_marks_a_test_file() {
    let dir = project();
    let test = run(dir.path(), &["node", "-f", "src/app.test.ts", "--symbols-only"]);
    assert!(test.contains("**src/app.test.ts** · test file"), "{test}");
    let source = run(dir.path(), &["node", "-f", "src/app.ts", "--symbols-only"]);
    assert!(!source.contains("test file"), "{source}");
}
