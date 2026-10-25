//! Oracle vectors include both the answer and the exact lazy callback reads.
use codegraph_search::{extract_query_paths, query_might_contain_paths, QueryPathOptions};
use serde_json::{json, Value};
use std::cell::RefCell;

#[test]
fn path_and_line_recognition_matches_the_typescript_oracle() {
    let fixtures: Value = serde_json::from_str(include_str!("fixtures/query-paths.json")).unwrap();
    let indexes: Vec<Vec<String>> = serde_json::from_value(fixtures["indexes"].clone()).unwrap();
    let vectors = fixtures["vectors"].as_array().unwrap();
    assert!(vectors.len() >= 1600);
    for (i, vector) in vectors.iter().enumerate() {
        let input = &vector["input"];
        let paths = &indexes[input["index"].as_u64().unwrap() as usize];
        let query = input["query"].as_str().unwrap();
        let exists_asked = RefCell::new(Vec::new());
        let symbols_asked = RefCell::new(Vec::new());
        let exists = |p: &str| {
            exists_asked.borrow_mut().push(p.to_owned());
            input["exists"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == p))
        };
        let symbols = |s: &str| {
            symbols_asked.borrow_mut().push(s.to_owned());
            input["symbols"][s]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        let options = QueryPathOptions {
            max_pins: input["maxPins"].as_u64().map(|n| n as usize),
            max_matches_per_span: input["maxMatchesPerSpan"].as_u64().map(|n| n as usize),
            exists_on_disk: input
                .get("exists")
                .map(|_| &exists as &dyn Fn(&str) -> bool),
            symbol_files: input
                .get("symbols")
                .map(|_| &symbols as &dyn Fn(&str) -> Vec<String>),
        };
        let result = extract_query_paths(query, paths, &options);
        let actual = json!({ "gate": query_might_contain_paths(query), "result": result, "existsAsked": exists_asked.take(), "symbolsAsked": symbols_asked.take() });
        assert_eq!(actual, vector["expected"], "vector {i}: {}", input);
    }
}
