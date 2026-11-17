//! A std `Option`, `Result`, `Box`, `Rc`, `Arc`, `Pin`, `Mutex`, `RwLock` or `RefCell` used in a file
//! that never sees a project type of the same name keeps its std reading. A project type of that name
//! elsewhere in the crate used to switch the std route off project-wide, and the call then fell to a
//! name match among the candidates, which could pick any of them.
//!
//! Most red tests run the same site in two projects: `N` declares the namesake, `C` does not. `C`
//! must show `by=instance-method`, which proves the fixture reaches the route, so a red result on `N`
//! is the namesake. The guards pin the cases that must stay blocked: a project name the file really
//! sees (declared, imported, globbed, read through a field of another file, or behind a `use super::*`)
//! and an unresolvable or dependency glob.
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

/// The calls the function or method `name` in `file` makes, each with the strategy that linked it.
fn typed_calls_of(cg: &CodeGraph, file: &str, name: &str) -> Vec<String> {
    let nodes = cg.get_nodes_by_file(file).unwrap();
    let Some(caller) = nodes
        .iter()
        .find(|n| n.name == name && matches!(n.kind, NodeKind::Function | NodeKind::Method))
    else {
        panic!("no function or method {name} in {file}");
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

/// Whether `calls` holds a call to `Relay::from_daemon` linked by the typed route.
fn typed_relay_call(calls: &[String]) -> bool {
    calls.iter().any(|c| c.contains("Relay::from_daemon") && c.ends_with("by=instance-method"))
}

const LINK: &str = "pub struct Relay;\n\nimpl Relay {\n    pub fn from_daemon(&self, line: &str) -> bool {\n        !line.is_empty()\n    }\n}\n";
const OPTION_ENUM: &str = "pub enum Option<T> {\n    Yes(T),\n    No,\n}\n";

// --- Option -----------------------------------------------------------------------------------

const OPTION_SITE: &str = r#"use crate::link::Relay;

pub fn option_try(o: Option<Relay>, line: &str) -> Option<bool> {
    Some(o.as_ref()?.from_daemon(line))
}

pub fn option_else(o: Option<Relay>, line: &str) -> bool {
    o.unwrap_or_else(|| Relay).from_daemon(line)
}
"#;

fn option_fixture(namesake: bool) -> Fixture {
    if namesake {
        index(&[
            ("src/lib.rs", "pub mod link;\npub mod shadow;\npub mod site;\n"),
            ("src/link.rs", LINK),
            ("src/shadow.rs", OPTION_ENUM),
            ("src/site.rs", OPTION_SITE),
        ])
    } else {
        index(&[
            ("src/lib.rs", "pub mod link;\npub mod site;\n"),
            ("src/link.rs", LINK),
            ("src/site.rs", OPTION_SITE),
        ])
    }
}

#[test]
fn option_as_ref_try_stays_std_beside_an_enum() {
    let control = option_fixture(false);
    assert!(typed_relay_call(&typed_calls_of(&control.cg, "src/site.rs", "option_try")), "control");
    let namesake = option_fixture(true);
    let calls = typed_calls_of(&namesake.cg, "src/site.rs", "option_try");
    assert!(typed_relay_call(&calls), "{calls:?}");
}

#[test]
fn option_unwrap_or_else_stays_std_beside_an_enum() {
    let control = option_fixture(false);
    assert!(typed_relay_call(&typed_calls_of(&control.cg, "src/site.rs", "option_else")), "control");
    let namesake = option_fixture(true);
    let calls = typed_calls_of(&namesake.cg, "src/site.rs", "option_else");
    assert!(typed_relay_call(&calls), "{calls:?}");
}

/// Guard: a same-file declaration is the project's `Option`, so the site stays blocked.
#[test]
fn a_same_file_option_stays_blocked() {
    let fx = index(&[
        ("src/lib.rs", "pub mod link;\npub mod same_file;\npub mod shadow;\n"),
        ("src/link.rs", LINK),
        ("src/shadow.rs", OPTION_ENUM),
        (
            "src/same_file.rs",
            "use crate::link::Relay;\n\npub enum Option<T> {\n    Yes(T),\n    No,\n}\n\npub fn same_file_else(o: Option<Relay>, line: &str) -> bool {\n    o.unwrap_or_else(|| Relay).from_daemon(line)\n}\n\npub fn same_file_try(o: Option<Relay>, line: &str) -> Option<bool> {\n    Some(o.as_ref()?.from_daemon(line))\n}\n",
        ),
    ]);
    for function in ["same_file_else", "same_file_try"] {
        let calls = typed_calls_of(&fx.cg, "src/same_file.rs", function);
        assert!(!typed_relay_call(&calls), "{function}: {calls:?}");
    }
}

/// Guard: `use crate::shadow::Option;` makes the project's `Option` the one the file means.
#[test]
fn an_imported_option_stays_blocked() {
    let fx = index(&[
        ("src/lib.rs", "pub mod imported;\npub mod link;\npub mod shadow;\n"),
        ("src/link.rs", LINK),
        ("src/shadow.rs", OPTION_ENUM),
        (
            "src/imported.rs",
            "use crate::link::Relay;\nuse crate::shadow::Option;\n\npub fn imported_else(o: Option<Relay>, line: &str) -> bool {\n    o.unwrap_or_else(|| Relay).from_daemon(line)\n}\n\npub fn imported_try(o: Option<Relay>, line: &str) -> Option<bool> {\n    Some(o.as_ref()?.from_daemon(line))\n}\n",
        ),
    ]);
    for function in ["imported_else", "imported_try"] {
        let calls = typed_calls_of(&fx.cg, "src/imported.rs", function);
        assert!(!typed_relay_call(&calls), "{function}: {calls:?}");
    }
}

/// Guard: a module glob that holds the project's `Option` brings it in.
#[test]
fn a_glob_holding_the_project_option_stays_blocked() {
    let fx = index(&[
        ("src/lib.rs", "pub mod globbed;\npub mod link;\npub mod shadow;\n"),
        ("src/link.rs", LINK),
        ("src/shadow.rs", OPTION_ENUM),
        (
            "src/globbed.rs",
            "use crate::link::Relay;\nuse crate::shadow::*;\n\npub fn globbed_else(o: Option<Relay>, line: &str) -> bool {\n    o.unwrap_or_else(|| Relay).from_daemon(line)\n}\n\npub fn globbed_try(o: Option<Relay>, line: &str) -> Option<bool> {\n    Some(o.as_ref()?.from_daemon(line))\n}\n",
        ),
    ]);
    for function in ["globbed_else", "globbed_try"] {
        let calls = typed_calls_of(&fx.cg, "src/globbed.rs", function);
        assert!(!typed_relay_call(&calls), "{function}: {calls:?}");
    }
}

/// Guard: a glob the walk cannot resolve may bring the project's `Option`, so the site stays blocked.
#[test]
fn an_unresolvable_glob_keeps_the_project_option_blocked() {
    let fx = index(&[
        ("src/lib.rs", "pub mod link;\npub mod shadow;\npub mod unknown;\n"),
        ("src/link.rs", LINK),
        ("src/shadow.rs", OPTION_ENUM),
        (
            "src/unknown.rs",
            "use crate::link::Relay;\nuse crate::nowhere::*;\n\npub fn unknown_else(o: Option<Relay>, line: &str) -> bool {\n    o.unwrap_or_else(|| Relay).from_daemon(line)\n}\n\npub fn unknown_try(o: Option<Relay>, line: &str) -> Option<bool> {\n    Some(o.as_ref()?.from_daemon(line))\n}\n",
        ),
    ]);
    for function in ["unknown_else", "unknown_try"] {
        let calls = typed_calls_of(&fx.cg, "src/unknown.rs", function);
        assert!(!typed_relay_call(&calls), "{function}: {calls:?}");
    }
}

/// Guard: a child module's `use super::*` reaches the parent's `use` of the project `Option`.
#[test]
fn a_child_glob_reaches_the_parents_project_option() {
    let fx = index(&[
        ("src/lib.rs", "pub mod link;\npub mod parent;\npub mod shadow;\n"),
        ("src/link.rs", LINK),
        ("src/shadow.rs", OPTION_ENUM),
        (
            "src/parent.rs",
            "use crate::link::Relay;\nuse crate::shadow::Option;\n\npub mod tests {\n    use super::*;\n\n    pub fn child_else(o: Option<Relay>, line: &str) -> bool {\n        o.unwrap_or_else(|| Relay).from_daemon(line)\n    }\n\n    pub fn child_try(o: Option<Relay>, line: &str) -> Option<bool> {\n        Some(o.as_ref()?.from_daemon(line))\n    }\n}\n",
        ),
    ]);
    for function in ["child_else", "child_try"] {
        let calls = typed_calls_of(&fx.cg, "src/parent.rs", function);
        assert!(!typed_relay_call(&calls), "{function}: {calls:?}");
    }
}

/// Guard: a field typed with the project `Option` in another file keeps the site blocked.
#[test]
fn a_field_typed_with_the_project_option_stays_blocked() {
    let fx = index(&[
        ("src/lib.rs", "pub mod holder;\npub mod link;\npub mod shadow;\npub mod user;\n"),
        ("src/link.rs", LINK),
        ("src/shadow.rs", OPTION_ENUM),
        (
            "src/holder.rs",
            "use crate::link::Relay;\nuse crate::shadow::Option;\n\npub struct Holder {\n    pub held: Option<Relay>,\n}\n",
        ),
        (
            "src/user.rs",
            "use crate::holder::Holder;\nuse crate::link::Relay;\n\nimpl Holder {\n    pub fn via(&self, line: &str) -> bool {\n        self.held.unwrap_or_else(|| Relay).from_daemon(line)\n    }\n}\n",
        ),
    ]);
    let calls = typed_calls_of(&fx.cg, "src/user.rs", "via");
    assert!(!typed_relay_call(&calls), "{calls:?}");
}

/// Guard: a glob from a declared dependency may bring the project's `Option`, so the site stays blocked.
#[test]
fn a_dependency_glob_keeps_the_project_option_blocked() {
    let fx = index(&[
        ("Cargo.toml", "[package]\nname = \"q\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nsome_dep = \"1\"\n"),
        ("src/lib.rs", "pub mod link;\npub mod shadow;\npub mod site_out;\n"),
        ("src/link.rs", LINK),
        ("src/shadow.rs", OPTION_ENUM),
        (
            "src/site_out.rs",
            "use crate::link::Relay;\nuse some_dep::*;\n\npub fn out_else(o: Option<Relay>, line: &str) -> bool {\n    o.unwrap_or_else(|| Relay).from_daemon(line)\n}\n\npub fn out_try(o: Option<Relay>, line: &str) -> Option<bool> {\n    Some(o.as_ref()?.from_daemon(line))\n}\n",
        ),
    ]);
    for function in ["out_else", "out_try"] {
        let calls = typed_calls_of(&fx.cg, "src/site_out.rs", function);
        assert!(!typed_relay_call(&calls), "{function}: {calls:?}");
    }
}

// --- Result alias -----------------------------------------------------------------------------

const ALIAS_SITE: &str = r#"use crate::link::Relay;
use crate::names::Result;

pub fn alias_default(r: Result<Relay>, line: &str) -> bool {
    r.unwrap_or_default().from_daemon(line)
}

pub fn alias_try(r: Result<Relay>, line: &str) -> Result<bool> {
    Ok(r.as_ref()?.from_daemon(line))
}
"#;

const ALIAS_SITE_CONTROL: &str = r#"use crate::link::Relay;

pub fn alias_default(r: Result<Relay, String>, line: &str) -> bool {
    r.unwrap_or_default().from_daemon(line)
}

pub fn alias_try(r: Result<Relay, String>, line: &str) -> Result<bool, String> {
    Ok(r.as_ref()?.from_daemon(line))
}
"#;

const STD_SITE: &str = r#"use crate::link::Relay;

pub fn std_default(r: Result<Relay, String>, line: &str) -> bool {
    r.unwrap_or_default().from_daemon(line)
}
"#;

fn alias_fixture(namesake: bool) -> Fixture {
    if namesake {
        index(&[
            ("src/lib.rs", "pub mod link;\npub mod names;\npub mod site_alias;\npub mod site_std;\n"),
            ("src/link.rs", LINK),
            ("src/names.rs", "pub type Result<T> = std::result::Result<T, String>;\n"),
            ("src/site_alias.rs", ALIAS_SITE),
            ("src/site_std.rs", STD_SITE),
        ])
    } else {
        index(&[
            ("src/lib.rs", "pub mod link;\npub mod site_alias;\npub mod site_std;\n"),
            ("src/link.rs", LINK),
            ("src/site_alias.rs", ALIAS_SITE_CONTROL),
            ("src/site_std.rs", STD_SITE),
        ])
    }
}

#[test]
fn result_alias_unwrap_or_default_stays_std() {
    let control = alias_fixture(false);
    assert!(typed_relay_call(&typed_calls_of(&control.cg, "src/site_alias.rs", "alias_default")), "control");
    let namesake = alias_fixture(true);
    let calls = typed_calls_of(&namesake.cg, "src/site_alias.rs", "alias_default");
    assert!(typed_relay_call(&calls), "{calls:?}");
}

#[test]
fn result_alias_as_ref_try_stays_std() {
    let control = alias_fixture(false);
    assert!(typed_relay_call(&typed_calls_of(&control.cg, "src/site_alias.rs", "alias_try")), "control");
    let namesake = alias_fixture(true);
    let calls = typed_calls_of(&namesake.cg, "src/site_alias.rs", "alias_try");
    assert!(typed_relay_call(&calls), "{calls:?}");
}

/// A file that does not import the alias sees the std `Result`, even with the alias declared elsewhere.
#[test]
fn std_result_beside_an_alias_elsewhere_stays_std() {
    let control = alias_fixture(false);
    assert!(typed_relay_call(&typed_calls_of(&control.cg, "src/site_std.rs", "std_default")), "control");
    let namesake = alias_fixture(true);
    let calls = typed_calls_of(&namesake.cg, "src/site_std.rs", "std_default");
    assert!(typed_relay_call(&calls), "{calls:?}");
}

/// The alias's first parameter is the target's first argument, so the alias reads as std.
/// No `C` of its own: its control is `result_alias_unwrap_or_default_stays_std` (same call, std `Result`).
#[test]
fn result_alias_with_a_default_second_parameter_reads_its_std_target() {
    let fx = index(&[
        ("src/lib.rs", "pub mod link;\npub mod names;\npub mod site;\n"),
        ("src/link.rs", LINK),
        ("src/names.rs", "pub type Result<T, E = String> = std::result::Result<T, E>;\n"),
        (
            "src/site.rs",
            "use crate::link::Relay;\nuse crate::names::Result;\n\npub fn pos(r: Result<Relay>, line: &str) -> bool {\n    r.unwrap_or_default().from_daemon(line)\n}\n",
        ),
    ]);
    let calls = typed_calls_of(&fx.cg, "src/site.rs", "pos");
    assert!(typed_relay_call(&calls), "{calls:?}");
}

/// Guard for the alias's first-parameter rule: `Result<E> = Result<Other, E>` makes the payload of
/// `Result<Relay>` an `Other`, so the typed route must not link `Relay::from_daemon`. It passes on HEAD
/// only because the whole site is blocked. HEAD does link `Relay::from_daemon` by `exact-match`; that is
/// the owner-gated name-fallback finding, and the stronger "no `Relay` edge at all" form waits for its fix.
#[test]
fn an_alias_whose_payload_is_another_type_does_not_link_relay_by_the_typed_route() {
    let fx = index(&[
        ("src/lib.rs", "pub mod link;\npub mod names;\npub mod other;\npub mod site;\n"),
        ("src/link.rs", LINK),
        ("src/other.rs", "pub struct Other;\n\nimpl Other {\n    pub fn from_daemon(&self, line: &str) -> bool {\n        line.len() > 1\n    }\n}\n"),
        ("src/names.rs", "pub type Result<E> = std::result::Result<crate::other::Other, E>;\n"),
        (
            "src/site.rs",
            "use crate::link::Relay;\nuse crate::names::Result;\n\npub fn neg(r: Result<Relay>, line: &str) -> bool {\n    r.unwrap_or_default().from_daemon(line)\n}\n",
        ),
    ]);
    let calls = typed_calls_of(&fx.cg, "src/site.rs", "neg");
    assert!(!typed_relay_call(&calls), "{calls:?}");
}

// --- Box, Rc, Arc, Pin ------------------------------------------------------------------------

const WRAP_SITE: &str = r#"use crate::link::Relay;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

pub fn box_as_ref(b: Box<Relay>, line: &str) -> bool {
    b.as_ref().from_daemon(line)
}

pub fn rc_as_ref(r: Rc<Relay>, line: &str) -> bool {
    r.as_ref().from_daemon(line)
}

pub fn arc_as_ref(a: Arc<Relay>, line: &str) -> bool {
    a.as_ref().from_daemon(line)
}

pub fn pin_as_mut(p: Pin<&mut Relay>, line: &str) -> bool {
    p.as_mut().from_daemon(line)
}

pub fn box_direct(b: Box<Relay>, line: &str) -> bool {
    b.from_daemon(line)
}

pub fn opt_box(o: Option<Box<Relay>>, line: &str) -> Option<bool> {
    Some(o.as_ref()?.from_daemon(line))
}

pub fn opt_pin(o: Option<Pin<&mut Relay>>, line: &str) -> Option<bool> {
    Some(o.as_ref()?.from_daemon(line))
}
"#;

const WRAP_SHADOW: &str = "pub struct Pin<T>(pub T);\npub struct Rc<T>(pub T);\npub struct Arc<T>(pub T);\npub struct Box<T>(pub T);\n";

fn wrap_fixture(namesake: bool) -> Fixture {
    if namesake {
        index(&[
            ("src/lib.rs", "pub mod link;\npub mod shadow;\npub mod wrap;\n"),
            ("src/link.rs", LINK),
            ("src/shadow.rs", WRAP_SHADOW),
            ("src/wrap.rs", WRAP_SITE),
        ])
    } else {
        index(&[
            ("src/lib.rs", "pub mod link;\npub mod wrap;\n"),
            ("src/link.rs", LINK),
            ("src/wrap.rs", WRAP_SITE),
        ])
    }
}

#[test]
fn box_as_ref_stays_std_beside_a_box() {
    let control = wrap_fixture(false);
    assert!(typed_relay_call(&typed_calls_of(&control.cg, "src/wrap.rs", "box_as_ref")), "control");
    let namesake = wrap_fixture(true);
    let calls = typed_calls_of(&namesake.cg, "src/wrap.rs", "box_as_ref");
    assert!(typed_relay_call(&calls), "{calls:?}");
}

#[test]
fn rc_as_ref_stays_std_beside_an_rc() {
    let control = wrap_fixture(false);
    assert!(typed_relay_call(&typed_calls_of(&control.cg, "src/wrap.rs", "rc_as_ref")), "control");
    let namesake = wrap_fixture(true);
    let calls = typed_calls_of(&namesake.cg, "src/wrap.rs", "rc_as_ref");
    assert!(typed_relay_call(&calls), "{calls:?}");
}

#[test]
fn arc_as_ref_stays_std_beside_an_arc() {
    let control = wrap_fixture(false);
    assert!(typed_relay_call(&typed_calls_of(&control.cg, "src/wrap.rs", "arc_as_ref")), "control");
    let namesake = wrap_fixture(true);
    let calls = typed_calls_of(&namesake.cg, "src/wrap.rs", "arc_as_ref");
    assert!(typed_relay_call(&calls), "{calls:?}");
}

#[test]
fn pin_as_mut_stays_std_beside_a_pin() {
    let control = wrap_fixture(false);
    assert!(typed_relay_call(&typed_calls_of(&control.cg, "src/wrap.rs", "pin_as_mut")), "control");
    let namesake = wrap_fixture(true);
    let calls = typed_calls_of(&namesake.cg, "src/wrap.rs", "pin_as_mut");
    assert!(typed_relay_call(&calls), "{calls:?}");
}

/// Nested: `Option<Box<Relay>>` read through `as_ref()?`. Today a namesake makes this site link nothing.
#[test]
fn opt_box_as_ref_try_stays_std_beside_a_box() {
    let control = wrap_fixture(false);
    assert!(typed_relay_call(&typed_calls_of(&control.cg, "src/wrap.rs", "opt_box")), "control");
    let namesake = wrap_fixture(true);
    let calls = typed_calls_of(&namesake.cg, "src/wrap.rs", "opt_box");
    assert!(typed_relay_call(&calls), "{calls:?}");
}

/// Nested: `Option<Pin<&mut Relay>>` read through `as_ref()?`. Today a namesake makes this site link nothing.
#[test]
fn opt_pin_as_ref_try_stays_std_beside_a_pin() {
    let control = wrap_fixture(false);
    assert!(typed_relay_call(&typed_calls_of(&control.cg, "src/wrap.rs", "opt_pin")), "control");
    let namesake = wrap_fixture(true);
    let calls = typed_calls_of(&namesake.cg, "src/wrap.rs", "opt_pin");
    assert!(typed_relay_call(&calls), "{calls:?}");
}

/// A call through `Box` without a wrapper method is not on the lent route, so a namesake leaves it alone.
#[test]
fn box_direct_is_unaffected_by_a_box_namesake() {
    let namesake = wrap_fixture(true);
    let calls = typed_calls_of(&namesake.cg, "src/wrap.rs", "box_direct");
    assert!(typed_relay_call(&calls), "{calls:?}");
}

// --- Mutex, RwLock, RefCell -------------------------------------------------------------------

const CELLS_SITE: &str = r#"use crate::link::Relay;
use std::cell::RefCell;
use std::sync::{Mutex, RwLock};

pub fn std_mutex_lock(m: Mutex<Relay>, line: &str) -> bool {
    m.lock().unwrap_or_else(|e| e.into_inner()).from_daemon(line)
}

pub fn std_rwlock_read(l: RwLock<Relay>, line: &str) -> bool {
    l.read().unwrap_or_else(|e| e.into_inner()).from_daemon(line)
}

pub fn std_refcell_borrow(c: RefCell<Relay>, line: &str) -> bool {
    c.borrow().from_daemon(line)
}
"#;

fn cells_fixture(namesake: bool) -> Fixture {
    if namesake {
        index(&[
            ("src/lib.rs", "pub mod cells;\npub mod link;\npub mod shadow;\n"),
            ("src/link.rs", LINK),
            ("src/shadow.rs", "pub struct Mutex<T>(pub T);\npub struct RwLock<T>(pub T);\npub struct RefCell<T>(pub T);\n"),
            ("src/cells.rs", CELLS_SITE),
        ])
    } else {
        index(&[
            ("src/lib.rs", "pub mod cells;\npub mod link;\n"),
            ("src/link.rs", LINK),
            ("src/cells.rs", CELLS_SITE),
        ])
    }
}

#[test]
fn std_mutex_lock_stays_std_beside_a_mutex() {
    let control = cells_fixture(false);
    assert!(typed_relay_call(&typed_calls_of(&control.cg, "src/cells.rs", "std_mutex_lock")), "control");
    let namesake = cells_fixture(true);
    let calls = typed_calls_of(&namesake.cg, "src/cells.rs", "std_mutex_lock");
    assert!(typed_relay_call(&calls), "{calls:?}");
}

#[test]
fn std_rwlock_read_stays_std_beside_an_rwlock() {
    let control = cells_fixture(false);
    assert!(typed_relay_call(&typed_calls_of(&control.cg, "src/cells.rs", "std_rwlock_read")), "control");
    let namesake = cells_fixture(true);
    let calls = typed_calls_of(&namesake.cg, "src/cells.rs", "std_rwlock_read");
    assert!(typed_relay_call(&calls), "{calls:?}");
}

#[test]
fn std_refcell_borrow_stays_std_beside_a_refcell() {
    let control = cells_fixture(false);
    assert!(typed_relay_call(&typed_calls_of(&control.cg, "src/cells.rs", "std_refcell_borrow")), "control");
    let namesake = cells_fixture(true);
    let calls = typed_calls_of(&namesake.cg, "src/cells.rs", "std_refcell_borrow");
    assert!(typed_relay_call(&calls), "{calls:?}");
}

/// Guard: a `Mutex` imported from a dependency is not the std one, so beside a project `Mutex` it stays blocked.
#[test]
fn a_dependency_mutex_stays_blocked_beside_a_project_mutex() {
    let fx = index(&[
        ("Cargo.toml", "[package]\nname = \"q\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nsome_dep = \"1\"\n"),
        ("src/lib.rs", "pub mod cells_dep;\npub mod link;\npub mod shadow;\n"),
        ("src/link.rs", LINK),
        ("src/shadow.rs", "pub struct Mutex<T>(pub T);\n"),
        (
            "src/cells_dep.rs",
            "use crate::link::Relay;\nuse some_dep::Mutex;\n\npub fn dep_mutex_lock(m: Mutex<Relay>, line: &str) -> bool {\n    m.lock().unwrap_or_else(|e| e.into_inner()).from_daemon(line)\n}\n",
        ),
    ]);
    let calls = typed_calls_of(&fx.cg, "src/cells_dep.rs", "dep_mutex_lock");
    assert!(!typed_relay_call(&calls), "{calls:?}");
}

/// Guard: a body-level `use std::option::Option;` does not change the parameter's type, which is read at
/// module level, so the project `Option` still blocks the site. An innermost-wins check would fail open here.
#[test]
fn a_body_std_option_does_not_reach_the_projects_parameter_type() {
    let fx = index(&[
        ("src/lib.rs", "pub mod link;\npub mod shadow;\npub mod site;\n"),
        ("src/link.rs", LINK),
        ("src/shadow.rs", OPTION_ENUM),
        (
            "src/site.rs",
            "use crate::link::Relay;\nuse crate::shadow::Option;\n\npub fn body_std(o: Option<Relay>, line: &str) -> bool {\n    use std::option::Option;\n    o.unwrap_or_else(|| Relay).from_daemon(line)\n}\n\npub fn body_std_try(o: Option<Relay>, line: &str) -> Option<bool> {\n    use std::option::Option;\n    Some(o.as_ref()?.from_daemon(line))\n}\n",
        ),
    ]);
    for function in ["body_std", "body_std_try"] {
        let calls = typed_calls_of(&fx.cg, "src/site.rs", function);
        assert!(!typed_relay_call(&calls), "{function}: {calls:?}");
    }
}

/// Guard: `use crate::shadow::{Box, Pin};` makes the project's wrappers the ones the file means, so each
/// lent route stays blocked: `as_ref`, `as_mut`, and the wrapper read through `Option`'s `as_ref()?`.
#[test]
fn an_imported_wrapper_stays_blocked_beside_a_project_wrapper() {
    let fx = index(&[
        ("src/lib.rs", "pub mod link;\npub mod shadow;\npub mod wrap;\n"),
        ("src/link.rs", LINK),
        ("src/shadow.rs", WRAP_SHADOW),
        (
            "src/wrap.rs",
            "use crate::link::Relay;\nuse crate::shadow::{Box, Pin};\n\npub fn box_as_ref(b: Box<Relay>, line: &str) -> bool { b.as_ref().from_daemon(line) }\npub fn pin_as_mut(p: Pin<&mut Relay>, line: &str) -> bool { p.as_mut().from_daemon(line) }\npub fn opt_box(o: Option<Box<Relay>>, line: &str) -> Option<bool> { Some(o.as_ref()?.from_daemon(line)) }\npub fn opt_pin(o: Option<Pin<&mut Relay>>, line: &str) -> Option<bool> { Some(o.as_ref()?.from_daemon(line)) }\n",
        ),
    ]);
    for function in ["box_as_ref", "pin_as_mut", "opt_box", "opt_pin"] {
        let calls = typed_calls_of(&fx.cg, "src/wrap.rs", function);
        assert!(!typed_relay_call(&calls), "{function}: {calls:?}");
    }
}

/// Guard: a project `Option` named by a module path (`shadow::Option`) is the project's, so the site stays blocked.
#[test]
fn a_qualified_project_option_stays_blocked() {
    let fx = index(&[
        ("src/lib.rs", "pub mod link;\npub mod qualified;\npub mod shadow;\n"),
        ("src/link.rs", LINK),
        ("src/shadow.rs", OPTION_ENUM),
        (
            "src/qualified.rs",
            "use crate::link::Relay;\nuse crate::shadow;\n\npub fn qual_else(o: shadow::Option<Relay>, line: &str) -> bool {\n    o.unwrap_or_else(|| Relay).from_daemon(line)\n}\n\npub fn qual_try(o: shadow::Option<Relay>, line: &str) -> Option<bool> {\n    Some(o.as_ref()?.from_daemon(line))\n}\n",
        ),
    ]);
    for function in ["qual_else", "qual_try"] {
        let calls = typed_calls_of(&fx.cg, "src/qualified.rs", function);
        assert!(!typed_relay_call(&calls), "{function}: {calls:?}");
    }
}

/// Guard: a binding from another file takes that file's `Option`, which is the project's there, so the site
/// stays blocked. `cross_try` has no valid control (it is `exact-match` on HEAD without the namesake too), so it
/// guards the fail-open direction only.
#[test]
fn a_binding_from_another_file_keeps_the_project_option_blocked() {
    let fx = index(&[
        ("src/lib.rs", "pub mod cross;\npub mod link;\npub mod other;\npub mod shadow;\n"),
        ("src/link.rs", LINK),
        ("src/shadow.rs", OPTION_ENUM),
        ("src/other.rs", "use crate::link::Relay;\nuse crate::shadow::Option;\n\npub fn get() -> Option<Relay> {\n    None\n}\n"),
        (
            "src/cross.rs",
            "use crate::link::Relay;\n\npub fn cross_else(line: &str) -> bool {\n    let o = crate::other::get();\n    o.unwrap_or_else(|| Relay).from_daemon(line)\n}\n\npub fn cross_try(line: &str) -> Option<bool> {\n    let o = crate::other::get();\n    Some(o.as_ref()?.from_daemon(line))\n}\n",
        ),
    ]);
    for function in ["cross_else", "cross_try"] {
        let calls = typed_calls_of(&fx.cg, "src/cross.rs", function);
        assert!(!typed_relay_call(&calls), "{function}: {calls:?}");
    }
}

/// A std `Arc` written by its path (`std::sync::Arc<Relay>`) beside a project `Arc` stays std: the lent route
/// reads the written path, so the bare project name does not switch it off.
#[test]
fn a_std_path_arc_stays_std_beside_a_project_arc() {
    let control = index(&[
        ("src/lib.rs", "pub mod arc;\npub mod link;\n"),
        ("src/link.rs", LINK),
        ("src/arc.rs", "use crate::link::Relay;\n\npub fn arc_full(a: std::sync::Arc<Relay>, line: &str) -> bool {\n    a.as_ref().from_daemon(line)\n}\n"),
    ]);
    assert!(typed_relay_call(&typed_calls_of(&control.cg, "src/arc.rs", "arc_full")), "control");
    let fx = index(&[
        ("src/lib.rs", "pub mod arc;\npub mod link;\npub mod shadow;\n"),
        ("src/link.rs", LINK),
        ("src/shadow.rs", "pub struct Arc<T>(pub T);\n"),
        ("src/arc.rs", "use crate::link::Relay;\n\npub fn arc_full(a: std::sync::Arc<Relay>, line: &str) -> bool {\n    a.as_ref().from_daemon(line)\n}\n"),
    ]);
    let calls = typed_calls_of(&fx.cg, "src/arc.rs", "arc_full");
    assert!(typed_relay_call(&calls), "{calls:?}");
}

const PIN_LOCAL_SITE: &str = "use crate::link::Relay;\nuse std::pin::Pin;\n\npub fn pin_local(line: &str) -> bool {\n    let mut r = Relay;\n    let p: Pin<&mut Relay> = Pin::new(&mut r);\n    p.as_mut().from_daemon(line)\n}\n";

/// A declared `Pin` local beside a project `Pin` stays std: the annotation is written in this file, so the
/// method route reads it as the std `Pin`.
#[test]
fn a_declared_pin_local_stays_std_beside_a_project_pin() {
    let control = index(&[
        ("src/lib.rs", "pub mod link;\npub mod site;\n"),
        ("src/link.rs", LINK),
        ("src/site.rs", PIN_LOCAL_SITE),
    ]);
    assert!(typed_relay_call(&typed_calls_of(&control.cg, "src/site.rs", "pin_local")), "control");
    let fx = index(&[
        ("src/lib.rs", "pub mod link;\npub mod shadow;\npub mod site;\n"),
        ("src/link.rs", LINK),
        ("src/shadow.rs", "pub struct Pin<T>(pub T);\n"),
        ("src/site.rs", PIN_LOCAL_SITE),
    ]);
    let calls = typed_calls_of(&fx.cg, "src/site.rs", "pin_local");
    assert!(typed_relay_call(&calls), "{calls:?}");
}

const ANN_OTHER: &str = "use crate::link::Relay;\n\npub fn get() -> Option<Relay> {\n    None\n}\n";
const ANN_SITE: &str = "use crate::link::Relay;\n\npub fn ann_try(line: &str) -> Option<bool> {\n    let o: Option<Relay> = crate::other::get();\n    Some(o.as_ref()?.from_daemon(line))\n}\n\npub fn ann_else(line: &str) -> bool {\n    let o: Option<Relay> = crate::other::get();\n    o.unwrap_or_else(|| Relay).from_daemon(line)\n}\n";

/// An annotated `Option<Relay>` local, written in a file that does not import the project `Option`, reads the
/// std one, whatever file its value comes from.
#[test]
fn an_annotated_std_option_local_stays_std_beside_a_project_option() {
    let control = index(&[
        ("src/lib.rs", "pub mod ann;\npub mod link;\npub mod other;\n"),
        ("src/link.rs", LINK),
        ("src/other.rs", ANN_OTHER),
        ("src/ann.rs", ANN_SITE),
    ]);
    for function in ["ann_try", "ann_else"] {
        assert!(typed_relay_call(&typed_calls_of(&control.cg, "src/ann.rs", function)), "control {function}");
    }
    let fx = index(&[
        ("src/lib.rs", "pub mod ann;\npub mod link;\npub mod other;\npub mod shadow;\n"),
        ("src/link.rs", LINK),
        ("src/other.rs", ANN_OTHER),
        ("src/shadow.rs", OPTION_ENUM),
        ("src/ann.rs", ANN_SITE),
    ]);
    for function in ["ann_try", "ann_else"] {
        let calls = typed_calls_of(&fx.cg, "src/ann.rs", function);
        assert!(typed_relay_call(&calls), "{function}: {calls:?}");
    }
}

/// Guard: the same annotated local with `use crate::shadow::Option;` in its file is the project's `Option`.
#[test]
fn an_annotated_project_option_local_stays_blocked() {
    let guarded = format!("use crate::shadow::Option;\n{ANN_SITE}");
    let fx = index(&[
        ("src/lib.rs", "pub mod ann;\npub mod link;\npub mod other;\npub mod shadow;\n"),
        ("src/link.rs", LINK),
        ("src/other.rs", ANN_OTHER),
        ("src/shadow.rs", OPTION_ENUM),
        ("src/ann.rs", guarded.as_str()),
    ]);
    for function in ["ann_try", "ann_else"] {
        let calls = typed_calls_of(&fx.cg, "src/ann.rs", function);
        assert!(!typed_relay_call(&calls), "{function}: {calls:?}");
    }
}

/// Guard: a dependency's `Option` (`some_dep::Option`) is not the std one, so the site stays blocked beside a
/// project `Option`. Each call must be linked (so the guard is not vacuous) and not by the typed route.
#[test]
fn a_dependency_option_stays_blocked_beside_a_project_option() {
    let fx = index(&[
        ("Cargo.toml", "[package]\nname = \"q\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nsome_dep = \"1\"\n"),
        ("src/lib.rs", "pub mod dep_site;\npub mod link;\npub mod shadow;\n"),
        ("src/link.rs", LINK),
        ("src/shadow.rs", OPTION_ENUM),
        (
            "src/dep_site.rs",
            "use crate::link::Relay;\n\npub fn dep_else(o: some_dep::Option<Relay>, line: &str) -> bool {\n    o.unwrap_or_else(|| Relay).from_daemon(line)\n}\n\npub fn dep_try(o: some_dep::Option<Relay>, line: &str) -> Option<bool> {\n    Some(o.as_ref()?.from_daemon(line))\n}\n",
        ),
    ]);
    for function in ["dep_else", "dep_try"] {
        let calls = typed_calls_of(&fx.cg, "src/dep_site.rs", function);
        assert!(calls.iter().any(|c| c.contains("Relay::from_daemon")), "{function} is not linked: {calls:?}");
        assert!(!typed_relay_call(&calls), "{function}: {calls:?}");
    }
}

/// Guard: `use` is order-independent within a block, so a module-level `use crate::shadow::Option;` written after
/// the functions that use `Option` still makes the project's `Option` the one they mean.
#[test]
fn a_trailing_module_import_still_blocks_the_project_option() {
    let fx = index(&[
        ("src/lib.rs", "pub mod link;\npub mod shadow;\npub mod trailing;\n"),
        ("src/link.rs", LINK),
        ("src/shadow.rs", OPTION_ENUM),
        (
            "src/trailing.rs",
            "use crate::link::Relay;\n\npub fn trailing_else(o: Option<Relay>, line: &str) -> bool {\n    o.unwrap_or_else(|| Relay).from_daemon(line)\n}\n\npub fn trailing_try(o: Option<Relay>, line: &str) -> Option<bool> {\n    Some(o.as_ref()?.from_daemon(line))\n}\n\nuse crate::shadow::Option;\n",
        ),
    ]);
    for function in ["trailing_else", "trailing_try"] {
        let calls = typed_calls_of(&fx.cg, "src/trailing.rs", function);
        assert!(calls.iter().any(|c| c.contains("Relay::from_daemon")), "{function} is not linked: {calls:?}");
        assert!(!typed_relay_call(&calls), "{function}: {calls:?}");
    }
}

const SAME_FILE_ALIAS_SITE: &str = "use crate::link::Relay;\n\npub type Result<T> = std::result::Result<T, String>;\n\npub fn same_default(r: Result<Relay>, line: &str) -> bool {\n    r.unwrap_or_default().from_daemon(line)\n}\n";
const SAME_FILE_CONTROL_SITE: &str = "use crate::link::Relay;\n\npub fn same_default(r: Result<Relay, String>, line: &str) -> bool {\n    r.unwrap_or_default().from_daemon(line)\n}\n";

/// The site's own file declares `Result<T>` as an alias of std's `Result` whose first parameter is the payload. The
/// file sees the alias, so the alias rule reads it as std and `unwrap_or_default` stays std. Control: the same call
/// on a plain std `Result`.
#[test]
fn a_same_file_result_alias_reads_as_std() {
    let control = index(&[
        ("src/lib.rs", "pub mod link;\npub mod same;\n"),
        ("src/link.rs", LINK),
        ("src/same.rs", SAME_FILE_CONTROL_SITE),
    ]);
    assert!(typed_relay_call(&typed_calls_of(&control.cg, "src/same.rs", "same_default")), "control");
    let namesake = index(&[
        ("src/lib.rs", "pub mod link;\npub mod same;\n"),
        ("src/link.rs", LINK),
        ("src/same.rs", SAME_FILE_ALIAS_SITE),
    ]);
    let calls = typed_calls_of(&namesake.cg, "src/same.rs", "same_default");
    assert!(typed_relay_call(&calls), "{calls:?}");
}

const GLOB_ALIAS_SITE: &str = "use crate::link::Relay;\nuse crate::names::*;\n\npub fn glob_default(r: Result<Relay>, line: &str) -> bool {\n    r.unwrap_or_default().from_daemon(line)\n}\n";
const GLOB_CONTROL_SITE: &str = "use crate::link::Relay;\n\npub fn glob_default(r: Result<Relay, String>, line: &str) -> bool {\n    r.unwrap_or_default().from_daemon(line)\n}\n";

/// The alias reaches the site through a glob, so the glob brings it into sight; the alias rule reads it as std.
#[test]
fn a_glob_brought_result_alias_reads_as_std() {
    let control = index(&[
        ("src/lib.rs", "pub mod link;\npub mod glob_site;\n"),
        ("src/link.rs", LINK),
        ("src/glob_site.rs", GLOB_CONTROL_SITE),
    ]);
    assert!(typed_relay_call(&typed_calls_of(&control.cg, "src/glob_site.rs", "glob_default")), "control");
    let namesake = index(&[
        ("src/lib.rs", "pub mod link;\npub mod names;\npub mod glob_site;\n"),
        ("src/link.rs", LINK),
        ("src/names.rs", "pub type Result<T> = std::result::Result<T, String>;\n"),
        ("src/glob_site.rs", GLOB_ALIAS_SITE),
    ]);
    let calls = typed_calls_of(&namesake.cg, "src/glob_site.rs", "glob_default");
    assert!(typed_relay_call(&calls), "{calls:?}");
}

const IO_STD_SITE: &str = "use crate::link::Relay;\nuse std::io;\n\npub fn io_default(r: io::Result<Relay>, line: &str) -> bool {\n    r.unwrap_or_default().from_daemon(line)\n}\n";
const IO_PROJECT_SITE: &str = "use crate::io;\nuse crate::link::Relay;\n\npub fn io_project(r: io::Result<Relay>, line: &str) -> bool {\n    r.unwrap_or_default().from_daemon(line)\n}\n";

/// `io::Result<Relay>` with `use std::io;` is std's `Result`: its root expands through the file's `use`, so a project
/// `Result` in another module does not reach it. Control: the same site in a project with no `Result` namesake.
/// The second site's `io` is a project module whose `Result` is a struct, so it must stay blocked.
#[test]
fn io_result_through_std_io_reads_as_std_beside_a_project_result() {
    let control = index(&[
        ("src/lib.rs", "pub mod link;\npub mod io_std;\n"),
        ("src/link.rs", LINK),
        ("src/io_std.rs", IO_STD_SITE),
    ]);
    assert!(typed_relay_call(&typed_calls_of(&control.cg, "src/io_std.rs", "io_default")), "control");
    let namesake = index(&[
        ("src/lib.rs", "pub mod link;\npub mod names;\npub mod io;\npub mod io_std;\npub mod io_project;\n"),
        ("src/link.rs", LINK),
        ("src/names.rs", "pub type Result<T> = std::result::Result<T, String>;\n"),
        ("src/io.rs", "pub struct Result<T>(pub T);\n"),
        ("src/io_std.rs", IO_STD_SITE),
        ("src/io_project.rs", IO_PROJECT_SITE),
    ]);
    let std_calls = typed_calls_of(&namesake.cg, "src/io_std.rs", "io_default");
    assert!(typed_relay_call(&std_calls), "{std_calls:?}");
    let project_calls = typed_calls_of(&namesake.cg, "src/io_project.rs", "io_project");
    assert!(!typed_relay_call(&project_calls), "{project_calls:?}");
}

const PRELUDE_SITE: &str = "use crate::link::Relay;\nuse crate::prelude::*;\n\npub fn prelude_else(o: Option<Relay>, line: &str) -> bool {\n    o.unwrap_or_else(|| Relay).from_daemon(line)\n}\n\npub fn prelude_try(o: Option<Relay>, line: &str) -> Option<bool> {\n    Some(o.as_ref()?.from_daemon(line))\n}\n";

/// Guard: `Option` reaches the site through a glob over a module that re-exports the project's `Option`. The
/// re-export must still block; the glob's reach is read, not assumed empty.
#[test]
fn a_prelude_re_export_still_blocks_the_project_option() {
    let fx = index(&[
        ("src/lib.rs", "pub mod link;\npub mod shadow;\npub mod prelude;\npub mod site_prelude;\n"),
        ("src/link.rs", LINK),
        ("src/shadow.rs", OPTION_ENUM),
        ("src/prelude.rs", "pub use crate::shadow::Option;\n"),
        ("src/site_prelude.rs", PRELUDE_SITE),
    ]);
    for function in ["prelude_else", "prelude_try"] {
        let calls = typed_calls_of(&fx.cg, "src/site_prelude.rs", function);
        assert!(calls.iter().any(|c| c.contains("Relay::from_daemon")), "{function} is not linked: {calls:?}");
        assert!(!typed_relay_call(&calls), "{function}: {calls:?}");
    }
}

/// Guard: a `use` written as a block's last statement, after the `let` that uses `Option`, still names the project's
/// `Option` in that block. Rust `use` applies to the whole block, so the binding's declaration must see it.
#[test]
fn a_block_scoped_trailing_import_still_blocks_the_project_option() {
    let fx = index(&[
        ("src/lib.rs", "pub mod link;\npub mod shadow;\npub mod site_block;\n"),
        ("src/link.rs", LINK),
        ("src/shadow.rs", OPTION_ENUM),
        (
            "src/site_block.rs",
            "use crate::link::Relay;\n\npub fn block_else(line: &str) -> bool {\n    let o: Option<Relay> = None;\n    let r = o.unwrap_or_else(|| Relay).from_daemon(line);\n    use crate::shadow::Option;\n    r\n}\n\npub fn block_try(line: &str) -> Option<bool> {\n    let o: Option<Relay> = None;\n    let r = Some(o.as_ref()?.from_daemon(line));\n    use crate::shadow::Option;\n    r\n}\n",
        ),
    ]);
    for function in ["block_else", "block_try"] {
        let calls = typed_calls_of(&fx.cg, "src/site_block.rs", function);
        assert!(calls.iter().any(|c| c.contains("Relay::from_daemon")), "{function} is not linked: {calls:?}");
        assert!(!typed_relay_call(&calls), "{function}: {calls:?}");
    }
}
