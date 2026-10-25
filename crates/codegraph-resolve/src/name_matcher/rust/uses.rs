//! What a Rust file imports and how its modules lay out: `use` trees, `mod` blocks, the Cargo
//! crate graph, and the project items a path (or a glob) can reach.
//!
//! Offsets here are byte offsets into the comment-stripped file text (`RustUses::text`); a node's
//! offset is its line start plus its UTF-16 column converted into that text.

use crate::ctx::Ctx;
use crate::frameworks::cargo_workspace::get_cargo_workspace_crate_map;
use crate::name_matcher::util::{memo_map, memoized};
use crate::types::UnresolvedRef;
use codegraph_jsre::{re, Re};
use codegraph_extract::strip_comments::strip_comments_for_regex;
use codegraph_types::jscompat::{js_trim, utf16_byte_index};
use codegraph_types::nodepath::posix;
use codegraph_types::{Node, NodeKind, Visibility};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

pub const RUST_PRELUDE: [&str; 33] = [
    "Ok", "Err", "Some", "None", "Result", "Option", "Box", "Vec", "String", "Default", "Drop", "Iterator",
    "IntoIterator", "From", "Into", "Clone", "Copy", "Send", "Sync", "Sized", "ToString", "ToOwned", "PartialEq",
    "Eq", "PartialOrd", "Ord", "AsRef", "AsMut", "Fn", "FnMut", "FnOnce", "Extend", "drop",
];

pub fn in_rust_prelude(name: &str) -> bool {
    RUST_PRELUDE.contains(&name)
}

pub const RUST_STD_ROOTS: [&str; 4] = ["std", "core", "alloc", "proc_macro"];

pub fn is_rust_std_root(name: &str) -> bool {
    RUST_STD_ROOTS.contains(&name)
}

#[derive(Debug, Clone)]
pub struct RustUsePath {
    pub segs: Vec<String>,
    pub at: usize,
    pub pub_: bool,
}

/// An inline `mod name { … }`; `parent` indexes the file's `blocks`.
#[derive(Debug, Clone)]
pub struct RustModBlock {
    pub name: String,
    pub open: usize,
    pub close: usize,
    pub parent: Option<usize>,
}

pub struct RustUses {
    /// Every identifier in a `use` of an in-project path.
    pub names: HashSet<String>,
    /// Segments before a `*` in an in-project `use`.
    pub globs: HashSet<String>,
    /// Leaves of `use`s that reach std or a dependency crate.
    pub external: HashSet<String>,
    /// Leaves of `use`s of in-project paths.
    pub bound: HashSet<String>,
    pub bound_from: HashMap<String, HashSet<String>>,
    pub paths: HashMap<String, Vec<RustUsePath>>,
    pub glob_paths: Vec<RustUsePath>,
    pub blocks: Vec<RustModBlock>,
    pub path_mods: HashSet<String>,
    pub balanced: bool,
    pub line_starts: Vec<usize>,
    pub text: String,
}

impl RustUses {
    /// Offset of `line` (1-based) at UTF-16 `column` (`lineStarts[line - 1] + max(0, column)`).
    pub fn offset_of(&self, line: u32, column: u32) -> usize {
        let start = (line as usize).checked_sub(1).and_then(|i| self.line_starts.get(i)).copied().unwrap_or(0);
        let start = start.min(self.text.len());
        start + utf16_byte_index(&self.text[start..], column as usize)
    }
}

// -- Cargo manifests -----------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DepKind {
    Normal,
    Dev,
    Build,
    Workspace,
}

#[derive(Debug, Clone)]
pub struct RustManifestDependency {
    pub key: String,
    pub pkg: String,
    pub path: Option<String>,
    pub workspace: bool,
    pub kind: DepKind,
}

fn read_dependency_fields(dep: &mut RustManifestDependency, fields: &str) {
    if let Some(pkg) = re!(r#"\bpackage\s*=\s*"([^"]+)""#).captures(fields).and_then(|c| c.text(1).map(String::from)) {
        if !pkg.is_empty() {
            dep.pkg = pkg.replace('-', "_");
        }
    }
    if let Some(at) = re!(r#"\bpath\s*=\s*"([^"]+)""#).captures(fields).and_then(|c| c.text(1).map(String::from)) {
        if !at.is_empty() {
            dep.path = Some(at.replace('\\', "/"));
        }
    }
    if re!(r"\bworkspace\s*=\s*true\b").is_match(fields) {
        dep.workspace = true;
    }
}

