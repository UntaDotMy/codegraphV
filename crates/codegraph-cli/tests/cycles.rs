//! `codegraphV cycles` (upstream 9bfdd092, #3099): circular imports between files. Exit 1 when
//! there are any is the contract that makes `codegraphV cycles || exit 1` a CI check.
use serde_json::{json, Value};
use std::{path::Path, process::Command};

fn index(files: &[(String, String)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (rel, content) in files {
        let path = dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    let init = Command::new(env!("CARGO_BIN_EXE_codegraphV"))
        .args(["init", "."])
        .current_dir(dir.path())
        .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
        .env("CODEGRAPHV_DAEMON", "0")
        .output()
        .unwrap();
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));
    dir
}

fn files(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect()
}

fn run(dir: &Path, extra: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_codegraphV"))
        .arg("cycles")
        .arg(dir)
        .args(extra)
        .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
        .env("CODEGRAPHV_DAEMON", "0")
        .output()
        .unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
}

/// `count` files in one import ring: each imports the next, the last the first.
fn ring(count: usize) -> Vec<(String, String)> {
    (0..count)
        .map(|i| {
            let next = (i + 1) % count;
            (format!("src/f{i:02}.ts"), format!("import {{ f{next} }} from './f{next:02}';\nexport function f{i}() {{ return f{next}(); }}\n"))
        })
        .collect()
}

const A: (&str, &str) = ("src/a.ts", "import { b } from './b';\nexport function a() { return b(); }\n");
const B: (&str, &str) = ("src/b.ts", "import { a } from './a';\nexport function b() { return a(); }\n");

#[test]
fn lists_the_files_of_a_circular_import_and_exits_1() {
    let dir = index(&files(&[A, B, ("src/plain.ts", "import { a } from './a';\nexport const plain = a;\n")]));
    let (code, out, _) = run(dir.path(), &[]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("Circular imports: 1 group, 2 files"), "{out}");
    assert!(out.contains("src/a.ts") && out.contains("src/b.ts"), "{out}");
    assert!(!out.contains("src/plain.ts"), "{out}");
}

#[test]
fn prints_the_whole_report_as_json_and_still_exits_1() {
    let dir = index(&files(&[A, B]));
    let (code, out, _) = run(dir.path(), &["--json"]);
    assert_eq!(code, 1);
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap(), json!({"count": 1, "cycles": [["src/a.ts", "src/b.ts"]]}));
}

#[test]
fn says_so_and_exits_0_when_the_project_has_none() {
    let dir = index(&files(&[
        ("src/a.ts", "export function a() { return 1; }\n"),
        ("src/b.ts", "import { a } from './a';\nexport function b() { return a(); }\n"),
    ]));
    let (code, out, _) = run(dir.path(), &[]);
    assert_eq!(code, 0);
    assert!(out.contains("No circular imports found"), "{out}");
    let (code, out, _) = run(dir.path(), &["--json"]);
    assert_eq!(code, 0);
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap(), json!({"count": 0, "cycles": []}));
}

#[test]
fn shows_the_first_files_of_a_long_group_and_json_all_of_them() {
    let dir = index(&ring(14));
    let (code, out, _) = run(dir.path(), &[]);
    assert_eq!(code, 1);
    assert!(out.contains("1. 14 files"), "{out}");
    assert!(out.contains("src/f11.ts") && !out.contains("src/f12.ts"), "{out}");
    assert!(out.contains("... and 2 more (--json lists them all)"), "{out}");
    let (_, out, _) = run(dir.path(), &["--json"]);
    let parsed: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed["cycles"].as_array().unwrap().len(), 1);
    assert_eq!(parsed["cycles"][0].as_array().unwrap().len(), 14);
}

#[test]
fn fails_plainly_on_a_project_with_no_index() {
    let dir = tempfile::tempdir().unwrap();
    let (code, out, err) = run(dir.path(), &[]);
    assert_eq!(code, 1);
    assert!((err + &out).contains("not indexed"));
}
