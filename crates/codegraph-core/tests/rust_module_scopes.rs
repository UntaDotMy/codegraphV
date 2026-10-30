//! Rust `use` bindings follow Rust's scopes, and module paths walk the crate's modules (upstream
//! 3f8e8b0f and 4955eac9, #2955, #3014; the `use` cases are PR #2308's by danusha2345).
//!
//! Four fixtures, as upstream's rust-module-path-call, rust-use-call, rust-use-scopes and
//! rust-crate-roots suites: each is indexed once and every case reads call or import edges from
//! it. A case that fails is named in the report with what was expected and what the graph holds.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

struct Fixture {
    cg: CodeGraph,
    root: tempfile::TempDir,
}

fn index(files: &[(&str, &str)]) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    for (relative, content) in files {
        let path = root.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let cg = CodeGraph::init_and_index(root.path().to_str().unwrap(), None).unwrap();
    Fixture { cg, root }
}

/// `kind qualifiedName file:line`: how a node reads in the expectations.
fn describe(cg: &CodeGraph, id: &str) -> String {
    match cg.get_node(id).unwrap() {
        Some(n) => format!("{} {} {}:{}", n.kind.as_str(), n.qualified_name, n.file_path, n.start_line),
        None => format!("? {id}"),
    }
}

impl Fixture {
    /// What the edges of `kind` out of the nodes `ids` reach, in the order the graph lists them.
    fn targets(&self, ids: &[String], kind: EdgeKind) -> Vec<String> {
        let mut out = Vec::new();
        for id in ids {
            for edge in self.cg.get_outgoing_edges(id).unwrap() {
                if edge.kind == kind {
                    out.push(describe(&self.cg, &edge.target));
                }
            }
        }
        out
    }

    fn ids_in(&self, file: &str) -> Vec<String> {
        self.cg.get_nodes_by_file(file).unwrap().into_iter().map(|n| n.id).collect()
    }

    /// What the function `name` in `file` calls (the first node of that name, as upstream's).
    fn calls(&self, file: &str, name: &str) -> Vec<String> {
        let nodes = self.cg.get_nodes_by_file(file).unwrap();
        let Some(caller) = nodes.iter().find(|n| n.name == name) else { return vec![format!("<no node {name} in {file}>")] };
        self.targets(&[caller.id.clone()], EdgeKind::Calls)
    }

    /// What the function `name` in `file` calls, narrowed to the nodes named `take`, by file.
    fn takes(&self, file: &str, name: &str) -> Vec<String> {
        let nodes = self.cg.get_nodes_by_file(file).unwrap();
        let Some(caller) = nodes.iter().find(|n| n.name == name && n.kind == NodeKind::Function) else {
            return vec![format!("<no function {name} in {file}>")];
        };
        let mut files: Vec<String> = self
            .cg
            .get_outgoing_edges(&caller.id)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .filter_map(|e| self.cg.get_node(&e.target).unwrap())
            .filter(|n| n.name == "take")
            .map(|n| n.file_path)
            .collect();
        files.sort();
        files
    }

    fn imports(&self, file: &str) -> Vec<String> {
        self.targets(&self.ids_in(file), EdgeKind::Imports)
    }
}

#[derive(Default)]
struct Checks(Vec<String>);

impl Checks {
    fn is(&mut self, case: &str, actual: Vec<String>, expected: &[&str]) {
        if actual != expected {
            self.0.push(format!("{case}\n    expected {expected:?}\n    actual   {actual:?}"));
        }
    }

    fn finish(self, total_note: &str) {
        assert!(self.0.is_empty(), "{} case(s) failed ({total_note}):\n  {}", self.0.len(), self.0.join("\n  "));
    }
}

