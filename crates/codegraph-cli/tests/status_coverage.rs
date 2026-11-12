//! `status` shows files parsed beside files found, and `--strict` gates on a usable index
//! (upstream 7889da66, #1658).
use codegraph_core::CodeGraph;
use serde_json::Value;
use std::{path::Path, process::Command};

const HAS_SYMBOL: &str = "export function hasSymbol() { return 1; }\n";
const NO_SYMBOL: &str = "// nothing but a comment\n";

struct Run {
    code: i32,
    stdout: String,
}

fn status(root: &Path, args: &[&str]) -> Run {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_codegraphV"))
        .arg("status")
        .arg(root)
        .args(args)
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("LOCALAPPDATA", home.path())
        .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
        .env("CODEGRAPHV_NO_DAEMON", "1")
        .output()
        .unwrap();
    Run { code: out.status.code().unwrap_or(-1), stdout: String::from_utf8_lossy(&out.stdout).into_owned() }
}

fn status_json(root: &Path, args: &[&str]) -> (i32, Value) {
    let mut all = vec!["--json"];
    all.extend_from_slice(args);
    let run = status(root, &all);
    (run.code, serde_json::from_str(&run.stdout).unwrap())
}

fn indexed(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, content) in files {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }
    CodeGraph::init_and_index(&dir.path().to_string_lossy(), None).unwrap().close();
    dir
}

fn open(root: &Path) -> CodeGraph {
    CodeGraph::open(&root.to_string_lossy(), Default::default()).unwrap()
}

fn language_row<'a>(stdout: &'a str, language: &str) -> Option<&'a str> {
    stdout.lines().find(|l| l.starts_with(&format!("  {language} ")))
}

#[test]
fn coverage_counts_files_with_a_symbol_beyond_their_file_node() {
    let dir = indexed(&[("a.ts", HAS_SYMBOL), ("b.ts", NO_SYMBOL)]);
    let coverage = open(dir.path()).get_language_coverage().unwrap();
    assert_eq!(coverage.parsed_files_by_language.get("typescript"), Some(&1));
    assert!(coverage.unparsed_languages.is_empty());
}

#[test]
fn coverage_names_a_language_none_of_whose_files_yielded_a_symbol() {
    let dir = indexed(&[("a.ts", HAS_SYMBOL), ("stub.lua", "-- nothing here\n")]);
    let coverage = open(dir.path()).get_language_coverage().unwrap();
    assert_eq!(coverage.parsed_files_by_language.get("lua"), Some(&0));
    assert_eq!(coverage.parsed_files_by_language.get("typescript"), Some(&1));
    assert_eq!(coverage.unparsed_languages, vec!["lua".to_string()]);
}

#[test]
fn coverage_leaves_out_file_level_languages_and_asks_the_nodes_when_the_count_cannot_say() {
    let dir = indexed(&[
        ("a.ts", HAS_SYMBOL),
        ("config.yaml", "key: value\n"),
        ("application.properties", "key=value\n"),
        ("pom.xml", "<project></project>\n"),
    ]);
    let cg = open(dir.path());
    let coverage = cg.get_language_coverage().unwrap();
    assert_eq!(coverage.parsed_files_by_language.keys().collect::<Vec<_>>(), vec!["typescript"]);
    assert!(coverage.unparsed_languages.is_empty());
    cg.store().conn().execute("UPDATE files SET node_count = 1", []).unwrap();
    assert_eq!(cg.get_language_coverage().unwrap().parsed_files_by_language.get("typescript"), Some(&1));
}

#[test]
fn coverage_counts_a_file_over_the_size_limit_as_found_not_parsed() {
    let huge = format!("// {}\n{HAS_SYMBOL}", "x".repeat(codegraph_types::limits::max_source_file_bytes() as usize));
    let dir = indexed(&[("a.ts", HAS_SYMBOL), ("huge.ts", &huge)]);
    let cg = open(dir.path());
    assert_eq!(cg.get_stats().unwrap().files_by_language.iter().find(|(l, _)| l.as_str() == "typescript").map(|(_, n)| *n), Some(2));
    assert_eq!(cg.get_language_coverage().unwrap().parsed_files_by_language.get("typescript"), Some(&1));
}

#[test]
fn status_text_adds_the_count_only_when_the_two_differ() {
    let healthy = indexed(&[("a.ts", HAS_SYMBOL), ("b.ts", HAS_SYMBOL), ("config.yaml", "key: value\n")]);
    let text = status(healthy.path(), &[]).stdout;
    assert_eq!(language_row(&text, "typescript"), Some("  typescript      2"), "{text}");
    assert_eq!(language_row(&text, "yaml"), Some("  yaml            1"), "{text}");
    assert!(!text.contains("No symbols came from"), "{text}");

    let mixed = indexed(&[("a.ts", HAS_SYMBOL), ("b.ts", NO_SYMBOL)]);
    let text = status(mixed.path(), &[]).stdout;
    assert_eq!(language_row(&text, "typescript"), Some("  typescript      2  (1 with symbols)"), "{text}");
}

