use codegraph_context::ContextBuilder;
use codegraph_db::Store;
use codegraph_types::*;

fn node(id: &str, path: &str, language: Language, kind: NodeKind) -> Node {
    Node::new(id, kind, id, id, path, language, 1, 2, 0, 0, 0)
}

#[test]
fn config_context_contains_keys_and_never_values() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    std::fs::write(
        dir.path().join("application.properties"),
        "db.password=top-secret\n",
    )
    .unwrap();
    let mut n = node(
        "password",
        "application.properties",
        Language::Properties,
        NodeKind::Constant,
    );
    n.signature = Some("db.password".into());
    store.insert_node(&n).unwrap();
    let b = ContextBuilder::new(dir.path().to_str().unwrap(), &store);
    assert_eq!(
        b.get_code("password").unwrap().as_deref(),
        Some("db.password")
    );
    let c = b
        .build_task_context(
            &TaskInput::Text("password".into()),
            &BuildContextOptions::default(),
        )
        .unwrap();
    assert!(!c.code_blocks.is_empty());
    for block in c.code_blocks {
        assert!(!block.content.contains("top-secret"));
    }
    drop(b);
    store.close().unwrap();
}

#[test]
fn refuses_lexical_escape_and_returns_none_for_missing_source() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(parent.path().join("outside.ts"), "secret\n").unwrap();
    let store = Store::initialize(root.join("codegraph.db")).unwrap();
    store
        .insert_nodes(&[
            node(
                "escape",
                "../outside.ts",
                Language::TypeScript,
                NodeKind::Function,
            ),
            node(
                "missing",
                "missing.ts",
                Language::TypeScript,
                NodeKind::Function,
            ),
        ])
        .unwrap();
    let b = ContextBuilder::new(root.to_str().unwrap(), &store);
    assert_eq!(b.get_code("escape").unwrap(), None);
    assert_eq!(b.get_code("missing").unwrap(), None);
    assert_eq!(b.get_code("no-node").unwrap(), None);
    drop(b);
    store.close().unwrap();
}

#[test]
fn source_positions_keep_crlf_and_utf16_size_accounting() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("codegraph.db")).unwrap();
    std::fs::write(
        dir.path().join("unicode.ts"),
        "// 🚀\r\nfunction rocket() {}\r\n",
    )
    .unwrap();
    store
        .insert_node(&node(
            "rocket",
            "unicode.ts",
            Language::TypeScript,
            NodeKind::Function,
        ))
        .unwrap();
    let b = ContextBuilder::new(dir.path().to_str().unwrap(), &store);
    assert_eq!(
        b.get_code("rocket").unwrap().as_deref(),
        Some("// 🚀\r\nfunction rocket() {}\r")
    );
    let c = b
        .build_task_context(
            &TaskInput::Text("rocket".into()),
            &BuildContextOptions {
                max_code_block_size: Some(8),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(c.code_blocks[0].content.ends_with("\n... (truncated) ..."));
    assert_eq!(
        c.stats.total_code_size,
        c.code_blocks[0].content.encode_utf16().count()
    );
    drop(b);
    store.close().unwrap();
}

#[cfg(unix)]
#[test]
fn symlinks_outside_project_are_refused_and_internal_symlinks_work() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(parent.path().join("outside.ts"), "secret").unwrap();
    std::fs::write(root.join("inside.ts"), "allowed").unwrap();
    std::os::unix::fs::symlink(parent.path().join("outside.ts"), root.join("escape.ts")).unwrap();
    std::os::unix::fs::symlink(root.join("inside.ts"), root.join("link.ts")).unwrap();
    let store = Store::initialize(root.join("codegraph.db")).unwrap();
    store
        .insert_nodes(&[
            node(
                "escape",
                "escape.ts",
                Language::TypeScript,
                NodeKind::Function,
            ),
            node(
                "inside",
                "link.ts",
                Language::TypeScript,
                NodeKind::Function,
            ),
        ])
        .unwrap();
    let b = ContextBuilder::new(root.to_str().unwrap(), &store);
    assert_eq!(b.get_code("escape").unwrap(), None);
    assert_eq!(b.get_code("inside").unwrap().as_deref(), Some("allowed"));
    drop(b);
    store.close().unwrap();
}

#[cfg(windows)]
#[test]
fn windows_junction_escape_is_refused() {
    use std::os::windows::process::CommandExt;
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("project");
    let outside = parent.path().join("outside");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret.ts"), "secret").unwrap();
    let link = root.join("escape");
    let status = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(&link)
        .arg(&outside)
        .creation_flags(0x08000000)
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let store = Store::initialize(root.join("codegraph.db")).unwrap();
    store
        .insert_node(&node(
            "escape",
            "escape/secret.ts",
            Language::TypeScript,
            NodeKind::Function,
        ))
        .unwrap();
    let b = ContextBuilder::new(root.to_str().unwrap(), &store);
    assert_eq!(b.get_code("escape").unwrap(), None);
    drop(b);
    store.close().unwrap();
    std::fs::remove_dir(link).unwrap();
}
