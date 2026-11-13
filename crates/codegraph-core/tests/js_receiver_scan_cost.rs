//! What the JS/TS local-binding scans cost per distinct name, counted in regex compiles. A
//! pattern built from a name compiles once per distinct name, with JIT (about half a millisecond
//! each), so a check every reference takes must not add one.
//!
//! BUG-301: a call whose receiver is no browser host global must not run the scan at all, which
//! made v0.0.10 resolve JavaScript and TypeScript about two thirds slower than v0.0.9.
//! BUG-303: the scan itself must not compile a destructured-parameter pattern per name.
use codegraph_core::CodeGraph;
use std::fs;
use std::sync::Mutex;

/// The compile counter is process-wide: the tests here must not index at the same time.
static SERIAL: Mutex<()> = Mutex::new(());

/// Index `files`; return how many regexes the run compiled.
fn compiles_indexing(files: &[(&str, String)]) -> usize {
    let dir = tempfile::tempdir().unwrap();
    for (name, source) in files {
        fs::write(dir.path().join(name), source).unwrap();
    }
    let before = codegraph_jsre::compiled_count();
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let compiled = codegraph_jsre::compiled_count() - before;
    drop(cg);
    compiled
}

/// One file calling a method on `receivers` distinct, undeclared receivers.
fn receiver_calls(receivers: usize) -> Vec<(&'static str, String)> {
    let mut source = String::from("function run() {\n");
    for i in 0..receivers {
        source.push_str(&format!("  receiver{i}.go();\n"));
    }
    source.push_str("}\n");
    vec![("app.js", source)]
}

/// One file calling `names` distinct functions that another file declares, so each call's only
/// candidate is in another file and the caller's file is scanned for a local binding of it.
fn cross_file_calls(names: usize) -> Vec<(&'static str, String)> {
    let mut caller = String::from("function run() {\n");
    let mut callee = String::new();
    for i in 0..names {
        caller.push_str(&format!("  helper{i}();\n"));
        callee.push_str(&format!("function helper{i}() {{}}\n"));
    }
    caller.push_str("}\n");
    vec![("app.js", caller), ("helpers.js", callee)]
}

#[test]
fn receivers_that_are_no_host_global_compile_no_pattern_of_their_own() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // The first run compiles the fixed patterns every index uses; the second sees 20 names, the
    // third the same 20 and 200 new ones.
    compiles_indexing(&receiver_calls(1));
    let few = compiles_indexing(&receiver_calls(20));
    let many = compiles_indexing(&receiver_calls(220));
    let per_new_name = many.saturating_sub(few) as f64 / 200.0;
    assert!(
        per_new_name < 1.0,
        "200 more receiver names compiled {} more regexes ({per_new_name:.1} per name; 20 names: {few}, 220: {many})",
        many.saturating_sub(few)
    );
}

#[test]
fn a_local_binding_scan_compiles_only_the_declaration_patterns_of_a_name() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    compiles_indexing(&cross_file_calls(1));
    let few = compiles_indexing(&cross_file_calls(20));
    let many = compiles_indexing(&cross_file_calls(220));
    let per_new_name = many.saturating_sub(few) as f64 / 200.0;
    // v0.0.10 compiled 6.33 per name here: v0.0.9's declaration, function, parameter-list and
    // call-site patterns plus a destructured-parameter pattern, which per-file spans replace.
    assert!(
        per_new_name < 5.5,
        "200 more called names compiled {} more regexes ({per_new_name:.2} per name; 20 names: {few}, 220: {many})",
        many.saturating_sub(few)
    );
}
