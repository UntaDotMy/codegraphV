//! Lua: a bare call through a `local` alias, followed to what the alias names.
//!
//! ```lua
//! local splitn = require("kong.tools.string").splitn   -- the module's `splitn`
//! local select_listener = utils.select_listener        -- through `local utils = require …`
//! local fmt = string.format / local type = type        -- the standard library's: no edge
//! ```
//!
//! `Tri::Undefined` when the call is not through such an alias (it resolves as before),
//! `Tri::Null` when the alias names nothing in the project.

use super::method::shares_receiver_word;
use super::reach::is_lua_local;
use super::util::{lines_of, memo_map};
use crate::ctx::Ctx;
use crate::types::{NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_search::is_test_path;
use codegraph_types::{Language, Node, NodeKind};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::LazyLock;

pub static LUA_LIBRARY_TABLES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "string", "table", "math", "io", "os", "coroutine", "debug", "utf8", "package", "bit", "bit32", "jit", "ffi", "vim", "ngx", "assert", "spy", "stub",
        "mock", "love",
    ]
    .into_iter()
    .collect()
});

pub static LUA_GLOBAL_FUNCTIONS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "assert", "error", "ipairs", "pairs", "next", "type", "tostring", "tonumber", "setmetatable", "getmetatable", "rawget", "rawset", "rawequal", "rawlen",
        "select", "pcall", "xpcall", "unpack", "print", "load", "loadstring", "loadfile", "dofile", "collectgarbage", "require", "setfenv", "getfenv", "newproxy",
        "typeof", "warn", "tick", "wait", "describe", "it", "before_each", "after_each", "setup", "teardown", "lazy_setup", "lazy_teardown", "pending", "finally",
        "insulate", "expose",
    ]
    .into_iter()
    .collect()
});

const LUA_REQUIRE_ALIAS: &str =
    r#"^(?:require|[A-Za-z_]\w*(?:[Rr]equire|_module|[Ii]mport))\s*\(?\s*(["'])([^"']+)\1\s*\)?((?:\s*\.\s*[A-Za-z_]\w*)*)\s*$"#;
const LUA_NAME_ALIAS: &str = r"^([A-Za-z_]\w*)((?:\s*\.\s*[A-Za-z_]\w*)*)\s*$";

struct LuaSite {
    file: String,
    line: u32,
    name: String,
    node: NodeRef,
}

fn lua_members(chain: &str) -> Vec<String> {
    chain.split('.').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

memo_map!(LuaMemberMemo, String, Tri<NodeRef>);

pub fn lua_alias_target(r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    let Some(decl) = lua_local_decl(&r.reference_name, &r.file_path, r.line, ctx) else { return Tri::Undefined };
    match lua_alias_of(&decl, ctx) {
        Tri::Undefined => Tri::Undefined,
        Tri::Null => Tri::Null,
        Tri::Value(target) => Tri::Value(ResolvedRef::new(r, target.id.clone(), 0.9, ResolvedBy::Import)),
    }
}

fn lua_local_decl(name: &str, file: &str, line: u32, ctx: &Ctx) -> Option<NodeRef> {
    let nodes = ctx.get_nodes_in_file(file);
    let fns: Vec<&NodeRef> = nodes.iter().filter(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method)).collect();
    let mut best: Option<NodeRef> = None;
    for n in nodes.iter() {
        if n.name != name || n.kind != NodeKind::Variable || n.start_line > line || n.signature.as_deref().map_or(true, str::is_empty) {
            continue;
        }
        if fns.iter().any(|f| f.start_line <= n.start_line && n.start_line <= f.end_line && !(f.start_line <= line && line <= f.end_line)) {
            continue;
        }
        if !is_lua_local(n, ctx) {
            continue;
        }
        if best.as_ref().map_or(true, |b| n.start_line > b.start_line) {
            best = Some(n.clone());
        }
    }
    let mut enclosing: Vec<&NodeRef> = fns.iter().copied().filter(|f| f.start_line <= line && line <= f.end_line && f.start_line < line).collect();
    enclosing.sort_by_key(|f| f.end_line - f.start_line);
    if let Some(func) = enclosing.first() {
        if best.as_ref().map_or(true, |b| b.start_line < func.start_line) {
            let lines = lines_of(ctx, file);
            let decl = Re::cached(&format!(r"^\s*local\s+{}\s*=\s*(.+?)\s*$", escape_regex(name)), "");
            let mut at = line as i64 - 1;
            while at > func.start_line as i64 {
                let text = lines.get((at - 1) as usize).map(String::as_str).unwrap_or("");
                if let Some(m) = decl.captures(text) {
                    let Some(file_node) = nodes.iter().find(|n| n.kind == NodeKind::File) else { break };
                    let mut synthetic = (**file_node).clone();
                    synthetic.name = name.to_string();
                    synthetic.kind = NodeKind::Variable;
                    synthetic.signature = Some(format!("= {}", m.text(1).unwrap_or("")));
                    synthetic.start_line = at as u32;
                    synthetic.end_line = at as u32;
                    return Some(Rc::new(synthetic));
                }
                at -= 1;
            }
        }
    }
    best
}

