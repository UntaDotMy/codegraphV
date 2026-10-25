//! Angular Router — routes declared in `Routes` arrays, navigation written as command arrays. Port
//! of `src/resolution/frameworks/angular-router.ts`.
//!
//! ```text
//! export const routes: Routes = [
//!   { path: 'login', component: AuthComponent },
//!   { path: 'article/:slug', loadComponent: () => import('./article.component') },
//!   { path: 'editor', children: [{ path: ':slug', loadComponent: … }] },
//!   { path: 'profile', loadChildren: () => import('./profile/profile.routes') },
//! ];
//! ```
//!
//! `extract()` reads every such array — typed `Routes` / `Route[]`, passed to
//! `RouterModule.forRoot/forChild(…)` or `provideRouter(…)`, or a routes file's `export default
//! [...]` — into one `route` node per screen, named by its path the way every other framework's
//! routes are. Angular paths are relative: a child's path joins its parent's, and a `loadChildren`
//! file's routes sit under the path that lazy-loads them, which only a cross-file pass can know —
//! so `extract()` names a routes file's routes as if it were mounted at `/`, and `post_extract()`
//! puts each under its mount (following an NgModule `loadChildren` through to the routing module it
//! imports).
//!
//! A route with `children` is a layout, not a screen: its component renders an outlet the children
//! fill, and the `''` child is the screen at its address. It is a screen only when no child claims
//! that address. A `redirectTo` entry and a `**` catch-all are not screens either.
//!
//! A path written as a constant (`path: internalRoutes.account.path`) is read from the object the
//! constant names, `$localize` defaults included, in the same cross-file pass.
//!
//! A route binds to its component with a `references` edge. A lazy `loadComponent` names a file,
//! and its component is the `@Component` class that file exports.
//!
//! **Navigation is a command array.** `router.navigate(['/article', slug])` is `/article/${…}`;
//! `navigateByUrl('/login')`, a guard's `router.createUrlTree(['/login'])` and `parseUrl('/login')`
//! name a path the way every other router's calls do. A relative navigation (`relativeTo`) is left
//! unresolved rather than guessed. `routerLink` is markup, read by the template synthesizer.

use super::expo_router::{
    add_route_to, app_root_for, first_argument_text, nav_metadata, nth_argument_text, parse_href_expression, read_string_at, routes_for_file, to_href,
    HrefLiteral, RootedRouteTable, RouteTable, HOLE,
};
use super::nextjs::destinations_for_href;
use super::object_literal::{match_bracket, read_fields, skip_string, top_level_objects};
use super::now_ms;
use super::package_deps::depends_on;
use crate::ctx::Ctx;
use crate::import_resolver::resolve_import_path;
use crate::types::{AlsoTarget, FrameworkExtractionResult, FrameworkResolver, NavigationCalls, NodeList, NodeRef, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::{is_js_space, js_trim};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use indexmap::{IndexMap, IndexSet};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

// -- Reading a routes array ----------------------------------------------------------------------------------------

/// The component a route names: a class in scope (`spec` is `None`), or a lazily imported file's
/// (`name` `None` = its default export).
#[derive(Debug, Clone, PartialEq)]
pub struct AngularComponentRef {
    pub name: Option<String>,
    pub spec: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AngularRoute {
    /// `/editor/:slug` within this file; a constant path segment is kept as `{expr}` until the cross-file pass reads it.
    pub path: String,
    pub line: u32,
    pub component: Option<AngularComponentRef>,
    /// The layouts the screen renders inside, outermost first: each ancestor route's component, around its `<router-outlet>`.
    pub layouts: Vec<AngularComponentRef>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AngularRedirect {
    /// `/` within this file.
    pub from: String,
    /// Where it sends the user, within this file — or, when `absolute`, in the app: `/home`.
    pub to: String,
    pub absolute: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AngularMount {
    /// Where the lazily loaded routes sit within this file: `/profile`.
    pub prefix: String,
    /// The `loadChildren` import: `./profile/profile.routes`.
    pub spec: String,
    pub line: u32,
}

#[derive(Debug, Default)]
pub struct ParsedAngularRoutes {
    pub routes: Vec<AngularRoute>,
    pub mounts: Vec<AngularMount>,
    pub redirects: Vec<AngularRedirect>,
}

/// Where a routes array opens: typed `Routes` / `Route[]`, handed to `RouterModule.forRoot/forChild`
/// or `provideRouter`, or a routes file's default export. `children: [` is reached by walking an
/// entry, never here.
fn array_openers_re() -> std::sync::Arc<Re> {
    Re::cached(
        r"(?::\s*(?:Routes|Route\s*\[\s*\])\s*=\s*|\bRouterModule\s*\.\s*for(?:Root|Child)\s*\(\s*|\bprovideRouter\s*\(\s*|\bexport\s+default\s+(?:<\s*Routes\s*>\s*)?|\b(?:const|let)\s+[A-Za-z_$][\w$]*\s*=\s*)\[",
        "g",
    )
}

/// The fields that make an object a route rather than any object in an array.
const ROUTE_KEYS: [&str; 5] = ["component", "loadComponent", "children", "loadChildren", "redirectTo"];

/// A path value the pass can name later: `internalRoutes.account.path`.
fn constant_path_re() -> &'static Re {
    re!(r"^[A-Za-z_$][\w$]*(?:\s*\.\s*[A-Za-z_$][\w$]*)+$")
}

/// A `$localize` tagged template's default text: `` $localize`:kebab-case@@routes.about:about` ``
/// is `about`.
pub fn localize_default(text: &str) -> Option<String> {
    let trimmed = js_trim(text);
    let m = re!(r"^\$localize\s*`((?:[^`\\$]|\\.)*)`$").captures(trimmed)?;
    let body = m.text(1)?;
    // `:meaning|description@@id:text` — the metadata block is the leading `:…:`.
    Some(match re!(r"^:[^:]*:").find(body) {
        Some(meta) => body[meta.end()..].to_string(),
        None => body.to_string(),
    })
}

/// A string written statically: a literal, a `$localize` default, or `+` between those —
/// `'/' + $localize`:…:about``, as Ghostfolio builds its links. `None` when any part is computed.
pub fn static_string(text: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut rest: &str = js_trim(text);
    while !rest.is_empty() {
        let mut part: Option<String> = None;
        let mut used = 0usize;
        let q = rest.as_bytes()[0];
        if q == b'"' || q == b'\'' {
            part = read_string_at(rest, 0);
            used = if part.is_none() { 0 } else { skip_string(rest, 0).map_or(0, |e| e + 1) };
        } else if rest.starts_with("$localize") {
            let end = rest.find('`').and_then(|tick| skip_string(rest, tick));
            if let Some(end) = end.filter(|e| *e > 0) {
                part = localize_default(&rest[..end + 1]);
                used = end + 1;
            }
        } else if q == b'`' {
            let end = skip_string(rest, 0).filter(|e| *e > 0);
            let body = end.map_or("", |e| &rest[1..e]);
            if let Some(end) = end {
                if !body.contains("${") {
                    part = Some(body.to_string());
                    used = end + 1;
                }
            }
        }
        let part = part?;
        if used == 0 {
            return None;
        }
        parts.push(part);
        rest = js_trim(&rest[used..]);
        if rest.is_empty() {
            break;
        }
        if !rest.starts_with('+') {
            return None;
        }
        rest = js_trim(&rest[1..]);
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.concat())
    }
}

fn non_empty_segments(s: &str) -> Vec<String> {
    s.split('/').filter(|seg| !seg.is_empty()).map(String::from).collect()
}

fn strip_whitespace(s: &str) -> String {
    re!(r"\s+").replace_all(s, |_| Some(String::new()))
}

/// A route's own path segment(s): a string, a `$localize` default, or a constant kept as `{expr}`;
/// `None` when it cannot be named.
fn path_segments(text: &str) -> Option<Vec<String>> {
    let trimmed = js_trim(text);
    if let Some(value) = static_string(trimmed) {
        if value.contains("**") {
            return None;
        }
        return Some(non_empty_segments(&value));
    }
    if constant_path_re().is_match(trimmed) {
        return Some(vec![format!("{{{}}}", strip_whitespace(trimmed))]);
    }
    // `` `issue/:${ProjectConst.IssueId}` ``: a template whose holes are constants.
    if trimmed.starts_with('`') && skip_string(trimmed, 0) == Some(trimmed.len() - 1) {
        let mut ok = true;
        let body = re!(r"\$\{([^}]*)\}").replace_all(&trimmed[1..trimmed.len() - 1], |c| {
            let hole = c.text(1).unwrap_or("");
            if !constant_path_re().is_match(js_trim(hole)) {
                ok = false;
            }
            Some(format!("{{{}}}", strip_whitespace(hole)))
        });
        if ok && !body.contains("**") {
            return Some(non_empty_segments(&body));
        }
        return None;
    }
    // `internalRoutes.account.subRoutes.access.path + '/:id'`: each part a string or a constant,
    // the constant a whole segment of its own.
    if trimmed.contains('+') {
        let mut joined = String::new();
        for part in split_plus(trimmed) {
            if let Some(literal) = static_string(&part) {
                joined.push_str(&literal);
            } else if constant_path_re().is_match(js_trim(&part)) {
                joined.push_str(&format!("{{{}}}", strip_whitespace(js_trim(&part))));
            } else {
                return None;
            }
        }
        return if joined.contains("**") { None } else { Some(non_empty_segments(&joined)) };
    }
    None
}

/// An expression's `+` operands at depth 0, strings and brackets stepped over.
fn split_plus(text: &str) -> Vec<String> {
    let b = text.as_bytes();
    let mut parts: Vec<String> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        let ch = b[i];
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            match skip_string(text, i) {
                Some(end) => i = end,
                None => return vec![text.to_string()],
            }
        } else if ch == b'(' || ch == b'[' || ch == b'{' {
            match match_bracket(text, i) {
                Some(end) => i = end,
                None => return vec![text.to_string()],
            }
        } else if ch == b'+' {
            parts.push(text[start..i].to_string());
            start = i + 1;
        }
        i += 1;
    }
    parts.push(text[start..].to_string());
    parts
}

