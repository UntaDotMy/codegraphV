//! Navigation and route-dispatch synthesis: GoFrame routes onto their handlers, Expo Router
//! helpers that return a screen, and the markup links (`<Link href>`, `<Link to>`, `<router-link>`,
//! SvelteKit `<a href>`). Port of `goframe-synthesizer.ts`, `expo-router-synthesizer.ts`,
//! `next-router-synthesizer.ts`, `react-router-synthesizer.ts`, `tanstack-router-synthesizer.ts`,
//! `vue-router-synthesizer.ts` and `sveltekit-synthesizer.ts`.

use super::common::*;
use crate::ctx::Ctx;
use crate::frameworks::expo_router::{
    match_route, normalize_href_path, read_string_at, route_table, routes_for_file, string_end, to_href, HrefLiteral, RouteTable, HOLE,
};
use crate::frameworks::goframe::GOFRAME_ROUTE_MARKER;
use crate::frameworks::nextjs::{destinations_for_href, next_route_table};
use crate::frameworks::object_literal::match_bracket;
use crate::frameworks::react::config_href_expression;
use crate::frameworks::react_router::react_router_table;
use crate::frameworks::sveltekit_router::svelte_kit_table;
use crate::frameworks::tanstack_router::{tanstack_destination, tanstack_table};
use crate::frameworks::vue_router::{parse_vue_path_object, route_name_in_expression, vue_route_table};
use crate::import_resolver::resolve_import_path;
use crate::meta;
use crate::synth_utils::{enclosing_fn, make_line_at};
use crate::types::NodeRef;
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_search::is_test_path;
use codegraph_types::jscompat::{js_trim, utf16_byte_index, utf16_len};
use codegraph_types::{Edge, EdgeKind, Language, NodeKind};
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};

/// Links a single component may carry before it is a navigation menu, not a decision.
const MAX_LINKS_PER_COMPONENT: usize = 24;

fn navigates_edge(source: &str, target: &str, line: u32, column: Option<u32>, metadata: codegraph_types::Metadata) -> Edge {
    let mut e = heuristic_edge(source, target, EdgeKind::Navigates, Some(line), metadata);
    e.column = column;
    e
}

/// The `seen` / per-component bookkeeping every link pass applies to an edge before drawing it.
#[derive(Default)]
struct LinkBudget {
    seen: HashSet<String>,
    per_component: HashMap<String, usize>,
}

impl LinkBudget {
    fn admit(&mut self, component: &str, target: &str) -> bool {
        let key = format!("{component}>{target}");
        if self.seen.contains(&key) {
            return false;
        }
        let count = self.per_component.entry(component.to_string()).or_insert(0);
        *count += 1;
        if *count > MAX_LINKS_PER_COMPONENT {
            return false;
        }
        self.seen.insert(key);
        true
    }
}

// ── GoFrame: route → controller method ────────────────────────────────────────

const GOFRAME_FANOUT_CAP: usize = 2000; // backstop only; real apps are 1 route → 1 method.

/// Pointer-parameter types in a Go method signature, in both qualified and bare forms:
/// `(ctx context.Context, req *cash.ListReq)` → `["cash.ListReq", "ListReq"]`.
fn pointer_param_types(sig: &str) -> Vec<String> {
    let mut out = Vec::new();
    for m in re!(r"\*\s*(?:(\w+)\.)?([A-Z]\w*)\b", "g").captures_iter(sig) {
        let name = m.text(2).unwrap_or("");
        if let Some(pkg) = m.text(1).filter(|p| !p.is_empty()) {
            out.push(format!("{pkg}.{name}"));
        }
        out.push(name.to_string());
    }
    out
}

/// The addon/plugin module a path lives under (`addons/hgexample/…` → `hgexample`), or `""` for
/// the core app.
fn addon_root(p: &str) -> String {
    re!(r"(?:^|/)addons/([^/]+)/").captures(p).and_then(|m| m.text(1).map(String::from)).unwrap_or_default()
}

