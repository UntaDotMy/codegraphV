//! Vue Router — routes declared in a config object, navigation often by NAME. Port of
//! `src/resolution/frameworks/vue-router.ts`.
//!
//! `frameworks/vue` reads Nuxt's file convention (`pages/about.vue` is `/about`), which is half the
//! Vue world. The other half — every plain Vue 3 app — declares its routes in one object:
//!
//! ```text
//! const router = createRouter({
//!   history: createWebHistory(),
//!   routes: [
//!     { name: 'login',   path: '/login',             component: () => import('@/views/Login') },
//!     { name: 'profile', path: '/profile/:username', component: Profile },
//!   ],
//! })
//! ```
//!
//! `extract()` reads that array into one `route` node per entry, named by its path the way every
//! other framework's routes are, bound to the component it names — an identifier, or the last
//! segment of a lazy `() => import(…)`, which is the `.vue` file's own name.
//!
//! **Navigation is usually a name, not a path.** `router.push({ name: 'login' })` is the common
//! idiom; `router.push('/')`, `router.push({ path: '/', query })` and Nuxt's `navigateTo('/x')`
//! name a path. So `resolve()` reads the argument as a name FIRST and falls back to the path
//! readers every other framework shares. A route's name lives only in the source — node metadata is
//! not persisted — so the table re-reads the config files its own route nodes came from, with the
//! same parser `extract` used.
//!
//! Left unresolved rather than guessed: a computed destination, a name or path nothing declares,
//! and a nested `children:` route whose path is relative to its parent.

use super::expo_router::{
    add_route_to, app_root_for, first_argument_text, nav_metadata, parse_href_expression, read_href_via_local, read_string_at, routes_for_file,
    to_href, HrefLiteral, RootedRouteTable, RouteTable,
};
use super::nextjs::destinations_for_href;
use super::object_literal::{match_bracket, read_fields, top_level_objects};
use super::now_ms;
use super::package_deps::depends_on;
use crate::ctx::Ctx;
use crate::import_resolver::resolve_import_path;
use crate::types::{AlsoTarget, FrameworkExtractionResult, FrameworkResolver, NavigationCalls, NodeRef, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use indexmap::{IndexMap, IndexSet};
use std::cell::RefCell;
use std::rc::Rc;

const ROUTE_LANGUAGES: [Language; 3] = [Language::TypeScript, Language::JavaScript, Language::Vue];

// -- Reading the routes array -----------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct VueRouteEntry {
    /// `/profile/:username` — the full path, a child's joined onto its parents'.
    pub path: String,
    /// `profile` — what `router.push({ name })` names, when the entry has one.
    pub name: Option<String>,
    /// The component the entry names, by identifier or by the tail of its lazy import.
    pub component: Option<String>,
    /// `@/views/dashboard/index` — the file a lazy `() => import(…)` component names.
    pub spec: Option<String>,
    /// The components of the parent routes it renders inside, outermost first.
    pub layouts: Vec<VueLayout>,
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VueLayout {
    pub component: String,
    pub spec: Option<String>,
}

/// A file that builds a router — the cheap gate before parsing anything.
fn router_factory_re() -> &'static Re {
    re!(r"\b(?:createRouter|createWebHistory|createWebHashHistory|createMemoryHistory)\s*\(|\bnew\s+(?:VueRouter|Router)\s*\(")
}

/// `routes: [` / `routes = [` — the array itself, for a file that only holds the table.
fn routes_array_re() -> &'static Re {
    re!(r"\broutes\s*[:=]\s*\[")
}

/// A file that imports from `vue-router`.
fn vue_router_import_re() -> &'static Re {
    re!(r#"\bfrom\s*['"]vue-router['"]"#)
}

/// A file in a `router/` or `routes/` directory, or named `router.js` / `routes.ts`.
fn route_file_path_re() -> &'static Re {
    re!(r"(?:^|\/)(?:router|routes)(?:\/|\.[cm]?[jt]s$)")
}

