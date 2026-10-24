//! Next.js — file-based pages and route handlers, and string-keyed navigation. Port of
//! `src/resolution/frameworks/nextjs.ts`.
//!
//! Two things static extraction cannot see on its own, and that together are most of what "how
//! does the site flow" means in a Next app:
//!
//! 1. **A page is a file.** `app/users/page.tsx` is `/users`, `app/(marketing)/about/page.tsx` is
//!    `/about` (a `(group)` is invisible in the URL), `app/blog/[slug]/page.tsx` is
//!    `/blog/:slug`, `app/docs/[...all]/page.tsx` is `/docs/:all*`; the Pages Router's
//!    `pages/about.tsx` is `/about`. `extract()` emits one `route` node per page, named by its
//!    path, with a `calls` ref to the file's default export so the route reaches the component
//!    that renders it — exactly as Expo Router's screens do. `app/api/users/route.ts` exports
//!    `GET` / `POST` / … — one route node per method, `POST /api/users`, with a `references` ref
//!    to that function, as every server resolver names a handler; `pages/api/users.ts` is
//!    `ANY /api/users` bound to its default export.
//!
//! 2. **Navigation is a string.** `router.push('/users')`, `redirect('/login')` /
//!    `permanentRedirect`, `NextResponse.redirect(new URL('/login', req.url))`: the extractor
//!    records each as a call that resolves to nothing, because the target is a path. `resolve()`
//!    claims those refs, reads the argument off the source (the Expo Router readers), matches it
//!    against this framework's own route table, and returns a **`navigates`** edge carrying the
//!    href. `<Link href="/x">` and an internal `<a href>` are JSX attributes, not calls, so a
//!    synthesizer reads them from the source instead.
//!
//! Precision rests on the string resolving to a real page: a computed href, a path no page serves,
//! a relative href, or a conditional that forks are left unresolved rather than guessed. Parallel
//! (`@slot`) and intercepting (`(.)photo`) routes are not modelled; `layout` / `loading` /
//! `error` / `template` files are not routes.

