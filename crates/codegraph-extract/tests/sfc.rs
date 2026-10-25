//! Persisted reference vectors run without Node, npm or the TS implementation.
use codegraph_extract::{extract_source, with_error_tolerance};
use codegraph_types::{EdgeKind, Language, NodeKind};
use serde_json::Value;

fn canonical(value: Value) -> Value {
    match value {
        Value::Array(values) => values.into_iter().map(canonical).collect(),
        Value::Object(object) => {
            let mut entries: Vec<_> = object
                .into_iter()
                .filter(|(k, v)| !matches!(k.as_str(), "durationMs" | "updatedAt") && !v.is_null())
                .collect();
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            entries
                .into_iter()
                .map(|(k, v)| (k, canonical(v)))
                .collect()
        }
        other => other,
    }
}

#[test]
fn component_extraction_matches_persisted_reference() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("golden/sfc.json")).unwrap();
    for (i, case) in cases.into_iter().enumerate() {
        let input = &case["input"];
        let result = with_error_tolerance(true, || {
            extract_source(
                input["file"].as_str().unwrap(),
                input["source"].as_str().unwrap(),
                Language::parse(input["language"].as_str().unwrap()).unwrap(),
            )
        })
        .unwrap();
        let actual = serde_json::to_value(result).unwrap();
        for key in ["nodes", "edges", "unresolvedReferences", "errors"] {
            let mut bag: Vec<_> = actual[key]
                .as_array()
                .unwrap()
                .iter()
                .cloned()
                .map(|v| serde_json::to_string(&canonical(v)).unwrap())
                .collect();
            bag.sort();
            assert_eq!(
                serde_json::to_value(bag).unwrap(),
                case["expected"][key],
                "case {i}: {key}: {}",
                input["source"]
            );
        }
    }
}

#[test]
fn vue_template_connects_component_to_own_method_and_keeps_external_call() {
    let source = "<script>\nexport default { methods: { save() { persist() } } }\n</script>\n<template>😀 {{ save() }} {{ helper() }}</template>";
    let out = extract_source("Form.vue", source, Language::Vue).unwrap();
    let component = out
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::Component)
        .unwrap();
    let save = out
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::Method && n.name == "save")
        .unwrap();
    assert!(out.edges.iter().any(|e| e.source == component.id
        && e.target == save.id
        && e.kind == EdgeKind::Calls
        && e.line == Some(4)
        && e.column == Some(16)));
    assert!(out
        .unresolved_references
        .iter()
        .any(|r| r.reference_name == "persist" && r.from_node_id == save.id));
    assert!(out
        .unresolved_references
        .iter()
        .any(|r| r.reference_name == "helper" && r.from_node_id == component.id));
    assert_eq!(
        out.edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Contains && e.target == save.id)
            .count(),
        1
    );
}

#[test]
fn svelte_module_and_instance_initializers_keep_their_owners() {
    let source = "<script module>\nconst shared = moduleCall()\n</script>\n<script>\nconst local = instanceCall()\nfunction render() { innerCall() }\n</script>";
    let out = extract_source("Panel.svelte", source, Language::Svelte).unwrap();
    let component = out
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::Component)
        .unwrap();
    let owner = |name: &str| {
        &out.unresolved_references
            .iter()
            .find(|r| r.reference_name == name)
            .unwrap()
            .from_node_id
    };
    assert_eq!(owner("instanceCall"), &component.id);
    assert_ne!(owner("moduleCall"), &component.id);
    assert_ne!(owner("innerCall"), &component.id);
    assert_eq!(
        out.nodes
            .iter()
            .filter(|n| n.kind == NodeKind::File)
            .count(),
        1
    );
}

#[test]
fn vue_deep_markup_and_malformed_unicode_do_not_panic() {
    let source = format!(
        "<template>{}{{{{ helper() }}}}{}</template>",
        "<div>".repeat(5_000),
        "</div>".repeat(5_000)
    );
    let out = extract_source("Deep.vue", &source, Language::Vue).unwrap();
    assert_eq!(
        out.unresolved_references
            .iter()
            .filter(|r| r.reference_name == "helper")
            .count(),
        1
    );
    for source in [
        "<template><a :x=😀",
        "<template>{{ `😀${work()}`",
        "<template>\u{2003}<x 😀=\"yes()\" :x=\"ok()\"",
        "<template><div @click=\"/*未完成\"",
    ] {
        extract_source("Partial.vue", source, Language::Vue).unwrap();
    }
}
