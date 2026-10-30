//! A C function-pointer table links only to C and C++ functions (upstream c3069670): a same-named
//! Python or Rust function is never what a C table registers, and must not displace the C one.
use codegraph_core::CodeGraph;
use codegraph_types::{Node, NodeKind};
use std::{collections::BTreeSet, fs, path::Path};

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn functions_named(cg: &CodeGraph, name: &str) -> Vec<Node> {
    cg.get_nodes_by_name(name).unwrap().into_iter().filter(|n| n.kind == NodeKind::Function).collect()
}

/// The id of every node a function-pointer dispatch edge leaves `caller` for.
fn dispatch_targets(cg: &CodeGraph, caller: &str) -> BTreeSet<String> {
    let callers = functions_named(cg, caller);
    assert_eq!(callers.len(), 1, "expected one `{caller}`: {callers:?}");
    let mut targets = BTreeSet::new();
    for edge in cg.get_outgoing_edges(&callers[0].id).unwrap() {
        let synthesized = edge.metadata.as_ref().and_then(|m| m.get("synthesizedBy")).and_then(|v| v.as_str());
        if synthesized != Some("fn-pointer-dispatch") {
            continue;
        }
        targets.insert(edge.target.clone());
    }
    targets
}

fn index(root: &Path) -> CodeGraph {
    CodeGraph::init_and_index(root.to_str().unwrap(), None).unwrap()
}

fn mixed_language_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "ops.h", "struct ops { int (*read)(int); int (*probe)(int); };\n");
    write(root, "impl.c", "int impl_read(int fd) { return fd; }\n");
    write(
        root,
        "ops.c",
        "#include \"ops.h\"\nint impl_read(int fd);\nstatic struct ops the_ops = { .read = impl_read, .probe = gen_probe };\nint use(struct ops *o, int fd) { return o->read(fd) + o->probe(fd); }\n",
    );
    // `aaa/` sorts before `impl.c`, so the Python function comes first among same-named candidates.
    write(root, "aaa/gen.py", "def impl_read():\n    return 1\n\ndef gen_probe():\n    return 2\n");
    dir
}

#[test]
fn registrations_link_to_the_c_function_never_a_same_named_python_one() {
    let dir = mixed_language_project();
    let cg = index(dir.path());
    let c_impl = functions_named(&cg, "impl_read").into_iter().find(|n| n.file_path == "impl.c").expect("the C impl_read is indexed");
    let py_impl = functions_named(&cg, "impl_read").into_iter().find(|n| n.file_path.ends_with("gen.py")).expect("the Python impl_read is indexed");
    let targets = dispatch_targets(&cg, "use");
    assert!(targets.contains(&c_impl.id), "the table entry should reach the C handler: {targets:?}");
    assert!(!targets.contains(&py_impl.id), "the Python function displaced the C handler: {targets:?}");
}

#[test]
fn a_registered_name_found_only_in_another_language_links_nowhere() {
    let dir = mixed_language_project();
    let cg = index(dir.path());
    let py_probe = functions_named(&cg, "gen_probe").into_iter().find(|n| n.file_path.ends_with("gen.py")).expect("the Python gen_probe is indexed");
    let targets = dispatch_targets(&cg, "use");
    assert!(!targets.contains(&py_probe.id), "a C table linked to a Python function: {targets:?}");
}

#[test]
fn c_and_cpp_registrations_keep_their_edges() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "ops.h", "struct ops { int (*read)(int); int (*write)(int); };\n");
    // Two files each define a static `handler`; the table's own file wins.
    write(root, "a.c", "#include \"ops.h\"\nstatic int handler(int fd) { return fd; }\nstatic struct ops a_ops = { .read = handler };\nint run_a(struct ops *o, int fd) { return o->read(fd); }\n");
    write(root, "b.c", "static int handler(int fd) { return fd + 1; }\n");
    write(root, "c.cpp", "struct cops { int (*step)(int); };\nint cpp_step(int fd) { return fd; }\nstatic cops c_ops = { .step = cpp_step };\nint run_c(cops *o, int fd) { return o->step(fd); }\n");
    let cg = index(root);
    let a_handler = functions_named(&cg, "handler").into_iter().find(|n| n.file_path == "a.c").expect("a.c handler");
    assert!(dispatch_targets(&cg, "run_a").contains(&a_handler.id), "the same-file handler should win");
    let cpp_step = functions_named(&cg, "cpp_step").into_iter().next().expect("cpp_step");
    assert!(dispatch_targets(&cg, "run_c").contains(&cpp_step.id), "a C++ registration should link");
}
