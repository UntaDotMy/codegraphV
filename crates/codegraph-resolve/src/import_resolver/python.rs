//! Python module resolution — the Python half of `import-resolver.ts`.
//!
//! Python imports name MODULES (`pkg.sub.mod`, `..sibling`), which the interpreter finds by
//! searching `sys.path` roots, so the answer depends on where packages start — not on any file
//! that merely ends in `pkg/sub/mod.py`.
//!
//! Two kinds of root, both derived from evidence:
//!  - PROJECT roots — what `sys.path` holds for any code in the repo: the repo root; each service
//!    root (a directory with its own `pyproject.toml`, `setup.cfg` or `setup.py`); the `src/` of
//!    any of those; and the package directories their build config declares. These may answer an
//!    importer anywhere, but only unambiguously.
//!  - LOCAL roots — the parent of every top-level regular package, a script's own directory, a
//!    namespace package's parent seen from inside it. These answer only importers within their
//!    REACH: a fixture's `tests/fixtures/proj/requests/` must never become the application's
//!    `requests`.

use super::{find_exported_symbol, resolve_import_path, Want};
use crate::ctx::Ctx;
use crate::types::{ImportMapping, NodeRef};
use codegraph_extract::strip_comments::{strip_comments_for_regex, utf16_to_byte_offset};
use codegraph_jsre::{re, utf16_len};
use codegraph_types::jscompat::js_trim;
use codegraph_types::{nodepath, Language, NodeKind};
use indexmap::{IndexMap, IndexSet};
use sha1::{Digest, Sha1};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

pub struct Heads {
    pub concrete: Vec<String>,
    pub namespace: Vec<String>,
}

pub struct PythonModuleIndex {
    pub files: HashSet<String>,
    /// Directories holding an `__init__.py` — regular packages.
    pub package_dirs: IndexSet<String>,
    /// Every directory that holds a `.py` file at any depth — namespace candidates.
    pub dirs: IndexSet<String>,
    pub project_roots: IndexSet<String>,
    /// Local roots, each with the directory whose importers it may answer.
    pub local_roots: IndexMap<String, String>,
    /// Per top-level name, the roots (project and local) that provide it, split by kind.
    heads_by_name: RefCell<HashMap<String, Rc<Heads>>>,
}

#[derive(Default)]
pub struct PythonModuleIndexes(RefCell<Option<Rc<PythonModuleIndex>>>);

#[derive(Default)]
pub struct PythonStarExportMemo(RefCell<HashMap<String, Option<Rc<HashSet<String>>>>>);

/// Directory names that, by near-universal convention, hold code that is not the project's
/// importable API: test inputs, sample projects, vendored copies.
const PYTHON_NON_PROJECT_DIRS: &[&str] =
    &["tests", "test", "testing", "fixtures", "testdata", "examples", "example", "samples", "vendor", "third_party"];

pub fn parent_dir(dir: &str) -> String {
    match dir.rfind('/') {
        Some(i) => dir[..i].to_string(),
        None => String::new(),
    }
}

pub fn join_rel(dir: &str, rel: &str) -> String {
    if dir.is_empty() {
        rel.to_string()
    } else {
        format!("{dir}/{rel}")
    }
}

/// Whether `dir` is `scope` or inside it.
fn is_within(dir: &str, scope: &str) -> bool {
    scope.is_empty() || dir == scope || dir.starts_with(&format!("{scope}/"))
}

fn python_module_index(ctx: &Ctx) -> Rc<PythonModuleIndex> {
    let memo = ctx.memo::<PythonModuleIndexes>();
    if let Some(i) = memo.0.borrow().as_ref() {
        return Rc::clone(i);
    }
    let index = Rc::new(build_python_module_index(ctx));
    *memo.0.borrow_mut() = Some(Rc::clone(&index));
    index
}

