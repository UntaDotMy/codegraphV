//! Finding a symbol a module exports, following re-export chains (`findExportedSymbol` of
//! `import-resolver.ts`), plus the static-member and Rust `use` helpers next to it.

use super::exports::{
    common_js_default_export, esm_default_export, exported_by_name, get_file_export_index, has_esm_default_export,
    is_default_binding_kind, is_python_module_file, nodes_in_file_named, stated_default_export, StatedDefault,
};
use super::python::python_star_allows;
use super::{is_esm_import_language, resolve_import_path};
use crate::ctx::Ctx;
use crate::types::{NodeRef, ReExport, UnresolvedRef};
use codegraph_types::{EdgeKind, Language, NodeKind};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// How deep a re-export chain is followed.
pub const REEXPORT_MAX_DEPTH: usize = 8;

/// What an import asks a module for.
#[derive(Debug, Clone)]
pub struct Want {
    pub is_default: bool,
    pub is_namespace: bool,
    pub exported_name: String,
    pub member_name: Option<String>,
}

#[derive(Default)]
pub struct ExportedSymbolMemos(RefCell<HashMap<String, Option<NodeRef>>>);

/// Find the symbol `want` names in `file_path`, following `export … from` chains.
pub fn find_exported_symbol(
    file_path: &str,
    want: &Want,
    language: Language,
    ctx: &Ctx,
    visited: &mut HashSet<String>,
    depth: usize,
) -> Option<NodeRef> {
    if depth == 0 && visited.is_empty() {
        let memo = ctx.memo::<ExportedSymbolMemos>();
        let key = format!(
            "{file_path}\0{}{}\0{}\0{}\0{}",
            want.is_default as u8,
            want.is_namespace as u8,
            want.exported_name,
            want.member_name.as_deref().unwrap_or(""),
            language.as_str()
        );
        if let Some(hit) = memo.0.borrow().get(&key) {
            return hit.clone();
        }
        let result = find_exported_symbol_walk(file_path, want, language, ctx, visited, depth);
        memo.0.borrow_mut().insert(key, result.clone());
        return result;
    }
    find_exported_symbol_walk(file_path, want, language, ctx, visited, depth)
}

