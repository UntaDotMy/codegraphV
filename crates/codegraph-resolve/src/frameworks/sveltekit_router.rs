//! SvelteKit — pages are directories, navigation is a string. Port of
//! `src/resolution/frameworks/sveltekit-router.ts`.
//!
//! `frameworks/svelte` already reads the route table out of the file tree:
//! `src/routes/article/[slug]/+page.svelte` is `/article/:slug`, `[[optional]]` is `:optional?` and
//! `[...rest]` is `*rest`. This file is the navigation half — without it a SvelteKit project's
//! screens are drawn as islands and the Screens tab stays hidden, because it is a picture of
//! `navigates` edges and there were none.
//!
//! Two calls carry a user from one page to another, and they do not agree on where the path goes:
//!
//! ```text
//! goto('/login')                     // $app/navigation, in the browser
//! redirect(303, '/article/' + slug)  // @sveltejs/kit, from a load or an action
//! ```
//!
//! `redirect` takes the status FIRST, so the destination is its second argument. Both are read with
//! the Expo Router readers and matched against this framework's own routes. `<a href="/login">` is
//! markup rather than a call, so a synthesizer reads it.
//!
//! Only `+page.svelte` is a screen. `+layout.svelte` and `+error.svelte` sit at the same path and
//! would be a second screen for one URL; `+server.ts` is an endpoint, not a page. A computed
//! destination, a path no page serves, and an external URL are left unresolved rather than guessed.