/// Pick the one handler for a route from same-request-type candidates. Ambiguity left over ⇒ no
/// edge (silent beats wrong).
fn select_handler<'a>(candidates: &'a [NodeRef], route_file: &str) -> Option<&'a NodeRef> {
    if candidates.len() == 1 {
        return Some(&candidates[0]);
    }
    let mut cands: Vec<&NodeRef> = candidates.iter().filter(|h| re!(r"/controller(s)?/").is_match(&h.file_path)).collect();
    if cands.is_empty() {
        cands = candidates.iter().collect();
    }
    if cands.len() == 1 {
        return Some(cands[0]);
    }
    let ar = addon_root(route_file);
    let same_module: Vec<&NodeRef> = cands.into_iter().filter(|h| addon_root(&h.file_path) == ar).collect();
    if same_module.len() == 1 {
        Some(same_module[0])
    } else {
        None
    }
}

pub fn goframe_route_edges(ctx: &Ctx) -> Vec<Edge> {
    // Route nodes the goframe extractor created, keyed by their package-qualified request type.
    let mut routes_by_req_type: IndexMap<String, Vec<NodeRef>> = IndexMap::new();
    let mut wanted: HashSet<String> = HashSet::new();
    each_node(ctx, NodeKind::Route, |route| {
        if route.language != Language::Go {
            return;
        }
        let Some(marker) = route.qualified_name.find(GOFRAME_ROUTE_MARKER) else { return };
        let join_key = route.qualified_name[marker + GOFRAME_ROUTE_MARKER.len()..].to_string();
        if join_key.is_empty() {
            return;
        }
        wanted.insert(join_key.clone());
        if let Some(dot) = join_key.rfind('.') {
            wanted.insert(join_key[dot + 1..].to_string()); // bare fallback
        }
        routes_by_req_type.entry(join_key).or_default().push(route);
    });
    if routes_by_req_type.is_empty() {
        return Vec::new();
    }

    // Handler candidates: Go methods whose signature takes a wanted request type by pointer,
    // indexed by every matching (qualified + bare) form.
    let mut handlers_by_key: HashMap<String, Vec<NodeRef>> = HashMap::new();
    each_node(ctx, NodeKind::Method, |method| {
        if method.language != Language::Go {
            return;
        }
        let Some(sig) = method.signature.as_deref().filter(|s| !s.is_empty()) else { return };
        for t in pointer_param_types(sig) {
            if !wanted.contains(&t) {
                continue;
            }
            handlers_by_key.entry(t).or_default().push(method.clone());
        }
    });

    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut added = 0;
    for (join_key, routes) in &routes_by_req_type {
        let bare = join_key.rfind('.').map_or(join_key.as_str(), |d| &join_key[d + 1..]);
        // Precise package-qualified match first; bare type only as a fallback.
        let Some(candidates) = handlers_by_key.get(join_key).or_else(|| handlers_by_key.get(bare)).filter(|c| !c.is_empty()) else { continue };
        for route in routes {
            let Some(handler) = select_handler(candidates, &route.file_path) else { continue };
            if route.id == handler.id {
                continue;
            }
            let key = format!("{}>{}", route.id, handler.id);
            if seen.contains(&key) || added >= GOFRAME_FANOUT_CAP {
                continue;
            }
            seen.insert(key);
            edges.push(calls_edge(
                &route.id,
                &handler.id,
                Some(route.start_line),
                meta! {
                    "synthesizedBy" => "goframe-route", "route" => route.name.as_str(), "requestType" => bare,
                    "registeredAt" => format!("{}:{}", handler.file_path, handler.start_line),
                },
            ));
            added += 1;
        }
    }
    edges
}

// ── Expo Router: navigation whose destination comes back from a helper ────────

/// A helper yielding more distinct screens than this is a table, not a decision.
const MAX_SCREENS_PER_HELPER: usize = 8;

struct NavSite {
    file: String,
    line: u32,
    method: String,
    callee: String,
}

struct Screen {
    node: NodeRef,
    href: String,
    line: u32,
    column: u32,
}

fn is_js_language(l: Language) -> bool {
    matches!(l, Language::TypeScript | Language::JavaScript | Language::Tsx | Language::Jsx)
}

