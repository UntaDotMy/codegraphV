//! TanStack Router — the path is a literal, and so is the destination. Port of
//! `src/resolution/frameworks/tanstack-router.ts`.
//!
//! Routes are declared two ways, and this reads both:
//!
//! ```text
//! // file-based (the plugin's default): the full path is the argument
//! export const Route = createFileRoute('/dashboard/invoices/$invoiceId')({
//!   component: InvoiceComponent,
//! })
//!
//! // code-based: a path per route, composed through its parent
//! const postsRoute = createRoute({ getParentRoute: () => rootRoute, path: 'posts' })
//! const postRoute  = createRoute({ getParentRoute: () => postsRoute, path: '$postId' })
//! ```
//!
//! Three things are TanStack's own, and each one decides whether the picture is right:
//!
//! 1. **A parameter is `$id`, not `:id`.** Route names are normalised to the `:id` every other
//!    framework here uses, so one matcher serves them all.
//! 2. **`to` is the route PATTERN, not a filled URL.** `<Link to="/posts/$postId" params={{ postId }}>`
//!    names the route and passes the values beside it — where React Router would write `/posts/5`.
//!    So a destination is normalised the same way a route name is, and then matches it exactly.
//! 3. **A destination is an object.** `navigate({ to: '/' })`, `throw redirect({ to: '/login' })` —
//!    the path is under a `to` key, and a `navigate({ search: … })` with no `to` stays on the page
//!    it is on.
//!
//! Not every route file is a page. A segment written `_auth` is a pathless layout — it does not
//! appear in the URL, and the file that declares it renders an outlet rather than a screen; a
//! `(group)` segment is likewise invisible; a `dashboard.route.tsx` is the layout for the
//! `/dashboard` subtree while `dashboard.index.tsx` — whose literal carries a trailing slash — is
//! the page AT `/dashboard`. Drawing both would put one address on the map twice.
//!
//! Left unresolved rather than guessed: a computed `to`, a path no route serves, and a code-based
//! route whose parent is declared in another file (the chain is composed within a file, which is
//! where a route tree is written).

use super::expo_router::{
    add_route_to, app_root_for, first_argument_text, nav_metadata, parse_href_expression, read_href_via_local, read_string_at, routes_for_file,
    to_href, HrefLiteral, RootedRouteTable, RouteTable,
};
use super::nextjs::destinations_for_href;
use super::object_literal::{match_bracket, read_fields};
use super::now_ms;
use super::package_deps::depends_on;
use crate::ctx::Ctx;
use crate::types::{AlsoTarget, FrameworkExtractionResult, FrameworkResolver, NavigationCalls, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_types::jscompat::{is_js_space, js_trim};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::rc::Rc;

const ROUTE_LANGUAGES: [Language; 4] = [Language::TypeScript, Language::JavaScript, Language::Tsx, Language::Jsx];

// -- Paths -----------------------------------------------------------------------------------------------------

/// A TanStack path in the form every other framework's routes take.
///
/// `$invoiceId` is `:invoiceId` and a bare `$` is a splat; a `_auth` segment is a pathless layout
/// and a `(group)` segment is a route group, neither of which appears in the URL; a trailing `_`
/// un-nests without changing the segment. Returns `None` for a path that names no address at all.
pub fn tanstack_path(raw: &str) -> Option<String> {
    if !raw.starts_with('/') {
        return None;
    }
    let mut segs: Vec<String> = Vec::new();
    for seg in raw.split('/') {
        if seg.is_empty() {
            continue;
        }
        if seg.starts_with('_') {
            continue; // pathless layout
        }
        if seg.starts_with('(') && seg.ends_with(')') {
            continue; // route group
        }
        let bare = seg.strip_suffix('_').unwrap_or(seg);
        if bare == "$" {
            segs.push(":splat*".to_string());
            continue;
        }
        segs.push(match bare.strip_prefix('$') {
            Some(rest) => format!(":{rest}"),
            None => bare.to_string(),
        });
    }
    Some(format!("/{}", segs.join("/")))
}

/// True when the literal names a pathless layout rather than a page.
///
/// `'/_auth'` is the layout file itself — it renders an outlet, at no address of its own.
/// `'/_auth/'` is the INDEX route inside that layout, and its address is whatever the layout sits
/// at: `_layout/index.tsx` is a project's home page, and reading it as a layout dropped `/` from
/// the map entirely.
fn is_pathless_layout(raw: &str) -> bool {
    if raw.len() > 1 && raw.ends_with('/') {
        return false; // an index route, not the layout
    }
    raw.split('/').filter(|s| !s.is_empty()).next_back().is_some_and(|last| last.starts_with('_'))
}

/// True for a file that wraps a subtree rather than rendering a page at its own address.
///
/// `<Outlet />` is where children render, so a route file that has one is the layout AROUND an
/// address and the index route beside it is the page AT it. The name `route.tsx` declares the same
/// thing by convention, whether or not it draws an outlet.
///
/// This is per-file on purpose: the alternative — a path that is a prefix of another route's — is
/// only knowable once every file has been read, and by then the extra screen is already in the
/// index.
fn is_layout_file(file_path: &str, content: &str) -> bool {
    let base = &file_path[file_path.rfind('/').map_or(0, |i| i + 1)..];
    if re!(r"(?:^|\.)route\.(?:tsx|ts|jsx|js)$").is_match(base) {
        return true;
    }
    re!(r"<Outlet\b").is_match(content)
}

fn language_for_file(file_path: &str) -> Language {
    if file_path.ends_with(".tsx") {
        return Language::Tsx;
    }
    if file_path.ends_with(".jsx") {
        return Language::Jsx;
    }
    if re!(r"\.(?:ts|mts|cts)$").is_match(file_path) {
        return Language::TypeScript;
    }
    Language::JavaScript
}

// -- Reading the routes ----------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct TanstackRouteEntry {
    /// `/dashboard/invoices/:invoiceId` — normalised the way the table wants it.
    pub path: String,
    /// The component the route renders, when it names one.
    pub component: Option<String>,
    /// True for the index route AT an address, which outranks the layout that wraps it.
    pub index: bool,
    /// True when the route came from `createFileRoute` — one route per file, so the file's own shape describes it.
    pub file_based: bool,
    pub line: u32,
}

