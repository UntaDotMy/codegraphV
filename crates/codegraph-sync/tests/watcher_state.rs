use codegraph_sync::watcher::state::*;

fn ready() -> WatchState {
    let mut s = WatchState::new(2000);
    s.start();
    s
}
fn ok() -> Result<WatchSyncResult, SyncError> {
    Ok(WatchSyncResult::default())
}

#[test]
fn quick_window_bursts_scoped_ceiling_and_stable_first_seen() {
    let mut s = ready();
    s.record_file("a.ts".into(), 1000);
    assert_eq!(s.deadline_ms, Some(1300));
    s.record_file("a.ts".into(), 1100);
    assert_eq!(s.snapshot.pending_files[0].first_seen_ms, 1000);
    s.record_file("b.ts".into(), 1200);
    s.record_file("c.ts".into(), 1300);
    assert_eq!(s.deadline_ms, Some(3300));
    let request = s.begin_sync(3300).unwrap();
    assert_eq!(
        request.paths,
        Some(vec!["a.ts".into(), "b.ts".into(), "c.ts".into()])
    );
    assert!(s.snapshot.pending_files.iter().all(|p| p.indexing));
    s.complete(&request, ok(), 3400);
    assert!(s.snapshot.pending_files.is_empty());
    for i in 0..501 {
        s.record_file(format!("file{i}.ts"), 4000);
    }
    assert!(s.begin_sync(6000).unwrap().paths.is_none());
}

#[test]
fn edits_and_topology_changes_during_sync_survive_for_followup() {
    let mut s = ready();
    s.record_file("a.ts".into(), 1000);
    let request = s.begin_sync(1300).unwrap();
    s.record_file("a.ts".into(), 1400);
    s.record_file("b.ts".into(), 1400);
    s.request_full(1450);
    assert!(s.snapshot.pending_files.iter().all(|p| !p.indexing));
    s.complete(&request, ok(), 1500);
    assert_eq!(s.snapshot.pending_files.len(), 2);
    let full = s.begin_sync(1800).unwrap();
    assert!(full.paths.is_none());
    s.request_full(1900);
    s.complete(&full, ok(), 2000);
    assert!(s.needs_full_scan);
    let followup = s.begin_sync(2300).unwrap();
    assert!(followup.paths.is_none());
    s.complete(&followup, ok(), 2400);
    assert!(!s.needs_full_scan);
}

#[test]
fn failed_full_sync_without_file_events_is_retried() {
    let mut s = ready();
    s.request_full(1000);
    let request = s.begin_sync(1300).unwrap();
    s.complete(&request, Err(SyncError::Other("disk full".into())), 1400);
    assert!(s.needs_full_scan);
    assert_eq!(s.deadline_ms, Some(3400));
    let request = s.begin_sync(3400).unwrap();
    assert!(request.paths.is_none());
    s.complete(&request, ok(), 3500);
    assert!(s.deadline_ms.is_none());
}

#[test]
fn contention_degrades_after_six_attempts_then_full_rearm_clears_stale_status() {
    let mut s = ready();
    s.record_file("a.ts".into(), 1000);
    for i in 0..6 {
        let now = 2000 + i * 40_000;
        let request = s.begin_sync(now).unwrap();
        let effects = s.complete(
            &request,
            Err(SyncError::LockUnavailable("busy".into())),
            now + 1,
        );
        assert!(effects.error.is_none());
        assert_eq!(effects.degraded.is_some(), i == 5);
    }
    assert!(!s.snapshot.active);
    assert!(s.snapshot.degraded_reason.is_some());
    assert!(s.rearm(300_000));
    assert!(s.snapshot.recovering);
    assert!(!s.rearm(300_001));
    let full = s.begin_sync(301_000).unwrap();
    assert!(full.paths.is_none());
    s.complete(&full, ok(), 301_100);
    assert!(s.snapshot.degraded_reason.is_none());
    assert!(!s.snapshot.recovering);
}

#[test]
fn generic_failures_have_bounded_backoff_and_do_not_auto_rearm() {
    let mut s = ready();
    s.record_file("a.ts".into(), 1000);
    for i in 0..6 {
        let request = s.begin_sync(2000 + i * 40_000).unwrap();
        let effects = s.complete(
            &request,
            Err(SyncError::Other("extractor failed".into())),
            3000 + i * 40_000,
        );
        assert_eq!(effects.error.as_deref(), Some("extractor failed"));
        if i < 5 {
            assert_eq!(
                s.deadline_ms.unwrap() - (3000 + i * 40_000),
                (2000i64 * (1 << i)).min(30000)
            );
        }
    }
    assert!(s
        .snapshot
        .degraded_reason
        .as_ref()
        .unwrap()
        .contains("6 times in a row"));
    assert!(!s.rearm(400_000));
}

#[test]
fn stopped_or_previous_generation_completions_cannot_clear_new_work() {
    let mut s = ready();
    s.record_file("a.ts".into(), 1000);
    let old = s.begin_sync(1300).unwrap();
    s.stop();
    s.start();
    s.record_file("a.ts".into(), 1500);
    s.complete(&old, ok(), 1600);
    assert_eq!(s.snapshot.pending_files.len(), 1);
    assert_eq!(s.snapshot.completed_syncs, 0);
}
