//! Reading the types a Rust file writes down: local bindings, return types, struct fields,
//! iterator items, and where a written type names a project item.
//!
//! Offsets into a file are UTF-16 indices of its `RustScopes::code` (one byte per unit).

use super::impls::*;
use super::uses::*;
use crate::ctx::Ctx;
use crate::name_matcher::util::{lines_of, memo_map, memoized, Bounded};
use crate::rust_scopes::*;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::{is_js_space, js_trim};
use codegraph_types::nodepath::posix;
use codegraph_types::{EdgeKind, Language, Node, NodeKind, ReferenceKind};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

pub const RUST_DEREF_WRAPPERS: [&str; 11] =
    ["Box", "Rc", "Arc", "Pin", "Cow", "MutexGuard", "RwLockReadGuard", "RwLockWriteGuard", "Ref", "RefMut", "ManuallyDrop"];
pub const RUST_SELF_WRAPPERS: [&str; 4] = ["Pin", "Box", "Rc", "Arc"];
pub const RUST_MARKER_TRAITS: [&str; 7] = ["Send", "Sync", "Unpin", "Sized", "Copy", "UnwindSafe", "RefUnwindSafe"];
pub const RUST_PRIMITIVE_TYPES: [&str; 17] =
    ["bool", "char", "str", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize", "f32", "f64"];

pub fn is_rust_marker_trait(name: &str) -> bool {
    RUST_MARKER_TRAITS.contains(&name)
}

pub fn is_rust_primitive(name: &str) -> bool {
    RUST_PRIMITIVE_TYPES.contains(&name)
}

fn strip(r: &Re, s: &str) -> String {
    r.replace_first(s, |_| String::new())
}

fn trim(s: &str) -> &str {
    js_trim(s)
}

/// A `UnresolvedRef` that only knows the file it sits in.
pub fn file_ref(file: &str) -> UnresolvedRef {
    UnresolvedRef::new("", "", ReferenceKind::Edge(EdgeKind::References), 0, 0, file, Language::Rust)
}

// -- Scopes --------------------------------------------------------------------------------

pub fn rust_scopes_of(file_path: &str, ctx: &Ctx) -> Option<Rc<RustScopes>> {
    let memo = ctx.memo::<Bounded<Option<Rc<RustScopes>>, 64>>();
    memo.get_or(file_path, true, || ctx.read_file(file_path).map(|source| Rc::new(read_rust_scopes(&source))))
}

/// The 1-based line of `offset`.
pub fn rust_line_at(scopes: &RustScopes, offset: usize) -> u32 {
    if scopes.line_starts.is_empty() {
        return 1;
    }
    let mut lo = 0usize;
    let mut hi = scopes.line_starts.len() - 1;
    while lo < hi {
        let mid = (lo + hi + 1) >> 1;
        if scopes.line_starts[mid] <= offset {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo as u32 + 1
}

/// `scopes.code.slice(from, to)`.
pub fn code_slice(scopes: &RustScopes, from: usize, to: usize) -> &str {
    let to = to.min(scopes.code.len());
    if from >= to {
        ""
    } else {
        &scopes.code[from..to]
    }
}

pub fn rust_ref_offset(scopes: &RustScopes, r: &UnresolvedRef) -> usize {
    let start = (r.line as usize).checked_sub(1).and_then(|i| scopes.line_starts.get(i)).copied().unwrap_or(0);
    start + r.column as usize
}

pub fn rust_node_offset(n: &Node, ctx: &Ctx) -> usize {
    let scopes = rust_scopes_of(&n.file_path, ctx);
    let start = scopes
        .and_then(|s| (n.start_line as usize).checked_sub(1).and_then(|i| s.line_starts.get(i).copied()))
        .unwrap_or(0);
    start + n.start_column as usize
}

// -- Written types -------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RustBound {
    pub traits: Vec<String>,
    pub file: String,
    pub at: usize,
}

/// A type as written, with the file and offset it is written at (so its names resolve there).
#[derive(Debug, Clone, Default)]
pub struct RustWrittenType {
    pub text: String,
    pub file: String,
    pub at: usize,
    /// The type the declaring struct/enum is (for `Self` in a field type).
    pub self_: Option<String>,
    pub generics: Option<String>,
    pub wrappers: Option<Vec<String>>,
    pub opaque: Option<Vec<String>>,
    pub bounds: Option<Rc<HashMap<String, RustBound>>>,
}

impl RustWrittenType {
    pub fn plain(text: impl Into<String>, file: impl Into<String>, at: usize) -> RustWrittenType {
        RustWrittenType { text: text.into(), file: file.into(), at, ..Default::default() }
    }
}

#[derive(Debug, Clone)]
pub enum RustReadKind {
    Named { name: String, path: Vec<String>, args: String },
    Traits(Vec<String>),
    Outside(String),
}

#[derive(Debug, Clone)]
pub struct RustReadBody {
    pub kind: RustReadKind,
    pub wrappers: Vec<String>,
    pub by_ref: bool,
    pub written: RustWrittenType,
}

#[derive(Debug, Clone)]
pub enum RustTypeRead {
    Unread,
    Read(RustReadBody),
}

/// The index of the `>` closing the `<` at `open`.
pub fn rust_angle_end(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut i = open;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'<' {
            depth += 1;
        } else if c == b'>' && (i == 0 || bytes[i - 1] != b'-') {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

#[derive(Debug, Clone)]
pub struct RustPathHead {
    pub segments: Vec<String>,
    pub name: String,
    pub args: String,
}

/// `a::b::Name<args>`: its path, name and the text between the angle brackets.
pub fn rust_path_head(text: &str) -> Option<RustPathHead> {
    let mut t = strip(re!(r"^::\s*"), trim(text));
    let mut args = String::new();
    if let Some(open) = t.find('<') {
        if rust_angle_end(&t, open) != Some(t.len() - 1) {
            return None;
        }
        args = trim(&t[open + 1..t.len() - 1]).to_string();
        t = strip(re!(r"\s*::\s*$"), trim(&t[..open]));
    }
    let mut parts: Vec<String> = t.split("::").map(|s| trim(s).to_string()).collect();
    if !parts.iter().all(|p| re!(r"^[A-Za-z_]\w*$").is_match(p)) {
        return None;
    }
    let name = parts.pop()?;
    Some(RustPathHead { segments: parts, name, args })
}

pub fn rust_group_is_whole(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    for (i, &c) in bytes.iter().enumerate() {
        if matches!(c, b'(' | b'[' | b'{') {
            depth += 1;
        } else if matches!(c, b')' | b']' | b'}') {
            depth -= 1;
            if depth == 0 {
                return i == bytes.len() - 1;
            }
        }
    }
    false
}

/// `callee(args)` at the end of `value`.
pub fn rust_trailing_call(value: &str) -> Option<(String, String)> {
    if !value.ends_with(')') {
        return None;
    }
    let bytes = value.as_bytes();
    let mut depth = 0i32;
    for i in (0..bytes.len()).rev() {
        let c = bytes[i];
        if matches!(c, b')' | b']' | b'}') {
            depth += 1;
        } else if matches!(c, b'(' | b'[' | b'{') {
            depth -= 1;
            if depth == 0 {
                if c != b'(' {
                    return None;
                }
                return Some((trim(&value[..i]).to_string(), value[i + 1..value.len() - 1].to_string()));
            }
        }
    }
    None
}

/// The return type of a fn signature `(params) -> Type`.
pub fn rust_return_text(signature: Option<&str>) -> Option<String> {
    let signature = signature.filter(|s| s.starts_with('('))?;
    let bytes = signature.as_bytes();
    let mut depth = 0i32;
    for (i, &c) in bytes.iter().enumerate() {
        if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
            if depth == 0 {
                let ret = re!(r"^\s*->\s*([\s\S]+)$").captures(&signature[i + 1..]).and_then(|c| c.text(1).map(|s| trim(s).to_string()));
                return ret.filter(|r| !r.is_empty());
            }
        }
    }
    None
}

/// `Result<T, E>` / `Option<T>` peeled `times` times to their first argument.
pub fn rust_unwrapped(written: &RustWrittenType, times: u32) -> Option<RustWrittenType> {
    let mut text = written.text.clone();
    for _ in 0..times {
        let head = rust_path_head(&text)?;
        if head.name != "Result" && head.name != "Option" {
            return None;
        }
        let first = split_rust_top_level(&head.args, ',').into_iter().next()?;
        text = first;
    }
    Some(RustWrittenType { text, ..written.clone() })
}

// -- `use` bindings ------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RustUseBinding {
    pub path: Vec<String>,
    pub from: i64,
    pub to: usize,
}

memo_map!(UseBindingsMemo, String, Rc<HashMap<String, Vec<RustUseBinding>>>);

fn expand_rust_use(tree: &str, prefix: &[String], bound: &mut HashMap<String, Vec<String>>) {
    let t = strip(re!(r"^::\s*"), trim(tree));
    if t.is_empty() {
        return;
    }
    if let Some(open) = t.find('{') {
        if t.ends_with('}') {
            let head = trim(&strip(re!(r"::\s*$"), &t[..open])).to_string();
            let mut within: Vec<String> = prefix.to_vec();
            if !head.is_empty() {
                within.extend(head.split("::").map(|s| trim(s).to_string()));
            }
            let bytes = t.as_bytes();
            let mut depth = 0i32;
            let mut start = open + 1;
            for i in open + 1..t.len() - 1 {
                let c = bytes[i];
                if c == b'{' {
                    depth += 1;
                } else if c == b'}' {
                    depth -= 1;
                } else if c == b',' && depth == 0 {
                    expand_rust_use(&t[start..i], &within, bound);
                    start = i + 1;
                }
            }
            expand_rust_use(&t[start..t.len() - 1], &within, bound);
            return;
        }
    }
    let Some(m) = re!(r"^((?:[A-Za-z_]\w*\s*::\s*)*)([A-Za-z_]\w*|\*)(?:\s+as\s+([A-Za-z_]\w*))?$").captures(&t) else { return };
    let last = m.text(2).unwrap_or("");
    let alias = m.text(3);
    if last == "*" || alias == Some("_") {
        return;
    }
    let mut segments: Vec<String> = prefix.to_vec();
    if let Some(lead) = m.text(1).filter(|s| !s.is_empty()) {
        segments.extend(lead.split("::").map(|s| trim(s).to_string()).filter(|s| !s.is_empty()));
    }
    if last == "self" {
        let name = alias.map(String::from).or_else(|| segments.last().cloned());
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            bound.insert(name, segments);
        }
        return;
    }
    let mut path = segments;
    path.push(last.to_string());
    bound.insert(alias.unwrap_or(last).to_string(), path);
}

fn rust_use_bindings(file_path: &str, ctx: &Ctx) -> Rc<HashMap<String, Vec<RustUseBinding>>> {
    memoized::<UseBindingsMemo>(ctx, file_path.to_string(), || {
        let mut bindings: HashMap<String, Vec<RustUseBinding>> = HashMap::new();
        if let Some(scopes) = rust_scopes_of(file_path, ctx) {
            for m in re!(r"(?:^|[;{}\s])use\s+([^;]{1,2000});", "g").captures_iter(&scopes.code) {
                let mut bound: HashMap<String, Vec<String>> = HashMap::new();
                expand_rust_use(m.text(1).unwrap_or(""), &[], &mut bound);
                let at = m.get(0).expect("group 0").start();
                let block = rust_inner_block_at(&scopes, at);
                for (name, path) in bound {
                    bindings.entry(name).or_default().push(RustUseBinding {
                        path,
                        from: block.map_or(-1, |b| b.0 as i64),
                        to: block.map_or(usize::MAX, |b| b.1),
                    });
                }
            }
        }
        Rc::new(bindings)
    })
}

/// The path `name` is `use`d as at `at` in `file_path` (innermost block wins).
pub fn rust_use_path(name: &str, file_path: &str, at: Option<usize>, ctx: &Ctx) -> Option<Vec<String>> {
    let all = rust_use_bindings(file_path, ctx);
    let mut best: Option<&RustUseBinding> = None;
    for b in all.get(name).map(Vec::as_slice).unwrap_or(&[]) {
        let visible = match at {
            None => b.from < 0,
            Some(at) => (at as i64) > b.from && at < b.to,
        };
        if visible && best.is_none_or(|x| b.from > x.from) {
            best = Some(b);
        }
    }
    best.map(|b| b.path.clone())
}

/// The module path a file declares (below `src`/`tests`/`examples`/`benches`).
pub fn rust_module_path(file_path: &str) -> Vec<String> {
    let parts: Vec<&str> = file_path.split('/').collect();
    let mut at: i64 = -1;
    let mut i = parts.len() as i64 - 2;
    while i >= 0 {
        if matches!(parts[i as usize], "src" | "tests" | "examples" | "benches") {
            at = i;
            break;
        }
        i -= 1;
    }
    let mut modules: Vec<String> = parts[(at + 1) as usize..].iter().map(|s| s.to_string()).collect();
    let file = strip(re!(r"\.rs$"), &modules.pop().unwrap_or_default());
    if file != "mod" && file != "lib" && file != "main" {
        modules.push(file);
    }
    modules
}

/// Whether the module path of `n`'s file passes through `modules` in order.
pub fn rust_modules_lead(modules: &[String], n: &Node) -> bool {
    let mut k = 0usize;
    for m in rust_module_path(&n.file_path) {
        if k < modules.len() && m == modules[k] {
            k += 1;
        }
    }
    k == modules.len()
}

memo_map!(WalkedItemsMemo, String, Option<Rc<HashSet<String>>>);

fn rust_walked_items(segs: &[String], file: &str, at: Option<usize>, ctx: &Ctx) -> Option<Rc<HashSet<String>>> {
    let from = rust_module_at(file, at.unwrap_or(0), ctx);
    let open = from.block.map(|b| rust_uses_of(file, ctx).blocks[b].open as i64).unwrap_or(-1);
    let key = format!("{file}\0{open}\0{}", segs.join("::"));
    memoized::<WalkedItemsMemo>(ctx, key, || {
        let items = rust_use_path_items(segs, &from, ctx, &mut HashSet::new())?;
        Some(Rc::new(items.iter().map(|n| n.id.clone()).collect()))
    })
}

// -- Looking a type name up ----------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RustTypeLookup {
    pub nodes: Vec<NodeRef>,
    pub outside: bool,
    pub undeclared: bool,
    pub name: String,
}