/// The component a `component:` / `loadComponent:` value names.
fn component_ref(field: &str, text: &str) -> Option<AngularComponentRef> {
    if field == "component" {
        let ident = re!(r"^\s*([A-Z][\w$]*)\s*$").captures(text)?;
        return Some(AngularComponentRef { name: Some(ident.text(1)?.to_string()), spec: None });
    }
    let (spec, member) = lazy_import(text)?;
    Some(AngularComponentRef { name: member, spec: Some(spec) })
}

/// `() => import('./x')` → `./x`; `.then(m => m.X)` / `.then((c) => c.X)` names the member.
fn lazy_import(text: &str) -> Option<(String, Option<String>)> {
    let imp = re!(r#"\bimport\s*\(\s*(['"`])([^'"`]+)\1\s*\)"#).captures(text)?;
    let whole = imp.get(0)?;
    let after = &text[whole.end()..];
    let then = re!(r"^\s*\.then\s*\(\s*\(?\s*([A-Za-z_$][\w$]*)\s*\)?\s*=>\s*\(?\s*\1\s*\.\s*([A-Za-z_$][\w$]*)").captures(after);
    // `async () => (await import('./x')).HomeModule`
    let awaited = re!(r"^\s*\)\s*\.\s*([A-Za-z_$][\w$]*)").captures(after);
    let member = match (&then, &awaited) {
        (Some(t), _) => t.text(2).map(String::from),
        (None, Some(a)) => a.text(1).map(String::from),
        (None, None) => None,
    };
    Some((imp.text(2)?.to_string(), member))
}

fn join_path(segs: &[String]) -> String {
    format!("/{}", segs.join("/"))
}

struct Walk<'a> {
    safe: &'a str,
    out: ParsedAngularRoutes,
    seen_paths: HashSet<String>,
    walked: HashSet<usize>,
}

impl Walk<'_> {
    fn line_of(&self, at: usize) -> u32 {
        self.safe.as_bytes()[..at.min(self.safe.len())].iter().filter(|c| **c == b'\n').count() as u32 + 1
    }

    fn walk(&mut self, open: usize, close: usize, prefix: &[String], depth: usize, layouts: &[AngularComponentRef]) {
        if self.walked.contains(&open) || depth > 12 {
            return;
        }
        self.walked.insert(open);
        let safe = self.safe;
        for obj in top_level_objects(safe, open + 1, close) {
            let fields = read_fields(safe, obj.0, obj.1);
            if !ROUTE_KEYS.iter().any(|k| fields.contains_key(*k)) {
                continue;
            }
            // A custom `matcher` decides the address at run time — no path to name.
            if fields.contains_key("matcher") {
                continue;
            }
            let path_field = fields.get("path");
            // `{ path: '**', redirectTo: 'home' }`: where an address nothing else matches lands.
            if let (Some(pf), Some(redirect_to)) = (path_field, fields.get("redirectTo")) {
                if static_string(&pf.text).as_deref() == Some("**") {
                    if let Some(target) = static_string(&redirect_to.text).filter(|t| !t.contains("**")) {
                        let absolute = target.starts_with('/');
                        let mut from = prefix.to_vec();
                        from.push("**".to_string());
                        let mut to: Vec<String> = if absolute { Vec::new() } else { prefix.to_vec() };
                        to.extend(non_empty_segments(&target));
                        self.out.redirects.push(AngularRedirect { from: join_path(&from), to: join_path(&to), absolute });
                    }
                    continue;
                }
            }
            let own: Vec<String> = match path_field {
                Some(pf) => match path_segments(&pf.text) {
                    Some(own) => own,
                    None => continue, // a computed path, or a `**` catch-all
                },
                None => Vec::new(),
            };
            let mut segs = prefix.to_vec();
            segs.extend(own);
            let line = self.line_of(path_field.map_or(obj.0, |pf| pf.at));

            if let Some(redirect_to) = fields.get("redirectTo") {
                // `{ path: '', redirectTo: 'home' }`: arriving at `/` is arriving at `/home`. A
                // relative target is a sibling, under the same parent.
                if let Some(target) = static_string(&redirect_to.text).filter(|t| !t.contains("**")) {
                    let absolute = target.starts_with('/');
                    let mut to: Vec<String> = if absolute { Vec::new() } else { prefix.to_vec() };
                    to.extend(non_empty_segments(&target));
                    self.out.redirects.push(AngularRedirect { from: join_path(&segs), to: join_path(&to), absolute });
                }
                continue;
            }

            if let Some(load_children) = fields.get("loadChildren") {
                if let Some((spec, _)) = lazy_import(&load_children.text) {
                    self.out.mounts.push(AngularMount { prefix: join_path(&segs), spec, line });
                }
                continue;
            }
            let component_field: Option<&str> = if fields.contains_key("component") {
                Some("component")
            } else if fields.contains_key("loadComponent") {
                Some("loadComponent")
            } else {
                None
            };
            let component = component_field.and_then(|f| component_ref(f, &fields[f].text));

            let children = fields.get("children");
            let mut child_claims_address = false;
            if let Some(children) = children {
                let child_open = safe.as_bytes().get(children.at..).and_then(|rest| rest.iter().position(|b| *b == b'[')).map(|i| i + children.at);
                let child_close = child_open.and_then(|o| match_bracket(safe, o));
                if let (Some(child_open), Some(child_close)) = (child_open, child_close) {
                    if child_close > child_open {
                        for child in top_level_objects(safe, child_open + 1, child_close) {
                            let child_fields = read_fields(safe, child.0, child.1);
                            match child_fields.get("path") {
                                None => child_claims_address = true,
                                Some(cp) => {
                                    if path_segments(&cp.text).is_some_and(|s| s.is_empty()) {
                                        child_claims_address = true;
                                    }
                                }
                            }
                        }
                        let mut inner: Vec<AngularComponentRef> = layouts.to_vec();
                        if let Some(c) = &component {
                            inner.push(c.clone());
                        }
                        self.walk(child_open, child_close, &segs, depth + 1, &inner);
                    }
                }
            }
            // A layout is a screen only where no child claims its address; a redirect and a
            // componentless grouping are no screen at all.
            if component_field.is_none() || (children.is_some() && child_claims_address) {
                continue;
            }
            let route = join_path(&segs);
            if !self.seen_paths.insert(route.clone()) {
                continue;
            }
            self.out.routes.push(AngularRoute { path: route, line, component, layouts: layouts.to_vec() });
        }
    }
}

