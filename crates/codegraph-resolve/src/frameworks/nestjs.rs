//! NestJS framework resolver. Port of `src/resolution/frameworks/nestjs.ts`.
//!
//! Handles NestJS decorator-based routing across its transport layers:
//!   - HTTP:          @Controller(prefix) + @Get/@Post/@Put/@Patch/@Delete/@Head/@Options/@All
//!   - GraphQL:       @Resolver + @Query/@Mutation/@Subscription
//!   - Microservices: @MessagePattern / @EventPattern
//!   - WebSockets:    @WebSocketGateway(namespace) + @SubscribeMessage(event)
//!
//! Like the other framework extractors this is regex-over-source (comment-stripped), not AST
//! traversal. NestJS differs from Spring/ASP.NET in two ways that this resolver has to account for:
//!
//!   1. An HTTP route's path is split across TWO decorators — the class-level `@Controller` prefix
//!      and the method-level `@Get`/`@Post` path — and both are frequently empty (`@Controller()`,
//!      `@Get()`). We pair each method decorator with its enclosing class and join the two paths.
//!
//!   2. `@Query()` is overloaded: it's a GraphQL *method* decorator (from `@nestjs/graphql`) AND a
//!      REST *parameter* decorator (from `@nestjs/common`). We only treat it as GraphQL when it
//!      sits inside an `@Resolver` class, which is what disambiguates the two.
//!
//! Offsets are byte offsets into the comment-stripped source; every character the scanners look
//! at is ASCII (the decorator length, which becomes a column, is counted in UTF-16 units).

use super::now_ms;
use super::package_deps::declared_dependencies;
use crate::ctx::Ctx;
use crate::types::{FrameworkExtractionResult, FrameworkResolver, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_types::jscompat::{is_js_space, js_trim, utf16_byte_index, utf16_len};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use indexmap::IndexMap;
use std::collections::HashSet;

pub struct NestjsResolver;

const LANGUAGES: [Language; 2] = [Language::TypeScript, Language::JavaScript];

const HTTP_METHODS: [&str; 8] = ["Get", "Post", "Put", "Patch", "Delete", "Head", "Options", "All"];
const GQL_OPS: [&str; 3] = ["Query", "Mutation", "Subscription"];

const PROVIDER_CONVENTIONS: [(&str, &str); 9] = [
    ("Service", ".service."),
    ("Controller", ".controller."),
    ("Resolver", ".resolver."),
    ("Gateway", ".gateway."),
    ("Repository", ".repository."),
    ("Guard", ".guard."),
    ("Interceptor", ".interceptor."),
    ("Pipe", ".pipe."),
    ("Module", ".module."),
];

/// The version marker for `VERSION_NEUTRAL`: served without a version segment.
const NEUTRAL: &str = "\u{0}neutral";

fn is_script_file(file_path: &str) -> bool {
    re!(r"\.(m?js|tsx?|cjs)$").is_match(file_path)
}

fn is_nest_script_file(file_path: &str) -> bool {
    re!(r"\.(m?[jt]s|cjs)$").is_match(file_path)
}

fn detect_language(file_path: &str) -> Language {
    if file_path.ends_with(".ts") || file_path.ends_with(".tsx") {
        Language::TypeScript
    } else {
        Language::JavaScript
    }
}

fn lang_name(language: Language) -> &'static str {
    if language == Language::TypeScript {
        "typescript"
    } else {
        "javascript"
    }
}

