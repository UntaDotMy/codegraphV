//! Go module path detection — port of `src/resolution/go-module.ts`.
//!
//! A project can hold several modules, so the resolver reads the `go.mod` nearest every
//! directory with indexed Go files and maps an import path to the module that declares
//! the longest prefix of it (#2322).

use codegraph_jsre::re;
use codegraph_types::nodepath;
use std::rc::Rc;

#[derive(Debug, PartialEq, Eq)]
pub struct GoModule {
    /// The module path declared in `go.mod`, e.g. `github.com/example/myproject`.
    pub module_path: String,
    /// Absolute path to the directory containing the `go.mod` file.
    pub root_dir: String,
}

/// Read the `go.mod` in `module_dir`; `None` if it is missing or has no `module` directive.
pub fn load_go_module(module_dir: &str) -> Option<Rc<GoModule>> {
    let go_mod = nodepath::join(&[module_dir, "go.mod"]);
    let content = String::from_utf8_lossy(&std::fs::read(go_mod).ok()?).into_owned();
    // `module <path>` is the first non-comment directive in any valid go.mod.
    let stripped = re!(r"//[^\n]*", "g").replace_all_str(&content, "");
    let m = re!(r"^\s*module\s+(\S+)\s*$", "m").captures(&stripped)?;
    // Strip optional quoting around the module path.
    let module_path = re!(r#"^["']|["']$"#, "g").replace_all_str(m.text(1).unwrap(), "");
    if module_path.is_empty() {
        return None;
    }
    Some(Rc::new(GoModule { module_path, root_dir: module_dir.to_string() }))
}

/// The module an import path belongs to: the one whose module path equals it or is a
/// `/`-bounded prefix of it; the longest wins, a tie prefers `own`.
pub fn find_go_module_for_import(import_path: &str, modules: &[Rc<GoModule>], own: Option<&Rc<GoModule>>) -> Option<Rc<GoModule>> {
    let mut best: Option<Rc<GoModule>> = None;
    let mut all: Vec<&Rc<GoModule>> = modules.iter().collect();
    if let Some(o) = own {
        if !modules.iter().any(|m| Rc::ptr_eq(m, o)) {
            all.push(o);
        }
    }
    for m in all {
        if import_path != m.module_path && !(import_path.starts_with(&m.module_path) && import_path[m.module_path.len()..].starts_with('/')) {
            continue;
        }
        let length: isize = best.as_ref().map_or(-1, |b| codegraph_jsre::utf16_len(&b.module_path) as isize);
        let mine = codegraph_jsre::utf16_len(&m.module_path) as isize;
        if mine > length || (mine == length && own.is_some_and(|o| Rc::ptr_eq(o, m))) {
            best = Some(Rc::clone(m));
        }
    }
    best
}

/// The project-relative directory (`/`-separated, `.` for the project root) of the
/// package `import_path` names inside module `m`.
pub fn go_module_package_dir(import_path: &str, m: &GoModule, project_root: &str) -> String {
    let mod_dir = nodepath::relative(project_root, &m.root_dir).split(nodepath::SEP).collect::<Vec<_>>().join("/");
    let rest = if import_path == m.module_path { "" } else { &import_path[m.module_path.len() + 1..] };
    nodepath::posix::join(&[&mod_dir, rest])
}