fn find_exported_symbol_walk(
    file_path: &str,
    want: &Want,
    language: Language,
    ctx: &Ctx,
    visited: &mut HashSet<String>,
    depth: usize,
) -> Option<NodeRef> {
    if depth > REEXPORT_MAX_DEPTH {
        return None;
    }
    if visited.contains(file_path) {
        return None;
    }
    visited.insert(file_path.to_string());

    let export_index = get_file_export_index(file_path, ctx);

    if want.is_default {
        // The file's own `export default` statement decides first: a default it leaves unnamed
        // has no declaration to link.
        if matches!(stated_default_export(file_path, &export_index, ctx), StatedDefault::Anonymous) {
            return None;
        }
        if let Some(direct) = esm_default_export(file_path, &export_index, ctx) {
            return Some(direct);
        }
        // CommonJS `module.exports = …` is the default of a `require`d module.
        if let Some(common_js) = common_js_default_export(file_path, ctx) {
            if let Some(source) = common_js.source.as_deref() {
                if let Some(next) = resolve_import_path(source, file_path, language, ctx) {
                    return find_exported_symbol(&next, want, language, ctx, visited, depth + 1);
                }
            } else if let Some(name) = common_js.name.as_deref() {
                let mut bound: Vec<NodeRef> = nodes_in_file_named(file_path, name, ctx)
                    .into_iter()
                    .filter(|n| is_default_binding_kind(n.kind))
                    .collect();
                bound.sort_by(|a, b| a.start_line.cmp(&b.start_line).then(a.start_column.cmp(&b.start_column)));
                if let Some(bound) = bound.into_iter().next() {
                    return Some(bound);
                }
            }
        }
    } else if want.is_namespace && want.member_name.is_some() {
        if let Some(direct) = exported_by_name(file_path, &export_index, want.member_name.as_deref().unwrap(), ctx) {
            return Some(direct);
        }
    } else {
        if let Some(direct) = exported_by_name(file_path, &export_index, &want.exported_name, ctx) {
            return Some(direct);
        }
        // `import { default as X }` is `import X`.
        if depth == 0
            && want.exported_name == "default"
            && is_esm_import_language(language)
            && has_esm_default_export(file_path, &export_index, ctx)
        {
            if let Some(esm_default) = esm_default_export(file_path, &export_index, ctx) {
                return Some(esm_default);
            }
        }
    }

    let re_exports = ctx.get_re_exports(file_path, language);
    if re_exports.is_empty() {
        return None;
    }

    let target_name: &str = if want.is_default {
        "default"
    } else if want.is_namespace && want.member_name.is_some() {
        want.member_name.as_deref().unwrap()
    } else {
        &want.exported_name
    };

    // Named re-exports first (the most specific), then `export * as ns`, then wildcards.
    for rex in re_exports.iter() {
        if let ReExport::Named { exported_name, original_name, source } = rex {
            if exported_name == target_name {
                let Some(next) = resolve_import_path(source, file_path, language, ctx) else { continue };
                let chained = find_exported_symbol(
                    &next,
                    &Want {
                        is_default: original_name == "default",
                        is_namespace: false,
                        exported_name: original_name.clone(),
                        member_name: None,
                    },
                    language,
                    ctx,
                    visited,
                    depth + 1,
                );
                if chained.is_some() {
                    return chained;
                }
            }
        }
    }

    // `import * as ns` of a barrel that re-exports `export * as inner from './inner'`:
    // `ns.inner.X` reads X from the module `inner` names.
    if want.is_namespace {
        if let Some(member) = want.member_name.as_deref() {
            let dot = member.find('.');
            let head = dot.map_or(member, |d| &member[..d]);
            let rex = dot.and_then(|_| {
                re_exports.iter().find_map(|r| match r {
                    ReExport::Namespace { exported_name, source } if exported_name == head => Some(source.clone()),
                    _ => None,
                })
            });
            if let Some(source) = rex {
                let next = resolve_import_path(&source, file_path, language, ctx);
                let rest = &member[dot.unwrap() + 1..];
                return next.and_then(|next| {
                    find_exported_symbol(
                        &next,
                        &Want {
                            exported_name: rest.split('.').next().unwrap_or("").to_string(),
                            member_name: Some(rest.to_string()),
                            ..want.clone()
                        },
                        language,
                        ctx,
                        visited,
                        depth + 1,
                    )
                });
            }
        }
    }

    for rex in re_exports.iter() {
        if let ReExport::Wildcard { source } = rex {
            let Some(next) = resolve_import_path(source, file_path, language, ctx) else { continue };
            // A Python `from x import *` brings in only what `x` lets out.
            if is_python_module_file(&next) && !want.is_default && !python_star_allows(&next, &want.exported_name, ctx) {
                continue;
            }
            let chained = find_exported_symbol(&next, want, language, ctx, visited, depth + 1);
            if chained.is_some() {
                return chained;
            }
        }
    }
    None
}

/// Node kinds that own static members.
pub fn is_static_member_container(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Class
            | NodeKind::Struct
            | NodeKind::Union
            | NodeKind::Interface
            | NodeKind::Enum
            | NodeKind::Trait
            | NodeKind::Protocol
    )
}

/// `Container.member` through an imported container: the member declared in the container's file.
pub fn resolve_static_member(container: &NodeRef, r: &UnresolvedRef, local_name: &str, ctx: &Ctx) -> Option<NodeRef> {
    if !is_static_member_container(container.kind) {
        return None;
    }
    let after = &r.reference_name[local_name.len() + 1..];
    let member = after.split('.').next().unwrap_or("");
    if member.is_empty() {
        return None;
    }
    let candidates: Vec<NodeRef> = ctx
        .get_nodes_by_qualified_name(&format!("{}::{member}", container.qualified_name))
        .iter()
        .filter(|n| n.file_path == container.file_path)
        .cloned()
        .collect();
    if candidates.is_empty() {
        return None;
    }
    if r.is_kind(EdgeKind::Calls) {
        if let Some(callable) = candidates.iter().find(|n| matches!(n.kind, NodeKind::Method | NodeKind::Function)) {
            return Some(Rc::clone(callable));
        }
    }
    candidates.into_iter().next()
}
