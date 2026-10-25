//! Python scoping the call matchers lean on: pytest fixtures, what a function binds locally,
//! `from x import y` names, and whether a module lives in the project.

use super::python::{python_binds_locally, python_global_bindings, python_import_keys, PythonBinding};
use super::util::*;
use crate::ctx::Ctx;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_extract::strip_comments::strip_comments_for_regex;
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::{js_trim, utf16_len};
use codegraph_types::{Language, Node, NodeKind};
use std::collections::HashMap;
use std::rc::Rc;

memo_map!(PluggedModules, String, Rc<Vec<String>>);
memo_map!(FixtureTypes, String, Option<String>);
memo_map!(LocalBinds, String, bool);
memo_map!(FromImports, String, Rc<HashMap<String, String>>);
memo_map!(ModuleLocal, String, bool);
memo_map!(OutsideNames, String, bool);
memo_map!(CommentFreeLines, String, Rc<Vec<String>>);
memo_map!(TopLevelBound, String, bool);

pub fn is_pytest_fixture(n: &Node, ctx: &Ctx) -> bool {
    if re!(r"(?:^|/)conftest\.py$").is_match(&n.file_path)
        || n.decorators.as_ref().is_some_and(|ds| ds.iter().any(|d| re!(r"(?:^|\.)fixture\b").is_match(d)))
    {
        return true;
    }
    is_decorated_fixture(n, ctx)
}

/// A fixture the file can see: its own, a `conftest.py` above it, or one a plugin module provides.
pub fn is_fixture_in_reach(n: &Node, file_path: &str, ctx: &Ctx) -> bool {
    if n.file_path == file_path {
        return is_pytest_fixture(n, ctx);
    }
    if let Some(conftest) = re!(r"^(.*?)(?:^|/)conftest\.py$").captures(&n.file_path) {
        let base = conftest.text(1).unwrap_or("");
        return base.is_empty() || file_path.starts_with(&format!("{base}/"));
    }
    if !is_pytest_fixture(n, ctx) {
        return false;
    }
    plugged_fixture_modules(file_path, ctx).iter().any(|m| n.file_path == *m || n.file_path.ends_with(&format!("/{m}")))
}

fn plugged_fixture_modules(file_path: &str, ctx: &Ctx) -> Rc<Vec<String>> {
    let dir = match file_path.rfind('/') {
        Some(i) => file_path[..i].to_string(),
        None => String::new(),
    };
    memoized::<PluggedModules>(ctx, dir.clone(), || {
        let mut modules = Vec::new();
        let mut d = dir;
        loop {
            let path = if d.is_empty() { "conftest.py".to_string() } else { format!("{d}/conftest.py") };
            if let Some(text) = ctx.read_file(&path).filter(|t| !t.is_empty()) {
                for m in re!(r"^\s*from\s+([\w.]+)\s+import\s+\*", "gm").captures_iter(&text) {
                    modules.push(format!("{}.py", m.text(1).unwrap_or("").replace('.', "/")));
                }
                let plugins = re!(r"^\s*pytest_plugins\s*=\s*[\[(]([^\])]*)[\])]", "m").captures(&text).and_then(|c| c.text(1).map(|s| s.to_string())).unwrap_or_default();
                for m in re!(r#"["']([\w.]+)["']"#, "g").captures_iter(&plugins) {
                    modules.push(format!("{}.py", m.text(1).unwrap_or("").replace('.', "/")));
                }
            }
            if d.is_empty() {
                break;
            }
            d = match d.rfind('/') {
                Some(i) => d[..i].to_string(),
                None => String::new(),
            };
        }
        Rc::new(modules)
    })
}