use super::expo_router::{
    decode_segment, default_export_name, first_argument_text, href_arms, match_route, nav_metadata, parse_href_expression,
    read_href_via_local, HrefLiteral, RouteEntry, RouteTable, HOLE,
};
use super::now_ms;
use super::package_deps::depends_on;
use crate::ctx::Ctx;
use crate::types::{AlsoTarget, FrameworkExtractionResult, FrameworkResolver, NavigationCalls, NodeRef, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::rc::Rc;

const ROUTE_LANGUAGES: [Language; 4] = [Language::TypeScript, Language::JavaScript, Language::Tsx, Language::Jsx];
const HTTP_EXPORTS: &str = "GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS";

// -- Route files -------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextRouteKind {
    /// A page component.
    Page,
    /// An App Router `route.ts` handler file.
    Handler,
    /// A Pages Router API file.
    Api,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NextRouteFile {
    pub kind: NextRouteKind,
    /// `/blog/:slug` — the path, in the form every other framework's routes use.
    pub path: String,
    /// The directory the Next app lives in (`""`, `apps/web/`) — what its navigation calls are gated on.
    pub root: String,
}

/// `[slug]` → `:slug`, `[...all]` / `[[...all]]` → `:all*`; anything else as written.
fn next_segment(seg: &str) -> String {
    if let Some(m) = re!(r"^\[\[\.\.\.([^\]]+)\]\]$").captures(seg) {
        return format!(":{}*", m.text(1).unwrap_or(""));
    }
    if let Some(m) = re!(r"^\[\.\.\.([^\]]+)\]$").captures(seg) {
        return format!(":{}*", m.text(1).unwrap_or(""));
    }
    if let Some(m) = re!(r"^\[([^\]]+)\]$").captures(seg) {
        return format!(":{}", m.text(1).unwrap_or(""));
    }
    seg.to_string()
}

/// What a file is to the router, or `None` for a file that is not a route.
pub fn next_route_for_file(file_path: &str) -> Option<NextRouteFile> {
    if re!(r"(?:^|\/)(?:__tests__|__mocks__|node_modules)\/").is_match(file_path) {
        return None;
    }
    if let Some(app) = re!(r"^((?:[^/]+\/)*?)(?:src\/)?app\/(.+)$").captures(file_path) {
        let m = re!(r"^(.*?)(?:^|\/)(page|route)\.(?:tsx|ts|jsx|js|mjs|cjs|mdx?)$").captures(app.text(2).unwrap_or(""))?;
        let segs: Vec<&str> = m.text(1).unwrap_or("").split('/').filter(|s| !s.is_empty()).collect();
        // Parallel and intercepting routes are a picture of their own; not modelled.
        if segs.iter().any(|s| s.starts_with('@') || re!(r"^\(\.{1,3}\)").is_match(s)) {
            return None;
        }
        let kept: Vec<String> = segs.iter().filter(|s| !(s.starts_with('(') && s.ends_with(')'))).map(|s| next_segment(s)).collect();
        return Some(NextRouteFile {
            kind: if m.text(2) == Some("page") { NextRouteKind::Page } else { NextRouteKind::Handler },
            path: format!("/{}", kept.join("/")),
            root: app.text(1).unwrap_or("").to_string(),
        });
    }
    if let Some(pages) = re!(r"^((?:[^/]+\/)*?)(?:src\/)?pages\/(.+)$").captures(file_path) {
        let rel = pages.text(2).unwrap_or("");
        let ext = re!(r"\.(?:tsx|ts|jsx|js|mjs|cjs|mdx?)$").find(rel)?;
        let bare = &rel[..ext.start()];
        let mut segs: Vec<&str> = bare.split('/').collect();
        let base = segs[segs.len() - 1];
        if base.starts_with('_') || re!(r"\.(?:test|spec|stories|config|d)$").is_match(bare) {
            return None;
        }
        if segs.last() == Some(&"index") {
            segs.pop();
        }
        let mapped: Vec<String> = segs.iter().map(|s| next_segment(s)).collect();
        return Some(NextRouteFile {
            kind: if segs.first() == Some(&"api") { NextRouteKind::Api } else { NextRouteKind::Page },
            path: format!("/{}", mapped.join("/")),
            root: pages.text(1).unwrap_or("").to_string(),
        });
    }
    None
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

// -- Route table — this framework's pages, matched the Expo Router way ----------------------------------

pub struct NextTable {
    /// Identity of the node array the table was built from — rebuild when it changes.
    pub table: RouteTable,
    /// The directories Next apps live in — a navigation call is only read from under one.
    pub roots: Vec<String>,
}

#[derive(Default)]
struct NextTableCache(RefCell<Option<Rc<NextTable>>>);

/// The page a route node is, or `None` for a node the table leaves out (a handler, another
/// framework's route).
fn table_page(node: &Node) -> Option<NextRouteFile> {
    next_route_for_file(&node.file_path).filter(|file| file.kind == NextRouteKind::Page && file.path == node.name)
}

pub fn next_route_table(ctx: &Ctx) -> Rc<NextTable> {
    let all = ctx.get_nodes_by_kind(NodeKind::Route);
    let cache = ctx.memo::<NextTableCache>();
    if let Some(cached) = cache.0.borrow().as_ref() {
        if Rc::ptr_eq(&cached.table.source, &all) {
            return Rc::clone(cached);
        }
    }
    let mut exact: IndexMap<String, NodeRef> = IndexMap::new();
    let mut dynamic: Vec<RouteEntry> = Vec::new();
    let mut roots: Vec<String> = Vec::new();
    for node in all.iter() {
        let Some(file) = table_page(node) else { continue };
        exact.insert(node.name.clone(), node.clone());
        if node.name.contains(':') {
            dynamic.push(RouteEntry { node: node.clone(), segs: node.name.split('/').skip(1).map(String::from).collect() });
        }
        if !roots.contains(&file.root) {
            roots.push(file.root);
        }
    }
    let table = Rc::new(NextTable { table: RouteTable { source: all, exact, dynamic }, roots });
    *cache.0.borrow_mut() = Some(Rc::clone(&table));
    table
}

/// `/users/${…}?tab=x` → `['users', '*']`; an absolute URL keeps its path; a relative href is nothing.
fn href_segments(href: &HrefLiteral) -> Option<Vec<String>> {
    let mut p: String = href.path.clone();
    let absolute = Re::cached(r"^(?:[a-z][a-z0-9+.-]*:)?\/\/[^/]*(\/.*)?$", "i")
        .captures(&p)
        .map(|m| m.text(1).map(String::from).unwrap_or_else(|| "/".to_string()));
    if let Some(rest) = absolute {
        p = rest;
    }
    if !p.starts_with('/') {
        return None;
    }
    Some(p.split('/').skip(1).filter(|s| !s.is_empty()).map(|s| if s.contains(HOLE) { "*".to_string() } else { decode_segment(s) }).collect())
}

/// A route a destination names, with the arm that named it — so each edge says the path it took.
#[derive(Clone)]
pub struct HrefDestination {
    pub node: NodeRef,
    /// The arm of the expression this route came from; its `display` is the edge's href.
    pub href: HrefLiteral,
}

/// Every route a destination names — one per arm of a conditional, deduped.
///
/// Each carries its OWN arm, because an edge that says `/search/${…}/page/${…}` while pointing at
/// `/admin/productlist/:pageNumber` names a path it did not take.
pub fn destinations_for_href(href: &HrefLiteral, table: &RouteTable) -> Vec<HrefDestination> {
    let mut out: Vec<HrefDestination> = Vec::new();
    for arm in href_arms(href) {
        let Some(segs) = href_segments(arm) else { continue };
        let Some(target) = match_route(&segs, table) else { continue };
        // An arm naming no route drops out; the arms that DO name one are still places this
        // navigation goes.
        if out.iter().any(|d| d.node.id == target.id) {
            continue;
        }
        out.push(HrefDestination { node: target, href: arm.clone() });
    }
    out
}

/// The single route an href names, or `None` when it names none. The first arm wins a fork.
pub fn page_for_href(href: &HrefLiteral, table: &RouteTable) -> Option<NodeRef> {
    destinations_for_href(href, table).into_iter().next().map(|d| d.node)
}

// -- Navigation calls ---------------------------------------------------------------------------------------

/// `router.push` / `.replace` / `.prefetch`, `redirect` / `permanentRedirect`, `NextResponse.redirect`.
fn nav_call_re() -> &'static Re {
    re!(r"(?:^|\.)(push|replace|prefetch)$|^(redirect|permanentRedirect)$|^(?:NextResponse|Response)\.(redirect)$")
}

/// The verb a navigation call name stands for, or `None`.
pub fn next_nav_verb(name: &str) -> Option<String> {
    let m = nav_call_re().captures(name)?;
    if m.text(3).is_some_and(|t| !t.is_empty()) {
        return Some("response.redirect".to_string());
    }
    m.text(1).or_else(|| m.text(2)).map(String::from)
}

// -- The resolver --------------------------------------------------------------------------------------------

pub struct NextjsResolver {
    nav: NavigationCalls,
}

impl NextjsResolver {
    pub fn new() -> Self {
        NextjsResolver {
            nav: NavigationCalls {
                tails: vec!["push", "replace", "prefetch", "redirect", "permanentRedirect"],
                // One table holds every Next app's pages, and a call under any of them matches
                // against all of it.
                scope: Box::new(|route, ctx| if table_page(route).is_some() { Some(next_route_table(ctx).roots.clone()) } else { None }),
            },
        }
    }
}

impl Default for NextjsResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameworkResolver for NextjsResolver {
    fn name(&self) -> &str {
        "nextjs"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&ROUTE_LANGUAGES)
    }

    fn app_dependencies(&self) -> Option<&[&'static str]> {
        Some(&["next"])
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        if depends_on(ctx, &["next"]) {
            return true;
        }
        let files = ctx.get_all_files();
        let has_config = files.iter().any(|f| re!(r"(?:^|\/)next\.config\.[cm]?[jt]s$").is_match(f));
        has_config && files.iter().any(|f| next_route_for_file(f).is_some())
    }

    fn claims_reference(&self, name: &str) -> bool {
        nav_call_re().is_match(name)
    }

    fn navigation(&self) -> Option<&NavigationCalls> {
        Some(&self.nav)
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<FrameworkExtractionResult> {
        let Some(file) = next_route_for_file(file_path) else { return Some(FrameworkExtractionResult::default()) };
        let language = language_for_file(file_path);
        let now = now_ms();
        let mut nodes: Vec<Node> = Vec::new();
        let mut references: Vec<UnresolvedRef> = Vec::new();
        let stripped = codegraph_text::strip_comments_for_regex(content, "typescript");
        let line_of = |index: usize| -> u32 { stripped.as_bytes()[..index].iter().filter(|c| **c == b'\n').count() as u32 + 1 };

        if file.kind == NextRouteKind::Handler {
            // `export async function GET(req) {…}` / `export const POST = …` — one route per method.
            let mut seen: Vec<String> = Vec::new();
            let decl = Re::cached(
                &format!(r"\bexport\s+(?:async\s+)?function\s+({HTTP_EXPORTS})\b|\bexport\s+(?:const|let)\s+({HTTP_EXPORTS})\s*="),
                "g",
            );
            for m in decl.captures_iter(&stripped) {
                let method = m.text(1).or_else(|| m.text(2)).unwrap_or("").to_string();
                if seen.contains(&method) {
                    continue;
                }
                seen.push(method.clone());
                let whole = m.get(0)?;
                let line = line_of(whole.start());
                let mut node = Node::new(
                    format!("route:{file_path}:{line}:{method}:{}", file.path),
                    NodeKind::Route,
                    format!("{method} {}", file.path),
                    format!("{file_path}::{method}:{}", file.path),
                    file_path,
                    language,
                    line,
                    line,
                    0,
                    codegraph_types::jscompat::utf16_len(whole.as_str()) as u32,
                    now,
                );
                node.is_exported = Some(true);
                let mut r = UnresolvedRef::new(node.id.clone(), method.clone(), EdgeKind::References, line, 0, file_path, language);
                r.candidates = Some(vec![method]);
                references.push(r);
                nodes.push(node);
            }
            return Some(FrameworkExtractionResult { nodes, references });
        }

        // A page, or a Pages Router API file: the default export is what runs.
        let is_api = file.kind == NextRouteKind::Api;
        let name = if is_api { format!("ANY {}", file.path) } else { file.path.clone() };
        let mut node = Node::new(
            if is_api { format!("route:{file_path}:1:ANY:{}", file.path) } else { format!("route:{file_path}:{}", file.path) },
            NodeKind::Route,
            name,
            if is_api { format!("{file_path}::ANY:{}", file.path) } else { format!("{file_path}::route:{}", file.path) },
            file_path,
            language,
            1,
            1,
            0,
            0,
            now,
        );
        node.is_exported = Some(true);
        let from_id = node.id.clone();
        nodes.push(node);
        if let Some((exported, index)) = default_export_name(&stripped) {
            let kind = if is_api { EdgeKind::References } else { EdgeKind::Calls };
            let mut r = UnresolvedRef::new(from_id, exported.clone(), kind, line_of(index), 0, file_path, language);
            r.candidates = Some(vec![exported]);
            references.push(r);
        }
        Some(FrameworkExtractionResult { nodes, references })
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        if !r.is_kind(EdgeKind::Calls) {
            return None;
        }
        let verb = next_nav_verb(&r.reference_name)?;
        if !ROUTE_LANGUAGES.contains(&r.language) {
            return None;
        }
        let next = next_route_table(ctx);
        let table = &next.table;
        if table.exact.is_empty() || !next.roots.iter().any(|root| r.file_path.starts_with(root.as_str())) {
            return None;
        }
        let callee = &r.reference_name[r.reference_name.rfind('.').map_or(0, |i| i + 1)..];
        let lines = ctx.get_file_lines(&r.file_path)?;

        let first = first_argument_text(&lines, r.line, r.column, callee)?;
        // `NextResponse.redirect(new URL('/login', req.url))` — the path is the URL's first argument.
        let arg: Option<String> =
            if re!(r"^\s*new\s+URL\s*\(").is_match(&first) { first_argument_text(&[first], 1, 0, "URL") } else { Some(first) };
        let mut href = arg.as_deref().and_then(parse_href_expression);
        if href.is_none() {
            let start = match ctx.node_by_id(&r.from_node_id) {
                Some(enclosing) if enclosing.file_path == r.file_path => enclosing.start_line,
                _ => r.line.saturating_sub(40).max(1),
            };
            href = read_href_via_local(&lines, r.line, r.column, callee, start);
        }
        let href = href?;
        // Every arm of a conditional destination is somewhere this call goes; the first is this
        // reference's resolution and the rest ride as `alsoTargets`.
        let targets = destinations_for_href(&href, table);
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
    fn pages_and_handlers_follow_the_file_tree() {
        let page = next_route_for_file("app/(marketing)/about/page.tsx").unwrap();
        assert_eq!((page.kind, page.path.as_str(), page.root.as_str()), (NextRouteKind::Page, "/about", ""));
        let dynamic = next_route_for_file("apps/web/src/app/blog/[slug]/page.tsx").unwrap();
        assert_eq!((dynamic.path.as_str(), dynamic.root.as_str()), ("/blog/:slug", "apps/web/"));
        let catch_all = next_route_for_file("app/docs/[[...all]]/page.tsx").unwrap();
        assert_eq!(catch_all.path, "/docs/:all*");
        let handler = next_route_for_file("app/api/users/route.ts").unwrap();
        assert_eq!((handler.kind, handler.path.as_str()), (NextRouteKind::Handler, "/api/users"));
        let legacy = next_route_for_file("pages/api/users/index.ts").unwrap();
        assert_eq!((legacy.kind, legacy.path.as_str()), (NextRouteKind::Api, "/api/users"));
        assert!(next_route_for_file("pages/_app.tsx").is_none());
        assert!(next_route_for_file("app/@modal/login/page.tsx").is_none());
        assert!(next_route_for_file("app/users/layout.tsx").is_none());
    }

    #[test]
    fn nav_verbs() {
        assert_eq!(next_nav_verb("router.push").as_deref(), Some("push"));
        assert_eq!(next_nav_verb("redirect").as_deref(), Some("redirect"));
        assert_eq!(next_nav_verb("NextResponse.redirect").as_deref(), Some("response.redirect"));
        assert_eq!(next_nav_verb("list.map"), None);
    }

    #[test]
    fn absolute_hrefs_keep_their_path() {
        let href = parse_href_expression("'https://example.com/users/3?x=1'").unwrap();
        assert_eq!(href_segments(&href), Some(vec!["users".to_string(), "3".to_string()]));
        let relative = parse_href_expression("'users'").unwrap();
        assert_eq!(href_segments(&relative), None);
    }
}
