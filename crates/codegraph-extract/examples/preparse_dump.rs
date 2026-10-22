//! Differential-test helper (driven by `scripts/rust-golden/preparse.mjs`).
//!
//!   preparse_dump <manifest.json>
//!
//! The manifest is a JSON array of `{ "abs": <path>, "rel": <project-relative path>,
//! "language": <language> }`. Prints a JSON array of `{ rel, out }`: the file's
//! text after the language's `preParse` hook.

use codegraph_types::Language;
use serde_json::{json, Value};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let manifest: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&args[1]).unwrap()).unwrap();
    let handle = std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(move || {
            let mut out = Vec::new();
            for entry in &manifest {
                let abs = entry["abs"].as_str().unwrap();
                let rel = entry["rel"].as_str().unwrap();
                let bytes = std::fs::read(abs).unwrap();
                let source = String::from_utf8_lossy(&bytes).into_owned();
                let language = Language::parse(entry["language"].as_str().unwrap()).unwrap();
                let started = std::time::Instant::now();
                let pre = codegraph_extract::pre_parse(rel, &source, language);
                out.push(json!({ "rel": rel, "out": pre, "ms": started.elapsed().as_secs_f64() * 1000.0 }));
            }
            println!("{}", Value::Array(out));
        })
        .unwrap();
    handle.join().unwrap();
}