fn lookup_outside(name: &str) -> RustTypeLookup {
    RustTypeLookup { nodes: Vec::new(), outside: true, undeclared: false, name: name.to_string() }
}

/// The project types a written `path::name` can mean at `at` of `file`.
pub fn rust_type_lookup(name: &str, written_path: &[String], file: &str, ctx: &Ctx, at: Option<usize>) -> RustTypeLookup {
    let mut segments: Vec<String> = written_path.to_vec();
    let mut real = name.to_string();
    let mut via_use = false;
    if segments.is_empty() {
        if let Some(bound) = rust_use_path(name, file, at, ctx).filter(|b| b.len() > 1) {
            real = bound[bound.len() - 1].clone();
            segments = bound[..bound.len() - 1].to_vec();
            via_use = true;
        }
    } else if let Some(head) = rust_use_path(&segments[0], file, at, ctx).filter(|h| !h.is_empty()) {
        let mut next = head;
        next.extend(segments[1..].iter().cloned());
        segments = next;
    }
    let root = segments.first().cloned();
    let crates = rust_project_crates(ctx);
    if let Some(root) = &root {
        if root == "std" || root == "core" || root == "alloc" {
            return lookup_outside(&real);
        }
        if !crates.contains(root) && rust_dependency_crates(ctx).contains(root) {
            return lookup_outside(&real);
        }
    }
    if segments.is_empty() && is_rust_primitive(&real) {
        return lookup_outside(&real);
    }
    let file_ref = file_ref(file);
    let all: Vec<NodeRef> = ctx
        .get_nodes_by_name(&real)
        .iter()
        .filter(|n| n.language == Language::Rust && n.name == real && is_rust_declared_type_kind(n.kind))
        .cloned()
        .collect();
    if all.is_empty() {
        if segments.is_empty() && (is_rust_std_type(&real) || in_rust_prelude(&real)) {
            return lookup_outside(&real);
        }
        return RustTypeLookup { nodes: Vec::new(), outside: false, undeclared: true, name: real };
    }
    let mut nodes: Vec<NodeRef> = all.into_iter().filter(|n| is_rust_item_visible_from(n, &file_ref)).collect();
    if !segments.is_empty() {
        let mut path = segments.clone();
        path.push(real.clone());
        let walked = rust_walked_items(&path, file, at, ctx);
        let named: Vec<NodeRef> = match &walked {
            None => Vec::new(),
            Some(w) => nodes.iter().filter(|n| w.contains(&n.id)).cloned().collect(),
        };
        if !named.is_empty() {
            return RustTypeLookup { nodes: named, outside: false, undeclared: false, name: real };
        }
    }
    let modules: Vec<String> =
        segments.iter().filter(|s| *s != "crate" && *s != "self" && *s != "super" && !crates.contains(*s)).cloned().collect();
    if !modules.is_empty() {
        let mut within: Vec<NodeRef> = nodes.iter().filter(|n| rust_modules_lead(&modules, n)).cloned().collect();
        if within.len() > 1 && root.as_ref().is_some_and(|r| r == "crate" || crates.contains(r)) {
            let depth = |n: &Node| rust_module_path(&n.file_path).len();
            let nearest = within.iter().map(|n| depth(n)).min().unwrap_or(0);
            within.retain(|n| depth(n) == nearest);
        }
        if !within.is_empty() {
            nodes = within;
        }
    }
    if let Some(root) = &root {
        let roots: HashSet<String> = HashSet::from([root.clone()]);
        nodes.retain(|n| rust_use_can_reach(&roots, n, &file_ref, ctx));
    }
    if segments.is_empty() && !via_use {
        let same: Vec<NodeRef> = nodes.iter().filter(|n| n.file_path == file).cloned().collect();
        if !same.is_empty() {
            return RustTypeLookup { nodes: same, outside: false, undeclared: false, name: real };
        }
        let u = rust_uses_of(file, ctx);
        if u.external.contains(&real) && !u.bound.contains(&real) {
            return lookup_outside(&real);
        }
        let globbed: Vec<NodeRef> = nodes.iter().filter(|n| rust_glob_scope(&real, n, &u, &file_ref, ctx)).cloned().collect();
        if !globbed.is_empty() {
            return RustTypeLookup { nodes: globbed, outside: false, undeclared: false, name: real };
        }
        if is_rust_std_type(&real) || in_rust_prelude(&real) {
            return lookup_outside(&real);
        }
        let dir = posix::dirname(file);
        let near: Vec<NodeRef> = nodes
            .iter()
            .filter(|n| file.starts_with(&format!("{}/", rust_module_dir(&n.file_path))) || posix::dirname(&n.file_path) == dir)
            .cloned()
            .collect();
        if !near.is_empty() {
            nodes = near;
        }
    }
    RustTypeLookup { nodes, outside: false, undeclared: false, name: real }
}