/// Where a routes table starts: a `routes: [` field, or a declaration named for what it holds —
/// `export const constantRoutes = [`, `const routes: RouteRecordRaw[] = [`, vue-element-admin's
/// per-module `const tableRouter = {`.
fn table_openers_re() -> std::sync::Arc<Re> {
    Re::cached(r"\broutes\s*[:=]\s*\[|\b(?:const|let|var)\s+(?:[A-Za-z_$][\w$]*)?(?:[Rr]outes?|[Rr]outers?)\s*(?::[^=;\n]*)?=\s*([[{])", "g")
}

struct Walk<'a> {
    safe: &'a str,
    out: Vec<VueRouteEntry>,
    seen: IndexSet<String>,
}

impl Walk<'_> {
    fn line_of(&self, at: usize) -> u32 {
        self.safe.as_bytes()[..at.min(self.safe.len())].iter().filter(|c| **c == b'\n').count() as u32 + 1
    }

    fn visit(&mut self, obj: (usize, usize), prefix: Option<&str>, layouts: &[VueLayout], depth: usize) {
        let safe = self.safe;
        let fields = read_fields(safe, obj.0, obj.1);
        let Some(path_field) = fields.get("path") else { return };
        let Some(own) = read_string_at(path_field.text.trim_start_matches(codegraph_types::jscompat::is_js_space), 0) else { return };
        let mut path: String;
        if own.starts_with('/') {
            path = own.clone();
        } else {
            let Some(prefix) = prefix else { return }; // a top-level `*` or relative path is no address
            path = if own.is_empty() { prefix.to_string() } else { format!("{}/{}", if prefix == "/" { "" } else { prefix }, own) };
        }
        if path.len() > 1 {
            path = re!(r"\/+$").replace_first(&path, |_| String::new());
        }

        let component_field = fields.get("component").or_else(|| fields.get("components"));
        let component = component_field.and_then(|f| component_of(&f.text));

        let children = fields.get("children");
        let mut child_claims_address = false;
        if let Some(children) = children {
            if depth < 12 {
                let open = safe.as_bytes().get(children.at..).and_then(|rest| rest.iter().position(|b| *b == b'[')).map(|i| i + children.at);
                let close = open.and_then(|o| match_bracket(safe, o));
                if let (Some(open), Some(close)) = (open, close) {
                    if close > open {
                        let mut inner: Vec<VueLayout> = layouts.to_vec();
                        if let Some(c) = &component {
                            inner.push(c.clone());
                        }
                        for child in top_level_objects(safe, open + 1, close) {
                            let child_fields = read_fields(safe, child.0, child.1);
                            let child_own = child_fields
                                .get("path")
                                .and_then(|f| read_string_at(f.text.trim_start_matches(codegraph_types::jscompat::is_js_space), 0));
                            if child_own.as_deref() == Some("") || child_own.as_deref() == Some(path.as_str()) {
                                child_claims_address = true;
                            }
                            self.visit(child, Some(&path), &inner, depth + 1);
                        }
                    }
                }
            }
        }
        let Some(component) = component else { return };
        if children.is_some() && (child_claims_address || fields.contains_key("redirect")) {
            return;
        }
        let name = fields.get("name").and_then(|f| read_string_at(f.text.trim_start_matches(codegraph_types::jscompat::is_js_space), 0));
        let key = format!("{} {}", path, name.as_deref().unwrap_or(""));
        if !self.seen.insert(key) {
            return;
        }
        let line = self.line_of(path_field.at);
        self.out.push(VueRouteEntry { path, name, component: Some(component.component), spec: component.spec, layouts: layouts.to_vec(), line });
    }
}