#[test]
fn calls_through_a_module_path_reach_the_module_s_own_item() {
    let f = index(&[
        ("Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\n"),
        (
            "src/lib.rs",
            "mod util;\nmod only;\nmod a;\nmod caller;\nmod importer;\nmod handle;\nmod worker;\nmod twice;\nmod inline_reexport;\nmod fs;\nmod uses_std;\nmod b;\nmod x;\nmod types;\nmod type_;\nmod decoy_list;\nmod uses_config;\nmod walks;\nmod plat;\n\npub struct Config;\n\npub fn root_fn() -> usize {\n    0\n}\n\npub fn via_self() -> usize {\n    self::util::take(1)\n}\n",
        ),
        // `super::x` and `crate::x`, with no module between them and the item.
        ("src/b/mod.rs", "pub mod c;\npub fn helper() -> usize {\n    1\n}\n"),
        (
            "src/b/c.rs",
            "pub fn helper() -> usize {\n    2\n}\npub fn via_bare_super() -> usize {\n    super::helper()\n}\npub fn via_bare_crate() -> usize {\n    crate::root_fn()\n}\n",
        ),
        // `super` in an inline `mod tests { }` is the file's own module, not its directory's parent.
        ("src/x/mod.rs", "pub mod tested;\npub mod shared;\npub struct Config;\npub fn root_fn() -> usize {\n    9\n}\n"),
        ("src/x/shared.rs", "pub fn helper() -> usize {\n    3\n}\n"),
        (
            "src/x/tested.rs",
            "mod shared;\n\npub fn tested_helper() -> usize {\n    4\n}\n\n#[cfg(test)]\nmod tests {\n    use super::tested_helper;\n\n    fn via_inline_super() -> usize {\n        super::shared::helper() + tested_helper()\n    }\n}\n",
        ),
        ("src/x/tested/shared.rs", "pub fn helper() -> usize {\n    5\n}\n"),
        // A module's item it passes on: a `pub use`, a `pub use …::*`.
        ("src/types/mod.rs", "mod list;\npub use self::list::make_list;\n"),
        ("src/types/list.rs", "pub fn make_list() -> usize {\n    6\n}\n"),
        ("src/decoy_list.rs", "pub fn make_list() -> usize {\n    7\n}\n"),
        (
            "src/type_/mod.rs",
            "pub mod prelude;\npub use prelude::*;\n\npub struct Type;\n\nimpl Type {\n    pub fn list() -> usize {\n        8\n    }\n}\n",
        ),
        ("src/type_/prelude.rs", "pub fn list() -> usize {\n    9\n}\n"),
        ("src/uses_config.rs", "use crate::{Config};\n"),
        // A re-export written in a macro's braces, as `cfg_if!` takes them, is the module's.
        ("src/plat/mod.rs", "cfg_if::cfg_if! {\n    if #[cfg(unix)] {\n        mod unix;\n        pub use self::unix::open;\n    }\n}\n"),
        ("src/plat/unix.rs", "pub fn open() -> usize {\n    10\n}\n"),
        ("src/decoy_open.rs", "pub fn open() -> usize {\n    11\n}\n"),
        (
            "src/walks.rs",
            "pub fn via_reexport() -> usize {\n    crate::types::make_list()\n}\n\npub fn via_glob() -> usize {\n    crate::type_::list()\n}\n\npub fn via_macro_reexport() -> usize {\n    crate::plat::open()\n}\n\npub fn via_block_std() {\n    use std::fs;\n    let _ = fs::read(\"config.toml\");\n}\n",
        ),
        // A namesake in an inline module, before the module's own item.
        (
            "src/twice.rs",
            "#[cfg(test)]\nmod tests {\n    pub fn pick() -> usize {\n        0\n    }\n}\n\npub fn pick() -> usize {\n    1\n}\n\npub fn via_twice() -> usize {\n    crate::twice::pick()\n}\n",
        ),
        // The item lives in an inline module the file passes on with a glob.
        (
            "src/inline_reexport.rs",
            "#[cfg(unix)]\nmod imp {\n    pub fn lone() {}\n}\npub use imp::*;\n\npub fn via_inline() {\n    crate::inline_reexport::lone();\n}\n",
        ),
        // A project module named like a module of the standard library.
        ("src/fs/mod.rs", "pub fn read(path: &str) -> usize {\n    path.len()\n}\n"),
        ("src/uses_std.rs", "use std::fs;\n\npub fn loads() {\n    let _ = fs::read(\"config.toml\");\n}\n"),
        (
            "src/util.rs",
            "pub struct Buf;\n\nimpl Buf {\n    pub fn take(&self, n: usize) -> usize {\n        n\n    }\n}\n\npub trait Take {\n    fn take(&self) -> usize;\n}\n\nfn outer() -> usize {\n    fn take() -> usize {\n        0\n    }\n    take()\n}\n\npub fn take(n: usize) -> usize {\n    n\n}\n",
        ),
        ("src/only.rs", "pub struct Only;\n\nimpl Only {\n    pub fn grab(&self) -> usize {\n        0\n    }\n}\n"),
        ("src/a/mod.rs", "pub mod b;\n"),
        ("src/a/b.rs", "pub struct Deep;\n\nimpl Deep {\n    pub fn get(&self) -> usize {\n        0\n    }\n}\n\npub fn get() -> usize {\n    1\n}\n"),
        ("src/importer.rs", "use crate::util::take;\n"),
        ("src/handle.rs", "pub struct Handle;\n"),
        (
            "src/worker.rs",
            "pub use crate::handle::Handle;\nmod dump;\n\npub trait Lock {\n    type Handle;\n}\n\npub struct Guard;\n\nimpl Lock for Guard {\n    type Handle = u8;\n}\n",
        ),
        ("src/worker/dump.rs", "use super::Handle;\n"),
        (
            "src/caller.rs",
            "use crate::util;\n\npub fn via_crate() -> usize {\n    crate::util::take(1)\n}\n\npub fn via_super() -> usize {\n    super::util::take(1)\n}\n\npub fn via_module() -> usize {\n    util::take(1)\n}\n\npub fn via_nested() -> usize {\n    crate::a::b::get()\n}\n\npub fn via_method_only() -> usize {\n    crate::only::grab()\n}\n\npub fn via_missing() -> usize {\n    crate::nowhere::take(1)\n}\n",
        ),
    ]);
    let mut c = Checks::default();
    let take = "function take src/util.rs:20";
    c.is("via crate::", f.calls("src/caller.rs", "via_crate"), &[take]);
    c.is("via super::", f.calls("src/caller.rs", "via_super"), &[take]);
    c.is("via a module bound by use", f.calls("src/caller.rs", "via_module"), &[take]);
    c.is("via self::", f.calls("src/lib.rs", "via_self"), &[take]);
    c.is("use of the function", f.imports("src/importer.rs"), &[take]);
    c.is(
        "a use never binds an associated type of the module's impl or trait",
        f.imports("src/worker/dump.rs").into_iter().filter(|t| t.contains("src/worker.rs")).collect(),
        &[],
    );
    c.is("a nested module path", f.calls("src/caller.rs", "via_nested"), &["function get src/a/b.rs:9"]);
    c.is("a method-only module links nothing", f.calls("src/caller.rs", "via_method_only"), &[]);
    c.is("a module not in the index links nothing", f.calls("src/caller.rs", "via_missing"), &[]);
    c.is("the module's own item before an inline namesake", f.calls("src/twice.rs", "via_twice"), &["function pick src/twice.rs:8"]);
    c.is(
        "an inline module's item the module passes on, from its own file",
        f.calls("src/inline_reexport.rs", "via_inline"),
        &["function lone src/inline_reexport.rs:3"],
    );
    c.is("std's fs::read under use std::fs", f.calls("src/uses_std.rs", "loads"), &[]);
    c.is("std's fs::read under a block's use std::fs", f.calls("src/walks.rs", "via_block_std"), &[]);
    c.is("super::helper with no module between", f.calls("src/b/c.rs", "via_bare_super"), &["function helper src/b/mod.rs:2"]);
    c.is("crate::root_fn with no module between", f.calls("src/b/c.rs", "via_bare_crate"), &["function root_fn src/lib.rs:23"]);
    let mut inline_super = f.calls("src/x/tested.rs", "via_inline_super");
    inline_super.sort();
    c.is(
        "super in an inline module is the module of its file",
        inline_super,
        &["function helper src/x/tested/shared.rs:1", "function tested_helper src/x/tested.rs:3"],
    );
    c.is("the use in an inline module", f.imports("src/x/tested.rs"), &["function tested_helper src/x/tested.rs:3"]);
    c.is("an item passed on by pub use", f.calls("src/walks.rs", "via_reexport"), &["function make_list src/types/list.rs:1"]);
    c.is("an item passed on by a glob", f.calls("src/walks.rs", "via_glob"), &["function list src/type_/prelude.rs:1"]);
    c.is("an item passed on in a macro's braces", f.calls("src/walks.rs", "via_macro_reexport"), &["function open src/plat/unix.rs:1"]);
    c.is("use crate::{Item}", f.imports("src/uses_config.rs"), &["struct Config src/lib.rs:21"]);
    c.finish("rust-module-path-call");
}

