//! Differential-test helper (driven by `scripts/rust-golden/extract.mjs`).
//!
//!   extract_dump <manifest.json> [--defer-on-errors]
//!
//! The manifest is a JSON array of `{ "abs": <path>, "rel": <project-relative path>,
//! "language": <optional language> }`. Prints a JSON array with one entry per
//! file: `{ rel, language, ok, error?, result? }` where `result` is the
//! `ExtractionResult` with its volatile fields (`updatedAt`, `durationMs`) removed.

use codegraph_extract::languages::detect_language;
use codegraph_types::Language;
use serde_json::{json, Value};

fn strip_volatile(result: &mut Value) {
    if let Some(obj) = result.as_object_mut() {
        obj.remove("durationMs");
        if let Some(Value::Array(nodes)) = obj.get_mut("nodes") {
            for n in nodes {
                if let Some(o) = n.as_object_mut() {
                    o.remove("updatedAt");
                }
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let manifest: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&args[1]).unwrap()).unwrap();
    let defer = args.iter().any(|a| a == "--defer-on-errors");
    // Walkers recurse per AST level; run on a big stack like the engine's workers do.
    let handle = std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(move || {
            let mut out = Vec::new();
            for entry in &manifest {
                let abs = entry["abs"].as_str().unwrap();
                let rel = entry["rel"].as_str().unwrap();
                let bytes = std::fs::read(abs).unwrap();
                let source = String::from_utf8_lossy(&bytes).into_owned();
                let language = match entry["language"].as_str() {
                    Some(l) => Language::parse(l).unwrap(),
                    None => detect_language(rel, Some(&source), None, None),
                };
                let r = codegraph_extract::with_error_tolerance(!defer, || {
                    codegraph_extract::extract_source(rel, &source, language)
                });
                out.push(match r {
                    Ok(res) => {
                        let mut v = serde_json::to_value(&res).unwrap();
                        strip_volatile(&mut v);
                        json!({ "rel": rel, "language": language.as_str(), "ok": true, "result": v })
                    }
                    Err(e) => json!({ "rel": rel, "language": language.as_str(), "ok": false, "error": e }),
                });
            }
            println!("{}", Value::Array(out));
        })
        .unwrap();
    handle.join().unwrap();
}