/// The dependencies a `Cargo.toml` declares, in the order they first appear.
pub fn rust_manifest_dependencies(text: &str) -> Vec<RustManifestDependency> {
    type Deps = IndexMap<(DepKind, String), RustManifestDependency>;
    fn entry(deps: &mut Deps, kind: DepKind, name: &str) -> (DepKind, String) {
        let key = name.replace('-', "_");
        let k = (kind, key.clone());
        deps.entry(k.clone()).or_insert_with(|| RustManifestDependency { key: key.clone(), pkg: key, path: None, workspace: false, kind });
        k
    }
    let mut deps: Deps = IndexMap::new();
    let mut kind: Option<DepKind> = None;
    let mut table: Option<(DepKind, String)> = None;
    for raw in re!(r"\r?\n").split(text) {
        let line: String = js_trim(&re!(r"#.*$").replace_first(raw, |_| String::new())).to_string();
        let header_inner = re!(r"^\[([^\]]+)\]$").captures(&line).map(|h| js_trim(h.text(1).unwrap_or("")).to_string());
        if let Some(inner) = header_inner {
            let m = re!(r"^(?:(workspace)\.|target\..+\.)?(dev-|build-)?dependencies(?:\.([A-Za-z0-9_-]+))?$").captures(&inner);
            let of = m.as_ref().map(|m| {
                if m.text(1).is_some() {
                    DepKind::Workspace
                } else if m.text(2) == Some("dev-") {
                    DepKind::Dev
                } else if m.text(2) == Some("build-") {
                    DepKind::Build
                } else {
                    DepKind::Normal
                }
            });
            let sub = m.as_ref().and_then(|m| m.text(3)).filter(|s| !s.is_empty()).map(String::from);
            kind = match (of, &sub) {
                (Some(of), None) => Some(of),
                _ => None,
            };
            table = match (of, sub) {
                (Some(of), Some(name)) => Some(entry(&mut deps, of, &name)),
                _ => None,
            };
            continue;
        }
        if let Some(t) = &table {
            if let Some(dep) = deps.get_mut(t) {
                read_dependency_fields(dep, &line);
            }
            continue;
        }
        let Some(k) = kind else { continue };
        let parsed: Option<(String, Option<String>, String)> = re!(r#"^"?([A-Za-z0-9_-]+)"?\s*(?:\.\s*([A-Za-z_-]+)\s*)?=\s*(.*)$"#)
            .captures(&line)
            .map(|d| (d.text(1).unwrap_or("").to_string(), d.text(2).filter(|s| !s.is_empty()).map(String::from), d.text(3).unwrap_or("").to_string()));
        if let Some((name, sub, value)) = parsed {
            let fields = match sub {
                Some(sub) => format!("{sub} = {value}"),
                None => value,
            };
            let key = entry(&mut deps, k, &name);
            if let Some(dep) = deps.get_mut(&key) {
                read_dependency_fields(dep, &fields);
            }
        }
    }
    deps.into_values().collect()
}

memo_map!(RustDependencyMemo, (), Rc<HashSet<String>>);
memo_map!(RustCrateNamesMemo, (), Rc<HashSet<String>>);

/// Dependency crates of the project's manifests, minus the project's own crates.
pub fn rust_dependency_crates(ctx: &Ctx) -> Rc<HashSet<String>> {
    memoized::<RustDependencyMemo>(ctx, (), || {
        let mut deps: HashSet<String> = HashSet::new();
        let mut manifests: Vec<String> = vec!["Cargo.toml".to_string()];
        for dir in get_cargo_workspace_crate_map(ctx).values() {
            manifests.push(format!("{dir}/Cargo.toml"));
        }
        let mut seen: HashSet<String> = HashSet::new();
        for manifest in manifests {
            if !seen.insert(manifest.clone()) {
                continue;
            }
            for d in rust_manifest_dependencies(&ctx.read_file(&manifest).map(|t| t.to_string()).unwrap_or_default()) {
                deps.insert(d.key);
            }
        }
        for own in rust_project_crates(ctx).iter() {
            deps.remove(own);
        }
        Rc::new(deps)
    })
}

/// The project's own crate names, `_`-spelled.
pub fn rust_project_crates(ctx: &Ctx) -> Rc<HashSet<String>> {
    memoized::<RustCrateNamesMemo>(ctx, (), || {
        let mut crates: HashSet<String> = HashSet::new();
        let root = ctx.read_file("Cargo.toml").map(|t| t.to_string()).unwrap_or_default();
        if let Some(name) = re!(r#"\[package\][^\[]*?\bname\s*=\s*"([^"]+)""#).captures(&root).and_then(|c| c.text(1).map(String::from)) {
            if !name.is_empty() {
                crates.insert(name.replace('-', "_"));
            }
        }
        for name in get_cargo_workspace_crate_map(ctx).keys() {
            crates.insert(name.replace('-', "_"));
        }
        Rc::new(crates)
    })
}

// -- Crate graph ---------------------------------------------------------------------------

pub struct RustCrate {
    pub name: String,
    /// Dependency key → the crate directory it names.
    pub keys: HashMap<String, String>,
    pub deps: HashSet<String>,
}

pub struct RustCrateGraph {
    pub crates: IndexMap<String, RustCrate>,
    pub reach: HashMap<String, HashSet<String>>,
    file_crate: RefCell<HashMap<String, Option<String>>>,
}

memo_map!(RustCrateGraphMemo, (), Rc<RustCrateGraph>);

fn join_crate_dir(dir: &str, rel: &str) -> Option<String> {
    let joined = if dir.is_empty() { rel.to_string() } else { format!("{dir}/{rel}") };
    let joined = posix::normalize(&joined);
    let joined = joined.trim_end_matches('/').to_string();
    if joined == "." {
        Some(String::new())
    } else if joined == ".." || joined.starts_with("../") || posix::is_absolute(&joined) {
        None
    } else {
        Some(joined)
    }
}

struct CrateScan {
    crates: IndexMap<String, RustCrate>,
    manifests: HashMap<String, String>,
    by_name: HashMap<String, String>,
}

impl CrateScan {
    fn add(&mut self, dir: &str, ctx: &Ctx) -> bool {
        if self.crates.contains_key(dir) {
            return true;
        }
        let path = if dir.is_empty() { "Cargo.toml".to_string() } else { format!("{dir}/Cargo.toml") };
        let Some(text) = ctx.read_file(&path).map(|t| t.to_string()).filter(|t| !t.is_empty()) else { return false };
        let Some(name) = re!(r#"\[package\][^\[]*?\bname\s*=\s*"([^"]+)""#)
            .captures(&text)
            .and_then(|c| c.text(1).map(String::from))
            .filter(|n| !n.is_empty())
            .map(|n| n.replace('-', "_"))
        else {
            return false;
        };
        self.crates.insert(dir.to_string(), RustCrate { name: name.clone(), keys: HashMap::new(), deps: HashSet::new() });
        self.manifests.insert(dir.to_string(), text);
        self.by_name.entry(name).or_insert_with(|| dir.to_string());
        true
    }
}

/// The workspace's crates, what each depends on, and what each can reach.
pub fn rust_crate_graph(ctx: &Ctx) -> Rc<RustCrateGraph> {
    memoized::<RustCrateGraphMemo>(ctx, (), || {
        let mut scan = CrateScan { crates: IndexMap::new(), manifests: HashMap::new(), by_name: HashMap::new() };
        scan.add("", ctx);
        for dir in get_cargo_workspace_crate_map(ctx).values() {
            let dir = re!(r"^\.(?:/|$)").replace_first(dir, |_| String::new());
            scan.add(&dir, ctx);
        }
        let root_manifest = ctx.read_file("Cargo.toml").map(|t| t.to_string()).unwrap_or_default();
        let inherited: HashMap<String, RustManifestDependency> = rust_manifest_dependencies(&root_manifest)
            .into_iter()
            .filter(|d| d.kind == DepKind::Workspace)
            .map(|d| (d.key.clone(), d))
            .collect();
        let mut queue: Vec<String> = scan.crates.keys().cloned().collect();
        let mut i = 0;
        while i < queue.len() {
            let dir = queue[i].clone();
            i += 1;
            let manifest = scan.manifests.get(&dir).cloned().unwrap_or_default();
            for dep in rust_manifest_dependencies(&manifest) {
                if dep.kind == DepKind::Workspace {
                    continue;
                }
                let shared = if dep.workspace { inherited.get(&dep.key) } else { None };
                let at = match shared {
                    Some(s) => s.path.as_ref().and_then(|p| join_crate_dir("", p)),
                    None => dep.path.as_ref().and_then(|p| join_crate_dir(&dir, p)),
                };
                if let Some(at) = &at {
                    if !scan.crates.contains_key(at) && scan.add(at, ctx) {
                        queue.push(at.clone());
                    }
                }
                let to = match &at {
                    Some(at) if scan.crates.contains_key(at) => Some(at.clone()),
                    _ => scan.by_name.get(shared.map(|s| s.pkg.as_str()).unwrap_or(&dep.pkg)).cloned(),
                };
                let Some(to) = to else { continue };
                if to == dir {
                    continue;
                }
                let crate_ = scan.crates.get_mut(&dir).expect("queued crate exists");
                crate_.keys.insert(dep.key.clone(), to.clone());
                if dep.kind == DepKind::Normal {
                    crate_.deps.insert(to);
                }
            }
        }
        let crates = scan.crates;
        let mut reach: HashMap<String, HashSet<String>> = HashMap::new();
        for dir in crates.keys() {
            let mut seen: HashSet<String> = HashSet::from([dir.clone()]);
            let mut pending = vec![dir.clone()];
            while let Some(next) = pending.pop() {
                for to in &crates[&next].deps {
                    if seen.insert(to.clone()) {
                        pending.push(to.clone());
                    }
                }
            }
            reach.insert(dir.clone(), seen);
        }
        Rc::new(RustCrateGraph { crates, reach, file_crate: RefCell::new(HashMap::new()) })
    })
}

/// The directory of the crate `file_path` belongs to (the longest manifest directory above it).
pub fn rust_crate_dir_of(file_path: &str, graph: &RustCrateGraph) -> Option<String> {
    if let Some(hit) = graph.file_crate.borrow().get(file_path) {
        return hit.clone();
    }
    let mut best: Option<String> = if graph.crates.contains_key("") { Some(String::new()) } else { None };
    for dir in graph.crates.keys() {
        if !dir.is_empty() && file_path.starts_with(&format!("{dir}/")) && best.as_ref().is_none_or(|b| dir.len() > b.len()) {
            best = Some(dir.clone());
        }
    }
    graph.file_crate.borrow_mut().insert(file_path.to_string(), best.clone());
    best
}

/// Whether a `use` rooted at any of `roots` can reach the crate `candidate` lives in.
pub fn rust_use_can_reach(roots: &HashSet<String>, candidate: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let graph = rust_crate_graph(ctx);
    let at = rust_crate_dir_of(&candidate.file_path, &graph);
    let own = rust_crate_dir_of(&r.file_path, &graph);
    let (Some(at), Some(own)) = (at, own) else { return true };
    let crate_ = &graph.crates[&own];
    for root in roots {
        let from = if root == "crate" || root == "self" || root == "super" || *root == crate_.name {
            Some(own.clone())
        } else {
            crate_.keys.get(root).cloned()
        };
        match from {
            None => return true,
            Some(from) => {
                if graph.reach.get(&from).is_some_and(|r| r.contains(&at)) {
                    return true;
                }
            }
        }
    }
    false
}

// -- `use` trees ---------------------------------------------------------------------------

struct UseBinding {
    segs: Vec<String>,
    local: Option<String>,
}

/// Every name a `use` tree binds with the path it names (`None` for a glob).
fn rust_use_tree_bindings(tree: &str) -> Vec<UseBinding> {
    let tokens: Vec<String> =
        re!(r"(?:r#)?\$?[A-Za-z_]\w*|::|[{},*]", "g").captures_iter(tree).map(|c| c.text(0).unwrap_or("").to_string()).collect();
    struct Reader<'a> {
        tokens: &'a [String],
        i: usize,
        out: Vec<UseBinding>,
    }
    impl Reader<'_> {
        fn tok(&self, i: usize) -> Option<&str> {
            self.tokens.get(i).map(String::as_str)
        }
        fn read(&mut self, prefix: &[String]) {
            let mut segs: Vec<String> = prefix.to_vec();
            while self.i < self.tokens.len() {
                let t = self.tokens[self.i].clone();
                if t == "::" {
                    self.i += 1;
                    continue;
                }
                if t == "*" {
                    self.i += 1;
                    self.out.push(UseBinding { segs, local: None });
                    return;
                }
                if t == "{" {
                    self.i += 1;
                    while self.i < self.tokens.len() && self.tokens[self.i] != "}" {
                        let before = self.i;
                        self.read(&segs);
                        if self.tok(self.i) == Some(",") || self.i == before {
                            self.i += 1;
                        }
                    }
                    self.i += 1;
                    return;
                }
                if t == "," || t == "}" || (t == "as" && segs.len() > prefix.len()) {
                    break;
                }
                segs.push(t.strip_prefix("r#").unwrap_or(&t).to_string());
                self.i += 1;
                if self.tok(self.i) != Some("::") {
                    break;
                }
            }
            if segs.len() == prefix.len() {
                return;
            }
            let mut local: Option<String> = None;
            if self.tok(self.i) == Some("as") {
                local = self.tok(self.i + 1).map(String::from);
                self.i += 2;
            }
            if segs.last().map(String::as_str) == Some("self") {
                segs.pop();
            }
            if local.is_none() {
                local = segs.last().cloned();
            }
            if let Some(local) = local {
                if !segs.is_empty() && local != "_" && re!(r"^[A-Za-z_]\w*$").is_match(&local) {
                    self.out.push(UseBinding { segs, local: Some(local) });
                }
            }
        }
    }
    let mut reader = Reader { tokens: &tokens, i: 0, out: Vec::new() };
    while reader.i < tokens.len() {
        let before = reader.i;
        reader.read(&[]);
        if reader.i == before {
            reader.i += 1;
        }
    }
    reader.out
}

