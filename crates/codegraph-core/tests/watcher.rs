use codegraph_core::{CodeGraph, NodeKind, OpenOptions, WatchOptions};
use std::{
    fs,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
fn fixture() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    fs::create_dir(d.path().join("src")).unwrap();
    fs::write(
        d.path().join("src/a.ts"),
        "export function alpha(){return 1;}\n",
    )
    .unwrap();
    d
}
fn wait(mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(
            start.elapsed() < Duration::from_secs(12),
            "watch sync timed out"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn native_auto_sync_updates_both_existing_cached_nodes_and_fresh_connections() {
    let d = fixture();
    let root = d.path().to_str().unwrap();
    let mut cg = CodeGraph::init_and_index(root, None).unwrap();
    let alpha = cg
        .get_nodes_by_name("alpha")
        .unwrap()
        .into_iter()
        .find(|n| n.kind == NodeKind::Function)
        .unwrap();
    assert!(!cg
        .get_node(&alpha.id)
        .unwrap()
        .unwrap()
        .signature
        .unwrap_or_default()
        .contains("value"));
    let count = Arc::new(AtomicUsize::new(0));
    let completed = count.clone();
    assert!(cg
        .watch(WatchOptions {
            debounce_ms: 100,
            on_sync_complete: Some(Arc::new(move |_| {
                completed.fetch_add(1, Ordering::SeqCst);
            })),
            ..Default::default()
        })
        .unwrap());
    cg.wait_until_watcher_ready(Duration::from_secs(3)).unwrap();
    fs::write(d.path().join("src/a.ts"),"export function alpha(value: number){return value;}\nexport function beta(){return alpha(2);}\n").unwrap();
    wait(|| count.load(Ordering::SeqCst) > 0);
    assert!(cg
        .get_nodes_by_name("beta")
        .unwrap()
        .iter()
        .any(|n| n.kind == NodeKind::Function));
    let current = cg.get_node(&alpha.id).unwrap().unwrap();
    assert!(
        current.signature.as_ref().unwrap().contains("value"),
        "{:?}",
        current.signature
    );
    assert!(cg
        .get_code(&alpha.id)
        .unwrap()
        .unwrap()
        .contains("value: number"));
    let fresh = CodeGraph::open(
        root,
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!fresh.get_nodes_by_name("beta").unwrap().is_empty());
    fresh.close();
    fs::remove_file(d.path().join("src/a.ts")).unwrap();
    wait(|| cg.get_nodes_by_file("src/a.ts").unwrap().is_empty());
    cg.unwatch();
    assert!(!cg.is_watching());
    cg.close();
}

#[test]
fn scoped_sync_followed_by_config_change_removes_excluded_source() {
    let d = fixture();
    let root = d.path().to_str().unwrap();
    let mut cg = CodeGraph::init_and_index(root, None).unwrap();
    assert!(cg
        .watch(WatchOptions {
            inert: true,
            debounce_ms: 100,
            ..Default::default()
        })
        .unwrap());
    fs::write(
        d.path().join("src/b.ts"),
        "export function bravo(){return 2;}\n",
    )
    .unwrap();
    cg.ingest_watch_event("src/b.ts");
    cg.wait_until_watcher_idle(Duration::from_secs(8)).unwrap();
    assert!(!cg.get_nodes_by_name("bravo").unwrap().is_empty());
    fs::write(
        d.path().join("codegraph.json"),
        r#"{"exclude":["src/b.ts"]}"#,
    )
    .unwrap();
    cg.ingest_watch_event("codegraph.json");
    cg.wait_until_watcher_idle(Duration::from_secs(8)).unwrap();
    assert!(cg.get_nodes_by_name("bravo").unwrap().is_empty());
    cg.close();
}

#[cfg(windows)]
#[test]
fn windows_read_noise_is_ignored_while_a_real_edit_stays_pending() {
    let d = fixture();
    let mut cg = CodeGraph::init_and_index(d.path().to_str().unwrap(), None).unwrap();
    assert!(cg
        .watch(WatchOptions {
            inert: true,
            debounce_ms: 2000,
            ..Default::default()
        })
        .unwrap());
    fs::read(d.path().join("src/a.ts")).unwrap();
    cg.ingest_watch_event("src/a.ts");
    assert!(cg.get_pending_files().is_empty());
    fs::write(
        d.path().join("src/a.ts"),
        "export function changed(){return 123;}\n",
    )
    .unwrap();
    cg.ingest_watch_event("src/a.ts");
    assert_eq!(cg.get_pending_files().len(), 1);
    cg.close();
}

#[cfg(windows)]
#[test]
#[ignore = "invoked only by the separate-process read test"]
fn read_child_process() {
    let path = std::env::var_os("CODEGRAPHV_WATCH_READ_PATH").expect("child read path");
    assert!(!fs::read(path).unwrap().is_empty());
}

#[cfg(windows)]
#[test]
fn native_windows_watcher_does_not_sync_for_separate_process_reads() {
    use std::os::windows::process::CommandExt;
    let d = fixture();
    let mut cg = CodeGraph::init_and_index(d.path().to_str().unwrap(), None).unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let n = count.clone();
    assert!(cg
        .watch(WatchOptions {
            debounce_ms: 100,
            on_sync_complete: Some(Arc::new(move |_| {
                n.fetch_add(1, Ordering::SeqCst);
            })),
            ..Default::default()
        })
        .unwrap());
    fs::read(d.path().join("src/a.ts")).unwrap();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "read_child_process"])
        .env("CODEGRAPHV_WATCH_READ_PATH", d.path().join("src/a.ts"))
        .creation_flags(0x08000000)
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    std::thread::sleep(Duration::from_millis(450));
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(cg.get_pending_files().is_empty());
    fs::write(
        d.path().join("src/a.ts"),
        "export function realWriteAfterRead(){return 42;}\n",
    )
    .unwrap();
    wait(|| count.load(Ordering::SeqCst) > 0);
    assert!(!cg
        .get_nodes_by_name("realWriteAfterRead")
        .unwrap()
        .is_empty());
    cg.close();
}

