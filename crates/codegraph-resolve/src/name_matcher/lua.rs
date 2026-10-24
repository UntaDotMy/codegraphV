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
    match lua_alias_of(&decl, ctx, 0) {
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

fn lua_alias_of(decl: &Node, ctx: &Ctx, depth: u32) -> Tri<NodeRef> {
    let signature = decl.signature.as_deref().unwrap_or("");
    let rhs = re!(r"^\s*=\s*").replace_first(signature, |_| String::new());
    let site = LuaSite { file: decl.file_path.clone(), line: decl.start_line, name: decl.name.clone(), node: Rc::new(decl.clone()) };
    lua_alias_expr(rhs.trim(), &site, ctx, depth)
}

fn lua_alias_expr(rhs: &str, site: &LuaSite, ctx: &Ctx, depth: u32) -> Tri<NodeRef> {
    if let Some(req) = Re::cached(LUA_REQUIRE_ALIAS, "").captures(rhs) {
        return lua_module_member(req.text(2).unwrap_or(""), &lua_members(req.text(3).unwrap_or("")), &site.node, ctx, depth);
    }
    let Some(named) = Re::cached(LUA_NAME_ALIAS, "").captures(rhs) else { return Tri::Undefined };
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
                return lua_module_member(module.text(2).unwrap_or(""), &members, &root_decl, ctx, depth);
            }
        }
        return Tri::Undefined;
    }
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

fn lua_module_member(name: &str, path: &[String], decl: &Node, ctx: &Ctx, depth: u32) -> Tri<NodeRef> {
    let Some(file) = lua_module_file(name, decl, ctx) else { return Tri::Null };
    if path.is_empty() {
        return Tri::Undefined;
    }
    let key = format!("{file}\0{}", path.join("."));
    let memo = ctx.memo::<LuaMemberMemo>();
    if let Some(hit) = super::util::MemoMap::lookup(&*memo, &key) {
        return hit;
    }
    // A cycle of re-exports names nothing.
    super::util::MemoMap::store(&*memo, key.clone(), Tri::Null);
    let found = lua_member_in(&file, path, ctx, depth);
    super::util::MemoMap::store(&*memo, key, found.clone());
    found
}

fn lua_member_in(file: &str, path: &[String], ctx: &Ctx, depth: u32) -> Tri<NodeRef> {
    let member = &path[path.len() - 1];
    let nodes = ctx.get_nodes_in_file(file);
    let in_file: Vec<&NodeRef> = nodes.iter().filter(|n| &n.name == member).collect();
    let fns: Vec<&NodeRef> = in_file.iter().copied().filter(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method)).collect();
    if !fns.is_empty() {
        let owner = if path.len() > 1 { Some(path[path.len() - 2].as_str()) } else { None };
        let owned = fns.iter().find(|n| owner.is_some_and(|o| n.qualified_name.ends_with(&format!("{o}::{member}"))));
        let chosen = owned.or_else(|| fns.iter().find(|n| n.kind == NodeKind::Method)).unwrap_or(&fns[0]);
        return Tri::Value((*chosen).clone());
    }
    if depth >= 3 {
        return Tri::Null;
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
                    return Tri::Value(func.clone());
                }
            }
            let at = nodes.iter().find(|n| n.kind == NodeKind::File).or_else(|| nodes.first());
            let next = match at {
                Some(at) => lua_alias_expr(&rhs, &LuaSite { file: file.to_string(), line, name: member.clone(), node: at.clone() }, ctx, depth + 1),
                None => Tri::Undefined,
            };
            if !next.is_undefined() {
                return next;
            }
        }
    }
    if let Some(alias) = in_file.iter().find(|n| n.kind == NodeKind::Variable && n.signature.as_deref().is_some_and(|s| !s.is_empty()) && is_lua_local(n, ctx)) {
        let next = lua_alias_of(alias, ctx, depth + 1);
        if !next.is_undefined() {
            return next;
        }
    }
    match in_file.iter().find(|n| n.kind == NodeKind::Variable && !is_lua_local(n, ctx)) {
        Some(global) => Tri::Value((*global).clone()),
        None => Tri::Null,
    }
}

fn lua_module_file(name: &str, decl: &Node, ctx: &Ctx) -> Option<String> {
    let r = UnresolvedRef::new(decl.id.clone(), name, codegraph_types::EdgeKind::Imports, decl.start_line, 0, decl.file_path.clone(), decl.language);
    let resolved = crate::import_resolver::resolve_import(&r, ctx)?;
    ctx.node_by_id(&resolved.target_node_id).map(|n| n.file_path.clone())
}

