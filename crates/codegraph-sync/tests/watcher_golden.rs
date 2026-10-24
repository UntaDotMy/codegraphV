#[path = "support/watcher_model.rs"]
mod model;
use codegraph_sync::watcher::state::WatchState;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
#[derive(Deserialize)]
struct Scenario {
    name: String,
    cases: Vec<Case>,
}
#[derive(Deserialize)]
struct Case {
    query: Value,
    expected: Value,
}
fn compact(mut value: Value) -> Value {
    if let Some(pending) = value["pendingFiles"].as_array().filter(|p| p.len() > 16) {
        let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(pending).unwrap()));
        let count = pending.len();
        let sample: Vec<_> = pending
            .iter()
            .take(8)
            .chain(pending.iter().skip(count - 8))
            .cloned()
            .collect();
        value["pendingFiles"] = serde_json::json!(sample);
        value["pendingCount"] = serde_json::json!(count);
        value["pendingSha256"] = Value::String(hash);
    }
    value
}
#[test]
fn scheduler_matches_reference_snapshots_without_node_or_os_timers() {
    let scenarios: Vec<Scenario> = serde_json::from_str(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/watcher.json"
        ))
        .unwrap(),
    )
    .unwrap();
    for s in scenarios {
        let mut state = WatchState::new(2000);
        let mut request = None;
        for c in s.cases {
            let actual = compact(model::step(&mut state, &mut request, &c.query));
            assert_eq!(actual, c.expected, "{}: {}", s.name, c.query);
        }
    }
}