/// The offsets of every `{` and `}` outside strings, raw strings and char literals.
fn rust_code_braces(text: &str) -> Vec<usize> {
    let scan = re!(r#"[{}]|\b(?:b?r(#*)")|"|'"#, "g");
    let mut braces: Vec<usize> = Vec::new();
    let mut pos = 0usize;
    while let Some(m) = scan.captures_at(text, pos) {
        let whole = m.get(0).expect("group 0");
        let at = whole.start();
        let c = whole.as_str();
        pos = whole.end();
        if c == "{" || c == "}" {
            braces.push(at);
        } else if c == "'" {
            let window: String = text[at..].chars().take(12).collect();
            if let Some(lit) =
                re!(r"^'(?:\\(?:u\{[0-9A-Fa-f]{1,6}\}|x[0-9A-Fa-f]{2}|.)|[^\\'\n])'").find(&window)
            {
                pos = at + lit.end();
            }
        } else if c == "\"" {
            let bytes = text.as_bytes();
            let mut end = at;
            let found: Option<usize> = loop {
                match text[end + 1..].find('"') {
                    None => break None,
                    Some(rel) => {
                        end = end + 1 + rel;
                        let mut slashes = 0usize;
                        while end > slashes && bytes[end - 1 - slashes] == b'\\' {
                            slashes += 1;
                        }
                        if slashes % 2 == 0 {
                            break Some(end);
                        }
                    }
                }
            };
            pos = found.map_or(text.len(), |e| e + 1);
        } else {
            let hashes = m.text(1).unwrap_or("");
            let needle = format!("\"{hashes}");
            let from = at + c.len();
            pos = match text.get(from..).and_then(|t| t.find(&needle)) {
                Some(rel) => from + rel + 1 + hashes.len(),
                None => text.len(),
            };
        }
        if pos > text.len() {
            break;
        }
    }
    braces
}

fn rust_mod_blocks(text: &str) -> (Vec<RustModBlock>, bool) {
    let mut heads: HashMap<usize, String> = HashMap::new();
    for m in re!(r"\bmod\s+(?:r#)?([A-Za-z_]\w*)\s*\{", "g").captures_iter(text) {
        let whole = m.get(0).expect("group 0");
        heads.insert(whole.end() - 1, m.text(1).unwrap_or("").to_string());
    }
    let mut blocks: Vec<RustModBlock> = Vec::new();
    if heads.is_empty() {
        return (blocks, true);
    }
    let mut open: Vec<Option<usize>> = Vec::new();
    let mut inner: Option<usize> = None;
    let mut balanced = true;
    for at in rust_code_braces(text) {
        if text.as_bytes()[at] == b'{' {
            let block = heads.get(&at).map(|name| {
                blocks.push(RustModBlock { name: name.clone(), open: at, close: text.len(), parent: inner });
                inner = Some(blocks.len() - 1);
                blocks.len() - 1
            });
            open.push(block);
            continue;
        }
        let Some(closed) = open.pop() else {
            balanced = false;
            continue;
        };
        if let Some(c) = closed {
            blocks[c].close = at;
            inner = blocks[c].parent;
        }
    }
    let balanced = balanced && open.is_empty();
    (blocks, balanced)
}

/// The innermost block containing `offset`.
pub fn rust_block_at(blocks: &[RustModBlock], offset: usize) -> Option<usize> {
    let mut at = None;
    for (i, b) in blocks.iter().enumerate() {
        if b.open > offset {
            break;
        }
        if offset < b.close {
            at = Some(i);
        }
    }
    at
}

fn tail_units(s: &str, end: usize, units: usize) -> &str {
    let mut start = end;
    let mut seen = 0usize;
    for c in s[..end].chars().rev() {
        if seen >= units {
            break;
        }
        seen += c.len_utf16();
        start -= c.len_utf8();
    }
    &s[start..end]
}

fn use_leaves(tree: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for leaf in re!(r"([A-Za-z_]\w*)\s*(?=[,}]|$|\s+as\b)|\bas\s+([A-Za-z_]\w*)", "g").captures_iter(tree) {
        let id = leaf.text(2).or(leaf.text(1)).unwrap_or("");
        if id != "self" && id != "as" {
            out.push(id.to_string());
        }
    }
    for m in re!(r"([A-Za-z_]\w*)\s*::\s*\{[^{}]*\bself\b", "g").captures_iter(tree) {
        out.push(m.text(1).unwrap_or("").to_string());
    }
    out
}

memo_map!(RustUsesMemo, String, Rc<RustUses>);

/// What `file_path` imports and how its inline modules nest.
pub fn rust_uses_of(file_path: &str, ctx: &Ctx) -> Rc<RustUses> {
    memoized::<RustUsesMemo>(ctx, file_path.to_string(), || {
        let source = ctx.read_file(file_path).map(|t| t.to_string()).unwrap_or_default();
        let text = strip_comments_for_regex(&source, "rust");
        let dependencies = rust_dependency_crates(ctx);
        let mut uses = RustUses {
            names: HashSet::new(),
            globs: HashSet::new(),
            external: HashSet::new(),
            bound: HashSet::new(),
            bound_from: HashMap::new(),
            paths: HashMap::new(),
            glob_paths: Vec::new(),
            blocks: Vec::new(),
            path_mods: HashSet::new(),
            balanced: true,
            line_starts: Vec::new(),
            text: String::new(),
        };
        for m in re!(r"(?:^|[;{}\s])use\s+([^;]{1,2000});", "g").captures_iter(&text) {
            let whole = m.get(0).expect("group 0");
            let tree = m.text(1).unwrap_or("");
            let root = re!(r"^\s*(?:::)?([A-Za-z_]\w*)").captures(tree).and_then(|c| c.text(1).map(String::from)).unwrap_or_default();
            let outside = is_rust_std_root_use(&root) || (!root.is_empty() && dependencies.contains(&root));
            let kw = whole.start() + whole.as_str().find("use").unwrap_or(0);
            let pub_ = re!(r"\bpub(?:\s*\([^()]*\))?\s*$").is_match(tail_units(&text, kw, 48));
            for b in rust_use_tree_bindings(tree) {
                if b.segs.is_empty() || b.segs[0].starts_with('$') || is_rust_std_root(&b.segs[0]) {
                    continue;
                }
                let path = RustUsePath { segs: b.segs, at: whole.start(), pub_ };
                match b.local {
                    None => uses.glob_paths.push(path),
                    Some(local) => uses.paths.entry(local).or_default().push(path),
                }
            }
            if outside {
                for id in use_leaves(tree) {
                    uses.external.insert(id);
                }
                continue;
            }
            for id in use_leaves(tree) {
                uses.bound.insert(id.clone());
                uses.bound_from.entry(id).or_default().insert(root.clone());
            }
            for id in re!(r"[A-Za-z_]\w*", "g").captures_iter(tree) {
                uses.names.insert(id.text(0).unwrap_or("").to_string());
            }
            for g in re!(r"(\w+)\s*::\s*(?:\{[^}]*)?\*", "g").captures_iter(tree) {
                uses.globs.insert(g.text(1).unwrap_or("").to_string());
            }
        }
        let (blocks, balanced) = rust_mod_blocks(&text);
        uses.blocks = blocks;
        uses.balanced = balanced;
        for m in re!(
            r"#\[\s*(?:cfg_attr\s*\([^\]]*\bpath|path)\s*=[^\]]*\]\s*(?:#!?\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+(?:r#)?([A-Za-z_]\w*)",
            "g"
        )
        .captures_iter(&text)
        {
            uses.path_mods.insert(m.text(1).unwrap_or("").to_string());
        }
        uses.line_starts.push(0);
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                uses.line_starts.push(i + 1);
            }
        }
        uses.text = text;
        Rc::new(uses)
    })
}