pub fn expo_router_return_edges(ctx: &Ctx) -> Vec<Edge> {
    let table = route_table(ctx);
    if table.exact.is_empty() {
        return Vec::new();
    }

    // 1. Every navigation call whose argument is a call.
    let js_file = re!(r"\.(?:[cm]?[jt]sx?)$");
    let gate = re!(r"\.(?:push|replace|navigate|dismissTo)\(");
    let nav_fed_by_call = re!(r"\.(push|replace|navigate|dismissTo)\(\s*(?:await\s+)?([A-Za-z_$][\w$.]*)\s*\(", "g");
    let mut sites: Vec<NavSite> = Vec::new();
    for file in ctx.get_all_files() {
        if !js_file.is_match(&file) {
            continue;
        }
        let Some(source) = ctx.read_file(&file).filter(|s| !s.is_empty()) else { continue };
        if !gate.is_match(&source) {
            continue;
        }
        let stripped = codegraph_text::strip_comments_for_regex(&source, "typescript");
        for m in nav_fed_by_call.captures_iter(&stripped) {
            let line = line_of(&stripped, m.get(0).unwrap().start());
            sites.push(NavSite {
                file: file.clone(),
                line,
                method: m.text(1).unwrap_or("").to_string(),
                callee: m.text(2).unwrap_or("").to_string(),
            });
        }
    }
    if sites.is_empty() {
        return Vec::new();
    }

    // 2. Each callee → the project function it names → the screens in its body.
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut screens_by_helper: HashMap<String, Option<Vec<Screen>>> = HashMap::new();
    for site in &sites {
        let Some(helper) = resolve_helper(site, ctx) else { continue };
        if !screens_by_helper.contains_key(&helper.id) {
            let screens = screens_in_body(&helper, ctx, &table);
            screens_by_helper.insert(helper.id.clone(), screens);
        }
        let Some(screens) = screens_by_helper.get(&helper.id).and_then(|s| s.as_ref()) else { continue };
        for s in screens {
            let key = format!("{}>{}", helper.id, s.node.id);
            if !seen.insert(key) {
                continue;
            }
            // The literal's own column, not the line's start: the two arms of
            // `return (await seen()) ? '/home/' : '/welcome/'` share a line, and only the column
            // lets the guard reader say WHICH arm each edge is.
            edges.push(navigates_edge(
                &helper.id,
                &s.node.id,
                s.line,
                Some(s.column),
                meta! {
                    "synthesizedBy" => "expo-router-return", "href" => s.href.as_str(), "navMethod" => site.method.as_str(),
                    "registeredAt" => format!("{}:{}", site.file, site.line),
                },
            ));
        }
    }
    edges
}

/// The function `site.callee` names, seen from `site.file`: same file first, then the file its
/// import points at, then a unique project-wide match. Ambiguity is a null — an edge onto the
/// wrong `load()` is worse than none.
fn resolve_helper(site: &NavSite, ctx: &Ctx) -> Option<NodeRef> {
    let segs: Vec<&str> = site.callee.split('.').collect();
    let bare = segs[segs.len() - 1];
    let head = segs[0];
    let candidates: Vec<NodeRef> = ctx
        .get_nodes_by_name(bare)
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method) && is_js_language(n.language))
        .cloned()
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let local: Vec<&NodeRef> = candidates.iter().filter(|n| n.file_path == site.file).collect();
    if local.len() == 1 {
        return Some(local[0].clone());
    }
    if local.len() > 1 {
        return None;
    }

    let lang = if site.file.ends_with('x') { Language::Tsx } else { Language::TypeScript };
    let mappings = ctx.get_import_mappings(&site.file, lang);
    if let Some(resolved) = mappings.iter().find(|im| im.local_name == head || im.local_name == bare).and_then(|im| im.resolved_path.as_ref()) {
        let via_import: Vec<&NodeRef> = candidates.iter().filter(|n| &n.file_path == resolved).collect();
        if via_import.len() == 1 {
            return Some(via_import[0].clone());
        }
    }
    if candidates.len() == 1 {
        Some(candidates[0].clone())
    } else {
        None
    }
}

