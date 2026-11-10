//! A bare Rust name as the scope around it binds it, and the item a Rust path names: what a match
//! by name can't read. Port of `matchRustUseBoundName`, `rustPathTarget` and `isRustNameFromStd`
//! (upstream 4955eac9), plus a call through a type a path or a `use` names (upstream 7021de5c).

use super::impls::{is_rust_declared_type_kind, is_rust_trait_impl_method};
use super::types::rust_scopes_of;
use super::uses::*;
use crate::ctx::Ctx;
use crate::name_matcher::util::{
    after_col, before_col, line_at, memo_map, memoized, starts_with_at,
};
use crate::rust_scopes::{
    rust_binding_at, rust_fn_at, rust_inner_block_at, BindingKind, RustBinding,
};
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
    format!(
        "{}\0{}\0{}\0{}\0{}",
        r.file_path, r.line, r.column, r.from_node_id, r.reference_name
    )
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
        Some(|k| {
            matches!(
                k,
                NodeKind::Function
                    | NodeKind::Struct
                    | NodeKind::Union
                    | NodeKind::EnumMember
                    | NodeKind::Variable
                    | NodeKind::Constant
            )
        })
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
    let Some(kinds) = renamed_kinds(r) else {
        return Tri::Undefined;
    };
    if r.language != Language::Rust || !re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name) {
        return Tri::Undefined;
    }
    let name = r.reference_name.as_str();
    // Written bare where the reference is: no method's name, nor a path's first or last segment —
    // and a call's own name, with its arguments after it: a chained call is recorded where its
    // receiver starts, which may be a local of the method's name (`deadline.as_ref().deadline()`).
    let Some(line) = line_at(ctx, &r.file_path, r.line) else {
        return Tri::Undefined;
    };
    if !starts_with_at(&line, name, r.column)
        || re!(r"(?:\.|::)\s*$").is_match(before_col(&line, r.column))
    {
        return Tri::Undefined;
    }
    let after = js_trim_start(after_col(
        after_col(&line, r.column),
        utf16_len(name) as u32,
    ));
    let is_call = r.is_kind(EdgeKind::Calls);
    if if is_call {
        !re!(r"^(?:\(|::\s*<)").is_match(after)
    } else {
        re!(r"^(?:\.|::)").is_match(after)
    } {
        return Tri::Undefined;
    }
    let offset = rust_use_ref_offset(r, ctx);
    // A type rustfmt wrapped below its path — `h: &scheduler::\n    Handle` — is
    // the path's, not the `use`'s of its last name. Comments are blanked, so
    // `// crate::scheduler::` above the name is no path.
    if r.is_kind(EdgeKind::References)
        && re!(r"^\s*$").is_match(before_col(&line, r.column))
        && r.line > 1
    {
        let prev = ctx
            .get_file_lines(&r.file_path)
            .and_then(|lines| lines.get((r.line as usize).wrapping_sub(2)).cloned())
            .unwrap_or_default();
        if re!(r"::\s*(?://.*|/\*.*)?$").is_match(&prev) {
            let qualifier = rust_scopes_of(&r.file_path, ctx)
                .map(|scopes| rust_written_qualifier(&scopes.code, offset))
                .unwrap_or_default();
            if !qualifier.is_empty() {
                let path = format!("{}::{name}", qualifier.join("::"));
                let Some(target) = rust_path_target(
                    &UnresolvedRef {
                        reference_name: path,
                        ..r.clone()
                    },
                    ctx,
                ) else {
                    return Tri::Undefined;
                };
                ctx.memo::<RustScopedRefs>()
                    .0
                    .borrow_mut()
                    .insert(scoped_key(r));
                return Tri::Value(ResolvedRef::new(
                    r,
                    target.id.clone(),
                    0.9,
                    ResolvedBy::Import,
                ));
            }
        }
    }
    let visible = rust_visible_use(name, &r.file_path, offset, ctx);
    let local = if r.is_kind(EdgeKind::References) {
        None
    } else {
        rust_shadowing_local(name, r, offset, visible.as_ref(), ctx)
    };
    let target: Option<Rc<Node>>;
    if let Some(local) = local {
        // A `let` that holds a function by its path is that function: serde's `let test =
        // test::<i8>;` before its `test(…)` calls is the `test` the name finds anyway; `let take =
        // crate::other::take;` is other's.
        let held = if local.kind == BindingKind::Let {
            local.init.as_deref().map(|init| {
                re!(r"\s+", "g").replace_all_str(
                    &re!(r"\s*::\s*<[\s\S]*>$").replace_first(init, |_| String::new()),
                    "",
                )
            })
        } else {
            None
        };
        let Some(held) = held.filter(|h| re!(r"^(?:[A-Za-z_]\w*::)*[A-Za-z_]\w*$").is_match(h))
        else {
            return Tri::Null;
        };
        if held == name {
            return Tri::Undefined;
        }
        target = if held.contains("::") {
            rust_path_target(
                &UnresolvedRef {
                    reference_name: held,
                    ..r.clone()
                },
                ctx,
            )
        } else {
            None
        };
    } else {
        // A rename: a path whose last segment is not the name it binds.
        let renames = |p: &RustUsePath| p.segs.last().map(String::as_str) != Some(name);
        let Some(visible) =
            visible.filter(|v| v.paths.iter().any(renames) || v.std.iter().any(renames))
        else {
            return Tri::Undefined;
        };
        if visible.paths.is_empty() {
            return Tri::Null;
        }
        target = rust_paths_nodes(&visible.paths, &r.file_path, ctx)
            .and_then(|nodes| nodes.into_iter().find(|n| kinds(n.kind)));
    }
    let Some(target) = target.filter(|t| kinds(t.kind)) else {
        return Tri::Null;
    };
    ctx.memo::<RustScopedRefs>()
        .0
        .borrow_mut()
        .insert(scoped_key(r));
    Tri::Value(ResolvedRef::new(
        r,
        target.id.clone(),
        0.9,
        ResolvedBy::Import,
    ))
}