fn is_rust_std_root_use(root: &str) -> bool {
    root == "std" || root == "core" || root == "alloc"
}

// -- Modules -------------------------------------------------------------------------------

/// A module: a file (or an inline block of one) and the directory its children live in.
#[derive(Debug, Clone)]
pub struct RustModule {
    pub file: String,
    pub dir: String,
    pub block: Option<usize>,
    pub root: bool,
    pub alt: Option<String>,
}

pub fn rust_join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

pub fn rust_parent_dir(p: &str) -> String {
    match p.rfind('/') {
        Some(i) => p[..i].to_string(),
        None => String::new(),
    }
}

fn is_rust_target_root(file_path: &str, ctx: &Ctx) -> bool {
    let graph = rust_crate_graph(ctx);
    let Some(dir) = rust_crate_dir_of(file_path, &graph) else {
        return re!(r"(?:^|/)(?:lib|main)\.rs$").is_match(file_path);
    };
    let rel = if dir.is_empty() { file_path } else { file_path.get(dir.len() + 1..).unwrap_or("") };
    re!(r"^(?:src/(?:lib|main)\.rs|src/bin/[^/]+(?:\.rs|/main\.rs)|(?:tests|examples|benches)/[^/]+(?:\.rs|/main\.rs)|build\.rs)$").is_match(rel)
}

