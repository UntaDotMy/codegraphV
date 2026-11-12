//! How a Next proxy hop reads in explore's Flow and the node trail (upstream 446d08c2).
use codegraph_graph::synthesized_hop::describe_synthesized_hop;
use codegraph_types::{Edge, EdgeKind, Provenance};
use serde_json::json;

fn hop(metadata: serde_json::Value) -> String {
    let mut e = Edge::new("route", "proxy", EdgeKind::Calls);
    e.provenance = Some(Provenance::Heuristic);
    e.metadata = metadata.as_object().cloned();
    describe_synthesized_hop(Some(&e)).expect("described").summary
}

#[test]
fn a_next_proxy_hop_names_its_matcher_or_every_request() {
    assert_eq!(hop(json!({ "synthesizedBy": "next-proxy", "matcher": "/dashboard/:rest*", "registeredAt": "proxy.ts:9" })), "Next proxy, matcher `/dashboard/:rest*`");
    assert_eq!(hop(json!({ "synthesizedBy": "next-proxy", "registeredAt": "proxy.ts:1" })), "Next proxy, on every request");
}