/// Whether `text` holds `name` as a word (`(?<![\w$])name(?![\w$])`).
fn has_word(text: &str, name: &str) -> bool {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$';
    text.match_indices(name).any(|(at, _)| {
        !text[..at].chars().next_back().is_some_and(word)
            && !text[at + name.len()..].chars().next().is_some_and(word)
    })
}

/// The local a bare name at `offset` of `r`'s function is there (rust_scopes reads the bindings): a
/// parameter, a `let`, a closure's, a pattern's — unless an item of the name is nearer, as a
/// block's own `use` (`fn f(take: fn()) { { use crate::util::take; take() } }`, and one in the body
/// itself, a scope inside the parameters') or a function the block declares. A nested `fn` sees no
/// local of the function around it. `None` for none.
fn rust_shadowing_local(
    name: &str,
    r: &UnresolvedRef,
    offset: usize,
    visible: Option<&RustVisibleUse>,
    ctx: &Ctx,
) -> Option<RustBinding> {
    let caller = ctx.node_by_id(&r.from_node_id)?;
    if caller.kind != NodeKind::Function && caller.kind != NodeKind::Method {
        return None;
    }
    // A local is written before the reference, in the function: the name's own text first.
    let lines = ctx.get_file_lines(&r.file_path).unwrap_or_default();
    let mut written = false;
    for l in caller.start_line..=r.line {
        let text = lines
            .get((l as usize).wrapping_sub(1))
            .map_or("", String::as_str);
        written = has_word(
            if l == r.line {
                before_col(text, r.column)
            } else {
                text
            },
            name,
        );
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
        let at = scopes
            .line_starts
            .get((n.start_line as usize).wrapping_sub(1))
            .copied()
            .unwrap_or(0)
            + n.start_column as usize;
        if rust_inner_block_at(&scopes, at)
            .is_some_and(|(from, to)| from >= local.from && from < offset && offset < to)
        {
            return None;
        }
    }
    Some(local)
}

/// The module path written right before `offset` in a Rust file's blanked code, read back over
/// whitespace and line breaks: `scheduler::` before `Handle` gives `["scheduler"]`. Empty when a
/// `::` follows no plain name (a turbofish's `>::`, a qualified self).
fn rust_written_qualifier(code: &str, offset: usize) -> Vec<String> {
    let bytes = code.as_bytes();
    let mut segs = Vec::new();
    let mut i = offset.min(bytes.len());
    loop {
        let mut j = i as isize - 1;
        while j >= 0 && bytes[j as usize].is_ascii_whitespace() {
            j -= 1;
        }
        if j < 1 || bytes[j as usize] != b':' || bytes[j as usize - 1] != b':' {
            break;
        }
        j -= 2;
        while j >= 0 && bytes[j as usize].is_ascii_whitespace() {
            j -= 1;
        }
        let mut k = j;
        while k >= 0 && (bytes[k as usize].is_ascii_alphanumeric() || bytes[k as usize] == b'_') {
            k -= 1;
        }
        let start = (k + 1) as usize;
        let end = (j + 1) as usize;
        if start > end || end > bytes.len() {
            return Vec::new();
        }
        let seg = &code[start..end];
        if !re!(r"^[A-Za-z_]\w*$").is_match(seg) {
            return Vec::new();
        }
        segs.insert(0, seg.to_string());
        i = start;
    }
    segs
}

