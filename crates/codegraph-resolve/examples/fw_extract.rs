//! Differential-test helper (driven by `scripts/rust-golden/fw-extract.mjs`).
//!
//!   fw_extract <manifest.json>
//!
//! The manifest is a JSON array of `{ "abs": <path>, "rel": <project-relative path> }`. Prints a
//! JSON array with one entry per (framework resolver, file) whose `extract` hook returned any
//! node or reference: `{ fw, rel, nodes, references }`, the nodes without their `updatedAt`.
//! Every resolver is tried on every file (the hooks filter on the path themselves), which is
//! exactly what the TypeScript side of the comparison does.

use codegraph_resolve::frameworks::all_framework_resolvers;
use serde_json::{json, Value};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let manifest: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&args[1]).unwrap()).unwrap();
    // The extractors recurse a little; run on a big stack like the engine's workers do.
    let handle = std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(move || {
            let resolvers = all_framework_resolvers();
            let mut out: Vec<Value> = Vec::new();
            for entry in &manifest {
                let abs = entry["abs"].as_str().unwrap();
                let rel = entry["rel"].as_str().unwrap();
                let Ok(bytes) = std::fs::read(abs) else { continue };
                let content = String::from_utf8_lossy(&bytes).into_owned();
                for fw in &resolvers {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fw.extract(rel, &content)));
                    let result = match result {
                        Ok(Some(result)) => result,
                        Ok(None) => continue,
                        Err(_) => {
                            out.push(json!({ "fw": fw.name(), "rel": rel, "panic": true }));
                            continue;
                        }
                    };
                    if result.nodes.is_empty() && result.references.is_empty() {
                        continue;
                    }
                    let nodes: Vec<Value> = result
                        .nodes
                        .iter()
                        .map(|n| {
                            let mut v = serde_json::to_value(n).unwrap();
                            if let Some(o) = v.as_object_mut() {
                                o.remove("updatedAt");
                            }
                            v
                        })
                        .collect();
                    let references: Vec<Value> = result
                        .references
                        .iter()
                        .map(|r| {
                            let mut v = json!({
                                "fromNodeId": r.from_node_id,
                                "referenceName": r.reference_name,
                                "referenceKind": serde_json::to_value(&r.reference_kind).unwrap(),
                                "line": r.line,
                                "column": r.column,
                                "filePath": r.file_path,
                                "language": r.language.as_str(),
                            });
                            if let Some(c) = &r.candidates {
                                v["candidates"] = json!(c);
                            }
                            v
                        })
                        .collect();
                    out.push(json!({ "fw": fw.name(), "rel": rel, "nodes": nodes, "references": references }));
                }
            }
            println!("{}", Value::Array(out));
        })
        .unwrap();
    handle.join().unwrap();
}