#[test]
fn bare_calls_follow_the_use_that_binds_the_name_where_it_is() {
    let f = index(&[
        ("Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nexternal = \"1\"\n"),
        (
            "src/lib.rs",
            "pub mod a;\npub mod util;\npub mod caller;\npub mod child;\npub mod method_only;\npub fn root_take() {}\npub enum Root { RootGood(i32) }\n",
        ),
        ("src/a.rs", "pub fn take() {}\npub fn consume() {}\npub fn selected() {}\npub enum Noise { take }\n"),
        (
            "src/util.rs",
            "pub struct Buf;\nimpl Buf { pub fn take(&self) {} }\npub fn outer() {\n    fn take() {}\n    take();\n}\npub fn take() {}\n",
        ),
        ("src/method_only.rs", "pub struct Buf;\nimpl Buf { pub fn take(&self) {} }\n"),
        (
            "src/caller.rs",
            "use crate::util::take;\npub fn imported() { take(); }\npub fn shadowed() {\n    fn take() {}\n    take();\n}\npub fn parameter(take: fn()) { take(); }\npub fn destructured((take, _): (fn(), i32)) { take(); }\npub fn type_label(callback: fn(take: i32)) { take(); }\npub fn generic<T: Fn(i32)>(callback: T) { take(); }\npub fn after_binding() {\n    take();\n    let take = || {};\n    take();\n}\n",
        ),
        ("src/alias.rs", "use crate::util::take as consume;\npub fn aliased() { consume(); }\n"),
        ("src/group.rs", "use crate::{util::{take as selected}, a::{take as other}};\npub fn grouped() { selected(); }\npub fn second() { other(); }\n"),
        ("src/child/mod.rs", "use super::util::take;\nuse self::util::take as local_take;\npub fn parent() { take(); }\npub fn child() { local_take(); }\n"),
        ("src/child/util.rs", "pub fn take() {}\n"),
        (
            "src/scoped.rs",
            "pub fn first() {\n    use crate::util::take;\n    take();\n}\npub fn second() {\n    use crate::a::take;\n    take();\n}\n",
        ),
        (
            "src/outer_shadow.rs",
            "fn take() {}\npub fn imported_in_block() {\n    use crate::util::take;\n    take();\n}\npub fn inner_import(take: fn()) {\n    {\n        use crate::util::take;\n        take();\n    }\n}\npub fn before_binding() {\n    use crate::util::take;\n    take();\n    let take = || {};\n}\n",
        ),
        ("src/tests_mod.rs", "pub fn tested_helper() {}\n#[cfg(test)]\nmod tests {\n    use super::tested_helper;\n    fn uses() { tested_helper(); }\n}\n"),
        ("src/external.rs", "use external::take as consume;\npub fn external_call() { consume(); }\n"),
        ("src/models.rs", "pub enum E { Good(i32) }\n"),
        ("src/model_decoy.rs", "pub enum E { Real(i32) }\nmod hidden { pub enum E { Good(i32) } }\n"),
        ("src/model_decoy_call.rs", "use crate::model_decoy::E::Good;\npub fn model_decoy() { Good(1); }\n"),
        ("src/variant.rs", "use crate::models::E::Good;\npub fn variant() { Good(1); }\n"),
        ("src/variant_alias.rs", "use crate::models::E::Good as Build;\npub fn alias_variant() { Build(1); }\n"),
        ("src/root_call.rs", "use crate::root_take as consume;\npub fn root_call() { consume(); }\n"),
        ("src/root_variant.rs", "use crate::Root::RootGood;\npub fn root_variant() { RootGood(1); }\n"),
        ("src/same_file.rs", "fn same_take() {}\nuse self::same_take as consume;\npub fn same_file() { consume(); }\n"),
        (
            "src/inline_reexport.rs",
            "#[cfg(unix)]\nmod imp { pub fn lone() {} }\npub use imp::*;\nuse self::lone as aliased;\npub fn via_alias() { aliased(); }\npub fn via_path() { crate::inline_reexport::lone(); }\n",
        ),
        ("src/duplicate.rs", "#[cfg(unix)]\npub fn take() {}\n#[cfg(windows)]\npub fn take() {}\n"),
        ("src/duplicate_call.rs", "use crate::duplicate::take;\npub fn duplicate_call() { take(); }\n"),
        ("tests/bad_inline.rs", "mod hidden { mod support { pub mod panic; } }\nuse support::panic::test_panic;\npub fn bad_inline() { test_panic(); }\n"),
        ("tests/support/panic.rs", "pub fn test_panic() {}\n"),
        ("src/sync.rs", "use crate::a::take;\npub fn synced() { take(); }\n"),
        ("src/unicode.rs", "use crate::util::take;\npub fn unicode() { let crab = \"🦀Я\"; take(); }\n"),
        ("src/unicode_scoped.rs", "pub fn unicode_scoped() { let crab = \"🦀Я\"; { use crate::util::take as consume; consume(); } }\n"),
        ("crates/one/Cargo.toml", "[package]\nname = \"one\"\nversion = \"0.1.0\"\n"),
        ("crates/one/src/lib.rs", "pub mod util;\n"),
        ("crates/one/src/util.rs", "pub fn take() {}\npub fn only_in_one() {}\n"),
        ("crates/two/Cargo.toml", "[package]\nname = \"two\"\nversion = \"0.1.0\"\n"),
        ("crates/two/src/lib.rs", "pub mod util;\npub mod caller;\n"),
        ("crates/two/src/util.rs", "pub fn take() {}\n"),
        (
            "crates/two/src/caller.rs",
            "use crate::util::take;\nuse one::util::only_in_one;\npub fn workspace_call() { take(); }\npub fn cross_crate() { only_in_one(); }\n",
        ),
    ]);
    let mut c = Checks::default();
    let take = "function take src/util.rs:7";
    c.is("the imported free function among same-name methods and functions", f.calls("src/caller.rs", "imported"), &[take]);
    c.is("an alias with no declaration of its local name", f.calls("src/alias.rs", "aliased"), &[take]);
    c.is("a grouped alias", f.calls("src/group.rs", "grouped"), &[take]);
    c.is("a second grouped alias", f.calls("src/group.rs", "second"), &["function take src/a.rs:1"]);
    c.is("super import anchors on the current module", f.calls("src/child/mod.rs", "parent"), &[take]);
    c.is("self import anchors on the current module", f.calls("src/child/mod.rs", "child"), &["function take src/child/util.rs:1"]);
    c.is("a block's use: first", f.calls("src/scoped.rs", "first"), &[take]);
    c.is("a block's use: second", f.calls("src/scoped.rs", "second"), &["function take src/a.rs:1"]);
    c.is("a local fn shadows the import", f.calls("src/caller.rs", "shadowed"), &["function shadowed::take src/caller.rs:4"]);
    c.is("a parameter shadows the import", f.calls("src/caller.rs", "parameter"), &[]);
    c.is("a destructured parameter shadows the import", f.calls("src/caller.rs", "destructured"), &[]);
    c.is("a type label is no binding", f.calls("src/caller.rs", "type_label"), &[take]);
    c.is("a generic bound is no binding", f.calls("src/caller.rs", "generic"), &[take]);
    c.is("a later let does not reach back", f.calls("src/caller.rs", "after_binding"), &[take]);
    c.is("a block's use shadows the file's own item", f.calls("src/outer_shadow.rs", "imported_in_block"), &[take]);
    c.is("a nested block's use before the parameter", f.calls("src/outer_shadow.rs", "inner_import"), &[take]);
    c.is("a use before a later let", f.calls("src/outer_shadow.rs", "before_binding"), &[take]);
    c.is("an alias of an external crate", f.calls("src/external.rs", "external_call"), &[]);
    c.is("a variant use never takes another module's enum", f.calls("src/model_decoy_call.rs", "model_decoy"), &[]);
    c.is("another crate of the workspace, by its package name", f.calls("crates/two/src/caller.rs", "cross_crate"), &["function only_in_one crates/one/src/util.rs:2"]);
    c.is("use super inside an inline mod tests", f.calls("src/tests_mod.rs", "uses"), &["function tested_helper src/tests_mod.rs:1"]);
    c.is("a relative path inside an inline module", f.calls("tests/bad_inline.rs", "bad_inline"), &["function test_panic tests/support/panic.rs:1"]);
    c.is("an inline module's function through its re-export, by alias", f.calls("src/inline_reexport.rs", "via_alias"), &["function lone src/inline_reexport.rs:2"]);
    c.is("an inline module's function through its re-export, by path", f.calls("src/inline_reexport.rs", "via_path"), &["function lone src/inline_reexport.rs:2"]);
    c.is("the first of two #[cfg] variants", f.calls("src/duplicate_call.rs", "duplicate_call"), &["function take src/duplicate.rs:2"]);
    c.is("a variant attached to its enum", f.calls("src/variant.rs", "variant"), &["enum_member E::Good src/models.rs:1"]);
    c.is("a renamed variant", f.calls("src/variant_alias.rs", "alias_variant"), &["enum_member E::Good src/models.rs:1"]);
    c.is("a variant of the crate root's enum", f.calls("src/root_variant.rs", "root_variant"), &["enum_member Root::RootGood src/lib.rs:7"]);
    c.is("a crate-root function by alias", f.calls("src/root_call.rs", "root_call"), &["function root_take src/lib.rs:6"]);
    c.is("a same-file alias", f.calls("src/same_file.rs", "same_file"), &["function same_take src/same_file.rs:1"]);
    c.is("UTF-16 columns after Cyrillic and emoji", f.calls("src/unicode.rs", "unicode"), &[take]);
    c.is("UTF-16 columns in a block's use after Cyrillic and emoji", f.calls("src/unicode_scoped.rs", "unicode_scoped"), &[take]);
    c.is("crate:: is the caller's package, not another's", f.calls("crates/two/src/caller.rs", "workspace_call"), &["function take crates/two/src/util.rs:1"]);

    // A source edit and a sync refresh the binding.
    c.is("before the edit", f.calls("src/sync.rs", "synced"), &["function take src/a.rs:1"]);
    let Fixture { mut cg, root } = f;
    fs::write(root.path().join("src/sync.rs"), "use crate::util::take;\npub fn synced() { take(); }\n").unwrap();
    cg.sync(&Default::default(), None).unwrap();
    let f = Fixture { cg, root };
    c.is("after the edit and a sync", f.calls("src/sync.rs", "synced"), &[take]);
    c.finish("rust-use-call");
}