/// A code-based route declaration, by the name it is bound to.
struct Decl {
    path: Option<String>,
    parent: Option<String>,
    component: Option<String>,
    root: bool,
    index: usize,
}

fn line_of(safe: &str, index: usize) -> u32 {
    safe.as_bytes()[..index.min(safe.len())].iter().filter(|c| **c == b'\n').count() as u32 + 1
}

/// Every route a file declares, file-based and code-based alike.
///
/// A code-based route's own `path` is a fragment (`posts`, `$postId`, `/`), so the chain of
/// `getParentRoute: () => parent` is followed to compose the full address — within the file, which
/// is where a route tree is written. A route that is another route's parent is the layout for that
/// subtree, and the address belongs to the index route under it.
pub fn parse_tanstack_routes(content: &str) -> Vec<TanstackRouteEntry> {
    // The calls that declare a route — the cheap gate before parsing anything.
    if !re!(r"\bcreate(?:File|Lazy(?:File)?|Root)?Route\s*\(").is_match(content) {
        return Vec::new();
    }
    let safe = codegraph_text::strip_comments_for_regex(content, "typescript");
    let mut out: Vec<TanstackRouteEntry> = Vec::new();

    // ---- file-based: the path is the first argument, the options follow ----
    let file_routes = Re::cached(r"\bcreate(?:Lazy)?FileRoute\s*\(", "g");
    for f in file_routes.captures_iter(&safe) {
        let whole = f.get(0).expect("group 0");
        let open = whole.end() - 1;
        let Some(close) = match_bracket(&safe, open) else { continue };
        let Some(raw) = read_string_at(safe[open + 1..close].trim_start_matches(is_js_space), 0) else { continue };
        let Some(path) = tanstack_path(&raw) else { continue };
        if is_pathless_layout(&raw) {
            continue;
        }
        out.push(TanstackRouteEntry {
            path,
            component: component_in(chain_after(&safe, close + 1)),
            // `createFileRoute('/dashboard/')` is the index page AT `/dashboard`;
            // `createFileRoute('/dashboard')` is the layout around it.
            index: raw.len() > 1 && raw.ends_with('/'),
            file_based: true,
            line: line_of(&safe, whole.start()),
        });
    }

    // ---- code-based: a fragment per route, composed through its parent ----
    let mut decls: IndexMap<String, Decl> = IndexMap::new();
    let named = Re::cached(r"\b(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=]*?)?=\s*create(Root)?Route\s*\(\s*\{", "g");
    for d in named.captures_iter(&safe) {
        let whole = d.get(0).expect("group 0");
        let from = whole.end() - 1;
        let Some(brace) = safe[from..].find('{').map(|i| i + from) else { continue };
        let Some(end) = match_bracket(&safe, brace) else { continue };
        let fields = read_fields(&safe, brace, end);
        let is_root = d.text(2).is_some();
        let path = if is_root {
            Some("/".to_string())
        } else {
            fields.get("path").and_then(|f| read_string_at(f.text.trim_start_matches(is_js_space), 0))
        };
        let parent = fields
            .get("getParentRoute")
            .and_then(|f| re!(r"=>\s*([A-Za-z_$][\w$]*)").captures(&f.text).and_then(|c| c.text(1).map(String::from)));
        let component = fields.get("component").and_then(|f| component_in(&f.text));
        decls.insert(d.text(1).unwrap_or("").to_string(), Decl { path, parent, component, root: is_root, index: whole.start() });
    }
    // A route with an index child is the LAYOUT around that address; the child with `path: '/'` is
    // what renders there. A parent with no index child still is the page at its own address — its
    // outlet is simply empty.
    let wraps_an_index: Vec<&str> = decls.values().filter(|r| r.path.as_deref() == Some("/")).filter_map(|r| r.parent.as_deref()).collect();
    for (name, decl) in &decls {
        if decl.path.is_none() {
            continue; // a pathless layout contributes no address
        }
        // `createRootRoute` is the outermost layout — every page renders inside it, and the index
        // route beside it is what renders at `/`. A `__root.tsx` that counted as a page put a
        // second `/` on every file-based project's map.
        if decl.root {
            continue;
        }
        if wraps_an_index.contains(&name.as_str()) {
            continue;
        }
        let Some(full) = compose_path(name, &decls) else { continue };
        let Some(path) = tanstack_path(&full) else { continue };
        out.push(TanstackRouteEntry {
            path,
            component: decl.component.clone(),
            index: decl.path.as_deref() == Some("/"),
            file_based: false,
            line: line_of(&safe, decl.index),
        });
    }
    out
}

