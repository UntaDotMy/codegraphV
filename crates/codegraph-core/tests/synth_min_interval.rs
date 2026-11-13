//! `CODEGRAPHV_SYNTH_MIN_INTERVAL_MS` (upstream 17395382, #2911): a long-lived instance's syncs
//! put off a whole-graph synthesis that comes within the interval of its last one, and run it once
//! the interval is up. The variable is process state, so these tests take turns with it.
use codegraph_core::codegraph::parse_synth_min_interval;
use codegraph_core::{CodeGraph, IndexOptions};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

static ENV: Mutex<()> = Mutex::new(());
const VAR: &str = "CODEGRAPHV_SYNTH_MIN_INTERVAL_MS";

fn with_interval<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
    let _turn = ENV.lock().unwrap_or_else(|p| p.into_inner());
    match value {
        Some(v) => std::env::set_var(VAR, v),
        None => std::env::remove_var(VAR),
    }
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    std::env::remove_var(VAR);
    out.unwrap_or_else(|p| std::panic::resume_unwind(p))
}

fn project() -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.ts"), "export function a() { return 1; }\n").unwrap();
    let cg = CodeGraph::init_and_index(&dir.path().to_string_lossy(), None).unwrap();
    (dir, cg)
}

fn pending(cg: &CodeGraph) -> Option<String> {
    cg.store().get_metadata("synthesis_pending").unwrap()
}

/// A sync that needs synthesis: an edit plus the pending flag a synthesis input sets.
fn edit_and_sync(dir: &Path, cg: &mut CodeGraph, n: u32) -> codegraph_core::orchestrator::SyncResult {
    std::fs::write(dir.join("a.ts"), format!("export function a() {{ return {n}; }}\n")).unwrap();
    cg.store().set_metadata("synthesis_pending", "1").unwrap();
    cg.sync(&IndexOptions::default(), None).unwrap()
}

#[test]
fn the_interval_parses_whole_milliseconds_between_one_second_and_a_day() {
    assert_eq!(parse_synth_min_interval(None), None);
    assert_eq!(parse_synth_min_interval(Some("")), None);
    assert_eq!(parse_synth_min_interval(Some("  ")), None);
    assert_eq!(parse_synth_min_interval(Some("999")), None);
    assert_eq!(parse_synth_min_interval(Some("1.5")), None);
    assert_eq!(parse_synth_min_interval(Some("abc")), None);
    assert_eq!(parse_synth_min_interval(Some("86400001")), None);
    assert_eq!(parse_synth_min_interval(Some("1000")), Some(Duration::from_millis(1000)));
    assert_eq!(parse_synth_min_interval(Some("86400000")), Some(Duration::from_millis(86_400_000)));
}

#[test]
fn without_the_variable_every_sync_synthesizes() {
    with_interval(None, || {
        let (dir, mut cg) = project();
        for n in 2..4 {
            let result = edit_and_sync(dir.path(), &mut cg, n);
            assert_eq!(pending(&cg).as_deref(), Some("0"));
            assert_eq!(result.synthesis_deferred_ms, None);
        }
    });
}

#[test]
fn an_invalid_value_holds_nothing() {
    with_interval(Some("500"), || {
        let (dir, mut cg) = project();
        edit_and_sync(dir.path(), &mut cg, 2);
        assert_eq!(pending(&cg).as_deref(), Some("0"));
    });
}

#[test]
fn a_sync_within_the_interval_puts_synthesis_off_and_a_later_run_does_it() {
    with_interval(Some("10000"), || {
        let (dir, mut cg) = project();
        let result = edit_and_sync(dir.path(), &mut cg, 2);
        assert_eq!(pending(&cg).as_deref(), Some("1"), "held after the full index's synthesis");
        assert!(result.synthesis_deferred_ms.is_some_and(|ms| ms > 0 && ms <= 10_000), "{:?}", result.synthesis_deferred_ms);
        // Still inside the interval: nothing runs, and it says how long is left.
        assert!(cg.run_deferred_synthesis().unwrap().is_some());
        assert_eq!(pending(&cg).as_deref(), Some("1"));
        // Shorten the interval so it has passed: the put-off synthesis runs.
        std::env::set_var(VAR, "1000");
        std::thread::sleep(Duration::from_millis(1100));
        assert_eq!(cg.run_deferred_synthesis().unwrap(), None);
        assert_eq!(pending(&cg).as_deref(), Some("0"));
    });
}

#[test]
fn a_fresh_instance_is_never_held() {
    with_interval(Some("10000"), || {
        let (dir, cg) = project();
        cg.close();
        let mut fresh = CodeGraph::open(&dir.path().to_string_lossy(), Default::default()).unwrap();
        edit_and_sync(dir.path(), &mut fresh, 2);
        assert_eq!(pending(&fresh).as_deref(), Some("0"));
    });
}

#[test]
fn a_watched_instance_runs_the_held_synthesis_by_itself() {
    with_interval(Some("5000"), || {
        let (dir, mut cg) = project();
        assert!(cg.watch(Default::default()).unwrap());
        cg.wait_until_watcher_ready(Duration::from_secs(10)).unwrap();
        // Two quick edits: the watcher's engine synthesizes on the first, holds the second.
        for n in 2..4 {
            std::fs::write(dir.path().join("a.ts"), format!("export function a() {{ return {n}; }}\n")).unwrap();
            cg.store().set_metadata("synthesis_pending", "1").unwrap();
            cg.ingest_watch_event("a.ts");
            cg.wait_until_watcher_idle(Duration::from_secs(20)).unwrap();
        }
        // No further edit: the held synthesis still runs once the interval is up.
        let deadline = Instant::now() + Duration::from_secs(25);
        while pending(&cg).as_deref() != Some("0") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(pending(&cg).as_deref(), Some("0"));
        cg.unwatch();
    });
}
