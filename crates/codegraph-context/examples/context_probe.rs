//! Same-database differential probe, JSON lines in/out. No Node runtime required.
use codegraph_db::{OpenOptions, Store};
use std::io::{self, BufRead};
#[path = "../tests/support/mod.rs"]
mod support;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let store = Store::open(
        args.get(1).ok_or("usage: context_probe <db> <root>")?,
        OpenOptions { read_only: true },
    )?;
    let root = args.get(2).ok_or("missing root")?;
    for line in io::stdin().lock().lines() {
        let q = serde_json::from_str(&line?)?;
        println!("{}", support::run(&store, root, &q));
    }
    store.close()?;
    Ok(())
}