pub fn rust_file_module(file_path: &str, ctx: &Ctx) -> RustModule {
    let root = is_rust_target_root(file_path, ctx);
    let dir = if root || file_path.ends_with("/mod.rs") || file_path == "mod.rs" {
        rust_parent_dir(file_path)
    } else {
        re!(r"\.rs$").replace_first(file_path, |_| String::new())
    };
    RustModule { file: file_path.to_string(), dir, block: None, root, alt: None }
}

pub fn rust_module_at(file_path: &str, offset: usize, ctx: &Ctx) -> RustModule {
    let own = rust_file_module(file_path, ctx);
    let uses = rust_uses_of(file_path, ctx);
    let Some(block) = rust_block_at(&uses.blocks, offset) else { return own };
    let mut names: Vec<&str> = Vec::new();
    let mut b = Some(block);
    while let Some(i) = b {
        names.push(&uses.blocks[i].name);
        b = uses.blocks[i].parent;
    }
    let mut dir = own.dir;
    for name in names.iter().rev() {
        dir = rust_join(&dir, name);
    }
    RustModule { file: file_path.to_string(), dir, block: Some(block), root: false, alt: None }
}

fn rust_crate_root_module(file_path: &str, ctx: &Ctx) -> Option<RustModule> {
    let graph = rust_crate_graph(ctx);
    let dir = rust_crate_dir_of(file_path, &graph)?;
    let base = if dir.is_empty() { String::new() } else { format!("{dir}/") };
    let rel = file_path.get(base.len()..).unwrap_or("");
    if let Some(t) = re!(r"^(src/bin|tests|examples|benches)/([^/]+?)(\.rs$|/)").captures(rel) {
        let file = if t.text(3) == Some(".rs") {
            file_path.to_string()
        } else {
            format!("{base}{}/{}/main.rs", t.text(1).unwrap_or(""), t.text(2).unwrap_or(""))
        };
        return if ctx.file_exists(&file) {
            Some(RustModule { dir: rust_parent_dir(&file), file, block: None, root: true, alt: None })
        } else {
            None
        };
    }
    if !rel.starts_with("src/") {
        return None;
    }
    let lib = format!("{base}src/lib.rs");
    let main = format!("{base}src/main.rs");
    let has_lib = ctx.file_exists(&lib);
    let has_main = ctx.file_exists(&main);
    if !has_lib && !has_main {
        return None;
    }
    let file = if has_main && (file_path == main || !has_lib) { main.clone() } else { lib.clone() };
    let alt = if has_lib && has_main { Some(if file == lib { main } else { lib }) } else { None };
    Some(RustModule { file, dir: format!("{base}src"), block: None, root: true, alt })
}

