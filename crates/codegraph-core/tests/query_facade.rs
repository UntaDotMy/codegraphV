//! End-to-end callers use the facade, real parsing/resolution and SQLite.
use codegraph_context::BuildOutput;
use codegraph_core::codegraph::{CodeGraph, IndexOptions, IndexState, OpenOptions};
use codegraph_types::*;
use std::fs;

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/payment.ts"),"export function processPayment() { return confirmPayment(); }\nexport function confirmPayment() { return 42; }\n").unwrap();
    fs::write(dir.path().join("src/checkout.ts"),"import { processPayment } from './payment';\nexport function processCheckout() { return processPayment(); }\n").unwrap();
    dir
}

#[test]
fn indexed_project_supports_search_flows_context_and_source() {
    let dir = fixture();
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    assert_eq!(cg.get_index_state(), Some(IndexState::Complete));
    let checkout = cg
        .get_nodes_by_name("processCheckout")
        .unwrap()
        .into_iter()
        .find(|n| n.kind == NodeKind::Function)
        .unwrap();
    let payment = cg
        .get_nodes_by_name("processPayment")
        .unwrap()
        .into_iter()
        .find(|n| n.kind == NodeKind::Function)
        .unwrap();
    let confirm = cg
        .get_nodes_by_name("confirmPayment")
        .unwrap()
        .into_iter()
        .find(|n| n.kind == NodeKind::Function)
        .unwrap();
    assert!(cg
        .get_callees(&checkout.id, 1)
        .unwrap()
        .iter()
        .any(|r| r.node.id == payment.id));
    assert!(cg
        .get_callers(&payment.id, 1)
        .unwrap()
        .iter()
        .any(|r| r.node.id == checkout.id));
    assert!(cg
        .find_path(&checkout.id, &confirm.id, &[EdgeKind::Calls])
        .unwrap()
        .is_some());
    assert_eq!(
        cg.get_file_dependencies("src/checkout.ts").unwrap(),
        vec!["src/payment.ts"]
    );
    assert!(cg
        .get_code(&checkout.id)
        .unwrap()
        .unwrap()
        .contains("return processPayment()"));
    assert!(cg
        .search_nodes("processCheckout", &SearchOptions::default())
        .unwrap()
        .iter()
        .any(|r| r.node.id == checkout.id));
    let context = cg
        .find_relevant_context(
            "processCheckout processPayment confirmPayment",
            &FindRelevantContextOptions {
                max_nodes: Some(30),
                traversal_depth: Some(3),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(context.nodes.contains_key(&checkout.id));
    assert!(context.nodes.contains_key(&payment.id));
    assert!(context.nodes.contains_key(&confirm.id));
    let BuildOutput::Text(markdown) = cg
        .build_context(
            &TaskInput::Text("processCheckout processPayment confirmPayment".into()),
            &BuildContextOptions {
                traversal_depth: Some(3),
                ..Default::default()
            },
        )
        .unwrap()
    else {
        panic!("expected markdown")
    };
    assert!(markdown.contains("## Code Context"));
    assert!(markdown.contains("processCheckout"));
    assert!(markdown.contains("## Call paths"));
    let BuildOutput::Text(json) = cg
        .build_context(
            &TaskInput::Titled {
                title: "processCheckout".into(),
                description: None,
            },
            &BuildContextOptions {
                format: Some(ContextFormat::Json),
                include_code: Some(false),
                ..Default::default()
            },
        )
        .unwrap()
    else {
        panic!("expected JSON")
    };
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["codeBlocks"], serde_json::json!([]));
    assert!(cg.get_stats().unwrap().node_count > 0);
    cg.close();
    let opened = CodeGraph::open(
        dir.path().to_str().unwrap(),
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(opened.get_node(&checkout.id).unwrap().is_some());
    assert!(opened.get_code(&checkout.id).unwrap().is_some());
    opened.close();
}

#[test]
fn incremental_sync_rebinds_unchanged_importers_and_removes_stale_edges() {
    let dir = fixture();
    let mut cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let checkout = cg
        .get_nodes_by_name("processCheckout")
        .unwrap()
        .into_iter()
        .find(|n| n.kind == NodeKind::Function)
        .unwrap();
    fs::write(dir.path().join("src/payment.ts"),"export function processPayment() { return finalizePayment(); }\nexport function finalizePayment() { return 7; }\n").unwrap();
    let sync = cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(sync.files_modified, 1);
    let target = cg
        .get_nodes_by_name("finalizePayment")
        .unwrap()
        .into_iter()
        .find(|n| n.kind == NodeKind::Function)
        .unwrap();
    assert!(cg
        .find_path(&checkout.id, &target.id, &[EdgeKind::Calls])
        .unwrap()
        .is_some());
    assert!(cg.get_nodes_by_name("confirmPayment").unwrap().is_empty());
    fs::remove_file(dir.path().join("src/payment.ts")).unwrap();
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert!(cg.get_callees(&checkout.id, 1).unwrap().is_empty());
    assert!(cg.get_nodes_by_file("src/payment.ts").unwrap().is_empty());
    cg.close();
}

#[test]
fn segment_seeds_reach_camel_case_symbols_and_ignore_orphan_vocabulary() {
    let dir = fixture();
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let words = vec!["process".into(), "payment".into()];
    let matches = cg.get_segment_matches(&words, 6).unwrap();
    assert!(matches.iter().any(|m| m.name == "processPayment"));
    cg.store()
        .insert_name_segments_batch(&["GhostOrderStateMachine".into()])
        .unwrap();
    assert!(cg
        .get_segment_matches(&["ghost".into(), "machine".into()], 6)
        .unwrap()
        .is_empty());
    let sub = cg
        .find_relevant_context("confirm payment", &FindRelevantContextOptions::default())
        .unwrap();
    assert!(sub.nodes.values().any(|n| n.name == "confirmPayment"));
    cg.close();
}
