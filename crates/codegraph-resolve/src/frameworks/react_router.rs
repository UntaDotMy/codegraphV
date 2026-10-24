//! React Router — routes declared in markup, navigation written as a string. Port of
//! `src/resolution/frameworks/react-router.ts`.
//!
//! `frameworks/react` already reads the route table out of the markup: `<Route path="/payment"
//! component={PaymentScreen}/>` (v5), `<Route path="/payment" element={<PaymentScreen/>}/>` (v6)
//! and `createBrowserRouter([{ path, element }])` (v6.4+) each become a `route` node named by its
//! path, bound to the component that renders it. This file is the other half.
//!
//! **Navigation is a string.** `history.push('/placeorder')` (v5, and the `useHistory` hook),
//! `navigate('/placeorder')` (v6's `useNavigate`), `router.navigate(…)` on a data router,
//! `redirect('/login')` from a loader or an action: the extractor records each as a call that
//! resolves to nothing, because the target is a path, not a symbol. `resolve()` claims those refs,
//! reads the argument off the source with the Expo Router readers, matches it against this
//! framework's own route table, and returns a **`navigates`** edge carrying the href.
//! `<Link to>` and `<Navigate to>` are JSX attributes rather than calls, so a synthesizer reads
//! them instead.
//!
//! Precision rests on the string naming a real route: a computed path, a path no route serves,
//! and a conditional that forks are left unresolved rather than guessed. `push` and `replace` are
//! two of the most common method names in JavaScript, so the receiver has to name a router — a
//! bare `push` is an array's, and is never claimed.