use super::expo_router::{
    add_route_to, app_root_for, nav_metadata, nth_argument_text, parse_href_expression, read_href_via_local, routes_for_file, RootedRouteTable,
    RouteTable,
};
use super::nextjs::destinations_for_href;
use super::package_deps::depends_on;
use crate::ctx::Ctx;
use crate::types::{AlsoTarget, FrameworkResolver, NavigationCalls, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::rc::Rc;

const NAV_LANGUAGES: [Language; 3] = [Language::TypeScript, Language::JavaScript, Language::Svelte];

// -- Route table — the `+page.svelte` files `frameworks/svelte` named ---------------------------------------

pub type SvelteKitTable = RootedRouteTable;

/// True for a page route node `frameworks/svelte` emitted, and no other.
fn is_svelte_kit_page(node: &Node) -> bool {
    node.language == Language::Svelte && node.file_path.ends_with("/+page.svelte") && node.id == format!("route:{}:{}:1", node.file_path, node.name)
}

/// True for a page the table holds. `[...rest]` becomes `*rest`, which matches anything — never an answer.
fn in_svelte_kit_table(node: &Node) -> bool {
    is_svelte_kit_page(node) && node.name.starts_with('/') && !node.name.contains('*')
}

/// `:id?` — a parameter SvelteKit serves the route with or without.
fn is_optional_param(seg: &str) -> bool {
    seg.starts_with(':') && seg.ends_with('?')
}

#[derive(Default)]
struct SvelteKitCache(RefCell<Option<Rc<SvelteKitTable>>>);

pub fn svelte_kit_table(ctx: &Ctx) -> Rc<SvelteKitTable> {
    let all = ctx.get_nodes_by_kind(NodeKind::Route);
    let cache = ctx.memo::<SvelteKitCache>();
    if let Some(cached) = cache.0.borrow().as_ref() {
        if Rc::ptr_eq(&cached.source, &all) {
            return Rc::clone(cached);
        }
    }
    let mut by_root: IndexMap<String, RouteTable> = IndexMap::new();
    let mut shortened: Vec<(String, String, Rc<Node>)> = Vec::new();
    for node in all.iter() {
        if !in_svelte_kit_table(node) {
            continue;
        }
        let root = app_root_for(&node.file_path);
        let table = by_root.entry(root.clone()).or_insert_with(|| RouteTable::new(all.clone()));
        add_route_to(table, &node.name, node);
        // `[[optional]]` is `:x?`: the route serves the path with and without it.
        let mut segs: Vec<&str> = node.name.split('/').skip(1).collect();
        while segs.len() > 1 && is_optional_param(segs[segs.len() - 1]) {
            segs.pop();
            shortened.push((root.clone(), format!("/{}", segs.join("/")), node.clone()));
        }
    }
    for (root, path, node) in &shortened {
        if let Some(t) = by_root.get_mut(root) {
            if !t.exact.contains_key(path) {
                add_route_to(t, path, node);
            }
        }
    }
    let table = Rc::new(RootedRouteTable { source: all, by_root });
    *cache.0.borrow_mut() = Some(Rc::clone(&table));
    table
}

// -- Navigation calls ------------------------------------------------------------------------------------------

/// Which argument of a navigation call is the destination — `redirect` puts the status first.
pub fn svelte_kit_href_argument(name: &str) -> Option<usize> {
    match name {
        "goto" => Some(0),
        "redirect" => Some(1),
        _ => None,
    }
}

// -- The resolver ------------------------------------------------------------------------------------------------

pub struct SvelteKitRouterResolver {
    nav: NavigationCalls,
}

impl SvelteKitRouterResolver {
    pub fn new() -> Self {
        SvelteKitRouterResolver {
            nav: NavigationCalls {
                tails: vec!["goto", "redirect"],
                // A call matches against the table of the app its file is in.
                scope: Box::new(|route, _| if in_svelte_kit_table(route) { Some(vec![app_root_for(&route.file_path)]) } else { None }),
            },
        }
    }
}

impl Default for SvelteKitRouterResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameworkResolver for SvelteKitRouterResolver {
    fn name(&self) -> &str {
        "sveltekit-router"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&NAV_LANGUAGES)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        depends_on(ctx, &["@sveltejs/kit"])
    }

    fn claims_reference(&self, name: &str) -> bool {
        svelte_kit_href_argument(name).is_some()
    }

    fn navigation(&self) -> Option<&NavigationCalls> {
        Some(&self.nav)
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        // `import { redirect } from '@sveltejs/kit'` is not a navigation.
        if !r.is_kind(EdgeKind::Calls) {
            return None;
        }
        let arg_index = svelte_kit_href_argument(&r.reference_name)?;
        if !NAV_LANGUAGES.contains(&r.language) {
            return None;
        }
        let table = svelte_kit_table(ctx);
        let routes = routes_for_file(&table, &r.file_path)?;
        if routes.exact.is_empty() {
            return None;
        }
        let lines = ctx.get_file_lines(&r.file_path)?;

        let arg = nth_argument_text(&lines, r.line, r.column, &r.reference_name, arg_index)?;
        let mut href = parse_href_expression(&arg);
        if href.is_none() && arg_index == 0 {
            let start = match ctx.node_by_id(&r.from_node_id) {
                Some(enclosing) if enclosing.file_path == r.file_path => enclosing.start_line,
                _ => r.line.saturating_sub(40).max(1),
            };
            href = read_href_via_local(&lines, r.line, r.column, &r.reference_name, start);
        }
        let href = href?;
        // Every arm of a conditional destination is somewhere this call goes; the first is this
        // reference's resolution and the rest ride as `alsoTargets`.
        let targets = destinations_for_href(&href, routes);
        let target = targets.first()?;
        let mut resolved = ResolvedRef::new(r, target.node.id.clone(), 0.95, ResolvedBy::Framework);
        if targets.len() > 1 {
            resolved.also_targets = Some(
                targets[1..]
                    .iter()
                    .map(|t| AlsoTarget { target_node_id: t.node.id.clone(), metadata: Some(nav_metadata(&t.href.display, &r.reference_name, None)) })
                    .collect(),
            );
        }
        resolved.edge_kind = Some(EdgeKind::Navigates);
        resolved.metadata = Some(nav_metadata(&target.href.display, &r.reference_name, None));
        Some(resolved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_takes_its_destination_second() {
        assert_eq!(svelte_kit_href_argument("goto"), Some(0));
        assert_eq!(svelte_kit_href_argument("redirect"), Some(1));
        assert_eq!(svelte_kit_href_argument("push"), None);
    }

    #[test]
    fn pages_are_recognised_by_their_own_id() {
        let page = Node::new("route:app/src/routes/+page.svelte:/:1", NodeKind::Route, "/", "q", "app/src/routes/+page.svelte", Language::Svelte, 1, 1, 0, 0, 0);
        assert!(in_svelte_kit_table(&page));
        let rest = Node::new("route:a/routes/x/+page.svelte:/x/*r:1", NodeKind::Route, "/x/*r", "q", "a/routes/x/+page.svelte", Language::Svelte, 1, 1, 0, 0, 0);
        assert!(!in_svelte_kit_table(&rest));
    }
}