/// The address a code-based route sits at, following `getParentRoute` up.
fn compose_path(name: &str, decls: &IndexMap<String, Decl>) -> Option<String> {
    let mut segs: Vec<String> = Vec::new();
    let mut cur: Option<&str> = Some(name);
    let mut hops = 0;
    while let Some(current) = cur {
        if hops >= 24 {
            break;
        }
        let decl = decls.get(current)?; // a parent declared in another file — not composed
        if let Some(path) = &decl.path {
            let own = path.split('/').filter(|s| !s.is_empty()).map(String::from);
            segs.splice(0..0, own);
        }
        cur = decl.parent.as_deref();
        hops += 1;
    }
    Some(format!("/{}", segs.join("/")))
}

/// The text of the call chain starting at `at` — `({ … })`, and any `.update({ … })` or `.lazy(…)`
/// after it, which is where a route's component may be written.
fn chain_after(s: &str, at: usize) -> &str {
    let b = s.as_bytes();
    let space = |i: usize| s.get(i..).and_then(|rest| rest.chars().next()).filter(|c| is_js_space(*c)).map(char::len_utf8);
    let mut i = at;
    let start = i;
    for _ in 0..8 {
        while i < b.len() {
            match space(i) {
                Some(n) => i += n,
                None => break,
            }
        }
        if b.get(i) == Some(&b'.') {
            i += 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$') {
                i += 1;
            }
            while i < b.len() {
                match space(i) {
                    Some(n) => i += n,
                    None => break,
                }
            }
        }
        if b.get(i) != Some(&b'(') {
            break;
        }
        let Some(close) = match_bracket(s, i) else { break };
        i = close + 1;
    }
    &s[start..i.min(s.len())]
}

/// The component a route names: an identifier, or the file a lazy import names.
fn component_in(text: &str) -> Option<String> {
    if let Some(lazy) = re!(r#"\bimport\s*\(\s*['"`]([^'"`]+)['"`]"#).captures(text) {
        let tail = re!(r"\.\w+$").replace_first(lazy.text(1).unwrap_or("").rsplit('/').next().unwrap_or(""), |_| String::new());
        return if tail.is_empty() { None } else { Some(tail) };
    }
    re!(r"(?:^|[^\w$])component\s*:\s*([A-Z][A-Za-z0-9_]*)")
        .captures(text)
        .and_then(|c| c.text(1).map(String::from))
        .or_else(|| re!(r"^\s*([A-Z][A-Za-z0-9_]*)\s*$").captures(text).and_then(|c| c.text(1).map(String::from)))
}