/// Kinds a Rust path to a module's item names: a module-level `pub const` or `static` (extracted
/// as a variable) too. Never a method. The module's own items only ([`rust_declared_items`]), so
/// never an impl's `const`.
fn is_path_item_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Function
            | NodeKind::Struct
            | NodeKind::Union
            | NodeKind::Enum
            | NodeKind::Trait
            | NodeKind::TypeAlias
            | NodeKind::Constant
            | NodeKind::Variable
    )
}

memo_map!(RustPathTargetsMemo, String, Option<String>);

/// The item a Rust path written in `r`'s file names — `crate::util::take`, `super::Item` (from an
/// inline `mod tests { }` too), `util::take` through a module a `use` binds, a project crate's by
/// its name — walking the modules as Rust does from the one the path is written in
/// ([`rust_walk_modules`]), else, as a 2015 path, from its crate's root, to what the last module
/// declares, passes on (`pub use`) or globs in ([`rust_module_items`]): a function, a type, a
/// trait, a module-level `const` or `static` — and in a `use`, an enum's variant.
///
/// A call through a type its path or a `use` names (`sim::items::ItemPool::for_def`,
/// `ItemPool::for_def` under `use sim::items::ItemPool`) is that type's own function or variant
/// ([`rust_type_member`]). A type the module declares itself, `Self`, and a type's path in any
/// other position stay unresolved here. `None` when the path names no module the project holds, or
/// one that holds nothing by that name — and for one whose head a `use` takes from the standard
/// library (`fs::read` under `use std::fs;`), whatever module of that name the project has.
pub fn rust_path_target(r: &UnresolvedRef, ctx: &Ctx) -> Option<Rc<Node>> {
    let written = r
        .reference_name
        .strip_prefix("::")
        .unwrap_or(&r.reference_name);
    let segs: Vec<String> = written.split("::").map(String::from).collect();
    if segs.len() < 2 || !segs.iter().all(|s| re!(r"^[A-Za-z_]\w*$").is_match(s)) {
        return None;
    }
    let variants = r.is_kind(EdgeKind::Imports);
    let offset = rust_use_ref_offset(r, ctx);
    // Rust names its modules in snake_case and its types in CamelCase.
    let owner = segs[segs.len() - 2].as_str();
    let typed = !variants && owner.starts_with(|c: char| c.is_ascii_uppercase());
    let owner_use = if typed && segs.len() == 2 {
        rust_visible_use(owner, &r.file_path, offset, ctx)
    } else {
        None
    };
    if typed
        && (!r.is_kind(EdgeKind::Calls)
            || owner == "Self"
            || (segs.len() == 2 && owner_use.as_ref().is_none_or(|u| u.paths.is_empty())))
    {
        return None;
    }
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
    let open = from.block.map_or(-1, |b| {
        rust_uses_of(&r.file_path, ctx).blocks[b].open as i64
    });
    // A type a `use` binds is that `use`'s, wherever it is written.
    let scope = owner_use
        .as_ref()
        .filter(|u| !u.paths.is_empty())
        .map(|u| format!("{}\0{}", u.block, u.paths[0].at))
        .unwrap_or_default();
    let key = format!(
        "{}\0{open}\0{}\0{scope}\0{}",
        r.file_path,
        u8::from(variants),
        segs.join("::")
    );
    if let Some(hit) = memoized::<RustPathTargetsMemo>(ctx, key.clone(), || {
        let named = |path: &[String]| -> Option<Vec<Rc<Node>>> {
            // A type a `use` binds, written alone: the item that `use` names.
            if path.len() == 1 {
                let ids = rust_use_items_at(&path[0], r, ctx)?;
                return Some(ids.iter().filter_map(|id| ctx.node_by_id(id)).collect());
            }
            let mut items = rust_use_path_items(path, &from, ctx, &mut HashSet::new());
            // As a 2015 path, from the crate's root — unless the module declares the head
            // itself, where a `#[path]` puts it (a cfg's choice): another module of that name
            // at the root is not it.
            if items.is_none() && bare && !rust_uses_of(&from.file, ctx).path_mods.contains(head) {
                if let Some(root) = rust_crate_root_module(&r.file_path, ctx) {
                    items = rust_use_path_items(path, &root, ctx, &mut HashSet::new());
                }
            }
            items
        };
        if typed {
            let mut found = None;
            if let Some(types) = named(&segs[..segs.len() - 1]) {
                for type_node in types {
                    if let Some(member) = rust_type_member(&type_node, &segs[segs.len() - 1], ctx) {
                        found = Some(member.id.clone());
                        break;
                    }
                }
            }
            found
        } else {
            named(&segs).and_then(|v| {
                v.into_iter()
                    .find(|n| {
                        is_path_item_kind(n.kind) || (variants && n.kind == NodeKind::EnumMember)
                    })
                    .map(|n| n.id.clone())
            })
        }
    }) {
        return ctx.node_by_id(&hit);
    }
    None
}