// -- Reading a written type ----------------------------------------------------------------

/// The traits bounding generic `name` where `written` is written (`None` when it isn't a generic there).
pub fn rust_generic_bounds(name: &str, written: &RustWrittenType, ctx: &Ctx) -> Option<Vec<String>> {
    if let Some(generics) = &written.generics {
        return rust_bounds_of(name, &[(generics.as_str(), "")]).map(|_| Vec::new());
    }
    let scopes = rust_scopes_of(&written.file, ctx)?;
    let mut items: Vec<(&str, &str)> = Vec::new();
    if let Some(f) = rust_fn_at(&scopes, written.at) {
        items.push((f.generics.as_str(), f.where_.as_str()));
    }
    if let Some(b) = rust_impl_at(&scopes, written.at) {
        items.push((b.generics.as_str(), b.where_.as_str()));
    }
    if items.is_empty() {
        return None;
    }
    rust_bounds_of(name, &items)
}

fn real_traits(bounds: &[String]) -> Vec<String> {
    bounds
        .iter()
        .filter(|b| !re!(r"^['?]").is_match(trim(b)) && !is_rust_marker_trait(&rust_path_head(b).map(|h| h.name).unwrap_or_default()))
        .cloned()
        .collect()
}

pub fn rust_read_type(written: &RustWrittenType, r: &UnresolvedRef, ctx: &Ctx) -> RustTypeRead {
    let mut wrappers: Vec<String> = written.wrappers.clone().unwrap_or_default();
    let by_ref = trim(&written.text).starts_with('&');
    let mut t = trim(&written.text).to_string();
    for _ in 0..8 {
        let before = t.clone();
        t = trim(&strip(re!(r"^mut\s+"), &strip(re!(r"^&\s*(?:'\w+\s+)?(?:mut\s+)?"), &t))).to_string();
        let unwrapped: Option<(String, String)> =
            re!(r"^(?:::\s*)?(?:(?:std|alloc|core)\s*::\s*(?:\w+\s*::\s*)*)?([A-Za-z_]\w*)\s*<").captures(&t).and_then(|wrapper| {
                let name = wrapper.text(1).unwrap_or("");
                if !RUST_DEREF_WRAPPERS.contains(&name) {
                    return None;
                }
                let open = wrapper.get(0).expect("group 0").end() - 1;
                if rust_angle_end(&t, open) != Some(t.len() - 1) {
                    return None;
                }
                let inner = strip(re!(r"^\s*'\w+\s*,"), &t[open + 1..t.len() - 1]);
                Some((trim(&inner).to_string(), name.to_string()))
            });
        if let Some((inner, name)) = unwrapped {
            t = inner;
            wrappers.push(name);
        }
        if t == before {
            break;
        }
    }
    let body = |kind: RustReadKind, wrappers: Vec<String>| RustTypeRead::Read(RustReadBody { kind, wrappers, by_ref, written: written.clone() });
    if t.is_empty() || t == "_" || t == "!" {
        return RustTypeRead::Unread;
    }
    if let Some(object) = re!(r"^(dyn|impl)\s+([\s\S]+)$").captures(&t) {
        let traits: Vec<String> = split_rust_top_level(object.text(2).unwrap_or(""), '+')
            .into_iter()
            .filter(|b| !re!(r"^['?]").is_match(b) && !is_rust_marker_trait(&rust_path_head(b).map(|h| h.name).unwrap_or_default()))
            .collect();
        return if traits.is_empty() {
            body(RustReadKind::Outside(object.text(1).unwrap_or("").to_string()), wrappers)
        } else {
            body(RustReadKind::Traits(traits), wrappers)
        };
    }
    if re!(r"^[(\[*]").is_match(&t) || re!(r#"^(?:unsafe\s+|extern\s+"[^"]*"\s+)*fn\s*\("#).is_match(&t) {
        return body(RustReadKind::Outside(t.chars().next().map(String::from).unwrap_or_default()), wrappers);
    }
    let Some(head) = rust_path_head(&t) else { return RustTypeRead::Unread };
    let RustPathHead { segments, name, args } = head;
    if !segments.is_empty() && (segments[0] == "Self" || rust_generic_bounds(&segments[0], written, ctx).is_some()) {
        return RustTypeRead::Unread;
    }
    if segments.is_empty() && name == "Self" {
        if let Some(self_) = &written.self_ {
            return body(RustReadKind::Named { name: self_.clone(), path: Vec::new(), args: String::new() }, wrappers);
        }
        let scopes = rust_scopes_of(&written.file, ctx);
        let mut at_ref = r.clone();
        at_ref.file_path = written.file.clone();
        at_ref.line = scopes.as_ref().map_or(r.line, |s| rust_line_at(s, written.at));
        let Some(self_) = rust_self_type(&at_ref, ctx) else { return RustTypeRead::Unread };
        if self_.is_trait {
            return body(RustReadKind::Traits(vec![self_.type_]), wrappers);
        }
        let path = self_.root.map(|r| vec![r]).unwrap_or_default();
        return body(RustReadKind::Named { name: self_.type_, path, args: String::new() }, wrappers);
    }
    if segments.is_empty() {
        if written.opaque.as_ref().is_some_and(|o| o.contains(&name)) {
            return RustTypeRead::Unread;
        }
        if let Some(known) = written.bounds.as_ref().and_then(|b| b.get(&name)) {
            let traits = real_traits(&known.traits);
            if traits.is_empty() {
                return RustTypeRead::Unread;
            }
            return RustTypeRead::Read(RustReadBody {
                kind: RustReadKind::Traits(traits),
                wrappers,
                by_ref,
                written: RustWrittenType::plain(written.text.clone(), known.file.clone(), known.at),
            });
        }
        if let Some(bounds) = rust_generic_bounds(&name, written, ctx) {
            let traits: Vec<String> =
                bounds.into_iter().filter(|b| !is_rust_marker_trait(&rust_path_head(b).map(|h| h.name).unwrap_or_default())).collect();
            return if traits.is_empty() { RustTypeRead::Unread } else { body(RustReadKind::Traits(traits), wrappers) };
        }
        if is_rust_primitive(&name) {
            return body(RustReadKind::Outside(name), wrappers);
        }
    }
    body(RustReadKind::Named { name, path: segments, args }, wrappers)
}

// -- Binding and value types ---------------------------------------------------------------

pub fn rust_binding_type(binding: &RustBinding, scopes: &RustScopes, r: &UnresolvedRef, ctx: &Ctx, depth: u32) -> Option<RustWrittenType> {
    if let Some(ty) = &binding.ty {
        return Some(RustWrittenType::plain(ty.clone(), r.file_path.clone(), binding.at));
    }
    if binding.kind == BindingKind::For {
        let element = binding.scrutinee.as_ref().and_then(|s| rust_iterated_element(s, binding.at, scopes, r, ctx, depth));
        let Some(site) = &binding.site else { return element };
        let element = element?;
        return if site.path.is_empty() && site.index.is_some() { rust_tuple_field(&element, site.index.unwrap()) } else { None };
    }
    if let Some(init) = &binding.init {
        return rust_value_type(init, binding.at, scopes, r, ctx, depth);
    }
    if let Some(site) = &binding.site {
        return rust_site_type(site, binding.scrutinee.as_deref(), binding.at, scopes, r, ctx, depth);
    }
    None
}

