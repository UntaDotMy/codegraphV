//! Runs native `codegraph_explore` for the differential harness
//! (`scripts/rust-golden/explore.mjs`): one project root argument, one JSON
//! query per stdin line, one `{"text","isError"}` JSON line per answer.
use codegraph_core::{CodeGraph, OpenOptions};
use serde_json::{json, Value};
use std::io::{BufRead, Write};

fn main() {
    let root = std::env::args().nth(1).expect("usage: explore_probe <project root>");
    let cg = CodeGraph::open(&root, OpenOptions { sync: false, read_only: true }).expect("open project");
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in std::io::stdin().lock().lines() {
        let args: Value = serde_json::from_str(&line.expect("stdin")).expect("JSON arguments");
        let answer = match codegraph_mcp::explore::handle(&cg, &args) {
            Ok(result) => json!({"text": result.message(), "isError": result.is_error}),
            Err(err) => json!({"text": format!("native error: {err}"), "isError": true}),
        };
        writeln!(out, "{answer}").unwrap();
        out.flush().unwrap();
    }
}