fn build_python_module_index(ctx: &Ctx) -> PythonModuleIndex {
    let mut files: HashSet<String> = HashSet::new();
    let mut package_dirs: IndexSet<String> = IndexSet::new();
    let mut dirs: IndexSet<String> = IndexSet::from([String::new()]);
    for f in ctx.get_all_files() {
        let norm = f.replace('\\', "/");
        if !re!(r"\.py$").is_match(&norm) {
            continue;
        }
        files.insert(norm.clone());
        let dir = parent_dir(&norm);
        if norm == "__init__.py" || norm.ends_with("/__init__.py") {
            package_dirs.insert(dir.clone());
        }
        let mut d = dir;
        while !d.is_empty() && !dirs.contains(&d) {
            dirs.insert(d.clone());
            d = parent_dir(&d);
        }
    }

    // Where a service's roots may answer from: everywhere (None), only itself when a package
    // encloses it, or the nearest enclosing directory named as non-project code.
    let service_reach = |service: &str| -> Option<String> {
        if package_dirs.contains(service) {
            return Some(service.to_string());
        }
        let mut a = parent_dir(service);
        while !a.is_empty() {
            if package_dirs.contains(&a) {
                return Some(service.to_string());
            }
            let last = &a[a.rfind('/').map_or(0, |i| i + 1)..];
            if PYTHON_NON_PROJECT_DIRS.contains(&last) {
                return Some(a);
            }
            a = parent_dir(&a);
        }
        None
    };

    let mut project_roots: IndexSet<String> = IndexSet::from([String::new()]);
    let mut local_roots: IndexMap<String, String> = IndexMap::new();
    let mut add_local = |local_roots: &mut IndexMap<String, String>, root: String, reach: String| {
        let replace = match local_roots.get(&root) {
            None => true,
            // Two claims on one root: the wider reach wins.
            Some(prior) => is_within(prior, &reach),
        };
        if replace {
            local_roots.insert(root, reach);
        }
    };
    for dir in &package_dirs {
        let mut top = dir.clone();
        while !top.is_empty() && package_dirs.contains(&parent_dir(&top)) {
            top = parent_dir(&top);
        }
        if !top.is_empty() {
            add_local(&mut local_roots, parent_dir(&top), parent_dir(&top));
        }
    }
    for service in &dirs {
        let has_config = files.contains(&join_rel(service, "setup.py"))
            || ctx.file_exists(&join_rel(service, "pyproject.toml"))
            || ctx.file_exists(&join_rel(service, "setup.cfg"));
        if !has_config && !service.is_empty() {
            continue;
        }
        let reach = if service.is_empty() { None } else { service_reach(service) };
        let mut roots = vec![service.clone()];
        let src = join_rel(service, "src");
        if dirs.contains(&src) && !package_dirs.contains(&src) {
            roots.push(src);
        }
        for declared in declared_python_roots(ctx, service) {
            if dirs.contains(&declared) {
                roots.push(declared);
            }
        }
        for root in roots {
            match &reach {
                None => {
                    project_roots.insert(root);
                }
                Some(r) => add_local(&mut local_roots, root, r.clone()),
            }
        }
    }
    for root in &project_roots {
        local_roots.shift_remove(root);
    }
    PythonModuleIndex { files, package_dirs, dirs, project_roots, local_roots, heads_by_name: RefCell::new(HashMap::new()) }
}

