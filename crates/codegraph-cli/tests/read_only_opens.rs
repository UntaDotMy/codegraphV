//! Read commands, `status` and the prompt hook open the index read-only (upstream 01873666 and
//! 475e56b2). A read-write open runs the writer's repairs: while `codegraphV index` is inside a
//! bulk-load window they recreate the secondary indexes and FTS triggers the indexer dropped, and
//! the indexer's next write, whose transaction had already read, fails with SQLITE_BUSY_SNAPSHOT.
//! The window is simulated with a writer connection in this process; each command runs in its own.
use codegraph_db::{OpenOptions, Store};
use serde_json::Value;
use std::{
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};

const AUTH: &str = "export function parseToken(raw: string) { return raw.trim(); }\nexport function login(user: string) { return parseToken(user); }\n";

fn cli(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_codegraphV"))
        .args(args)
        .current_dir(root)
        .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("auth.ts"), AUTH).unwrap();
    let init = cli(dir.path(), &["init", "--yes"]);
    assert!(init.status.success(), "{}", text(&init.stderr));
    dir
}

fn writer(root: &Path) -> Store {
    Store::open(root.join(".codegraphV").join("codegraph.db"), OpenOptions { read_only: false }).unwrap()
}

/// Every index and trigger the database holds, sorted: what a repair would change.
fn schema_objects(store: &Store) -> Vec<String> {
    let mut stmt = store.conn().prepare("SELECT type || ' ' || name FROM sqlite_master WHERE type IN ('index', 'trigger') ORDER BY 1").unwrap();
    let rows = stmt.query_map([], |row| row.get::<_, String>(0)).unwrap();
    rows.map(|r| r.unwrap()).collect()
}

/// Another live process holds the index lock for a full index: this test process.
fn pretend_to_rebuild(root: &Path, store: &Store) {
    store.set_metadata("index_state", "indexing").unwrap();
    std::fs::write(root.join(".codegraphV").join("codegraph.lock"), std::process::id().to_string()).unwrap();
}

fn finish_rebuild(root: &Path, store: &Store) {
    store.set_metadata("index_state", "complete").unwrap();
    std::fs::remove_file(root.join(".codegraphV").join("codegraph.lock")).unwrap();
}

const READ_COMMANDS: &[&[&str]] = &[
    &["status"],
    &["status", "--json"],
    &["query", "parseToken"],
    &["files"],
    &["files", "--json"],
    &["node", "parseToken"],
    &["explore", "parseToken", "login"],
    &["callers", "parseToken"],
    &["callees", "login"],
    &["impact", "parseToken"],
    &["context", "how does login work"],
    &["affected", "auth.ts"],
];

#[test]
fn read_commands_leave_a_bulk_load_window_alone_and_the_writers_next_write_succeeds() {
    let dir = project();
    let store = writer(dir.path());
    // The indexer's bulk window: secondary indexes and FTS triggers dropped.
    store.begin_bulk_parse_load().unwrap();
    store.begin_bulk_node_load().unwrap();
    let window = schema_objects(&store);
    // The indexer's transaction has read, and writes next.
    store.conn().execute_batch("BEGIN; SELECT count(*) FROM edges;").unwrap();

    for args in READ_COMMANDS {
        let out = cli(dir.path(), args);
        assert!(out.status.success(), "{args:?}: {}\n{}", text(&out.stdout), text(&out.stderr));
    }

    assert_eq!(schema_objects(&store), window, "a read command ran the writer's repairs inside the window");
    store.set_metadata("probe", "1").expect("the writer's next write must not fail with SQLITE_BUSY_SNAPSHOT");
    store.conn().execute_batch("COMMIT").unwrap();
}

