//! A Rust method call through a standard-library collection held in a `RefCell`
//! field (`store.batch.borrow_mut().entry(..)` with `batch: RefCell<HashMap<..>>`)
//! is not a call of a project method that happens to share its name.
//!
//! The same holds behind a Mutex or RwLock guard (`self.map.lock().unwrap_or_else(..).entry(..)`),
//! and with a project `Result` alias in the crate (`src/errors.rs`): the alias must not switch
//! the standard lock accessor off.
//!
//! A std `Mutex`, `RefCell` or `Vec` written with its std path (`std::sync::Mutex`, `std::cell::RefCell`,
//! `std::vec::Vec`) is read as std when the crate declares a type of the same name, including a `Vec`
//! iterated with `for row in store.rows.iter()`, and a std `HashMap` read through `values()`
//! whose items are a `HashMap` or a `BTreeMap`.
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

const ERRORS: &str = r#"
/// The crate's own result alias, as the installer declares it.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
"#;

const RENDER: &str = r#"
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

pub struct Store {
    pub batch: RefCell<HashMap<String, u8>>,
    pub guarded: Mutex<HashMap<String, u8>>,
    pub keyed: RefCell<HashMap<(&'static str, usize), String>>,
    pub shared: Arc<Mutex<HashMap<String, u8>>>,
    pub rwmap: RwLock<HashMap<String, u8>>,
}

/// The standard-library entry API behind a RefCell borrow is not Installer::entry.
pub fn batch(store: &Store) {
    store.batch.borrow_mut().entry("a".to_string());
}

/// The same behind a Mutex guard.
pub fn guarded(store: &Store) {
    store.guarded.lock().unwrap_or_else(|e| e.into_inner()).entry("a".to_string());
}

/// The same behind a RefCell with a tuple key, as `batch_sql` is declared.
pub fn keyed(store: &Store) {
    store.keyed.borrow_mut().entry(("a", 1)).or_default();
}

/// The same behind an Arc and a Mutex.
pub fn shared(store: &Store) {
    store.shared.lock().unwrap().entry("a".to_string());
}

/// The same behind an RwLock write guard.
pub fn rw_entry(store: &Store) {
    store.rwmap.write().unwrap().entry("a".to_string());
}

/// The same behind an RwLock write guard, through `unwrap_or_else`.
pub fn rw_write_guarded(store: &Store) {
    store.rwmap.write().unwrap_or_else(|e| e.into_inner()).entry("a".to_string());
}
"#;

const CONTROLS: &str = r#"
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex, RwLock};

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
    pub rw: RwLock<Bounded>,
}

pub struct Wrapped {
    pub rc: Rc<RefCell<Bounded>>,
    pub arc: Arc<Mutex<Bounded>>,
    pub boxed: Box<RefCell<Bounded>>,
    pub rcmap: Rc<RefCell<HashMap<String, u8>>>,
}

pub fn free_fn() -> u8 {
    0
}