fn rust_lib_module(crate_dir: &str, ctx: &Ctx) -> Option<RustModule> {
    let file = if crate_dir.is_empty() { "src/lib.rs".to_string() } else { format!("{crate_dir}/src/lib.rs") };
    if ctx.file_exists(&file) {
        Some(RustModule { dir: rust_parent_dir(&file), file, block: None, root: true, alt: None })
    } else {
        None
    }
}

/// The result of following a `use` path through the module tree.
pub enum Walked {
    Module(RustModule),
    /// std, core, alloc or a dependency crate.
    Outside,
    Unknown,
}

fn rust_submodule(m: &RustModule, name: &str, ctx: &Ctx, depth: u32) -> Option<RustModule> {
    let dir = rust_join(&m.dir, name);
    let layout = rust_uses_of(&m.file, ctx);
    if layout.path_mods.contains(name) {
        return None;
    }
    if let Some(block) = layout.blocks.iter().position(|b| b.name == name && b.parent == m.block) {
        return Some(RustModule { file: m.file.clone(), dir, block: Some(block), root: false, alt: None });
    }
    for file in [format!("{dir}.rs"), format!("{dir}/mod.rs")] {
        if ctx.file_exists(&file) {
            return Some(RustModule { file, dir, block: None, root: false, alt: None });
        }
    }
    if depth >= 8 {
        return None;
    }
    for p in layout.paths.get(name).map(Vec::as_slice).unwrap_or(&[]) {
        if rust_block_at(&layout.blocks, p.at) != m.block {
            continue;
        }
        if let Walked::Module(passed) = rust_walk_modules(&p.segs, m, ctx, depth + 1) {
            return Some(passed);
        }
    }
    None
}

fn rust_super_module(m: &RustModule, ctx: &Ctx) -> Option<RustModule> {
    if let Some(block) = m.block {
        let layout = rust_uses_of(&m.file, ctx);
        return match layout.blocks[block].parent {
            Some(parent) => Some(RustModule {
                file: m.file.clone(),
                dir: rust_parent_dir(&m.dir),
                block: Some(parent),
                root: false,
                alt: None,
            }),
            None => Some(rust_file_module(&m.file, ctx)),
        };
    }
    if m.root {
        return None;
    }
    let dir = rust_parent_dir(&m.dir);
    let mut files = vec![rust_join(&dir, "mod.rs")];
    if !dir.is_empty() {
        files.push(format!("{dir}.rs"));
    }
    files.push(rust_join(&dir, "lib.rs"));
    files.push(rust_join(&dir, "main.rs"));
    for file in files {
        if ctx.file_exists(&file) {
            return Some(rust_file_module(&file, ctx));
        }
    }
    None
}

/// Follow `segs` (`crate`, `self`, `super`, a child module, a dependency crate…) from `from`.
pub fn rust_walk_modules(segs: &[String], from: &RustModule, ctx: &Ctx, depth: u32) -> Walked {
    let Some(first) = segs.first() else { return Walked::Module(from.clone()) };
    let mut at: Option<RustModule>;
    if first == "crate" {
        at = rust_crate_root_module(&from.file, ctx);
    } else if first == "self" {
        at = Some(from.clone());
    } else if first == "super" {
        at = rust_super_module(from, ctx);
    } else if is_rust_std_root(first) {
        return Walked::Outside;
    } else {
        at = rust_submodule(from, first, ctx, depth);
        if at.is_none() {
            let graph = rust_crate_graph(ctx);
            let own = rust_crate_dir_of(&from.file, &graph);
            let dep: Option<String> = own.as_ref().and_then(|own| {
                let crate_ = graph.crates.get(own)?;
                if *first == crate_.name {
                    Some(own.clone())
                } else {
                    crate_.keys.get(first).cloned()
                }
            });
            if let Some(dep) = dep {
                at = rust_lib_module(&dep, ctx);
            } else if rust_dependency_crates(ctx).contains(first) {
                return Walked::Outside;
            }
        }
    }
    for seg in &segs[1..] {
        let Some(cur) = at else { return Walked::Unknown };
        at = if seg == "self" {
            Some(cur)
        } else if seg == "super" {
            rust_super_module(&cur, ctx)
        } else {
            rust_submodule(&cur, seg, ctx, depth)
        };
    }
    match at {
        Some(m) => Walked::Module(m),
        None => Walked::Unknown,
    }
}

pub fn is_rust_module_item_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Function
            | NodeKind::Struct
            | NodeKind::Enum
            | NodeKind::Union
            | NodeKind::TypeAlias
            | NodeKind::Trait
            | NodeKind::Constant
            | NodeKind::Variable
    )
}

