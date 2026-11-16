//! How a C++ template-argument hop reads (upstream 01a4931).
use codegraph_graph::synthesized_hop::describe_synthesized_hop;
use codegraph_types::{Edge, EdgeKind, Provenance};
use serde_json::json;

#[test]
fn a_template_argument_hop_says_which_argument_for_which_parameter_and_where() {
    let mut e = Edge::new("a", "b", EdgeKind::Calls);
    e.provenance = Some(Provenance::Heuristic);
    e.metadata = json!({ "synthesizedBy": "cpp-template-argument", "via": "Txn", "argument": "PlainTxn", "registeredAt": "use.cc:2" }).as_object().cloned();
    let hop = describe_synthesized_hop(Some(&e)).expect("described");
    assert_eq!(hop.summary, "template argument `PlainTxn` for `Txn`");
    assert_eq!(hop.detail, "");
    assert_eq!(hop.registered_at.as_deref(), Some("use.cc:2"));
}
