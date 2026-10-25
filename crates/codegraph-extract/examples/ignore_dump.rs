//! Prints what the Rust node-ignore port says for a case corpus built by
//! `scripts/rust-golden/ignore-diff.mjs`; diff against that script's `run`.

use codegraph_extract::scan::ignore::{Ignore, IgnoreError};

fn classify(e: &IgnoreError) -> &'static str {
    match e {
        IgnoreError::Empty => "E:empty",
        IgnoreError::NotRelative(_) => "E:rel",
        IgnoreError::Regex(_) => "E:regex",
    }
}

fn main() {
    let file = std::env::args().nth(1).expect("usage: ignore_dump <cases.json>");
    let cases: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
    let mut out = String::new();
    for c in cases.as_array().unwrap() {
        let id = c["id"].as_str().unwrap();
        let build = || {
            let mut ig = Ignore::new();
            match c["kind"].as_str().unwrap() {
                "text" => {
                    ig.add(c["patterns"].as_str().unwrap());
                }
                _ => {
                    let list: Vec<&str> = c["patterns"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
                    ig.add_patterns(&list);
                }
            }
            ig
        };
        let (a, b) = (build(), build());
        for p in c["paths"].as_array().unwrap() {
            let p = p.as_str().unwrap();
            let x = match a.ignores(p) {
                Ok(true) => "I".to_string(),
                Ok(false) => "N".to_string(),
                Err(e) => classify(&e).to_string(),
            };
            let y = match b.test(p) {
                Ok(r) => format!("{}{}", if r.ignored { "I" } else { "N" }, if r.unignored { "U" } else { "-" }),
                Err(e) => classify(&e).to_string(),
            };
            out.push_str(&format!("{id}\t{}\t{x}\t{y}\n", serde_json::to_string(p).unwrap()));
        }
    }
    print!("{out}");
}