/// Every route a file's routes tables declare.
///
/// The tables are walked, not pattern-matched: a `name` is written ABOVE the `path` it belongs to,
/// so reading fields out of a window around each `path` hands an entry its PREDECESSOR's name —
/// vue-realworld's `login` came out as `/register`, silently, for every route in the file. So each
/// `{…}` is matched as a unit and only its own depth-1 fields are read; a `meta: {…}` and a lazy
/// `component: () => import(…)` are stepped over rather than searched, and `children` are walked
/// in turn with the parent's path in front of theirs.
///
/// A route with children is a layout: its component renders the outlet they fill. It is a screen
/// of its own only when no child claims its address and it does not `redirect` elsewhere.
///
/// A top-level entry whose path does not start with `/` (`*`, a catch-all) is not an address. A
/// table is read from a file that builds a router, imports `vue-router`, or lives in a `router/`
/// directory.
pub fn parse_vue_routes(content: &str, file_path: &str) -> Vec<VueRouteEntry> {
    let route_file = router_factory_re().is_match(content)
        || routes_array_re().is_match(content)
        || vue_router_import_re().is_match(content)
        || route_file_path_re().is_match(file_path);
    if !route_file {
        return Vec::new();
    }
    let safe = codegraph_text::strip_comments_for_regex(content, "typescript");
    let mut walk = Walk { safe: &safe, out: Vec::new(), seen: IndexSet::new() };
    let mut walked: IndexSet<usize> = IndexSet::new();

    let openers = table_openers_re();
    let mut pos = 0usize;
    while pos <= safe.len() {
        let Some(m) = openers.captures_at(&safe, pos) else { break };
        let whole = m.get(0).expect("group 0");
        let open = whole.end() - 1;
        pos = whole.end().max(pos + 1);
        let Some(close) = match_bracket(&safe, open) else { continue };
        if walked.contains(&open) {
            continue;
        }
        walked.insert(open);
        if safe.as_bytes()[open] == b'{' {
            walk.visit((open, close), None, &[], 0);
        } else {
            for obj in top_level_objects(&safe, open + 1, close) {
                walk.visit(obj, None, &[], 0);
            }
        }
        pos = close;
    }
    walk.out
}

/// The component an entry names: an identifier, or the file a lazy import names.
fn component_of(value: &str) -> Option<VueLayout> {
    if let Some(lazy) = re!(r#"\bimport\s*\(\s*['"`]([^'"`]+)['"`]"#).captures(value) {
        let spec = lazy.text(1).unwrap_or("");
        let tail = re!(r"\.\w+$").replace_first(spec.rsplit('/').next().unwrap_or(""), |_| String::new());
        return if tail.is_empty() { None } else { Some(VueLayout { component: tail, spec: Some(spec.to_string()) }) };
    }
    let ident = re!(r"^\s*([A-Z][A-Za-z0-9_]*)\s*$").captures(value)?;
    Some(VueLayout { component: ident.text(1)?.to_string(), spec: None })
}

fn language_for_file(file_path: &str) -> Language {
    if file_path.ends_with(".vue") {
        return Language::Vue;
    }
    if re!(r"\.(?:ts|mts|cts)$").is_match(file_path) {
        return Language::TypeScript;
    }
    Language::JavaScript
}

/// The id a config-declared route carries — a verbatim reconstruction, so the table can recognise its own.
fn route_id(file_path: &str, line: u32, path: &str) -> String {
    format!("route:{file_path}:{line}:{path}:vue")
}

// -- Route table — by path, and by name --------------------------------------------------------------------------

/// One app's routes, by path and — Vue's own idiom — by name.
pub struct VueAppRoutes {
    pub table: RouteTable,
    /// `login` → the route node, for `router.push({ name: 'login' })`.
    pub by_name: IndexMap<String, NodeRef>,
}

pub type VueRouteTable = RootedRouteTable<VueAppRoutes>;

/// True for a route node this resolver emitted, and no other.
fn is_vue_config_route(node: &Node) -> bool {
    node.id == route_id(&node.file_path, node.start_line, &node.name)
}

/// True for a Nuxt page route `frameworks/vue` emitted.
fn is_nuxt_page(node: &Node) -> bool {
    node.language == Language::Vue && format!("/{}", node.file_path).contains("/pages/") && node.id == format!("route:{}:{}:1", node.file_path, node.name)
}