fn rust_declared_items(m: &RustModule, name: &str, ctx: &Ctx, public_only: bool) -> Vec<Rc<Node>> {
    let mut out: Vec<Rc<Node>> = Vec::new();
    let files: Vec<&String> = match &m.alt {
        Some(alt) => vec![&m.file, alt],
        None => vec![&m.file],
    };
    for file in files {
        let layout = rust_uses_of(file, ctx);
        let block = if *file == m.file { m.block } else { None };
        for n in ctx.get_nodes_in_file_named(file, name) {
            if n.language.as_str() != "rust" || n.qualified_name != name || !is_rust_module_item_kind(n.kind) {
                continue;
            }
            if public_only && n.visibility == Some(Visibility::Private) {
                continue;
            }
            let at = layout.offset_of(n.start_line, n.start_column);
            if rust_block_at(&layout.blocks, at) == block {
                out.push(n);
            }
        }
    }
    out
}

fn rust_module_items(m: &RustModule, name: &str, ctx: &Ctx, seen: &mut HashSet<String>, public_only: bool) -> Option<Vec<Rc<Node>>> {
    let uses = rust_uses_of(&m.file, ctx);
    let open = m.block.map(|b| uses.blocks[b].open as i64).unwrap_or(-1);
    let key = format!("{}\0{}\0{}\0{}", m.file, open, name, if public_only { 1 } else { 0 });
    if seen.contains(&key) {
        return Some(Vec::new());
    }
    if seen.len() >= 32 {
        return None;
    }
    if !uses.balanced {
        return None;
    }
    seen.insert(key.clone());
    let result = (|| {
        let declared = rust_declared_items(m, name, ctx, public_only);
        if !declared.is_empty() {
            return Some(declared);
        }
        let here = |p: &&RustUsePath| (p.pub_ || !public_only) && rust_block_at(&uses.blocks, p.at) == m.block;
        let named: Vec<&RustUsePath> = uses.paths.get(name).map(|v| v.iter().filter(here).collect()).unwrap_or_default();
        let mut found: Vec<Rc<Node>> = Vec::new();
        for p in named {
            found.extend(rust_use_path_items(&p.segs, m, ctx, seen)?);
        }
        if !found.is_empty() {
            return Some(found);
        }
        let globs: Vec<&RustUsePath> = uses.glob_paths.iter().filter(here).collect();
        let globbed = rust_globbed_items(&globs, &|_| m.clone(), name, ctx, seen)?;
        found.extend(globbed);
        Some(found)
    })();
    seen.remove(&key);
    result
}

fn rust_globbed_items(
    globs: &[&RustUsePath],
    from_of: &dyn Fn(&RustUsePath) -> RustModule,
    name: &str,
    ctx: &Ctx,
    seen: &mut HashSet<String>,
) -> Option<Vec<Rc<Node>>> {
    let mut found: Vec<Rc<Node>> = Vec::new();
    let mut unknown = false;
    for g in globs {
        let from = from_of(g);
        let items = match rust_walk_modules(&g.segs, &from, ctx, 0) {
            Walked::Outside => continue,
            Walked::Unknown => None,
            Walked::Module(module) => rust_module_items(&module, name, ctx, seen, !rust_module_within(&from, &module, ctx)),
        };
        match items {
            None => unknown = true,
            Some(items) => found.extend(items),
        }
    }
    if !found.is_empty() || !unknown {
        Some(found)
    } else {
        None
    }
}

fn rust_module_within(inner: &RustModule, outer: &RustModule, ctx: &Ctx) -> bool {
    if inner.file == outer.file {
        let layout = rust_uses_of(&inner.file, ctx);
        let mut b = inner.block;
        loop {
            if b == outer.block {
                return true;
            }
            match b {
                None => return false,
                Some(i) => b = layout.blocks[i].parent,
            }
        }
    }
    let prefix = if outer.dir.is_empty() { String::new() } else { format!("{}/", outer.dir) };
    inner.file.starts_with(&prefix) && !(outer.dir.is_empty() && outer.block.is_some())
}

pub fn rust_use_path_items(segs: &[String], from: &RustModule, ctx: &Ctx, seen: &mut HashSet<String>) -> Option<Vec<Rc<Node>>> {
    if segs.len() < 2 {
        return None;
    }
    match rust_walk_modules(&segs[..segs.len() - 1], from, ctx, 0) {
        Walked::Outside => Some(Vec::new()),
        Walked::Unknown => None,
        Walked::Module(m) => rust_module_items(&m, &segs[segs.len() - 1], ctx, seen, false),
    }
}

memo_map!(RustUseItemsMemo, String, Option<Rc<HashSet<String>>>);
memo_map!(RustGlobItemsMemo, String, Option<Rc<HashSet<String>>>);

/// The ids of the items the file's `use`s of `name` import, or `None` when that can't be known.
pub fn rust_use_items(name: &str, file_path: &str, ctx: &Ctx) -> Option<Rc<HashSet<String>>> {
    memoized::<RustUseItemsMemo>(ctx, format!("{file_path}\0{name}"), || {
        let uses = rust_uses_of(file_path, ctx);
        let paths = if uses.balanced { uses.paths.get(name) } else { None };
        let paths = paths?;
        let mut ids: HashSet<String> = HashSet::new();
        for p in paths {
            let items = rust_use_path_items(&p.segs, &rust_module_at(file_path, p.at, ctx), ctx, &mut HashSet::new())?;
            for n in items {
                ids.insert(n.id.clone());
            }
        }
        Some(Rc::new(ids))
    })
}