#[test]
fn read_commands_say_when_another_process_is_rebuilding() {
    let dir = project();
    let store = writer(dir.path());

    let at_rest = cli(dir.path(), &["query", "parseToken", "--json"]);
    assert!(at_rest.status.success());
    assert!(!text(&at_rest.stderr).to_lowercase().contains("rebuild"), "{}", text(&at_rest.stderr));
    let status = serde_json::from_str::<Value>(&text(&cli(dir.path(), &["status", "--json"]).stdout)).unwrap();
    assert!(status.get("rebuildInProgress").is_none(), "an index at rest gains no key: {status}");

    pretend_to_rebuild(dir.path(), &store);
    for args in [&["query", "parseToken", "--json"][..], &["files", "--json"], &["callers", "parseToken", "--json"], &["explore", "parseToken"], &["node", "parseToken"]] {
        let out = cli(dir.path(), args);
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        let lines: Vec<String> = text(&out.stderr).lines().filter(|l| l.to_lowercase().contains("rebuild")).map(String::from).collect();
        assert_eq!(lines.len(), 1, "{args:?} must say it once on stderr: {}", text(&out.stderr));
        if args.contains(&"--json") {
            serde_json::from_str::<Value>(&text(&out.stdout)).unwrap_or_else(|e| panic!("{args:?}: stdout must stay JSON: {e}"));
        }
    }

    let status = serde_json::from_str::<Value>(&text(&cli(dir.path(), &["status", "--json"]).stdout)).unwrap();
    assert_eq!(status["rebuildInProgress"], true);
    assert_eq!(status["index"]["reindexRecommended"], false, "the advice for an index at rest does not fit a live rebuild");
    let words = text(&cli(dir.path(), &["status"]).stdout);
    assert!(words.to_lowercase().contains("rebuild of this index is in progress"), "{words}");

    finish_rebuild(dir.path(), &store);
    assert!(!text(&cli(dir.path(), &["query", "parseToken"]).stderr).to_lowercase().contains("rebuild"));
}

fn hook(cwd: &Path, prompt: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_codegraphV"))
        .arg("prompt-hook")
        .current_dir(cwd)
        .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
        .env_remove("CODEGRAPHV_NO_PROMPT_HOOK")
        .env_remove("CODEGRAPHV_PROMPT_HOOK")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let payload = serde_json::json!({ "prompt": prompt, "cwd": cwd }).to_string();
    child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

// Prose with no verified identifier: the hook's vocabulary tier.
const PROSE: &str = "please fix the token parse handling";

#[test]
fn the_hook_fills_an_empty_vocabulary_through_its_own_writer_and_stays_out_of_a_rebuild() {
    let dir = project();
    let store = writer(dir.path());

    let at_rest = hook(dir.path(), PROSE);
    assert!(at_rest.status.success());
    let injected = text(&at_rest.stdout);
    assert!(injected.contains("found indexed symbols matching this prompt"), "the vocabulary tier answers at rest: {injected:?}");

    // An index built before the vocabulary existed: empty, and the hook's one write fills it.
    store.conn().execute_batch("DELETE FROM name_segment_vocab").unwrap();
    assert!(store.is_name_segment_vocab_empty().unwrap());
    let healed = hook(dir.path(), PROSE);
    assert!(text(&healed.stdout).contains("found indexed symbols matching this prompt"), "{:?} / {}", text(&healed.stdout), text(&healed.stderr));
    assert!(!store.is_name_segment_vocab_empty().unwrap(), "the hook fills it through a writer of its own");

    // During a rebuild the index is half-built: the hook says nothing, and writes nothing.
    store.conn().execute_batch("DELETE FROM name_segment_vocab").unwrap();
    pretend_to_rebuild(dir.path(), &store);
    for prompt in [PROSE, "how does login work?"] {
        let during = hook(dir.path(), prompt);
        assert!(during.status.success());
        assert!(during.stdout.is_empty(), "{prompt:?}: {}", text(&during.stdout));
    }
    assert!(store.is_name_segment_vocab_empty().unwrap(), "the hook must not write during a rebuild");
}

#[test]
fn the_hook_opens_read_only_inside_a_bulk_load_window() {
    let dir = project();
    let store = writer(dir.path());
    store.begin_bulk_parse_load().unwrap();
    store.begin_bulk_node_load().unwrap();
    let window = schema_objects(&store);
    store.conn().execute_batch("BEGIN; SELECT count(*) FROM edges;").unwrap();

    let out = hook(dir.path(), "how does login work?");
    assert!(out.status.success());
    assert_eq!(schema_objects(&store), window, "the hook ran the writer's repairs inside the window");
    store.set_metadata("probe", "1").expect("the writer's next write must not fail with SQLITE_BUSY_SNAPSHOT");
    store.conn().execute_batch("COMMIT").unwrap();
}
