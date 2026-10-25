//! JSON-lines differential and in-process timing probe for component extractors.
use codegraph_types::Language;
use serde_json::{json, Value};
use std::io::{self, BufRead};

fn extract_raw(input: &Value) -> codegraph_types::ExtractionResult {
    let language = Language::parse(input["language"].as_str().unwrap()).unwrap();
    codegraph_extract::with_error_tolerance(true, || {
        codegraph_extract::extract_source(
            input["file"].as_str().unwrap(),
            input["source"].as_str().unwrap(),
            language,
        )
    })
    .unwrap()
}

fn extract(input: &Value) -> Value {
    let result = extract_raw(input);
    let mut value = serde_json::to_value(result).unwrap();
    value.as_object_mut().unwrap().remove("durationMs");
    for node in value["nodes"].as_array_mut().unwrap() {
        node.as_object_mut().unwrap().remove("updatedAt");
    }
    value
}

fn main() {
    for line in io::stdin().lock().lines() {
        let input: Value = serde_json::from_str(&line.unwrap()).unwrap();
        if input["op"] == "bench" {
            assert!(!cfg!(debug_assertions), "benchmark requires --release");
            let cases = input["cases"].as_array().unwrap();
            let rounds = input["rounds"].as_u64().unwrap_or(5);
            for _ in 0..input["warmup"].as_u64().unwrap_or(5) {
                for case in cases {
                    std::hint::black_box(extract_raw(case));
                }
            }
            let mut samples = Vec::new();
            let mut count = 0usize;
            for _ in 0..7 {
                let start = std::time::Instant::now();
                for _ in 0..rounds {
                    for case in cases {
                        let result = std::hint::black_box(extract_raw(case));
                        count += result.nodes.len()
                            + result.edges.len()
                            + result.unresolved_references.len()
                            + result.errors.len();
                    }
                }
                samples.push(start.elapsed().as_secs_f64() * 1000.0 / rounds as f64);
            }
            println!("{}", json!({ "samplesMs": samples, "count": count }));
        } else {
            println!("{}", extract(&input));
        }
    }
}