fn lua_alias_of(decl: &Node, ctx: &Ctx) -> Tri<NodeRef> {
    lua_follow(lua_alias_of_step(decl, ctx), ctx)
}

fn lua_alias_of_step(decl: &Node, ctx: &Ctx) -> LuaStep {
    let signature = decl.signature.as_deref().unwrap_or("");
    let rhs = re!(r"^\s*=\s*").replace_first(signature, |_| String::new());
    let site = LuaSite { file: decl.file_path.clone(), line: decl.start_line, name: decl.name.clone(), node: Rc::new(decl.clone()) };
    lua_alias_step(rhs.trim(), &site, ctx)
}

/// One step of a Lua alias: what it names, or the module member it leads to, which the walk in
/// [`lua_module_member`] takes next. Steps rather than calls keep a chain of re-exports from taking
/// a stack frame per module (upstream 329dd12: 2,000 modules overflowed).
enum LuaStep {
    Done(Tri<NodeRef>),
    Member { name: String, path: Vec<String>, decl: NodeRef },
}

impl LuaStep {
    fn is_no_step(&self) -> bool {
        matches!(self, LuaStep::Done(t) if t.is_undefined())
    }
}

fn lua_follow(step: LuaStep, ctx: &Ctx) -> Tri<NodeRef> {
    match step {
        LuaStep::Done(t) => t,
        LuaStep::Member { name, path, decl } => lua_module_member(&name, path, &decl, ctx),
    }
}

/// `require(name)` from `decl`'s file, then `path`: a member to walk to, or, with no path, the
/// module itself.
fn lua_member_step(name: &str, path: Vec<String>, decl: &NodeRef, ctx: &Ctx) -> LuaStep {
    // `require "kong.conf_loader"` called directly: the module's returned value.
    if path.is_empty() {
        return LuaStep::Done(if lua_module_file(name, decl, ctx).is_some() { Tri::Undefined } else { Tri::Null });
    }
    LuaStep::Member { name: name.to_owned(), path, decl: decl.clone() }
}

