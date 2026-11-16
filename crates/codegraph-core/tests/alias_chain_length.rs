//! Upstream 329dd12 (#3215), `__tests__/alias-chain-length.test.ts`: a walk along a chain of
//! aliases follows it to its end, so it can't take a stack frame per step — a generated chain
//! thousands of aliases long ran the stack out and failed the whole index. TypeScript's and
//! Lua's walks have one place to go at each step; Pascal's can branch and stays bounded.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

fn indexed(files: &[(String, String)]) -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    for (file, text) in files {
        let path = dir.path().join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

/// What the calls written in `name` (or the file's own top level) reach, as `file::qualified`.
fn calls(cg: &CodeGraph, file: &str, name: Option<&str>) -> Vec<String> {
    let nodes = cg.store().get_all_nodes().unwrap();
    let mut out: Vec<String> = nodes
        .iter()
        .filter(|n| n.file_path == file && name.map_or(n.kind == NodeKind::File, |w| n.name == w))
        .flat_map(|n| cg.store().get_outgoing_edges(&n.id, &[EdgeKind::Calls], None).unwrap())
        .filter_map(|e| nodes.iter().find(|n| n.id == e.target))
        .map(|n| format!("{}::{}", n.file_path, n.qualified_name))
        .collect();
    out.sort();
    out
}

#[test]
fn typescript_twenty_thousand_aliases_index_and_both_ends_reach_the_class() {
    let mut ts = String::from("export class G { m() {} }\nexport class F { m() {} }\nexport type T0 = F;\n");
    for i in 1..20_000 {
        ts += &format!("export type T{i} = T{};\n", i - 1);
    }
    ts += "export function first(x: T19999) { x.m(); }\nexport function second(y: T0) { y.m(); }\n";
    let started = std::time::Instant::now();
    let (_dir, cg) = indexed(&[("chain.ts".into(), ts)]);
    assert_eq!(calls(&cg, "chain.ts", Some("first")), ["chain.ts::F::m"]);
    assert_eq!(calls(&cg, "chain.ts", Some("second")), ["chain.ts::F::m"]);
    assert!(started.elapsed().as_secs() < 90);
}

#[test]
fn lua_a_member_re_exported_through_2500_modules_reaches_its_function_from_either_end() {
    let n = 2_500;
    let mut files: Vec<(String, String)> = (1..n)
        .map(|i| (format!("m{i}.lua"), format!("local m{0} = require(\"m{0}\")\nreturn {{ check = m{0}.check }}\n", i + 1)))
        .collect();
    files.push((format!("m{n}.lua"), "local M = {}\nfunction M.check() return true end\nreturn M\n".into()));
    files.push(("main.lua".into(), format!("local a = require(\"m1\").check\nlocal b = require(\"m{}\").check\na()\nb()\n", n - 1)));
    let (_dir, cg) = indexed(&files);
    let want = format!("m{n}.lua::M::check");
    assert_eq!(calls(&cg, "main.lua", None), [want.clone(), want]);
}

fn pascal(n: usize) -> String {
    let mut pas = String::from("unit U;\n\ninterface\n\ntype\n  TF = class\n    procedure Run;\n  end;\n  TG = class\n    procedure Run;\n  end;\n  TA0 = TF;\n");
    for i in 1..n {
        pas += &format!("  TA{i} = TA{};\n", i - 1);
    }
    pas += &format!(
        "  THolder = class\n    FA: TA{};\n    FE: TA0;\n  end;\n  TUser = class\n    H: THolder;\n    procedure One;\n    procedure Two;\n  end;\n\nimplementation\n\n",
        n - 1
    );
    pas + "procedure TF.Run;\nbegin\nend;\n\nprocedure TG.Run;\nbegin\nend;\n\nprocedure TUser.One;\nbegin\n  H.FA.Run;\nend;\n\nprocedure TUser.Two;\nbegin\n  H.FE.Run;\nend;\n\nend.\n"
}

#[test]
fn pascal_900_aliases_within_the_bound_reach_the_class_from_either_end() {
    let (_dir, cg) = indexed(&[("u.pas".into(), pascal(900))]);
    assert_eq!(calls(&cg, "u.pas", Some("One")), ["u.pas::TF::Run"]);
    assert_eq!(calls(&cg, "u.pas", Some("Two")), ["u.pas::TF::Run"]);
}

#[test]
fn pascal_20000_aliases_index_the_far_end_is_past_the_bound_the_near_one_reaches() {
    let (_dir, cg) = indexed(&[("u.pas".into(), pascal(20_000))]);
    assert_eq!(calls(&cg, "u.pas", Some("One")), Vec::<String>::new());
    assert_eq!(calls(&cg, "u.pas", Some("Two")), ["u.pas::TF::Run"]);
}
