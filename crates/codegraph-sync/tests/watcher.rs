use codegraph_sync::watcher::*;
use std::{
    fs,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

fn fixture() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    fs::create_dir(d.path().join("src")).unwrap();
    fs::write(d.path().join("src/a.ts"), "export const a=1;").unwrap();
    d
}
fn inert() -> WatchOptions {
    WatchOptions {
        inert: true,
        debounce_ms: 100,
        ..Default::default()
    }
}

#[test]
fn lifecycle_pending_filters_scope_refresh_and_exact_sync_paths() {
    let d = fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let record = calls.clone();
    let w = FileWatcher::new(
        d.path().to_str().unwrap(),
        move |paths| {
            record.lock().unwrap().push(paths);
            Ok(WatchSyncResult::default())
        },
        inert(),
    )
    .unwrap();
    assert!(!w.is_active());
    assert!(w.start());
    assert!(w.start());
    w.wait_until_ready(Duration::from_secs(2)).unwrap();
    for p in [
        ".git/index",
        "node_modules/pkg/x.ts",
        ".codegraph/codegraph.db",
        "../escape.ts",
    ] {
        w.ingest_event(p);
    }
    assert!(w.get_pending_files().is_empty());
    w.ingest_event("src/a.ts");
    assert_eq!(w.get_pending_files()[0].path, "src/a.ts");
    assert!(w.flush_now());
    w.wait_until_idle(Duration::from_secs(5)).unwrap();
    assert_eq!(calls.lock().unwrap()[0], Some(vec!["src/a.ts".into()]));
    fs::write(d.path().join("codegraph.json"), r#"{"exclude":["src/**"]}"#).unwrap();
    w.ingest_event("codegraph.json");
    w.ingest_event("src/a.ts");
    assert!(w.get_pending_files().is_empty());
    w.flush_now();
    w.wait_until_idle(Duration::from_secs(5)).unwrap();
    assert_eq!(calls.lock().unwrap().last(), Some(&None));
    w.stop();
    assert!(!w.is_active());
    assert!(w.snapshot().degraded_reason.is_none());
    assert!(w.start());
    w.stop();
}

#[test]
fn events_arrive_while_sync_is_in_flight_and_are_retained() {
    let d = fixture();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let count = Arc::new(AtomicUsize::new(0));
    let n = count.clone();
    let w = FileWatcher::new(
        d.path().to_str().unwrap(),
        move |_| {
            if n.fetch_add(1, Ordering::SeqCst) == 0 {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }
            Ok(WatchSyncResult::default())
        },
        inert(),
    )
    .unwrap();
    assert!(w.start());
    w.ingest_event("src/a.ts");
    w.flush_now();
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(w.get_pending_files()[0].indexing);
    std::thread::sleep(Duration::from_millis(3));
    w.ingest_event("src/a.ts");
    assert!(!w.get_pending_files()[0].indexing);
    release_tx.send(()).unwrap();
    w.wait_until_idle(Duration::from_secs(5)).unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert!(w.get_pending_files().is_empty());
}

#[test]
fn resource_exhaustion_degrades_once_while_watch_limit_only_warns() {
    let d = fixture();
    let degraded = Arc::new(AtomicUsize::new(0));
    let n = degraded.clone();
    let mut options = inert();
    options.on_degraded = Some(Arc::new(move |_| {
        n.fetch_add(1, Ordering::SeqCst);
    }));
    let w = FileWatcher::new(
        d.path().to_str().unwrap(),
        |_| Ok(WatchSyncResult::default()),
        options,
    )
    .unwrap();
    assert!(w.start());
    w.ingest_failure(WatchFailure::InotifyLimit);
    assert!(w.is_active());
    assert!(w.snapshot().partial_coverage);
    w.ingest_failure(WatchFailure::ResourceExhausted);
    w.ingest_failure(WatchFailure::ResourceExhausted);
    assert!(!w.is_active());
    assert_eq!(degraded.load(Ordering::SeqCst), 1);
    assert!(!w.rearm_after_lock_contention());
    assert!(w.start());
    assert!(w.snapshot().degraded_reason.is_none());
}

#[test]
fn native_events_cover_edits_new_directories_and_deletion() {
    let d = fixture();
    let paths = Arc::new(Mutex::new(Vec::new()));
    let p = paths.clone();
    let w = FileWatcher::new(
        d.path().to_str().unwrap(),
        move |batch| {
            p.lock().unwrap().push(batch);
            Ok(WatchSyncResult::default())
        },
        WatchOptions {
            debounce_ms: 100,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(w.start());
    fs::write(d.path().join("src/a.ts"), "export const changed=2;").unwrap();
    wait(|| !paths.lock().unwrap().is_empty());
    paths.lock().unwrap().clear();
    fs::create_dir(d.path().join("newdir")).unwrap();
    fs::write(d.path().join("newdir/b.ts"), "export const b=3;").unwrap();
    wait(|| paths.lock().unwrap().iter().any(|p| p.is_none()));
    paths.lock().unwrap().clear();
    fs::remove_file(d.path().join("src/a.ts")).unwrap();
    wait(|| !paths.lock().unwrap().is_empty());
    w.stop();
    assert_eq!(w.snapshot().watched_directories, 0);
}

fn wait(mut f: impl FnMut() -> bool) {
    let start = std::time::Instant::now();
    while !f() {
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "watch event timed out"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn lock_rearm_keeps_stale_status_until_the_full_catchup_commits() {
    use std::sync::atomic::AtomicBool;
    let d = fixture();
    let locked = Arc::new(AtomicBool::new(true));
    let gate = locked.clone();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let record = calls.clone();
    let w = FileWatcher::new(
        d.path().to_str().unwrap(),
        move |paths| {
            record.lock().unwrap().push(paths);
            if gate.load(Ordering::SeqCst) {
                return Err(SyncError::LockUnavailable("busy".into()));
            }
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(WatchSyncResult::default())
        },
        WatchOptions {
            inert: true,
            debounce_ms: 10,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(w.start());
    w.ingest_event("src/a.ts");
    wait(|| w.snapshot().degraded_reason.is_some());
    assert!(!w.is_active());
    assert_eq!(calls.lock().unwrap().len(), 6);
    locked.store(false, Ordering::SeqCst);
    assert!(w.rearm_after_lock_contention());
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(w.snapshot().recovering);
    assert!(w.snapshot().degraded_reason.is_some());
    assert_eq!(calls.lock().unwrap().last(), Some(&None));
    assert!(!w.rearm_after_lock_contention());
    release_tx.send(()).unwrap();
    w.wait_until_idle(Duration::from_secs(3)).unwrap();
    assert!(w.snapshot().degraded_reason.is_none());
    assert!(!w.snapshot().recovering);
}

#[test]
fn deterministic_failures_stop_after_the_retry_budget() {
    let d = fixture();
    let errors = Arc::new(AtomicUsize::new(0));
    let n = errors.clone();
    let w = FileWatcher::new(
        d.path().to_str().unwrap(),
        |_| Err(SyncError::Other("extractor failed".into())),
        WatchOptions {
            inert: true,
            debounce_ms: 10,
            on_sync_error: Some(Arc::new(move |_| {
                n.fetch_add(1, Ordering::SeqCst);
            })),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(w.start());
    w.ingest_event("src/a.ts");
    wait(|| !w.is_active());
    assert_eq!(errors.load(Ordering::SeqCst), 6);
    assert!(w
        .snapshot()
        .degraded_reason
        .unwrap()
        .contains("6 times in a row"));
    assert!(!w.rearm_after_lock_contention());
}

#[test]
fn completion_callback_can_enqueue_a_followup_event_without_deadlocking() {
    let d = fixture();
    let handle: Arc<Mutex<std::sync::Weak<FileWatcher>>> =
        Arc::new(Mutex::new(std::sync::Weak::new()));
    let callback_handle = handle.clone();
    let count = Arc::new(AtomicUsize::new(0));
    let n = count.clone();
    let options = WatchOptions {
        inert: true,
        debounce_ms: 100,
        on_sync_complete: Some(Arc::new(move |_| {
            if n.fetch_add(1, Ordering::SeqCst) == 0 {
                if let Some(w) = callback_handle.lock().unwrap().upgrade() {
                    assert!(w.start());
                    w.ingest_event("src/b.ts");
                }
            }
        })),
        ..Default::default()
    };
    let w = Arc::new(
        FileWatcher::new(
            d.path().to_str().unwrap(),
            |_| Ok(WatchSyncResult::default()),
            options,
        )
        .unwrap(),
    );
    *handle.lock().unwrap() = Arc::downgrade(&w);
    assert!(w.start());
    w.ingest_event("src/a.ts");
    wait(|| count.load(Ordering::SeqCst) == 2);
}