fn lua_alias_step(rhs: &str, site: &LuaSite, ctx: &Ctx) -> LuaStep {
    if let Some(req) = Re::cached(LUA_REQUIRE_ALIAS, "").captures(rhs) {
        return lua_member_step(req.text(2).unwrap_or(""), lua_members(req.text(3).unwrap_or("")), &site.node, ctx);
    }
    let Some(named) = Re::cached(LUA_NAME_ALIAS, "").captures(rhs) else { return LuaStep::Done(Tri::Undefined) };
    let root = named.text(1).unwrap_or("").to_string();
    let path = lua_members(named.text(2).unwrap_or(""));
    let root_decl = if root == site.name { None } else { lua_local_decl(&root, &site.file, site.line.saturating_sub(1), ctx) };
    if let Some(root_decl) = root_decl {
        let signature = root_decl.signature.as_deref().unwrap_or("");
        let text = re!(r"^\s*=\s*").replace_first(signature, |_| String::new());
        if let Some(module) = Re::cached(LUA_REQUIRE_ALIAS, "").captures(text.trim()) {
            if !path.is_empty() {
                let mut members = lua_members(module.text(3).unwrap_or(""));
                members.extend(path);
                return lua_member_step(module.text(2).unwrap_or(""), members, &root_decl, ctx);
            }
        }
        return LuaStep::Done(Tri::Undefined);
    }
    LuaStep::Done(lua_global_alias(&root, &path, site, ctx))
}

/// An alias of a name no local in reach holds: a host table's method, a library function the
/// project patches, or nothing of the project's.
fn lua_global_alias(root: &str, path: &[String], site: &LuaSite, ctx: &Ctx) -> Tri<NodeRef> {
    let root = root.to_string();
    if path.is_empty() {
        return if LUA_GLOBAL_FUNCTIONS.contains(root.as_str()) { Tri::Null } else { Tri::Undefined };
    }
    if !LUA_LIBRARY_TABLES.contains(root.as_str()) {
        let member = path[path.len() - 1].clone();
        let holder = if path.len() > 1 { path[path.len() - 2].clone() } else { root.clone() };
        let owned: Vec<NodeRef> = ctx
            .get_nodes_by_name(&member)
            .iter()
            .filter(|n| {
                matches!(n.language, Language::Lua | Language::Luau)
                    && n.kind == NodeKind::Method
                    && shares_receiver_word(&holder, &n.qualified_name)
                    && !(is_test_path(&n.file_path) && !is_test_path(&site.file))
            })
            .cloned()
            .collect();
        return if owned.len() == 1 { Tri::Value(owned[0].clone()) } else { Tri::Undefined };
    }
    if path.len() != 1 {
        return Tri::Undefined;
    }
    let patched: Vec<NodeRef> = ctx
        .get_nodes_by_name(&path[0])
        .iter()
        .filter(|n| {
            matches!(n.kind, NodeKind::Function | NodeKind::Method)
                && re!(r"::|\.").split(&n.qualified_name).into_iter().next() == Some(root.as_str())
                && (n.file_path == site.file || !is_test_path(&n.file_path))
        })
        .cloned()
        .collect();
    if patched.len() == 1 {
        Tri::Value(patched[0].clone())
    } else {
        Tri::Null
    }
}

/// `member` of the module `require(name)` returns, from `decl`'s file: its function of that name.
/// A member a module re-exports from another is followed to its end, in a loop: each module leads
/// to one place, so a chain has one answer wherever it is entered, which every module the walk
/// passes gets, and a chain that comes back to itself has none.
fn lua_module_member(name: &str, path: Vec<String>, decl: &NodeRef, ctx: &Ctx) -> Tri<NodeRef> {
    let mut walked: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut at = Some((name.to_owned(), path, decl.clone()));
    let mut found = Tri::Null;
    while let Some((name, path, decl)) = at.take() {
        let Some(file) = lua_module_file(&name, &decl, ctx) else {
            found = Tri::Null;
            break;
        };
        if path.is_empty() {
            found = Tri::Undefined;
            break;
        }
        let key = format!("{file}\0{}", path.join("."));
        if let Some(hit) = super::util::MemoMap::lookup(&*ctx.memo::<LuaMemberMemo>(), &key) {
            found = hit;
            break;
        }
        if !seen.insert(key.clone()) {
            found = Tri::Null;
            break;
        }
        walked.push(key);
        match lua_member_in(&file, &path, ctx) {
            LuaStep::Member { name, path, decl } => at = Some((name, path, decl)),
            LuaStep::Done(t) => found = t,
        }
    }
    for key in walked {
        super::util::MemoMap::store(&*ctx.memo::<LuaMemberMemo>(), key, found.clone());
    }
    found
}