/// Every screen and lazy mount a file's routes arrays declare. Entries are walked as objects, never
/// read out of a window of text: `path` may be written after `component`.
pub fn parse_angular_routes(content: &str) -> ParsedAngularRoutes {
    // A file that could hold a routes array — the cheap gate before scanning.
    if !re!(r#"['"]@angular\/router['"]"#).is_match(content) {
        return ParsedAngularRoutes::default();
    }
    let safe = codegraph_text::strip_comments_for_regex(content, "typescript");
    let mut walk = Walk { safe: &safe, out: ParsedAngularRoutes::default(), seen_paths: HashSet::new(), walked: HashSet::new() };

    for m in array_openers_re().captures_iter(&safe) {
        let whole = m.get(0).expect("group 0");
        let open = whole.end() - 1;
        let Some(close) = match_bracket(&safe, open) else { continue };
        // `const x = [` is a routes array only when it says so: `] as Routes` / `] satisfies Routes`.
        if re!(r"^\b(?:const|let)\b").is_match(whole.as_str()) {
            let tail: String = safe[close..].chars().take(32).collect();
            if !re!(r"^\]\s*(?:as|satisfies)\s+Routes\b").is_match(&tail) {
                continue;
            }
        }
        walk.walk(open, close, &[], 0, &[]);
    }
    walk.out
}

/// The id a config-declared route carries — reconstructed exactly, so the resolver recognises its own.
fn route_id(file_path: &str, line: u32, in_file_path: &str) -> String {
    format!("route:{file_path}:{line}:{in_file_path}:angular")
}

/// True for a route node this resolver emitted.
pub fn is_angular_route(node: &Node) -> bool {
    node.kind == NodeKind::Route && node.id.ends_with(":angular") && node.id == route_id(&node.file_path, node.start_line, &in_file_path(node))
}

/// The path a route has within its own file, before any mount — kept in the qualified name so the
/// cross-file pass is idempotent.
fn in_file_path(node: &Node) -> String {
    let marker = "::route:";
    match node.qualified_name.find(marker) {
        None => node.name.clone(),
        Some(at) => node.qualified_name[at + marker.len()..].to_string(),
    }
}

// -- The component a file lazily exports ----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ComponentClass {
    pub name: String,
    pub is_default: bool,
    pub decorator_at: usize,
}

/// The `@Component` classes a file declares, in order, with whether each is the default export.
pub fn component_classes_in(content: &str) -> Vec<ComponentClass> {
    let mut out: Vec<ComponentClass> = Vec::new();
    let safe = codegraph_text::strip_comments_for_regex(content, "typescript");
    let component_class = Re::cached(r"@Component\s*\(", "g");
    let mut pos = 0usize;
    while pos <= safe.len() {
        let Some(m) = component_class.captures_at(&safe, pos) else { break };
        let whole = m.get(0).expect("group 0");
        let open = whole.end() - 1;
        pos = whole.end().max(pos + 1);
        let Some(close) = match_bracket(&safe, open) else { continue };
        let window: String = safe.get(close + 1..).unwrap_or("").chars().take(399).collect();
        if let Some(after) = re!(r"^\s*(export\s+)?(default\s+)?(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)").captures(&window) {
            out.push(ComponentClass {
                name: after.text(3).unwrap_or("").to_string(),
                is_default: after.text(2).is_some_and(|t| !t.is_empty()),
                decorator_at: whole.start(),
            });
        }
        pos = close;
    }
    out
}

/// The component class a lazily loaded file provides: the member named, else its default export,
/// else its only component.
fn lazy_component(spec: &str, member: Option<&str>, from_file: &str, ctx: &Ctx) -> Option<NodeRef> {
    let file = resolve_import_path(spec, from_file, Language::TypeScript, ctx)?;
    let mut name: Option<String> = member.map(String::from);
    if name.is_none() {
        let content = ctx.read_file(&file).filter(|c| !c.is_empty())?;
        let classes = component_classes_in(&content);
        name = classes
            .iter()
            .find(|c| c.is_default)
            .or(if classes.len() == 1 { classes.first() } else { None })
            .map(|c| c.name.clone());
        if name.is_none() {
            name = re!(r"\bexport\s+default\s+class\s+([A-Za-z_$][\w$]*)").captures(&content).and_then(|c| c.text(1).map(String::from));
        }
    }
    let name = name.filter(|n| !n.is_empty())?;
    ctx.get_nodes_in_file(&file).iter().find(|n| n.kind == NodeKind::Class && n.name == name).cloned()
}

/// A path's segments with each `{expr}` read from the constant it names, as seen from `file` —
/// where the route (or the mount) is written.
fn resolved_segments(path_text: &str, file: &str, ctx: &Ctx) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for seg in path_text.split('/').filter(|seg| !seg.is_empty()) {
        if seg.starts_with('{') && seg.ends_with('}') && seg[1..].find('{').is_none() {
            match constant_value(&seg[1..seg.len() - 1], file, ctx) {
                None => out.push(seg.to_string()),
                Some(value) => out.extend(non_empty_segments(&value)),
            }
            continue;
        }
        // `:{ProjectConst.IssueId}` — a constant inside a segment.
        out.push(re!(r"\{([^{}]+)\}").replace_all(seg, |c| {
            let expr = c.text(1).unwrap_or("");
            Some(constant_value(expr, file, ctx).unwrap_or_else(|| c.get(0).map_or("", |w| w.as_str()).to_string()))
        }));
    }
    out
}

/// A route's lazily loaded component: `import:./x.component#default` / `#XComponent`.
fn lazy_component_ref_re() -> &'static Re {
    re!(r"^import:([^#]+)#([\w$]+)$")
}

/// A layout a screen renders inside: `layout:ProfileComponent`, `layout:import:./x#default`.
fn layout_ref_re() -> &'static Re {
    re!(r"^layout:(.+)$")
}

/// The class a route's component names, by its import (or this file) — or a lazy import's.
fn route_component(encoded: &str, from_file: &str, ctx: &Ctx) -> Option<NodeRef> {
    if let Some(lazy) = lazy_component_ref_re().captures(encoded) {
        let member = lazy.text(2).filter(|m| *m != "default");
        return lazy_component(lazy.text(1).unwrap_or(""), member, from_file, ctx);
    }
    let file = declaring_file(encoded, from_file, ctx)?;
    ctx.get_nodes_in_file(&file).iter().find(|n| n.kind == NodeKind::Class && n.name == encoded).cloned()
}

// -- The cross-file pass: mounts and constant paths -----------------------------------------------------------------

/// The file a routes import names, and — for an NgModule — the routing modules it imports. A barrel
/// is looked through first: an Nx library is imported by its alias, which names the library's
/// `src/index.ts`, whose `export * from './lib/home.module'` names the module.
fn route_files_loaded_by(spec: &str, from_file: &str, ctx: &Ctx, route_files: &HashSet<String>, mount_files: &IndexSet<String>) -> Vec<String> {
    let target = resolve_import_path(spec, from_file, Language::TypeScript, ctx);
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    // A barrel can re-export another barrel; a few hops settle it.
    let mut barrels: Vec<String> = target.into_iter().collect();
    let mut hop = 0;
    while hop < 4 && !barrels.is_empty() {
        let mut next: Vec<String> = Vec::new();
        for file in &barrels {
            if !seen.insert(file.clone()) {
                continue;
            }
            if route_files.contains(file) || mount_files.contains(file) {
                out.push(file.clone());
                continue;
            }
            let Some(content) = ctx.read_file(file).filter(|c| !c.is_empty()) else { continue };
            // `loadChildren: () => import('./layout/layout.module').then(m => m.LayoutModule)`: the
            // module holds no routes; the routing module it imports does.
            if re!(r"@NgModule\s*\(").is_match(&content) {
                for m in Re::cached(r#"\bimport\s+[^'"]*?from\s+(['"])([^'"]+)\1"#, "g").captures_iter(&content) {
                    let imported = resolve_import_path(m.text(2).unwrap_or(""), file, Language::TypeScript, ctx);
                    if let Some(imported) = imported {
                        if (route_files.contains(&imported) || mount_files.contains(&imported)) && !out.contains(&imported) {
                            out.push(imported);
                        }
                    }
                }
                continue;
            }
            for m in Re::cached(r#"\bexport\s+(?:\*|\{[^}]*\})\s*(?:as\s+[\w$]+\s*)?from\s+(['"])([^'"]+)\1"#, "g").captures_iter(&content) {
                if let Some(reexported) = resolve_import_path(m.text(2).unwrap_or(""), file, Language::TypeScript, ctx) {
                    if !seen.contains(&reexported) {
                        next.push(reexported);
                    }
                }
            }
        }
        barrels = next;
        hop += 1;
    }
    out
}

