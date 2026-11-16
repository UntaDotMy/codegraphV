//! A Rust method call through a standard-library collection held in a `RefCell`
//! field (`store.batch.borrow_mut().entry(..)` with `batch: RefCell<HashMap<..>>`)
//! is not a call of a project method that happens to share its name.
//!
//! The same holds behind a Mutex guard (`self.map.lock().unwrap_or_else(..).entry(..)`).
//!
//! The controls must keep linking, and by their type: a project type reached through a
//! RefCell or a Mutex borrow, a project-typed field, an extension trait implemented on a
//! foreign type, and a generic or `dyn` receiver of a project trait. A same-file namesake
//! on another type makes the bare name ambiguous, so a control passes only when its
//! receiver's type is read.
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

/// The targets of the calls the function `name` in `file` makes, described with the
/// strategy that linked each (`resolvedBy`).
fn typed_calls_of(cg: &CodeGraph, file: &str, name: &str) -> Vec<String> {
    let nodes = cg.get_nodes_by_file(file).unwrap();
    let Some(caller) = nodes.iter().find(|n| n.name == name && n.kind == NodeKind::Function) else {
        return vec![format!("<no function {name} in {file}>")];
    };
    let mut out = Vec::new();
    for edge in cg.get_outgoing_edges(&caller.id).unwrap() {
        if edge.kind == EdgeKind::Calls {
            let by = edge.metadata.as_ref().and_then(|m| m.get("resolvedBy")).and_then(|v| v.as_str()).unwrap_or("?");
            out.push(format!("{} by={by}", describe(cg, &edge.target)));
        }
    }
    out.sort();
    out
}

/// The targets of the calls the function `name` in `file` makes, described.
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

const INSTALLER: &str = r#"
pub struct Installer;

impl Installer {
    pub fn entry(&self, id: &str, loc: u8) -> u8 {
        let _ = id;
        loc
    }
}
"#;

const RENDER: &str = r#"
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Mutex;

pub struct Store {
    pub batch: RefCell<HashMap<String, u8>>,
    pub guarded: Mutex<HashMap<String, u8>>,
}

/// The standard-library entry API behind a RefCell borrow is not Installer::entry.
pub fn batch(store: &Store) {
    store.batch.borrow_mut().entry("a".to_string());
}

/// The same behind a Mutex guard.
pub fn guarded(store: &Store) {
    store.guarded.lock().unwrap_or_else(|e| e.into_inner()).entry("a".to_string());
}
"#;

const CONTROLS: &str = r#"
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Mutex;

pub struct Relay;

impl Relay {
    pub fn answered(&self) -> u8 {
        0
    }
}

pub struct Link {
    pub relay: Relay,
}

pub fn tick(link: &Link) -> u8 {
    link.relay.answered()
}

pub struct Bounded;

impl Bounded {
    pub fn get(&self) -> u8 {
        0
    }

    pub fn ping(&self) -> u8 {
        0
    }
}

/// Same-file namesake on another type: the bare names `get` and `ping` are ambiguous here.
pub struct Spare;

impl Spare {
    pub fn get(&self) -> u8 {
        1
    }

    pub fn ping(&self) -> u8 {
        1
    }
}

pub struct Ctx {
    pub cache: RefCell<Bounded>,
    pub guarded: Mutex<Bounded>,
}

pub fn node(c: &Ctx) -> u8 {
    c.cache.borrow().get()
}

pub fn locked(c: &Ctx) -> u8 {
    c.guarded.lock().unwrap().get()
}

pub fn pinged(c: &Ctx) -> u8 {
    c.cache.borrow().ping()
}

pub trait Bump {
    fn bump(&self) -> u8;
}

impl Bump for HashMap<u8, u8> {
    fn bump(&self) -> u8 {
        1
    }
}

pub fn run(m: &HashMap<u8, u8>) -> u8 {
    m.bump()
}

pub fn generic<T: Bump>(x: &T) -> u8 {
    x.bump()
}

pub fn dynamic(x: &dyn Bump) -> u8 {
    x.bump()
}
"#;

fn fixture() -> Fixture {
    index(&[
        ("src/lib.rs", "pub mod installer;\npub mod render;\npub mod ctl;\n"),
        ("src/installer.rs", INSTALLER),
        ("src/render.rs", RENDER),
        ("src/ctl.rs", CONTROLS),
    ])
}

#[test]
fn std_collection_behind_refcell_is_not_a_project_method() {
    let fx = fixture();
    let calls = calls_of(&fx.cg, "src/render.rs", "batch");
    assert!(
        !calls.iter().any(|c| c.contains("Installer::entry")),
        "a HashMap behind a RefCell borrow linked to Installer::entry: {calls:?}"
    );
}

#[test]
fn project_typed_field_still_links() {
    let fx = fixture();
    let calls = calls_of(&fx.cg, "src/ctl.rs", "tick");
    assert!(calls.iter().any(|c| c.contains("Relay::answered")), "{calls:?}");
}

#[test]
fn std_collection_behind_mutex_lock_is_not_a_project_method() {
    let fx = fixture();
    let calls = calls_of(&fx.cg, "src/render.rs", "guarded");
    assert!(
        !calls.iter().any(|c| c.contains("Installer::entry")),
        "a HashMap behind a Mutex lock linked to Installer::entry: {calls:?}"
    );
}

#[test]
fn project_type_behind_refcell_borrow_still_links() {
    let fx = fixture();
    let calls = typed_calls_of(&fx.cg, "src/ctl.rs", "node");
    assert!(calls.iter().any(|c| c.contains("Bounded::get") && c.ends_with("by=instance-method")), "{calls:?}");
}

#[test]
fn project_type_behind_mutex_lock_still_links() {
    let fx = fixture();
    let calls = typed_calls_of(&fx.cg, "src/ctl.rs", "locked");
    assert!(calls.iter().any(|c| c.contains("Bounded::get") && c.ends_with("by=instance-method")), "{calls:?}");
}

#[test]
fn project_type_method_behind_refcell_borrow_still_links() {
    let fx = fixture();
    let calls = typed_calls_of(&fx.cg, "src/ctl.rs", "pinged");
    assert!(calls.iter().any(|c| c.contains("Bounded::ping") && c.ends_with("by=instance-method")), "{calls:?}");
}

#[test]
fn extension_impl_on_foreign_receiver_still_links() {
    let fx = fixture();
    let calls = calls_of(&fx.cg, "src/ctl.rs", "run");
    assert!(calls.iter().any(|c| c.contains("bump")), "{calls:?}");
}

#[test]
fn generic_and_dyn_receivers_of_a_project_trait_still_link() {
    let fx = fixture();
    let generic = calls_of(&fx.cg, "src/ctl.rs", "generic");
    assert!(generic.iter().any(|c| c.contains("bump")), "{generic:?}");
    let dynamic = calls_of(&fx.cg, "src/ctl.rs", "dynamic");
    assert!(dynamic.iter().any(|c| c.contains("bump")), "{dynamic:?}");
}