/// What module `file` has as `path`: a function of its own, or the step to where it re-exports it from.
fn lua_member_in(file: &str, path: &[String], ctx: &Ctx) -> LuaStep {
    let member = &path[path.len() - 1];
    let nodes = ctx.get_nodes_in_file(file);
    let in_file: Vec<&NodeRef> = nodes.iter().filter(|n| &n.name == member).collect();
    let fns: Vec<&NodeRef> = in_file.iter().copied().filter(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method)).collect();
    if !fns.is_empty() {
        let owner = if path.len() > 1 { Some(path[path.len() - 2].as_str()) } else { None };
        let owned = fns.iter().find(|n| owner.is_some_and(|o| n.qualified_name.ends_with(&format!("{o}::{member}"))));
        let chosen = owned.or_else(|| fns.iter().find(|n| n.kind == NodeKind::Method)).unwrap_or(&fns[0]);
        return LuaStep::Done(Tri::Value((*chosen).clone()));
    }
    let raw = ctx.read_file(file).unwrap_or_default();
    let no_block = re!(r"--\[(=*)\[[\s\S]*?\]\1\]", "g").replace_all(&raw, |m| Some(m.text(0).unwrap_or("").chars().filter(|c| *c == '\n').collect::<String>()));
    let source = re!(r"--[^\n]*", "g").replace_all_str(&no_block, "");
    let exported = Re::cached(
        &format!(r"(?:^|[\s{{,.]){}\s*=\s*([A-Za-z_]\w*(?:\s*\.\s*[A-Za-z_]\w*)*)\s*(?:[,;}}]|$)", escape_regex(member)),
        "m",
    )
    .captures(&source);
    if let Some(exported) = exported {
        let rhs = exported.text(1).unwrap_or("").to_string();
        if &rhs != member {
            let start = exported.get(0).map(|m| m.start()).unwrap_or(0);
            let line = source[..start].split('\n').count() as u32 + 1;
            if re!(r"^[A-Za-z_]\w*$").is_match(&rhs) {
                if let Some(func) = nodes.iter().find(|n| n.name == rhs && matches!(n.kind, NodeKind::Function | NodeKind::Method)) {
                    return LuaStep::Done(Tri::Value(func.clone()));
                }
            }
            let at = nodes.iter().find(|n| n.kind == NodeKind::File).or_else(|| nodes.first());
            // A member to walk to always ends in an answer: only no step at all falls through.
            let next = match at {
                Some(at) => lua_alias_step(&rhs, &LuaSite { file: file.to_string(), line, name: member.clone(), node: at.clone() }, ctx),
                None => LuaStep::Done(Tri::Undefined),
            };
            if !next.is_no_step() {
                return next;
            }
        }
    }
    // The module re-exports an alias of its own (`local splitn = require(…).splitn`).
    if let Some(alias) = in_file.iter().find(|n| n.kind == NodeKind::Variable && n.signature.as_deref().is_some_and(|s| !s.is_empty()) && is_lua_local(n, ctx)) {
        let next = lua_alias_of_step(alias, ctx);
        if !next.is_no_step() {
            return next;
        }
    }
    LuaStep::Done(match in_file.iter().find(|n| n.kind == NodeKind::Variable && !is_lua_local(n, ctx)) {
        Some(global) => Tri::Value((*global).clone()),
        None => Tri::Null,
    })
}

fn lua_module_file(name: &str, decl: &Node, ctx: &Ctx) -> Option<String> {
    let r = UnresolvedRef::new(decl.id.clone(), name, codegraph_types::EdgeKind::Imports, decl.start_line, 0, decl.file_path.clone(), decl.language);
    let resolved = crate::import_resolver::resolve_import(&r, ctx)?;
    ctx.node_by_id(&resolved.target_node_id).map(|n| n.file_path.clone())
}