#[test]
fn a_use_binds_its_name_in_its_own_scope_and_a_local_hides_it() {
    let take = "pub fn take() {}\npub fn noop() {}\n";
    let f = index(&[
        ("Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        (
            "src/lib.rs",
            "pub mod a;\npub mod b;\npub mod c;\npub mod other;\npub mod util;\npub mod real;\npub mod wrong;\npub mod blocks;\npub mod params;\npub mod lets;\npub mod literals;\npub mod macros;\npub mod cfgs;\n",
        ),
        ("src/a.rs", take),
        ("src/b.rs", take),
        ("src/c.rs", take),
        ("src/other.rs", take),
        ("src/util.rs", take),
        ("src/real.rs", take),
        ("src/wrong.rs", take),
        (
            "src/blocks.rs",
            "use crate::a::take;\npub fn outer() { take(); }\npub fn inner() { { use crate::b::take; take(); } }\npub fn sibling() { { use crate::b::take; } { take(); } }\npub fn after() { { use crate::b::take; } take(); }\npub fn two() { use crate::c::take; take(); }\npub fn three() { take(); }\n",
        ),
        (
            "src/params.rs",
            "use crate::util::take;\npub fn root(take: fn()) { take(); }\npub fn body(take: fn()) { use crate::other::take; take(); }\npub fn block(take: fn()) { { use crate::other::take; take(); } }\npub fn closure() { let _ = |take: fn()| take(); }\npub fn nested(take: fn()) { fn inner() { take(); } }\n",
        ),
        (
            "src/lets.rs",
            "use crate::util::take;\nuse crate::util::noop;\npub fn outer_let() { let take = || {}; take(); }\npub fn initializer() { let take: fn() = { take(); noop }; }\npub fn inner_use() { let take = || {}; { use crate::other::take; take(); } }\npub fn inner_let() { { let take = || {}; take(); } }\npub fn if_let(v: Option<fn()>) { if let Some(take) = v { take(); } }\npub fn after_if_let(v: Option<fn()>) { if let Some(take) = v { } take(); }\npub fn holds_fn() { let take = take; take(); }\npub fn holds_generic() { let take = take::<u8>; take(); }\npub fn holds_path() { let take = crate::other::take; take(); }\n",
        ),
        (
            "src/literals.rs",
            "use crate::real::take;\n// use crate::wrong::take;\n/* use crate::wrong::take; */\nconst NORMAL: &str = \"use crate::wrong::take; { }\";\nconst RAW: &str = r##\"use crate::wrong::take; mod fake {\"##;\npub fn call() { take(); }\n",
        ),
        (
            "src/macros.rs",
            "macro_rules! items { ($($i:item)*) => { $($i)* } }\nitems! {\n    use crate::b::take;\n}\npub fn call() { take(); }\npub fn local() { use crate::c::take; take(); }\n",
        ),
        // The braces inside a macro's are its input too: `cfg_if!`'s `use`s are the module's.
        (
            "src/cfgs.rs",
            "cfg_if::cfg_if! {\n    if #[cfg(unix)] {\n        use crate::b::take;\n    } else {\n        use crate::b::take;\n    }\n}\npub fn call() { take(); }\npub fn local() { use crate::c::take; take(); }\n",
        ),
    ]);
    let mut c = Checks::default();
    c.is("blocks: outer", f.takes("src/blocks.rs", "outer"), &["src/a.rs"]);
    c.is("blocks: inner", f.takes("src/blocks.rs", "inner"), &["src/b.rs"]);
    c.is("blocks: sibling", f.takes("src/blocks.rs", "sibling"), &["src/a.rs"]);
    c.is("blocks: after", f.takes("src/blocks.rs", "after"), &["src/a.rs"]);
    c.is("blocks: two", f.takes("src/blocks.rs", "two"), &["src/c.rs"]);
    c.is("blocks: three", f.takes("src/blocks.rs", "three"), &["src/a.rs"]);
    c.is("params: a parameter", f.takes("src/params.rs", "root"), &[]);
    c.is("params: a use in the body is nearer", f.takes("src/params.rs", "body"), &["src/other.rs"]);
    c.is("params: a use in a block is nearer", f.takes("src/params.rs", "block"), &["src/other.rs"]);
    c.is("params: a closure's parameter", f.takes("src/params.rs", "closure"), &[]);
    c.is("params: a nested fn sees no parameter of the function around it", f.takes("src/params.rs", "inner"), &["src/util.rs"]);
    c.is("lets: a let from its statement on", f.takes("src/lets.rs", "outer_let"), &[]);
    c.is("lets: the initializer is before the binding", f.takes("src/lets.rs", "initializer"), &["src/util.rs"]);
    c.is("lets: an inner use is nearer", f.takes("src/lets.rs", "inner_use"), &["src/other.rs"]);
    c.is("lets: a let in its own block", f.takes("src/lets.rs", "inner_let"), &[]);
    c.is("lets: if let", f.takes("src/lets.rs", "if_let"), &[]);
    c.is("lets: after an if let", f.takes("src/lets.rs", "after_if_let"), &["src/util.rs"]);
    c.is("lets: a let that holds the function", f.takes("src/lets.rs", "holds_fn"), &["src/util.rs"]);
    c.is("lets: a let that holds a generic function", f.takes("src/lets.rs", "holds_generic"), &["src/util.rs"]);
    c.is("lets: a let that holds a function by its path", f.takes("src/lets.rs", "holds_path"), &["src/other.rs"]);
    c.is("literals: no use in a comment or a string", f.takes("src/literals.rs", "call"), &["src/real.rs"]);
    c.is("macros: a use in a macro's braces is the module's", f.takes("src/macros.rs", "call"), &["src/b.rs"]);
    c.is("macros: a local use", f.takes("src/macros.rs", "local"), &["src/c.rs"]);
    c.is("cfg_if: the module's use", f.takes("src/cfgs.rs", "call"), &["src/b.rs"]);
    c.is("cfg_if: a local use", f.takes("src/cfgs.rs", "local"), &["src/c.rs"]);
    c.finish("rust-use-scopes");
}