/// Per context, renewed with the route list (whose identity changes when the resolver's caches do).
#[derive(Default)]
struct ConstantTexts(RefCell<ConstantTextsInner>);

#[derive(Default)]
struct ConstantTextsInner {
    source: Option<NodeList>,
    memo: HashMap<String, Option<String>>,
}

/// The source text a constant member chain names — `internalRoutes.account.path` is the `path:`
/// field of `account` in `export const internalRoutes = {…}` — read from the file the chain's root
/// is imported from (or declared in).
pub fn constant_text(expr: &str, from_file: &str, ctx: &Ctx) -> Option<String> {
    let source = ctx.get_nodes_by_kind(NodeKind::Route);
    let cell = ctx.memo::<ConstantTexts>();
    {
        let mut inner = cell.0.borrow_mut();
        if inner.source.as_ref().is_none_or(|s| !Rc::ptr_eq(s, &source)) {
            inner.source = Some(source);
            inner.memo.clear();
        }
    }
    let key = format!("{from_file}\0{expr}");
    if let Some(hit) = cell.0.borrow().memo.get(&key) {
        return hit.clone();
    }
    // `const { create } = internalRoutes.accounts.subRoutes;` / `const accounts =
    // internalRoutes.accounts;` make `create.path` a chain rooted at the import.
    let aliased = local_alias(expr, from_file, ctx);
    if let Some(aliased) = aliased.filter(|a| a != expr) {
        let value = constant_text(&aliased, from_file, ctx);
        cell.0.borrow_mut().memo.insert(key, value.clone());
        return value;
    }
    let mut pieces = expr.split('.').map(|part| js_trim(part).to_string());
    let root = pieces.next().unwrap_or_default();
    let chain: Vec<String> = pieces.collect();
    let mut value: Option<String> = None;
    let file = if root.is_empty() { None } else { declaring_file(&root, from_file, ctx) };
    let content = match (&file, root.is_empty()) {
        (Some(file), false) => declaration_source(&root, file, ctx, 0),
        _ => None,
    };
    if let (Some(content), false, true) = (&content, root.is_empty(), !chain.is_empty()) {
        let safe = codegraph_text::strip_comments_for_regex(content, "typescript");
        let esc_root = escape_regex(&root);
        let decl = Re::new(&format!(r"\b(?:const|let)\s+{esc_root}\s*(?::[^=]+)?=\s*\{{"), "").find(&safe);
        let enum_decl = if decl.is_some() { None } else { Re::new(&format!(r"\benum\s+{esc_root}\s*\{{"), "").find(&safe) };
        let class_decl = if decl.is_some() || enum_decl.is_some() { None } else { Re::new(&format!(r"\bclass\s+{esc_root}\b[^{{]*\{{"), "").find(&safe) };
        if let Some(decl) = decl {
            value = read_chain(&safe, decl.end() - 1, &chain);
        } else if let (Some(enum_decl), true) = (enum_decl, chain.len() == 1) {
            // `enum Paths { Board = 'board' }`
            let open = enum_decl.end() - 1;
            let end = match_bracket(&safe, open).unwrap_or(open).max(open);
            let body = &safe[open..end];
            value = Re::new(&format!(r#"\b{}\s*=\s*(['"`][^'"`]*['"`])"#, escape_regex(&chain[0])), "")
                .captures(body)
                .and_then(|c| c.text(1).map(String::from));
        } else if let Some(class_decl) = class_decl {
            // `class ProjectConst { static readonly IssueId = 'issueId' }`, and
            // `class RouterUtil { static Configuration = { Visualizer: 'visualizer' } }`.
            let open = class_decl.end() - 1;
            let close = match_bracket(&safe, open);
            let body = match close {
                Some(close) if close > open => &safe[open..close],
                _ => "",
            };
            let field = Re::new(&format!(r"\bstatic\s+(?:readonly\s+)?{}\s*(?::[^=;]+)?=\s*", escape_regex(&chain[0])), "").find(body);
            if let Some(field) = field {
                let at = open + field.end();
                if safe.as_bytes().get(at) == Some(&b'{') {
                    value = if chain.len() > 1 { read_chain(&safe, at, &chain[1..]) } else { None };
                } else if chain.len() == 1 {
                    value = re!(r#"^(['"`])[^'"`]*\1"#).find(&safe[at..]).map(|m| m.as_str().to_string());
                }
            }
        }
    }
    cell.0.borrow_mut().memo.insert(key, value.clone());
    value
}

/// Follow `links` down the nested object literal opening at `open` in `safe`; the last link's text.
fn read_chain(safe: &str, open: usize, links: &[String]) -> Option<String> {
    let mut start = open;
    let mut end = match_bracket(safe, start);
    let mut i = 0;
    while i < links.len() {
        let Some(e) = end.filter(|e| *e > start) else { break };
        let fields = read_fields(safe, start, e);
        let field = fields.get(&links[i])?;
        if i == links.len() - 1 {
            return Some(js_trim(&field.text).to_string());
        }
        let next_start = safe.as_bytes().get(field.at..).and_then(|rest| rest.iter().position(|b| *b == b'{')).map(|p| p + field.at);
        match next_start {
            Some(s) => {
                start = s;
                end = match_bracket(safe, s);
            }
            None => end = None,
        }
        i += 1;
    }
    None
}

/// The source that declares `root`: `file` itself, or — when `file` is a barrel (an Nx library's
/// `src/index.ts`) — the module it re-exports it from.
fn declaration_source(root: &str, file: &str, ctx: &Ctx, hops: usize) -> Option<String> {
    let content = ctx.read_file(file).filter(|c| !c.is_empty())?;
    if Re::new(&format!(r"\b(?:const|let|class|enum)\s+{}\b", escape_regex(root)), "").is_match(&content) {
        return Some(content.to_string());
    }
    if hops >= 3 {
        return None;
    }
    for m in Re::cached(r#"\bexport\s+(\*|\{[^}]*\})\s*from\s*(['"])([^'"]+)\2"#, "g").captures_iter(&content) {
        let names = m.text(1).unwrap_or("");
        if names != "*" && !Re::new(&format!(r"\b{}\b", escape_regex(root)), "").is_match(names) {
            continue;
        }
        let target = resolve_import_path(m.text(3).unwrap_or(""), file, Language::TypeScript, ctx);
        if let Some(found) = target.and_then(|t| declaration_source(root, &t, ctx, hops + 1)) {
            return Some(found);
        }
    }
    None
}

/// `create.path` as the chain its root was destructured from in `from_file`, or `None` when the root
/// is no local alias.
fn local_alias(expr: &str, from_file: &str, ctx: &Ctx) -> Option<String> {
    let content = ctx.read_file(from_file).filter(|c| !c.is_empty())?;
    let mut pieces = expr.split('.');
    let root = pieces.next().filter(|r| !r.is_empty())?;
    let rest: Vec<&str> = pieces.collect();
    let rebuild = |head: String| -> String {
        let mut all = vec![head];
        all.extend(rest.iter().map(|s| s.to_string()));
        all.join(".")
    };
    let destructure = Re::cached(r"\b(?:const|let)\s*\{([^}]*)\}\s*=\s*([A-Za-z_$][\w$]*(?:\s*\.\s*[A-Za-z_$][\w$]*)*)\s*;?", "g");
    for m in destructure.captures_iter(&content) {
        for binding in m.text(1).unwrap_or("").split(',') {
            let mut sides = binding.split(':').map(|part| js_trim(part).to_string());
            let prop = sides.next().unwrap_or_default();
            let local = sides.next();
            if local.as_deref().unwrap_or(&prop) == root && !prop.is_empty() {
                return Some(rebuild(format!("{}.{}", strip_whitespace(m.text(2).unwrap_or("")), prop)));
            }
        }
    }
    let simple = Re::new(
        &format!(r"\b(?:const|let)\s+{}\s*=\s*([A-Za-z_$][\w$]*(?:\s*\.\s*[A-Za-z_$][\w$]*)+)\s*;", escape_regex(root)),
        "",
    )
    .captures(&content);
    simple.map(|s| rebuild(strip_whitespace(s.text(1).unwrap_or(""))))
}

/// A constant path's value: `internalRoutes.account.path` → `account`.
fn constant_value(expr: &str, from_file: &str, ctx: &Ctx) -> Option<String> {
    constant_text(expr, from_file, ctx).and_then(|text| static_string(&text))
}

/// The file declaring `name` for code in `from_file`: its import, else the file itself.
fn declaring_file(name: &str, from_file: &str, ctx: &Ctx) -> Option<String> {
    let content = ctx.read_file(from_file).filter(|c| !c.is_empty())?;
    for m in Re::cached(r#"\bimport\s*\{([^}]*)\}\s*from\s*(['"])([^'"]+)\2"#, "g").captures_iter(&content) {
        let names_match = m.text(1).unwrap_or("").split(',').any(|s| {
            let piece = js_trim(s);
            let last = re!(r"\s+as\s+").split(piece).pop().unwrap_or(piece);
            js_trim(last) == name
        });
        if names_match {
            return resolve_import_path(m.text(3).unwrap_or(""), from_file, Language::TypeScript, ctx);
        }
    }
    Some(from_file.to_string())
}

// -- Route table ------------------------------------------------------------------------------------------------------

pub struct AngularMounts {
    /// Every file that lazy-loads routes, whether or not it declares a screen of its own.
    pub mount_files: IndexSet<String>,
    /// Target file → the file that mounts it, and the prefix of the mount.
    loaded_by: HashMap<String, (String, String)>,
    memo: RefCell<HashMap<String, Vec<String>>>,
}

impl AngularMounts {
    /// The path segments a file's routes sit under: its parent's prefix plus the mount's.
    pub fn prefix_of(&self, file: &str, ctx: &Ctx) -> Vec<String> {
        self.prefix_inner(file, &mut HashSet::new(), ctx)
    }

    fn prefix_inner(&self, file: &str, seen: &mut HashSet<String>, ctx: &Ctx) -> Vec<String> {
        if let Some(hit) = self.memo.borrow().get(file) {
            return hit.clone();
        }
        let mut prefix: Vec<String> = Vec::new();
        if let Some((parent, mount_prefix)) = self.loaded_by.get(file) {
            if !seen.contains(file) {
                seen.insert(file.to_string());
                prefix = self.prefix_inner(parent, seen, ctx);
                prefix.extend(resolved_segments(mount_prefix, parent, ctx));
            }
        }
        self.memo.borrow_mut().insert(file.to_string(), prefix.clone());
        prefix
    }
}

#[derive(Default)]
struct MountMemo(RefCell<Option<(NodeList, Rc<AngularMounts>)>>);

/// Where every routes file is mounted — the `loadChildren` chain from the app down, settled from
/// the top. Shared by the cross-file pass, which names the routes, and the route table, which reads
/// the redirects in files that only mount others (jira-clone's `app.routes.ts` holds nothing but two
/// mounts and `'' → 'project'`).
pub fn angular_mounts(ctx: &Ctx, routes: &[NodeRef]) -> Rc<AngularMounts> {
    let source = ctx.get_nodes_by_kind(NodeKind::Route);
    let memo = ctx.memo::<MountMemo>();
    if let Some((cached_source, mounts)) = memo.0.borrow().as_ref() {
        if Rc::ptr_eq(cached_source, &source) {
            return Rc::clone(mounts);
        }
    }
    let route_files: HashSet<String> = routes.iter().map(|r| r.file_path.clone()).collect();
    let mut mounts_by_file: IndexMap<String, Vec<AngularMount>> = IndexMap::new();
    for file in ctx.get_all_files().iter() {
        if !re!(r"\.[cm]?ts$").is_match(file) || !ctx.file_contains(file, "loadChildren") {
            continue;
        }
        let Some(content) = ctx.read_file(file).filter(|c| !c.is_empty()) else { continue };
        let parsed = parse_angular_routes(&content);
        if !parsed.mounts.is_empty() {
            mounts_by_file.insert(file.clone(), parsed.mounts);
        }
    }
    let mount_files: IndexSet<String> = mounts_by_file.keys().cloned().collect();
    let mut loaded_by: HashMap<String, (String, String)> = HashMap::new();
    for (file, mounts) in &mounts_by_file {
        for mount in mounts {
            for target in route_files_loaded_by(&mount.spec, file, ctx, &route_files, &mount_files) {
                // A file mounted from two places keeps its first mount.
                if target != *file && !loaded_by.contains_key(&target) {
                    loaded_by.insert(target, (file.clone(), mount.prefix.clone()));
                }
            }
        }
    }
    let mounts = Rc::new(AngularMounts { mount_files, loaded_by, memo: RefCell::new(HashMap::new()) });
    *memo.0.borrow_mut() = Some((source, Rc::clone(&mounts)));
    mounts
}

#[derive(Default)]
struct WorkspaceRoots(RefCell<HashMap<String, Option<String>>>);

/// The app a routes file belongs to. An Angular workspace — the directory an `angular.json` or an
/// Nx `nx.json` sits in — is one app however its routes are split: angular-spotify declares its
/// screens across a dozen `libs/*` packages, each with a `src/` of its own, and keyed by those every
/// template's `routerLink` looked for its routes in the wrong table. Outside a workspace, the
/// conventional app root.
pub fn angular_app_root(file_path: &str, ctx: &Ctx) -> String {
    let memo = ctx.memo::<WorkspaceRoots>();
    let dir: &str = file_path.rfind('/').map_or("", |slash| &file_path[..slash]);
    let cached = memo.0.borrow().get(dir).cloned();
    let found = match cached {
        Some(found) => found,
        None => {
            let mut found: Option<String> = None;
            let mut d: Option<String> = Some(dir.to_string());
            while let Some(current) = d {
                let at = if current.is_empty() { String::new() } else { format!("{current}/") };
                if ctx.file_exists(&format!("{at}angular.json")) || ctx.file_exists(&format!("{at}nx.json")) {
                    found = Some(at);
                    break;
                }
                d = if current.is_empty() {
                    None
                } else if let Some(slash) = current.rfind('/') {
                    Some(current[..slash].to_string())
                } else {
                    Some(String::new())
                };
            }
            memo.0.borrow_mut().insert(dir.to_string(), found.clone());
            found
        }
    };
    found.unwrap_or_else(|| app_root_for(file_path))
}

pub type AngularRouteTable = RootedRouteTable;

/// True for a route the table holds: this resolver's, at an absolute path.
fn in_angular_table(node: &Node) -> bool {
    is_angular_route(node) && node.name.starts_with('/')
}

#[derive(Default)]
struct AngularTableCache(RefCell<Option<Rc<AngularRouteTable>>>);

pub fn angular_route_table(ctx: &Ctx) -> Rc<AngularRouteTable> {
    let all = ctx.get_nodes_by_kind(NodeKind::Route);
    let cache = ctx.memo::<AngularTableCache>();
    if let Some(cached) = cache.0.borrow().as_ref() {
        if Rc::ptr_eq(&cached.source, &all) {
            return Rc::clone(cached);
        }
    }
    let mut by_root: IndexMap<String, RouteTable> = IndexMap::new();
    let mut by_file: IndexMap<String, NodeRef> = IndexMap::new();
    for node in all.iter() {
        if !in_angular_table(node) {
            continue;
        }
        let root = angular_app_root(&node.file_path, ctx);
        let table = by_root.entry(root).or_insert_with(|| RouteTable::new(all.clone()));
        add_route_to(table, &node.name, node);
        if !by_file.contains_key(&node.file_path) {
            by_file.insert(node.file_path.clone(), node.clone());
        }
    }
    // Redirects: arriving at `/` is arriving at wherever `redirectTo` sends the user. A file's mount
    // prefix is what its routes' names add to their in-file paths. A chain (`/` → `/pages` →
    // `/pages/dashboard`) settles in a few passes.
    struct Alias {
        root: String,
        from: String,
        to: String,
    }
    let mut aliases: Vec<Alias> = Vec::new();
    let angular_routes: Vec<NodeRef> = all.iter().filter(|n| is_angular_route(n)).cloned().collect();
    let mounts = if by_file.is_empty() { None } else { Some(angular_mounts(ctx, &angular_routes)) };
    let mut redirect_files: IndexMap<String, Vec<String>> = IndexMap::new();
    for (file, sample) in &by_file {
        let own = resolved_segments(&in_file_path(sample), file, ctx);
        let full: Vec<String> = sample.name.split('/').filter(|seg| !seg.is_empty()).map(String::from).collect();
        let keep = full.len().saturating_sub(own.len());
        redirect_files.insert(file.clone(), full[..keep].to_vec());
    }
    // A file that only mounts others can still redirect: its prefix is where it is mounted.
    if let Some(mounts) = &mounts {
        for file in &mounts.mount_files {
            if !redirect_files.contains_key(file) {
                redirect_files.insert(file.clone(), mounts.prefix_of(file, ctx));
            }
        }
    }
    for (file, prefix) in &redirect_files {
        let Some(content) = ctx.read_file(file).filter(|c| !c.is_empty() && c.contains("redirectTo")) else { continue };
        let app_root = angular_app_root(file, ctx);
        let root = if by_root.contains_key(&app_root) {
            app_root
        } else if by_root.len() == 1 {
            by_root.keys().next().cloned().unwrap_or_default()
        } else {
            continue;
        };
        for r in parse_angular_routes(&content).redirects {
            let mut from = prefix.clone();
            from.extend(resolved_segments(&r.from, file, ctx));
            let mut to: Vec<String> = if r.absolute { Vec::new() } else { prefix.clone() };
            to.extend(resolved_segments(&r.to, file, ctx));
            aliases.push(Alias { root: root.clone(), from: join_path(&from), to: join_path(&to) });
        }
    }
    for _pass in 0..4 {
        let mut added = false;
        for a in &aliases {
            let Some(table) = by_root.get_mut(&a.root) else { continue };
            let target = table.exact.get(&a.to).cloned();
            if let Some(target) = target {
                if !table.exact.contains_key(&a.from) {
                    table.exact.insert(a.from.clone(), target);
                    added = true;
                }
            }
        }
        if !added {
            break;
        }
    }
    // `/` that no route or redirect serves falls through to the app's root `**` redirect. Only `/`:
    // letting the wildcard answer any path would draw every destination this reading could not name
    // as a trip home.
    for a in &aliases {
        if a.from != "/**" {
            continue;
        }
        let Some(table) = by_root.get_mut(&a.root) else { continue };
        let target = table.exact.get(&a.to).cloned();
        if let Some(target) = target {
            if !table.exact.contains_key("/") {
                table.exact.insert("/".to_string(), target);
            }
        }
    }
    let table = Rc::new(RootedRouteTable { source: all, by_root });
    *cache.0.borrow_mut() = Some(Rc::clone(&table));
    table
}

#[derive(Default)]
struct AppCounts(RefCell<Option<(NodeList, usize)>>);

/// How many apps the table splits into — `by_root.len()` of [`angular_route_table`], counted from
/// the routes alone: building the table reads every file that mounts or redirects.
fn angular_app_count(ctx: &Ctx) -> usize {
    let all = ctx.get_nodes_by_kind(NodeKind::Route);
    let memo = ctx.memo::<AppCounts>();
    if let Some((source, count)) = memo.0.borrow().as_ref() {
        if Rc::ptr_eq(source, &all) {
            return *count;
        }
    }
    let mut roots: HashSet<String> = HashSet::new();
    for node in all.iter() {
        if in_angular_table(node) {
            roots.insert(angular_app_root(&node.file_path, ctx));
        }
    }
    let count = roots.len();
    *memo.0.borrow_mut() = Some((all, count));
    count
}

/// The routes a file navigates among: its app's, or — for a shared library outside any app (an Nx
/// `libs/ui`) — the one app's, when there is only one.
pub fn angular_routes_for<'a>(table: &'a AngularRouteTable, file_path: &str) -> Option<&'a RouteTable> {
    if let Some(own) = routes_for_file(table, file_path).filter(|own| !own.exact.is_empty()) {
        return Some(own);
    }
    if table.by_root.len() == 1 {
        table.by_root.values().next()
    } else {
        None
    }
}

// -- Navigation -----------------------------------------------------------------------------------------------------------

/// `this.router.navigate`, `router.navigateByUrl`, `this._router.createUrlTree`, `router.parseUrl`.
fn nav_call_re() -> &'static Re {
    re!(r"^(?:this\.)?_?[rR]outer\.(navigate|navigateByUrl|createUrlTree|parseUrl)$")
}

