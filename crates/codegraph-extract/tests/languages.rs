//! The native language walkers against the TypeScript engine's output for the files in
//! tests/fixtures/languages (vectors from scripts/rust-golden/languages.mjs). Runs without Node.
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
            entries.into_iter().map(|(k, v)| (k, canonical(v))).collect()
        }
        other => other,
    }
}

#[test]
fn language_walkers_match_the_reference_engine() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("golden/languages.json")).unwrap();
    assert!(!cases.is_empty());
    for case in cases {
        let input = &case["input"];
        let file = input["file"].as_str().unwrap();
        let language = Language::parse(input["language"].as_str().unwrap()).unwrap();
        let result = with_error_tolerance(true, || extract_source(file, input["source"].as_str().unwrap(), language))
            .unwrap_or_else(|e| panic!("{file}: {e}"));
        let actual = serde_json::to_value(result).unwrap();
        for key in ["nodes", "edges", "unresolvedReferences", "errors"] {
            let mut bag: Vec<String> = actual[key]
                .as_array()
                .unwrap()
                .iter()
                .cloned()
                .map(|v| serde_json::to_string(&canonical(v)).unwrap())
                .collect();
            bag.sort();
            let expected: Vec<String> = serde_json::from_value(case["expected"][key].clone()).unwrap();
            let missing: Vec<_> = expected.iter().filter(|e| !bag.contains(e)).take(5).collect();
            let extra: Vec<_> = bag.iter().filter(|a| !expected.contains(a)).take(5).collect();
            assert!(bag == expected, "{file}: {key} differs\n  missing: {missing:#?}\n  extra: {extra:#?}");
        }
    }
}