#[test]
fn crate_starts_from_the_root_the_manifest_places() {
    let bin = index(&[
        (
            "Cargo.toml",
            "[package]\nname = \"ripgrep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[[bin]]\nbench = false\npath = \"crates/core/main.rs\"\nname = \"rg\"\n",
        ),
        ("crates/core/main.rs", "mod flags;\nmod messages;\n\nfn main() {\n    let _ = messages::errored();\n}\n"),
        ("crates/core/messages.rs", "pub fn errored() -> bool {\n    false\n}\n"),
        ("crates/core/flags/mod.rs", "pub mod parse;\npub mod doc;\n\npub enum Category {\n    Input,\n}\n"),
        ("crates/core/flags/parse.rs", "pub fn lookup() -> usize {\n    1\n}\n"),
        ("crates/core/flags/doc.rs", "use crate::flags::Category;\n\npub fn generate() -> usize {\n    crate::flags::parse::lookup()\n}\n"),
        // Namesakes the paths must not reach.
        ("crates/core/flags/doc/parse.rs", "pub fn lookup() -> usize {\n    2\n}\n"),
        ("crates/core/other.rs", "pub fn errored() -> bool {\n    true\n}\npub enum Category {\n    Output,\n}\n"),
    ]);
    let member = index(&[
        ("Cargo.toml", "[workspace]\nmembers = [\"core-lib\"]\n"),
        (
            "core-lib/Cargo.toml",
            "[package]\ncategories = [\"asynchronous\", \"network-programming\"]\ndescription = \"Core types\"\nedition = \"2021\"\nname = \"core-lib\"\nversion = \"0.1.0\"\n",
        ),
        ("core-lib/src/lib.rs", "pub mod body;\npub mod other;\n"),
        ("core-lib/src/body.rs", "pub struct Body;\n\n#[cfg(test)]\nmod tests {\n    use crate::body::Body;\n}\n"),
        ("core-lib/src/other.rs", "pub struct Body;\n"),
    ]);
    let mut c = Checks::default();
    c.is("a binary placed with a path: a call through crate::", bin.calls("crates/core/flags/doc.rs", "generate"), &["function lookup crates/core/flags/parse.rs:1"]);
    c.is("a binary placed with a path: a use of crate::", bin.imports("crates/core/flags/doc.rs"), &["enum Category crates/core/flags/mod.rs:4"]);
    c.is("a binary placed with a path: its own module", bin.calls("crates/core/main.rs", "main"), &["function errored crates/core/messages.rs:1"]);
    c.is("a name written after an array", member.imports("core-lib/src/body.rs"), &["struct Body core-lib/src/body.rs:1"]);
    c.finish("rust-crate-roots");
}