/// The destination a command array names: `['/article', slug]` is `/article/${…}`; `None` when it
/// is relative or not a literal array.
pub fn commands_href(text: &str, from_root: bool) -> Option<HrefLiteral> {
    let arr = js_trim(text);
    if !arr.starts_with('[') {
        return None;
    }
    let close = match_bracket(arr, 0)?;
    let mut parts: Vec<String> = Vec::new();
    let mut element = String::new();
    let flush = |element: &mut String, parts: &mut Vec<String>| {
        let e = js_trim(element).to_string();
        element.clear();
        if e.is_empty() {
            return;
        }
        if let Some(literal) = static_string(&e) {
            parts.extend(non_empty_segments(&literal));
            return;
        }
        // A matrix-params object says nothing about the path.
        if e.starts_with('{') {
            return;
        }
        parts.push(HOLE.to_string());
    };
    let b = arr.as_bytes();
    let mut i = 1usize;
    while i < close {
        let ch = b[i];
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            let end = skip_string(arr, i)?;
            element.push_str(&arr[i..end + 1]);
            i = end + 1;
            continue;
        }
        if ch == b'[' || ch == b'{' || ch == b'(' {
            let end = match_bracket(arr, i)?;
            element.push_str(&arr[i..end + 1]);
            i = end + 1;
            continue;
        }
        if ch == b',' {
            flush(&mut element, &mut parts);
            i += 1;
        } else {
            let c = arr[i..].chars().next().expect("a char at a valid offset");
            element.push(c);
            i += c.len_utf8();
        }
    }
    flush(&mut element, &mut parts);
    // Only an absolute destination: `['/login']`. In a template, `['../', id]` and a bare `['edit']`
    // are relative to wherever the component is; a `router.navigate(['project', 'issue', id])` with
    // no `relativeTo` starts from the root.
    let first_element = arr[1..close].split(',').next().unwrap_or("");
    let first = static_string(first_element)?;
    if !first.starts_with('/') && !(from_root && !first.starts_with('.')) {
        return None;
    }
    let path_text = format!("/{}", parts.iter().filter(|p| !p.is_empty()).cloned().collect::<Vec<_>>().join("/"));
    names_somewhere(Some(HrefLiteral { display: path_text.replace(HOLE, "${…}"), path: path_text, alternates: None }))
}