/// The screens named by string literals in the helper's body — each literal that begins with `/`,
/// resolved like an href would be. `None` when the body cannot be read or names too many screens
/// to be a decision.
fn screens_in_body(helper: &NodeRef, ctx: &Ctx, table: &RouteTable) -> Option<Vec<Screen>> {
    let lines = ctx.get_file_lines(&helper.file_path)?;
    let from = (helper.start_line as usize).saturating_sub(1).min(lines.len());
    let to = (helper.end_line as usize).min(lines.len()).max(from);
    let body = codegraph_text::strip_comments_for_regex(&lines[from..to].join("\n"), "typescript");
    // Scan the BODY, not the signature: a return type of literal routes is string literals too,
    // and they come FIRST. The body starts at the arrow, or at the first brace for a declaration.
    let arrow = body.find("=>");
    let brace = body.find('{');
    let scan_from = match (arrow, brace) {
        (Some(a), b) if b.is_none_or(|b| a < b) => a + 2,
        (_, Some(b)) => b + 1,
        _ => 0,
    };
    let mut found: IndexMap<String, Screen> = IndexMap::new();
    let bytes = body.as_bytes();
    let mut i = scan_from;
    while i < bytes.len() {
        let ch = bytes[i];
        if ch != b'"' && ch != b'\'' && ch != b'`' {
            i += 1;
            continue;
        }
        let start = i;
        let literal = read_string_at(&body, i);
        i = string_end(&body, i) + 1;
        let Some(literal) = literal.filter(|l| l.starts_with('/')) else { continue };
        let Some(href) = to_href(Some(literal)) else { continue };
        let Some(segs) = normalize_href_path(&href.path, &helper.file_path) else { continue };
        let Some(route) = match_route(&segs, table) else { continue };
        if found.contains_key(&route.id) {
            continue;
        }
        // `body` begins at column 0 of the helper's first line and the comment stripper preserves
        // offsets, so the literal's column is exact.
        let line_start = body[..start].rfind('\n').map_or(0, |p| p + 1);
        let newlines = body.as_bytes()[..start].iter().filter(|b| **b == b'\n').count() as u32;
        found.insert(
            route.id.clone(),
            Screen { node: route, href: href.display, line: helper.start_line + newlines, column: utf16_len(&body[line_start..start]) as u32 },
        );
        if found.len() > MAX_SCREENS_PER_HELPER {
            return None;
        }
    }
    if found.is_empty() {
        None
    } else {
        Some(found.into_values().collect())
    }
}

// ── Next.js: <Link href> / <a href> ───────────────────────────────────────────

/// A `{ pathname: '…' }` object's literal, read the way the TypeScript does.
///
/// The TS runs a sticky regex whose `lastIndex` is an offset into the WHOLE source against a
/// string already sliced to start at the object — so it can only match when the object text
/// happens to repeat at twice the offset. That is reproduced here, not corrected.
fn sticky_pathname_literal(safe: &str, at: usize) -> Option<String> {
    let close = safe[at..].find('}').map(|c| at + c);
    let sliced = &safe[at..close.unwrap_or(safe.len())];
    let at16 = utf16_len(&safe[..at]);
    if at16 > utf16_len(sliced) {
        return None;
    }
    let key = Re::cached(r"\bpathname\s*:\s*", "y");
    let head = key.captures_at(sliced, utf16_byte_index(sliced, at16))?;
    let whole = head.get(0)?;
    let pos16 = at16 + utf16_len(&sliced[..whole.start()]) + utf16_len(whole.as_str());
    if pos16 > utf16_len(safe) {
        return None;
    }
    read_string_at(safe, utf16_byte_index(safe, pos16))
}

