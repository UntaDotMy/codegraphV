//! Persisted reference vectors for grammar-free template extractors run without
//! Node, npm or the TypeScript implementation.
use codegraph_extract::{extract_source, with_error_tolerance};
use codegraph_types::Language;
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
fn template_extraction_matches_persisted_reference() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("golden/templates.json")).unwrap();
    assert!(!cases.is_empty(), "persisted reference vectors are empty");
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