/// Package roots a build config declares, relative to the repo root.
fn declared_python_roots(ctx: &Ctx, service: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |raw: &str| {
        let t = js_trim(raw);
        let t = re!(r#"^["']|["']$"#, "g").replace_all_str(t, "");
        let t = re!(r"^\.(?:/|$)").replace_all_str(&t, "");
        let dir = re!(r"/+$").replace_all_str(&t, "");
        if dir.is_empty() || dir.starts_with("..") || nodepath::is_absolute(&dir) {
            return;
        }
        out.push(join_rel(service, &dir));
    };
    if let Some(toml) = ctx.read_file(&join_rel(service, "pyproject.toml")).filter(|s| !s.is_empty()) {
        for (table, body) in ini_tables(&toml) {
            match table.as_str() {
                "tool.setuptools.packages.find" => {
                    if let Some(m) = re!(r"^\s*where\s*=\s*\[([^\]]*)\]", "m").captures(&body) {
                        for item in m.text(1).unwrap().split(',') {
                            add(item);
                        }
                    }
                }
                "tool.setuptools.package-dir" => {
                    if let Some(m) = re!(r#"^\s*["']{2}\s*=\s*["']([^"']+)["']"#, "m").captures(&body) {
                        add(m.text(1).unwrap());
                    }
                }
                "tool.setuptools" => {
                    if let Some(m) = re!(r#"package-dir\s*=\s*\{[^}]*?["']{2}\s*=\s*["']([^"']+)["']"#).captures(&body) {
                        add(m.text(1).unwrap());
                    }
                }
                "tool.poetry" => {
                    if let Some(pkgs) = re!(r"^\s*packages\s*=\s*\[([\s\S]*?)\]\s*$", "m").captures(&body) {
                        for m in re!(r#"\bfrom\s*=\s*["']([^"']+)["']"#, "g").captures_iter(pkgs.text(1).unwrap()) {
                            add(m.text(1).unwrap());
                        }
                    }
                }
                "tool.hatch.build.targets.wheel" => {
                    if let Some(pkgs) = re!(r"^\s*packages\s*=\s*\[([^\]]*)\]", "m").captures(&body) {
                        for item in pkgs.text(1).unwrap().split(',') {
                            let p = re!(r#"^["']|["']$"#, "g").replace_all_str(js_trim(item), "");
                            if let Some(slash) = p.rfind('/') {
                                add(&p[..slash]);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(cfg) = ctx.read_file(&join_rel(service, "setup.cfg")).filter(|s| !s.is_empty()) {
        for (table, body) in ini_tables(&cfg) {
            if table == "options" {
                if let Some(m) = re!(r"^\s*package_dir\s*=\s*(?:\n\s+)?=\s*(\S+)\s*$", "m").captures(&body) {
                    add(m.text(1).unwrap());
                }
            } else if table == "options.packages.find" {
                if let Some(m) = re!(r"^\s*where\s*=\s*(\S+)\s*$", "m").captures(&body) {
                    add(m.text(1).unwrap());
                }
            }
        }
    }
    out
}

/// `[table]` sections of a TOML / INI file, with the body under each.
fn ini_tables(content: &str) -> Vec<(String, String)> {
    let headers: Vec<(usize, usize, String)> = re!(r"^[ \t]*\[([^\[\]\n]+)\][ \t]*(?:#.*)?$", "gm")
        .captures_iter(content)
        .map(|h| {
            let whole = h.get(0).unwrap();
            (whole.start(), whole.end(), h.text(1).unwrap().to_string())
        })
        .collect();
    let mut out = Vec::new();
    for (i, (_, end_of_header, name)) in headers.iter().enumerate() {
        let end = if i + 1 < headers.len() { headers[i + 1].0 } else { content.len() };
        out.push((js_trim(name).replace(['"', '\''], ""), content[*end_of_header..end].to_string()));
    }
    out
}

/// The module file at `base` (dir-relative path), in Python's own order: a regular package
/// `base/__init__.py` beats a module `base.py`; a bare directory (a namespace portion) has no
/// file of its own.
fn python_module_at(base: &str, index: &PythonModuleIndex) -> Option<String> {
    let init = join_rel(base, "__init__.py");
    if index.files.contains(&init) {
        return Some(init);
    }
    let module = format!("{base}.py");
    index.files.contains(&module).then_some(module)
}

/// The module file for the dotted path `rel_path` (slash-joined) under `root`, walking it the
/// way the import system does: every prefix must be a package — regular, or a namespace
/// directory — so a prefix that is a plain module ends the walk.
fn python_module_under(root: &str, rel_path: &str, index: &PythonModuleIndex) -> Option<String> {
    let parts: Vec<&str> = rel_path.split('/').collect();
    let mut base = root.to_string();
    for part in &parts[..parts.len() - 1] {
        base = join_rel(&base, part);
        if index.files.contains(&join_rel(&base, "__init__.py")) {
            continue;
        }
        if index.files.contains(&format!("{base}.py")) || !index.dirs.contains(&base) {
            return None;
        }
    }
    python_module_at(&join_rel(root, rel_path), index)
}

#[derive(PartialEq, Eq)]
enum HeadKind {
    Concrete,
    Namespace,
}

/// How `root` provides the top-level name `head`: concretely, as a namespace portion, or not at all.
fn python_head_kind(root: &str, head: &str, index: &PythonModuleIndex) -> Option<HeadKind> {
    let base = join_rel(root, head);
    if python_module_at(&base, index).is_some() {
        return Some(HeadKind::Concrete);
    }
    index.dirs.contains(&base).then_some(HeadKind::Namespace)
}

/// Every root (project or local) providing the top-level name `head`, by kind.
fn python_root_heads(head: &str, index: &PythonModuleIndex) -> Rc<Heads> {
    if let Some(h) = index.heads_by_name.borrow().get(head) {
        return Rc::clone(h);
    }
    let mut heads = Heads { concrete: Vec::new(), namespace: Vec::new() };
    for root in index.project_roots.iter().chain(index.local_roots.keys()) {
        match python_head_kind(root, head, index) {
            Some(HeadKind::Concrete) => heads.concrete.push(root.clone()),
            Some(HeadKind::Namespace) => heads.namespace.push(root.clone()),
            None => {}
        }
    }
    let heads = Rc::new(heads);
    index.heads_by_name.borrow_mut().insert(head.to_string(), Rc::clone(&heads));
    heads
}

/// Resolve a Python import specifier to the file of the module it names — `pkg.sub.mod` →
/// `pkg/sub/mod.py`, `pkg` → `pkg/__init__.py`, `..sibling` relative to `from_file`'s package.
/// `None` when the module isn't in the index, when a relative import climbs above its top-level
/// package, or when two project roots that don't contain the importer both provide it.
pub fn resolve_python_module(specifier: &str, from_file: &str, ctx: &Ctx) -> Option<String> {
    if specifier.is_empty() || !re!(r"^\.*[\w.]*$").is_match(specifier) {
        return None;
    }
    let index = python_module_index(ctx);
    let importer_dir = parent_dir(&from_file.replace('\\', "/"));

    // Relative: leading dots are package levels (one dot = the importer's own package), the rest
    // a dotted path below it.
    let dots = specifier.len() - specifier.trim_start_matches('.').len();
    if dots > 0 {
        let mut base = importer_dir.clone();
        for _level in 1..dots {
            // Python refuses to climb out of the top-level package. Only a regular package
            // proves where the top is; a namespace package's boundary is unknown.
            if index.package_dirs.contains(&base) && !index.package_dirs.contains(&parent_dir(&base)) {
                return None;
            }
            if base.is_empty() {
                return None;
            }
            base = parent_dir(&base);
        }
        let rest = &specifier[dots..];
        if !rest.is_empty() {
            return python_module_under(&base, &rest.replace('.', "/"), &index);
        }
        // `from . import x` — the package itself, i.e. its `__init__.py`.
        let init = join_rel(&base, "__init__.py");
        return index.files.contains(&init).then_some(init);
    }

    let rel_path = specifier.replace('.', "/");
    let head = specifier.split('.').next().unwrap_or("");

    // Roots that contain the importer, nearest first.
    let mut containing: Vec<String> = Vec::new();
    let mut dir: Option<String> = Some(importer_dir.clone());
    while let Some(d) = dir {
        let head_dir = join_rel(&d, head);
        let is_root = index.project_roots.contains(&d)
            || index.local_roots.contains_key(&d)
            || (!index.package_dirs.contains(&d)
                && (d == importer_dir || importer_dir == head_dir || importer_dir.starts_with(&format!("{head_dir}/"))));
        if is_root {
            containing.push(d.clone());
        }
        dir = if d.is_empty() { None } else { Some(parent_dir(&d)) };
    }

    // A root containing the importer is on its path: the nearest one that holds the module
    // answers. If one holds `head` itself as a regular package or module but not the rest of the
    // path, Python stops there too.
    let mut head_is_local = false;
    for root in &containing {
        if let Some(hit) = python_module_under(root, &rel_path, &index) {
            return Some(hit);
        }
        if python_head_kind(root, head, &index) == Some(HeadKind::Concrete) {
            head_is_local = true;
        }
    }
    if head_is_local {
        return None;
    }

    // Otherwise a root elsewhere whose reach includes the importer, as `sys.path` would rank
    // them: a root holding `head` concretely shadows any namespace portion, and either way the
    // answer must be unambiguous.
    let heads = python_root_heads(head, &index);
    let reaches = |root: &String| -> bool {
        !containing.contains(root) && (index.project_roots.contains(root) || is_within(&importer_dir, &index.local_roots[root]))
    };
    let concrete: Vec<&String> = heads.concrete.iter().filter(|r| reaches(r)).collect();
    if !concrete.is_empty() {
        return if concrete.len() == 1 { python_module_under(concrete[0], &rel_path, &index) } else { None };
    }
    let mut found: Option<String> = None;
    for root in &heads.namespace {
        if !reaches(root) {
            continue;
        }
        let Some(hit) = python_module_under(root, &rel_path, &index) else { continue };
        if found.as_ref().is_some_and(|f| *f != hit) {
            return None;
        }
        found = Some(hit);
    }
    found
}

/// A digest of the Python root set — each root with its tier and reach. A change here can move
/// the answer for any import anywhere, so sync re-opens every Python resolution when it changes.
/// Built fresh, not from the memo.
pub fn python_root_fingerprint(ctx: &Ctx) -> String {
    let index = build_python_module_index(ctx);
    if index.files.is_empty() {
        return String::new();
    }
    let mut hash = Sha1::new();
    let mut roots: Vec<&String> = index.project_roots.iter().collect();
    roots.sort_by(|a, b| js_cmp(a, b));
    hash.update(roots.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n").as_bytes());
    hash.update(b"\0");
    let mut local: Vec<String> = index.local_roots.iter().map(|(root, reach)| format!("{root}\t{reach}")).collect();
    local.sort_by(|a, b| js_cmp(a, b));
    hash.update(local.join("\n").as_bytes());
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// `Array.prototype.sort()` default order: by UTF-16 code units.
pub fn js_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

pub struct PythonReopenScope {
    pub target_files: Vec<String>,
    pub source_dirs: Vec<String>,
    pub module_leaves: Vec<String>,
}

/// What a sync must re-open when Python files were added or removed but the root set held.
pub fn python_reopen_scope(ctx: &Ctx, changed_files: &[String]) -> PythonReopenScope {
    let index = build_python_module_index(ctx);
    let roots: Vec<&String> = index.project_roots.iter().chain(index.local_roots.keys()).collect();
    let module_names = |file: &str| -> Vec<String> {
        let mut out = Vec::new();
        let m = re!(r"(?:^|/)__init__\.py$").replace_all_str(file, "");
        let module = re!(r"\.py$").replace_all_str(&m, "");
        for root in &roots {
            if !root.is_empty() && !module.starts_with(&format!("{root}/")) {
                continue;
            }
            let rel = if root.is_empty() { module.as_str() } else { &module[root.len() + 1..] };
            if !rel.is_empty() {
                out.push(rel.replace('/', "."));
            }
        }
        out
    };
    let mut affected: IndexSet<String> = IndexSet::new();
    let mut source_dirs: IndexSet<String> = IndexSet::new();
    for raw in changed_files {
        let file = raw.replace('\\', "/");
        if !re!(r"\.py$").is_match(&file) {
            continue;
        }
        for m in module_names(&file) {
            affected.insert(m);
        }
        if file == "__init__.py" || file.ends_with("/__init__.py") {
            source_dirs.insert(parent_dir(&file));
        }
    }
    let mut target_files = Vec::new();
    // `index.files` is a Set in the TypeScript, iterated in insertion (path) order.
    let mut files: Vec<&String> = index.files.iter().collect();
    files.sort();
    for file in files {
        let hit = module_names(file).iter().any(|m| {
            let mut i = m.len();
            while i > 0 {
                if affected.contains(&m[..i]) {
                    return true;
                }
                i = m[..i].rfind('.').unwrap_or(0);
            }
            false
        });
        if hit {
            target_files.push(file.clone());
        }
    }
    let mut leaves: IndexSet<String> = IndexSet::new();
    for m in &affected {
        leaves.insert(m[m.rfind('.').map_or(0, |i| i + 1)..].to_string());
    }
    PythonReopenScope { target_files, source_dirs: source_dirs.into_iter().collect(), module_leaves: leaves.into_iter().collect() }
}

/// The Python files that import one of the packages whose `__init__.py` is in `init_files`, each
/// with the local names bound from it and what each name binds to NOW (`file\0name` of the
/// definition, `file\0` for a submodule, `None` when it can't be said). An importer that is
/// itself a package `__init__.py` passes the binding on, so its own importers are included too,
/// to a fixed point.
pub fn python_package_importers(ctx: &Ctx, init_files: &[String]) -> IndexMap<String, IndexMap<String, Option<String>>> {
    let index = python_module_index(ctx);
    let mut packages: HashSet<String> = init_files.iter().map(|f| f.replace('\\', "/")).collect();
    let mut out: IndexMap<String, IndexMap<String, Option<String>>> = IndexMap::new();
    let binding_now = |imp: &ImportMapping, file: &str| -> Option<String> {
        if imp.is_namespace || imp.exported_name == "*" {
            return None;
        }
        let package_file = resolve_import_path(&imp.source, file, Language::Python, ctx)?;
        let want = Want { is_default: false, is_namespace: false, exported_name: imp.exported_name.clone(), member_name: None };
        if let Some(symbol) = find_exported_symbol(&package_file, &want, Language::Python, ctx, &mut HashSet::new(), 0) {
            return Some(format!("{}\0{}", symbol.file_path, symbol.name));
        }
        let sub_spec = if imp.source.ends_with('.') {
            format!("{}{}", imp.source, imp.exported_name)
        } else {
            format!("{}.{}", imp.source, imp.exported_name)
        };
        match resolve_import_path(&sub_spec, file, Language::Python, ctx) {
            Some(sub) => Some(format!("{sub}\0")),
            None => Some("\0".to_string()),
        }
    };
    // Candidate importers from the import nodes (named by their specifier), so only files that
    // import an edited package pay for a mapping parse.
    let import_nodes: Vec<NodeRef> =
        ctx.get_nodes_by_kind(NodeKind::Import).iter().filter(|n| re!(r"\.py$").is_match(&n.file_path)).cloned().collect();
    let mut grew = true;
    while grew {
        grew = false;
        let mut candidates: IndexSet<String> = IndexSet::new();
        for n in &import_nodes {
            let target = resolve_import_path(&n.name, &n.file_path, Language::Python, ctx);
            if let Some(t) = target {
                if packages.contains(&t) && index.files.contains(&n.file_path) {
                    candidates.insert(n.file_path.clone());
                }
            }
        }
        for file in &candidates {
            for imp in ctx.get_import_mappings(file, Language::Python).iter() {
                let target = resolve_import_path(&imp.source, file, Language::Python, ctx);
                let Some(target) = target else { continue };
                if !packages.contains(&target) || target == *file {
                    continue;
                }
                let names = out.entry(file.clone()).or_default();
                names.insert(imp.local_name.clone(), binding_now(imp, file));
                // `import pkg.sub` is used as `pkg.sub.N`.
                if imp.is_namespace {
                    names.insert(imp.source.split('.').next().unwrap().to_string(), None);
                }
                if re!(r"(?:^|/)__init__\.py$").is_match(file) && !packages.contains(file) {
                    packages.insert(file.clone());
                    grew = true;
                }
            }
        }
    }
    out
}

/// The file node of the module `specifier` names, as seen from `from_file`.
pub fn python_module_file_node(specifier: &str, from_file: &str, ctx: &Ctx) -> Option<NodeRef> {
    let file = resolve_import_path(specifier, from_file, Language::Python, ctx)?;
    if file == from_file {
        return None;
    }
    ctx.get_nodes_in_file(&file).iter().find(|n| n.kind == NodeKind::File).cloned()
}

/// The names `from <file> import *` binds: the string literals of the module's `__all__` when it
/// assigns one, otherwise every public (non-`_`) name. `None` means "every public name".
fn python_star_exports(file_path: &str, ctx: &Ctx) -> Option<Rc<HashSet<String>>> {
    let memo = ctx.memo::<PythonStarExportMemo>();
    if let Some(v) = memo.0.borrow().get(file_path) {
        return v.clone();
    }
    let mut names: Option<Rc<HashSet<String>>> = None;
    if let Some(content) = ctx.read_file(file_path).filter(|c| c.contains("__all__")) {
        // Locate every statement that builds `__all__` on blanked source (so a docstring mention
        // is ignored), then read the literals from the original at the same offsets. Only a list
        // written out in full is trusted.
        let code = strip_comments_for_regex(&content, "python");
        // Blanking can shrink multi-byte characters, so offsets are carried through UTF-16.
        let to_original = |code_off: usize| -> usize {
            if code.len() == content.len() {
                code_off
            } else {
                utf16_to_byte_offset(&content, utf16_len(&code[..code_off]))
            }
        };
        let mut literal = true;
        let mut collected: HashSet<String> = HashSet::new();
        for m in re!(r"^[ \t]*__all__\b[ \t]*(\.|\+=|=|:[^=\n]*=)?[ \t]*(.?)", "gm").captures_iter(&code) {
            let whole = m.get(0).unwrap();
            // `open` is the offset of the last character the match consumed.
            let last_char_len = whole.as_str().chars().next_back().map_or(0, |c| c.len_utf8());
            let open = whole.end() - last_char_len;
            let bracket = m.text(2).unwrap_or("");
            let close: Option<usize> = match bracket {
                "[" => code[open..].find(']').map(|i| open + i),
                "(" => code[open..].find(')').map(|i| open + i),
                _ => None,
            };
            let assigns = m.get(1).is_some_and(|g| g.as_str() != ".");
            let tail = match close {
                None => "",
                Some(c) => match code[c..].find('\n') {
                    Some(nl) => &code[c + 1..c + nl],
                    None => &code[c + 1..],
                },
            };
            if !assigns || close.is_none() || !js_trim(tail).is_empty() {
                literal = false;
                break;
            }
            let (o, c) = (to_original(open), to_original(close.unwrap()));
            for lit in re!(r#"["']([A-Za-z_]\w*)["']"#, "g").captures_iter(&content[o..c.max(o)]) {
                collected.insert(lit.text(1).unwrap().to_string());
            }
        }
        if literal && !collected.is_empty() {
            names = Some(Rc::new(collected));
        }
    }
    memo.0.borrow_mut().insert(file_path.to_string(), names.clone());
    names
}

pub fn python_star_allows(file_path: &str, name: &str, ctx: &Ctx) -> bool {
    match python_star_exports(file_path, ctx) {
        Some(names) => names.contains(name),
        None => !name.starts_with('_'),
    }
}
