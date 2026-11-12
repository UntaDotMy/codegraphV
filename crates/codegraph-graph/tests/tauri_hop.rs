//! How a Tauri hop reads in explore's Flow and the node trail (upstream 2008e2f3).
use codegraph_graph::synthesized_hop::describe_synthesized_hop;
use codegraph_types::{Edge, EdgeKind, Provenance};
use serde_json::json;

fn hop(metadata: serde_json::Value) -> (String, String, Option<String>) {
    let mut e = Edge::new("a", "b", EdgeKind::Calls);
    e.provenance = Some(Provenance::Heuristic);
    e.metadata = metadata.as_object().cloned();
    let h = describe_synthesized_hop(Some(&e)).expect("described");
    (h.summary, h.detail, h.registered_at)
}

#[test]
fn names_the_command_and_the_event_with_the_wiring_site() {
    let (summary, _, at) = hop(json!({ "synthesizedBy": "tauri-command", "command": "greet", "registeredAt": "src-tauri/src/lib.rs:6" }));
    assert_eq!(summary, "Tauri command `greet`");
    assert_eq!(at.as_deref(), Some("src-tauri/src/lib.rs:6"));
    let (summary, detail, at) = hop(json!({ "synthesizedBy": "tauri-event", "event": "saved", "tier": "server→client", "registeredAt": "src/a.ts:3" }));
    assert_eq!((summary.as_str(), detail.as_str(), at.as_deref()), ("Tauri event `saved`", " → frontend", Some("src/a.ts:3")));
    assert_eq!(hop(json!({ "synthesizedBy": "tauri-event", "event": "x", "tier": "client→server" })).1, " → Rust");
}