#[test]
fn status_names_a_language_that_yielded_nothing_without_failing() {
    let dir = indexed(&[("a.ts", HAS_SYMBOL), ("stub.lua", "-- nothing here\n")]);
    let run = status(dir.path(), &[]);
    assert_eq!(run.code, 0);
    assert_eq!(language_row(&run.stdout, "lua"), Some("  lua             1  (0 with symbols)"), "{}", run.stdout);
    assert!(run.stdout.contains("No symbols came from any lua file") && run.stdout.contains("stub.lua"), "{}", run.stdout);
}

#[test]
fn status_json_adds_the_coverage_fields() {
    let dir = indexed(&[("a.ts", HAS_SYMBOL), ("stub.lua", "-- nothing here\n")]);
    let (code, json) = status_json(dir.path(), &[]);
    assert_eq!(code, 0);
    assert_eq!(json["parsedFilesByLanguage"], serde_json::json!({"lua": 0, "typescript": 1}));
    assert_eq!(json["unparsedLanguages"], serde_json::json!(["lua"]));
    assert!(json.get("filesByLanguage").is_some(), "{json}");
}

#[test]
fn strict_passes_a_healthy_index_with_the_same_text() {
    let dir = indexed(&[("a.ts", HAS_SYMBOL)]);
    let strict = status(dir.path(), &["--strict"]);
    assert_eq!(strict.code, 0, "{}", strict.stdout);
    assert_eq!(strict.stdout, status(dir.path(), &[]).stdout);
    assert_eq!(status_json(dir.path(), &["--strict"]).0, 0);
}

#[test]
fn strict_fails_a_language_that_yielded_nothing_in_either_mode() {
    let dir = indexed(&[("a.ts", HAS_SYMBOL), ("stub.lua", "-- nothing here\n")]);
    assert_eq!(status(dir.path(), &["--strict"]).code, 1);
    assert_eq!(status_json(dir.path(), &["--strict"]).0, 1);
}

#[test]
fn strict_does_not_fail_one_file_without_symbols_in_a_language_that_has_some() {
    let dir = indexed(&[("a.ts", HAS_SYMBOL), ("b.ts", NO_SYMBOL)]);
    assert_eq!(status(dir.path(), &["--strict"]).code, 0);
}

#[test]
fn strict_fails_with_no_index_in_either_mode() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(status(dir.path(), &[]).code, 0);
    assert_eq!(status(dir.path(), &["--strict"]).code, 1);
    assert_eq!(status_json(dir.path(), &["--strict"]).0, 1);
}

#[test]
fn strict_fails_an_unfinished_index() {
    for state in ["indexing", "partial", "failed"] {
        let dir = indexed(&[("a.ts", HAS_SYMBOL)]);
        let cg = open(dir.path());
        cg.store().set_metadata("index_state", state).unwrap();
        cg.close();
        assert_eq!(status(dir.path(), &[]).code, 0, "{state}");
        assert_eq!(status(dir.path(), &["--strict"]).code, 1, "{state}");
    }
}

#[test]
fn strict_fails_files_the_index_does_not_have_and_a_file_that_lost_its_symbols() {
    let dir = indexed(&[("a.ts", HAS_SYMBOL)]);
    std::fs::write(dir.path().join("b.ts"), HAS_SYMBOL).unwrap();
    assert_eq!(status(dir.path(), &[]).code, 0);
    assert_eq!(status(dir.path(), &["--strict"]).code, 1);

    let dir = indexed(&[("a.ts", HAS_SYMBOL)]);
    let cg = open(dir.path());
    cg.store().conn().execute("DELETE FROM nodes WHERE file_path = 'a.ts'", []).unwrap();
    cg.store().conn().execute("UPDATE files SET node_count = 0 WHERE path = 'a.ts'", []).unwrap();
    cg.close();
    assert_eq!(status(dir.path(), &[]).code, 0);
    assert_eq!(status(dir.path(), &["--strict"]).code, 1);
}

#[test]
fn strict_does_not_fail_one_file_that_cannot_be_parsed() {
    let dir = indexed(&[("a.ts", HAS_SYMBOL), ("bad.ts", "@@@ ((( }}} class 1 = ;\n")]);
    let run = status(dir.path(), &["--strict"]);
    assert_eq!(run.code, 0, "{}", run.stdout);
}
