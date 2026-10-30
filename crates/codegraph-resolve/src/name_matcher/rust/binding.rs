//! A bare Rust name as the scope around it binds it, and the item a Rust path names: what a match
//! by name can't read. Port of `matchRustUseBoundName`, `rustPathTarget` and `isRustNameFromStd`
//! (upstream 4955eac9).

use super::types::rust_scopes_of;
use super::uses::*;
use crate::ctx::Ctx;
use crate::name_matcher::util::{after_col, before_col, line_at, memo_map, memoized, starts_with_at};
use crate::rust_scopes::{rust_binding_at, rust_fn_at, rust_inner_block_at, BindingKind, RustBinding};
use crate::types::{ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::jscompat::{js_trim_start, utf16_len};
use codegraph_types::{EdgeKind, Language, Node, NodeKind, ReferenceKind};
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

/// References [`match_rust_use_bound_name`] answered from the scope around them: a rename, a `let`
/// that holds a function. What the scope says the name is needs no other look at the name
/// ([`is_rust_scoped_ref`]).
#[derive(Default)]
struct RustScopedRefs(RefCell<HashSet<String>>);

fn scoped_key(r: &UnresolvedRef) -> String {
    format!("{}\0{}\0{}\0{}\0{}", r.file_path, r.line, r.column, r.from_node_id, r.reference_name)
}

/// Whether the scope around `r` already answered what its name is.
pub fn is_rust_scoped_ref(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let scoped = ctx.memo::<RustScopedRefs>();
    let set = scoped.0.borrow();
    !set.is_empty() && set.contains(&scoped_key(r))
}

/// What a bare Rust name a `use` renames may be, by the reference's kind.
fn renamed_kinds(r: &UnresolvedRef) -> Option<fn(NodeKind) -> bool> {
    if r.is_kind(EdgeKind::Calls) {
        Some(|k| matches!(k, NodeKind::Function | NodeKind::Struct | NodeKind::Union | NodeKind::EnumMember | NodeKind::Variable | NodeKind::Constant))
    } else if r.reference_kind == ReferenceKind::FunctionRef {
        Some(|k| k == NodeKind::Function)
    } else if r.is_kind(EdgeKind::References) {
        Some(|k| is_rust_module_item_kind(k) || k == NodeKind::EnumMember)
    } else {
        None
    }
}

/// A bare Rust name as the scope around it binds it, which a match by its name can't read:
/// - a call to a name a local binds there — a parameter, a `let`, a closure's parameter, nearer
///   than any `use` of the name — calls that local: `Null`;
/// - a name a `use` renames (`use crate::util::take as consume; consume()`) is the item its path
///   names, which no declaration carries by that name, or nothing: a rename of a std item, of one
///   the walk can't follow.
///
/// `Undefined` otherwise.
pub fn match_rust_use_bound_name(r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    let Some(kinds) = renamed_kinds(r) else { return Tri::Undefined };
    if r.language != Language::Rust || !re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name) {
        return Tri::Undefined;
    }
    let name = r.reference_name.as_str();
    // Written bare where the reference is: no method's name, nor a path's first or last segment —
    // and a call's own name, with its arguments after it: a chained call is recorded where its
    // receiver starts, which may be a local of the method's name (`deadline.as_ref().deadline()`).
    let Some(line) = line_at(ctx, &r.file_path, r.line) else { return Tri::Undefined };
    if !starts_with_at(&line, name, r.column) || re!(r"(?:\.|::)\s*$").is_match(before_col(&line, r.column)) {
        return Tri::Undefined;
    }
    let after = js_trim_start(after_col(after_col(&line, r.column), utf16_len(name) as u32));
    let is_call = r.is_kind(EdgeKind::Calls);
    if if is_call { !re!(r"^(?:\(|::\s*<)").is_match(after) } else { re!(r"^(?:\.|::)").is_match(after) } {
        return Tri::Undefined;
    }
    let offset = rust_use_ref_offset(r, ctx);
    let visible = rust_visible_use(name, &r.file_path, offset, ctx);
    let local = if r.is_kind(EdgeKind::References) { None } else { rust_shadowing_local(name, r, offset, visible.as_ref(), ctx) };
    let target: Option<Rc<Node>>;
    if let Some(local) = local {
        // A `let` that holds a function by its path is that function: serde's `let test =
        // test::<i8>;` before its `test(…)` calls is the `test` the name finds anyway; `let take =
        // crate::other::take;` is other's.
        let held = if local.kind == BindingKind::Let {
            local.init.as_deref().map(|init| re!(r"\s+", "g").replace_all_str(&re!(r"\s*::\s*<[\s\S]*>$").replace_first(init, |_| String::new()), ""))
        } else {
            None
        };
        let Some(held) = held.filter(|h| re!(r"^(?:[A-Za-z_]\w*::)*[A-Za-z_]\w*$").is_match(h)) else { return Tri::Null };
        if held == name {
            return Tri::Undefined;
        }
        target = if held.contains("::") { rust_path_target(&UnresolvedRef { reference_name: held, ..r.clone() }, ctx) } else { None };
    } else {
        // A rename: a path whose last segment is not the name it binds.
        let renames = |p: &RustUsePath| p.segs.last().map(String::as_str) != Some(name);
        let Some(visible) = visible.filter(|v| v.paths.iter().any(renames) || v.std.iter().any(renames)) else { return Tri::Undefined };
        if visible.paths.is_empty() {
            return Tri::Null;
        }
        target = rust_paths_nodes(&visible.paths, &r.file_path, ctx).and_then(|nodes| nodes.into_iter().find(|n| kinds(n.kind)));
    }
    let Some(target) = target.filter(|t| kinds(t.kind)) else { return Tri::Null };
    ctx.memo::<RustScopedRefs>().0.borrow_mut().insert(scoped_key(r));
    Tri::Value(ResolvedRef::new(r, target.id.clone(), 0.9, ResolvedBy::Import))
}

