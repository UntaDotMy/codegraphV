//! A Rust module loaded with `#[path = "…"]` is the module that declares it
//! (upstream cc179781, #3064; the `use super::*` case is PR #1892 by
//! @seonghobae). Its `super` is the declaring module, wherever the file lies,
//! so `use super::*` brings in that module's items, private ones included, and
//! `crate::` is the declaring file's crate. The path is read from the
//! declaring file's directory, or from an inline module's own directory. A
//! module whose cfg alternatives each name a file, or whose path a `cfg_attr`
//! sets, stays unread. A name only one of a module's cfg alternatives holds is
//! that one's; a name two of them hold depends on the build. A file several
//! declarations load is each one's from its own side, and an example the
//! manifest places at a path of its own is its own crate's root.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

struct Fixture {
    cg: CodeGraph,
    _root: tempfile::TempDir,
}

fn index(files: &[(&str, &str)]) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    for (relative, content) in files {
        let path = root.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let cg = CodeGraph::init_and_index(root.path().to_str().unwrap(), None).unwrap();
    Fixture { cg, _root: root }
}

fn describe(cg: &CodeGraph, id: &str) -> String {
    match cg.get_node(id).unwrap() {
        Some(n) => format!(
            "{} {} {}:{}",
            n.kind.as_str(),
            n.qualified_name,
            n.file_path,
            n.start_line
        ),
        None => format!("? {id}"),
    }
}

fn calls_of(cg: &CodeGraph, file: &str, name: &str) -> Vec<String> {
    let nodes = cg.get_nodes_by_file(file).unwrap();
    let Some(caller) = nodes
        .iter()
        .find(|n| n.name == name && n.kind == NodeKind::Function)
    else {
        return vec![format!("<no function {name} in {file}>")];
    };
    let mut out = Vec::new();
    for edge in cg.get_outgoing_edges(&caller.id).unwrap() {
        if edge.kind == EdgeKind::Calls {
            out.push(describe(cg, &edge.target));
        }
    }
    out.sort();
    out
}

#[derive(Default)]
struct Checks(Vec<String>);

impl Checks {
    fn is(&mut self, case: &str, actual: Vec<String>, expected: &[&str]) {
        if actual != expected {
            self.0.push(format!(
                "{case}\n    expected {expected:?}\n    actual   {actual:?}"
            ));
        }
    }

    fn finish(self) {
        assert!(
            self.0.is_empty(),
            "{} case(s) failed:\n  {}",
            self.0.len(),
            self.0.join("\n  ")
        );
    }
}