/// The href, when it names at least one segment of its own. A path made only of holes
/// (`` [`/${a}`, b, c] ``) would match any route of its length — a hole scores against a literal
/// segment — so it names nothing.
pub fn names_somewhere(href: Option<HrefLiteral>) -> Option<HrefLiteral> {
    let href = href?;
    let segs: Vec<&str> = href.path.split('/').filter(|seg| !seg.is_empty()).collect();
    if segs.is_empty() || segs.iter().any(|seg| !seg.contains(HOLE)) {
        Some(href)
    } else {
        None
    }
}

/// A class property's initializer, read from its body: `routerLinkAbout =
/// publicRoutes.about.routerLink;`.
pub fn property_initializer(owner: &Node, name: &str, content: &str) -> Option<String> {
    let lines: Vec<&str> = content.split('\n').collect();
    let from = (owner.start_line as usize).saturating_sub(1).min(lines.len());
    let to = (owner.end_line as usize).clamp(from, lines.len());
    let body = lines[from..to].join("\n");
    let decl = Re::new(
        &format!(r"(?:^|[\s;{{])(?:(?:public|private|protected|readonly|static|override)\s+)*{}\s*(?::[^=;\n]+)?=\s*", escape_regex(name)),
        "",
    )
    .find(&body)?;
    let b = body.as_bytes();
    let start = decl.end();
    let mut i = start;
    while i < b.len() {
        let ch = b[i];
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            let end = skip_string(&body, i)?;
            i = end + 1;
            continue;
        }
        if ch == b'[' || ch == b'{' || ch == b'(' {
            let end = match_bracket(&body, i)?;
            i = end + 1;
            continue;
        }
        if ch == b';' || ch == b'\n' {
            break;
        }
        i += 1;
    }
    let text = js_trim(&body[start..i.min(body.len())]);
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// The array an arrow function returns: `(id) => ['/x', id]` or `(id) => { return ['/x', id]; }`.
fn arrow_return(f: &str) -> Option<String> {
    let arrow = f.find("=>")?;
    let b = f.as_bytes();
    let mut i = arrow + 2;
    while let Some(c) = f.get(i..).and_then(|r| r.chars().next()).filter(|c| is_js_space(*c)) {
        i += c.len_utf8();
    }
    if b.get(i) == Some(&b'(') {
        i += 1;
    }
    if b.get(i) == Some(&b'[') {
        let end = match_bracket(f, i)?;
        return if end > i { Some(f[i..=end].to_string()) } else { None };
    }
    if b.get(i) == Some(&b'{') {
        let ret = re!(r"\breturn\s*\[").find(&f[i..])?;
        let open = i + ret.end() - 1;
        let end = match_bracket(f, open)?;
        return if end > open { Some(f[open..=end].to_string()) } else { None };
    }
    None
}

