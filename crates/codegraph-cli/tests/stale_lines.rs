//! Upstream #3260: `callers`, `callees` and `impact` say when a file in the answer changed on
//! disk after the last sync; with `--json` the notice goes to stderr so stdout stays JSON.
use std::process::Command;

fn run(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_codegraphV"))
        .args(args)
        .current_dir(root)
        .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
        .env("CODEGRAPHV_DAEMON", "0")
        .output()
        .unwrap()
}

#[test]
fn a_drifted_file_is_flagged_and_json_stays_parseable() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let source = "fn plain_helper() -> u32 { 1 }\nfn direct() -> u32 { plain_helper() }\n";
    std::fs::write(root.join("main.rs"), source).unwrap();
    assert!(run(root, &["init", "."]).status.success());
    std::fs::write(root.join("main.rs"), format!("// a\n// b\n// c\n{source}")).unwrap();
    for args in [&["callers", "plain_helper"][..], &["callees", "direct"], &["impact", "plain_helper"]] {
        let out = run(root, args);
        assert!(out.status.success(), "{args:?}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("Changed on disk after the last index sync: main.rs"), "{args:?}");
        let mut json_args = args.to_vec();
        json_args.push("--json");
        let out = run(root, &json_args);
        assert!(out.status.success(), "{json_args:?}");
        serde_json::from_slice::<serde_json::Value>(&out.stdout).expect("stdout is JSON");
        assert!(String::from_utf8_lossy(&out.stderr).contains("Changed on disk after the last index sync: main.rs"), "{json_args:?}");
    }
}
