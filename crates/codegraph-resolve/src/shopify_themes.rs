//! Liquid `render`/`section` references onto theme files — port of `src/resolution/shopify-themes.ts`.

use crate::ctx::Ctx;
use crate::types::{ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_extract::languages::shopify_theme_root;
use codegraph_jsre::re;
use codegraph_types::{Language, NodeKind};
use regex::Regex;
use std::sync::LazyLock;

static SHOPIFY_THEME_FOLDERS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(assets|blocks|config|layout|locales|sections|snippets|templates)$").unwrap());

/// `Undefined` when the reference is not a theme-file reference, `Null` when it is but names no
/// indexed file.
pub fn match_shopify_theme_file(r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    if r.language != Language::Liquid || !re!(r"^(sections|snippets)/").is_match(&r.reference_name) {
        return Tri::Undefined;
    }
    let exists = |rel: &str| ctx.file_exists(rel);
    let Some(theme) = shopify_theme_root(&r.file_path, &exists, &SHOPIFY_THEME_FOLDERS) else { return Tri::Undefined };
    let target = if theme.is_empty() { r.reference_name.clone() } else { format!("{theme}/{}", r.reference_name) };
    let tail = &target[target.rfind('/').map_or(0, |i| i + 1)..];
    let file = ctx.get_nodes_by_name(tail).iter().find(|n| n.kind == NodeKind::File && n.file_path == target).cloned();
    match file {
        None => Tri::Null,
        Some(f) => Tri::Value(ResolvedRef::new(r, f.id.clone(), if theme.is_empty() { 0.95 } else { 0.85 }, ResolvedBy::FilePath)),
    }
}
