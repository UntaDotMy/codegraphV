//! BUG-301: a JS/TS call whose receiver is no browser host global must not run the per-name
//! local-binding scan. That scan compiles several regexes for each distinct name (JIT, about
//! half a millisecond each), so asking it of every receiver made v0.0.10 resolve JavaScript and
//! TypeScript about two thirds slower than v0.0.9. The host-global set decides first.
use codegraph_core::CodeGraph;
use std::fs;

/// Index one file calling a method on `receivers` distinct, undeclared receivers; return how
/// many regexes the run compiled.
fn compiles_indexing(receivers: usize) -> usize {
    let dir = tempfile::tempdir().unwrap();
    let mut source = String::from("function run() {\n");
    for i in 0..receivers {
        source.push_str(&format!("  receiver{i}.go();\n"));
    }
    source.push_str("}\n");
    fs::write(dir.path().join("app.js"), source).unwrap();
    let before = codegraph_jsre::compiled_count();
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let compiled = codegraph_jsre::compiled_count() - before;
    drop(cg);
    compiled
}

#[test]
fn receivers_that_are_no_host_global_compile_no_pattern_of_their_own() {
    // The first run compiles the fixed patterns every index uses; the second sees 20 names, the
    // third the same 20 and 200 new ones.
    compiles_indexing(1);
    let few = compiles_indexing(20);
    let many = compiles_indexing(220);
    let per_new_name = many.saturating_sub(few) as f64 / 200.0;
    assert!(
        per_new_name < 1.0,
        "200 more receiver names compiled {} more regexes ({per_new_name:.1} per name; 20 names: {few}, 220: {many})",
        many.saturating_sub(few)
    );
}
