use codegraph_sync::watcher::state::*;
use serde_json::{json, Value};
pub fn step(state: &mut WatchState, request: &mut Option<SyncRequest>, q: &Value) -> Value {
    let now = q["now"].as_i64().unwrap_or(1000);
    match q["op"].as_str().unwrap() {
        "new" => {
            *state = WatchState::new(q["debounce"].as_u64().unwrap_or(2000));
            *request = None;
        }
        "start" => state.start(),
        "stop" => state.stop(),
        "event" => state.record_file(q["path"].as_str().unwrap().into(), now),
        "full" => state.request_full(now),
        "begin" => {
            if let Some(r) = state.begin_sync(now) {
                *request = Some(r);
            }
        }
        "finish" => {
            if let Some(r) = request.take() {
                let result = match q["result"].as_str().unwrap_or("ok") {
                    "lock" => Err(SyncError::LockUnavailable("busy".into())),
                    "error" => Err(SyncError::Other("sync failed".into())),
                    _ => Ok(WatchSyncResult::default()),
                };
                state.complete(&r, result, now);
            }
        }
        "rearm" => {
            state.rearm(now);
        }
        _ => panic!("bad operation"),
    }
    json!({"active":state.snapshot.active,"ready":state.snapshot.ready,"syncing":state.snapshot.syncing,"recovering":state.snapshot.recovering,
        "degradedReason":state.snapshot.degraded_reason,"pendingFiles":state.snapshot.pending_files,"needsFullScan":state.needs_full_scan,"deadline":state.deadline_ms,
        "paths":state.current.as_ref().map(|r|r.paths.clone())})
}