/// The class a pytest fixture parameter is built from (`return Foo()` / `yield foo`), by name.
pub fn python_fixture_return_type(receiver: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    if !re!(r"^[a-z_]\w*$").is_match(receiver) || receiver == "self" || receiver == "cls" {
        return None;
    }
    let key = format!("{}\0{receiver}", r.from_node_id);
    memoized::<FixtureTypes>(ctx, key, || {
        let caller = ctx.node_by_id(&r.from_node_id)?;
        let lines = lines_of(ctx, &caller.file_path);
        let signature = if matches!(caller.kind, NodeKind::Function | NodeKind::Method) {
            let from = caller.start_line as usize - 1;
            let joined = lines.iter().skip(from).take(5).cloned().collect::<Vec<_>>().join(" ");
            re!(r"\)\s*(?:->[^:]*)?:").split(&joined).first().map(|s| s.to_string()).unwrap_or_default()
        } else {
            String::new()
        };
        if !Re::cached(&format!(r"[(,]\s*{}\s*(?:[:=,)]|$)", escape_regex(receiver)), "").is_match(&signature) {
            return None;
        }
        let mut fixtures: Vec<NodeRef> = ctx
            .get_nodes_by_name(receiver)
            .iter()
            .filter(|n| n.kind == NodeKind::Function && n.language == Language::Python && is_fixture_in_reach(n, &r.file_path, ctx))
            .cloned()
            .collect();
        fixtures.sort_by(|a, b| {
            let ka = if a.file_path == r.file_path { -1i32 } else { 0 };
            let kb = if b.file_path == r.file_path { -1i32 } else { 0 };
            ka.cmp(&kb).then(utf16_len(&b.file_path).cmp(&utf16_len(&a.file_path)))
        });
        let fixture = fixtures.first()?;
        let flines = lines_of(ctx, &fixture.file_path);
        let body = flines.iter().skip(fixture.start_line as usize).take((fixture.end_line - fixture.start_line) as usize).cloned().collect::<Vec<_>>().join("\n");
        let returned = re!(r"^\s*(?:return|yield)\s+([A-Za-z_][\w.]*)\s*(\()?", "m").captures(&body)?;
        let first = returned.text(1)?.to_string();
        let direct = if returned.text(2).is_some() {
            Some(first)
        } else {
            Re::cached(&format!(r"^\s*{}\s*=\s*([A-Za-z_][\w.]*)\s*\(", first.replace('.', "\\.")), "m")
                .captures(&body)
                .and_then(|c| c.text(1).map(|s| s.to_string()))
        };
        let cls = direct.and_then(|d| d.rsplit('.').next().map(|s| s.to_string()))?;
        if re!(r"^[A-Z]").is_match(&cls) {
            Some(cls)
        } else {
            None
        }
    })
}

fn is_decorated_fixture(n: &Node, ctx: &Ctx) -> bool {
    if n.language != Language::Python || n.kind != NodeKind::Function {
        return false;
    }
    let lines = lines_of(ctx, &n.file_path);
    let mut open: i64 = 0;
    let mut i = n.start_line as i64 - 2;
    while i >= 0 && i >= n.start_line as i64 - 16 {
        let text = lines.get(i as usize).map(|l| js_trim(l)).unwrap_or("");
        open += text.matches(')').count() as i64 - text.matches('(').count() as i64;
        i -= 1;
        if open > 0 {
            continue;
        }
        if !text.starts_with('@') {
            return false;
        }
        if re!(r"^@(?:\w+\.)*fixture\b").is_match(text) {
            return true;
        }
    }
    false
}

/// The ref's file with comments blanked, one string per line, memoized.
fn comment_free_lines(file_path: &str, ctx: &Ctx) -> Rc<Vec<String>> {
    memoized::<CommentFreeLines>(ctx, file_path.to_string(), || {
        let source = ctx.read_file(file_path).map(|s| s.to_string()).unwrap_or_default();
        Rc::new(crate::ctx::split_lines(&strip_comments_for_regex(&source, "python")))
    })
}

/// Is `name` bound inside the function enclosing `r` (parameter, assignment, loop or `as` target),
/// or assigned at module level?
pub fn is_python_locally_bound(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    python_local_binding(name, r, ctx)
}

