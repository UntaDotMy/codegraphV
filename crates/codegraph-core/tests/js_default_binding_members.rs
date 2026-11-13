//! A member read through require() or a default import is that module's export
//! (upstream 39fc0839, #3162).
use codegraph_core::{CodeGraph, IndexOptions};
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;
use std::path::Path;

fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (relative, content) in files {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

fn index(files: &[(&str, &str)]) -> (tempfile::TempDir, CodeGraph) {
    let root = tempfile::tempdir().unwrap();
    write_files(root.path(), files);
    let cg = CodeGraph::init_and_index(root.path().to_str().unwrap(), None).unwrap();
    (root, cg)
}

fn calls_of(cg: &CodeGraph, name: &str) -> Vec<String> {
    let nodes = cg.store().get_all_nodes().unwrap();
    let run = nodes.iter().find(|n| n.name == name && n.kind == NodeKind::Function).unwrap_or_else(|| panic!("no function {name}"));
    let mut out = Vec::new();
    for edge in cg.get_outgoing_edges(&run.id).unwrap() {
        if edge.kind != EdgeKind::Calls {
            continue;
        }
        let Some(target) = cg.get_node(&edge.target).unwrap() else { continue };
        out.push(format!("{} {}:{}", edge.line.unwrap_or(0), target.file_path, target.qualified_name));
    }
    out.sort();
    out
}

fn caller(binding: &str, members: &[&str]) -> String {
    let body = members.iter().map(|m| format!("  named.{m}();")).collect::<Vec<_>>().join("\n");
    format!("{binding}\nfunction run() {{\n{body}\n}}\nmodule.exports.run = run;\n")
}

#[test]
fn a_member_reaches_the_export_of_that_name() {
    let shapes = [
        ("anonymous functions", "exports.first = function () {};\nexports.second = function () {};\n", "m.js:second"),
        ("named functions", "exports.first = function first() {};\nexports.second = function second() {};\n", "m.js:second"),
        ("a function of another name", "exports.first = function one() {};\nexports.second = function two() {};\n", "m.js:two"),
        ("local functions", "function first() {}\nfunction second() {}\nexports.first = first;\nexports.second = second;\n", "m.js:second"),
        ("an object of them", "function first() {}\nfunction second() {}\nmodule.exports = { first, second };\n", "m.js:second"),
    ];
    for (shape, module, target) in shapes {
        let (_root, cg) = index(&[("m.js", module), ("use.js", &caller("const named = require('./m');", &["second"]))]);
        assert_eq!(calls_of(&cg, "run"), vec![format!("3 {target}")], "{shape}");
        cg.close();
    }

    let (_root, cg) = index(&[
        ("index.js", "module.exports = require('./lib/m');\n"),
        ("lib/m.js", "exports.first = function () {};\nexports.second = function () {};\n"),
        ("use.js", &caller("const named = require('./index');", &["second"])),
    ]);
    assert_eq!(calls_of(&cg, "run"), vec!["3 lib/m.js:second".to_string()], "{:?}", calls_of(&cg, "run"));
    cg.close();

    let (_root, cg) = index(&[
        ("m.js", "exports.first = function () {};\nexports.second = function () {};\n"),
        ("use.mjs", "import named from './m.js';\nexport function run() {\n  named.second();\n}\n"),
    ]);
    assert_eq!(calls_of(&cg, "run"), vec!["3 m.js:second".to_string()], "{:?}", calls_of(&cg, "run"));
    cg.close();

    let (_root, cg) = index(&[
        ("plain.mjs", "export function first() {}\nexport function second() {}\n"),
        ("made.mjs", "export function first() {}\nexport function second() {}\nexport default make();\nfunction make() { return {}; }\n"),
        (
            "use.js",
            "const plain = require('./plain.mjs');\nconst made = require('./made.mjs');\nfunction run() {\n  plain.second();\n  made.second();\n}\nmodule.exports.run = run;\n",
        ),
    ]);
    assert_eq!(
        calls_of(&cg, "run"),
        vec!["4 plain.mjs:second".to_string(), "5 made.mjs:second".to_string()],
        "{:?}",
        calls_of(&cg, "run")
    );
    cg.close();
}

#[test]
fn a_missing_name_links_nothing_and_a_stated_assignment_wins() {
    let (_root, cg) = index(&[
        ("typings/passport.d.ts", "declare module 'passport-x' {\n  const passport: { use(): void };\n  export = passport;\n}\n"),
        ("use.ts", "import passport from 'passport-x';\nexport function run() {\n  passport.use();\n}\n"),
    ]);
    assert_eq!(calls_of(&cg, "run"), vec!["3 typings/passport.d.ts:passport".to_string()], "{:?}", calls_of(&cg, "run"));
    cg.close();

    let (_root, cg) = index(&[
        ("m.js", "exports.first = function () {};\nexports.second = function () {};\n"),
        ("plain.mjs", "export function first() {}\n"),
        (
            "use.js",
            "const named = require('./m');\nconst plain = require('./plain.mjs');\nfunction run() {\n  named.missing();\n  plain.missing();\n}\nmodule.exports.run = run;\n",
        ),
    ]);
    assert_eq!(calls_of(&cg, "run"), Vec::<String>::new(), "{:?}", calls_of(&cg, "run"));
    cg.close();

    let (_root, cg) = index(&[
        ("m.js", "function main() {}\nmodule.exports = main;\nmodule.exports.extra = function () {};\n"),
        ("use.js", "const named = require('./m');\nfunction run() {\n  named();\n  named.extra();\n}\nmodule.exports.run = run;\n"),
    ]);
    assert_eq!(calls_of(&cg, "run"), vec!["3 m.js:main".to_string(), "4 m.js:extra".to_string()], "{:?}", calls_of(&cg, "run"));
    cg.close();
}

#[test]
fn sync_follows_the_export_appearing_and_leaving() {
    let root = tempfile::tempdir().unwrap();
    write_files(root.path(), &[
        ("m.js", "exports.first = function () {};\n"),
        ("use.js", &caller("const named = require('./m');", &["second"])),
    ]);
    let mut cg = CodeGraph::init_and_index(root.path().to_str().unwrap(), None).unwrap();
    assert_eq!(calls_of(&cg, "run"), Vec::<String>::new(), "{:?}", calls_of(&cg, "run"));
    fs::write(root.path().join("m.js"), "exports.first = function () {};\nexports.second = function () {};\n").unwrap();
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(calls_of(&cg, "run"), vec!["3 m.js:second".to_string()], "{:?}", calls_of(&cg, "run"));
    fs::write(root.path().join("m.js"), "exports.first = function () {};\n").unwrap();
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(calls_of(&cg, "run"), Vec::<String>::new(), "{:?}", calls_of(&cg, "run"));
    cg.close();
}

#[test]
fn a_stated_default_keeps_its_own_member() {
    let (_root, cg) = index(&[
        ("stated.ts", "export class Widget { static make() {} }\nexport function make() {}\nexport default Widget;\n"),
        ("instance.ts", "export class Store { load() {} }\nexport default new Store();\n"),
        ("value.ts", "class Cache { read() {} }\nconst cache: Cache = new Cache();\nexport function read() {}\nexport default cache;\n"),
        (
            "use.ts",
            "import Widget from './stated';\nimport store from './instance';\nimport cache from './value';\nexport function run() {\n  Widget.make();\n  store.load();\n  cache.read();\n}\n",
        ),
    ]);
    assert_eq!(
        calls_of(&cg, "run"),
        vec!["5 stated.ts:Widget::make".to_string(), "6 instance.ts:Store::load".to_string(), "7 value.ts:Cache::read".to_string()],
        "{:?}",
        calls_of(&cg, "run")
    );
    cg.close();
}
