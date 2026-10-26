use codegraph_mcp::handoff::{daemon_accepts_client, proxy_uses_daemon, ExeWatch, Relay, TOOLS_CHANGED};
use serde_json::{json, Value};
use std::{path::Path, time::{Duration, SystemTime}};

fn line(v: Value) -> String {
    format!("{v}\n")
}

fn initialized_relay() -> Relay {
    let mut relay = Relay::new();
    relay.from_client(&line(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"capabilities":{"roots":{}},"rootUri":"file:///p"}})));
    assert!(relay.from_daemon(&line(json!({"jsonrpc":"2.0","id":0,"result":{}}))));
    relay.from_client(&line(json!({"jsonrpc":"2.0","method":"notifications/initialized"})));
    relay
}

#[test]
fn relay_replays_the_session_then_unanswered_requests_in_order() {
    let mut relay = initialized_relay();
    relay.from_client(&line(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"a"}})));
    relay.from_client(&line(json!({"jsonrpc":"2.0","id":"two","method":"tools/call","params":{"name":"b"}})));
    relay.from_client(&line(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"c"}})));
    assert!(relay.from_daemon(&line(json!({"jsonrpc":"2.0","id":1,"result":{}}))));

    let replay: Vec<Value> = relay.replay().iter().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(replay.len(), 4, "{replay:?}");
    assert_eq!(replay[0]["method"], "initialize");
    assert_eq!(replay[0]["params"]["rootUri"], "file:///p");
    assert_ne!(replay[0]["id"], json!(0), "the replayed initialize carries its own id");
    assert_eq!(replay[1]["method"], "notifications/initialized");
    assert_eq!(replay[2]["id"], "two");
    assert_eq!(replay[3]["id"], 3);

    // The successor's answer to the replayed initialize is the proxy's, not the client's;
    // what the successor asks the client (roots) still goes through.
    assert!(!relay.from_daemon(&line(json!({"jsonrpc":"2.0","id":replay[0]["id"],"result":{}}))));
    assert!(relay.from_daemon(&line(json!({"jsonrpc":"2.0","id":"codegraph-roots","method":"roots/list","params":{}}))));
    assert!(relay.from_daemon(&line(json!({"jsonrpc":"2.0","id":"two","result":{}}))));
    assert!(relay.from_daemon(&line(json!({"jsonrpc":"2.0","id":3,"result":{}}))));

    // Only the client's own requests count as answered; the proxy's initialize does not.
    assert_eq!(relay.answered(), 4);

    // Answered requests are not sent a second time on the next hand-off.
    let again: Vec<Value> = relay.replay().iter().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(again.len(), 2, "{again:?}");
    assert_ne!(again[0]["id"], replay[0]["id"], "each hand-off gets a fresh initialize id");
}

#[test]
fn relay_resends_an_unanswered_initialize_as_is() {
    let mut relay = Relay::new();
    let init = json!({"jsonrpc":"2.0","id":7,"method":"initialize","params":{}});
    relay.from_client(&line(init.clone()));
    let replay: Vec<Value> = relay.replay().iter().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(replay, vec![init.clone()]);
    assert!(relay.from_daemon(&line(json!({"jsonrpc":"2.0","id":7,"result":{}}))), "the client's own initialize answer reaches it");
}

#[test]
fn relay_passes_client_responses_and_unparsable_lines_through_without_tracking() {
    let mut relay = initialized_relay();
    relay.from_client(&line(json!({"jsonrpc":"2.0","id":"codegraph-roots","result":{"roots":[]}})));
    relay.from_client("not json\n");
    assert!(relay.from_daemon("not json either\n"));
    assert_eq!(relay.replay().len(), 2, "only initialize + initialized");
}

#[test]
fn tools_changed_is_the_mcp_notification() {
    let v: Value = serde_json::from_str(TOOLS_CHANGED).unwrap();
    assert_eq!(v, json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}));
}

#[test]
fn version_rules_let_older_proxies_ride_a_newer_daemon_only() {
    assert!(daemon_accepts_client("0.0.5", "0.0.5"));
    assert!(daemon_accepts_client("0.0.5", "0.0.4"));
    assert!(!daemon_accepts_client("0.0.5", "0.0.6"));
    assert!(proxy_uses_daemon("0.0.5", "0.0.5"));
    assert!(proxy_uses_daemon("0.0.5", "0.0.6"));
    assert!(!proxy_uses_daemon("0.0.5", "0.0.4"));
    // Not a version: only an exact match is trusted.
    assert!(daemon_accepts_client("dev", "dev"));
    assert!(!daemon_accepts_client("0.0.5", "dev"));
    assert!(!proxy_uses_daemon("0.0.5", ""));
}

fn bump_mtime(path: &Path, secs: u64) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() + Duration::from_secs(secs)).unwrap();
}

#[test]
fn exe_watch_never_fires_for_an_unchanged_binary() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("codegraphV");
    std::fs::write(&exe, b"v1").unwrap();
    let mut watch = ExeWatch::new(exe);
    for _ in 0..5 {
        assert!(!watch.poll(|_| panic!("no probe for an unchanged binary")));
    }
}

#[test]
fn exe_watch_fires_once_a_replacement_is_stable_and_runs() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("codegraphV");
    std::fs::write(&exe, b"v1").unwrap();
    let mut watch = ExeWatch::new(exe.clone());
    std::fs::write(&exe, b"v2-longer").unwrap();
    bump_mtime(&exe, 60);
    assert!(!watch.poll(|_| panic!("a file seen changed once may still be mid-copy")));
    assert!(watch.poll(|_| true), "stable and runnable");
}

#[test]
fn exe_watch_ignores_a_replacement_that_does_not_run_or_is_still_changing() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("codegraphV");
    std::fs::write(&exe, b"v1").unwrap();
    let mut watch = ExeWatch::new(exe.clone());
    std::fs::write(&exe, b"v2").unwrap();
    bump_mtime(&exe, 60);
    assert!(!watch.poll(|_| true));
    std::fs::write(&exe, b"v2-still-copying").unwrap();
    bump_mtime(&exe, 120);
    assert!(!watch.poll(|_| panic!("changed again since the last poll: not stable yet")));
    assert!(!watch.poll(|_| false), "a replacement that fails --version is not a hand-off");
    assert!(!watch.poll(|_| panic!("a rejected binary is not probed again")));
    std::fs::remove_file(&exe).unwrap();
    assert!(!watch.poll(|_| panic!("a missing binary is mid-rename, not an upgrade")));
}