impl FrameworkResolver for NestjsResolver {
    fn name(&self) -> &str {
        "nestjs"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        // Primary, fast path: any @nestjs/* dependency in a package.json — the root's, or a
        // workspace's (`apps/api/`, `server/`).
        if declared_dependencies(ctx).iter().any(|name| name.starts_with("@nestjs/")) {
            return true;
        }

        // Fallback: NestJS-specific decorators in conventionally named files.
        for file in ctx.get_all_files() {
            if file.ends_with(".controller.ts")
                || file.ends_with(".controller.js")
                || file.ends_with(".module.ts")
                || file.ends_with(".resolver.ts")
                || file.ends_with(".gateway.ts")
            {
                if let Some(content) = ctx.read_file(&file).filter(|c| !c.is_empty()) {
                    if content.contains("@nestjs/")
                        || content.contains("@Controller")
                        || content.contains("@Module(")
                        || content.contains("@Resolver(")
                        || content.contains("@WebSocketGateway(")
                    {
                        return true;
                    }
                }
            }
        }

        false
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        // Resolve provider/controller references (e.g. constructor-injected `UsersService`) to
        // their class, preferring the Nest file-name convention (`*.service.ts`,
        // `*.controller.ts`, …).
        for (suffix, convention) in PROVIDER_CONVENTIONS {
            if !r.reference_name.ends_with(suffix) {
                continue;
            }
            let by_name = ctx.get_nodes_by_name(&r.reference_name);
            let candidates: Vec<_> = by_name.iter().filter(|n| n.kind == NodeKind::Class).collect();
            if candidates.is_empty() {
                return None;
            }
            // A TS module reaches another file's class only by importing it — the import
            // resolver's — so a guess by name stays in the feature: the file itself, or the
            // convention file beside it (`users.controller.ts` → `users.service.ts`). Every Nest
            // sample app declares its own `CatsService`, and the file-name preference sent specs
            // across apps.
            if let Some(own) = candidates.iter().find(|n| n.file_path == r.file_path) {
                return Some(ResolvedRef::new(r, own.id.clone(), 0.85, ResolvedBy::Framework));
            }
            if re!(r"\.(?:m?[jt]sx?|cjs|cts|mts)$").is_match(&r.file_path) {
                let dir = &r.file_path[..r.file_path.rfind('/').map(|i| i + 1).unwrap_or(0)];
                let beside = candidates
                    .iter()
                    .find(|n| n.file_path.contains(convention) && n.file_path.starts_with(dir) && !n.file_path[dir.len()..].contains('/'));
                return beside.map(|n| ResolvedRef::new(r, n.id.clone(), 0.85, ResolvedBy::Framework));
            }
            let preferred = candidates.iter().find(|n| n.file_path.contains(convention));
            let target = preferred.unwrap_or(&candidates[0]);
            return Some(ResolvedRef::new(r, target.id.clone(), if preferred.is_some() { 0.85 } else { 0.7 }, ResolvedBy::Framework));
        }
        None
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<FrameworkExtractionResult> {
        if !is_script_file(file_path) {
            return Some(FrameworkExtractionResult::default());
        }
        let mut nodes: Vec<Node> = Vec::new();
        let mut references: Vec<UnresolvedRef> = Vec::new();
        let now = now_ms();
        let lang = detect_language(file_path);
        let safe = codegraph_text::strip_comments_for_regex(content, lang_name(lang));
        let safe: &str = &safe;

        let mut add_route = |index: usize, method: &str, path: &str, length: usize, handler: Option<&str>| {
            let line = line_at(safe, index);
            let node = Node::new(
                format!("route:{file_path}:{line}:{method}:{path}"),
                NodeKind::Route,
                format!("{method} {path}"),
                format!("{file_path}::{method}:{path}"),
                file_path,
                lang,
                line,
                line,
                0,
                length as u32,
                now,
            );
            let id = node.id.clone();
            nodes.push(node);
            if let Some(handler) = handler.filter(|h| !h.is_empty()) {
                references.push(UnresolvedRef::new(id, handler, EdgeKind::References, line, 0, file_path, lang));
            }
        };

        let scopes = build_class_scopes(safe);

        // HTTP routes: method decorator path joined onto the enclosing controller's prefix.
        for hit in find_decorators(safe, &HTTP_METHODS) {
            let scope = scope_for(&scopes, hit.index);
            let prefix = match scope {
                Some(s) if s.kind == ClassKind::Controller => s.prefix.as_str(),
                _ => "",
            };
            let path = join_http_path(prefix, &parse_string_arg(&hit.args));
            add_route(hit.index, &hit.name.to_uppercase(), &path, hit.length, method_name_after(safe, hit.end).as_deref());
        }

        // GraphQL operations: only inside an @Resolver class (disambiguates the REST `@Query()`
        // parameter decorator, which lives inside @Controller classes).
        for hit in find_decorators(safe, &GQL_OPS) {
            let scope = scope_for(&scopes, hit.index);
            if !scope.is_some_and(|s| s.kind == ClassKind::Resolver) {
                continue;
            }
            let handler = method_name_after(safe, hit.end);
            let name = parse_graphql_name(&hit.args, handler.as_deref());
            add_route(hit.index, &hit.name.to_uppercase(), &name, hit.length, handler.as_deref());
        }

        // Microservice message/event handlers.
        for hit in find_decorators(safe, &["MessagePattern", "EventPattern"]) {
            let verb = if hit.name == "EventPattern" { "EVENT" } else { "MESSAGE" };
            let handler = method_name_after(safe, hit.end);
            let arg = parse_string_arg(&hit.args);
            let name = if !arg.is_empty() { arg } else { handler.clone().unwrap_or_default() };
            add_route(hit.index, verb, &name, hit.length, handler.as_deref());
        }

        // WebSocket message handlers, prefixed with the gateway namespace when present.
        for hit in find_decorators(safe, &["SubscribeMessage"]) {
            let scope = scope_for(&scopes, hit.index);
            let namespace = match scope {
                Some(s) if s.kind == ClassKind::Gateway => s.prefix.as_str(),
                _ => "",
            };
            let handler = method_name_after(safe, hit.end);
            let arg = parse_string_arg(&hit.args);
            let event = if !arg.is_empty() { arg } else { handler.clone().unwrap_or_default() };
            let name = if !namespace.is_empty() { format!("{namespace}:{event}") } else { event };
            add_route(hit.index, "WS", &name, hit.length, handler.as_deref());
        }

        Some(FrameworkExtractionResult { nodes, references })
    }

    /// Cross-file finalization for `RouterModule.register([...])`. The per-file `extract()` above
    /// only sees `@Controller(prefix) + @Get(path)` — it can't learn about the route prefix
    /// supplied by a sibling `app.module.ts` like:
    ///
    /// ```text
    /// RouterModule.register([
    ///   { path: 'admin', module: AdminModule, children: [
    ///     { path: 'users', module: UsersModule } ] } ])
    /// ```
    ///
    /// This pass scans every `*.module.{ts,js}` file, walks the registration tree to build a
    /// `Module → /full/prefix` map, walks each `@Module({ controllers: [...] })` to build a
    /// `Controller → Module` map, and rewrites affected route nodes so `GET /` becomes
    /// `GET /admin/users` (and `@Controller('foo') + @Get(':id')` under that same module becomes
    /// `GET /admin/users/foo/:id`).
    ///
    /// The route node's `id` and `qualifiedName` are deliberately preserved across the update:
    /// `id` because existing route→handler edges reference it, `qualifiedName` because it still
    /// encodes the *original* in-file `method:path` — which keeps this pass idempotent (a second
    /// run recovers the same input regardless of how many times it has already prefixed).
    fn post_extract(&self, ctx: &Ctx) -> Option<Vec<Node>> {
        let mut module_to_prefix: IndexMap<String, String> = IndexMap::new();
        let mut controller_to_module: IndexMap<String, String> = IndexMap::new();
        let mut bootstraps: Vec<NestBootstrap> = Vec::new();

        for file_path in ctx.get_all_files() {
            if !is_nest_script_file(&file_path) {
                continue;
            }
            let is_module = re!(r"\.module\.(m?[jt]s|cjs)$").is_match(&file_path);
            if !is_module && !ctx.file_contains(&file_path, "NestFactory") {
                continue;
            }
            let Some(content) = ctx.read_file(&file_path).filter(|c| !c.is_empty()) else { continue };
            let safe = codegraph_text::strip_comments_for_regex(&content, lang_name(detect_language(&file_path)));
            if is_module {
                collect_router_module_registrations(&safe, &mut module_to_prefix);
                collect_module_controllers(&safe, &mut controller_to_module);
            }
            // Every app counts, even one that adds nothing: its routes must not borrow another
            // app's prefix.
            if re!(r"\bNestFactory\s*\.\s*create").is_match(&safe) {
                bootstraps.push(NestBootstrap { root: app_root_of(&file_path), config: read_bootstrap(&safe) });
            }
        }

        let mut controller_to_prefix: IndexMap<String, String> = IndexMap::new();
        for (controller, module) in &controller_to_module {
            // `''` and `'/'` are no-op prefixes; skip them so we don't run updates that would set
            // name to the value it already has.
            if let Some(prefix) = module_to_prefix.get(module).filter(|p| !p.is_empty() && *p != "/") {
                controller_to_prefix.insert(controller.clone(), prefix.clone());
            }
        }

        let adds = bootstraps.iter().any(|b| !b.config.prefix.is_empty() || b.config.uri);
        if controller_to_prefix.is_empty() && !adds {
            return Some(Vec::new());
        }

        // Route → what the app adds to it. A RouterModule prefix binds through the controller
        // class's line range; the bootstrap's prefix and versioning only to a route its source
        // shows is in a `@Controller` — an Express route has the same qualified-name shape.
        let mut plan: IndexMap<String, Planned> = IndexMap::new();
        for (controller_name, prefix) in &controller_to_prefix {
            for cls in ctx.get_nodes_by_name(controller_name).iter().filter(|n| n.kind == NodeKind::Class) {
                for route in ctx.get_nodes_in_file(&cls.file_path).iter() {
                    if route.kind != NodeKind::Route || route.start_line < cls.start_line || route.start_line > cls.end_line {
                        continue;
                    }
                    plan.insert(route.id.clone(), Planned { route: (**route).clone(), module_prefix: prefix.clone(), config: None, version: None });
                }
            }
        }
        if adds {
            for file_path in ctx.get_all_files() {
                if !is_nest_script_file(&file_path) {
                    continue;
                }
                if !ctx.file_contains(&file_path, "@Controller") {
                    continue;
                }
                let Some(config) = bootstrap_for(&bootstraps, &file_path) else { continue };
                if config.prefix.is_empty() && !config.uri {
                    continue;
                }
                let routes: Vec<_> = ctx
                    .get_nodes_in_file(&file_path)
                    .iter()
                    .filter(|n| n.kind == NodeKind::Route && re!(r"::(?:GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS|ALL):").is_match(&n.qualified_name))
                    .cloned()
                    .collect();
                if routes.is_empty() {
                    continue;
                }
                let Some(content) = ctx.read_file(&file_path).filter(|c| !c.is_empty()) else { continue };
                let safe = codegraph_text::strip_comments_for_regex(&content, lang_name(detect_language(&file_path)));
                let scopes = build_class_scopes(&safe);
                let hits = find_decorators(&safe, &HTTP_METHODS);
                for route in routes {
                    let Some(hit) = hits.iter().find(|h| line_at(&safe, h.index) == route.start_line) else { continue };
                    let Some(scope) = scope_for(&scopes, hit.index).filter(|s| s.kind == ClassKind::Controller) else { continue };
                    let module_prefix = plan.get(&route.id).map(|p| p.module_prefix.clone()).unwrap_or_default();
                    plan.insert(
                        route.id.clone(),
                        Planned { route: (*route).clone(), module_prefix, config: Some(config.clone()), version: route_version(&safe, hit, scope) },
                    );
                }
            }
        }

        let mut updates: Vec<Node> = Vec::new();
        for planned in plan.values() {
            if let Some(updated) = apply_app_config(&planned.route, &planned.module_prefix, planned.config.as_ref(), planned.version.as_ref()) {
                if updated.name != planned.route.name {
                    updates.push(updated);
                }
            }
        }
        Some(updates)
    }
}

/// A route and what the app adds to it. `version` is `None` for "the route says nothing"
/// (`undefined`), `Some(None)` for a version that is not a literal (`null`).
struct Planned {
    route: Node,
    module_prefix: String,
    config: Option<NestAppConfig>,
    version: Option<Option<String>>,
}

// ---------------------------------------------------------------------------
// The app's bootstrap: global prefix and URI versioning
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct NestAppConfig {
    /// `setGlobalPrefix('api')` — '' when none, or not a literal.
    prefix: String,
    /// Paths the prefix skips: `exclude: ['health', { path: 'metrics', method }]`, without a
    /// leading `/`.
    excludes: Vec<String>,
    /// URI versioning is on (`enableVersioning({ type: VersioningType.URI })`, the default type).
    uri: bool,
    /// `defaultVersion: '1'` — the version a route without its own is served at; `None` for none.
    default_version: Option<String>,
    /// The segment's prefix: `v` unless `prefix:` says otherwise (`false` is none).
    version_prefix: String,
}

struct NestBootstrap {
    root: String,
    config: NestAppConfig,
}

/// The app a file belongs to: the directory above its `src/` (`apps/api/`), else the repository.
fn app_root_of(file_path: &str) -> String {
    re!(r"^((?:[^/]+/)*?)src/").captures(file_path).and_then(|m| m.text(1).map(String::from)).unwrap_or_default()
}

/// The config of the app that holds `file_path`: the bootstrap with the longest root above it,
/// else — for a file outside every app, like an Nx `libs/` controller — the app, when there is
/// only one.
fn bootstrap_for<'a>(bootstraps: &'a [NestBootstrap], file_path: &str) -> Option<&'a NestAppConfig> {
    let mut root: Option<&str> = None;
    for b in bootstraps {
        if file_path.starts_with(b.root.as_str()) && root.is_none_or(|r| utf16_len(&b.root) > utf16_len(r)) {
            root = Some(&b.root);
        }
    }
    let root = match root {
        Some(root) => root,
        None => {
            let distinct: HashSet<&str> = bootstraps.iter().map(|b| b.root.as_str()).collect();
            if distinct.len() != 1 {
                return None;
            }
            bootstraps[0].root.as_str()
        }
    };
    // A seed or migration script creates the same app without serving it: the bootstrap that
    // configures the app is the one that serves it.
    let here: Vec<&NestBootstrap> = bootstraps.iter().filter(|b| b.root == root).collect();
    here.iter().find(|b| !b.config.prefix.is_empty() || b.config.uri).or(here.first()).map(|b| &b.config)
}