/// Whether `text` holds `name` as a word (`(?<![\w$])name(?![\w$])`).
fn has_word(text: &str, name: &str) -> bool {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$';
    text.match_indices(name).any(|(at, _)| {
        !text[..at].chars().next_back().is_some_and(word) && !text[at + name.len()..].chars().next().is_some_and(word)
    })
}

/// The local a bare name at `offset` of `r`'s function is there (rust_scopes reads the bindings): a
/// parameter, a `let`, a closure's, a pattern's — unless an item of the name is nearer, as a
/// block's own `use` (`fn f(take: fn()) { { use crate::util::take; take() } }`, and one in the body
/// itself, a scope inside the parameters') or a function the block declares. A nested `fn` sees no
/// local of the function around it. `None` for none.
fn rust_shadowing_local(name: &str, r: &UnresolvedRef, offset: usize, visible: Option<&RustVisibleUse>, ctx: &Ctx) -> Option<RustBinding> {
    let caller = ctx.node_by_id(&r.from_node_id)?;
    if caller.kind != NodeKind::Function && caller.kind != NodeKind::Method {
        return None;
    }
    // A local is written before the reference, in the function: the name's own text first.
    let lines = ctx.get_file_lines(&r.file_path).unwrap_or_default();
    let mut written = false;
    for l in caller.start_line..=r.line {
        let text = lines.get((l as usize).wrapping_sub(1)).map_or("", String::as_str);
        written = has_word(if l == r.line { before_col(text, r.column) } else { text }, name);
        if written {
            break;
        }
    }
    if !written {
        return None;
    }
    let scopes = rust_scopes_of(&r.file_path, ctx)?;
    let (local, _) = rust_binding_at(&scopes, name, offset)?;
    if rust_fn_at(&scopes, offset).is_some_and(|f| local.from < f.from) {
        return None;
    }
    if visible.is_some_and(|v| v.block >= local.from as isize) {
        return None;
    }
    // `fn f(take: fn()) { fn take() {} take() }`: the block's own item.
    for n in ctx.get_nodes_in_file_named(&r.file_path, name) {
        if n.kind != NodeKind::Function || n.qualified_name == name {
            continue;
        }
        let at = scopes.line_starts.get((n.start_line as usize).wrapping_sub(1)).copied().unwrap_or(0) + n.start_column as usize;
        if rust_inner_block_at(&scopes, at).is_some_and(|(from, to)| from >= local.from && from < offset && offset < to) {
            return None;
        }
    }
    Some(local)
}

/// Kinds a Rust path to a module's item names: never a method, nor a variable (an impl's `const`,
/// a `static`).
fn is_path_item_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Function | NodeKind::Struct | NodeKind::Union | NodeKind::Enum | NodeKind::Trait | NodeKind::TypeAlias | NodeKind::Constant)
}

memo_map!(RustPathTargetsMemo, String, Option<String>);