fn workspace() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/core\", \"crates/py\"]\nresolver = \"2\"\n"),
        (
            "crates/core/Cargo.toml",
            "[package]\nname = \"core-crate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[[example]]\nname = \"demo\"\npath = \"examples/nested/demo.rs\"\n",
        ),
        (
            "crates/core/src/lib.rs",
            "pub mod scaling;\npub mod platform;\npub mod util;\n\npub fn root_helper() -> i32 {\n    0\n}\n\npub mod gen_user;\npub mod process;\npub mod park;\n",
        ),
        (
            "crates/core/src/process/mod.rs",
            "#[path = \"unix/mod.rs\"]\n#[cfg(unix)]\nmod imp;\n\n#[cfg(unix)]\npub(crate) mod unix {\n    pub(crate) use super::imp::*;\n}\n\n#[path = \"windows.rs\"]\n#[cfg(windows)]\nmod imp;\n\npub(crate) fn via_windows() {\n    imp::only_on_windows();\n}\n\npub(crate) fn via_either() {\n    imp::on_each();\n}\n",
        ),
        (
            "crates/core/src/process/unix/mod.rs",
            "pub(crate) mod orphan;\n\npub(crate) struct GlobalOrphanQueue;\n\npub(crate) fn on_each() {}\n",
        ),
        ("crates/core/src/process/unix/orphan.rs", "pub(crate) trait OrphanQueue {}\n"),
        (
            "crates/core/src/process/windows.rs",
            "pub(crate) fn only_on_windows() {}\n\npub(crate) fn on_each() {}\n",
        ),
        (
            "crates/core/src/park.rs",
            "use crate::process::imp::orphan::OrphanQueue;\nuse crate::process::unix::GlobalOrphanQueue;\n\npub fn park(queue: GlobalOrphanQueue, orphans: &dyn OrphanQueue) {}\n",
        ),
        ("crates/core/src/util.rs", "pub fn helper() -> i32 {\n    10\n}\n"),
        (
            "crates/core/src/gen_user.rs",
            "#[path = \"..generated/gen.rs\"]\nmod gen;\n\npub fn via_gen() -> i32 {\n    gen::make_gen()\n}\n",
        ),
        ("crates/core/src/..generated/gen.rs", "pub fn make_gen() -> i32 {\n    13\n}\n"),
        (
            "crates/core/examples/nested/demo.rs",
            "mod util;\n\nfn main() {\n    let _ = crate::util::helper();\n}\n",
        ),
        ("crates/core/examples/nested/util.rs", "pub fn helper() -> i32 {\n    11\n}\n"),
        (
            "crates/core/tests/a.rs",
            "#[path = \"support/helpers.rs\"]\nmod support;\n\n#[test]\nfn uses_support() {\n    let _ = support::make();\n}\n",
        ),
        (
            "crates/core/tests/b.rs",
            "#[path = \"support/helpers.rs\"]\nmod support;\n\n#[test]\nfn uses_support_too() {\n    let _ = support::make();\n}\n",
        ),
        ("crates/core/tests/support/helpers.rs", "pub fn make() -> i32 {\n    12\n}\n"),
        (
            "crates/core/src/scaling.rs",
            "pub fn predict_rating_multi(x: i32) -> i32 {\n    x + 1\n}\n\nfn private_helper() -> i32 {\n    2\n}\n\n#[cfg(test)]\n#[path = \"../../../tests/unit/scaling_tests.rs\"]\nmod tests;\n",
        ),
        (
            "crates/core/src/platform.rs",
            "#[cfg(unix)]\n#[path = \"platform/unix_impl.rs\"]\nmod imp;\n#[cfg(windows)]\n#[path = \"platform/windows_impl.rs\"]\nmod imp;\n\npub fn open() -> i32 {\n    3\n}\n\npub fn via_imp() -> i32 {\n    imp::open_impl()\n}\n\nmod inline {\n    #[path = \"other.rs\"]\n    mod inner;\n}\n\n#[cfg_attr(docsrs, path = \"docs_impl.rs\")]\nmod documented;\n\npub fn via_documented() -> i32 {\n    documented::run()\n}\n",
        ),
        ("crates/core/src/platform/unix_impl.rs", "pub fn open_impl() -> i32 {\n    1\n}\n"),
        ("crates/core/src/platform/windows_impl.rs", "pub fn open_impl() -> i32 {\n    2\n}\n"),
        (
            "crates/core/src/platform/inline/other.rs",
            "pub fn from_inner() -> i32 {\n    super::super::open()\n}\n",
        ),
        ("crates/core/src/platform/other.rs", "pub fn decoy() -> i32 {\n    4\n}\n"),
        ("crates/core/src/documented.rs", "pub fn run() -> i32 {\n    5\n}\n"),
        ("crates/core/src/platform/docs_impl.rs", "pub fn run() -> i32 {\n    6\n}\n"),
        ("crates/py/Cargo.toml", "[package]\nname = \"py-crate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        (
            "crates/py/src/lib.rs",
            "pub fn predict_rating_multi(x: i32) -> i32 {\n    x + 99\n}\n\npub fn private_helper() -> i32 {\n    98\n}\n\npub fn root_helper() -> i32 {\n    97\n}\n\npub fn open() -> i32 {\n    96\n}\n\npub fn make() -> i32 {\n    95\n}\n",
        ),
        (
            "tests/unit/scaling_tests.rs",
            "use super::*;\n\n#[test]\nfn uses_private() {\n    let _ = private_helper();\n}\n\n#[test]\nfn uses_crate() {\n    let _ = crate::root_helper();\n}\n",
        ),
    ]
}