/// One version value: `'2'`, `['1', '2']` (the first), `VERSION_NEUTRAL`.
fn version_value(text: &str) -> Option<String> {
    let t = js_trim(text);
    if re!(r"^VERSION_NEUTRAL\b").is_match(t) {
        return Some(NEUTRAL.to_string());
    }
    re!(r#"^\[?\s*(['"`])([^'"`]*)\1"#).captures(t).and_then(|lit| lit.text(2).map(String::from))
}

/// `setGlobalPrefix` and `enableVersioning` from the file that calls `NestFactory.create`.
fn read_bootstrap(safe: &str) -> NestAppConfig {
    let mut config = NestAppConfig { prefix: String::new(), excludes: Vec::new(), uri: false, default_version: None, version_prefix: "v".to_string() };
    if let Some(prefix_call) = re!(r"\.setGlobalPrefix\s*\(").captures(safe) {
        let whole = prefix_call.get(0).unwrap();
        let open = whole.end() - 1;
        let close = matching_close(safe, open);
        let args = match close {
            Some(close) if close > open => &safe[open + 1..close],
            _ => "",
        };
        if let Some(lit) = re!(r#"^\s*(['"`])([^'"`$]*)\1"#).captures(args) {
            config.prefix = lit.text(2).unwrap_or("").trim_matches('/').to_string();
            if let Some(ex) = re!(r"\bexclude\s*:\s*\[").captures(args) {
                let ex_open = ex.get(0).unwrap().end() - 1;
                let list = match matching_close(args, ex_open) {
                    Some(ex_close) if ex_close > ex_open => &args[ex_open + 1..ex_close],
                    _ => "",
                };
                for item in split_top_level(list) {
                    let text = js_trim(&item);
                    let path = re!(r#"^(['"])([^'"]*)\1$"#)
                        .captures(text)
                        .and_then(|m| m.text(2).map(String::from))
                        .or_else(|| re!(r#"\bpath\s*:\s*(['"])([^'"]*)\1"#).captures(text).and_then(|m| m.text(2).map(String::from)));
                    if let Some(path) = path {
                        config.excludes.push(path.trim_start_matches('/').to_string());
                    }
                }
            }
        }
    }
    if let Some(versioning) = re!(r"\.enableVersioning\s*\(").captures(safe) {
        let whole = versioning.get(0).unwrap();
        let open = whole.end() - 1;
        let args = match matching_close(safe, open) {
            Some(close) if close > open => &safe[open + 1..close],
            _ => "",
        };
        let ty = re!(r"\btype\s*:\s*VersioningType\s*\.\s*(\w+)").captures(args).and_then(|m| m.text(1).map(String::from));
        config.uri = ty.as_deref().is_none_or(|t| t == "URI");
        if let Some(def) = re!(r"\bdefaultVersion\s*:\s*").captures(args) {
            config.default_version = version_value(&args[def.get(0).unwrap().end()..]);
        }
        if let Some(pre) = re!(r#"\bprefix\s*:\s*(false|(['"`])([^'"`]*)\2)"#).captures(args) {
            config.version_prefix = if pre.text(1) == Some("false") { String::new() } else { pre.text(3).unwrap_or("").to_string() };
        }
    }
    config
}

/// A route excluded from the global prefix: `health`, `health/(.*)`, `health{/*wildcard}`,
/// `health/*`.
fn is_excluded(path: &str, excludes: &[String]) -> bool {
    let route = path.trim_start_matches('/');
    for pattern in excludes {
        let wild = re!(r"(?:\{/?\*\w*\}|/?\(\.\*\)|/?\*\w*)$").find(pattern);
        let base = match &wild {
            Some(w) => pattern[..w.start()].trim_end_matches('/'),
            None => pattern.trim_end_matches('/'),
        };
        if route == base || (wild.is_some() && route.starts_with(&format!("{base}/"))) {
            return true;
        }
    }
    false
}

/// The version a route is served at: `@Version(…)` among its own decorators, else its
/// controller's (`@Version(…)` on the class, `@Controller({ version })`). `None` when neither
/// says; `Some(None)` for a version that is not a literal.
fn route_version(safe: &str, hit: &DecoratorHit, scope: &ClassScope) -> Option<Option<String>> {
    // The method's decorators: back through the ones stacked above this one…
    let from = decorator_chain_start(safe, hit.index, scope.start);
    // …and on to the handler itself — past the decorators between (`@UseGuards(…)`,
    // `@Version('2')`).
    let handler = method_name_after(safe, hit.end);
    let at = handler.as_ref().and_then(|h| {
        let pattern = format!(r"(?<![@\w$]){}\s*\(", h.replace('$', r"\$"));
        Re::cached(&pattern, "").find(&safe[hit.end..]).map(|m| m.start())
    });
    let to = hit.end + at.unwrap_or(0);
    if let Some(own) = re!(r"@Version\s*\(([^)]*)\)").captures(sub(safe, from, to)) {
        return Some(version_value(own.text(1).unwrap_or("")));
    }
    // The class's decorators, on either side of `@Controller`.
    let class_at = re!(r"\bclass\b").captures_at(safe, scope.start).and_then(|m| m.get(0)).map(|w| w.start());
    let head_end = class_at.unwrap_or_else(|| scope.start + utf16_byte_index(&safe[scope.start..], 400));
    let head = sub(safe, decorator_chain_start(safe, scope.start, 0), head_end);
    if let Some(on_class) = re!(r"@Version\s*\(([^)]*)\)").captures(head) {
        return Some(version_value(on_class.text(1).unwrap_or("")));
    }
    if let Some(in_controller) = re!(r"@Controller\s*\(\s*\{[^)]*\bversion\s*:\s*([^,}]+)").captures(head) {
        return Some(version_value(in_controller.text(1).unwrap_or("")));
    }
    None
}

/// `s[from..to]`, empty when the range is not one.
fn sub(s: &str, from: usize, to: usize) -> &str {
    s.get(from..to).unwrap_or("")
}

/// Where the stack of decorators ending at `at` begins: back over each `@Name(…)` / `@Name`
/// above it. A decorator's arguments may hold an object (`@ApiResponse({ status: 200 })`), so
/// this steps over balanced parentheses rather than stopping at the first brace.
fn decorator_chain_start(safe: &str, at: usize, floor: usize) -> usize {
    let b = safe.as_bytes();
    let mut start = at;
    loop {
        let mut i = start;
        while i > floor {
            let Some(c) = safe[..i].chars().next_back() else { break };
            if !is_js_space(c) {
                break;
            }
            i -= c.len_utf8();
        }
        if i > 0 && b[i - 1] == b')' {
            let mut depth = 0i32;
            let mut j = i as isize - 1;
            while j >= floor as isize {
                if b[j as usize] == b')' {
                    depth += 1;
                } else if b[j as usize] == b'(' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                j -= 1;
            }
            if j < floor as isize {
                return start;
            }
            i = j as usize;
        }
        let mut name = i;
        while name > floor && (b[name - 1].is_ascii_alphanumeric() || matches!(b[name - 1], b'_' | b'$' | b'.')) {
            name -= 1;
        }
        if name == i || name == 0 || b[name - 1] != b'@' {
            return start;
        }
        start = name - 1;
    }
}

/// A route's name with everything the app adds to it, in NestJS's order:
/// `/{globalPrefix}/{v}{version}/{RouterModule prefix}/{controller}/{method}`. Recomputed from the
/// in-file `method:path` in the qualified name, so the pass is idempotent.
fn apply_app_config(route: &Node, module_prefix: &str, config: Option<&NestAppConfig>, version: Option<&Option<String>>) -> Option<Node> {
    let sep = route.qualified_name.find("::")?;
    let tail = &route.qualified_name[sep + 2..];
    let colon = tail.find(':')?;
    let method = &tail[..colon];
    let inner = join_http_path(module_prefix, &tail[colon + 1..]);
    let mut path = inner.clone();
    if let Some(config) = config {
        let v: Option<&str> = match version {
            None => config.default_version.as_deref(),
            Some(v) => v.as_deref(),
        };
        let version_seg = match v {
            Some(v) if config.uri && v != NEUTRAL => format!("{}{}", config.version_prefix, v),
            _ => String::new(),
        };
        let prefix = if !config.prefix.is_empty() && !is_excluded(&inner, &config.excludes) { config.prefix.as_str() } else { "" };
        path = join_http_path(&join_http_path(prefix, &version_seg), &inner);
    }
    let mut updated = route.clone();
    updated.name = format!("{method} {path}");
    updated.updated_at = now_ms();
    Some(updated)
}

// ---------------------------------------------------------------------------
// Decorator scanning
// ---------------------------------------------------------------------------

struct DecoratorHit {
    /// Decorator name without the leading `@` (e.g. `Get`).
    name: String,
    /// Raw text between the decorator's parentheses.
    args: String,
    /// Index of the leading `@` in the (comment-stripped) source.
    index: usize,
    /// Index just past the decorator's closing `)`.
    end: usize,
    /// Character (UTF-16) length of the whole `@Name(...)` decorator.
    length: usize,
}

/// Find every `@Name(...)` decorator whose name is in `names`. Uses a string-aware balanced-paren
/// reader for the argument list so type thunks like `@Query(() => [User])` are captured whole
/// rather than truncated at the inner `()`.
fn find_decorators(safe: &str, names: &[&str]) -> Vec<DecoratorHit> {
    let mut hits: Vec<DecoratorHit> = Vec::new();
    let decorator = Re::cached(&format!(r"@({})\s*\(", names.join("|")), "");
    let mut pos = 0;
    while let Some(m) = decorator.captures_at(safe, pos) {
        let Some(whole) = m.get(0) else { break };
        let open_index = whole.end() - 1; // position of '('
        pos = whole.end();
        let Some(parsed) = read_args(safe, open_index) else { continue };
        hits.push(DecoratorHit {
            name: m.text(1).unwrap_or("").to_string(),
            length: utf16_len(&safe[whole.start()..parsed.end]),
            args: parsed.args,
            index: whole.start(),
            end: parsed.end,
        });
        pos = parsed.end; // resume past the args so nested text isn't re-scanned
    }
    hits
}

struct ReadArgs {
    args: String,
    end: usize,
}

/// Read a balanced `(...)` starting at `open_index` (which must point at `(`). String-aware, so
/// parens inside string literals don't unbalance the count. Returns the inner text and the index
/// just past the closing `)`.
fn read_args(s: &str, open_index: usize) -> Option<ReadArgs> {
    let b = s.as_bytes();
    if b.get(open_index) != Some(&b'(') {
        return None;
    }
    let mut depth = 0i32;
    let mut in_str: Option<u8> = None;
    let mut i = open_index;
    while i < b.len() {
        let ch = b[i];
        if let Some(q) = in_str {
            if ch == b'\\' {
                i += 2;
                continue;
            }
            if ch == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            in_str = Some(ch);
            i += 1;
            continue;
        }
        if ch == b'(' {
            depth += 1;
        } else if ch == b')' {
            depth -= 1;
            if depth == 0 {
                return Some(ReadArgs { args: s[open_index + 1..i].to_string(), end: i + 1 });
            }
        }
        i += 1;
    }
    None
}

/// Advance past whitespace.
fn eat_ws(safe: &str, i: usize) -> usize {
    i + re!(r"^\s*").find(&safe[i..]).map(|m| m.end()).unwrap_or(0)
}

/// Skip stacked decorators (`@UseGuards(...)`, `@HttpCode(204)`, …) from `i`; `None` when one's
/// arguments never close.
fn skip_stacked_decorators(safe: &str, mut i: usize) -> Option<usize> {
    loop {
        i = eat_ws(safe, i);
        if safe.as_bytes().get(i) != Some(&b'@') {
            break;
        }
        let Some(m) = re!(r"^@[\w.]+").find(&safe[i..]) else { break };
        i += m.end();
        i = eat_ws(safe, i);
        if safe.as_bytes().get(i) == Some(&b'(') {
            i = read_args(safe, i)?.end;
        }
    }
    Some(i)
}

/// Starting just after a method decorator's `)`, return the name of the method it decorates.
/// Skips any further stacked decorators (`@UseGuards(...)`, `@HttpCode(204)`, …) and
/// access/async modifiers in between.
fn method_name_after(safe: &str, start: usize) -> Option<String> {
    let mut i = skip_stacked_decorators(safe, start)?;

    // Skip access/async/static modifiers.
    loop {
        i = eat_ws(safe, i);
        match re!(r"^(?:public|private|protected|async|static)\b").find(&safe[i..]) {
            Some(m) if m.end() > 0 => i += m.end(),
            _ => break,
        }
    }

    i = eat_ws(safe, i);
    re!(r"^([A-Za-z_$][\w$]*)\s*\(").captures(&safe[i..]).and_then(|m| m.text(1).map(String::from))
}

// ---------------------------------------------------------------------------
// Class scopes (controller / resolver / gateway boundaries)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClassKind {
    Controller,
    Resolver,
    Gateway,
    Other,
}

struct ClassScope {
    kind: ClassKind,
    /// HTTP prefix (controller) or WS namespace (gateway); '' otherwise.
    prefix: String,
    start: usize,
    end: usize,
}

/// Build the list of class-level decorator scopes, sorted by position. Each scope runs from its
/// decorator up to the next class decorator (of any kind), which lets a method decorator find its
/// enclosing class regardless of how many classes share a file.
fn build_class_scopes(safe: &str) -> Vec<ClassScope> {
    let defs: [(ClassKind, &str, fn(&str) -> String); 6] = [
        (ClassKind::Controller, "Controller", parse_controller_prefix),
        (ClassKind::Resolver, "Resolver", |_| String::new()),
        (ClassKind::Gateway, "WebSocketGateway", parse_gateway_namespace),
        (ClassKind::Other, "Injectable", |_| String::new()),
        (ClassKind::Other, "Module", |_| String::new()),
        (ClassKind::Other, "Catch", |_| String::new()),
    ];

    let mut raw: Vec<(ClassKind, String, usize)> = Vec::new();
    for (kind, name, prefix_of) in defs {
        for hit in find_decorators(safe, &[name]) {
            raw.push((kind, prefix_of(&hit.args), hit.index));
        }
    }
    raw.sort_by_key(|r| r.2);

    let ends: Vec<usize> = (0..raw.len()).map(|i| raw.get(i + 1).map(|n| n.2).unwrap_or(safe.len())).collect();
    raw.into_iter().zip(ends).map(|((kind, prefix, index), end)| ClassScope { kind, prefix, start: index, end }).collect()
}

fn scope_for(scopes: &[ClassScope], index: usize) -> Option<&ClassScope> {
    scopes.iter().find(|s| index >= s.start && index < s.end)
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

/// First string literal anywhere in the args, or '' (covers `'x'`, `{ k: 'x' }`).
fn parse_string_arg(args: &str) -> String {
    re!(r#"['"`]([^'"`]*)['"`]"#).captures(args).and_then(|m| m.text(1).map(String::from)).unwrap_or_default()
}

/// `@Controller('users')` | `@Controller({ path: 'users', host })` | `@Controller(['a','b'])` |
/// `@Controller()`.
fn parse_controller_prefix(args: &str) -> String {
    if let Some(obj) = re!(r#"path\s*:\s*['"`]([^'"`]*)['"`]"#).captures(args) {
        return obj.text(1).unwrap_or("").to_string();
    }
    parse_string_arg(args)
}

/// `@WebSocketGateway({ namespace: 'chat' })` | `@WebSocketGateway(81, { namespace: '/chat' })` |
/// `@WebSocketGateway()`.
fn parse_gateway_namespace(args: &str) -> String {
    re!(r#"namespace\s*:\s*['"`]([^'"`]*)['"`]"#).captures(args).and_then(|m| m.text(1).map(String::from)).unwrap_or_default()
}

/// GraphQL operation name. Prefers an explicit `{ name: 'x' }` or a leading string literal
/// (`@Query('users')`); otherwise the field name defaults to the handler method name. Avoids
/// mistaking a `description` string for the name.
fn parse_graphql_name(args: &str, handler: Option<&str>) -> String {
    if let Some(named) = re!(r#"name\s*:\s*['"`]([^'"`]*)['"`]"#).captures(args) {
        return named.text(1).unwrap_or("").to_string();
    }
    if let Some(lead) = re!(r#"^\s*['"`]([^'"`]*)['"`]"#).captures(args) {
        return lead.text(1).unwrap_or("").to_string();
    }
    handler.unwrap_or("").to_string()
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Join a controller prefix and method path into a single normalised `/path`.
fn join_http_path(prefix: &str, sub: &str) -> String {
    let parts: Vec<&str> = [prefix, sub].iter().map(|p| js_trim(p).trim_matches('/')).filter(|p| !p.is_empty()).collect();
    format!("/{}", parts.join("/"))
}

fn line_at(safe: &str, index: usize) -> u32 {
    safe[..index].matches('\n').count() as u32 + 1
}

// ---------------------------------------------------------------------------
// RouterModule + @Module walkers (used by post_extract above)
// ---------------------------------------------------------------------------

/// Walk every `RouterModule.register([...])` call (and the equivalent `RouterModule.forRoot([...])`
/// and `forChild([...])` aliases) and populate `out` with `Module → /full/prefix`. Recursive
/// `children` arrays inherit their parent's prefix.
///
/// First-write-wins: if the same module appears in two registrations we keep the first prefix seen
/// rather than overwriting. NestJS itself does the same.
fn collect_router_module_registrations(safe: &str, out: &mut IndexMap<String, String>) {
    let registration = re!(r"\bRouterModule\s*\.\s*(?:register|forRoot|forChild)\s*\(");
    let mut pos = 0;
    while let Some(m) = registration.captures_at(safe, pos) {
        let Some(whole) = m.get(0) else { break };
        let open_index = whole.end() - 1;
        pos = whole.end();
        let Some(parsed) = read_args(safe, open_index) else { continue };
        let items = parse_routes_array(&parsed.args);
        walk_routes_tree(&items, "", out);
        pos = parsed.end;
    }
}

struct RouteItem {
    path: String,
    module_name: Option<String>,
    children: Vec<RouteItem>,
}

/// Parse a `[ {...}, {...} ]` argument list into a list of `RouteItem`s. The args are expected to
/// be an inline literal — references to a `const routes: Routes = [...]` declared earlier in the
/// file aren't followed (rare in practice; the registration is usually inline).
fn parse_routes_array(args: &str) -> Vec<RouteItem> {
    let trimmed = js_trim(args);
    if !trimmed.starts_with('[') {
        return Vec::new();
    }
    // Strip outer [ ... ] respecting balanced brackets.
    let Some(close) = matching_close(trimmed, 0) else { return Vec::new() };
    parse_route_objects(&trimmed[1..close])
}

fn parse_route_objects(s: &str) -> Vec<RouteItem> {
    split_top_level_objects(s)
        .into_iter()
        .map(|obj| {
            let path = parse_string_field(&obj, "path");
            let module_name = parse_ident_field(&obj, "module");
            let children = parse_array_field(&obj, "children").map(|c| parse_route_objects(&c)).unwrap_or_default();
            RouteItem { path, module_name, children }
        })
        .collect()
}

fn walk_routes_tree(items: &[RouteItem], parent_prefix: &str, out: &mut IndexMap<String, String>) {
    for item in items {
        let my_prefix = join_http_path(parent_prefix, &item.path);
        if let Some(module_name) = &item.module_name {
            if !out.contains_key(module_name) {
                out.insert(module_name.clone(), my_prefix.clone());
            }
        }
        if !item.children.is_empty() {
            walk_routes_tree(&item.children, &my_prefix, out);
        }
    }
}

/// Walk every `@Module(...)` decorator and populate `out` with `Controller →
/// enclosingModuleClassName`, based on the decorator's `controllers: [...]` field and the class
/// declaration that follows the decorator (skipping stacked decorators and export/default/abstract
/// modifiers).
fn collect_module_controllers(safe: &str, out: &mut IndexMap<String, String>) {
    for hit in find_decorators(safe, &["Module"]) {
        let Some(class_name) = class_name_after(safe, hit.end) else { continue };
        for controller in parse_controllers_field(&hit.args) {
            // First-write-wins, same as RouterModule, so a controller listed in two modules picks
            // up the one declared earliest in source.
            if !out.contains_key(&controller) {
                out.insert(controller, class_name.clone());
            }
        }
    }
}

fn parse_controllers_field(args: &str) -> Vec<String> {
    let Some(inner) = parse_array_field(args, "controllers") else { return Vec::new() };
    inner.split(',').map(|s| js_trim(s).to_string()).filter(|s| re!(r"^[A-Za-z_$][\w$]*$").is_match(s)).collect()
}

/// Starting just after a class decorator's `)`, return the name of the class it decorates.
/// Mirrors `method_name_after` for methods: skips stacked decorators and
/// `export`/`default`/`abstract` modifiers.
fn class_name_after(safe: &str, start: usize) -> Option<String> {
    let i = skip_stacked_decorators(safe, start)?;
    let i = eat_ws(safe, i);
    re!(r"^(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)")
        .captures(&safe[i..])
        .and_then(|m| m.text(1).map(String::from))
}

// ---------------------------------------------------------------------------
// Small string utilities (object/array literal splitters)
// ---------------------------------------------------------------------------

/// A list's comma-separated items at depth 0, strings and brackets stepped over.
fn split_top_level(list: &str) -> Vec<String> {
    let b = list.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut depth = 0i32;
    let mut in_str: Option<u8> = None;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        let ch = b[i];
        if let Some(q) = in_str {
            if ch == b'\\' {
                i += 2;
                continue;
            }
            if ch == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            in_str = Some(ch);
            i += 1;
            continue;
        }
        if matches!(ch, b'[' | b'{' | b'(') {
            depth += 1;
        } else if matches!(ch, b']' | b'}' | b')') {
            depth -= 1;
        } else if ch == b',' && depth == 0 {
            out.push(sub(list, start, i).to_string());
            start = i + 1;
        }
        i += 1;
    }
    out.push(list.get(start..).unwrap_or("").to_string());
    out.into_iter().filter(|item| !js_trim(item).is_empty()).collect()
}

/// Return the index of the bracket that closes the one at `open`.
fn matching_close(s: &str, open: usize) -> Option<usize> {
    let b = s.as_bytes();
    if !matches!(b.get(open), Some(b'[' | b'{' | b'(')) {
        return None;
    }
    let mut depth = 0i32;
    let mut in_str: Option<u8> = None;
    let mut i = open;
    while i < b.len() {
        let ch = b[i];
        if let Some(q) = in_str {
            if ch == b'\\' {
                i += 2;
                continue;
            }
            if ch == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            in_str = Some(ch);
            i += 1;
            continue;
        }
        if matches!(ch, b'{' | b'[' | b'(') {
            depth += 1;
        } else if matches!(ch, b'}' | b']' | b')') {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Split `s` into the contents of each top-level object literal. Brackets and string literals are
/// balanced so nested arrays/objects/strings inside an object don't cause an early split.
fn split_top_level_objects(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut depth = 0i32;
    let mut obj_start: Option<usize> = None;
    let mut in_str: Option<u8> = None;
    let mut i = 0usize;
    while i < b.len() {
        let ch = b[i];
        if let Some(q) = in_str {
            if ch == b'\\' {
                i += 2;
                continue;
            }
            if ch == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            in_str = Some(ch);
            i += 1;
            continue;
        }
        if depth == 0 && ch == b'{' {
            depth = 1;
            obj_start = Some(i);
            i += 1;
            continue;
        }
        if matches!(ch, b'{' | b'[' | b'(') {
            depth += 1;
        } else if matches!(ch, b'}' | b']' | b')') {
            depth -= 1;
            if depth == 0 && ch == b'}' {
                if let Some(start) = obj_start.take() {
                    out.push(sub(s, start + 1, i).to_string());
                }
            }
        }
        i += 1;
    }
    out
}

/// Read a string-valued field — `key: 'value'` — out of one object literal's body. Returns `''`
/// if not present. The leading character class guards against matching a field whose name
/// *contains* the target as a suffix.
fn parse_string_field(obj: &str, name: &str) -> String {
    let field = Re::cached(&format!(r#"(?:^|[,{{\s]){name}\s*:\s*['"`]([^'"`]*)['"`]"#), "");
    let found = field.captures(obj).and_then(|m| m.text(1).map(String::from));
    found.unwrap_or_default()
}

/// Read an identifier-valued field — `key: SomeIdent` — out of one object body.
fn parse_ident_field(obj: &str, name: &str) -> Option<String> {
    let field = Re::cached(&format!(r"(?:^|[,{{\s]){name}\s*:\s*([A-Za-z_$][\w$]*)"), "");
    let found = field.captures(obj).and_then(|m| m.text(1).map(String::from));
    found
}

/// Read an array-valued field — `key: [ ... ]` — as the raw inner text.
fn parse_array_field(obj: &str, name: &str) -> Option<String> {
    let field = Re::cached(&format!(r"(?:^|[,{{\s]){name}\s*:\s*\["), "");
    let m = field.find(obj)?;
    let open = m.end() - 1;
    let close = matching_close(obj, open)?;
    Some(obj[open + 1..close].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(src: &str) -> FrameworkExtractionResult {
        NestjsResolver.extract("src/cats/cats.controller.ts", src).unwrap()
    }

    #[test]
    fn controller_prefix_joins_method_paths() {
        let src = "@Controller('cats')\nexport class CatsController {\n  @Get()\n  findAll() {}\n\n  @Get(':id')\n  @UseGuards(AuthGuard)\n  findOne() {}\n\n  @Post()\n  async create() {}\n}\n";
        let result = extract(src);
        let names: Vec<&str> = result.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["GET /cats", "GET /cats/:id", "POST /cats"]);
        let refs: Vec<&str> = result.references.iter().map(|r| r.reference_name.as_str()).collect();
        assert_eq!(refs, vec!["findAll", "findOne", "create"]);
    }

    #[test]
    fn graphql_only_inside_a_resolver() {
        let src = "@Resolver()\nexport class R {\n  @Query(() => [User])\n  users() {}\n}\n@Controller()\nexport class C {\n  @Get()\n  list(@Query() q) {}\n}\n";
        let result = extract(src);
        let names: Vec<&str> = result.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["GET /", "QUERY users"]);
    }

    #[test]
    fn websocket_and_microservice_handlers() {
        let src = "@WebSocketGateway({ namespace: 'chat' })\nexport class G {\n  @SubscribeMessage('events')\n  onEvent() {}\n  @MessagePattern('sum')\n  sum() {}\n  @EventPattern('user_created')\n  created() {}\n}\n";
        let result = extract(src);
        let names: Vec<&str> = result.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["MESSAGE sum", "EVENT user_created", "WS chat:events"]);
    }

    #[test]
    fn router_module_tree_is_flattened() {
        let src = "RouterModule.register([{ path: 'admin', module: AdminModule, children: [{ path: 'users', module: UsersModule }] }])";
        let mut out = IndexMap::new();
        collect_router_module_registrations(src, &mut out);
        assert_eq!(out.get("AdminModule").map(String::as_str), Some("/admin"));
        assert_eq!(out.get("UsersModule").map(String::as_str), Some("/admin/users"));
    }

    #[test]
    fn bootstrap_prefix_exclusions_and_versions() {
        let config = read_bootstrap("app.setGlobalPrefix('/api/', { exclude: ['health', { path: 'metrics', method: 0 }] });\napp.enableVersioning({ type: VersioningType.URI, defaultVersion: '1' });");
        assert_eq!(config.prefix, "api");
        assert_eq!(config.excludes, vec!["health", "metrics"]);
        assert!(config.uri);
        assert_eq!(config.default_version.as_deref(), Some("1"));
        assert!(is_excluded("/health", &config.excludes));
        assert!(!is_excluded("/cats", &config.excludes));
    }
}