/// True for a route the table holds: a config route or a Nuxt page, at an absolute path.
fn in_vue_table(node: &Node) -> bool {
    (is_vue_config_route(node) || is_nuxt_page(node)) && node.name.starts_with('/')
}

#[derive(Default)]
struct VueTableCache(RefCell<Option<Rc<VueRouteTable>>>);

pub fn vue_route_table(ctx: &Ctx) -> Rc<VueRouteTable> {
    let all = ctx.get_nodes_by_kind(NodeKind::Route);
    let cache = ctx.memo::<VueTableCache>();
    if let Some(cached) = cache.0.borrow().as_ref() {
        if Rc::ptr_eq(&cached.source, &all) {
            return Rc::clone(cached);
        }
    }
    let mut by_root: IndexMap<String, VueAppRoutes> = IndexMap::new();
    let mut config_files: IndexMap<String, (String, Vec<NodeRef>)> = IndexMap::new();
    for node in all.iter() {
        if !in_vue_table(node) {
            continue;
        }
        let root = app_root_for(&node.file_path);
        let app = by_root.entry(root.clone()).or_insert_with(|| VueAppRoutes { table: RouteTable::new(all.clone()), by_name: IndexMap::new() });
        add_route_to(&mut app.table, &node.name, node);
        if is_vue_config_route(node) {
            config_files.entry(node.file_path.clone()).or_insert_with(|| (root, Vec::new())).1.push(node.clone());
        }
    }
    // A route's NAME is not persisted on the node, so the config files its own route nodes came
    // from are re-read with the same parser `extract` used.
    for (file_path, (root, nodes)) in &config_files {
        let Some(content) = ctx.read_file(file_path).filter(|c| !c.is_empty()) else { continue };
        let mut by_path: IndexMap<&str, &NodeRef> = IndexMap::new();
        for n in nodes {
            by_path.insert(n.name.as_str(), n);
        }
        let app = by_root.get_mut(root).expect("the table of a config file's root exists");
        for entry in parse_vue_routes(&content, file_path) {
            let Some(name) = entry.name.filter(|n| !n.is_empty()) else { continue };
            if let Some(node) = by_path.get(entry.path.as_str()) {
                if !app.by_name.contains_key(&name) {
                    app.by_name.insert(name, (*node).clone());
                }
            }
        }
    }
    let table = Rc::new(RootedRouteTable { source: all, by_root });
    *cache.0.borrow_mut() = Some(Rc::clone(&table));
    table
}

// -- Navigation calls ----------------------------------------------------------------------------------------------

/// `router.push` / `.replace` (the Composition API), `this.$router.push` / `$router.push` (the
/// Options API and templates), and Nuxt's `navigateTo`.
///
/// As everywhere else, `push` and `replace` need a receiver that names a router: an unqualified
/// `push` is an array's.
fn nav_call_re() -> &'static Re {
    re!(r"^(?:this\.)?\$?router\.(?:push|replace)$|^navigateTo$")
}

/// The verb a navigation call name stands for, or `None`.
pub fn vue_nav_verb(name: &str) -> Option<String> {
    if !nav_call_re().is_match(name) {
        return None;
    }
    Some(match name.rfind('.') {
        None => name.to_string(),
        Some(dot) => name[dot + 1..].to_string(),
    })
}

