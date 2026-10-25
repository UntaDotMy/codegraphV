//! Run the real native query APIs repeatedly inside one process. Parsing,
//! startup and DB opening are excluded, as in the TypeScript comparison.
use codegraph_db::{OpenOptions, Store};
use codegraph_graph::{
    named_symbol_flow::{resolve_named_symbol_flow, NamedSymbolFlowOptions},
    GraphTraverser,
};
use codegraph_types::{EdgeKind, SearchOptions, TraversalOptions};
use serde::Deserialize;
use serde_json::json;
use std::{hint::black_box, io::Read, time::Instant};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    warmup: usize,
    samples: usize,
    repetitions: usize,
    queries: Vec<Query>,
}
#[derive(Deserialize)]
struct Query {
    op: String,
    query: Option<String>,
    id: Option<String>,
    to: Option<String>,
}

fn run(store: &Store, q: &Query) -> Result<usize, Box<dyn std::error::Error>> {
    let t = GraphTraverser::new(store);
    let id = q.id.as_deref().unwrap_or("n0");
    Ok(match q.op.as_str() {
        "search" => black_box(store.search_nodes(
            q.query.as_deref().unwrap(),
            &SearchOptions {
                limit: Some(20),
                ..Default::default()
            },
        )?)
        .len(),
        "bfs" => black_box(t.traverse_bfs(
            id,
            &TraversalOptions {
                max_depth: Some(3),
                limit: Some(100),
                ..Default::default()
            },
        )?)
        .nodes
        .len(),
        "callers" => black_box(t.get_callers(id, 3)?).len(),
        "impact" => black_box(t.get_impact_radius(id, 3)?).nodes.len(),
        "path" => black_box(t.find_path(id, q.to.as_deref().unwrap(), &[EdgeKind::Calls])?)
            .map(|p| p.len())
            .unwrap_or(0),
        "flow" => black_box(resolve_named_symbol_flow(
            store,
            store,
            q.query.as_deref().unwrap(),
            &NamedSymbolFlowOptions::default(),
            None,
        )?)
        .chains
        .first()
        .map(|c| c.steps.len())
        .unwrap_or(0),
        "dominant" => usize::from(black_box(store.get_dominant_file()?).is_some()),
        other => return Err(format!("unknown benchmark operation {other}").into()),
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if cfg!(debug_assertions) {
        return Err(
            "benchmark requires cargo build --release -p codegraph-graph --example graph_bench"
                .into(),
        );
    }
    let db = std::env::args()
        .nth(1)
        .ok_or("usage: graph_bench <db> < config.json")?;
    let store = Store::open(db, OpenOptions { read_only: true })?;
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let config: Config = serde_json::from_str(&input)?;
    let mut rows = Vec::new();
    for q in &config.queries {
        for _ in 0..config.warmup {
            black_box(run(&store, q)?);
        }
        let mut samples_ms = Vec::new();
        let mut count = 0;
        for _ in 0..config.samples {
            let start = Instant::now();
            for _ in 0..config.repetitions {
                count = run(&store, q)?;
            }
            samples_ms.push(start.elapsed().as_secs_f64() * 1000.0 / config.repetitions as f64);
        }
        rows.push(
            json!({ "op": q.op, "query": q.query, "samplesMs": samples_ms, "resultCount": count }),
        );
    }
    println!("{}", json!({ "engine": "rust-release", "rows": rows }));
    store.close()?;
    Ok(())
}