pub fn next_link_edges(ctx: &Ctx) -> Vec<Edge> {
    let table = next_route_table(ctx);
    if table.table.exact.is_empty() {
        return Vec::new();
    }
    let jsx_file = re!(r"\.(?:[cm]?[jt]sx?|mdx)$");
    let link_tag = re!(r##"<(Link|NextLink|a)\b([^>]*?)\bhref\s*=\s*(?:"([^"]*)"|'([^']*)'|\{\s*)"##, "g");
    let mut edges = Vec::new();
    let mut budget = LinkBudget::default();
    for file in ctx.get_all_files() {
        if !jsx_file.is_match(&file) || is_test_path(&file) {
            continue;
        }
        if !table.roots.iter().any(|root| file.starts_with(root.as_str())) {
            continue;
        }
        let Some(source) = ctx.read_file(&file).filter(|s| s.contains("href")) else { continue };
        let safe = codegraph_text::strip_comments_for_regex(&source, "typescript");
        let nodes = ctx.get_nodes_in_file(&file);
        let line_of = make_line_at(&safe, 1);
        for m in link_tag.captures_iter(&safe) {
            let whole = m.get(0).unwrap();
            let tag = m.text(1).unwrap_or("");
            let mut literal: Option<String> = m.text(3).or(m.text(4)).map(String::from);
            if literal.is_none() {
                // `href={…}`: a string, a template, or an object with a literal pathname.
                let at = whole.end();
                match safe.as_bytes().get(at).copied() {
                    Some(b'"' | b'\'' | b'`') => literal = read_string_at(&safe, at),
                    Some(b'{') => literal = sticky_pathname_literal(&safe, at),
                    _ => {}
                }
            }
            let Some(literal) = literal else { continue };
            // An external `<a href>` is a link out of the site, not a transition.
            if tag == "a" && !literal.starts_with('/') {
                continue;
            }
            let Some(href) = to_href(Some(literal)) else { continue };
            let line = line_of.at(whole.start());
            let Some(component) = enclosing_fn(&nodes, line) else { continue };
            // A destination written as a choice names one route per arm, and the user reaches
            // every one of them — each is drawn.
            for dest in destinations_for_href(&href, &table.table) {
                if !budget.admit(&component.id, &dest.node.id) {
                    continue;
                }
                edges.push(navigates_edge(
                    &component.id,
                    &dest.node.id,
                    line,
                    None,
                    meta! {
                        "synthesizedBy" => "next-link", "href" => dest.href.display.as_str(),
                        "navMethod" => if tag == "a" { "a" } else { "link" }, "registeredAt" => format!("{file}:{line}"),
                    },
                ));
            }
        }
    }
    edges
}

// ── React Router: <Link to> and friends ───────────────────────────────────────

/// The tags that carry a route as a `to` attribute.
const LINK_TAGS: [&str; 6] = ["Link", "NavLink", "Navigate", "Redirect", "LinkContainer", "IndexLinkContainer"];

/// `<Tag … to=…>` for any of `tags`, the attribute anywhere in the tag.
fn link_tag_pattern(tags: &[String]) -> std::sync::Arc<Re> {
    // An attribute before `to` may hold an arrow (`onMouseEnter={() => …}`), whose `>` is not the
    // tag's end.
    let alt = tags.iter().map(|t| escape_regex(t)).collect::<Vec<_>>().join("|");
    Re::cached(&format!(r##"<({alt})\b((?:[^>]|=>)*?)\bto\s*=\s*(?:"([^"]*)"|'([^']*)'|(?=\{{))"##), "g")
}

#[derive(Default)]
struct LinkWrappers {
    /// File → the wrapper components it declares by name.
    named: HashMap<String, HashSet<String>>,
    /// Files whose default export is a wrapper.
    defaults: HashSet<String>,
}

/// Every styled-components / emotion wrapper of `Link` or `NavLink` in the project —
/// react-boilerplate's header links are `export default styled(Link)`…``, used as
/// `<HeaderLink to="/features">` from another file.
fn link_wrappers(ctx: &Ctx) -> LinkWrappers {
    let jsx_file = re!(r"\.(?:[cm]?[jt]sx?|mdx)$");
    let router_import = re!(r##"\bfrom\s*['"]react-router(?:-dom)?['"]"##);
    let styled_use = re!(r"\bstyled\s*\(\s*(?:Link|NavLink)\s*\)");
    let styled_const = re!(r"\b(?:export\s+)?(?:const|let)\s+([A-Z][\w$]*)\s*=\s*styled\s*\(\s*(?:Link|NavLink)\s*\)", "g");
    let styled_default = re!(r"\bexport\s+default\s+styled\s*\(\s*(?:Link|NavLink)\s*\)");
    let mut out = LinkWrappers::default();
    for file in ctx.get_all_files() {
        if !jsx_file.is_match(&file) || is_test_path(&file) {
            continue;
        }
        if !ctx.file_contains(&file, "styled") {
            continue;
        }
        let Some(source) = ctx.read_file(&file).filter(|s| !s.is_empty()) else { continue };
        if !styled_use.is_match(&source) || !router_import.is_match(&source) {
            continue;
        }
        if styled_default.is_match(&source) {
            out.defaults.insert(file.clone());
        }
        for m in styled_const.captures_iter(&source) {
            out.named.entry(file.clone()).or_default().insert(m.text(1).unwrap_or("").to_string());
        }
    }
    out
}

fn language_of(file: &str) -> Language {
    if file.ends_with(".tsx") {
        Language::Tsx
    } else if re!(r"\.[cm]?ts$").is_match(file) {
        Language::TypeScript
    } else if file.ends_with(".jsx") {
        Language::Jsx
    } else {
        Language::JavaScript
    }
}

/// The link tags `file` can write: React Router's own and the wrappers in scope there.
fn link_tags_for(file: &str, wrappers: &LinkWrappers, ctx: &Ctx) -> Vec<String> {
    let mut tags: Vec<String> = LINK_TAGS.iter().map(|t| t.to_string()).collect();
    if let Some(named) = wrappers.named.get(file) {
        tags.extend(named.iter().cloned());
    }
    if !wrappers.named.is_empty() || !wrappers.defaults.is_empty() {
        let lang = language_of(file);
        for m in ctx.get_import_mappings(file, lang).iter() {
            let Some(target) = resolve_import_path(&m.source, file, lang, ctx) else { continue };
            if (m.is_default && wrappers.defaults.contains(&target)) || wrappers.named.get(&target).is_some_and(|s| s.contains(&m.exported_name)) {
                tags.push(m.local_name.clone());
            }
        }
    }
    tags
}

pub fn react_router_link_edges(ctx: &Ctx) -> Vec<Edge> {
    let table = react_router_table(ctx);
    if table.by_root.is_empty() {
        return Vec::new();
    }
    let jsx_file = re!(r"\.(?:[cm]?[jt]sx?|mdx)$");
    let mut edges = Vec::new();
    let mut budget = LinkBudget::default();
    let wrappers = link_wrappers(ctx);
    for file in ctx.get_all_files() {
        if !jsx_file.is_match(&file) || is_test_path(&file) {
            continue;
        }
        let Some(routes) = routes_for_file(&table, &file).filter(|r| !r.exact.is_empty()) else { continue };
        let Some(source) = ctx.read_file(&file).filter(|s| re!(r"\bto\s*=").is_match(s)) else { continue };
        let tags = link_tags_for(&file, &wrappers, ctx);
        let pattern = link_tag_pattern(&tags);
        let alt = tags.iter().map(|t| escape_regex(t)).collect::<Vec<_>>().join("|");
        if !Re::cached(&format!(r"<(?:{alt})\b"), "").is_match(&source) {
            continue;
        }
        let safe = codegraph_text::strip_comments_for_regex(&source, "typescript");
        let nodes = ctx.get_nodes_in_file(&file);
        let line_of = make_line_at(&safe, 1);
        for m in pattern.captures_iter(&safe) {
            let whole = m.get(0).unwrap();
            let tag = m.text(1).unwrap_or("");
            let quoted: Option<&str> = m.text(3).or(m.text(4));
            let mut href: Option<HrefLiteral>;
            if let Some(q) = quoted {
                href = to_href(Some(q.to_string()));
            } else {
                // `to={…}` holds an EXPRESSION, read with the same reader the `history.push(…)`
                // path uses — a string, a template, a `{ pathname }` object, or a conditional
                // whose arms agree.
                let at = whole.end();
                let Some(close) = match_bracket(&safe, at) else { continue };
                let expr = &safe[at + 1..close];
                href = crate::frameworks::expo_router::parse_href_expression(expr);
                // `to={paths.app.discussion.getHref(id)}`: a route-config object's href.
                if href.is_none() {
                    if let Some(configured) = config_href_expression(expr, &file, ctx) {
                        href = crate::frameworks::expo_router::parse_href_expression(&configured);
                    }
                }
            }
            // A relative `to` is resolved against the route this markup renders under — a nesting
            // this scan does not read, so it is not a destination.
            let Some(href) = href.filter(|h| h.path.starts_with('/')) else { continue };
            let line = line_of.at(whole.start());
            let Some(component) = enclosing_fn(&nodes, line) else { continue };
            for dest in destinations_for_href(&href, routes) {
                if !budget.admit(&component.id, &dest.node.id) {
                    continue;
                }
                edges.push(navigates_edge(
                    &component.id,
                    &dest.node.id,
                    line,
                    None,
                    meta! {
                        "synthesizedBy" => "react-router-link", "href" => dest.href.display.as_str(),
                        "navMethod" => match tag { "Navigate" => "navigate", "Redirect" => "redirect", _ => "link" },
                        "registeredAt" => format!("{file}:{line}"),
                    },
                ));
            }
        }
    }
    edges
}

// ── TanStack Router: <Link to> / <Navigate to> ────────────────────────────────

pub fn tanstack_link_edges(ctx: &Ctx) -> Vec<Edge> {
    let table = tanstack_table(ctx);
    if table.by_root.is_empty() {
        return Vec::new();
    }
    let jsx_file = re!(r"\.(?:[cm]?[jt]sx?)$");
    let has_link_tag = re!(r"<(?:Link|Navigate)\b");
    let link_tag = re!(r##"<(Link|Navigate)\b([^>]*?)\bto\s*=\s*(?:"([^"]*)"|'([^']*)'|\{\s*)"##, "g");
    let mut edges = Vec::new();
    let mut budget = LinkBudget::default();
    for file in ctx.get_all_files() {
        if !jsx_file.is_match(&file) || is_test_path(&file) {
            continue;
        }
        let Some(routes) = routes_for_file(&table, &file).filter(|r| !r.exact.is_empty()) else { continue };
        let Some(source) = ctx.read_file(&file).filter(|s| has_link_tag.is_match(s)) else { continue };
        let safe = codegraph_text::strip_comments_for_regex(&source, "typescript");
        let nodes = ctx.get_nodes_in_file(&file);
        let line_of = make_line_at(&safe, 1);
        for m in link_tag.captures_iter(&safe) {
            let whole = m.get(0).unwrap();
            let mut literal: Option<String> = m.text(3).or(m.text(4)).map(String::from);
            if literal.is_none() {
                // `to={…}`: a string or a template.
                let at = whole.end();
                if matches!(safe.as_bytes().get(at).copied(), Some(b'"' | b'\'' | b'`')) {
                    literal = read_string_at(&safe, at);
                }
            }
            let Some(literal) = literal else { continue };
            let json = serde_json::to_string(&literal).unwrap_or_default();
            let Some(href) = tanstack_destination(&json) else { continue };
            let line = line_of.at(whole.start());
            let Some(component) = enclosing_fn(&nodes, line) else { continue };
            for dest in destinations_for_href(&href, routes) {
                if !budget.admit(&component.id, &dest.node.id) {
                    continue;
                }
                edges.push(navigates_edge(
                    &component.id,
                    &dest.node.id,
                    line,
                    None,
                    meta! {
                        "synthesizedBy" => "tanstack-link", "href" => dest.href.display.as_str(),
                        "navMethod" => if m.text(1) == Some("Navigate") { "navigate" } else { "link" },
                        "registeredAt" => format!("{file}:{line}"),
                    },
                ));
            }
        }
    }
    edges
}

// ── Vue Router: <router-link to> ──────────────────────────────────────────────

pub fn vue_router_link_edges(ctx: &Ctx) -> Vec<Edge> {
    let table = vue_route_table(ctx);
    if table.by_root.is_empty() {
        return Vec::new();
    }
    let template_file = re!(r"\.(?:vue|[cm]?[jt]sx?)$");
    let has_link_tag = re!(r"<(?:router-link|RouterLink|NuxtLink|nuxt-link)\b");
    let link_tag = re!(r##"<(router-link|RouterLink|NuxtLink|nuxt-link)\b([^>]*?)\s:?to\s*=\s*(?:"([^"]*)"|'([^']*)')"##, "g");
    let mut edges = Vec::new();
    let mut budget = LinkBudget::default();
    for file in ctx.get_all_files() {
        if !template_file.is_match(&file) || is_test_path(&file) {
            continue;
        }
        let Some(routes) = routes_for_file(&table, &file).filter(|r| !r.table.exact.is_empty()) else { continue };
        let Some(source) = ctx.read_file(&file).filter(|s| has_link_tag.is_match(s)) else { continue };
        let nodes = ctx.get_nodes_in_file(&file);
        let line_of = make_line_at(&source, 1);
        for m in link_tag.captures_iter(&source) {
            let whole = m.get(0).unwrap();
            // A bound `:to` holds an expression; a plain `to` holds a literal path.
            let value = js_trim(m.text(3).or(m.text(4)).unwrap_or(""));
            if value.is_empty() {
                continue;
            }
            let line = line_of.at(whole.start());
            let Some(component) = enclosing_fn(&nodes, line) else { continue };
            let bound = whole.as_str().contains(":to");
            let named = if bound { route_name_in_expression(value) } else { None };
            let by_name = named.as_ref().and_then(|n| routes.by_name.get(n));
            // A `{ name }` destination names exactly one route; a path may be written as a
            // choice, and then every arm is drawn.
            let destinations: Vec<(NodeRef, String)> = if let (Some(node), Some(name)) = (by_name, named.as_ref()) {
                vec![(node.clone(), name.clone())]
            } else {
                let href = if bound { parse_vue_path_object(value).or_else(|| to_href(read_string_at(value, 0))) } else { to_href(Some(value.to_string())) };
                let Some(href) = href.filter(|h| h.path.starts_with('/')) else { continue };
                destinations_for_href(&href, &routes.table).into_iter().map(|d| (d.node, d.href.display)).collect()
            };
            for (target, display) in destinations {
                if !budget.admit(&component.id, &target.id) {
                    continue;
                }
                let mut md = meta! { "synthesizedBy" => "vue-router-link", "href" => display.as_str(), "navMethod" => "link" };
                if named.as_ref().is_some_and(|n| routes.by_name.contains_key(n)) {
                    md.insert("by".to_string(), "name".into());
                }
                md.insert("registeredAt".to_string(), format!("{file}:{line}").into());
                edges.push(navigates_edge(&component.id, &target.id, line, None, md));
            }
        }
    }
    edges
}

// ── SvelteKit: <a href> and the page component a route is served by ───────────

pub fn svelte_kit_link_edges(ctx: &Ctx) -> Vec<Edge> {
    let table = svelte_kit_table(ctx);
    if table.by_root.is_empty() {
        return Vec::new();
    }
    let markup_file = re!(r"\.svelte$");
    let link_tag = re!(r##"<a\b([^>]*?)\bhref\s*=\s*(?:"([^"]*)"|'([^']*)'|\{\s*)"##, "g");
    let interpolation = re!(r"\{[^}]*\}", "g");
    let mut edges = Vec::new();
    let mut budget = LinkBudget::default();
    for file in ctx.get_all_files() {
        if !markup_file.is_match(&file) || is_test_path(&file) {
            continue;
        }
        let Some(routes) = routes_for_file(&table, &file).filter(|r| !r.exact.is_empty()) else { continue };
        let Some(source) = ctx.read_file(&file).filter(|s| s.contains("href")) else { continue };
        let nodes = ctx.get_nodes_in_file(&file);
        let line_of = make_line_at(&source, 1);
        for m in link_tag.captures_iter(&source) {
            let whole = m.get(0).unwrap();
            let mut literal: Option<String> = m.text(2).or(m.text(3)).map(String::from);
            if literal.is_none() {
                // `href={…}`: a string or a template with holes.
                let at = whole.end();
                if matches!(source.as_bytes().get(at).copied(), Some(b'"' | b'\'' | b'`')) {
                    literal = read_string_at(&source, at);
                }
            }
            let Some(literal) = literal else { continue };
            // An external href is a link out of the site, not a transition. A Svelte `{expr}`
            // inside a quoted attribute is an interpolation, so it becomes the same hole a
            // template literal's `${…}` does.
            if !literal.starts_with('/') {
                continue;
            }
            let hole = HOLE.to_string();
            let Some(href) = to_href(Some(interpolation.replace_all(&literal, |_| Some(hole.clone())))) else { continue };
            let line = line_of.at(whole.start());
            let Some(component) = enclosing_fn(&nodes, line) else { continue };
            for dest in destinations_for_href(&href, routes) {
                if !budget.admit(&component.id, &dest.node.id) {
                    continue;
                }
                edges.push(navigates_edge(
                    &component.id,
                    &dest.node.id,
                    line,
                    None,
                    meta! {
                        "synthesizedBy" => "sveltekit-link", "href" => dest.href.display.as_str(), "navMethod" => "a",
                        "registeredAt" => format!("{file}:{line}"),
                    },
                ));
            }
        }
    }
    edges
}

/// One `calls` edge from each `+page.svelte` route to the component in its own file — the page
/// that renders when a navigation lands there.
pub fn svelte_kit_page_component_edges(ctx: &Ctx) -> Vec<Edge> {
    let table = svelte_kit_table(ctx);
    if table.by_root.is_empty() {
        return Vec::new();
    }
    let mut pages: IndexMap<String, NodeRef> = IndexMap::new();
    for routes in table.by_root.values() {
        for page in routes.exact.values() {
            pages.entry(page.id.clone()).or_insert_with(|| page.clone());
        }
    }
    let mut edges = Vec::new();
    for page in pages.values() {
        let in_file = ctx.get_nodes_in_file(&page.file_path);
        let Some(component) = in_file.iter().find(|n| n.kind == NodeKind::Component) else { continue };
        edges.push(calls_edge(
            &page.id,
            &component.id,
            Some(component.start_line),
            meta! { "synthesizedBy" => "sveltekit-page", "registeredAt" => page.file_path.as_str() },
        ));
    }
    edges
}