#[test]
fn super_and_crate_follow_the_declaring_module() {
    let f = index(&workspace());
    let mut c = Checks::default();
    c.is(
        "use super::* reaches the declaring module's private item",
        calls_of(&f.cg, "tests/unit/scaling_tests.rs", "uses_private"),
        &["function private_helper crates/core/src/scaling.rs:5"],
    );
    c.is(
        "crate:: starts at the declaring file's crate",
        calls_of(&f.cg, "tests/unit/scaling_tests.rs", "uses_crate"),
        &["function root_helper crates/core/src/lib.rs:5"],
    );
    drop(f);

    let small = index(&[
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/core\", \"crates/py\"]\nresolver = \"2\"\n"),
        ("crates/core/Cargo.toml", "[package]\nname = \"core-crate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        ("crates/py/Cargo.toml", "[package]\nname = \"py-crate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        ("crates/core/src/lib.rs", "pub mod scaling;\n"),
        (
            "crates/core/src/scaling.rs",
            "pub fn predict_rating_multi(x: i32) -> i32 { x + 1 }\n\n#[cfg(test)]\n#[path = \"../../../tests/unit/scaling_tests.rs\"]\nmod tests;\n",
        ),
        ("crates/py/src/lib.rs", "pub fn predict_rating_multi(x: i32) -> i32 { x + 99 }\n"),
        (
            "tests/unit/scaling_tests.rs",
            "use super::*;\n\n#[test]\nfn pr_elom_rowmean() {\n    let _ = predict_rating_multi(1);\n}\n",
        ),
    ]);
    c.is(
        "use super::* prefers the declaring module over a same-named decoy",
        calls_of(&small.cg, "tests/unit/scaling_tests.rs", "pr_elom_rowmean"),
        &["function predict_rating_multi crates/core/src/scaling.rs:1"],
    );
    c.finish();
}

#[test]
fn inline_module_path_starts_in_its_own_directory() {
    let f = index(&workspace());
    let mut c = Checks::default();
    c.is(
        "super::super:: walks to the file that declared the inline module",
        calls_of(
            &f.cg,
            "crates/core/src/platform/inline/other.rs",
            "from_inner",
        ),
        &["function open crates/core/src/platform.rs:8"],
    );
    c.finish();
}

#[test]
fn shared_helper_dotdot_folder_and_example_root() {
    let f = index(&workspace());
    let mut c = Checks::default();
    c.is(
        "the first test's path reaches the shared helper",
        calls_of(&f.cg, "crates/core/tests/a.rs", "uses_support"),
        &["function make crates/core/tests/support/helpers.rs:1"],
    );
    c.is(
        "the second test's path reaches the same helper",
        calls_of(&f.cg, "crates/core/tests/b.rs", "uses_support_too"),
        &["function make crates/core/tests/support/helpers.rs:1"],
    );
    c.is(
        "a folder whose name only starts with two dots stays inside the project",
        calls_of(&f.cg, "crates/core/src/gen_user.rs", "via_gen"),
        &["function make_gen crates/core/src/..generated/gen.rs:1"],
    );
    c.is(
        "an example the manifest places is its own crate root",
        calls_of(&f.cg, "crates/core/examples/nested/demo.rs", "main"),
        &["function helper crates/core/examples/nested/util.rs:1"],
    );
    c.finish();
}

#[test]
fn a_name_two_cfg_alternatives_hold_and_cfg_attr_stay_unread() {
    let f = index(&workspace());
    let mut c = Checks::default();
    c.is(
        "cfg alternatives name a file each",
        calls_of(&f.cg, "crates/core/src/platform.rs", "via_imp"),
        &[],
    );
    c.is(
        "a name both cfg alternatives hold is the build's choice",
        calls_of(&f.cg, "crates/core/src/process/mod.rs", "via_either"),
        &[],
    );
    c.is(
        "a cfg_attr sets the path",
        calls_of(&f.cg, "crates/core/src/platform.rs", "via_documented"),
        &[],
    );
    c.finish();
}

#[test]
fn reach_what_only_one_of_its_cfg_alternatives_holds() {
    let f = index(&workspace());
    let mut c = Checks::default();
    c.is(
        "the name only the windows alternative declares",
        calls_of(&f.cg, "crates/core/src/process/mod.rs", "via_windows"),
        &["function only_on_windows crates/core/src/process/windows.rs:1"],
    );
    c.finish();
}

#[test]
fn a_use_through_a_glob_of_cfg_alternatives_reaches_the_item() {
    let f = index(&workspace());
    let file = f
        .cg
        .get_nodes_by_file("crates/core/src/park.rs")
        .unwrap()
        .into_iter()
        .find(|n| n.kind == NodeKind::File)
        .expect("park.rs file node");
    let mut imported: Vec<String> = f
        .cg
        .get_outgoing_edges(&file.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Imports)
        .map(|e| {
            let target = f.cg.get_node(&e.target).unwrap().unwrap();
            format!("{} {}:{}", target.name, target.file_path, target.start_line)
        })
        .collect();
    imported.sort();
    assert_eq!(
        imported,
        [
            "GlobalOrphanQueue crates/core/src/process/unix/mod.rs:3",
            "OrphanQueue crates/core/src/process/unix/orphan.rs:1",
        ]
    );
}