pub fn rust_local_type(name: &str, at: usize, scopes: &RustScopes, r: &UnresolvedRef, ctx: &Ctx, depth: u32) -> Option<RustWrittenType> {
    if depth > 3 {
        return None;
    }
    let (binding, ambiguous) = rust_binding_at(scopes, name, at)?;
    if ambiguous {
        None
    } else {
        rust_binding_type(&binding, scopes, r, ctx, depth + 1)
    }
}

pub fn rust_value_type(value: &str, at: usize, scopes: &RustScopes, r: &UnresolvedRef, ctx: &Ctx, depth: u32) -> Option<RustWrittenType> {
    if depth > 3 {
        return None;
    }
    let mut v = trim(value).to_string();
    let mut unwraps = 0u32;
    let mut awaits = 0u32;
    loop {
        let before = v.clone();
        if re!(r"\.\s*await$").is_match(&v) {
            v = strip(re!(r"\s*\.\s*await$"), &v);
            awaits += 1;
        }
        if v.ends_with('?') {
            v = v[..v.len() - 1].trim_end_matches(is_js_space).to_string();
            unwraps += 1;
        }
        if let Some((callee, _)) = rust_trailing_call(&v) {
            if re!(r"\.\s*(?:unwrap|expect)$").is_match(&callee) {
                v = strip(re!(r"\s*\.\s*(?:unwrap|expect)$"), &callee);
                unwraps += 1;
            }
        }
        if v == before {
            break;
        }
    }
    v = trim(&strip(re!(r"^&\s*(?:mut\s+)?"), &v)).to_string();
    let local = |text: &str| -> Option<RustWrittenType> { Some(RustWrittenType::plain(text, r.file_path.clone(), at)) };
    let unwrapped = |t: Option<RustWrittenType>| -> Option<RustWrittenType> {
        match t {
            Some(t) if unwraps > 0 => rust_unwrapped(&t, unwraps),
            other => other,
        }
    };
    if re!(r##"^(?:[bc]?r#*)?"[^"]*"#*$|^b?'[^']*'$|^-?\d[\w.]*$|^(?:true|false)$|^(?:move\s+)?\|"##).is_match(&v)
        || (v.starts_with('[') && rust_group_is_whole(&v))
    {
        return local("()");
    }
    if re!(r"^(?:vec|format)\s*!\s*[\[({]").is_match(&v) {
        let open = re!(r"[\[({]").find(&v).map(|m| m.start()).unwrap_or(0);
        if rust_group_is_whole(&v[open..]) {
            return local(if v.starts_with("vec") { "Vec<_>" } else { "String" });
        }
    }
    if v.starts_with('(') {
        if !rust_group_is_whole(&v) {
            return None;
        }
        let inner = &v[1..v.len() - 1];
        return if split_rust_top_level(inner, ',').len() == 1 && !re!(r",\s*$").is_match(inner) {
            unwrapped(rust_value_type(inner, at, scopes, r, ctx, depth + 1))
        } else {
            local("()")
        };
    }
    let call = rust_trailing_call(&v);
    if let Some((callee_raw, args)) = &call {
        if re!(r"^[A-Za-z_][\w:\s<>,]*$").is_match(callee_raw) && !callee_raw.contains('.') {
            let callee = strip(re!(r"::<[^()]*>$"), &re!(r"\s+", "g").replace_all(callee_raw, |_| Some(String::new())));
            if !re!(r"^[A-Za-z_]\w*(?:::[A-Za-z_]\w*)*$").is_match(&callee) {
                return None;
            }
            let segments: Vec<&str> = callee.split("::").collect();
            let last = segments[segments.len() - 1];
            let wrapper = re!(r"^(?:(?:std|alloc)::(?:boxed|rc|sync)::)?(Box|Rc|Arc)::(?:new|pin|from)$")
                .captures(&callee)
                .and_then(|c| c.text(1).map(String::from));
            if let Some(wrapper) = wrapper {
                if split_rust_top_level(args, ',').len() == 1 {
                    let held = rust_value_type(args, at, scopes, r, ctx, depth + 1);
                    return match held {
                        Some(mut held) if unwraps == 0 => {
                            let mut w = vec![wrapper];
                            w.extend(held.wrappers.take().unwrap_or_default());
                            held.wrappers = Some(w);
                            Some(held)
                        }
                        _ => None,
                    };
                }
            }
            if re!(r"^[A-Z]").is_match(last) {
                if segments.len() == 1 && last == "Some" {
                    return unwrapped(local("Option<_>"));
                }
                if segments.len() == 1 && (last == "Ok" || last == "Err") {
                    return unwrapped(local("Result<_, _>"));
                }
                if re!(r"^[A-Z][A-Z0-9_]*$").is_match(last) && last.len() > 1 {
                    return None;
                }
                if segments.len() >= 2 && re!(r"^[A-Z]").is_match(segments[segments.len() - 2]) {
                    return unwrapped(local(&segments[..segments.len() - 1].join("::")));
                }
                return unwrapped(local(&callee));
            }
            if segments.len() >= 2 && re!(r"^[A-Z]").is_match(segments[segments.len() - 2]) {
                let owner: Vec<String> = segments[..segments.len() - 1].iter().map(|s| s.to_string()).collect();
                return rust_associated_return(&owner, last, unwraps, at, r, ctx);
            }
            let modules: Vec<String> = segments[..segments.len() - 1].iter().map(|s| s.to_string()).collect();
            return rust_function_return(&modules, last, unwraps, at, r, ctx);
        }
    }
    if let Some((callee, args)) = &call {
        if trim(args).is_empty() {
            if let Some(cloned) = re!(r"^([a-z_]\w*)\s*\.\s*clone$").captures(callee) {
                return unwrapped(rust_local_type(cloned.text(1).unwrap_or(""), at, scopes, r, ctx, depth));
            }
        }
        let method = re!(r"^([\s\S]+?)\s*\.\s*([a-z_]\w*)\s*(?:::\s*<([^()]*)>)?$").captures(callee);
        return method.and_then(|m| {
            rust_method_return(
                m.text(1).unwrap_or(""),
                m.text(2).unwrap_or(""),
                m.text(3).map(String::from),
                unwraps,
                awaits,
                at,
                scopes,
                r,
                ctx,
                depth,
            )
        });
    }
    if let Some(access) = re!(r"^([\s\S]+?)\s*\.\s*([A-Za-z_]\w*|\d+)$").captures(&v) {
        if !re!(r"^\d").is_match(&v) {
            return unwrapped(rust_field_value(access.text(1).unwrap_or(""), access.text(2).unwrap_or(""), at, scopes, r, ctx, depth));
        }
    }
    if let Some(literal) = re!(r"^((?:[A-Za-z_]\w*\s*::\s*)*[A-Z]\w*)\s*(?:::\s*<[^{}]*>\s*)?\{").captures(&v) {
        let brace = v.find('{').unwrap_or(0);
        if !rust_group_is_whole(&v[brace..]) {
            return None;
        }
        let collapsed = re!(r"\s+", "g").replace_all(literal.text(1).unwrap_or(""), |_| Some(String::new()));
        let segments: Vec<&str> = collapsed.split("::").collect();
        let used = if segments.len() >= 2 && re!(r"^[A-Z]").is_match(segments[segments.len() - 2]) { &segments[..segments.len() - 1] } else { &segments[..] };
        return unwrapped(local(&used.join("::")));
    }
    if re!(r"^[A-Za-z_]\w*(?:\s*::\s*[A-Za-z_]\w*)*$").is_match(&v) {
        let segments: Vec<String> = v.split("::").map(|s| trim(s).to_string()).collect();
        let last = &segments[segments.len() - 1];
        if segments.len() == 1 && re!(r"^[a-z_]").is_match(last) {
            return unwrapped(rust_local_type(last, at, scopes, r, ctx, depth));
        }
        if !re!(r"^[A-Z]").is_match(last) || (re!(r"^[A-Z][A-Z0-9_]*$").is_match(last) && last.len() > 1) {
            return None;
        }
        if segments.len() == 1 && last == "None" {
            return local("Option<_>");
        }
        if segments.len() >= 2 && re!(r"^[A-Z]").is_match(&segments[segments.len() - 2]) {
            return local(&segments[..segments.len() - 1].join("::"));
        }
        return unwrapped(local(&segments.join("::")));
    }
    None
}

/// The offset just inside the signature of `fn_` (its parameter list), so generics resolve there.
pub fn rust_signature_offset(fn_: &Node, ctx: &Ctx) -> usize {
    let start = rust_node_offset(fn_, ctx);
    let own = rust_scopes_of(&fn_.file_path, ctx).and_then(|s| s.fns.iter().find(|f| f.at >= start && f.at <= start + 512).map(|f| f.from));
    own.map_or(start, |from| from + 1)
}

fn rust_associated_return(owner: &[String], fn_: &str, unwraps: u32, at: usize, r: &UnresolvedRef, ctx: &Ctx) -> Option<RustWrittenType> {
    let written = RustWrittenType::plain(owner.join("::"), r.file_path.clone(), at);
    let read = rust_read_type(&written, r, ctx);
    let constructs = re!(r"^(?:new|default|from|open|create|connect|bind|empty|(?:new|with|from)_\w+)$").is_match(fn_);
    let RustTypeRead::Read(body) = read else { return None };
    let (name, path) = match &body.kind {
        RustReadKind::Outside(_) => return if constructs { Some(written) } else { None },
        RustReadKind::Named { name, path, .. } => (name.clone(), path.clone()),
        RustReadKind::Traits(_) => return None,
    };
    let lookup = rust_type_lookup(&name, &path, &body.written.file, ctx, Some(body.written.at));
    if lookup.outside {
        return if constructs { Some(written) } else { None };
    }
    let fns = rust_owned_methods(&lookup.name, fn_, &lookup, ctx);
    if fns.is_empty() {
        return if (fn_ == "new" || fn_ == "default") && unwraps == 0 { Some(written) } else { None };
    }
    let returns: HashSet<String> = fns.iter().map(|f| rust_return_text(f.signature.as_deref()).unwrap_or_default()).collect();
    if returns.len() != 1 {
        return None;
    }
    let text = returns.into_iter().next().unwrap();
    if text.is_empty() {
        return None;
    }
    let made = RustWrittenType::plain(text, fns[0].file_path.clone(), rust_signature_offset(&fns[0], ctx));
    let value = if unwraps > 0 { rust_unwrapped(&made, unwraps) } else { Some(made) };
    if let Some(v) = &value {
        if trim(&v.text) == "Self" || rust_path_head(&v.text).is_some_and(|h| h.name == name) {
            return Some(written);
        }
    }
    let files: HashSet<&String> = fns.iter().map(|f| &f.file_path).collect();
    if files.len() > 1 {
        None
    } else {
        value
    }
}

fn rust_function_return(modules: &[String], fn_: &str, unwraps: u32, at: usize, r: &UnresolvedRef, ctx: &Ctx) -> Option<RustWrittenType> {
    let mut fns: Vec<NodeRef> = ctx
        .get_nodes_by_name(fn_)
        .iter()
        .filter(|n| n.language == Language::Rust && n.kind == NodeKind::Function && n.name == fn_)
        .cloned()
        .collect();
    let same: Vec<NodeRef> = fns.iter().filter(|n| n.file_path == r.file_path).cloned().collect();
    if !same.is_empty() && modules.is_empty() {
        fns = same;
    } else {
        let mut path: Vec<String>;
        if modules.is_empty() {
            path = rust_use_path(fn_, &r.file_path, Some(at), ctx)?;
        } else {
            let head = rust_use_path(&modules[0], &r.file_path, Some(at), ctx).filter(|h| !h.is_empty());
            path = head.unwrap_or_else(|| vec![modules[0].clone()]);
            path.extend(modules[1..].iter().cloned());
            path.push(fn_.to_string());
        }
        let walked = rust_walked_items(&path, &r.file_path, Some(at), ctx);
        let named: Vec<NodeRef> = match &walked {
            None => Vec::new(),
            Some(w) => fns.iter().filter(|n| w.contains(&n.id)).cloned().collect(),
        };
        if !named.is_empty() {
            fns = named;
        } else {
            let crates = rust_project_crates(ctx);
            let segments: Vec<String> = path[..path.len() - 1]
                .iter()
                .filter(|s| *s != "crate" && *s != "self" && *s != "super" && !crates.contains(*s))
                .cloned()
                .collect();
            if !segments.is_empty() {
                fns.retain(|n| rust_modules_lead(&segments, n));
            }
        }
    }
    let returns: HashSet<String> = fns.iter().map(|f| rust_return_text(f.signature.as_deref()).unwrap_or_default()).collect();
    if fns.is_empty() || returns.len() != 1 {
        return None;
    }
    let text = returns.into_iter().next().unwrap();
    if text.is_empty() {
        return None;
    }
    let made = RustWrittenType::plain(text, fns[0].file_path.clone(), rust_signature_offset(&fns[0], ctx));
    if unwraps > 0 {
        rust_unwrapped(&made, unwraps)
    } else {
        Some(made)
    }
}

#[allow(clippy::too_many_arguments)]
fn rust_method_return(
    receiver: &str,
    method: &str,
    turbofish: Option<String>,
    unwraps: u32,
    awaits: u32,
    at: usize,
    scopes: &RustScopes,
    r: &UnresolvedRef,
    ctx: &Ctx,
    depth: u32,
) -> Option<RustWrittenType> {
    let memo = ctx.memo::<Bounded<Option<RustWrittenType>, 4096>>();
    let key = format!(
        "{}\0{at}\0{receiver}\0{method}\0{}\0{unwraps}\0{awaits}\0{depth}",
        r.file_path,
        turbofish.as_deref().unwrap_or("")
    );
    memo.get_or(&key, false, || read_rust_method_return(receiver, method, turbofish.as_deref(), unwraps, awaits, at, scopes, r, ctx, depth))
}

#[allow(clippy::too_many_arguments)]
fn read_rust_method_return(
    receiver: &str,
    method: &str,
    turbofish: Option<&str>,
    unwraps: u32,
    awaits: u32,
    at: usize,
    scopes: &RustScopes,
    r: &UnresolvedRef,
    ctx: &Ctx,
    depth: u32,
) -> Option<RustWrittenType> {
    let recv = trim(receiver);
    let written = if recv == "self" {
        Some(RustWrittenType::plain("Self", r.file_path.clone(), at))
    } else if re!(r"^[a-z_]\w*$").is_match(recv) {
        rust_local_type(recv, at, scopes, r, ctx, depth)
    } else if re!(r"[)?]$|\.\s*(?:[A-Za-z_]\w*|\d+)$").is_match(recv) {
        rust_value_type(recv, at, scopes, r, ctx, depth + 1)
    } else {
        return None;
    }?;
    let read = rust_read_type(&written, r, ctx);
    let result = match super::calls::rust_typed_method_call(&read, method, r, ctx, 0) {
        super::calls::CallRes::Hit(h) => h,
        _ => return None,
    };
    let target = ctx.node_by_id(&result.target_node_id)?;
    let text = rust_return_text(target.signature.as_deref())?;
    let made = RustWrittenType::plain(text, target.file_path.clone(), rust_signature_offset(&target, ctx));
    let mut made = Some(rust_return_instance(made, &target, turbofish, &read, at, r, ctx));
    let is_async = rust_is_async_fn(&target, ctx);
    if awaits > 1 || (awaits == 0 && is_async) {
        return None;
    }
    if awaits == 1 && !is_async {
        made = made.and_then(|m| rust_future_output(&m));
    }
    match made {
        Some(m) if unwraps > 0 => rust_unwrapped(&m, unwraps),
        other => other,
    }
}

fn identifier_names(text: &str) -> Vec<String> {
    re!(r"(?<!['\w])([A-Za-z_]\w*)", "g").captures_iter(text).map(|m| m.text(1).unwrap_or("").to_string()).collect()
}

fn rust_return_instance(
    made: RustWrittenType,
    target: &Node,
    turbofish: Option<&str>,
    read: &RustTypeRead,
    at: usize,
    r: &UnresolvedRef,
    ctx: &Ctx,
) -> RustWrittenType {
    let names: Vec<String> =
        identifier_names(&made.text).into_iter().filter(|n| !re!(r"^(?:mut|dyn|impl|const|for)$").is_match(n)).collect();
    let generics: Vec<String> = names.iter().filter(|n| *n != "Self" && rust_generic_bounds(n, &made, ctx).is_some()).cloned().collect();
    if generics.is_empty() {
        let bare = trim(&strip(re!(r"^(?:&\s*(?:'\w+\s+)?(?:mut\s+)?)+"), &made.text)).to_string();
        if bare == "Self" {
            if let RustTypeRead::Read(RustReadBody { kind: RustReadKind::Named { name, path, args }, written, .. }) = read {
                if super::types::rust_trait_of(target, ctx).is_some() {
                    let mut type_text = path.iter().cloned().chain(std::iter::once(name.clone())).collect::<Vec<_>>().join("::");
                    if !args.is_empty() {
                        type_text.push_str(&format!("<{args}>"));
                    }
                    let text = re!(r"Self\s*$").replace_first(&made.text, |_| type_text.clone());
                    return RustWrittenType::plain(text, written.file.clone(), written.at);
                }
            }
        }
        return made;
    }
    let unique: Vec<String> = {
        let mut seen = HashSet::new();
        generics.iter().filter(|g| seen.insert((*g).clone())).cloned().collect()
    };
    let opaque = RustWrittenType { opaque: Some(unique.clone()), ..made.clone() };
    let Some(turbofish) = turbofish else { return opaque };
    let own = rust_scopes_of(&made.file, ctx).and_then(|s| rust_fn_at(&s, made.at).map(|f| f.generics.clone()));
    let params: Vec<String> = own
        .map(|g| {
            split_rust_top_level(&g, ',')
                .iter()
                .filter_map(|p| re!(r"^\s*(?:const\s+)?([A-Za-z_]\w*)").captures(p).and_then(|c| c.text(1).map(String::from)))
                .collect()
        })
        .unwrap_or_default();
    let args: Vec<String> =
        split_rust_top_level(turbofish, ',').iter().map(|a| trim(a).to_string()).filter(|a| !a.starts_with('\'')).collect();
    let mut bound: HashMap<String, String> = HashMap::new();
    for g in &unique {
        let Some(i) = params.iter().position(|p| p == g) else { return opaque };
        if i >= args.len() || args[i] == "_" {
            return opaque;
        }
        bound.insert(g.clone(), args[i].clone());
    }
    if names.iter().any(|n| !bound.contains_key(n) && (n == "Self" || !(is_rust_std_type(n) || in_rust_prelude(n) || is_rust_primitive(n)))) {
        return opaque;
    }
    let text = re!(r"(?<!['\w])([A-Za-z_]\w*)", "g").replace_all(&made.text, |c| {
        let n = c.text(1).unwrap_or("");
        Some(bound.get(n).cloned().unwrap_or_else(|| n.to_string()))
    });
    RustWrittenType::plain(text, r.file_path.clone(), at)
}

fn rust_is_async_fn(fn_: &Node, ctx: &Ctx) -> bool {
    let Some(scopes) = rust_scopes_of(&fn_.file_path, ctx) else { return false };
    let start = rust_node_offset(fn_, ctx);
    let head = code_slice(&scopes, start, start + 300);
    let bare = strip(re!(r"^r#"), &fn_.name);
    let Some(name) = Re::cached(&format!(r"\bfn\s+(?:r#)?{}\b", escape_regex(&bare)), "").find(head) else { return false };
    re!(r"\basync\b").is_match(&head[..name.start()])
}

fn rust_future_output(written: &RustWrittenType) -> Option<RustWrittenType> {
    let mut t = trim(&written.text).to_string();
    for wrapper in [re!(r"^(?:(?:std|core)\s*::\s*pin\s*::\s*)?Pin\s*<"), re!(r"^(?:(?:std|alloc)\s*::\s*boxed\s*::\s*)?Box\s*<")] {
        if let Some(open) = wrapper.find(&t) {
            if rust_angle_end(&t, open.end() - 1) == Some(t.len() - 1) {
                t = trim(&t[open.end()..t.len() - 1]).to_string();
            }
        }
    }
    if let Some(boxed) = re!(r"^(?:[A-Za-z_]\w*\s*::\s*)*(?:BoxFuture|LocalBoxFuture)\s*<").find(&t) {
        if rust_angle_end(&t, boxed.end() - 1) != Some(t.len() - 1) {
            return None;
        }
        let args: Vec<String> =
            split_rust_top_level(&t[boxed.end()..t.len() - 1], ',').into_iter().filter(|a| !a.trim().starts_with('\'')).collect();
        return if args.len() == 1 { Some(RustWrittenType { text: trim(&args[0]).to_string(), ..written.clone() }) } else { None };
    }
    let future = re!(r"^(?:impl|dyn)\s+(?:(?:std|core)\s*::\s*future\s*::\s*)?(?:Future|IntoFuture)\s*<").find(&t)?;
    let end = rust_angle_end(&t, future.end() - 1)?;
    if !re!(r"^\s*(?:\+[\s\S]*)?$").is_match(&t[end + 1..]) {
        return None;
    }
    let output = split_rust_top_level(&t[future.end()..end], ',')
        .iter()
        .find_map(|a| re!(r"^\s*Output\s*=\s*([\s\S]+)$").captures(a).and_then(|c| c.text(1).map(String::from)))?;
    Some(RustWrittenType { text: trim(&output).to_string(), ..written.clone() })
}

pub fn rust_field_value(
    receiver: &str,
    field: &str,
    at: usize,
    scopes: &RustScopes,
    r: &UnresolvedRef,
    ctx: &Ctx,
    depth: u32,
) -> Option<RustWrittenType> {
    let recv = trim(receiver);
    let written = if recv == "self" {
        Some(RustWrittenType::plain("Self", r.file_path.clone(), at))
    } else if re!(r"^[a-z_]\w*$").is_match(recv) {
        rust_local_type(recv, at, scopes, r, ctx, depth)
    } else {
        rust_value_type(recv, at, scopes, r, ctx, depth + 1)
    }?;
    let RustTypeRead::Read(body) = rust_read_type(&written, r, ctx) else { return None };
    let RustReadKind::Named { name, path, .. } = &body.kind else { return None };
    let lookup = rust_type_lookup(name, path, &body.written.file, ctx, Some(body.written.at));
    let structs: Vec<&NodeRef> =
        if lookup.outside { Vec::new() } else { lookup.nodes.iter().filter(|n| matches!(n.kind, NodeKind::Struct | NodeKind::Union)).collect() };
    if structs.len() != 1 {
        return None;
    }
    let decl = structs[0];
    let fields = rust_declared_fields(decl, ctx, false)?;
    let text = if re!(r"^\d+$").is_match(field) {
        field.parse::<usize>().ok().and_then(|i| fields.tuple.as_ref().and_then(|t| t.get(i)).cloned())
    } else {
        fields.named.as_ref().and_then(|n| n.get(field)).cloned()
    }
    .filter(|t| !t.is_empty())?;
    let mut value = RustWrittenType {
        text,
        file: decl.file_path.clone(),
        at: rust_node_offset(decl, ctx),
        self_: Some(decl.name.clone()),
        generics: Some(fields.generics.clone()),
        ..Default::default()
    };
    if recv == "self" {
        if let Some(bounds) = rust_self_param_bounds(&fields.generics, scopes, at, &r.file_path) {
            value.bounds = Some(Rc::new(bounds));
        }
    }
    Some(value)
}

fn rust_self_param_bounds(struct_generics: &str, scopes: &RustScopes, at: usize, file: &str) -> Option<HashMap<String, RustBound>> {
    if trim(struct_generics).is_empty() {
        return None;
    }
    let block = rust_impl_at(scopes, at)?;
    if block.kind != ImplKind::Impl {
        return None;
    }
    let mut header = trim(&strip(re!(r"^impl\b"), code_slice(scopes, block.at, block.from))).to_string();
    if header.starts_with('<') {
        let end = rust_angle_end(&header, 0)?;
        header = trim(&header[end + 1..]).to_string();
    }
    let mut depth = 0i32;
    let bytes: Vec<u8> = header.bytes().collect();
    for i in 0..bytes.len() {
        let c = bytes[i];
        if c == b'<' {
            depth += 1;
        } else if c == b'>' && (i == 0 || bytes[i - 1] != b'-') {
            depth -= 1;
        } else if depth == 0 && is_js_space(c as char) && re!(r"^for\s").is_match(&header[i + 1..]) {
            header = trim(&header[(i + 4).min(header.len())..]).to_string();
            break;
        }
    }
    let self_ = rust_path_head(trim(&strip(re!(r"\bwhere\b[\s\S]*$"), &header)))?;
    let args: Vec<String> = split_rust_top_level(&self_.args, ',').iter().map(|a| trim(a).to_string()).filter(|a| !a.is_empty()).collect();
    let params: Vec<String> = split_rust_top_level(struct_generics, ',')
        .iter()
        .map(|p| re!(r"^\s*(?:const\s+)?('?[A-Za-z_]\w*)").captures(p).and_then(|c| c.text(1).map(String::from)).unwrap_or_default())
        .collect();
    if params.len() != args.len() {
        return None;
    }
    let own: HashSet<String> = split_rust_top_level(&block.generics, ',')
        .iter()
        .filter_map(|p| re!(r"^\s*(?:const\s+)?([A-Za-z_]\w*)").captures(p).and_then(|c| c.text(1).map(String::from)))
        .collect();
    let mut bounds: HashMap<String, RustBound> = HashMap::new();
    for (i, p) in params.iter().enumerate() {
        let arg = &args[i];
        if p.is_empty() || p.starts_with('\'') || !own.contains(arg) {
            continue;
        }
        if let Some(traits) = rust_bounds_of(arg, &[(block.generics.as_str(), block.where_.as_str())]).filter(|t| !t.is_empty()) {
            bounds.insert(p.clone(), RustBound { traits, file: file.to_string(), at });
        }
    }
    if bounds.is_empty() {
        None
    } else {
        Some(bounds)
    }
}

fn rust_tuple_field(written: &RustWrittenType, index: usize) -> Option<RustWrittenType> {
    let t = strip(re!(r"^(?:&\s*(?:'\w+\s+)?(?:mut\s+)?)+"), trim(&written.text));
    if !t.starts_with('(') || !rust_group_is_whole(&t) {
        return None;
    }
    let fields: Vec<String> = split_rust_top_level(&t[1..t.len() - 1], ',').iter().map(|f| trim(f).to_string()).filter(|f| !f.is_empty()).collect();
    fields.get(index).map(|f| RustWrittenType { text: f.clone(), wrappers: None, ..written.clone() })
}

const RUST_ITEM_ADAPTERS: [&str; 13] =
    ["iter", "iter_mut", "into_iter", "drain", "rev", "copied", "cloned", "skip", "take", "step_by", "peekable", "by_ref", "fuse"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Projection {
    Key,
    Value,
    Enumerate,
}

fn item_projection(name: &str) -> Option<Projection> {
    Some(match name {
        "keys" | "into_keys" => Projection::Key,
        "values" | "values_mut" | "into_values" => Projection::Value,
        "enumerate" => Projection::Enumerate,
        _ => return None,
    })
}

fn rust_iterated_element(iter: &str, at: usize, scopes: &RustScopes, r: &UnresolvedRef, ctx: &Ctx, depth: u32) -> Option<RustWrittenType> {
    let mut v = trim(iter).to_string();
    let mut steps: Vec<String> = Vec::new();
    for _ in 0..8 {
        if let Some(value) = rust_value_type(&strip(re!(r"^&\s*(?:mut\s+)?"), &v), at, scopes, r, ctx, depth + 1) {
            steps.reverse();
            return rust_step_items(&value, &steps, ctx);
        }
        let call = rust_trailing_call(&v);
        let m = call.as_ref().and_then(|(callee, _)| re!(r"^([\s\S]+?)\s*\.\s*([a-z_]\w*)$").captures(callee));
        let Some(m) = m else { return None };
        let step = m.text(2).unwrap_or("").to_string();
        if !(RUST_ITEM_ADAPTERS.contains(&step.as_str()) || item_projection(&step).is_some()) {
            return None;
        }
        let next = m.text(1).unwrap_or("").to_string();
        steps.push(step);
        v = next;
    }
    None
}

fn rust_step_items(value: &RustWrittenType, steps: &[String], ctx: &Ctx) -> Option<RustWrittenType> {
    let mut projection: Option<Projection> = None;
    let mut numbered = false;
    for step in steps {
        match item_projection(step) {
            Some(Projection::Enumerate) => {
                if numbered {
                    return None;
                }
                numbered = true;
            }
            Some(p) => {
                if numbered || projection.is_some() {
                    return None;
                }
                projection = Some(p);
            }
            None => {}
        }
    }
    let item = rust_item_of(value, projection, ctx)?;
    if numbered {
        Some(RustWrittenType { text: format!("(usize, {})", item.text), ..item })
    } else {
        Some(item)
    }
}

const RUST_STD_SEQUENCES: [&str; 9] = ["Vec", "VecDeque", "HashSet", "BTreeSet", "BinaryHeap", "LinkedList", "Option", "IndexSet", "SmallVec"];
const RUST_STD_MAPS: [&str; 3] = ["HashMap", "BTreeMap", "IndexMap"];
const RUST_ITERATOR_TRAITS: [&str; 4] = ["Iterator", "IntoIterator", "DoubleEndedIterator", "ExactSizeIterator"];

fn rust_item_of(written: &RustWrittenType, projection: Option<Projection>, ctx: &Ctx) -> Option<RustWrittenType> {
    let mut t = trim(&written.text).to_string();
    for _ in 0..6 {
        let before = t.clone();
        t = trim(&strip(re!(r"^&\s*(?:'\w+\s+)?(?:mut\s+)?"), &t)).to_string();
        if let Some(pointer) = re!(
            r"^(?:(?:std|alloc|core)\s*::\s*(?:\w+\s*::\s*)*)?(?:Box|Rc|Arc|Cow|Ref|RefMut|MutexGuard|RwLockReadGuard|RwLockWriteGuard)\s*<"
        )
        .find(&t)
        {
            if rust_angle_end(&t, pointer.end() - 1) == Some(t.len() - 1) {
                t = trim(&strip(re!(r"^\s*'\w+\s*,"), &t[pointer.end()..t.len() - 1])).to_string();
            }
        }
        if t == before {
            break;
        }
    }
    let as_ = |text: Option<&str>| -> Option<RustWrittenType> {
        text.filter(|t| !t.is_empty()).map(|t| RustWrittenType { text: trim(t).to_string(), wrappers: None, ..written.clone() })
    };
    let type_args = |args: &str| -> Vec<String> {
        split_rust_top_level(args, ',').iter().map(|a| trim(a).to_string()).filter(|a| !a.is_empty() && !a.starts_with('\'')).collect()
    };
    let std_ = |name: &str| !is_rust_project_named(name, ctx);
    if t.starts_with('[') {
        if !rust_group_is_whole(&t) || projection.is_some() {
            return None;
        }
        let first = split_rust_top_level(&t[1..t.len() - 1], ';').into_iter().next();
        return as_(first.as_deref());
    }
    if let Some(object) = re!(r"^(?:impl|dyn)\s+([\s\S]+)$").captures(&t) {
        if projection.is_some() {
            return None;
        }
        for bound in split_rust_top_level(object.text(1).unwrap_or(""), '+') {
            let Some(head) = rust_path_head(trim(&bound)) else { continue };
            if !RUST_ITERATOR_TRAITS.contains(&head.name.as_str()) {
                continue;
            }
            let item = type_args(&head.args)
                .iter()
                .find_map(|a| re!(r"^Item\s*=\s*([\s\S]+)$").captures(a).and_then(|c| c.text(1).map(String::from)));
            return as_(item.as_deref());
        }
        return None;
    }
    let head = rust_path_head(&t)?;
    if !std_(&head.name) {
        return None;
    }
    let args = type_args(&head.args);
    if RUST_STD_SEQUENCES.contains(&head.name.as_str()) {
        return if projection.is_some() || args.len() != 1 { None } else { as_(Some(&args[0])) };
    }
    let pair = |args: &[String]| -> Option<RustWrittenType> {
        match projection {
            Some(Projection::Key) => as_(Some(&args[0])),
            Some(Projection::Value) => as_(Some(&args[1])),
            _ => as_(Some(&format!("({}, {})", args[0], args[1]))),
        }
    };
    if RUST_STD_MAPS.contains(&head.name.as_str()) {
        if args.len() < 2 {
            return None;
        }
        return pair(&args);
    }
    let iterator = match head.name.as_str() {
        "Iter" | "IterMut" | "IntoIter" | "Drain" => "entry",
        "Keys" | "IntoKeys" => "key",
        "Values" | "ValuesMut" | "IntoValues" => "value",
        _ => return None,
    };
    if iterator == "key" || iterator == "value" {
        return if projection.is_some() || args.len() != 2 { None } else { as_(Some(&args[if iterator == "key" { 0 } else { 1 }])) };
    }
    if args.len() == 1 {
        return if projection.is_some() { None } else { as_(Some(&args[0])) };
    }
    if args.len() == 2 {
        return pair(&args);
    }
    None
}

fn rust_site_type(
    site: &RustPatternSite,
    scrutinee: Option<&str>,
    at: usize,
    scopes: &RustScopes,
    r: &UnresolvedRef,
    ctx: &Ctx,
    depth: u32,
) -> Option<RustWrittenType> {
    if site.path.is_empty() {
        let (index, scrutinee) = (site.index?, scrutinee.filter(|s| !s.is_empty())?);
        let value = rust_value_type(scrutinee, at, scopes, r, ctx, depth + 1)?;
        return rust_tuple_field(&value, index);
    }
    let segments: Vec<&str> = site.path.split("::").collect();
    let last = segments[segments.len() - 1];
    if segments.len() == 1 && (last == "Some" || last == "Ok" || last == "Err") {
        if site.index != Some(0) {
            return None;
        }
        let scrutinee = scrutinee.filter(|s| !s.is_empty())?;
        let value = rust_value_type(scrutinee, at, scopes, r, ctx, depth + 1)?;
        let head = rust_path_head(&strip(re!(r"^&\s*(?:'\w+\s+)?(?:mut\s+)?"), &value.text))?;
        let args = split_rust_top_level(&head.args, ',');
        if last == "Some" && head.name == "Option" && args.len() == 1 {
            return Some(RustWrittenType { text: args[0].clone(), ..value });
        }
        if last == "Ok" && head.name == "Result" && !args.is_empty() {
            return Some(RustWrittenType { text: args[0].clone(), ..value });
        }
        if last == "Err" && head.name == "Result" && args.len() == 2 {
            return Some(RustWrittenType { text: args[1].clone(), ..value });
        }
        return None;
    }
    let decl = rust_pattern_declaration(&segments, at, r, ctx)?;
    let text = match (site.index, &site.field) {
        (Some(i), _) => decl.fields.tuple.as_ref().and_then(|t| t.get(i)).cloned(),
        (None, Some(f)) => decl.fields.named.as_ref().and_then(|n| n.get(f)).cloned(),
        _ => None,
    }
    .filter(|t| !t.is_empty())?;
    Some(RustWrittenType { text, file: decl.file, at: decl.at, self_: Some(decl.self_), generics: Some(decl.generics), ..Default::default() })
}

struct PatternDeclaration {
    fields: Rc<RustFields>,
    file: String,
    at: usize,
    self_: String,
    generics: String,
}

fn rust_pattern_declaration(segments: &[&str], at: usize, r: &UnresolvedRef, ctx: &Ctx) -> Option<PatternDeclaration> {
    let last = segments[segments.len() - 1];
    if segments.len() >= 2 {
        let owner = rust_read_type(&RustWrittenType::plain(segments[..segments.len() - 1].join("::"), r.file_path.clone(), at), r, ctx);
        let RustTypeRead::Read(RustReadBody { kind: RustReadKind::Named { name, path, .. }, written, .. }) = owner else { return None };
        let enums: Vec<NodeRef> = rust_type_lookup(&name, &path, &written.file, ctx, Some(written.at))
            .nodes
            .into_iter()
            .filter(|n| n.kind == NodeKind::Enum)
            .collect();
        if enums.len() != 1 {
            return None;
        }
        let enum_node = &enums[0];
        let want = format!("{}::{last}", enum_node.qualified_name);
        let variant = ctx.get_nodes_in_file_named(&enum_node.file_path, last).into_iter().find(|n| n.kind == NodeKind::EnumMember && n.qualified_name == want)?;
        let fields = rust_declared_fields(&variant, ctx, false)?;
        let generics = rust_declared_fields(enum_node, ctx, true).map(|f| f.generics.clone()).unwrap_or_default();
        return Some(PatternDeclaration {
            fields,
            file: variant.file_path.clone(),
            at: rust_node_offset(&variant, ctx),
            self_: enum_node.name.clone(),
            generics,
        });
    }
    let structs: Vec<NodeRef> =
        rust_type_lookup(last, &[], &r.file_path, ctx, Some(at)).nodes.into_iter().filter(|n| n.kind == NodeKind::Struct).collect();
    if structs.len() != 1 {
        return None;
    }
    let fields = rust_declared_fields(&structs[0], ctx, false)?;
    let generics = fields.generics.clone();
    Some(PatternDeclaration {
        fields,
        file: structs[0].file_path.clone(),
        at: rust_node_offset(&structs[0], ctx),
        self_: structs[0].name.clone(),
        generics,
    })
}

/// A struct's (or enum variant's) fields as written.
#[derive(Debug, Default)]
pub struct RustFields {
    pub tuple: Option<Vec<String>>,
    pub named: Option<HashMap<String, String>>,
    pub generics: String,
}

memo_map!(DeclaredFieldsMemo, String, Option<Rc<RustFields>>);

pub fn rust_declared_fields(decl: &Node, ctx: &Ctx, generics_only: bool) -> Option<Rc<RustFields>> {
    let key = format!("{}{}", decl.id, if generics_only { "\0g" } else { "" });
    memoized::<DeclaredFieldsMemo>(ctx, key, || read_rust_declared_fields(decl, ctx, generics_only).map(Rc::new))
}

fn read_rust_declared_fields(decl: &Node, ctx: &Ctx, generics_only: bool) -> Option<RustFields> {
    let scopes = rust_scopes_of(&decl.file_path, ctx)?;
    let code = scopes.code.as_str();
    let bytes = code.as_bytes();
    let start = rust_node_offset(decl, ctx);
    let name = Re::cached(&format!(r"\b{}\b", escape_regex(&decl.name)), "").find(code_slice(&scopes, start, start + 512))?;
    let mut p = start + name.start() + decl.name.len();
    while p < bytes.len() && is_js_space(bytes[p] as char) {
        p += 1;
    }
    let mut generics = String::new();
    if bytes.get(p) == Some(&b'<') {
        let end = rust_angle_end(code_slice(&scopes, p, p + 2000), 0)?;
        generics = code_slice(&scopes, p + 1, p + end).to_string();
        p += end + 1;
    }
    if generics_only {
        return Some(RustFields { generics, ..Default::default() });
    }
    let mut open = p;
    while open < bytes.len() && !matches!(bytes[open], b'(' | b'{' | b';' | b',' | b'}') {
        open += 1;
    }
    let bracket = bytes.get(open).copied();
    if bracket != Some(b'(') && bracket != Some(b'{') {
        return Some(RustFields { generics, ..Default::default() });
    }
    let mut depth = 0i32;
    let mut close = open;
    while close < bytes.len() {
        let c = bytes[close];
        if matches!(c, b'(' | b'[' | b'{') {
            depth += 1;
        } else if matches!(c, b')' | b']' | b'}') {
            depth -= 1;
            if depth == 0 {
                break;
            }
        }
        close += 1;
    }
    let field = |text: &str| -> String {
        let a = re!(r"#\s*\[[^\[\]]*(?:\[[^\[\]]*\][^\[\]]*)*\]", "g").replace_all(text, |_| Some(" ".to_string()));
        trim(&strip(re!(r"^\s*pub\s*(?:\([^)]*\))?"), &a)).to_string()
    };
    let parts: Vec<String> = split_rust_top_level(code_slice(&scopes, open + 1, close), ',').iter().map(|p| field(p)).collect();
    if bracket == Some(b'(') {
        return Some(RustFields { tuple: Some(parts), generics, ..Default::default() });
    }
    let mut named: HashMap<String, String> = HashMap::new();
    for part in &parts {
        if let Some(m) = re!(r"^([A-Za-z_]\w*)\s*:\s*([\s\S]+)$").captures(part) {
            named.insert(m.text(1).unwrap_or("").to_string(), trim(m.text(2).unwrap_or("")).to_string());
        }
    }
    Some(RustFields { named: Some(named), generics, ..Default::default() })
}

// -- Methods and traits --------------------------------------------------------------------

memo_map!(TraitOfMemo, String, Option<NodeRef>);

/// The trait a method is declared in (a method of a `trait` body), if any.
pub fn rust_trait_of(method: &Node, ctx: &Ctx) -> Option<NodeRef> {
    memoized::<TraitOfMemo>(ctx, method.id.clone(), || {
        let owner = rust_owner_of(&method.qualified_name);
        if owner.is_empty() {
            return None;
        }
        ctx.get_nodes_in_file_named(&method.file_path, owner)
            .into_iter()
            .find(|n| n.kind == NodeKind::Trait && n.name == owner && n.start_line <= method.start_line && n.end_line >= method.start_line)
    })
}

pub fn rust_methods_named(name: &str, method: &str, ctx: &Ctx) -> Vec<NodeRef> {
    ctx.get_method_matches(name, method, Language::Rust).iter().filter(|m| m.language == Language::Rust).cloned().collect()
}

/// Inherent (non-trait) methods named `method` on the type `name` of `lookup`.
pub fn rust_owned_methods(name: &str, method: &str, lookup: &RustTypeLookup, ctx: &Ctx) -> Vec<NodeRef> {
    let matches: Vec<NodeRef> = rust_methods_named(name, method, ctx).into_iter().filter(|m| rust_trait_of(m, ctx).is_none()).collect();
    let types: Vec<&NodeRef> = lookup.nodes.iter().filter(|n| RUST_OWNER_KINDS.contains(&n.kind)).collect();
    if types.is_empty() {
        return matches;
    }
    let namesakes = ctx
        .get_nodes_by_name(name)
        .iter()
        .filter(|n| n.language == Language::Rust && n.name == name && RUST_OWNER_KINDS.contains(&n.kind))
        .count();
    if namesakes <= types.len() {
        return matches;
    }
    let ids: HashSet<&String> = types.iter().map(|t| &t.id).collect();
    let files: HashSet<&String> = types.iter().map(|t| &t.file_path).collect();
    matches
        .into_iter()
        .filter(|m| {
            files.contains(&m.file_path)
                || rust_type_lookup(name, &[], &m.file_path, ctx, Some(rust_node_offset(m, ctx))).nodes.iter().any(|n| ids.contains(&n.id))
        })
        .collect()
}

#[allow(dead_code)]
fn _keep() {
    let _ = lines_of;
}