pub fn calls_free() -> u8 {
    free_fn()
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

pub fn rc_pinged(w: &Wrapped) -> u8 {
    w.rc.borrow().ping()
}

pub fn arc_locked(w: &Wrapped) -> u8 {
    w.arc.lock().unwrap().get()
}

/// A local bound from a guard, then called: the receiver is a binding, not a chain.
pub fn local_guard(c: &Ctx) -> u8 {
    let g = c.cache.borrow();
    g.get()
}

pub fn boxed_pinged(w: &Wrapped) -> u8 {
    w.boxed.borrow_mut().ping()
}

pub fn rc_map_get(w: &Wrapped) -> u8 {
    w.rcmap.borrow().get("a")
}

pub fn read_locked(c: &Ctx) -> u8 {
    c.rw.read().unwrap().get()
}

pub fn write_locked(c: &Ctx) -> u8 {
    c.rw.write().unwrap().ping()
}

pub fn try_locked(c: &Ctx) -> u8 {
    c.guarded.try_lock().unwrap().get()
}

pub fn try_read_locked(c: &Ctx) -> u8 {
    c.rw.try_read().unwrap().get()
}

pub fn try_write_locked(c: &Ctx) -> u8 {
    c.rw.try_write().unwrap().ping()
}

pub fn option_unwrap_or(o: Option<Bounded>) -> u8 {
    o.unwrap_or(Bounded).get()
}

pub fn option_unwrap_or_default(o: Option<Bounded>) -> u8 {
    o.unwrap_or_default().ping()
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
        ("src/lib.rs", "pub mod errors;\npub mod installer;\npub mod render;\npub mod ctl;\n"),
        ("src/errors.rs", ERRORS),
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
fn std_collection_behind_refcell_with_a_tuple_key_is_not_a_project_method() {
    let fx = fixture();
    let calls = calls_of(&fx.cg, "src/render.rs", "keyed");
    assert!(!calls.iter().any(|c| c.contains("Installer::entry")), "{calls:?}");
}

#[test]
fn std_collection_behind_arc_and_mutex_is_not_a_project_method() {
    let fx = fixture();
    let calls = calls_of(&fx.cg, "src/render.rs", "shared");
    assert!(!calls.iter().any(|c| c.contains("Installer::entry")), "{calls:?}");
}

#[test]
fn project_type_behind_rc_and_refcell_still_links() {
    let fx = fixture();
    let calls = typed_calls_of(&fx.cg, "src/ctl.rs", "rc_pinged");
    assert!(calls.iter().any(|c| c.contains("Bounded::ping") && c.ends_with("by=instance-method")), "{calls:?}");
}

#[test]
fn project_type_behind_arc_and_mutex_still_links() {
    let fx = fixture();
    let calls = typed_calls_of(&fx.cg, "src/ctl.rs", "arc_locked");
    assert!(calls.iter().any(|c| c.contains("Bounded::get") && c.ends_with("by=instance-method")), "{calls:?}");
}

#[test]
fn a_plain_call_still_links_by_name() {
    let fx = fixture();
    let calls = calls_of(&fx.cg, "src/ctl.rs", "calls_free");
    assert!(calls.iter().any(|c| c.contains("free_fn")), "{calls:?}");
}

#[test]
fn project_type_through_a_local_bound_from_a_guard_still_links() {
    let fx = fixture();
    let calls = typed_calls_of(&fx.cg, "src/ctl.rs", "local_guard");
    assert!(calls.iter().any(|c| c.contains("Bounded::get") && c.ends_with("by=instance-method")), "{calls:?}");
}

#[test]
fn std_map_behind_rwlock_write_is_not_a_project_method() {
    let fx = fixture();
    let calls = calls_of(&fx.cg, "src/render.rs", "rw_entry");
    assert!(!calls.iter().any(|c| c.contains("Installer::entry")), "{calls:?}");
}

#[test]
fn std_map_behind_rc_refcell_borrow_is_not_a_project_method() {
    let fx = fixture();
    let calls = calls_of(&fx.cg, "src/ctl.rs", "rc_map_get");
    assert!(!calls.iter().any(|c| c.contains("Spare::get") || c.contains("Bounded::get")), "{calls:?}");
}

#[test]
fn project_type_behind_box_and_refcell_borrow_mut_still_links() {
    let fx = fixture();
    let calls = typed_calls_of(&fx.cg, "src/ctl.rs", "boxed_pinged");
    assert!(calls.iter().any(|c| c.contains("Bounded::ping") && c.ends_with("by=instance-method")), "{calls:?}");
}

#[test]
fn project_type_behind_rwlock_read_still_links() {
    let fx = fixture();
    let calls = typed_calls_of(&fx.cg, "src/ctl.rs", "read_locked");
    assert!(calls.iter().any(|c| c.contains("Bounded::get") && c.ends_with("by=instance-method")), "{calls:?}");
}

#[test]
fn project_type_behind_rwlock_write_still_links() {
    let fx = fixture();
    let calls = typed_calls_of(&fx.cg, "src/ctl.rs", "write_locked");
    assert!(calls.iter().any(|c| c.contains("Bounded::ping") && c.ends_with("by=instance-method")), "{calls:?}");
}

#[test]
fn project_type_behind_try_lock_still_links() {
    let fx = fixture();
    let calls = typed_calls_of(&fx.cg, "src/ctl.rs", "try_locked");
    assert!(calls.iter().any(|c| c.contains("Bounded::get") && c.ends_with("by=instance-method")), "{calls:?}");
}

#[test]
fn project_type_behind_try_read_still_links() {
    let fx = fixture();
    let calls = typed_calls_of(&fx.cg, "src/ctl.rs", "try_read_locked");
    assert!(calls.iter().any(|c| c.contains("Bounded::get") && c.ends_with("by=instance-method")), "{calls:?}");
}

#[test]
fn project_type_behind_try_write_still_links() {
    let fx = fixture();
    let calls = typed_calls_of(&fx.cg, "src/ctl.rs", "try_write_locked");
    assert!(calls.iter().any(|c| c.contains("Bounded::ping") && c.ends_with("by=instance-method")), "{calls:?}");
}

#[test]
fn project_type_held_by_option_unwrap_or_still_links() {
    let fx = fixture();
    let calls = typed_calls_of(&fx.cg, "src/ctl.rs", "option_unwrap_or");
    assert!(calls.iter().any(|c| c.contains("Bounded::get") && c.ends_with("by=instance-method")), "{calls:?}");
}

#[test]
fn project_type_held_by_option_unwrap_or_default_still_links() {
    let fx = fixture();
    let calls = typed_calls_of(&fx.cg, "src/ctl.rs", "option_unwrap_or_default");
    assert!(calls.iter().any(|c| c.contains("Bounded::ping") && c.ends_with("by=instance-method")), "{calls:?}");
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

#[test]
fn std_map_behind_rwlock_write_unwrap_or_else_is_not_a_project_method() {
    let fx = fixture();
    let calls = calls_of(&fx.cg, "src/render.rs", "rw_write_guarded");
    assert!(!calls.iter().any(|c| c.contains("Installer::entry")), "{calls:?}");
}

const STD_NAMESAKE_INSTALLER: &str = r#"
pub struct Installer;

impl Installer {
    pub fn entry(&self, id: &str, loc: u8) -> u8 {
        let _ = id;
        loc
    }
}
"#;

const STD_NAMESAKE_STORE: &str = r#"
use std::collections::HashMap;

pub struct Store {
    pub guarded: std::sync::Mutex<HashMap<String, u8>>,
    pub cell: std::cell::RefCell<HashMap<String, u8>>,
    pub rows: std::vec::Vec<HashMap<String, u8>>,
}

pub fn std_locked(store: &Store) {
    store.guarded.lock().unwrap_or_else(|e| e.into_inner()).entry("a".to_string());
}

pub fn std_borrowed(store: &Store) {
    store.cell.borrow_mut().entry("a".to_string());
}

pub fn std_rows(store: &Store) {
    for row in store.rows.iter() {
        row.entry("a".to_string());
    }
}
"#;

fn std_namesake_fixture() -> Fixture {
    index(&[
        ("src/lib.rs", "pub mod installer;\npub mod names;\npub mod store;\n"),
        ("src/installer.rs", STD_NAMESAKE_INSTALLER),
        ("src/names.rs", "pub struct Mutex;\npub struct RefCell;\npub struct Vec;\n"),
        ("src/store.rs", STD_NAMESAKE_STORE),
    ])
}

#[test]
fn std_mutex_lock_with_a_project_mutex_namesake_is_not_a_project_method() {
    let fx = std_namesake_fixture();
    let calls = calls_of(&fx.cg, "src/store.rs", "std_locked");
    assert!(!calls.iter().any(|c| c.contains("Installer::entry")), "{calls:?}");
}

#[test]
fn std_refcell_borrow_with_a_project_refcell_namesake_is_not_a_project_method() {
    let fx = std_namesake_fixture();
    let calls = calls_of(&fx.cg, "src/store.rs", "std_borrowed");
    assert!(!calls.iter().any(|c| c.contains("Installer::entry")), "{calls:?}");
}

// Red on 485dc466: rust_item_of checked the head name only. Green after the std-rooted fix.
#[test]
fn std_vec_iteration_with_a_project_vec_namesake_is_not_a_project_method() {
    let fx = std_namesake_fixture();
    let calls = calls_of(&fx.cg, "src/store.rs", "std_rows");
    assert!(!calls.iter().any(|c| c.contains("Installer::entry")), "{calls:?}");
}

const STD_MAP_NAMESAKE_STORE: &str = r#"
pub struct Store {
    pub map: std::collections::HashMap<String, std::collections::HashMap<String, u8>>,
}

pub fn std_values(store: &Store) {
    for row in store.map.values() {
        row.entry("a".to_string());
    }
}
"#;

fn std_map_namesake_fixture() -> Fixture {
    index(&[
        ("src/lib.rs", "pub mod installer;\npub mod names;\npub mod store;\n"),
        ("src/installer.rs", STD_NAMESAKE_INSTALLER),
        ("src/names.rs", "pub struct HashMap;\n"),
        ("src/store.rs", STD_MAP_NAMESAKE_STORE),
    ])
}

#[test]
fn std_hash_map_values_with_a_project_hash_map_namesake_is_not_a_project_method() {
    let fx = std_map_namesake_fixture();
    let calls = calls_of(&fx.cg, "src/store.rs", "std_values");
    assert!(!calls.iter().any(|c| c.contains("Installer::entry")), "{calls:?}");
}

const STD_BTREE_ITEM_STORE: &str = r#"
pub struct Store {
    pub map: std::collections::HashMap<String, std::collections::BTreeMap<String, u8>>,
}

pub fn std_btree_values(store: &Store) {
    for row in store.map.values() {
        row.entry("a".to_string());
    }
}
"#;

fn std_btree_item_fixture() -> Fixture {
    index(&[
        ("src/lib.rs", "pub mod installer;\npub mod names;\npub mod store;\n"),
        ("src/installer.rs", STD_NAMESAKE_INSTALLER),
        ("src/names.rs", "pub struct HashMap;\n"),
        ("src/store.rs", STD_BTREE_ITEM_STORE),
    ])
}

#[test]
fn std_hash_map_values_of_btree_maps_with_a_project_hash_map_namesake_is_not_a_project_method() {
    let fx = std_btree_item_fixture();
    let calls = calls_of(&fx.cg, "src/store.rs", "std_btree_values");
    assert!(!calls.iter().any(|c| c.contains("Installer::entry")), "{calls:?}");
}
