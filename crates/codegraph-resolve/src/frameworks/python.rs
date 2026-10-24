//! Python framework resolvers: Django, Flask and FastAPI. Port of
//! `src/resolution/frameworks/python.ts`.

use super::name_heuristic::{pick_by_name_and_kind, NamePickOptions};
use super::now_ms;
use crate::ctx::Ctx;
use crate::import_resolver::resolve_import_path;
use crate::types::{FrameworkExtractionResult, FrameworkResolver, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_types::jscompat::{js_trim, utf16_byte_index, utf16_len};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use indexmap::IndexMap;
use std::collections::HashSet;
use std::sync::Arc;

const LANGUAGES: [Language; 1] = [Language::Python];

// Directory patterns
const MODEL_DIRS: [&str; 3] = ["models", "app/models", "src/models"];
const VIEW_DIRS: [&str; 4] = ["views", "app/views", "src/views", "api/views"];
const FORM_DIRS: [&str; 3] = ["forms", "app/forms", "src/forms"];
const ROUTER_DIRS: [&str; 4] = ["/routers/", "/api/", "/routes/", "/endpoints/"];
const DEP_DIRS: [&str; 3] = ["/dependencies/", "/deps/", "/core/"];

const CLASS_KINDS: [NodeKind; 1] = [NodeKind::Class];
const VIEW_KINDS: [NodeKind; 2] = [NodeKind::Class, NodeKind::Function];
const VARIABLE_KINDS: [NodeKind; 1] = [NodeKind::Variable];
const FUNCTION_KINDS: [NodeKind; 1] = [NodeKind::Function];

/// A framework name heuristic's pick (see `name_heuristic`), preferring these folders.
fn resolve_by_name_and_kind(r: &UnresolvedRef, kinds: &[NodeKind], preferred_dir_patterns: &[&str], ctx: &Ctx) -> Option<String> {
    pick_by_name_and_kind(r, kinds, &|f: &str| preferred_dir_patterns.iter().any(|d| f.contains(d)), ctx, &NamePickOptions::default())
}

fn line_of(text: &str, index: usize) -> u32 {
    text[..index].matches('\n').count() as u32 + 1
}

// ---------------------------------------------------------------------------
// Django
// ---------------------------------------------------------------------------

pub struct DjangoResolver;

impl FrameworkResolver for DjangoResolver {
    fn name(&self) -> &str {
        "django"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        for file in ["requirements.txt", "setup.py", "pyproject.toml"] {
            if ctx.read_file(file).is_some_and(|c| !c.is_empty() && c.to_lowercase().contains("django")) {
                return true;
            }
        }
        ctx.file_exists("manage.py")
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        let name = r.reference_name.as_str();
        if name.ends_with("Model") || re!(r"^[A-Z][a-z]+$").is_match(name) {
            if let Some(result) = resolve_by_name_and_kind(r, &CLASS_KINDS, &MODEL_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.8, ResolvedBy::Framework));
            }
        }
        if name.ends_with("View") || name.ends_with("ViewSet") {
            if let Some(result) = resolve_by_name_and_kind(r, &VIEW_KINDS, &VIEW_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.8, ResolvedBy::Framework));
            }
        }
        if name.ends_with("Form") {
            if let Some(result) = resolve_by_name_and_kind(r, &CLASS_KINDS, &FORM_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.8, ResolvedBy::Framework));
            }
        }
        // ORM dynamic dispatch: QuerySet._fetch_all (and siblings) call `self._iterable_class(self)`
        // — a runtime dispatch to the iterable class (default ModelIterable) whose __iter__ runs
        // the SQL compiler. Static parsing can't resolve an attribute-as-callable, so it leaves an
        // unresolved `_iterable_class` ref and a hole in the QuerySet→compiler chain. Bridge it to
        // ModelIterable.__iter__ so the flow actually exists in the graph.
        if name == "_iterable_class" {
            if let Some(target) = resolve_model_iterable_iter(ctx) {
                return Some(ResolvedRef::new(r, target, 0.7, ResolvedBy::Framework));
            }
        }
        None
    }

    // Let two ref shapes past resolveOne's "no possible match" pre-filter so they reach
    // resolution: the ORM dynamic-dispatch `_iterable_class` (a QuerySet attribute, not a declared
    // symbol), and a Django `include('app.urls')` module path — a dotted module name with no
    // symbol/import to match, which resolution (resolvePythonAbsoluteModule) then maps to its
    // `urls.py` file so the included URLconf records a dependency on the root urlconf.
    fn claims_reference(&self, name: &str) -> bool {
        name == "_iterable_class" || name.ends_with(".urls")
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<FrameworkExtractionResult> {
        if !file_path.ends_with(".py") {
            return Some(FrameworkExtractionResult::default());
        }

        let mut nodes: Vec<Node> = Vec::new();
        let mut references: Vec<UnresolvedRef> = Vec::new();
        let now = now_ms();
        let safe = codegraph_text::strip_comments_for_regex(content, "python");
        let safe: &str = &safe;

        // path('url', handler, name=...) / re_path(r'...', handler) / url(r'...', handler)
        // Capture groups: 1=function name, 2=url string, 3=handler expr
        // Handler expr may contain one balanced () pair (e.g. View.as_view(), include('x.y'))
        let route_regex = re!(r#"\b(path|re_path|url)\s*\(\s*r?['"]([^'"]+)['"]\s*,\s*([\w.]+(?:\s*\([^)]*\))?)"#, "g");
        for m in route_regex.captures_iter(safe) {
            let Some(whole) = m.get(0) else { continue };
            let url_path = m.text(2).unwrap_or("");
            let line = line_of(safe, whole.start());

            let node = Node::new(
                format!("route:{file_path}:{line}:{url_path}"),
                NodeKind::Route,
                url_path,
                format!("{file_path}::route:{url_path}"),
                file_path,
                Language::Python,
                line,
                line,
                0,
                utf16_len(whole.as_str()) as u32,
                now,
            );
            let route_id = node.id.clone();
            nodes.push(node);

            let handler = js_trim(m.text(3).unwrap_or(""));
            if let Some((name, kind)) = resolve_handler_name(handler) {
                references.push(UnresolvedRef::new(route_id, name, kind, line, 0, file_path, Language::Python));
            }
        }

        // DRF router registration: `router.register(r'articles', ArticleViewSet)` → route → the
        // ViewSet class (the core CRUD endpoints, which path()/url() miss). The STRING first arg
        // separates this from `admin.site.register(Model, Admin)` (whose first arg is a model
        // class, not a string); the View/ViewSet suffix on the 2nd arg keeps it to DRF viewsets.
        let router_regex = re!(r#"\.register\s*\(\s*r?['"]([^'"]+)['"]\s*,\s*([\w.]+)"#, "g");
        for m in router_regex.captures_iter(safe) {
            let Some(whole) = m.get(0) else { continue };
            let prefix = re!(r"^\^|/?\$$", "g").replace_all(m.text(1).unwrap_or(""), |_| Some(String::new()));
            let viewset = m.text(2).unwrap_or("").rsplit('.').next().unwrap_or("");
            if !re!(r"View(Set)?$").is_match(viewset) {
                continue;
            }
            let line = line_of(safe, whole.start());
            let node = Node::new(
                format!("route:{file_path}:{line}:VIEWSET:{prefix}"),
                NodeKind::Route,
                format!("VIEWSET /{prefix}"),
                format!("{file_path}::route:{prefix}"),
                file_path,
                Language::Python,
                line,
                line,
                0,
                utf16_len(whole.as_str()) as u32,
                now,
            );
            let route_id = node.id.clone();
            nodes.push(node);
            references.push(UnresolvedRef::new(route_id, viewset, EdgeKind::References, line, 0, file_path, Language::Python));
        }

        Some(FrameworkExtractionResult { nodes, references })
    }
}

/// Find ModelIterable.__iter__ — the default iterable QuerySet invokes via
/// `self._iterable_class(self)`. Its __iter__ statically calls the SQL compiler, so linking the
/// dynamic dispatch here closes the QuerySet→SQL call chain. (Over-approximates to the default
/// iterable; .values()/.values_list() swap in other BaseIterable subclasses, but ModelIterable is
/// the canonical path.)
fn resolve_model_iterable_iter(ctx: &Ctx) -> Option<String> {
    let classes = ctx.get_nodes_by_name("ModelIterable");
    let cls = classes.iter().find(|n| n.kind == NodeKind::Class)?;
    let iters = ctx.get_nodes_by_name("__iter__");
    iters.iter().find(|n| n.file_path == cls.file_path && n.start_line >= cls.start_line && n.start_line <= cls.end_line).map(|n| n.id.clone())
}

/// Parse a Django URL handler expression and return the symbol/module to link. `None` for shapes
/// we can't confidently link (e.g. lambdas).
fn resolve_handler_name(expr: &str) -> Option<(String, EdgeKind)> {
    // include('module.path')
    if let Some(include_match) = re!(r#"^include\s*\(\s*['"]([^'"]+)['"]"#).captures(expr) {
        return Some((include_match.text(1)?.to_string(), EdgeKind::Imports));
    }

    // Strip trailing .as_view(...) or .as_view()
    let head = re!(r"\.as_view\s*\([^)]*\)\s*$").replace_first(expr, |_| String::new());
    // Drop any other trailing method call
    let head = re!(r"\.\w+\s*\([^)]*\)\s*$").replace_first(&head, |_| String::new());

    let last = head.split('.').filter(|s| !s.is_empty()).last()?;
    if !re!(r"^[A-Za-z_][A-Za-z0-9_]*$").is_match(last) {
        return None;
    }

    Some((last.to_string(), EdgeKind::References))
}

// ---------------------------------------------------------------------------
// Flask
// ---------------------------------------------------------------------------

pub struct FlaskResolver;

impl FrameworkResolver for FlaskResolver {
    fn name(&self) -> &str {
        "flask"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        for f in ["requirements.txt", "pyproject.toml", "Pipfile", "setup.py"] {
            if ctx.read_file(f).is_some_and(|c| !c.is_empty() && Re::cached(r"\bflask\b", "i").is_match(&c)) {
                return true;
            }
        }
        // Any app entrypoint (root OR subdir, e.g. conduit/app.py) that imports flask and
        // instantiates Flask(...) — covers Flask(__name__), Flask(__name__.split…), and the
        // app-factory pattern. Bounded to entrypoint-named files.
        let entrypoints: Vec<String> =
            ctx.get_all_files().into_iter().filter(|f| re!(r"(?:^|/)(app|application|main|wsgi|__init__)\.py$").is_match(f)).take(50).collect();
        for f in entrypoints {
            if let Some(c) = ctx.read_file(&f).filter(|c| !c.is_empty()) {
                if re!(r"\bFlask\s*\(").is_match(&c) && re!(r"\bimport\s+flask\b|\bfrom\s+flask\b").is_match(&c) {
                    return true;
                }
            }
        }
        false
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        if r.reference_name.ends_with("_bp") || r.reference_name.ends_with("_blueprint") {
            if let Some(result) = resolve_by_name_and_kind(r, &VARIABLE_KINDS, &[], ctx) {
                return Some(ResolvedRef::new(r, result, 0.8, ResolvedBy::Framework));
            }
        }
        None
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<FrameworkExtractionResult> {
        if !file_path.ends_with(".py") {
            return Some(FrameworkExtractionResult::default());
        }
        let safe = codegraph_text::strip_comments_for_regex(content, "python");
        let decorator = extract_decorator_routes(
            file_path,
            &safe,
            &DecoratorRouteOpts {
                // Flask: @x.route('/path', methods=[...] | (...)) — the handler is the next `def`,
                // allowing intervening decorators (@login_required) and stacked @x.route() lines.
                // methods may be a list OR a tuple (methods=('GET',)).
                decorator_regex: Re::cached(r#"@(\w+)\.route\s*\(\s*['"]([^'"]*)['"](?:\s*,\s*methods\s*=\s*[[(]([^\])]+)[\])])?\s*\)"#, "g"),
                default_method: "GET",
                method_group: None,
                method_from_group: Some(3),
                path_group: 2,
                find_handler: true,
            },
        );
        let restful = extract_flask_restful(file_path, &safe);
        let rules = extract_flask_url_rules(file_path, &safe);
        let mut nodes = decorator.nodes;
        nodes.extend(restful.nodes);
        nodes.extend(rules.nodes);
        let mut references = decorator.references;
        references.extend(restful.references);
        references.extend(rules.references);
        Some(FrameworkExtractionResult { nodes, references })
    }
}

// ---------------------------------------------------------------------------
// FastAPI
// ---------------------------------------------------------------------------

pub struct FastapiResolver;

impl FrameworkResolver for FastapiResolver {
    fn name(&self) -> &str {
        "fastapi"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        let fastapi = Re::cached(r"\bfastapi\b", "i");
        for file in ["requirements.txt", "pyproject.toml"] {
            if ctx.read_file(file).is_some_and(|c| !c.is_empty() && fastapi.is_match(&c)) {
                return true;
            }
        }
        for file in ["app.py", "main.py", "api.py"] {
            if ctx.read_file(file).is_some_and(|c| !c.is_empty() && c.contains("FastAPI(")) {
                return true;
            }
        }
        // A service that is one directory of a monorepo (`backend/pyproject.toml`,
        // `backend/app/main.py`): its manifest or its app object sits below the root.
        let mut looked = 0;
        for file in ctx.get_all_files() {
            let norm = file.replace('\\', "/");
            let base = &norm[norm.rfind('/').map(|i| i + 1).unwrap_or(0)..];
            if base == "requirements.txt" || base == "pyproject.toml" || base == "requirements-dev.txt" {
                if ctx.read_file(&file).is_some_and(|c| !c.is_empty() && fastapi.is_match(&c)) {
                    return true;
                }
                looked += 1;
                if looked >= 40 {
                    break;
                }
            } else if (base == "main.py" || base == "app.py" || base == "api.py") && norm.split('/').count() <= 4 {
                if ctx.read_file(&file).is_some_and(|c| !c.is_empty() && c.contains("FastAPI(")) {
                    return true;
                }
                looked += 1;
                if looked >= 40 {
                    break;
                }
            }
        }
        false
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        let name = r.reference_name.as_str();
        if name.ends_with("_router") || name == "router" {
            if let Some(result) = resolve_by_name_and_kind(r, &VARIABLE_KINDS, &ROUTER_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.8, ResolvedBy::Framework));
            }
        }
        if name.starts_with("get_") || name.starts_with("Depends") {
            if let Some(result) = resolve_by_name_and_kind(r, &FUNCTION_KINDS, &DEP_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.75, ResolvedBy::Framework));
            }
        }
        None
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<FrameworkExtractionResult> {
        if !file_path.ends_with(".py") {
            return Some(FrameworkExtractionResult::default());
        }
        Some(extract_decorator_routes(
            file_path,
            &codegraph_text::strip_comments_for_regex(content, "python"),
            &DecoratorRouteOpts {
                // FastAPI: @x.METHOD('/path') -> handler on the next def line. Path may be empty
                // ("") for routes mounted at the router/prefix root.
                decorator_regex: Re::cached(r#"@(\w+)\.(get|post|put|patch|delete|options|head)\s*\(\s*['"]([^'"]*)['"]"#, "g"),
                default_method: "",
                method_group: Some(2),
                method_from_group: None,
                path_group: 3,
                find_handler: true,
            },
        ))
    }

    /// Cross-file finalization for prefixes. A router's routes are written relative to where it is
    /// mounted —
    ///
    /// ```text
    /// router = APIRouter(prefix="/items")                 # items.py
    /// api_router.include_router(items.router)              # api.py
    /// app.include_router(api_router, prefix="/api/v1")     # main.py
    /// @router.get("/{id}")                                 # → GET /api/v1/items/{id}
    /// ```
    ///
    /// — and per-file `extract()` can only see `GET /{id}`. This pass reads every
    /// `APIRouter(prefix=…)` and every `X.include_router(router, prefix=…)` whose prefix is a
    /// literal (a `settings.API_V1_STR` is unknown and that mount is left alone), resolves the
    /// included router to the file and variable it is (`items.router` through the module import,
    /// `items_router` through the alias, a local name), composes the prefixes down the tree, and
    /// renames the routes decorated with each router to the path a request takes. `id` and
    /// `qualifiedName` are preserved, so the pass is idempotent on every sync.
    fn post_extract(&self, ctx: &Ctx) -> Option<Vec<Node>> {
        struct Mount {
            from_var: String,
            prefix: String,
            target: (String, String), // (file, variable)
        }
        // file → router variable → APIRouter(prefix=)
        let mut own: IndexMap<String, IndexMap<String, String>> = IndexMap::new();
        // mounting file → mounts
        let mut mounts: IndexMap<String, Vec<Mount>> = IndexMap::new();
        // file → decorator line → router variable
        let mut receivers: IndexMap<String, IndexMap<u32, String>> = IndexMap::new();
        for file in ctx.get_all_files() {
            if !file.ends_with(".py") {
                continue;
            }
            let Some(content) = ctx.read_file(&file).filter(|c| !c.is_empty() && (c.contains("APIRouter") || c.contains("include_router"))) else {
                continue;
            };
            let safe = codegraph_text::strip_comments_for_regex(&content, "python");
            let safe: &str = &safe;
            let mut vars: IndexMap<String, String> = IndexMap::new();
            let decl = re!(r"\b([A-Za-z_]\w*)\s*=\s*APIRouter\s*\(([^)]*)\)", "g");
            for m in decl.captures_iter(safe) {
                let prefix = re!(r#"\bprefix\s*=\s*['"]([^'"]*)['"]"#).captures(m.text(2).unwrap_or("")).and_then(|p| p.text(1).map(String::from));
                vars.insert(m.text(1).unwrap_or("").to_string(), prefix.unwrap_or_default());
            }
            let mut by_line: IndexMap<u32, String> = IndexMap::new();
            let deco = re!(r"@([A-Za-z_]\w*)\.(?:get|post|put|patch|delete|options|head)\s*\(", "g");
            for m in deco.captures_iter(safe) {
                let Some(whole) = m.get(0) else { continue };
                by_line.insert(line_of(safe, whole.start()), m.text(1).unwrap_or("").to_string());
            }
            receivers.insert(file.clone(), by_line);
            let inc = re!(r"\b([A-Za-z_]\w*)\.include_router\s*\(\s*([A-Za-z_][\w.]*)\s*((?:,[^)]*)?)\)", "g");
            for m in inc.captures_iter(safe) {
                let rest = m.text(3).unwrap_or("");
                let literal = re!(r#"\bprefix\s*=\s*['"]([^'"]*)['"]"#).captures(rest).and_then(|l| l.text(1).map(String::from));
                if literal.is_none() && re!(r"\bprefix\s*=").is_match(rest) {
                    continue; // a computed prefix: unknown, not guessed
                }
                let Some(target) = included_router(m.text(2).unwrap_or(""), &file, &vars, ctx) else { continue };
                mounts.entry(file.clone()).or_default().push(Mount {
                    from_var: m.text(1).unwrap_or("").to_string(),
                    prefix: literal.unwrap_or_default(),
                    target,
                });
            }
            own.insert(file, vars);
        }
        if mounts.is_empty() && !own.values().any(|vars| vars.values().any(|p| !p.is_empty())) {
            return Some(Vec::new());
        }

        // The include-derived base of each (file, variable): the mounting router's base, plus its
        // own prefix, plus the mount's — to a fixed point.
        let key = |file: &str, variable: &str| format!("{file}\0{variable}");
        let mut base: IndexMap<String, String> = IndexMap::new();
        for _round in 0..8 {
            let mut next: IndexMap<String, String> = IndexMap::new();
            for (file, list) in &mounts {
                for mount in list {
                    let from_base = base.get(&key(file, &mount.from_var)).map(String::as_str).unwrap_or("");
                    let from_own = own.get(file).and_then(|vars| vars.get(&mount.from_var)).map(String::as_str).unwrap_or("");
                    let full = join_py_paths(&join_py_paths(from_base, from_own), &mount.prefix);
                    let k = key(&mount.target.0, &mount.target.1);
                    match next.get(&k) {
                        // two mounts, two paths: ambiguous
                        Some(seen) if *seen != full => {
                            next.insert(k, "\0".to_string());
                        }
                        _ => {
                            next.insert(k, full);
                        }
                    }
                }
            }
            next.retain(|_, v| v != "\0");
            let mut changed = next.len() != base.len();
            if !changed {
                changed = next.iter().any(|(k, v)| base.get(k) != Some(v));
            }
            base = next;
            if !changed {
                break;
            }
        }

        let mut updates: Vec<Node> = Vec::new();
        let no_vars: IndexMap<String, String> = IndexMap::new();
        for (file, by_line) in &receivers {
            let vars = own.get(file).unwrap_or(&no_vars);
            for route in ctx.get_nodes_in_file(file).iter() {
                if route.kind != NodeKind::Route {
                    continue;
                }
                let Some(variable) = by_line.get(&route.start_line).filter(|v| !v.is_empty()) else { continue };
                let prefix = join_py_paths(
                    base.get(&key(file, variable)).map(String::as_str).unwrap_or(""),
                    vars.get(variable).map(String::as_str).unwrap_or(""),
                );
                if prefix.is_empty() || prefix == "/" {
                    continue;
                }
                let Some(sep) = route.qualified_name.find("::") else { continue };
                let Some(colon_rel) = route.qualified_name[sep + 2..].find(':') else { continue };
                let colon = sep + 2 + colon_rel;
                let method = &route.qualified_name[sep + 2..colon];
                let original = &route.qualified_name[colon + 1..];
                let name = js_trim(&format!("{method} {}", join_py_paths(&prefix, original))).to_string();
                if name != route.name {
                    let mut updated = (**route).clone();
                    updated.name = name;
                    updates.push(updated);
                }
            }
        }
        Some(updates)
    }
}

/// `/api/v1` + `/items` → `/api/v1/items`; `/items` + `` → `/items`; `` + `` → ``.
fn join_py_paths(prefix: &str, path: &str) -> String {
    let a = prefix.trim_end_matches('/');
    let b = path.trim_start_matches('/');
    if a.is_empty() {
        return if b.is_empty() { String::new() } else { format!("/{b}") };
    }
    if b.is_empty() {
        a.to_string()
    } else {
        format!("{a}/{b}")
    }
}

/// The file and variable an `include_router` argument names: `items.router` through the module's
/// import, `items_router` through an alias import, or a router defined in the same file.
fn included_router(expr: &str, file: &str, local: &IndexMap<String, String>, ctx: &Ctx) -> Option<(String, String)> {
    let segs: Vec<&str> = expr.split('.').collect();
    let head = segs[0];
    if segs.len() == 1 && local.contains_key(head) {
        return Some((file.to_string(), head.to_string()));
    }
    let mappings = ctx.get_import_mappings(file, Language::Python);
    let mapping = mappings.iter().find(|im| im.local_name == head)?;
    if segs.len() > 1 {
        // `items.router`: `items` is a module — `from app.api.routes import items`.
        let module_file = resolve_import_path(&format!("{}.{}", mapping.source, mapping.exported_name), file, Language::Python, ctx)
            .or_else(|| resolve_import_path(&mapping.source, file, Language::Python, ctx));
        return module_file.map(|f| (f, segs[segs.len() - 1].to_string()));
    }
    // `items_router`: `from .items import router as items_router`.
    let module_file = resolve_import_path(&mapping.source, file, Language::Python, ctx);
    module_file.map(|f| (f, mapping.exported_name.clone()))
}

struct DecoratorRouteOpts {
    decorator_regex: Arc<Re>,
    default_method: &'static str,
    method_group: Option<usize>,
    /// `methods=[...]` list.
    method_from_group: Option<usize>,
    path_group: usize,
    find_handler: bool,
}

fn extract_decorator_routes(file_path: &str, content: &str, opts: &DecoratorRouteOpts) -> FrameworkExtractionResult {
    let mut nodes: Vec<Node> = Vec::new();
    let mut references: Vec<UnresolvedRef> = Vec::new();
    let now = now_ms();
    for m in opts.decorator_regex.captures_iter(content) {
        let Some(whole) = m.get(0) else { continue };
        let route_path = m.text(opts.path_group).unwrap_or("");
        let mut method = opts.default_method.to_string();
        if let Some(text) = opts.method_group.and_then(|g| m.text(g)).filter(|t| !t.is_empty()) {
            method = text.to_uppercase();
        } else if let Some(text) = opts.method_from_group.and_then(|g| m.text(g)).filter(|t| !t.is_empty()) {
            if let Some(mm) = Re::cached(r#"['"]([A-Z]+)['"]"#, "i").captures(text) {
                method = mm.text(1).unwrap_or("").to_uppercase();
            }
        }
        let line = line_of(content, whole.start());
        let shown = if route_path.is_empty() { "/" } else { route_path };
        let name = if method.is_empty() { shown.to_string() } else { format!("{method} {shown}") };
        let node = Node::new(
            format!("route:{file_path}:{line}:{method}:{route_path}"),
            NodeKind::Route,
            name,
            format!("{file_path}::{method}:{route_path}"),
            file_path,
            Language::Python,
            line,
            line,
            0,
            utf16_len(whole.as_str()) as u32,
            now,
        );
        let route_id = node.id.clone();
        nodes.push(node);

        let mut handler_name: Option<String> = None;
        if opts.find_handler {
            let tail = &content[whole.end()..];
            if let Some(def_match) = re!(r"\n\s*(?:async\s+)?def\s+(\w+)").captures(tail) {
                handler_name = def_match.text(1).map(String::from);
            }
        }
        if let Some(handler) = handler_name.filter(|h| !h.is_empty()) {
            references.push(UnresolvedRef::new(route_id, handler, EdgeKind::References, line, 0, file_path, Language::Python));
        }
    }
    FrameworkExtractionResult { nodes, references }
}

/// Flask-RESTful: `api.add_resource(ResourceClass, '/path'[, '/path2'])` (and variants like
/// redash's `add_org_resource`). The ResourceClass holds the HTTP-verb methods (get/post/…), so
/// the route references the class — its verb methods resolve as the handlers via the class.
/// Method is ANY (the class decides which verbs it serves).
fn extract_flask_restful(file_path: &str, safe: &str) -> FrameworkExtractionResult {
    let mut nodes: Vec<Node> = Vec::new();
    let mut references: Vec<UnresolvedRef> = Vec::new();
    let now = now_ms();
    let resource = re!(r#"\.add\w*[Rr]esource\s*\(\s*(\w+)\s*,\s*((?:['"][^'"]+['"]\s*,?\s*)+)"#, "g");
    for m in resource.captures_iter(safe) {
        let Some(whole) = m.get(0) else { continue };
        let class_name = m.text(1).unwrap_or("");
        let paths: Vec<String> = re!(r#"['"]([^'"]+)['"]"#, "g").captures_iter(m.text(2).unwrap_or("")).map(|p| p.text(1).unwrap_or("").to_string()).collect();
        let line = line_of(safe, whole.start());
        for route_path in paths {
            let node = Node::new(
                format!("route:{file_path}:{line}:ANY:{route_path}"),
                NodeKind::Route,
                format!("ANY {route_path}"),
                format!("{file_path}::ANY:{route_path}"),
                file_path,
                Language::Python,
                line,
                line,
                0,
                0,
                now,
            );
            let route_id = node.id.clone();
            nodes.push(node);
            references.push(UnresolvedRef::new(route_id, class_name, EdgeKind::References, line, 0, file_path, Language::Python));
        }
    }
    FrameworkExtractionResult { nodes, references }
}

/// Flask's imperative registration: `bp.add_url_rule('/path', 'name', view_func=View.as_view('name'),
/// methods=[…])`, and a project's own wrapper around it — any call handing a list of path strings
/// and a `view_func=` over, like flaskbb's `register_view(auth, routes=["/login"],
/// view_func=Login.as_view("login"))`. A class-based view (`X.as_view(…)`) serves whatever verbs
/// it defines, so its route is ANY unless `methods=` says; a plain function defaults to GET, as
/// Flask does.
fn extract_flask_url_rules(file_path: &str, safe: &str) -> FrameworkExtractionResult {
    let mut nodes: Vec<Node> = Vec::new();
    let mut references: Vec<UnresolvedRef> = Vec::new();
    let now = now_ms();
    let mut seen: HashSet<String> = HashSet::new();
    let opener = re!(r"(\.add_url_rule|\b[A-Za-z_]\w*)\s*\(");
    let mut pos = 0usize;
    while let Some(m) = opener.captures_at(safe, pos) {
        let Some(whole) = m.get(0) else { break };
        let open = whole.end();
        pos = open;
        let callee = m.text(1).unwrap_or("");
        let Some(args) = balanced_args(safe, open) else { continue };
        if !re!(r"\bview_func\s*=|\.add_url_rule").is_match(&format!("{callee}{args}")) {
            continue;
        }
        let is_rule = callee == ".add_url_rule";
        let mut paths: Vec<String> = Vec::new();
        if is_rule {
            if let Some(first) = re!(r#"^\s*(?:rule\s*=\s*)?(['"])([^'"]*)\1"#).captures(args) {
                paths = vec![first.text(2).unwrap_or("").to_string()];
            }
        } else if let Some(list) = re!(r#"(?:^|,)\s*(?:routes|rules|urls|paths)?\s*=?\s*\[((?:\s*(['"])[^'"]*\2\s*,?)+)\]"#).captures(args) {
            paths = re!(r#"(['"])([^'"]*)\1"#, "g").captures_iter(list.text(1).unwrap_or("")).map(|q| q.text(2).unwrap_or("").to_string()).collect();
        }
        paths.retain(|p| p.starts_with('/'));
        if paths.is_empty() {
            continue;
        }
        // `view_func=X` / `view_func=X.as_view(…)`, or add_url_rule's third positional argument.
        let expr: Option<String> = re!(r"\bview_func\s*=\s*([A-Za-z_][\w.]*)").captures(args).and_then(|c| c.text(1).map(String::from)).or_else(|| {
            if is_rule {
                re!(r#"^\s*(['"])[^'"]*\1\s*,\s*(?:(['"])[^'"]*\2|None)\s*,\s*([A-Za-z_][\w.]*)"#).captures(args).and_then(|c| c.text(3).map(String::from))
            } else {
                None
            }
        });
        let Some(expr) = expr.filter(|e| !e.is_empty()) else { continue };
        let class_view = expr.ends_with(".as_view");
        let target = if class_view { &expr[..expr.len() - ".as_view".len()] } else { expr.as_str() };
        let listed = re!(r"\bmethods\s*=\s*[[(]([^\])]*)[\])]").captures(args).and_then(|c| c.text(1).map(String::from)).filter(|l| !l.is_empty());
        let methods: Vec<String> = match listed {
            Some(listed) => re!(r#"['"](\w+)['"]"#, "g").captures_iter(&listed).map(|q| q.text(1).unwrap_or("").to_uppercase()).collect(),
            None => vec![if class_view { "ANY" } else { "GET" }.to_string()],
        };
        let line = line_of(safe, whole.start());
        let name = target.rsplit('.').next().unwrap_or("");
        for route_path in &paths {
            for method in &methods {
                let id = format!("route:{file_path}:{line}:{method}:{route_path}");
                if seen.contains(&id) {
                    continue;
                }
                seen.insert(id.clone());
                nodes.push(Node::new(
                    id.clone(),
                    NodeKind::Route,
                    format!("{method} {route_path}"),
                    format!("{file_path}::{method}:{route_path}"),
                    file_path,
                    Language::Python,
                    line,
                    line,
                    0,
                    0,
                    now,
                ));
                references.push(UnresolvedRef::new(id, name, EdgeKind::References, line, 0, file_path, Language::Python));
            }
        }
        pos = open + args.len();
    }
    FrameworkExtractionResult { nodes, references }
}

/// The text between an opening `(` at `open` and its matching `)`, or `None`. Strings are skipped.
fn balanced_args(text: &str, open: usize) -> Option<&str> {
    let b = text.as_bytes();
    let limit = (open + utf16_byte_index(&text[open..], 4000)).min(b.len());
    let mut depth = 1i32;
    let mut quote: Option<u8> = None;
    let mut i = open;
    while i < limit {
        let ch = b[i];
        if let Some(q) = quote {
            if ch == b'\\' {
                i += 1;
            } else if ch == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if ch == b'"' || ch == b'\'' {
            quote = Some(ch);
        } else if matches!(ch, b'(' | b'[' | b'{') {
            depth += 1;
        } else if matches!(ch, b')' | b']' | b'}') {
            depth -= 1;
            if depth == 0 {
                return text.get(open..i);
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn django_urls_and_viewsets() {
        let src = "urlpatterns = [\n    path('home/', views.home, name='home'),\n    path('api/', include('app.urls')),\n    re_path(r'^list$', ListView.as_view()),\n]\nrouter.register(r'articles', ArticleViewSet)\nadmin.site.register(r'x', Thing)\n";
        let result = DjangoResolver.extract("urls.py", src).unwrap();
        let names: Vec<&str> = result.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["home/", "api/", "^list$", "VIEWSET /articles"]);
        let refs: Vec<(&str, bool)> = result.references.iter().map(|r| (r.reference_name.as_str(), r.is_kind(EdgeKind::Imports))).collect();
        assert_eq!(refs, vec![("home", false), ("app.urls", true), ("ListView", false), ("ArticleViewSet", false)]);
    }

    #[test]
    fn flask_routes_methods_and_rules() {
        let src = "@app.route('/a', methods=['POST'])\n@login_required\ndef a(): pass\n\n@bp.route('/b')\nasync def b(): pass\n\napi.add_resource(Users, '/users', '/people')\nbp.add_url_rule('/c', 'c', view_func=CView.as_view('c'))\n";
        let result = FlaskResolver.extract("app.py", src).unwrap();
        let names: Vec<&str> = result.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["POST /a", "GET /b", "ANY /users", "ANY /people", "ANY /c"]);
        let refs: Vec<&str> = result.references.iter().map(|r| r.reference_name.as_str()).collect();
        assert_eq!(refs, vec!["a", "b", "Users", "Users", "CView"]);
    }

    #[test]
    fn fastapi_decorators() {
        let src = "@router.get('/{id}')\nasync def read(id): pass\n\n@app.post('')\ndef create(): pass\n";
        let result = FastapiResolver.extract("main.py", src).unwrap();
        let names: Vec<&str> = result.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["GET /{id}", "POST /"]);
    }

    #[test]
    fn py_paths_join() {
        assert_eq!(join_py_paths("/api/v1", "/items"), "/api/v1/items");
        assert_eq!(join_py_paths("/items", ""), "/items");
        assert_eq!(join_py_paths("", ""), "");
        assert_eq!(join_py_paths("", "x"), "/x");
    }
}
