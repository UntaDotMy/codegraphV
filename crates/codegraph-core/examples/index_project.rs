//! Orchestrator harness (driven by `scripts/rust-golden/index.mjs`).
//!
//!   index_project <project-root> <db> [sync <scoped-path>...]
//!
//! Creates the database, runs `ExtractionOrchestrator::index_all` over the project — extraction
//! and storage only, as `CodeGraph.orchestrator.indexAll()` does in the TypeScript engine — and
//! prints the `IndexResult` as JSON. With `sync`, runs a (scoped) sync over an existing database
//! instead and prints the `SyncResult`.

use codegraph_core::orchestrator::ExtractionOrchestrator;
use codegraph_db::{OpenOptions, Store};
use std::rc::Rc;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: index_project <project-root> <db> [sync [paths...]]");
        std::process::exit(2);
    }
    let syncing = args.get(3).is_some_and(|a| a == "sync");
    let store = Rc::new(if syncing { Store::open(&args[2], OpenOptions::default()).expect("open database") } else { Store::initialize(&args[2]).expect("create database") });
    let orchestrator = ExtractionOrchestrator::new(&args[1], store.clone());
    let json = if syncing {
        let scoped: Vec<String> = args[4..].to_vec();
        let result = orchestrator.sync(None, if scoped.is_empty() { None } else { Some(&scoped) }, None, None).expect("sync");
        serde_json::to_string(&result)
    } else {
        let result = orchestrator.index_all(None, None, false).expect("index");
        serde_json::to_string(&result)
    };
    println!("{}", json.expect("serialize"));
    drop(orchestrator);
    if let Ok(store) = Rc::try_unwrap(store) {
        let _ = store.close();
    }
}