/// Kinds a type's associated item is extracted as: an impl's functions, a `const`, a variant.
const RUST_MEMBER_KINDS: [NodeKind; 5] = [
    NodeKind::Method,
    NodeKind::Function,
    NodeKind::Variable,
    NodeKind::Constant,
    NodeKind::EnumMember,
];

memo_map!(RustTypeMembersMemo, String, Option<String>);

/// The associated item `member` of a Rust type, as `Type::member` reaches it: an enum's variant;
/// else the type's own (inherent) function or `const`, a trait impl's only when it is the one item
/// of that name, or a trait's own declared method. An impl of the type is written in the type's
/// crate, so a namesake type's impl in another crate is never it. `None` when none is.
fn rust_type_member(type_node: &Node, member: &str, ctx: &Ctx) -> Option<Rc<Node>> {
    if type_node.language != Language::Rust
        || !is_rust_declared_type_kind(type_node.kind)
        || type_node.kind == NodeKind::TypeAlias
    {
        return None;
    }
    let key = format!("{}\0{member}", type_node.id);
    let id = memoized::<RustTypeMembersMemo>(ctx, key, || {
        let mut found = if type_node.kind == NodeKind::Enum {
            rust_enum_variants(type_node, member, ctx)
                .into_iter()
                .next()
                .map(|n| n.id.clone())
        } else {
            None
        };
        if found.is_none() {
            let graph = rust_crate_graph(ctx);
            let crate_dir = rust_crate_dir_of(&type_node.file_path, &graph);
            let qualified = format!("{}::{member}", type_node.name);
            let all: Vec<Rc<Node>> = ctx
                .get_nodes_by_name(member)
                .iter()
                .filter(|n| {
                    n.language == Language::Rust
                        && RUST_MEMBER_KINDS.contains(&n.kind)
                        && n.qualified_name == qualified
                        && rust_crate_dir_of(&n.file_path, &graph) == crate_dir
                        && (type_node.kind != NodeKind::Trait
                            || (n.file_path == type_node.file_path
                                && n.start_line >= type_node.start_line
                                && n.start_line <= type_node.end_line))
                })
                .cloned()
                .collect();
            let inherent: Vec<Rc<Node>> = all
                .iter()
                .filter(|n| !is_rust_trait_impl_method(n, ctx))
                .cloned()
                .collect();
            let pool: Vec<Rc<Node>> = if !inherent.is_empty() {
                inherent
            } else if all.len() == 1 {
                all
            } else {
                Vec::new()
            };
            found = pool
                .iter()
                .find(|n| n.file_path == type_node.file_path)
                .or_else(|| pool.first())
                .map(|n| n.id.clone());
        }
        found
    });
    id.and_then(|id| ctx.node_by_id(&id))
}

/// Whether a bare Rust name where `r` is was brought in from the standard library: by the `use`
/// that binds it there ([`rust_visible_use`]), or with none, the file's last `use` of it — unless
/// the crate holds a module by the root's name, which a 2015 path names (`use core::Thing;` with a
/// `mod core;`).
pub fn is_rust_name_from_std(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let name = r.reference_name.as_str();
    let uses = rust_uses_of(&r.file_path, ctx);
    let std: RustUsePath =
        match rust_visible_use(name, &r.file_path, rust_use_ref_offset(r, ctx), ctx) {
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
                let mut all: Vec<&RustUsePath> = uses
                    .paths
                    .get(name)
                    .into_iter()
                    .flatten()
                    .chain(uses.std_paths.get(name).into_iter().flatten())
                    .collect();
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
