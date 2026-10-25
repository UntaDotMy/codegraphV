//! A project opened for a `projectPath` is released on its own idle deadline (#2087, de2b80e9).
use codegraph_core::CodeGraph;
use codegraph_mcp::ToolHandler;
use serde_json::json;
use std::time::{Duration, Instant};

fn indexed(dir: &std::path::Path, name: &str) -> String {
    let root = dir.join(name);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("app.ts"), "export function start(){return 1;}\n").unwrap();
    CodeGraph::init_and_index(root.to_str().unwrap(), None).unwrap().close();
    root.to_string_lossy().into_owned()
}

fn status(handler: &mut ToolHandler, root: &str) {
    let result = handler.call("codegraphV_status", &json!({"projectPath": root}));
    assert!(!result.is_error);
}

#[test]
fn each_project_goes_on_its_own_deadline_and_reopens_on_the_next_call() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (indexed(dir.path(), "a"), indexed(dir.path(), "b"));
    let mut handler = ToolHandler::new(None, false);
    handler.idle_timeout = Some(Duration::from_secs(60));
    let before_a = Instant::now();
    status(&mut handler, &a);
    let between = Instant::now();
    status(&mut handler, &b);
    assert_eq!(handler.open_project_count(), 2);
    // The oldest project's deadline is the next one.
    let due = handler.next_release_due().unwrap();
    assert!(due >= before_a + Duration::from_secs(60) && due <= between + Duration::from_secs(60));
    // At A's deadline only A goes; B keeps its own, later deadline.
    let released = handler.release_idle(due);
    assert_eq!(released.len(), 1);
    assert_eq!(handler.open_project_count(), 1);
    assert!(handler.next_release_due().unwrap() >= between + Duration::from_secs(60));
    assert!(handler.release_idle(Instant::now()).is_empty());
    // A released project answers again on the next call.
    status(&mut handler, &a);
    assert_eq!(handler.open_project_count(), 2);
    assert_eq!(handler.release_idle(Instant::now() + Duration::from_secs(120)).len(), 2);
    assert_eq!(handler.open_project_count(), 0);
}

#[test]
fn the_default_project_stays_open() {
    let dir = tempfile::tempdir().unwrap();
    let (main, other) = (indexed(dir.path(), "main"), indexed(dir.path(), "other"));
    let mut handler = ToolHandler::new(Some(main.clone()), false);
    handler.idle_timeout = Some(Duration::from_secs(1));
    status(&mut handler, &main);
    status(&mut handler, &other);
    assert_eq!(handler.release_idle(Instant::now() + Duration::from_secs(10)).len(), 1);
    assert_eq!(handler.open_project_count(), 1);
    assert!(handler.next_release_due().is_none());
}

#[test]
fn a_zero_timeout_never_releases() {
    let dir = tempfile::tempdir().unwrap();
    let a = indexed(dir.path(), "a");
    let mut handler = ToolHandler::new(None, false);
    handler.idle_timeout = None;
    status(&mut handler, &a);
    assert!(handler.next_release_due().is_none());
    assert!(handler.release_idle(Instant::now() + Duration::from_secs(86_400)).is_empty());
    assert_eq!(handler.open_project_count(), 1);
}
