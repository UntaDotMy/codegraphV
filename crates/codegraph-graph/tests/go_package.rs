use codegraph_graph::go_package::{go_package_clause, GoPackageReader};

#[test]
fn reads_past_comments_and_build_tags() {
    assert_eq!(
        go_package_clause("//go:build linux\n/* license */\npackage gin\n"),
        Some("gin".into())
    );
    assert_eq!(
        go_package_clause("\u{FEFF}package refresh\r\n"),
        Some("refresh".into())
    );
    assert_eq!(
        go_package_clause("/* package wrong */\n// package wrong\npackage actual"),
        Some("actual".into())
    );
    assert_eq!(go_package_clause("var x = 1\npackage late"), None);
    assert_eq!(go_package_clause("// no clause"), None);
}

#[test]
fn caches_heads_and_rejects_paths_outside_the_root() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("version.go"), "package refresh\n").unwrap();
    std::fs::write(outside.path().join("outside.go"), "package escaped\n").unwrap();
    let reader = GoPackageReader::new(root.path());
    assert_eq!(reader.package_of("version.go"), Some("refresh".into()));
    std::fs::write(root.path().join("version.go"), "package changed\n").unwrap();
    assert_eq!(reader.package_of("version.go"), Some("refresh".into()));
    assert_eq!(
        GoPackageReader::new(root.path()).package_of("version.go"),
        Some("changed".into())
    );
    assert_eq!(
        reader.package_of(outside.path().join("outside.go").to_str().unwrap()),
        None
    );
    assert_eq!(reader.package_of("../outside.go"), None);
    assert_eq!(reader.package_of("missing.go"), None);
}

#[test]
fn package_beyond_the_head_budget_is_not_read() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("huge.go"),
        format!("/*{}*/\npackage late\n", " ".repeat(8192)),
    )
    .unwrap();
    assert_eq!(
        GoPackageReader::new(root.path()).package_of("huge.go"),
        None
    );
}

#[cfg(unix)]
#[test]
fn refuses_a_symlink_whose_target_is_outside_the_project() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("outside.go");
    std::fs::write(&target, "package escaped\n").unwrap();
    std::os::unix::fs::symlink(&target, root.path().join("escape.go")).unwrap();
    assert_eq!(
        GoPackageReader::new(root.path()).package_of("escape.go"),
        None
    );
}
