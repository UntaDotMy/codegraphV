//! An object-like macro can rename a type (upstream cb1e13d8). hiredis's sdscompat.h has
//! `#define sds hisds`, so an `sds` member in redis-cli.c, which includes it, is a `hisds`,
//! and a namesake typedef is one that file never compiles.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, Node, NodeKind};
use std::fs;
use std::path::Path;

fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (rel, content) in files {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }
}

fn index(files: &[(&str, &str)]) -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    write_files(dir.path(), files);
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

fn rel(path: &str) -> String {
    path.replace('\\', "/")
}

fn holder(cg: &CodeGraph, name: &str) -> Node {
    let mut found: Vec<Node> = cg
        .get_nodes_by_name(name)
        .unwrap()
        .into_iter()
        .filter(|n| matches!(n.kind, NodeKind::Struct | NodeKind::TypeAlias | NodeKind::Class))
        .collect();
    assert_eq!(found.len(), 1, "{name}: {found:#?}");
    found.remove(0)
}

fn referenced(cg: &CodeGraph, name: &str) -> Vec<String> {
    let node = holder(cg, name);
    let mut labels: Vec<String> = cg
        .get_outgoing_edges(&node.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::References)
        .map(|e| {
            let target = cg.get_node(&e.target).unwrap().unwrap();
            format!("{} {} @{}", target.kind, target.name, rel(&target.file_path))
        })
        .collect();
    labels.sort();
    labels
}

const LIBRARY: [(&str, &str); 3] = [
    ("deps/lib/str.h", "typedef char *histr;\n"),
    (
        "deps/lib/compat.h",
        "#ifndef COMPAT_H\n#define COMPAT_H\n#define str histr /* the library's */\n#endif\n",
    ),
    ("src/str.h", "typedef char *str;\n"),
];

const HISTR: &str = "type_alias histr @deps/lib/str.h";
const STR: &str = "type_alias str @src/str.h";

#[test]
fn links_what_the_macro_names_in_a_file_that_includes_it() {
    let mut files = LIBRARY.to_vec();
    files.push((
        "src/client.c",
        "#include <compat.h>\n#include <str.h>\nstruct client {\n    str name;\n    str *names;\n};\ntypedef str client_name;\n",
    ));
    files.push(("src/server.c", "#include \"str.h\"\nstruct server { str name; };\n"));
    let (_dir, cg) = index(&files);
    assert_eq!(referenced(&cg, "client"), [HISTR, HISTR]);
    assert_eq!(referenced(&cg, "client_name"), [HISTR]);
    assert_eq!(referenced(&cg, "server"), [STR]);
}

#[test]
fn reaches_through_an_include_and_holds_for_cpp() {
    let mut files = LIBRARY.to_vec();
    files.push(("src/indirect.h", "#include <compat.h>\n"));
    files.push(("src/third.c", "#include \"indirect.h\"\nstruct third { str name; };\n"));
    files.push(("src/fourth.cpp", "#include \"indirect.h\"\nclass fourth { str name; };\n"));
    let (_dir, cg) = index(&files);
    assert_eq!(referenced(&cg, "third"), [HISTR]);
    assert_eq!(referenced(&cg, "fourth"), [HISTR]);
}

#[test]
fn a_file_can_rename_a_type_for_itself_from_its_define_on() {
    let mut files = LIBRARY.to_vec();
    files.push((
        "src/own.c",
        "#include \"str.h\"\nstruct before { str name; };\n#define str histr\nstruct after { str name; };\n",
    ));
    let (_dir, cg) = index(&files);
    assert_eq!(referenced(&cg, "before"), [STR]);
    assert_eq!(referenced(&cg, "after"), [HISTR]);
}

#[test]
fn starts_at_the_include_as_the_preprocessor_does() {
    let mut files = LIBRARY.to_vec();
    files.push((
        "src/later.c",
        "#include \"str.h\"\nstruct before { str name; };\n#include <compat.h>\nstruct after { str name; };\n",
    ));
    let (_dir, cg) = index(&files);
    assert_eq!(referenced(&cg, "before"), [STR]);
    assert_eq!(referenced(&cg, "after"), [HISTR]);
}

#[test]
fn holds_only_where_the_flag_around_it_is_set() {
    let (_dir, cg) = index(&[
        ("src/object.h", "typedef struct redisObject { int x; } robj;\n"),
        (
            "src/redismodule.h",
            "typedef struct RedisModuleString RedisModuleString;\n#if defined REDISMODULE_CORE\n#include \"object.h\"\n#define RedisModuleString robj\n#endif\nstruct header_holder { RedisModuleString *s; };\n",
        ),
        ("src/server.h", "#define REDISMODULE_CORE 1\n#include \"redismodule.h\"\n"),
        ("src/core.c", "#include \"server.h\"\nstruct core_holder { RedisModuleString *s; };\n"),
        ("modules/module.c", "#include \"../src/redismodule.h\"\nstruct module_holder { RedisModuleString *s; };\n"),
    ]);
    assert_eq!(referenced(&cg, "core_holder"), ["struct robj @src/object.h"]);
    assert_eq!(referenced(&cg, "module_holder"), ["type_alias RedisModuleString @src/redismodule.h"]);
    assert_eq!(referenced(&cg, "header_holder"), ["type_alias RedisModuleString @src/redismodule.h"]);
}

#[test]
fn leaves_the_name_written_when_the_macro_names_no_project_type_or_two() {
    let mut files = LIBRARY.to_vec();
    files.push((
        "src/outside.c",
        "#include \"str.h\"\n#define str outside_type\nstruct outside { str name; };\n",
    ));
    files.push((
        "src/both.h",
        "#ifdef ONE\n#define str histr\n#else\n#define str histr_other\n#endif\n",
    ));
    files.push(("deps/lib/other.h", "typedef char *histr_other;\n"));
    files.push(("src/both.c", "#define ONE\n#include \"both.h\"\n#include \"str.h\"\nstruct both { str name; };\n"));
    let (_dir, cg) = index(&files);
    assert_eq!(referenced(&cg, "outside"), [STR]);
    assert_eq!(referenced(&cg, "both"), [STR]);
}

#[test]
fn never_reads_a_macro_in_a_block_comment() {
    let mut files = LIBRARY.to_vec();
    files.push(("src/commented.h", "/*\n#define str histr\n*/\n"));
    files.push(("src/commented.c", "#include \"commented.h\"\n#include \"str.h\"\nstruct commented { str name; };\n"));
    let (_dir, cg) = index(&files);
    assert_eq!(referenced(&cg, "commented"), [STR]);
}
