//! Reproducible warm retrieval timings, with answer hashes to detect changes.
//! Run before and after a change using the same release build settings:
//! cargo run --release --locked -p codegraph-mcp --example retrieval_bench
//! Pass an indexed project root and queries to also measure complete explore calls.
use codegraph_core::{CodeGraph, OpenOptions};
use codegraph_search::{extract_query_paths, QueryPathOptions};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{hint::black_box, time::Instant};

fn measure<T: Serialize>(mut run: impl FnMut() -> T) -> Value {
    const SAMPLES: usize = 9;
    const REPEATS: usize = 5;
    for _ in 0..3 {
        black_box(run());
    }
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let start = Instant::now();
        for _ in 0..REPEATS {
            black_box(run());
        }
        samples.push(start.elapsed().as_secs_f64() * 1000.0 / REPEATS as f64);
    }
    let result = serde_json::to_vec(&run()).unwrap();
    let mut sorted = samples.clone();
    sorted.sort_by(f64::total_cmp);
    json!({
        "medianMs": sorted[SAMPLES / 2], "p95Ms": sorted[SAMPLES - 1],
        "samplesMs": samples, "resultBytes": result.len(),
        "resultSha256": format!("{:x}", Sha256::digest(&result))
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut rows = Vec::new();
    for count in [100, 5_000, 40_000] {
        let paths: Vec<_> = (0..count).map(|i| format!("src/module_{i}/file_{i}.rs")).collect();
        for query in [
            "index_all sync".to_string(),
            format!("src/module_{}/file_{}.rs:1-40", count - 1, count - 1),
            "file_42.rs index_all".to_string(),
        ] {
            let mut row = measure(|| extract_query_paths(&query, &paths, &QueryPathOptions::default()));
            row["operation"] = json!("queryPaths");
            row["files"] = json!(count);
            row["query"] = json!(query);
            rows.push(row);
        }
    }
    let mut args = std::env::args().skip(1);
    if let Some(root) = args.next() {
        let cg = CodeGraph::open(&root, OpenOptions { read_only: true, sync: false })?;
        for query in args {
            let input = json!({"query": query});
            // Fresh history each time: measure the same source, not an already-shown reply.
            let mut row = measure(|| codegraph_mcp::explore::handle(&cg, &input).unwrap());
            let text = codegraph_mcp::explore::handle(&cg, &input)?.message();
            row["operation"] = json!("explore");
            row["query"] = json!(query);
            row["responseChars"] = json!(text.chars().count());
            row["sourceLines"] = json!(text.lines().filter(|line| line.split_once('\t')
                .is_some_and(|(n, _)| n.trim().parse::<u32>().is_ok())).count());
            rows.push(row);
        }
        cg.close();
    }
    println!("{}", serde_json::to_string_pretty(&json!({
        "platform": std::env::consts::OS, "arch": std::env::consts::ARCH,
        "samples": 9, "repeats": 5, "rows": rows,
        "scope": "Warm in-process calls; excludes startup, indexing, MCP transport and model usage. Character/byte counts are not token counts."
    }))?);
    Ok(())
}