/// Where an Angular destination expression leads — the argument of `router.navigate(…)`, or a
/// `[routerLink]` binding:
///
/// - a command array, `['/article', slug]`;
/// - a static string, `'/login'` or `'/' + $localize…`, and `'/editor/' + slug`;
/// - a route constant, `internalRoutes.zen.routerLink`;
/// - a property of the class it is written in, `this.routerLinkAbout`, holding one of those;
/// - any of those `.concat(id)`, one more segment.
///
/// `owner` is the class the expression is written in, for `this.` properties.
pub fn angular_destination(expr: &str, file: &str, owner: Option<&Node>, ctx: &Ctx, depth: usize, from_root: bool) -> Option<HrefLiteral> {
    names_somewhere(destination_of(expr, file, owner, ctx, depth, from_root))
}

fn destination_of(expr: &str, file: &str, owner: Option<&Node>, ctx: &Ctx, depth: usize, from_root: bool) -> Option<HrefLiteral> {
    let text = js_trim(expr);
    if text.is_empty() || depth > 4 {
        return None;
    }
    if text.starts_with('[') {
        return commands_href(text, from_root);
    }
    if let Some(literal) = static_string(text) {
        return to_href(Some(literal));
    }
    // `'/editor/' + article.slug`: the literal head, a hole for the rest.
    if re!(r#"^['"]"#).is_match(text) && text.contains('+') {
        let head = static_string(&text[..text.find('+').unwrap_or(0)]);
        if let Some(head) = head.filter(|h| h.starts_with('/')) {
            let with_slash = re!(r"\/?$").replace_first(&head, |_| "/".to_string());
            return to_href(Some(format!("{with_slash}{HOLE}")));
        }
    }
    // `this.routerLinkAdminControlUsers.concat(userId)`: one segment more.
    if let Some(concat) = Re::new(r"^(.*?)\.concat\s*\((.*)\)$", "s").captures(text) {
        let base = angular_destination(concat.text(1).unwrap_or(""), file, owner, ctx, depth + 1, from_root);
        let added = concat.text(2).unwrap_or("").split(',').filter(|a| !js_trim(a).is_empty()).count();
        return match base {
            Some(base) if added > 0 => {
                let trimmed = base.path.strip_suffix('/').unwrap_or(&base.path);
                to_href(Some(format!("{trimmed}{}", format!("/{HOLE}").repeat(added))))
            }
            other => other,
        };
    }
    // `update.routerLink(dataSource, symbol)`: a route constant that builds its commands — its
    // returned array, the arguments holes.
    if let Some(call) = Re::new(r"^((?:this\s*\.\s*)?[A-Za-z_$][\w$]*(?:\s*\.\s*[A-Za-z_$][\w$]*)+)\s*\((.*)\)$", "s").captures(text) {
        let callee = strip_whitespace(call.text(1).unwrap_or(""));
        let callee = callee.strip_prefix("this.").unwrap_or(&callee).to_string();
        let f = constant_text(&callee, file, ctx);
        let returned = f.and_then(|f| arrow_return(&f));
        return returned.and_then(|r| commands_href(&r, from_root));
    }
    let Some(chain) = re!(r"^(?:this\s*\.\s*)?([A-Za-z_$][\w$]*(?:\s*\.\s*[A-Za-z_$][\w$]*)*)$").captures(text) else {
        return parse_href_expression(text);
    };
    let joined = strip_whitespace(chain.text(1).unwrap_or(""));
    let parts: Vec<&str> = joined.split('.').collect();
    let content = ctx.read_file(file).filter(|c| !c.is_empty());
    if let (Some(content), Some(owner)) = (&content, owner) {
        if text.starts_with("this") || parts.len() == 1 {
            if let Some(init) = property_initializer(owner, parts[0], content) {
                if parts.len() == 1 {
                    return angular_destination(&init, file, Some(owner), ctx, depth + 1, from_root);
                }
                return angular_destination(&format!("{init}.{}", parts[1..].join(".")), file, Some(owner), ctx, depth + 1, from_root);
            }
        }
    }
    let constant = constant_text(&parts.join("."), file, ctx)?;
    angular_destination(&constant, file, owner, ctx, depth + 1, from_root)
}

// -- The resolver -----------------------------------------------------------------------------------------------------------

pub struct AngularRouterResolver {
    nav: NavigationCalls,
}

impl AngularRouterResolver {
    pub fn new() -> Self {
        AngularRouterResolver {
            nav: NavigationCalls {
                tails: vec!["navigate", "navigateByUrl", "createUrlTree", "parseUrl"],
                // A call matches against its own app's table — or, in a workspace of one app,
                // against that app's from anywhere, a shared library included
                // (`angular_routes_for`).
                scope: Box::new(|route, ctx| {
                    if !in_angular_table(route) {
                        return None;
                    }
                    Some(if angular_app_count(ctx) == 1 { vec![String::new()] } else { vec![angular_app_root(&route.file_path, ctx)] })
                }),
            },
        }
    }
}

