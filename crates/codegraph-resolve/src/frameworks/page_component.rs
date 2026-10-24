//! A file-routed page — Nuxt's `pages/admin.vue`, Astro's `src/pages/about.astro` — is served by
//! the component its file IS: the single-file-component extractor's one component per file, named
//! after the file. Port of `src/resolution/frameworks/page-component.ts`.
//!
//! The route says so with a `calls` reference by that name, the way a Next.js page names its
//! default export, and `resolve_page_component` lands it on the SAME file's component — never on a
//! same-named one elsewhere, since every `index.vue` is a component named `index`.

use crate::ctx::Ctx;
use crate::types::{ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_types::{EdgeKind, Language, Node, NodeKind, ReferenceKind};

/// The component name a single-file-component extractor gives a file: its basename, extension off.
pub fn page_component_name(file_path: &str, extension: &str) -> String {
    let base = file_path.rsplit(['/', '\\']).next().unwrap_or(file_path);
    base.strip_suffix(extension).unwrap_or(base).to_string()
}

pub fn page_component_ref(route: &Node, extension: &str, language: Language) -> UnresolvedRef {
    let name = page_component_name(&route.file_path, extension);
    let mut r = UnresolvedRef::new(route.id.clone(), name.clone(), EdgeKind::Calls, 1, 0, route.file_path.clone(), language);
    r.candidates = Some(vec![name]);
    r
}

/// A page route's reference to its file's own component, or none when this is not one.
pub fn resolve_page_component(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    if r.reference_kind != ReferenceKind::Edge(EdgeKind::Calls) || !r.from_node_id.starts_with(&format!("route:{}:", r.file_path)) {
        return None;
    }
    let component = ctx.get_nodes_in_file(&r.file_path).iter().find(|n| n.kind == NodeKind::Component && n.name == r.reference_name).cloned()?;
    Some(ResolvedRef::new(r, component.id.clone(), 0.95, ResolvedBy::Framework))
}
