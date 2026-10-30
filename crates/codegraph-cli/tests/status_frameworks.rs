//! `status` reports the frameworks a sync detected since the last full index (upstream 31ec9082,
//! #2516): files indexed before then were extracted without them, so it recommends a re-index
//! while they differ, in the text and in `--json`, and a full index clears the note.
use serde_json::Value;
use std::{path::Path, process::Command};

const EXPRESS: &str = r#"{"dependencies":{"express":"^4.18.0"}}"#;
const ROUTES: &str = "const app = express();\napp.get('/hello', function hello(req, res) {});\n";

fn run(root: &Path, args: &[&str]) -> String {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_codegraphV"))
        .args(args)
        .current_dir(root)
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("LOCALAPPDATA", home.path())
        .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{args:?}: {}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn status_json(root: &Path) -> Value {
    serde_json::from_str::<Value>(&run(root, &["status", "--json"])).unwrap()["index"].clone()
}

#[test]
fn status_recommends_a_reindex_while_the_frameworks_differ_and_a_full_index_clears_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/server.js"), ROUTES).unwrap();
    std::fs::write(dir.path().join("src/util.js"), "function util() { return 1; }\n").unwrap();
    run(dir.path(), &["init", "--yes"]);
    let index = status_json(dir.path());
    assert_eq!(index["frameworksChangedSinceIndex"], Value::Null);
    assert_eq!(index["reindexRecommended"], false);
    assert!(!run(dir.path(), &["status"]).contains("Frameworks changed"));

    // Express arrives with the next edit; `sync` notices it.
    std::fs::write(dir.path().join("package.json"), EXPRESS).unwrap();
    std::fs::write(dir.path().join("src/util.js"), "function util() { return 22; }\n").unwrap();
    run(dir.path(), &["sync", "--quiet"]);

    let index = status_json(dir.path());
    assert_eq!(index["frameworksChangedSinceIndex"], serde_json::json!({"added": ["express"], "removed": []}));
    assert_eq!(index["reindexRecommended"], true);
    let text = run(dir.path(), &["status"]);
    assert!(text.contains("Frameworks changed since the last full index (added express)"), "{text}");
    assert!(text.contains("codegraphV index"), "{text}");

    // A full index reads every file with Express, and the note goes.
    run(dir.path(), &["index"]);
    let index = status_json(dir.path());
    assert_eq!(index["frameworksChangedSinceIndex"], Value::Null);
    assert_eq!(index["reindexRecommended"], false);
    assert!(!run(dir.path(), &["status"]).contains("Frameworks changed"));
}