/// The id a TanStack route carries — a verbatim reconstruction, so the table can recognise its own.
fn route_id(file_path: &str, line: u32, path: &str) -> String {
    format!("route:{file_path}:{line}:{path}:tanstack")
}

// -- Route table ---------------------------------------------------------------------------------------------------

pub type TanstackTable = RootedRouteTable;

/// True for a route node this resolver emitted, and no other.
fn is_tanstack_route(node: &Node) -> bool {
    node.id == route_id(&node.file_path, node.start_line, &node.name)
}

#[derive(Default)]
struct TanstackCache(RefCell<Option<Rc<TanstackTable>>>);

pub fn tanstack_table(ctx: &Ctx) -> Rc<TanstackTable> {
    let all = ctx.get_nodes_by_kind(NodeKind::Route);
    let cache = ctx.memo::<TanstackCache>();
    if let Some(cached) = cache.0.borrow().as_ref() {
        if Rc::ptr_eq(&cached.source, &all) {
            return Rc::clone(cached);
        }
    }
    let mut by_root: IndexMap<String, RouteTable> = IndexMap::new();
    for node in all.iter() {
        if !is_tanstack_route(node) {
            continue;
        }
        let root = app_root_for(&node.file_path);
        let table = by_root.entry(root).or_insert_with(|| RouteTable::new(all.clone()));
        add_route_to(table, &node.name, node);
    }
    let table = Rc::new(RootedRouteTable { source: all, by_root });
    *cache.0.borrow_mut() = Some(Rc::clone(&table));
    table
}

// -- Navigation calls ------------------------------------------------------------------------------------------------

/// `navigate({ to })` from `useNavigate`, `router.navigate({ to })`, and a thrown `redirect({ to })`.
fn nav_call_re() -> &'static Re {
    re!(r"^(?:navigate|redirect)$|^(?:router|Route)\.navigate$")
}

/// The verb a navigation call name stands for, or `None`.
pub fn tanstack_nav_verb(name: &str) -> Option<String> {
    if !nav_call_re().is_match(name) {
        return None;
    }
    Some(match name.rfind('.') {
        None => name.to_string(),
        Some(dot) => name[dot + 1..].to_string(),
    })
}

/// The destination in a TanStack navigation: `{ to: '/posts/$postId' }`.
///
/// `to` is the route pattern, so it is normalised exactly as a route name is and then names that
/// route. A `navigate({ search: … })` with no `to` is a change of search parameters on the page the
/// user is already on.
pub fn tanstack_destination(expr: &str) -> Option<HrefLiteral> {
    let args = js_trim(expr);
    let literal = if args.starts_with('{') { to_key_of(args) } else { read_string_at(args, 0) }?;
    match tanstack_path(&literal) {
        None => parse_href_expression(args),
        Some(path) => to_href(Some(path)),
    }
}

/// The `to:` value of an object destination, or `None` when it has none or it is computed.
fn to_key_of(args: &str) -> Option<String> {
    let end = match_bracket(args, 0)?;
    let fields = read_fields(args, 0, end);
    let field = fields.get("to")?;
    read_string_at(field.text.trim_start_matches(is_js_space), 0)
}

// -- The resolver --------------------------------------------------------------------------------------------------------

pub struct TanstackRouterResolver {
    nav: NavigationCalls,
}

impl TanstackRouterResolver {
    pub fn new() -> Self {
        TanstackRouterResolver {
            nav: NavigationCalls {
                tails: vec!["navigate", "redirect"],
                // A call matches against the table of the app its file is in.
                scope: Box::new(|route, _| if is_tanstack_route(route) { Some(vec![app_root_for(&route.file_path)]) } else { None }),
            },
        }
    }
}