/// The item a Rust path written in `r`'s file names — `crate::util::take`, `super::Item` (from an
/// inline `mod tests { }` too), `util::take` through a module a `use` binds, a project crate's by
/// its name — walking the modules as Rust does from the one the path is written in
/// ([`rust_walk_modules`]), else, as a 2015 path, from its crate's root, to what the last module
/// declares, passes on (`pub use`) or globs in ([`rust_module_items`]): a function, a type, a
/// trait — and in a `use`, an enum's variant. Never a method, an `impl`'s or a trait's associated
/// item, nor a function's local item. `None` when the path names no module the project holds, or
/// one that holds nothing by that name — and for one whose head a `use` takes from the standard
/// library (`fs::read` under `use std::fs;`), whatever module of that name the project has.
pub fn rust_path_target(r: &UnresolvedRef, ctx: &Ctx) -> Option<Rc<Node>> {
    let written = r.reference_name.strip_prefix("::").unwrap_or(&r.reference_name);
    let segs: Vec<String> = written.split("::").map(String::from).collect();
    if segs.len() < 2 || !segs.iter().all(|s| re!(r"^[A-Za-z_]\w*$").is_match(s)) {
        return None;
    }
    let variants = r.is_kind(EdgeKind::Imports);
    // `Foo::new(…)`, `io::Error::new(…)`: through a type, by its name, to no module's item — only
    // a `use` names a variant that way. (Rust names its modules in snake_case.)
    if !variants && segs[segs.len() - 2].starts_with(|c: char| c.is_ascii_uppercase()) {
        return None;
    }
    let offset = rust_use_ref_offset(r, ctx);
    let from = rust_module_at(&r.file_path, offset, ctx);
    let head = segs[0].as_str();
    let bare = head != "crate" && head != "self" && head != "super";
    if bare {
        if let Some(bound) = rust_visible_use(head, &r.file_path, offset, ctx) {
            if !bound.std.is_empty() && bound.paths.is_empty() {
                return None;
            }
        }
    }
    let open = from.block.map_or(-1, |b| rust_uses_of(&r.file_path, ctx).blocks[b].open as i64);
    let key = format!("{}\0{open}\0{}\0{}", r.file_path, u8::from(variants), segs.join("::"));
    if let Some(hit) = memoized::<RustPathTargetsMemo>(ctx, key.clone(), || {
        let mut items = rust_use_path_items(&segs, &from, ctx, &mut HashSet::new());
        if items.is_none() && bare {
            if let Some(root) = rust_crate_root_module(&r.file_path, ctx) {
                items = rust_use_path_items(&segs, &root, ctx, &mut HashSet::new());
            }
        }
        items.and_then(|v| v.into_iter().find(|n| is_path_item_kind(n.kind) || (variants && n.kind == NodeKind::EnumMember))).map(|n| n.id.clone())
    }) {
        return ctx.node_by_id(&hit);
    }
    None
}

/// Whether a bare Rust name where `r` is was brought in from the standard library: by the `use`
/// that binds it there ([`rust_visible_use`]), or with none, the file's last `use` of it — unless
/// the crate holds a module by the root's name, which a 2015 path names (`use core::Thing;` with a
/// `mod core;`).
pub fn is_rust_name_from_std(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let name = r.reference_name.as_str();
    let uses = rust_uses_of(&r.file_path, ctx);
    let std: RustUsePath = match rust_visible_use(name, &r.file_path, rust_use_ref_offset(r, ctx), ctx) {
        Some(visible) => {
            if !visible.paths.is_empty() {
                return false;
            }
            match visible.std.last() {
                Some(std) => std.clone(),
                None => return false,
            }
        }
        None => {
            let mut all: Vec<&RustUsePath> = uses.paths.get(name).into_iter().flatten().chain(uses.std_paths.get(name).into_iter().flatten()).collect();
            all.sort_by_key(|p| p.at);
            match all.pop() {
                Some(last) if is_rust_std_root(&last.segs[0]) => last.clone(),
                _ => return false,
            }
        }
    };
    if std.segs.len() < 2 {
        return false;
    }
    let root = std.segs[0].as_str();
    let crate_root = rust_crate_root_module(&r.file_path, ctx);
    rust_submodule(&rust_module_at(&r.file_path, std.at, ctx), root, ctx, 0).is_none()
        && crate_root.is_none_or(|cr| rust_submodule(&cr, root, ctx, 0).is_none())
}
