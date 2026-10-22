//! JSON-lines differential probe; also used by the persisted golden tests.
use codegraph_db::{OpenOptions, Store};
use std::io::{self, BufRead};

#[path = "../tests/support/mod.rs"]
mod support;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = std::env::args().nth(1).ok_or("usage: graph_probe <db>")?;
    let store = Store::open(db, OpenOptions { read_only: true })?;
    for line in io::stdin().lock().lines() {
        let query: serde_json::Value = serde_json::from_str(&line?)?;
        println!("{}", support::run_query(&store, &query));
    }
    Ok(())
}
