//! A module kept in several copies (two Android build flavors, `rc2/…` vs `rcpro2/…`) gives every
//! bare name one candidate per copy. Directory proximity is capped at six shared folders, so on a
//! deep tree the copies score alike and the first indexed won: a call in one copy could land on
//! the other's. The caller's own copy, when it is uniquely closest, must win and score 0.8 (upstream
//! 81559dcc); a caller equidistant from the copies, the caller itself, and a lone candidate keep
//! their scores.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, Node, NodeKind};
use std::fs;

// Six shared segments already reach the proximity cap.
const ROOT: &str = "a/b/c/d/e/f";
const FLAVORS: &str = "platform/apps/mobile/android/app/src";

fn write(dir: &std::path::Path, relative: &str, lines: &[&str]) {
    let path = dir.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, lines.join("\n") + "\n").unwrap();
}

fn python_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for module in ["rc2", "rcpro2"] {
        write(dir.path(), &format!("{ROOT}/{module}/util.py"), &["def compute():", "    return 1"]);
        write(dir.path(), &format!("{ROOT}/{module}/main.py"), &["def run():", "    return compute()"]);
    }
    write(dir.path(), &format!("{ROOT}/rc2/only.py"), &["def solo():", "    return 2"]);
    write(dir.path(), &format!("{ROOT}/rc2/solo_caller.py"), &["def use_solo():", "    return solo()"]);
    // Equidistant from both copies.
    write(dir.path(), &format!("{ROOT}/top.py"), &["def top():", "    return compute()"]);
    dir
}

fn index(dir: &tempfile::TempDir) -> CodeGraph {
    CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap()
}

fn node(cg: &CodeGraph, kind: NodeKind, name: &str, file: &str) -> Node {
    cg.get_nodes_by_kind(kind)
        .unwrap()
        .into_iter()
        .find(|n| n.name == name && n.file_path == file)
        .unwrap_or_else(|| panic!("no {kind:?} {name} in {file}"))
}

/// `(target id, confidence)` of the edges of `kind` leaving `from`.
fn edges(cg: &CodeGraph, from: &Node, kind: EdgeKind) -> Vec<(String, f64)> {
    cg.get_outgoing_edges(&from.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == kind)
        .map(|e| {
            let confidence = e.metadata.as_ref().and_then(|m| m.get("confidence")).and_then(|c| c.as_f64()).expect("a resolved edge carries its confidence");
            (e.target, confidence)
        })
        .collect()
}

#[test]
fn each_module_calls_its_own_copy_at_point_eight() {
    let dir = python_fixture();
    let cg = index(&dir);
    for module in ["rc2", "rcpro2"] {
        let run = node(&cg, NodeKind::Function, "run", &format!("{ROOT}/{module}/main.py"));
        let own = node(&cg, NodeKind::Function, "compute", &format!("{ROOT}/{module}/util.py"));
        assert_eq!(edges(&cg, &run, EdgeKind::Calls), vec![(own.id, 0.8)], "{module}");
    }
}

#[test]
fn a_caller_equidistant_from_both_copies_keeps_the_prior_choice_and_score() {
    let dir = python_fixture();
    let cg = index(&dir);
    let top = node(&cg, NodeKind::Function, "top", &format!("{ROOT}/top.py"));
    let calls = edges(&cg, &top, EdgeKind::Calls);
    assert_eq!(calls.len(), 1, "{calls:?}");
    let copies: Vec<String> = ["rc2", "rcpro2"].iter().map(|m| node(&cg, NodeKind::Function, "compute", &format!("{ROOT}/{m}/util.py")).id).collect();
    assert!(copies.contains(&calls[0].0), "{calls:?}");
    assert_eq!(calls[0].1, 0.7);
}

#[test]
fn a_kotlin_flavor_calls_its_own_copy_of_the_package() {
    let dir = python_fixture();
    for flavor in ["rc2", "rcpro2"] {
        let package = format!("{FLAVORS}/{flavor}/kotlin/com/app/util");
        write(dir.path(), &format!("{package}/Util.kt"), &["package com.app.util", "", "fun compute(): Int = 1"]);
        write(dir.path(), &format!("{package}/Main.kt"), &["package com.app.util", "", "fun run(): Int = compute()"]);
    }
    let cg = index(&dir);
    for flavor in ["rc2", "rcpro2"] {
        let package = format!("{FLAVORS}/{flavor}/kotlin/com/app/util");
        let run = node(&cg, NodeKind::Function, "run", &format!("{package}/Main.kt"));
        let own = node(&cg, NodeKind::Function, "compute", &format!("{package}/Util.kt"));
        assert_eq!(edges(&cg, &run, EdgeKind::Calls), vec![(own.id, 0.8)], "{flavor}");
    }
}

#[test]
fn a_java_flavor_instantiates_its_own_copy_of_the_class() {
    let dir = python_fixture();
    for flavor in ["rc2", "rcpro2"] {
        let package = format!("{FLAVORS}/{flavor}/java/com/app/util");
        write(dir.path(), &format!("{package}/Util.java"), &["package com.app.util;", "", "public class Util {", "  public int compute() { return 1; }", "}"]);
        write(dir.path(), &format!("{package}/Main.java"), &["package com.app.util;", "", "public class Main {", "  int run() { return new Util().compute(); }", "}"]);
    }
    let cg = index(&dir);
    for flavor in ["rc2", "rcpro2"] {
        let package = format!("{FLAVORS}/{flavor}/java/com/app/util");
        let run = node(&cg, NodeKind::Method, "run", &format!("{package}/Main.java"));
        let own = node(&cg, NodeKind::Class, "Util", &format!("{package}/Util.java"));
        let made: Vec<String> = edges(&cg, &run, EdgeKind::Instantiates).into_iter().map(|(id, _)| id).collect();
        assert_eq!(made, vec![own.id], "{flavor}");
    }
}

#[test]
fn a_match_of_the_caller_itself_stays_at_the_proximity_score() {
    // Recursion, or a receiver the call lost: the caller's own copy being the caller says nothing
    // of which copy was meant.
    let dir = python_fixture();
    for module in ["rc2", "rcpro2"] {
        write(dir.path(), &format!("{ROOT}/{module}/walk.py"), &["def walk(n):", "    return walk(n - 1)"]);
    }
    let cg = index(&dir);
    let walk = node(&cg, NodeKind::Function, "walk", &format!("{ROOT}/rcpro2/walk.py"));
    assert_eq!(edges(&cg, &walk, EdgeKind::Calls), vec![(walk.id.clone(), 0.7)]);
}

#[test]
fn a_single_candidate_name_is_unaffected() {
    let dir = python_fixture();
    let cg = index(&dir);
    let caller = node(&cg, NodeKind::Function, "use_solo", &format!("{ROOT}/rc2/solo_caller.py"));
    let solo = node(&cg, NodeKind::Function, "solo", &format!("{ROOT}/rc2/only.py"));
    assert_eq!(edges(&cg, &caller, EdgeKind::Calls), vec![(solo.id, 0.9)]);
}
