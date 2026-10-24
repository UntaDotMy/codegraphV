//! Reference-resolution harness (driven by `scripts/rust-golden/resolve.mjs`).
//!
//!   resolve_db <project-root> <db> [--synth]
//!
//! Opens a database the TypeScript engine populated but did not resolve, runs the Rust
//! resolver over it exactly as `CodeGraph.indexAll` does after extraction (initialize,
//! post-extract, batched resolution, the two deferred passes), and prints the stats as JSON.
//! The edges and remaining references it leaves are then diffed against the TypeScript
//! resolver's run over a copy of the same database.

use codegraph_db::{OpenOptions, Store};
use codegraph_resolve::ReferenceResolver;
use serde_json::json;
use std::rc::Rc;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: resolve_db <project-root> <db>");
        std::process::exit(2);
    }
    let store = Rc::new(Store::open(&args[2], OpenOptions::default()).expect("open database"));
    let synth = args.iter().skip(3).any(|a| a == "--synth");
    let resolver = ReferenceResolver::create(&args[1], store.clone());
    resolver.run_post_extract();
    let stats = resolver.resolve_and_persist_batched(None, 5000, synth).expect("resolve");
    let chained = resolver.resolve_chained_calls_via_conformance().expect("chained");
    let this_member = resolver.resolve_deferred_this_member_refs().expect("this-member");
    let frameworks = resolver.detected_frameworks();
    println!(
        "{}",
        json!({
            "total": stats.total,
            "resolved": stats.resolved,
            "unresolved": stats.unresolved,
            "byMethod": stats.by_method,
            "chained": chained,
            "thisMember": this_member,
            "frameworks": frameworks,
        })
    );
    drop(resolver);
    if let Ok(store) = Rc::try_unwrap(store) {
        let _ = store.close();
    }
}