use super::expo_router::{
    add_route_to, app_root_for, first_argument_text, nav_metadata, parse_href_expression, read_href_via_local, routes_for_file,
    RootedRouteTable, RouteTable,
};
use super::nextjs::destinations_for_href;
use super::package_deps::depends_on;
use super::react::{config_href_expression, is_table_route};
use crate::ctx::Ctx;
use crate::types::{AlsoTarget, FrameworkResolver, NavigationCalls, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::rc::Rc;

const ROUTE_LANGUAGES: [Language; 4] = [Language::TypeScript, Language::JavaScript, Language::Tsx, Language::Jsx];

// -- Route table — the routes `frameworks/react` read out of the markup --------------------------------

pub type ReactRouterTable = RootedRouteTable;

/// The app a route file belongs to — the shared rule (`app_root_for`).
pub fn react_router_root(file_path: &str) -> String {
    app_root_for(file_path)
}

/// True for a route node `frameworks/react` emitted, and no other.
///
/// Its id is a verbatim reconstruction of the node's own fields, which no other framework's route
/// id is: a server route carries its METHOD (`route:file:12:POST:/login`), a file-based page
/// carries no line. A path built from a constant keeps the id it was extracted with, and a route
/// read from a table another file hands the router says so (`…:12:table:/login`).
fn is_react_router_route(node: &Node) -> bool {
    let prefix = format!("route:{}:{}:", node.file_path, node.start_line);
    if (node.language != Language::Tsx && node.language != Language::Jsx) || !node.id.starts_with(&prefix) {
        return false;
    }
    if is_table_route(node) {
        return true;
    }
    // Its name, or — for a path built from a constant, renamed after extraction — the path as the
    // file wrote it; never a server route's `METHOD:`.
    let rest = &node.id[prefix.len()..];
    rest == node.name || (!re!(r"^[A-Z]+:").is_match(rest) && node.signature.as_deref().is_some_and(|s| s.starts_with("route-parts:")))
}

/// True for a route the table holds. A nested route's path is relative to its parent; without the
/// tree it is not a destination. A splat matches everything, so it answers nothing.
fn is_destination(node: &Node) -> bool {
    is_react_router_route(node) && node.name.starts_with('/') && !node.name.ends_with('*')
}

/// `:id?` — a parameter React Router serves the route with or without.
fn is_optional_param(seg: &str) -> bool {
    seg.starts_with(':') && seg.ends_with('?')
}

#[derive(Default)]
struct ReactRouterCache(RefCell<Option<Rc<ReactRouterTable>>>);

pub fn react_router_table(ctx: &Ctx) -> Rc<ReactRouterTable> {
    let all = ctx.get_nodes_by_kind(NodeKind::Route);
    let cache = ctx.memo::<ReactRouterCache>();
    if let Some(cached) = cache.0.borrow().as_ref() {
        if Rc::ptr_eq(&cached.source, &all) {
            return Rc::clone(cached);
        }
    }
    let mut by_root: IndexMap<String, RouteTable> = IndexMap::new();
    let mut shortened: Vec<(String, String, Rc<Node>)> = Vec::new();
    for node in all.iter() {
        if !is_destination(node) {
            continue;
        }
        let root = react_router_root(&node.file_path);
        let path: &str = if node.name.len() > 1 && node.name.ends_with('/') { &node.name[..node.name.len() - 1] } else { &node.name };
        let table = by_root.entry(root.clone()).or_insert_with(|| RouteTable::new(all.clone()));
        add_route_to(table, path, node);
        // React Router's optional parameter: `/cart/:id?` is the screen for `/cart/5` AND for a
        // bare `/cart`, which the navbar's cart icon links to. The matcher pairs a route with an
        // href of the same length, so the shorter form is its own entry — collected now,
        // registered after every literal path, so a route someone actually wrote always wins.
        let mut segs: Vec<&str> = path.split('/').skip(1).collect();
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

// -- Navigation calls ---------------------------------------------------------------------------------------

/// `history.push` / `.replace` (v5, `useHistory`), `navigate(…)` (v6, `useNavigate`),
/// `router.navigate(…)` (a data router), `redirect(…)` (a loader or an action).
///
/// The receiver is required for `push` / `replace`: an unqualified `push` is an array's, and
/// claiming it would put every `paths.push('/tmp/x')` in the repo one string-match away from a
/// route.
fn nav_call_re() -> &'static Re {
    re!(r"^(?:history|navigate|router)\.(?:push|replace|navigate)$|^(?:navigate|redirect)$")
}

/// The verb a navigation call name stands for, or `None`.
pub fn react_router_nav_verb(name: &str) -> Option<String> {
    if !nav_call_re().is_match(name) {
        return None;
    }
    Some(match name.rfind('.') {
        None => name.to_string(),
        Some(dot) => name[dot + 1..].to_string(),
    })
}

// -- The resolver --------------------------------------------------------------------------------------------

pub struct ReactRouterResolver {
    nav: NavigationCalls,
}

impl ReactRouterResolver {
    pub fn new() -> Self {
        ReactRouterResolver {
            nav: NavigationCalls {
                tails: vec!["push", "replace", "navigate", "redirect"],
                // A call matches against the table of the app its file is in.
                scope: Box::new(|route, _| if is_destination(route) { Some(vec![react_router_root(&route.file_path)]) } else { None }),
            },
        }
    }
}

impl Default for ReactRouterResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameworkResolver for ReactRouterResolver {
    fn name(&self) -> &str {
        "react-router"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&ROUTE_LANGUAGES)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        depends_on(ctx, &["react-router", "react-router-dom", "react-router-native"])
    }

    fn claims_reference(&self, name: &str) -> bool {
        nav_call_re().is_match(name)
    }

    fn navigation(&self) -> Option<&NavigationCalls> {
        Some(&self.nav)
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        if !r.is_kind(EdgeKind::Calls) {
            return None;
        }
        let verb = react_router_nav_verb(&r.reference_name)?;
        if !ROUTE_LANGUAGES.contains(&r.language) {
            return None;
        }
        let table = react_router_table(ctx);
        let routes = routes_for_file(&table, &r.file_path)?;
        if routes.exact.is_empty() {
            return None;
        }
        let lines = ctx.get_file_lines(&r.file_path)?;

        let arg = first_argument_text(&lines, r.line, r.column, &verb)?;
        let mut href = parse_href_expression(&arg);
        // `navigate(paths.auth.login.getHref())`: a route-config object's href.
        if href.is_none() {
            if let Some(configured) = config_href_expression(&arg, &r.file_path, ctx) {
                href = parse_href_expression(&configured);
            }
        }
        if href.is_none() {
            let start = match ctx.node_by_id(&r.from_node_id) {
                Some(enclosing) if enclosing.file_path == r.file_path => enclosing.start_line,
                _ => r.line.saturating_sub(40).max(1),
            };
            href = read_href_via_local(&lines, r.line, r.column, &verb, start);
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
                    .map(|t| AlsoTarget { target_node_id: t.node.id.clone(), metadata: Some(nav_metadata(&t.href.display, &verb, None)) })
                    .collect(),
            );
        }
        resolved.edge_kind = Some(EdgeKind::Navigates);
        resolved.metadata = Some(nav_metadata(&target.href.display, &verb, None));
        Some(resolved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_router_receivers_are_navigation() {
        assert_eq!(react_router_nav_verb("history.push").as_deref(), Some("push"));
        assert_eq!(react_router_nav_verb("navigate").as_deref(), Some("navigate"));
        assert_eq!(react_router_nav_verb("redirect").as_deref(), Some("redirect"));
        assert_eq!(react_router_nav_verb("paths.push"), None);
        assert_eq!(react_router_nav_verb("push"), None);
    }

    #[test]
    fn routes_are_recognised_by_their_own_id() {
        let mut node = Node::new("route:src/App.js:12:/login", NodeKind::Route, "/login", "q", "src/App.js", Language::Jsx, 12, 12, 0, 0, 0);
        assert!(is_destination(&node));
        node.name = "/admin/*".to_string();
        assert!(!is_destination(&node));
        let server = Node::new("route:src/App.js:12:POST:/login", NodeKind::Route, "POST /login", "q", "src/App.js", Language::Jsx, 12, 12, 0, 0, 0);
        assert!(!is_destination(&server));
        let nested = Node::new("route:src/App.js:12:team", NodeKind::Route, "team", "q", "src/App.js", Language::Jsx, 12, 12, 0, 0, 0);
        assert!(!is_destination(&nested));
    }
}