/// The route name in a `{ name: 'login' }` destination, or `None` for anything else.
pub fn route_name_in_expression(expr: &str) -> Option<String> {
    let args = codegraph_types::jscompat::js_trim(expr);
    if !args.starts_with('{') {
        return None;
    }
    let key = re!(r#"\bname\s*:\s*['"`]"#).find(args)?;
    read_string_at(args, key.end() - 1)
}

/// `{ path: '/', query }` — Vue's object destination, whose key is `path`, not `pathname`.
pub fn parse_vue_path_object(expr: &str) -> Option<HrefLiteral> {
    let args = codegraph_types::jscompat::js_trim(expr);
    if !args.starts_with('{') {
        return None;
    }
    let key = re!(r#"\bpath\s*:\s*['"`]"#).find(args)?;
    to_href(read_string_at(args, key.end() - 1))
}

/// True for the `calls` ref this resolver's `extract` emitted from a route to its component.
fn is_vue_route_ref(r: &UnresolvedRef) -> bool {
    r.from_node_id.starts_with("route:") && r.from_node_id.ends_with(":vue")
}

/// The component a route names — a `.vue` file's own component node, or a component declared in a
/// plain script. Nearest app root first; an ambiguous name resolves to nothing rather than to an
/// arbitrary one of several.
fn vue_component_named(name: &str, from_file: &str, ctx: &Ctx) -> Option<NodeRef> {
    let candidates: Vec<NodeRef> = ctx
        .get_nodes_by_name(name)
        .iter()
        .filter(|n| n.kind == NodeKind::Component || (n.kind == NodeKind::Function && n.file_path.ends_with(".vue")))
        .cloned()
        .collect();
    if candidates.is_empty() {
        return None;
    }
    if candidates.len() == 1 {
        return Some(candidates[0].clone());
    }
    let root = app_root_for(from_file);
    let near: Vec<&NodeRef> = candidates.iter().filter(|n| n.file_path.starts_with(&root)).collect();
    if near.len() == 1 {
        Some(near[0].clone())
    } else {
        None
    }
}

/// `import:@/views/Login#Login` for a lazy component, the bare name for an identifier.
fn component_ref_name(component: &str, spec: Option<&str>) -> String {
    match spec {
        None => component.to_string(),
        Some(spec) => format!("import:{spec}#{component}"),
    }
}

/// The component a route's reference names. A lazy import names a FILE, and the component is that
/// file's: vue-element-admin's views are all `…/index.vue`, so the name alone (`index`) fits
/// dozens. The import is resolved as the router file would resolve it, or — through an alias the
/// resolver doesn't know, like vben's `#/views/…` — by the one file in the same app whose path
/// ends the way the specifier does.
fn route_component(encoded: &str, from_file: &str, ctx: &Ctx) -> Option<NodeRef> {
    let lazy = re!(r"^import:(.+)#([^#]+)$").captures(encoded);
    // `component: Layout` — the file's own import of it says which file that is (`import Layout
    // from '@/layout'`, a `layout/index.vue`).
    let spec: Option<String> = match &lazy {
        Some(l) => l.text(1).map(String::from),
        None => ctx.get_import_mappings(from_file, language_for_file(from_file)).iter().find(|m| m.local_name == encoded).map(|m| m.source.clone()),
    };
    let name: &str = match &lazy {
        Some(l) => l.text(2).unwrap_or(""),
        None => encoded,
    };
    if let Some(spec) = &spec {
        let file = resolve_import_path(spec, from_file, Language::Vue, ctx).or_else(|| file_by_suffix(spec, from_file, ctx));
        if let Some(component) = file.and_then(|f| component_in_file(&f, ctx)) {
            return Some(component);
        }
    }
    if name == "index" {
        None
    } else {
        vue_component_named(name, from_file, ctx)
    }
}

fn component_in_file(file: &str, ctx: &Ctx) -> Option<NodeRef> {
    let nodes = ctx.get_nodes_in_file(file);
    nodes
        .iter()
        .find(|n| n.kind == NodeKind::Component)
        .or_else(|| nodes.iter().find(|n| n.kind == NodeKind::Function && n.name.chars().next().is_some_and(|c| c.is_ascii_uppercase())))
        .cloned()
}

const COMPONENT_EXTENSIONS: [&str; 10] = ["", ".vue", ".tsx", ".jsx", ".ts", ".js", "/index.vue", "/index.tsx", "/index.ts", "/index.js"];

/// The one file under `from_file`'s app whose path ends like `spec` minus its alias (`#/`, `@/`, `~/`).
fn file_by_suffix(spec: &str, from_file: &str, ctx: &Ctx) -> Option<String> {
    let rest = re!(r"^(?:[@#~$][\w-]*\/|\.{1,2}\/)+").replace_first(spec, |_| String::new());
    if rest == spec || rest.is_empty() {
        return None;
    }
    let root = app_root_for(from_file);
    let mut hits: IndexSet<String> = IndexSet::new();
    for file in ctx.get_all_files().iter() {
        if !file.starts_with(&root) {
            continue;
        }
        for ext in COMPONENT_EXTENSIONS {
            let tail = format!("{rest}{ext}");
            if *file == tail || file.ends_with(&format!("/{tail}")) {
                hits.insert(file.clone());
            }
        }
    }
    if hits.len() == 1 {
        hits.into_iter().next()
    } else {
        None
    }
}

// -- The resolver ----------------------------------------------------------------------------------------------------

pub struct VueRouterResolver {
    nav: NavigationCalls,
}

impl VueRouterResolver {
    pub fn new() -> Self {
        VueRouterResolver {
            nav: NavigationCalls {
                tails: vec!["push", "replace", "navigateTo"],
                // A call matches against the table of the app its file is in.
                scope: Box::new(|route, _| if in_vue_table(route) { Some(vec![app_root_for(&route.file_path)]) } else { None }),
            },
        }
    }
}

impl Default for VueRouterResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameworkResolver for VueRouterResolver {
    fn name(&self) -> &str {
        "vue-router"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&ROUTE_LANGUAGES)
    }

    fn app_dependencies(&self) -> Option<&[&'static str]> {
        Some(&["vue-router", "nuxt", "nuxt3"])
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        depends_on(ctx, &["vue-router", "nuxt", "nuxt3"])
    }

    fn claims_reference(&self, name: &str) -> bool {
        nav_call_re().is_match(name) || name.starts_with("import:") || name.starts_with("layout:")
    }

    fn navigation(&self) -> Option<&NavigationCalls> {
        Some(&self.nav)
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<FrameworkExtractionResult> {
        let entries = parse_vue_routes(content, file_path);
        if entries.is_empty() {
            return Some(FrameworkExtractionResult::default());
        }
        let language = language_for_file(file_path);
        let now = now_ms();
        let mut nodes: Vec<Node> = Vec::new();
        let mut references: Vec<UnresolvedRef> = Vec::new();
        for entry in &entries {
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
                // `calls`, not `references`, for the same reason Next.js binds a page that way: a
                // `references` candidate list is filtered to the ref's own language family, and a
                // router config is `.js` while the component it names is `.vue` — so the right
                // component was dropped and a same-named `.js` function in a store was picked
                // instead. A `calls` edge to a component is read as the page a screen renders.
                let mut r = UnresolvedRef::new(
                    node.id.clone(),
                    component_ref_name(component, entry.spec.as_deref()),
                    EdgeKind::Calls,
                    entry.line,
                    0,
                    file_path,
                    language,
                );
                r.candidates = Some(vec![component.clone()]);
                references.push(r);
            }
            // The layouts around it: what they render — a sidebar, a navbar — is on this screen too.
            for layout in &entry.layouts {
                references.push(UnresolvedRef::new(
                    node.id.clone(),
                    format!("layout:{}", component_ref_name(&layout.component, layout.spec.as_deref())),
                    EdgeKind::References,
                    entry.line,
                    0,
                    file_path,
                    language,
                ));
            }
            nodes.push(node);
        }
        Some(FrameworkExtractionResult { nodes, references })
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        if r.is_kind(EdgeKind::References) && is_vue_route_ref(r) && r.reference_name.starts_with("layout:") {
            let layout = route_component(&r.reference_name["layout:".len()..], &r.file_path, ctx)?;
            let mut resolved = ResolvedRef::new(r, layout.id.clone(), 0.95, ResolvedBy::Framework);
            let mut metadata = serde_json::Map::new();
            metadata.insert("layout".into(), serde_json::Value::Bool(true));
            resolved.metadata = Some(metadata);
            return Some(resolved);
        }
        if !r.is_kind(EdgeKind::Calls) {
            return None;
        }

        // A route naming the component it renders — this resolver's own reference, bound here
        // rather than by name alone: a Vue app usually has a `.vue` `Login` view AND a `login`
        // action in a store, and only one of them is the screen.
        if is_vue_route_ref(r) {
            let component = route_component(&r.reference_name, &r.file_path, ctx)?;
            return Some(ResolvedRef::new(r, component.id.clone(), 0.95, ResolvedBy::Framework));
        }

        let verb = vue_nav_verb(&r.reference_name)?;
        if !ROUTE_LANGUAGES.contains(&r.language) {
            return None;
        }
        let table = vue_route_table(ctx);
        let routes = routes_for_file(&table, &r.file_path)?;
        if routes.table.exact.is_empty() {
            return None;
        }
        let lines = ctx.get_file_lines(&r.file_path)?;

        let arg = first_argument_text(&lines, r.line, r.column, &verb)?;

        // By name first — `{ name: 'login' }` is the idiom Vue apps are written in.
        if let Some(named) = route_name_in_expression(&arg) {
            let target = routes.by_name.get(&named)?;
            let mut resolved = ResolvedRef::new(r, target.id.clone(), 0.95, ResolvedBy::Framework);
            resolved.edge_kind = Some(EdgeKind::Navigates);
            let mut metadata = nav_metadata(&named, &verb, None);
            metadata.insert("by".into(), serde_json::Value::String("name".into()));
            resolved.metadata = Some(metadata);
            return Some(resolved);
        }

        // Otherwise a path, read exactly as every other framework reads one.
        let mut href = parse_href_expression(&arg).or_else(|| parse_vue_path_object(&arg));
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
        // An arm that is no path of the app — an absolute URL, which Nuxt's
        // `navigateTo(url, { external: true })` leaves the app for — opens none of its routes.
        let targets: Vec<_> = destinations_for_href(&href, &routes.table).into_iter().filter(|t| t.href.path.starts_with('/')).collect();
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
    fn names_belong_to_their_own_entry() {
        let src = r#"
import { createRouter } from 'vue-router'
const router = createRouter({
  routes: [
    { name: 'login', path: '/login', component: () => import('@/views/Login.vue') },
    { name: 'profile', path: '/profile/:username', component: Profile },
    { path: '/admin', component: Layout, children: [
      { path: '', component: AdminHome },
      { path: 'users', name: 'users', component: Users },
    ] },
  ],
})
"#;
        let entries = parse_vue_routes(src, "src/router/index.js");
        let summary: Vec<(String, Option<String>, Option<String>)> = entries.iter().map(|e| (e.path.clone(), e.name.clone(), e.component.clone())).collect();
        assert_eq!(
            summary,
            vec![
                ("/login".to_string(), Some("login".to_string()), Some("Login".to_string())),
                ("/profile/:username".to_string(), Some("profile".to_string()), Some("Profile".to_string())),
                ("/admin".to_string(), None, Some("AdminHome".to_string())),
                ("/admin/users".to_string(), Some("users".to_string()), Some("Users".to_string())),
            ]
        );
        assert_eq!(entries[3].layouts, vec![VueLayout { component: "Layout".into(), spec: None }]);
    }

    #[test]
    fn destinations_by_name_or_path() {
        assert_eq!(route_name_in_expression("{ name: 'login', params }").as_deref(), Some("login"));
        assert_eq!(route_name_in_expression("'/login'"), None);
        assert_eq!(parse_vue_path_object("{ path: '/x?y=1', query }").map(|h| h.path), Some("/x".to_string()));
        assert_eq!(vue_nav_verb("this.$router.push").as_deref(), Some("push"));
        assert_eq!(vue_nav_verb("list.push"), None);
    }
}