#[cfg(windows)]
fn link(target: &Path, path: &Path) {
    use std::os::windows::process::CommandExt;
    let p = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(path)
        .arg(target)
        .creation_flags(0x08000000)
        .output()
        .unwrap();
    assert!(p.status.success(), "{}", String::from_utf8_lossy(&p.stderr));
}

fn unlink(path: &Path) {
    #[cfg(windows)]
    fs::remove_dir(path).unwrap();
    #[cfg(unix)]
    fs::remove_file(path).unwrap();
}
#[cfg(unix)]
fn link(target: &Path, path: &Path) {
    std::os::unix::fs::symlink(target, path).unwrap();
}

#[test]
fn external_directory_links_are_watched_and_retargeted() {
    let d = fixture();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    fs::write(
        a.path().join("a.ts"),
        "export function outsideAlpha(){return 1;}",
    )
    .unwrap();
    fs::write(
        b.path().join("b.ts"),
        "export function outsideBravo(){return 2;}",
    )
    .unwrap();
    let linked = d.path().join("linked");
    link(a.path(), &linked);
    let mut cg = CodeGraph::init_and_index(d.path().to_str().unwrap(), None).unwrap();
    assert!(cg
        .watch(WatchOptions {
            debounce_ms: 100,
            ..Default::default()
        })
        .unwrap());
    assert!(cg.watcher_snapshot().watched_directories >= 2);
    fs::write(
        a.path().join("a.ts"),
        "export function linkedEdit(){return 3;}",
    )
    .unwrap();
    wait(|| !cg.get_nodes_by_name("linkedEdit").unwrap().is_empty());
    unlink(&linked);
    link(b.path(), &linked);
    wait(|| !cg.get_nodes_by_name("outsideBravo").unwrap().is_empty());
    assert!(cg.get_nodes_by_name("linkedEdit").unwrap().is_empty());
    cg.unwatch();
    assert_eq!(cg.watcher_snapshot().watched_directories, 0);
    cg.close();
    unlink(&linked);
}

#[cfg(unix)]
#[test]
fn background_sync_and_reader_follow_a_replaced_database() {
    let d = fixture();
    let root = d.path().to_str().unwrap();
    let mut server = CodeGraph::init_and_index(root, None).unwrap();
    assert!(server
        .watch(WatchOptions {
            debounce_ms: 100,
            ..Default::default()
        })
        .unwrap());
    let mut rebuilder = CodeGraph::recreate(root).unwrap();
    rebuilder
        .index_all(&Default::default(), None, None)
        .unwrap();
    rebuilder.close();
    fs::write(
        d.path().join("src/b.ts"),
        "export function replacedBeta(){return 2;}",
    )
    .unwrap();
    wait(|| !server.get_nodes_by_name("replacedBeta").unwrap().is_empty());
    assert!(!server.reopen_if_replaced());
    server.close();
}