fn python_local_binding(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let mut fns: Vec<NodeRef> = ctx
        .get_nodes_in_file(&r.file_path)
        .iter()
        .filter(|f| matches!(f.kind, NodeKind::Function | NodeKind::Method) && f.start_line <= r.line && f.end_line >= r.line)
        .cloned()
        .collect();
    fns.sort_by_key(|f| f.end_line - f.start_line);
    let func = fns.first().cloned();
    let key = format!(
        "{}\0{}\0{name}\0{}",
        r.file_path,
        func.as_ref().map(|f| f.id.as_str()).unwrap_or(""),
        if func.is_some() { String::new() } else { r.line.to_string() }
    );
    memoized::<LocalBinds>(ctx, key, || {
        if python_from_imports(&r.file_path, ctx).contains_key(name) {
            return false;
        }
        let lines = comment_free_lines(&r.file_path, ctx);
        let n = escape_regex(name);
        let assigns = Re::cached(&format!(r"^\s*(?:[\w\s,*()\[\]]*,\s*)?\(?\*?{n}\)?\s*(?:,[\w\s,*()\[\]]*)?(?::[^=]+)?=(?!=)"), "");
        let targets = Re::cached(&format!(r"\bfor\s+[\w\s,()]*\b{n}\b[\w\s,()]*\s+in\b|\bas\s+{n}\b"), "");
        let params = Re::cached(&format!(r"[(,]\s*\*{{0,2}}{n}\s*(?:[:=,)]|$)"), "");
        let mut bound = false;
        if let Some(f) = &func {
            let mut i = f.start_line as usize - 1;
            let mut signature = String::new();
            let stop = lines.len().min(f.start_line as usize + 20);
            while i < stop {
                signature.push_str(&lines[i]);
                if re!(r"\)\s*(?:->[^:]*)?:\s*(?:#.*)?$").is_match(&lines[i]) {
                    break;
                }
                i += 1;
            }
            let from_paren = match signature.find('(') {
                Some(p) => &signature[p..],
                None => "",
            };
            bound = params.is_match(from_paren);
            let mut line = i + 1;
            while !bound && line + 1 < r.line as usize {
                let text = lines.get(line).map(|s| s.as_str()).unwrap_or("");
                bound = assigns.is_match(text) || targets.is_match(text);
                line += 1;
            }
        }
        if !bound {
            let module_key = format!("{}\0{n}", r.file_path);
            bound = memoized::<TopLevelBound>(ctx, module_key, || {
                let top = Re::cached(&format!(r"^(?:[\w,\s]*,\s*)?{n}\s*(?:,[\w\s,]*)?(?::[^=]+)?=(?!=)"), "");
                lines.iter().any(|l| top.is_match(l))
            });
        }
        bound
    })
}

/// `from module import name [as alias]` bindings of a file: bound name → module.
pub fn python_from_imports(file_path: &str, ctx: &Ctx) -> Rc<HashMap<String, String>> {
    memoized::<FromImports>(ctx, file_path.to_string(), || {
        let mut names: HashMap<String, String> = HashMap::new();
        let text = ctx.read_file(file_path).map(|s| s.to_string()).unwrap_or_default();
        for m in re!(r"^\s*from\s+([\w.]+)\s+import\s+(\([^)]*\)|[^\n#]+)", "gm").captures_iter(&text) {
            let module = m.text(1).unwrap_or("").to_string();
            let items = re!(r"[()]", "g").replace_all_str(m.text(2).unwrap_or(""), "");
            for item in items.split(',') {
                let bound = re!(r"(\w+)\s*$").captures(js_trim(item)).and_then(|c| c.text(1).map(|s| s.to_string()));
                if let Some(b) = bound {
                    if b != "*" {
                        names.insert(b, module.clone());
                    }
                }
            }
        }
        Rc::new(names)
    })
}

pub fn is_python_name_imported_from_outside(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let imports = python_from_imports(&r.file_path, ctx);
    let Some(module) = imports.get(name) else { return false };
    if module.is_empty() || module.starts_with('.') {
        return false;
    }
    !is_python_project_module(module, &r.file_path, ctx)
}

fn is_python_project_module(module: &str, from_file: &str, ctx: &Ctx) -> bool {
    memoized::<ModuleLocal>(ctx, format!("{from_file}\0{module}"), || ctx.resolve_module_file(module, from_file, Language::Python).is_some())
}

/// A receiver name that is an imported module from outside the project (and not shadowed locally).
pub fn is_python_outside_module_receiver(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if r.language != Language::Python {
        return false;
    }
    is_python_outside_module_name(name, &r.file_path, ctx) && !python_binds_locally(name, r, ctx, false)
}

pub fn is_python_outside_module_name(name: &str, file_path: &str, ctx: &Ctx) -> bool {
    if !is_word_str(name) {
        return false;
    }
    memoized::<OutsideNames>(ctx, format!("outside\0{file_path}\0{name}"), || {
        let keys = python_import_keys(name, file_path, ctx);
        if keys.is_empty() {
            return false;
        }
        if python_global_bindings(name, file_path, ctx).iter().any(|b| !matches!(b, PythonBinding::Import { .. })) {
            return false;
        }
        if ctx.get_nodes_in_file(file_path).iter().any(|n| n.name == name && matches!(n.kind, NodeKind::Class | NodeKind::Function)) {
            return false;
        }
        keys.iter().all(|key| {
            let module = &key[..key.find(':').unwrap_or(key.len())];
            !module.starts_with('.') && !is_python_project_module_any_root(module, file_path, ctx)
        })
    })
}

fn is_python_project_module_any_root(module: &str, from_file: &str, ctx: &Ctx) -> bool {
    let parts: Vec<&str> = module.split('.').collect();
    (0..parts.len()).any(|i| is_python_project_module(&parts[i..].join("."), from_file, ctx))
}
