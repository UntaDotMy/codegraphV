//! Upstream 329dd12 (#3215), `__tests__/resolution-order-independence.test.ts`: what a walk
//! answers can't depend on who asked first. With a pool, who asks first is which worker gets
//! which chunk, so these ask in both orders, each on a fresh resolver.
use codegraph_core::CodeGraph;
use codegraph_resolve::{c_macro_renames::c_macro_rename, resolver::ReferenceResolver};
use codegraph_types::{EdgeKind, Language, NodeKind};
use std::fs;

fn indexed(files: &[(&str, &str)]) -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    for (file, text) in files {
        let path = dir.path().join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

fn calls(cg: &CodeGraph, file: &str, name: &str) -> Vec<String> {
    let nodes = cg.store().get_all_nodes().unwrap();
    let mut out: Vec<String> = nodes
        .iter()
        .filter(|n| n.file_path == file && n.name == name && n.kind != NodeKind::File)
        .flat_map(|n| cg.store().get_outgoing_edges(&n.id, &[EdgeKind::Calls], None).unwrap())
        .filter_map(|e| nodes.iter().find(|n| n.id == e.target))
        .map(|n| n.qualified_name.clone())
        .collect();
    out.sort();
    out
}

const C_FILES: &[(&str, &str)] = &[
    ("types.h", "struct hisds { int len; };\nstruct sds { int other; };\n"),
    ("impl.c", "#define sds hisds\nstruct holder {\n  sds value;\n};\n"),
    ("main.c", "#include \"types.h\"\n#include \"impl.c\"\nstruct user {\n  sds name;\n};\n"),
    ("own.c", "#include \"types.h\"\n#define sds hisds\nstruct mine {\n  sds name;\n};\n"),
    ("plain.c", "#include \"types.h\"\nstruct theirs {\n  sds name;\n};\n"),
];

/// What `sds` is renamed to in each file, asked in `order` on a fresh resolver.
fn renames(cg: &CodeGraph, root: &std::path::Path, order: &[(&str, usize)]) -> Vec<(String, Option<String>)> {
    let resolver = ReferenceResolver::new(root.to_str().unwrap(), cg.store().clone());
    let ctx = resolver.context();
    let mut out: Vec<(String, Option<String>)> =
        order.iter().map(|(file, line)| ((*file).to_owned(), c_macro_rename("sds", file, *line, Language::C, ctx))).collect();
    out.sort();
    out
}

#[test]
fn a_source_file_another_includes_renames_for_its_includer_too_whichever_asks_first() {
    let (dir, cg) = indexed(C_FILES);
    let forward = renames(&cg, dir.path(), &[("main.c", 4), ("impl.c", 3)]);
    let reverse = renames(&cg, dir.path(), &[("impl.c", 3), ("main.c", 4)]);
    assert_eq!(forward, [("impl.c".into(), Some("hisds".into())), ("main.c".into(), Some("hisds".into()))]);
    assert_eq!(reverse, forward);
}

#[test]
fn a_source_file_nothing_includes_renames_for_itself_alone_whichever_asks_first() {
    let (dir, cg) = indexed(C_FILES);
    let forward = renames(&cg, dir.path(), &[("own.c", 4), ("plain.c", 3)]);
    let reverse = renames(&cg, dir.path(), &[("plain.c", 3), ("own.c", 4)]);
    assert_eq!(forward, [("own.c".into(), Some("hisds".into())), ("plain.c".into(), None)]);
    assert_eq!(reverse, forward);
}

#[test]
fn alias_chains_that_come_back_to_themselves_name_nothing() {
    let classes = "export class G { m() { return 2; } }\nexport class F { m() { return 1; } }\n";
    let types = format!("{classes}export type A = B;\nexport type B = C;\nexport type C = A;\n");
    let (_d, cg) = indexed(&[
        ("types.ts", types.as_str()),
        ("use.ts", "import { A, C } from './types';\nexport function first(x: A) { return x.m(); }\nexport function second(y: C) { return y.m(); }\n"),
    ]);
    assert_eq!(calls(&cg, "use.ts", "first"), Vec::<String>::new());
    assert_eq!(calls(&cg, "use.ts", "second"), Vec::<String>::new());

    let reexport = |next: &str| format!("local {next} = require(\"{next}\")\nreturn {{ check = {next}.check }}\n");
    let (m1, m2, m3, m4) = (reexport("m2"), reexport("m3"), reexport("m4"), reexport("m1"));
    let (_d, cg) = indexed(&[
        ("m1.lua", &m1), ("m2.lua", &m2), ("m3.lua", &m3), ("m4.lua", &m4),
        ("m5.lua", "local function check() return 1 end\nreturn { check = check }\n"),
        ("use.lua", "local m1 = require(\"m1\")\nlocal m4 = require(\"m4\")\nlocal a = m1.check\nlocal b = m4.check\nlocal function one() return a() end\nlocal function two() return b() end\nreturn { one = one, two = two }\n"),
    ]);
    assert_eq!(calls(&cg, "use.lua", "one"), Vec::<String>::new());
    assert_eq!(calls(&cg, "use.lua", "two"), Vec::<String>::new());
}

fn pascal_unit(aliases: &str) -> String {
    format!("unit U;\n\ninterface\n\ntype\n  TF = class\n    procedure Run;\n  end;\n  TG = class\n    procedure Run;\n  end;\n{aliases}\n  THolder = class\n    FA: TA;\n    FE: TE;\n  end;\n  TUser = class\n    H: THolder;\n    procedure One;\n    procedure Two;\n  end;\n\nimplementation\n\nprocedure TF.Run;\nbegin\nend;\n\nprocedure TG.Run;\nbegin\nend;\n\nprocedure TUser.One;\nbegin\n  H.FA.Run;\nend;\n\nprocedure TUser.Two;\nbegin\n  H.FE.Run;\nend;\n\nend.\n")
}

#[test]
fn pascal_alias_chains_cycles_and_ifdef_arms() {
    let (_d, cg) = indexed(&[("u.pas", &pascal_unit("  TE = TD;\n  TD = TC;\n  TC = TB;\n  TB = TA;\n  TA = TE;"))]);
    assert_eq!(calls(&cg, "u.pas", "One"), Vec::<String>::new(), "a cycle names nothing");
    assert_eq!(calls(&cg, "u.pas", "Two"), Vec::<String>::new());
    let (_d, cg) = indexed(&[(
        "u.pas",
        &pascal_unit("  {$IFDEF ONE}\n  TE = TA;\n  {$ELSE}\n  TE = TF;\n  {$ENDIF}\n  TD = TE;\n  TC = TD;\n  TB = TC;\n  TA = TB;"),
    )]);
    assert_eq!(calls(&cg, "u.pas", "One"), ["TF::Run"], "an alias declared twice goes on to the arm that leads somewhere");
    assert_eq!(calls(&cg, "u.pas", "Two"), ["TF::Run"]);
}