impl Default for TanstackRouterResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameworkResolver for TanstackRouterResolver {
    fn name(&self) -> &str {
        "tanstack-router"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&ROUTE_LANGUAGES)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        depends_on(
            ctx,
            &["@tanstack/react-router", "@tanstack/solid-router", "@tanstack/router", "@tanstack/react-start", "@tanstack/start"],
        )
    }

    fn claims_reference(&self, name: &str) -> bool {
        nav_call_re().is_match(name)
    }

    fn navigation(&self) -> Option<&NavigationCalls> {
        Some(&self.nav)
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<FrameworkExtractionResult> {
        // A file-based route file describes ONE route, so the file's own shape says whether that
        // route is a page. A file holding a code-based route TREE describes many, and its root
        // component draws the outlet they render into — judging that file by the same rule would
        // drop every route in it.
        let layout = is_layout_file(file_path, content);
        let entries: Vec<TanstackRouteEntry> = parse_tanstack_routes(content).into_iter().filter(|e| !(e.file_based && layout)).collect();
        if entries.is_empty() {
            return Some(FrameworkExtractionResult::default());
        }
        let language = language_for_file(file_path);
        let now = now_ms();
        let mut nodes: Vec<Node> = Vec::new();
        let mut references: Vec<UnresolvedRef> = Vec::new();
        // An index route is the page AT its address; a layout at the same address wraps it. One
        // address, one screen — the index wins it.
        let mut by_path: IndexMap<String, TanstackRouteEntry> = IndexMap::new();
        for entry in entries {
            let replace = match by_path.get(&entry.path) {
                None => true,
                Some(held) => entry.index && !held.index,
            };
            if replace {
                by_path.insert(entry.path.clone(), entry);
            }
        }
        for entry in by_path.values() {
            let node = Node::new(
                route_id(file_path, entry.line, &entry.path),
                NodeKind::Route,
                entry.path.clone(),
                format!("{file_path}::route:{}", entry.path),
                file_path,
                language,
                entry.line,
                entry.line,
                0,
                0,
                now,
            );
            if let Some(component) = &entry.component {
                // `calls`, as every component-backed screen binds: a `references` candidate list
                // is filtered to the ref's own language family.
                let mut r = UnresolvedRef::new(node.id.clone(), component.clone(), EdgeKind::Calls, entry.line, 0, file_path, language);
                r.candidates = Some(vec![component.clone()]);
                references.push(r);
            }
            nodes.push(node);
        }
        Some(FrameworkExtractionResult { nodes, references })
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        if !r.is_kind(EdgeKind::Calls) {
            return None;
        }
        let verb = tanstack_nav_verb(&r.reference_name)?;
        if !ROUTE_LANGUAGES.contains(&r.language) {
            return None;
        }
        let table = tanstack_table(ctx);
        let routes = routes_for_file(&table, &r.file_path)?;
        if routes.exact.is_empty() {
            return None;
        }
        let lines = ctx.get_file_lines(&r.file_path)?;

        let arg = first_argument_text(&lines, r.line, r.column, &verb)?;
        let mut href = tanstack_destination(&arg);
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
    fn paths_are_normalised_like_every_other_framework() {
        assert_eq!(tanstack_path("/posts/$postId").as_deref(), Some("/posts/:postId"));
        assert_eq!(tanstack_path("/_auth/invoices").as_deref(), Some("/invoices"));
        assert_eq!(tanstack_path("/files/$").as_deref(), Some("/files/:splat*"));
        assert_eq!(tanstack_path("/(group)/about/").as_deref(), Some("/about"));
        assert_eq!(tanstack_path("posts"), None);
    }

    #[test]
    fn layouts_are_not_pages() {
        assert!(is_pathless_layout("/_auth"));
        assert!(!is_pathless_layout("/_auth/"));
        assert!(!is_pathless_layout("/posts"));
        assert!(is_layout_file("src/routes/dashboard.route.tsx", ""));
        assert!(is_layout_file("src/routes/x.tsx", "<Outlet />"));
        assert!(!is_layout_file("src/routes/x.tsx", "<div/>"));
    }

    #[test]
    fn file_and_code_based_routes_are_read() {
        let file = "export const Route = createFileRoute('/dashboard/invoices/$invoiceId')({\n  component: InvoiceComponent,\n})\n";
        let entries = parse_tanstack_routes(file);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "/dashboard/invoices/:invoiceId");
        assert_eq!(entries[0].component.as_deref(), Some("InvoiceComponent"));
        assert!(entries[0].file_based);

        let code = "const rootRoute = createRootRoute({ component: Root })\nconst postsRoute = createRoute({ getParentRoute: () => rootRoute, path: 'posts', component: Posts })\nconst postRoute = createRoute({ getParentRoute: () => postsRoute, path: '$postId', component: Post })\n";
        let entries = parse_tanstack_routes(code);
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["/posts", "/posts/:postId"]);
    }

    #[test]
    fn destinations_are_route_patterns() {
        let to = tanstack_destination("{ to: '/posts/$postId', params: { postId } }").unwrap();
        assert_eq!(to.path, "/posts/:postId");
        assert!(tanstack_destination("{ search: { page: 2 } }").is_none());
    }
}