impl Default for AngularRouterResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameworkResolver for AngularRouterResolver {
    fn name(&self) -> &str {
        "angular-router"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&[Language::TypeScript, Language::JavaScript])
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        depends_on(ctx, &["@angular/router", "@angular/core"])
    }

    fn claims_reference(&self, name: &str) -> bool {
        nav_call_re().is_match(name) || lazy_component_ref_re().is_match(name) || layout_ref_re().is_match(name)
    }

    fn navigation(&self) -> Option<&NavigationCalls> {
        Some(&self.nav)
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<FrameworkExtractionResult> {
        if !re!(r"\.[cm]?ts$").is_match(file_path) || file_path.ends_with(".d.ts") {
            return Some(FrameworkExtractionResult::default());
        }
        let routes = parse_angular_routes(content).routes;
        if routes.is_empty() {
            return Some(FrameworkExtractionResult::default());
        }
        let now = now_ms();
        let mut nodes: Vec<Node> = Vec::new();
        let mut references: Vec<UnresolvedRef> = Vec::new();
        for route in &routes {
            let node = Node::new(
                route_id(file_path, route.line, &route.path),
                NodeKind::Route,
                route.path.clone(),
                format!("{file_path}::route:{}", route.path),
                file_path,
                Language::TypeScript,
                route.line,
                route.line,
                0,
                0,
                now,
            );
            let make_ref = |component: &AngularComponentRef, layout: bool| -> UnresolvedRef {
                let name = match &component.spec {
                    None => component.name.clone().unwrap_or_default(),
                    Some(spec) => format!("import:{spec}#{}", component.name.as_deref().unwrap_or("default")),
                };
                UnresolvedRef::new(
                    node.id.clone(),
                    format!("{}{}", if layout { "layout:" } else { "" }, name),
                    EdgeKind::References,
                    route.line,
                    0,
                    file_path,
                    Language::TypeScript,
                )
            };
            if let Some(component) = &route.component {
                references.push(make_ref(component, false));
            }
            // The layouts around it: their links and buttons are on this screen too.
            for layout in &route.layouts {
                references.push(make_ref(layout, true));
            }
            nodes.push(node);
        }
        Some(FrameworkExtractionResult { nodes, references })
    }

    /// Put each routes file's routes under the path that lazy-loads it, and read constant path
    /// segments. Recomputed from each route's in-file path (its qualified name), so a second run
    /// changes nothing.
    fn post_extract(&self, ctx: &Ctx) -> Option<Vec<Node>> {
        let routes: Vec<NodeRef> = ctx.get_nodes_by_kind(NodeKind::Route).iter().filter(|n| is_angular_route(n)).cloned().collect();
        if routes.is_empty() {
            return Some(Vec::new());
        }
        let mounts = angular_mounts(ctx, &routes);
        let mut changed: Vec<Node> = Vec::new();
        for route in &routes {
            let mut segs = mounts.prefix_of(&route.file_path, ctx);
            segs.extend(resolved_segments(&in_file_path(route), &route.file_path, ctx));
            let name = join_path(&segs);
            if name != route.name {
                let mut updated: Node = (**route).clone();
                updated.name = name;
                changed.push(updated);
            }
        }
        Some(changed)
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        if let Some(layout) = layout_ref_re().captures(&r.reference_name) {
            if !r.from_node_id.ends_with(":angular") {
                return None;
            }
            let component = route_component(layout.text(1).unwrap_or(""), &r.file_path, ctx)?;
            let mut resolved = ResolvedRef::new(r, component.id.clone(), 0.95, ResolvedBy::Framework);
            let mut metadata = serde_json::Map::new();
            metadata.insert("layout".into(), serde_json::Value::Bool(true));
            resolved.metadata = Some(metadata);
            return Some(resolved);
        }
        if let Some(lazy) = lazy_component_ref_re().captures(&r.reference_name) {
            if !r.from_node_id.ends_with(":angular") {
                return None;
            }
            let member = lazy.text(2).filter(|m| *m != "default");
            let component = lazy_component(lazy.text(1).unwrap_or(""), member, &r.file_path, ctx)?;
            return Some(ResolvedRef::new(r, component.id.clone(), 0.95, ResolvedBy::Framework));
        }

        let nav = nav_call_re().captures(&r.reference_name)?;
        if !r.is_kind(EdgeKind::Calls) {
            return None;
        }
        let verb = nav.text(1)?.to_string();
        let table = angular_route_table(ctx);
        let routes = angular_routes_for(&table, &r.file_path)?;
        if routes.exact.is_empty() {
            return None;
        }
        let lines = ctx.get_file_lines(&r.file_path)?;
        let arg = first_argument_text(&lines, r.line, r.column, &verb)?;
        // `navigate(['..', id], { relativeTo: this.route })` goes somewhere relative to here.
        if let Some(extras) = nth_argument_text(&lines, r.line, r.column, &verb, 1) {
            if !extras.is_empty() && re!(r"\brelativeTo\b").is_match(&extras) {
                return None;
            }
        }
        let mut owner: Option<NodeRef> = None;
        for n in ctx.get_nodes_in_file(&r.file_path).iter() {
            if n.kind == NodeKind::Class && n.start_line <= r.line && n.end_line >= r.line && owner.as_ref().is_none_or(|inner| n.start_line >= inner.start_line) {
                owner = Some(n.clone());
            }
        }
        let href = angular_destination(&arg, &r.file_path, owner.as_deref(), ctx, 0, true)?;
        if !href.path.starts_with('/') {
            return None;
        }
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

/// For the template synthesizer: a template file's path, from the component file that names it.
pub fn template_path_for(component_file: &str, template_url: &str) -> String {
    let dir = match component_file.rfind('/') {
        None => ".",
        Some(0) => "/",
        Some(i) => &component_file[..i],
    };
    super::posix::normalize(&format!("{dir}/{template_url}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_strings_cover_literals_localize_and_plus() {
        assert_eq!(static_string("'/login'").as_deref(), Some("/login"));
        assert_eq!(static_string("'/' + 'about'").as_deref(), Some("/about"));
        assert_eq!(static_string("$localize`:kebab-case@@routes.about:about`").as_deref(), Some("about"));
        assert_eq!(static_string("'/' + $localize`:@@x:about`").as_deref(), Some("/about"));
        assert_eq!(static_string("`a${b}`"), None);
        assert_eq!(static_string("computed"), None);
    }

    #[test]
    fn routes_arrays_are_walked_as_objects() {
        let src = r#"
import { Routes } from '@angular/router';
export const routes: Routes = [
  { path: '', redirectTo: 'home', pathMatch: 'full' },
  { path: 'login', component: AuthComponent },
  { component: Edit, path: 'editor', children: [
    { path: '', component: EditorHome },
    { path: ':slug', loadComponent: () => import('./slug.component').then(m => m.SlugComponent) },
  ] },
  { path: 'profile', loadChildren: () => import('./profile/profile.routes') },
  { path: '**', redirectTo: 'home' },
];
"#;
        let parsed = parse_angular_routes(src);
        let paths: Vec<&str> = parsed.routes.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["/login", "/editor", "/editor/:slug"]);
        assert_eq!(parsed.mounts.len(), 1);
        assert_eq!(parsed.mounts[0].prefix, "/profile");
        assert_eq!(parsed.mounts[0].spec, "./profile/profile.routes");
        assert_eq!(parsed.redirects.len(), 2);
        assert_eq!(parsed.redirects[0], AngularRedirect { from: "/".into(), to: "/home".into(), absolute: false });
        assert_eq!(parsed.redirects[1].from, "/**");
        let slug = &parsed.routes[2];
        assert_eq!(slug.component, Some(AngularComponentRef { name: Some("SlugComponent".into()), spec: Some("./slug.component".into()) }));
        assert_eq!(slug.layouts, vec![AngularComponentRef { name: Some("Edit".into()), spec: None }]);
    }

    #[test]
    fn command_arrays_are_destinations() {
        let href = commands_href("['/article', slug]", false).unwrap();
        assert_eq!(href.path, format!("/article/{HOLE}"));
        assert_eq!(href.display, "/article/${…}");
        assert!(commands_href("['edit']", false).is_none());
        assert!(commands_href("['edit']", true).is_some());
        assert!(commands_href("['../', id]", true).is_none());
        assert!(commands_href("[`/${a}`, b]", false).is_none());
    }

    #[test]
    fn template_paths_are_normalised() {
        assert_eq!(template_path_for("src/app/a.component.ts", "./a.component.html"), "src/app/a.component.html");
        assert_eq!(template_path_for("src/app/a.component.ts", "../shared/x.html"), "src/shared/x.html");
        assert_eq!(template_path_for("a.component.ts", "a.html"), "a.html");
    }

    #[test]
    fn component_classes_are_found_in_order() {
        let src = "@Component({selector: 'a'})\nexport default class A {}\n@Component({selector: 'b'})\nexport class B {}\n";
        let classes = component_classes_in(src);
        let summary: Vec<(&str, bool)> = classes.iter().map(|c| (c.name.as_str(), c.is_default)).collect();
        assert_eq!(summary, vec![("A", true), ("B", false)]);
    }
}