/// The ids of the items the file's globs bring in under `name`, or `None` when unknown.
pub fn rust_glob_items(name: &str, file_path: &str, ctx: &Ctx) -> Option<Rc<HashSet<String>>> {
    memoized::<RustGlobItemsMemo>(ctx, format!("{file_path}\0{name}"), || {
        let uses = rust_uses_of(file_path, ctx);
        if !(uses.balanced && !uses.glob_paths.is_empty()) {
            return None;
        }
        let globs: Vec<&RustUsePath> = uses.glob_paths.iter().collect();
        let items = rust_globbed_items(&globs, &|g| rust_module_at(file_path, g.at, ctx), name, ctx, &mut HashSet::new())?;
        Some(Rc::new(items.iter().map(|n| n.id.clone()).collect()))
    })
}

/// Whether a glob import brings `candidate` in under `name`.
pub fn rust_glob_scope(name: &str, candidate: &Node, uses: &RustUses, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let ids = if is_rust_module_item_kind(candidate.kind) { rust_glob_items(name, &r.file_path, ctx) } else { None };
    match ids {
        None => rust_glob_covers(uses, candidate, r),
        Some(ids) => ids.contains(&candidate.id),
    }
}

/// `foo/bar.rs` → `bar`; `foo/mod.rs`, `lib.rs`, `main.rs` → the directory.
pub fn rust_module_name(file_path: &str) -> String {
    let parts: Vec<&str> = file_path.split('/').collect();
    let last = parts[parts.len() - 1];
    let base = last.strip_suffix(".rs").unwrap_or(last);
    if base == "mod" || base == "lib" || base == "main" {
        if parts.len() >= 2 {
            parts[parts.len() - 2].to_string()
        } else {
            base.to_string()
        }
    } else {
        base.to_string()
    }
}

/// The directory a file's module owns: the directory itself for `mod.rs`/`lib.rs`/`main.rs`, else
/// the sibling directory named after the file.
pub fn rust_module_dir(file_path: &str) -> String {
    let base = posix::basename(file_path);
    let dir = posix::dirname(file_path);
    if base == "mod.rs" || base == "lib.rs" || base == "main.rs" {
        return dir;
    }
    posix::join(&[&dir, &re!(r"\.rs$").replace_first(&base, |_| String::new())])
}

/// Whether `candidate` is visible from `r`'s file (a private item is visible from its module and below).
pub fn is_rust_item_visible_from(candidate: &Node, r: &UnresolvedRef) -> bool {
    if candidate.language.as_str() != "rust"
        || candidate.visibility != Some(Visibility::Private)
        || !is_rust_module_item_kind(candidate.kind)
    {
        return true;
    }
    candidate.file_path == r.file_path || r.file_path.starts_with(&format!("{}/", rust_module_dir(&candidate.file_path)))
}

pub fn is_rust_associated_item(n: &Node) -> bool {
    n.language.as_str() == "rust"
        && matches!(n.kind, NodeKind::TypeAlias | NodeKind::Variable | NodeKind::Constant)
        && n.qualified_name.contains("::")
}

/// Whether a `use …::*` of the candidate's module (or of `super`) reaches it.
pub fn rust_glob_covers(uses: &RustUses, candidate: &Node, r: &UnresolvedRef) -> bool {
    if uses.globs.contains(&rust_module_name(&candidate.file_path)) {
        return true;
    }
    if !uses.globs.contains("super") {
        return false;
    }
    fn dir(p: &str) -> &str {
        match p.rfind('/') {
            Some(i) => &p[..i],
            None => "",
        }
    }
    let cd = dir(&candidate.file_path);
    cd == dir(&r.file_path) || cd == dir(dir(&r.file_path))
}

/// Whether `candidate` sits in an inline `mod name { … }` of its own file.
pub fn is_in_rust_inline_module(candidate: &Node, name: &str, ctx: &Ctx) -> bool {
    let layout = rust_uses_of(&candidate.file_path, ctx);
    if !layout.balanced {
        return false;
    }
    let at = layout.offset_of(candidate.start_line, candidate.start_column);
    rust_block_at(&layout.blocks, at).is_some_and(|b| layout.blocks[b].name == name)
}

#[allow(dead_code)]
fn _keep(_: &Re) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn bindings(tree: &str) -> Vec<(Vec<String>, Option<String>)> {
        rust_use_tree_bindings(tree).into_iter().map(|b| (b.segs, b.local)).collect()
    }

    #[test]
    fn a_use_tree_binds_each_leaf_with_its_path() {
        let got = bindings("crate::a::{b, c as d, e::{self, f}, *}");
        let paths: Vec<String> = got.iter().map(|(s, l)| format!("{}={}", s.join("::"), l.clone().unwrap_or("*".into()))).collect();
        assert_eq!(paths, ["crate::a::b=b", "crate::a::c=d", "crate::a::e=e", "crate::a::e::f=f", "crate::a=*"]);
    }

    #[test]
    fn braces_inside_strings_and_chars_are_skipped() {
        let text = "fn a() { let s = \"{\"; let c = '}'; let r = r#\"}\"#; }";
        let braces = rust_code_braces(text);
        assert_eq!(braces.len(), 2);
    }

    #[test]
    fn mod_blocks_nest() {
        let (blocks, balanced) = rust_mod_blocks("mod a { mod b { fn x() {} } }");
        assert!(balanced);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1].parent, Some(0));
    }

    #[test]
    fn manifest_dependencies_read_every_table_form() {
        let deps = rust_manifest_dependencies(
            "[dependencies]\nserde = \"1\"\nfoo-bar = { path = \"../foo\", package = \"foo-pkg\" }\n[dev-dependencies.tempfile]\nversion = \"3\"\n",
        );
        let keys: Vec<(String, DepKind)> = deps.iter().map(|d| (d.key.clone(), d.kind)).collect();
        assert_eq!(
            keys,
            [("serde".to_string(), DepKind::Normal), ("foo_bar".to_string(), DepKind::Normal), ("tempfile".to_string(), DepKind::Dev)]
        );
        assert_eq!(deps[1].path.as_deref(), Some("../foo"));
        assert_eq!(deps[1].pkg, "foo_pkg");
    }
}
