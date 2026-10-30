//! Deleting the target of a directory link inside the project must neither spin the watcher nor
//! stop auto-sync (upstream 6162d872, #2938). On Windows libuv reported the watched folder being
//! deleted by its own absolute path and re-armed the read, so the Node watcher saw tens of
//! thousands of events a second and never synced again. This watcher reads the folder through the
//! native change notification; the test pins that the same situation stays quiet here.
#![cfg(windows)]

use codegraph_sync::watcher::*;
use std::{
    fs,
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// CPU time this process has used, in milliseconds, as the OS reports it.
fn cpu_ms() -> f64 {
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command", &format!("(Get-Process -Id {}).TotalProcessorTime.TotalMilliseconds", std::process::id())])
        .output()
        .expect("powershell runs");
    String::from_utf8_lossy(&out.stdout).trim().parse().expect("a CPU time in milliseconds")
}

fn wait(mut f: impl FnMut() -> bool, what: &str) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_deleted_link_target_leaves_the_watcher_idle_and_syncing() {
    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(project.path().join("src/a.ts"), "export const a=1;").unwrap();
    fs::create_dir(outside.path().join("lib")).unwrap();
    fs::write(outside.path().join("lib/x.ts"), "export const x=1;").unwrap();
    let link = project.path().join("libs");
    let made = Command::new("cmd").args(["/C", "mklink", "/J"]).arg(&link).arg(outside.path().join("lib")).output().unwrap();
    assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stdout));

    let batches = Arc::new(Mutex::new(Vec::new()));
    let record = batches.clone();
    let w = FileWatcher::new(
        project.path().to_str().unwrap(),
        move |paths| {
            record.lock().unwrap().push(paths);
            Ok(WatchSyncResult::default())
        },
        WatchOptions { debounce_ms: 100, ..Default::default() },
    )
    .unwrap();
    assert!(w.start());
    w.wait_until_ready(Duration::from_secs(5)).unwrap();
    // The project root and the link each have their own stream, so the deletion below really is
    // that of a watched folder.
    assert_eq!(w.snapshot().watched_directories, 2);
    batches.lock().unwrap().clear();

    fs::remove_dir_all(outside.path().join("lib")).unwrap();
    wait(|| !batches.lock().unwrap().is_empty(), "a sync after the link target was deleted");

    let before = cpu_ms();
    std::thread::sleep(Duration::from_secs(2));
    let used = cpu_ms() - before;
    assert!(used < 1000.0, "the process used {used} ms of CPU in 2 s after the link target was deleted");
    assert!(w.is_active());
    assert!(w.snapshot().degraded_reason.is_none(), "{:?}", w.snapshot().degraded_reason);

    batches.lock().unwrap().clear();
    fs::write(project.path().join("src/a.ts"), "export const a=2;").unwrap();
    wait(|| batches.lock().unwrap().iter().any(|b| b.as_ref().is_some_and(|p| p.iter().any(|f| f == "src/a.ts"))) || batches.lock().unwrap().iter().any(|b| b.is_none()), "a sync after a later edit");
    w.stop();
}
