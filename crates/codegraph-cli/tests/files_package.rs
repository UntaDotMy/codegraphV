//! `codegraphV files --format package` (upstream 17d5426d, #1871): sections by owning package,
//! `--filter` applies first, the default tree and the `--json` shape are unchanged.
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

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

#[test]
fn files_lists_by_package_and_leaves_the_other_shapes_alone() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"private":true}"#);
    write(root, "apps/api/package.json", r#"{"name":"@acme/api"}"#);
    write(root, "apps/web/package.json", r#"{"name":"@acme/web"}"#);
    write(root, "apps/api/src/handler.ts", "export function handler() { return 1; }\n");
    write(root, "apps/web/src/view.ts", "export function view() { return 2; }\n");
    write(root, "scripts/tool.ts", "export function tool() { return 3; }\n");
    run(root, &["init", "--yes"]);

    let out = run(root, &["files", "--format", "package", "--no-metadata"]);
    assert!(out.contains("Files by Package (3 files, 2 packages)"), "{out}");
    assert!(out.contains("**@acme/api in apps/api (1 file)**\n- apps/api/src/handler.ts\n"), "{out}");
    assert!(out.contains("**no owning package (1 file)**\n- scripts/tool.ts\n"), "{out}");
    assert!(out.find("@acme/web").unwrap() < out.find("no owning package").unwrap(), "{out}");

    let filtered = run(root, &["files", "--format", "package", "--no-metadata", "--filter", "apps/web"]);
    assert!(filtered.contains("@acme/web in apps/web (1 file)") && !filtered.contains("@acme/api"), "{filtered}");

    assert!(!run(root, &["files"]).contains("Files by Package"));
    let plain = run(root, &["files", "--json"]);
    assert_eq!(plain, run(root, &["files", "--json", "--format", "package"]));
    assert!(plain.trim_start().starts_with('['), "{plain}");
}
