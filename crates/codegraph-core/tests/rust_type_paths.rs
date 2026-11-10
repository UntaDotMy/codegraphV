//! Calls through a Rust type a path or a `use` names, and workspace members that
//! stay inside the project (upstream 7021de5c, #3025; the cross-crate cases are
//! PR #2631 by BGamboa13).
//!
//! A call such as `ItemPool::for_def` under `use sim::items::ItemPool`, or
//! `sim::items::ItemPool::for_def` written in full, is that type's own function.
//! A namesake in another crate is not. A `use` of a module-level `const` or
//! `static` links it. A type rustfmt wraps under its module path
//! (`scheduler::` then `Handle`) follows that path. A member a manifest writes
//! outside the project is not read.
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
        Some(n) => format!("{} {} {}:{}", n.kind.as_str(), n.qualified_name, n.file_path, n.start_line),
        None => format!("? {id}"),
    }
}

fn calls_of(cg: &CodeGraph, file: &str, name: &str) -> Vec<String> {
    let nodes = cg.get_nodes_by_file(file).unwrap();
    let Some(caller) = nodes.iter().find(|n| n.name == name && n.kind == NodeKind::Function) else {
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

/// `file:qualifiedName` of every edge of `kind` into the symbol.
fn incoming(cg: &CodeGraph, file: &str, qualified: &str, kind: EdgeKind) -> Vec<String> {
    let nodes = cg.get_nodes_by_file(file).unwrap();
    let Some(target) = nodes.iter().find(|n| n.qualified_name == qualified) else {
        return vec![format!("<no {qualified} in {file}>")];
    };
    let mut out = Vec::new();
    for edge in cg.get_incoming_edges(&target.id).unwrap() {
        if edge.kind != kind {
            continue;
        }
        match cg.get_node(&edge.source).unwrap() {
            Some(src) => out.push(format!("{}:{}", src.file_path, src.qualified_name)),
            None => out.push(format!("? {}", edge.source)),
        }
    }
    out.sort();
    out
}

/// Every `file:qualifiedName` with any edge into a node named `Handle` or `new`.
fn touches(cg: &CodeGraph, file: &str) -> Vec<String> {
    let mut out = Vec::new();
    for node in cg.get_nodes_by_file(file).unwrap() {
        if node.name != "Handle" && node.name != "new" {
            continue;
        }
        for edge in cg.get_incoming_edges(&node.id).unwrap() {
            if let Some(src) = cg.get_node(&edge.source).unwrap() {
                out.push(format!("{}:{}", src.file_path, src.qualified_name));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

#[derive(Default)]
struct Checks(Vec<String>);

impl Checks {
    fn is(&mut self, case: &str, actual: Vec<String>, expected: &[&str]) {
        if actual != expected {
            self.0.push(format!("{case}\n    expected {expected:?}\n    actual   {actual:?}"));
        }
    }

    fn has(&mut self, case: &str, actual: &[String], needle: &str) {
        if !actual.iter().any(|s| s == needle) {
            self.0.push(format!("{case}\n    missing  {needle}\n    actual   {actual:?}"));
        }
    }

    fn lacks(&mut self, case: &str, actual: &[String], needle: &str) {
        if actual.iter().any(|s| s == needle) {
            self.0.push(format!("{case}\n    unwanted {needle}\n    actual   {actual:?}"));
        }
    }

    fn finish(self) {
        assert!(self.0.is_empty(), "{} case(s) failed:\n  {}", self.0.len(), self.0.join("\n  "));
    }
}

#[test]
fn calls_through_a_type_a_path_or_a_use_names() {
    let f = index(&[
        ("Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nserde_json = \"1\"\n"),
        ("src/lib.rs", "pub mod runtime;\npub mod driver;\npub mod models;\npub mod codec;\npub mod alias;\npub mod user;\n"),
        ("src/runtime/mod.rs", "mod handle;\npub use handle::Handle;\n"),
        ("src/runtime/handle.rs", "pub struct Handle;\n\nimpl Handle {\n    pub fn current() -> Handle {\n        Handle\n    }\n}\n"),
        ("src/driver.rs", "pub struct Handle;\n\nimpl Handle {\n    pub fn current() -> Handle {\n        Handle\n    }\n}\n"),
        ("src/models.rs", "pub enum Mode {\n    Fast(u8),\n    Slow,\n}\n\npub struct Fast(pub u8);\n"),
        ("src/codec.rs", "pub trait Codec {\n    fn encode(&self) -> u8;\n}\n\npub struct Bytes;\n\nimpl Codec for Bytes {\n    fn encode(&self) -> u8 {\n        1\n    }\n}\n"),
        ("src/alias.rs", "pub type Shortcut = crate::driver::Handle;\n"),
        (
            "src/user.rs",
            "use std::collections::HashMap;\nuse serde_json::Value;\nuse crate::runtime::Handle;\nuse crate::codec::{Bytes, Codec};\nuse crate::alias::Shortcut;\n\npub fn current() {\n    let _ = Handle::current();\n}\n\npub fn variant() {\n    let _ = crate::models::Mode::Fast(1);\n}\n\npub fn through_trait() {\n    let _ = Codec::encode(&Bytes);\n}\n\npub fn outside() {\n    let _ = HashMap::<u8, u8>::new();\n    let _ = Value::from(1);\n}\n\npub fn through_alias() {\n    let _ = Shortcut::current();\n}\n",
        ),
    ]);
    let mut c = Checks::default();
    c.is(
        "Handle::current under use crate::runtime::Handle",
        calls_of(&f.cg, "src/user.rs", "current"),
        &["method Handle::current src/runtime/handle.rs:4"],
    );
    c.is(
        "crate::models::Mode::Fast",
        calls_of(&f.cg, "src/user.rs", "variant"),
        &["enum_member Mode::Fast src/models.rs:2"],
    );
    c.is(
        "Codec::encode under use of the trait",
        calls_of(&f.cg, "src/user.rs", "through_trait"),
        &["method Codec::encode src/codec.rs:2"],
    );
    c.is("HashMap::new and Value::from link nothing", calls_of(&f.cg, "src/user.rs", "outside"), &[]);
    c.finish();
}

const WORKSPACE: &str = "[workspace]\nresolver = \"2\"\nmembers = [\"crates/sim\", \"crates/server\", \"crates/client\"]\n\n[workspace.dependencies]\nsim = { path = \"crates/sim\" }\n";
const SIM_TOML: &str = "[package]\nname = \"sim\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
const SIM_LIB: &str = "pub mod items;\nmod rng;\npub use rng::Rng;\npub const RANK_COUNT: usize = 6;\npub fn median_u32(v: &[u32]) -> u32 { v[0] }\n";
const SIM_ITEMS: &str = "pub struct ItemPool { pub n: u32 }\nimpl ItemPool {\n    pub fn for_def(def: u32) -> ItemPool { ItemPool { n: def } }\n}\npub enum Drop { Gold(u32), Nothing }\npub fn roll_item(p: &ItemPool) -> u32 { p.n }\n";
const SIM_RNG: &str = "pub struct Rng { s: u64 }\nimpl Rng {\n    pub fn new(s: u64) -> Rng { Rng { s } }\n}\n";
const CLIENT_TOML: &str = "[package]\nname = \"client\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
const CLIENT_LIB: &str = "pub mod items;\n";
const CLIENT_ITEMS: &str = "pub struct ItemPool;\nimpl ItemPool {\n    pub fn for_def(def: u32) -> ItemPool { ItemPool }\n}\npub fn roll_item() -> u32 { 0 }\n";
const SERVER_SIM: &str = "[package]\nname = \"server\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nsim.workspace = true\n";

fn workspace(server: &[(&str, &str)], server_toml: &str) -> Vec<(String, String)> {
    let mut files = vec![
        ("Cargo.toml".to_string(), WORKSPACE.to_string()),
        ("crates/sim/Cargo.toml".to_string(), SIM_TOML.to_string()),
        ("crates/sim/src/lib.rs".to_string(), SIM_LIB.to_string()),
        ("crates/sim/src/items.rs".to_string(), SIM_ITEMS.to_string()),
        ("crates/sim/src/rng.rs".to_string(), SIM_RNG.to_string()),
        ("crates/client/Cargo.toml".to_string(), CLIENT_TOML.to_string()),
        ("crates/client/src/lib.rs".to_string(), CLIENT_LIB.to_string()),
        ("crates/client/src/items.rs".to_string(), CLIENT_ITEMS.to_string()),
        ("crates/server/Cargo.toml".to_string(), server_toml.to_string()),
    ];
    for (file, text) in server {
        files.push((format!("crates/server/src/{file}"), (*text).to_string()));
    }
    files
}

fn index_owned(files: &[(String, String)]) -> Fixture {
    let borrowed: Vec<(&str, &str)> = files.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    index(&borrowed)
}

#[test]
fn cross_crate_paths_link_the_named_item() {
    let mut c = Checks::default();

    let qualified = index_owned(&workspace(
        &[
            ("lib.rs", "pub mod rift;\npub mod econ;\n"),
            ("rift.rs", "pub fn open() -> u32 {\n    let pool = sim::items::ItemPool::for_def(3);\n    sim::items::roll_item(&pool)\n}\n"),
            ("econ/mod.rs", "pub mod offline;\n"),
            ("econ/offline.rs", "pub fn tick() {\n    let _ = sim::items::ItemPool::for_def(\n        4,\n    );\n}\n"),
        ],
        SERVER_SIM,
    ));
    c.is(
        "ItemPool::for_def across crates",
        incoming(&qualified.cg, "crates/sim/src/items.rs", "ItemPool::for_def", EdgeKind::Calls),
        &["crates/server/src/econ/offline.rs:tick", "crates/server/src/rift.rs:open"],
    );
    c.is(
        "roll_item across crates",
        incoming(&qualified.cg, "crates/sim/src/items.rs", "roll_item", EdgeKind::Calls),
        &["crates/server/src/rift.rs:open"],
    );
    c.is(
        "the client decoy for_def",
        incoming(&qualified.cg, "crates/client/src/items.rs", "ItemPool::for_def", EdgeKind::Calls),
        &[],
    );

    let root_items = index_owned(&workspace(
        &[(
            "lib.rs",
            "pub mod rift;\n",
        ), (
            "rift.rs",
            "use sim::RANK_COUNT;\npub fn open(v: &[u32]) -> usize {\n    let _r = sim::Rng::new(7);\n    let _m = sim::median_u32(v);\n    let _d = sim::items::Drop::Gold(3);\n    RANK_COUNT\n}\n",
        )],
        SERVER_SIM,
    ));
    c.is(
        "Rng::new re-export",
        incoming(&root_items.cg, "crates/sim/src/rng.rs", "Rng::new", EdgeKind::Calls),
        &["crates/server/src/rift.rs:open"],
    );
    c.is(
        "median_u32 at the crate root",
        incoming(&root_items.cg, "crates/sim/src/lib.rs", "median_u32", EdgeKind::Calls),
        &["crates/server/src/rift.rs:open"],
    );
    c.is(
        "Drop::Gold",
        incoming(&root_items.cg, "crates/sim/src/items.rs", "Drop::Gold", EdgeKind::Calls),
        &["crates/server/src/rift.rs:open"],
    );
    let rank = incoming(&root_items.cg, "crates/sim/src/lib.rs", "RANK_COUNT", EdgeKind::Imports);
    c.has("use sim::RANK_COUNT", &rank, "crates/server/src/rift.rs:crates/server/src/rift.rs");

    let via_use = index_owned(&workspace(
        &[(
            "lib.rs",
            "pub mod rift;\n",
        ), (
            "rift.rs",
            "use sim::items::{roll_item, ItemPool};\nuse sim::Rng;\npub fn open() -> u32 {\n    let _r = Rng::new(1);\n    roll_item(&ItemPool::for_def(2))\n}\n",
        )],
        SERVER_SIM,
    ));
    c.is(
        "ItemPool::for_def through a use",
        incoming(&via_use.cg, "crates/sim/src/items.rs", "ItemPool::for_def", EdgeKind::Calls),
        &["crates/server/src/rift.rs:open"],
    );
    c.is(
        "roll_item through a use",
        incoming(&via_use.cg, "crates/sim/src/items.rs", "roll_item", EdgeKind::Calls),
        &["crates/server/src/rift.rs:open"],
    );
    c.is(
        "Rng::new through a use",
        incoming(&via_use.cg, "crates/sim/src/rng.rs", "Rng::new", EdgeKind::Calls),
        &["crates/server/src/rift.rs:open"],
    );
    c.lacks("use of ItemPool", &incoming(&via_use.cg, "crates/sim/src/items.rs", "ItemPool", EdgeKind::Imports), "<no ItemPool in crates/sim/src/items.rs>");
    if incoming(&via_use.cg, "crates/sim/src/items.rs", "ItemPool", EdgeKind::Imports).is_empty() {
        c.0.push("use of ItemPool linked nothing".to_string());
    }
    if incoming(&via_use.cg, "crates/sim/src/rng.rs", "Rng", EdgeKind::Imports).is_empty() {
        c.0.push("use of Rng linked nothing".to_string());
    }

    let mut renamed_files = workspace(
        &[(
            "lib.rs",
            "pub fn open() -> u32 {\n    let p = core_sim::items::ItemPool::for_def(1);\n    vendored::helper(p.n)\n}\n",
        )],
        "[package]\nname = \"server\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ncore_sim = { path = \"../sim\", package = \"sim\" }\nvendored = { path = \"../../libs/vendored\" }\n",
    );
    renamed_files.push(("libs/vendored/Cargo.toml".to_string(), "[package]\nname = \"vendored\"\nversion = \"0.1.0\"\nedition = \"2021\"\n".to_string()));
    renamed_files.push(("libs/vendored/src/lib.rs".to_string(), "pub fn helper(n: u32) -> u32 { n }\n".to_string()));
    let renamed = index_owned(&renamed_files);
    c.is(
        "renamed path dependency core_sim",
        incoming(&renamed.cg, "crates/sim/src/items.rs", "ItemPool::for_def", EdgeKind::Calls),
        &["crates/server/src/lib.rs:open"],
    );
    c.is(
        "vendored helper inside the project",
        incoming(&renamed.cg, "libs/vendored/src/lib.rs", "helper", EdgeKind::Calls),
        &["crates/server/src/lib.rs:open"],
    );

    let inside = index_owned(&workspace(
        &[
            ("lib.rs", "pub mod ledger;\npub mod econ;\n"),
            ("ledger.rs", "pub const KEY_RANK_COUNT: usize = 6;\npub static LIMIT: u32 = 9;\npub fn persist() {}\n"),
            ("econ/mod.rs", "pub mod sinks;\npub fn shared() {}\n"),
            (
                "econ/sinks.rs",
                "use crate::ledger::{KEY_RANK_COUNT, LIMIT};\npub fn drain() -> usize {\n    crate::ledger::persist();\n    super::shared();\n    KEY_RANK_COUNT + LIMIT as usize\n}\n",
            ),
        ],
        SERVER_SIM,
    ));
    c.is(
        "crate::ledger::persist",
        incoming(&inside.cg, "crates/server/src/ledger.rs", "persist", EdgeKind::Calls),
        &["crates/server/src/econ/sinks.rs:drain"],
    );
    c.is(
        "super::shared",
        incoming(&inside.cg, "crates/server/src/econ/mod.rs", "shared", EdgeKind::Calls),
        &["crates/server/src/econ/sinks.rs:drain"],
    );
    if incoming(&inside.cg, "crates/server/src/ledger.rs", "KEY_RANK_COUNT", EdgeKind::Imports).is_empty() {
        c.0.push("use of KEY_RANK_COUNT linked nothing".to_string());
    }
    if incoming(&inside.cg, "crates/server/src/ledger.rs", "LIMIT", EdgeKind::Imports).is_empty() {
        c.0.push("use of LIMIT linked nothing".to_string());
    }

    let qualified_type = index_owned(&workspace(
        &[
            ("lib.rs", "pub mod scheduler;\npub mod driver;\npub mod multi;\n"),
            ("scheduler.rs", "pub enum Handle { A, B }\n"),
            ("driver.rs", "pub struct Handle { pub n: u32 }\n"),
            ("multi/mod.rs", "mod handle;\npub(crate) use handle::Handle;\n"),
            ("multi/handle.rs", "pub struct Handle { pub n: u32 }\n"),
            (
                "user.rs",
                "use crate::multi::Handle;\nuse crate::{driver, scheduler};\npub fn sched(h: &scheduler::Handle) -> u32 { match h { scheduler::Handle::A => 1, _ => 2 } }\npub fn drv(h: &driver::Handle) -> u32 { h.n }\npub fn own(h: &Handle) -> u32 { h.n }\n",
            ),
        ],
        SERVER_SIM,
    ));
    let sched_refs = incoming(&qualified_type.cg, "crates/server/src/scheduler.rs", "Handle", EdgeKind::References);
    let drv_refs = incoming(&qualified_type.cg, "crates/server/src/driver.rs", "Handle", EdgeKind::References);
    let own_refs = incoming(&qualified_type.cg, "crates/server/src/multi/handle.rs", "Handle", EdgeKind::References);
    c.has("scheduler::Handle", &sched_refs, "crates/server/src/user.rs:sched");
    c.is("driver::Handle", drv_refs.clone(), &["crates/server/src/user.rs:drv"]);
    c.lacks("multi Handle is not sched", &own_refs, "crates/server/src/user.rs:sched");
    c.lacks("multi Handle is not drv", &own_refs, "crates/server/src/user.rs:drv");
    c.has("bare Handle is the use", &own_refs, "crates/server/src/user.rs:own");

    let wrapped = index_owned(&workspace(
        &[
            ("lib.rs", "pub mod scheduler;\npub mod multi;\n"),
            ("scheduler.rs", "pub enum Handle { A, B }\n"),
            ("multi/mod.rs", "mod handle;\npub(crate) use handle::Handle;\n"),
            ("multi/handle.rs", "pub struct Handle { pub n: u32 }\n"),
            (
                "user.rs",
                "use crate::multi::Handle;\nuse crate::scheduler;\npub fn sched(\n    h: &scheduler::\n        Handle,\n) -> u32 { 1 }\n",
            ),
        ],
        SERVER_SIM,
    ));
    let wrapped_sched = incoming(&wrapped.cg, "crates/server/src/scheduler.rs", "Handle", EdgeKind::References);
    let wrapped_multi = incoming(&wrapped.cg, "crates/server/src/multi/handle.rs", "Handle", EdgeKind::References);
    c.has("wrapped scheduler:: Handle", &wrapped_sched, "crates/server/src/user.rs:sched");
    c.lacks("wrapped Handle is not the use", &wrapped_multi, "crates/server/src/user.rs:sched");

    let commented = index_owned(&workspace(
        &[
            ("lib.rs", "pub mod scheduler;\npub mod multi;\n"),
            ("scheduler.rs", "pub struct Handle { pub n: u32 }\nimpl Handle { pub fn new() -> Handle { Handle { n: 0 } } }\n"),
            ("multi/mod.rs", "mod handle;\npub(crate) use handle::Handle;\n"),
            ("multi/handle.rs", "pub struct Handle { pub n: u32 }\nimpl Handle { pub fn new() -> Handle { Handle { n: 1 } } }\n"),
            (
                "user.rs",
                "use crate::multi::Handle;\npub fn make() -> Handle {\n    // crate::scheduler::\n    Handle { n: 3 }\n}\npub fn make_block() -> Handle {\n    /* crate::scheduler:: */\n    Handle { n: 4 }\n}\n",
            ),
        ],
        SERVER_SIM,
    ));
    let comment_sched = touches(&commented.cg, "crates/server/src/scheduler.rs");
    let comment_multi = touches(&commented.cg, "crates/server/src/multi/handle.rs");
    for name in ["make", "make_block"] {
        let site = format!("crates/server/src/user.rs:{name}");
        c.lacks("a comment is not a qualifier", &comment_sched, &site);
        c.has("the use still names Handle", &comment_multi, &site);
    }

    let pkg = |name: &str, dep: &str| format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{dep}");
    let split = index(&[
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/server\", \"crates/client\"]\n"),
        ("crates/server/Cargo.toml", &pkg("server", "\n[dependencies]\nutil = { path = \"../../libs/util_a\", package = \"util_a\" }\n")),
        ("crates/server/src/lib.rs", "pub fn s() -> u32 { util::unique_a(1) }\n"),
        ("crates/client/Cargo.toml", &pkg("client", "\n[dependencies]\nutil = { path = \"../../libs/util_b\", package = \"util_b\" }\n")),
        ("crates/client/src/lib.rs", "pub fn c() -> u32 { util::unique_b(2) }\n"),
        ("libs/util_a/Cargo.toml", &pkg("util_a", "")),
        ("libs/util_a/src/lib.rs", "pub fn unique_a(n: u32) -> u32 { n }\npub fn unique_b(n: u32) -> u32 { n + 1 }\n"),
        ("libs/util_b/Cargo.toml", &pkg("util_b", "")),
        ("libs/util_b/src/lib.rs", "pub fn unique_b(n: u32) -> u32 { n }\n"),
    ]);
    c.lacks(
        "client's util is util_b",
        &incoming(&split.cg, "libs/util_a/src/lib.rs", "unique_b", EdgeKind::Calls),
        "crates/client/src/lib.rs:c",
    );

    c.finish();
}
