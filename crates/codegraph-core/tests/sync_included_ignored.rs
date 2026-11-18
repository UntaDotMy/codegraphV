//! Upstream #3086: source that `codegraph.json` `include` forces in despite `.gitignore` is
//! indexed, so its edits, additions and deletions are pending changes too. Git status never
//! reports ignored paths, so the git fast path alone missed every one of them.
use codegraph_core::CodeGraph;
use std::path::Path;
use std::process::Command;

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

#[test]
fn included_gitignored_source_changes_are_pending() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("local")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join(".gitignore"), "local/\n").unwrap();
    std::fs::write(root.join("codegraph.json"), r#"{"include":["local/"]}"#).unwrap();
    std::fs::write(root.join("local/A.java"), "class A { void a() {} }\n").unwrap();
    std::fs::write(root.join("local/Gone.java"), "class Gone {}\n").unwrap();
    std::fs::write(root.join("src/T.java"), "class T {}\n").unwrap();
    git(root, &["init", "-q"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);

    let cg = CodeGraph::init_and_index(root.to_str().unwrap(), None).unwrap();
    assert!(cg.get_file("local/A.java").unwrap().is_some(), "the include indexes it");
    let none = cg.get_changed_files().unwrap();
    assert!(none.added.is_empty() && none.modified.is_empty() && none.removed.is_empty(), "{none:?}");

    std::fs::write(root.join("local/A.java"), "class A { void a() {} void b() {} }\n").unwrap();
    std::fs::write(root.join("local/B.java"), "class B {}\n").unwrap();
    std::fs::remove_file(root.join("local/Gone.java")).unwrap();
    let changed = cg.get_changed_files().unwrap();
    assert_eq!(sorted(changed.modified), vec!["local/A.java".to_string()]);
    assert_eq!(sorted(changed.added), vec!["local/B.java".to_string()]);
    assert_eq!(sorted(changed.removed), vec!["local/Gone.java".to_string()]);
}
