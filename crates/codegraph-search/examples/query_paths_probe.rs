//! JSON-lines probe for `scripts/rust-golden/query-paths.mjs`.
use codegraph_search::{extract_query_paths, query_might_contain_paths, QueryPathOptions};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::io::{self, BufRead};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for line in io::stdin().lock().lines() {
        let input: Value = serde_json::from_str(&line?)?;
        let paths: Vec<String> = serde_json::from_value(input["paths"].clone())?;
        let query = input["query"].as_str().ok_or("missing query")?;
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
        let result = extract_query_paths(query, &paths, &options);
        println!(
            "{}",
            json!({ "gate": query_might_contain_paths(query), "result": result, "existsAsked": exists_asked.take(), "symbolsAsked": symbols_asked.take() })
        );
    }
    Ok(())
}
