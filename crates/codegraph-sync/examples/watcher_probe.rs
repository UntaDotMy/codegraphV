//! Clock-driven scheduler probe shared with the portable golden suite.
use codegraph_sync::watcher::state::WatchState;
use std::io::{self, BufRead};
#[path = "../tests/support/watcher_model.rs"]
mod model;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut state = WatchState::new(2000);
    let mut request = None;
    for line in io::stdin().lock().lines() {
        let query = serde_json::from_str(&line?)?;
        println!("{}", model::step(&mut state, &mut request, &query));
    }
    Ok(())
}
