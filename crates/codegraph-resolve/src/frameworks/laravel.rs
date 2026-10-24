//! Laravel framework resolver. Port of `src/resolution/frameworks/laravel.ts`.

use super::now_ms;
use super::posix;
use crate::ctx::Ctx;
use crate::types::{FrameworkExtractionResult, FrameworkResolver, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::jscompat::{js_trim, utf16_len, utf16_prefix};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use indexmap::{IndexMap, IndexSet};

pub struct LaravelResolver;

const LANGUAGES: [Language; 1] = [Language::Php];

/// Laravel facade mappings to underlying classes. Exported for potential use in facade
/// resolution.
pub const FACADE_MAPPINGS: [(&str, &str); 20] = [
    ("Auth", "Illuminate\\Auth\\AuthManager"),
    ("Cache", "Illuminate\\Cache\\CacheManager"),
    ("Config", "Illuminate\\Config\\Repository"),
    ("DB", "Illuminate\\Database\\DatabaseManager"),
    ("Event", "Illuminate\\Events\\Dispatcher"),
    ("File", "Illuminate\\Filesystem\\Filesystem"),
    ("Gate", "Illuminate\\Auth\\Access\\Gate"),
    ("Hash", "Illuminate\\Hashing\\HashManager"),
    ("Log", "Illuminate\\Log\\LogManager"),
    ("Mail", "Illuminate\\Mail\\Mailer"),
    ("Queue", "Illuminate\\Queue\\QueueManager"),
    ("Redis", "Illuminate\\Redis\\RedisManager"),
    ("Request", "Illuminate\\Http\\Request"),
    ("Response", "Illuminate\\Http\\Response"),
    ("Route", "Illuminate\\Routing\\Router"),
    ("Session", "Illuminate\\Session\\SessionManager"),
    ("Storage", "Illuminate\\Filesystem\\FilesystemManager"),
    ("URL", "Illuminate\\Routing\\UrlGenerator"),
    ("Validator", "Illuminate\\Validation\\Factory"),
    ("View", "Illuminate\\View\\Factory"),
];

const ROUTE_QN: &str = "::route:";
const DEFAULT_API_FILE: &str = "routes/api.php";
/// What a file outside `routes/` must contain to mount a routes file.
const MOUNT_NEEDLES: [&str; 2] = ["routes/", "withRouting("];

/// A route's `Controller@action`: a class name, or a namespace path under the controllers root
/// (`Common\Uploads`), or a fully qualified one (`Modules\OfflinePayments\Http\Controllers\Settings`).
fn controller_action_re() -> &'static codegraph_jsre::Re {
    re!(r"^([A-Za-z_][\w\\]*)@(\w+)$")
}

/// A class written with its namespace path (`Common\Companies`).
fn namespaced_class_re() -> &'static codegraph_jsre::Re {
    re!(r"^[A-Za-z_]\w*(?:\\\w+)+$")
}

impl FrameworkResolver for LaravelResolver {
    fn name(&self) -> &str {
        "laravel"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        // Check for artisan file (Laravel signature)
        ctx.file_exists("artisan") || ctx.file_exists("app/Http/Kernel.php")
    }

    // `Controller@method` route refs name no declared symbol, so resolveOne's pre-filter would
    // drop them before resolve() runs (Pattern 4). Claim them — same hook the django ORM / Rails
    // routing work needed.
    fn claims_reference(&self, name: &str) -> bool {
        controller_action_re().is_match(name) || namespaced_class_re().is_match(name)
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        let name = r.reference_name.as_str();

        // Pattern 1: Model::method() - Eloquent static calls
        if let Some(model_match) = re!(r"^([A-Z][a-zA-Z]+)::(\w+)$").captures(name) {
            let class_name = model_match.text(1).unwrap_or("");
            let method_name = model_match.text(2).unwrap_or("");
            if let Some(result) = resolve_model_call(class_name, method_name, ctx) {
                return Some(ResolvedRef::new(r, result, 0.85, ResolvedBy::Framework));
            }
        }

        // Pattern 2: Facade calls - Auth::user(), Cache::get(). Facades typically resolve to
        // external Laravel code: mark as external but note the facade.
        if re!(r"^(Auth|Cache|DB|Log|Mail|Queue|Session|Storage|Validator|Route|Request|Response)::(\w+)$").is_match(name) {
            return None; // External, can't resolve to local node
        }

        // Pattern 3: Helper function calls - route(), view(), config(). These are Laravel
        // helpers - external.
        if ["route", "view", "config", "env", "app", "abort", "redirect", "response", "request", "session", "url", "asset", "mix"].contains(&name) {
            return None;
        }

        // Pattern 5: a resource route's controller, named by string or `::class`.
        if r.is_kind(EdgeKind::Imports) && r.from_node_id.contains(":RESOURCE:") {
            if let Some(result) = resolve_controller_class(name, ctx) {
                return Some(ResolvedRef::new(r, result, 0.9, ResolvedBy::Framework));
            }
        }

        // Pattern 4: Controller method references
        if let Some(controller_match) = controller_action_re().captures(name) {
            let controller = controller_match.text(1).unwrap_or("");
            let method = controller_match.text(2).unwrap_or("");
            if let Some(result) = resolve_controller_method(controller, method, ctx) {
                return Some(ResolvedRef::new(r, result, 0.9, ResolvedBy::Framework));
            }
        }

        None
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<FrameworkExtractionResult> {
        if !file_path.ends_with(".php") {
            return Some(FrameworkExtractionResult::default());
        }
        let mut nodes: Vec<Node> = Vec::new();
        let mut references: Vec<UnresolvedRef> = Vec::new();
        let now = now_ms();
        let safe = codegraph_text::strip_comments_for_regex(content, "php");
        let safe: &str = &safe;
        // `Route::prefix('admin')->group(…)` / `Route::group(['prefix' => …], …)` scopes, so a
        // route is named by the path a request takes inside this file. A file mounted under a
        // prefix elsewhere (`routes/api.php` under `/api`) is composed in postExtract, from this
        // in-file path.
        let groups = route_groups(safe);
        let line_of = |index: usize| safe[..index].matches('\n').count() as u32 + 1;

        // Route::METHOD('/path', handler-expr)
        // handler-expr can be: [Class::class, 'method'] | 'Controller@method' | Closure | Class::class
        let route_regex = re!(r#"Route::(get|post|put|patch|delete|options|any)\s*\(\s*['"]([^'"]+)['"]\s*,\s*([^)]+)\)"#, "g");
        for m in route_regex.captures_iter(safe) {
            let Some(whole) = m.get(0) else { continue };
            let route_path = m.text(2).unwrap_or("");
            let handler_expr = m.text(3).unwrap_or("");
            let line = line_of(whole.start());
            let upper = m.text(1).unwrap_or("").to_uppercase();
            let in_file = join_route_path(&[&prefix_at(&groups, whole.start())[..], route_path]);
            let node = Node::new(
                format!("route:{file_path}:{line}:{upper}:{route_path}"),
                NodeKind::Route,
                format!("{upper} {in_file}"),
                format!("{file_path}::route:{in_file}"),
                file_path,
                Language::Php,
                line,
                line,
                0,
                utf16_len(whole.as_str()) as u32,
                now,
            );
            let route_id = node.id.clone();
            nodes.push(node);

            if let Some(handler_name) = extract_laravel_handler(handler_expr) {
                references.push(UnresolvedRef::new(route_id, handler_name, EdgeKind::References, line, 0, file_path, Language::Php));
            }
        }

        // Route::resource('name', Controller::class) / Route::apiResource('name', Controller::class)
        let resource_regex = re!(r#"Route::(resource|apiResource)\s*\(\s*['"]([^'"]+)['"]\s*(?:,\s*([^)]+))?\)"#, "g");
        for m in resource_regex.captures_iter(safe) {
            let Some(whole) = m.get(0) else { continue };
            let resource_name = m.text(2).unwrap_or("");
            let line = line_of(whole.start());
            let in_file = join_route_path(&[&prefix_at(&groups, whole.start())[..], resource_name]);
            let node = Node::new(
                format!("route:{file_path}:{line}:RESOURCE:{resource_name}"),
                NodeKind::Route,
                format!("resource:{}", &in_file[1..]),
                format!("{file_path}::route:{in_file}"),
                file_path,
                Language::Php,
                line,
                line,
                0,
                utf16_len(whole.as_str()) as u32,
                now,
            );
            let route_id = node.id.clone();
            nodes.push(node);

            if let Some(handler_expr) = m.text(3).filter(|h| !h.is_empty()) {
                // The controller is the first argument; an options array may follow.
                let split = split_args(handler_expr);
                let first = split.first().map(String::as_str).unwrap_or(handler_expr);
                if let Some(controller_name) = extract_laravel_handler(first) {
                    references.push(UnresolvedRef::new(route_id, controller_name, EdgeKind::Imports, line, 0, file_path, Language::Php));
                }
            }
        }

        Some(FrameworkExtractionResult { nodes, references })
    }

    /// Name each route by the path a request takes once its FILE is mounted. Laravel serves
    /// `routes/api.php` under `/api` (the default RouteServiceProvider and Laravel 11's
    /// `withRouting(api:)` both do), and any routes file can be mounted under a prefix:
    /// `Route::prefix('v1')->group(base_path('routes/v1.php'))`, `withRouting(api: …, apiPrefix:)`,
    /// or a `require` inside a prefixed group. Mounts compose down the include tree; a file reached
    /// at two different prefixes is left at its in-file path, since one name cannot be two
    /// addresses.
    ///
    /// Every Laravel route is recomputed from its `qualifiedName` (the in-file path, group
    /// prefixes included), so the pass is idempotent and a mount that goes away is undone on the
    /// next run.
    fn post_extract(&self, ctx: &Ctx) -> Option<Vec<Node>> {
        let routes: Vec<_> = ctx.get_nodes_by_kind(NodeKind::Route).iter().filter(|n| is_laravel_route(n)).cloned().collect();
        if routes.is_empty() {
            return Some(Vec::new());
        }

        let mut mounts: Vec<Mount> = Vec::new();
        let mut custom_loader = false;
        for file in ctx.get_all_files() {
            if !file.ends_with(".php") {
                continue;
            }
            // A mount names a `routes/` path (base_path('routes/api.php'),
            // __DIR__.'/../routes/api.php'), is a routes file including a sibling (`require
            // __DIR__.'/auth.php'`), or is Laravel 11's `withRouting(`, whose `using:` loader may
            // name no path at all. Everything else is skipped unread.
            let in_routes = file.starts_with("routes/");
            if !in_routes && !MOUNT_NEEDLES.iter().any(|needle| ctx.file_contains(&file, needle)) {
                continue;
            }
            let Some(content) = ctx.read_file(&file).filter(|c| !c.is_empty()) else { continue };
            if !in_routes && !MOUNT_NEEDLES.iter().any(|needle| content.contains(needle)) {
                continue;
            }
            let safe = codegraph_text::strip_comments_for_regex(&content, "php");
            let found = route_file_mounts(&safe, &file, ctx);
            mounts.extend(found.mounts);
            if found.custom_loader {
                custom_loader = true;
            }
        }

        // Nothing mounts routes/api.php, and no custom loader might: Laravel's own default is
        // `/api`. Seeded before composing, so a file api.php includes inherits it.
        let mut defaults: IndexMap<String, String> = IndexMap::new();
        let mounted: IndexSet<&str> = mounts.iter().map(|m| m.target.as_str()).collect();
        if !custom_loader && !mounted.contains(DEFAULT_API_FILE) && ctx.file_exists(DEFAULT_API_FILE) {
            defaults.insert(DEFAULT_API_FILE.to_string(), "/api".to_string());
        }

        // Compose prefixes down the include tree until nothing changes.
        let mut prefix_of = defaults.clone();
        for _round in 0..8 {
            let mut next = defaults.clone();
            let mut ambiguous: IndexSet<String> = IndexSet::new();
            for Mount { source, target, prefix } in &mounts {
                if target == source {
                    continue;
                }
                let full = join_route_path(&[prefix_of.get(source).map(String::as_str).unwrap_or(""), prefix]);
                match next.get(target) {
                    Some(seen) if *seen != full => {
                        ambiguous.insert(target.clone());
                    }
                    _ => {
                        next.insert(target.clone(), full);
                    }
                }
            }
            for a in &ambiguous {
                next.shift_remove(a);
            }
            let mut changed = next.len() != prefix_of.len();
            if !changed {
                changed = next.iter().any(|(k, v)| prefix_of.get(k) != Some(v));
            }
            prefix_of = next;
            if !changed {
                break;
            }
        }
        let mut updates: Vec<Node> = Vec::new();
        for route in &routes {
            let at = route.qualified_name.find(ROUTE_QN);
            let in_file = match at {
                Some(at) => &route.qualified_name[at + ROUTE_QN.len()..],
                // JS: indexOf → -1, so slice(-1 + len) — never reached for a route this resolver
                // made (see `is_laravel_route`).
                None => "",
            };
            let full = join_route_path(&[prefix_of.get(&route.file_path).map(String::as_str).unwrap_or(""), in_file]);
            let name = if route.name.starts_with("resource:") {
                format!("resource:{}", &full[1..])
            } else {
                let space = route.name.find(' ').unwrap_or(route.name.len().saturating_sub(1));
                format!("{} {}", &route.name[..space], full)
            };
            if name != route.name {
                let mut updated = (**route).clone();
                updated.name = name;
                updates.push(updated);
            }
        }
        Some(updates)
    }
}

struct Mount {
    source: String,
    target: String,
    prefix: String,
}

/// A route this resolver made: its `qualifiedName` holds the in-file path.
fn is_laravel_route(n: &Node) -> bool {
    n.language == Language::Php && n.id.starts_with(&format!("route:{}:", n.file_path)) && n.qualified_name.starts_with(&format!("{}{ROUTE_QN}/", n.file_path))
}

/// A group's parenthesised arguments and the prefix it adds (`{prefix}` when not a literal).
struct RouteGroup {
    open: usize,
    close: usize,
    prefix: String,
}

/// Every route group in a file: `Route::group([...], …)` and the `->group(…)` that ends a
/// `Route::…` chain (`Route::middleware('auth')->prefix('admin')->group(…)`), with the prefix the
/// chain or the array names. A prefix that is not a string literal
/// (`LaravelLocalization::setLocale()`) is kept as the `{prefix}` segment it stands for, not
/// dropped. Each group's split arguments ride along for the mount reader.
fn route_groups(safe: &str) -> Vec<RouteGroup> {
    route_groups_with_args(safe).into_iter().map(|(g, _)| g).collect()
}

fn route_groups_with_args(safe: &str) -> Vec<(RouteGroup, Vec<String>)> {
    let mut groups: Vec<(RouteGroup, Vec<String>)> = Vec::new();
    let start = re!(r"\bRoute::(\w+)\s*\(", "g");
    for m in start.captures_iter(safe) {
        let Some(whole) = m.get(0) else { continue };
        let mut name = m.text(1).unwrap_or("").to_string();
        let mut open = whole.end() - 1;
        let mut prefixes: Vec<String> = Vec::new();
        loop {
            let Some(close) = match_paren(safe, open) else { break };
            let args = split_args(&safe[open + 1..close]);
            if name == "prefix" {
                prefixes.push(literal_or_placeholder(args.first().map(String::as_str).unwrap_or("")));
            }
            if name == "group" {
                // Route::group(['prefix' => 'admin', …], …) carries it in the array.
                if prefixes.is_empty() && re!(r"^\s*\[").is_match(args.first().map(String::as_str).unwrap_or("")) {
                    if let Some(in_array) = re!(r#"['"]prefix['"]\s*=>\s*([^,\]]+)"#).captures(&args[0]) {
                        prefixes.push(literal_or_placeholder(in_array.text(1).unwrap_or("")));
                    }
                }
                let parts: Vec<&str> = prefixes.iter().map(String::as_str).collect();
                groups.push((RouteGroup { open, close, prefix: join_route_path(&parts) }, args));
                break;
            }
            let window = utf16_prefix(&safe[close + 1..], 200);
            let Some(next) = re!(r"^\s*->\s*(\w+)\s*\(").captures(window) else { break };
            name = next.text(1).unwrap_or("").to_string();
            open = close + 1 + next.get(0).map(|w| w.end()).unwrap_or(1) - 1;
        }
    }
    groups
}

/// The composed prefix of every group whose arguments contain `at`, outermost first.
fn prefix_at(groups: &[RouteGroup], at: usize) -> String {
    let mut enclosing: Vec<&RouteGroup> = groups.iter().filter(|g| g.open < at && at < g.close).collect();
    enclosing.sort_by_key(|g| g.open);
    let parts: Vec<&str> = enclosing.iter().map(|g| g.prefix.as_str()).collect();
    join_route_path(&parts)
}

struct FileMounts {
    mounts: Vec<Mount>,
    custom_loader: bool,
}

/// The routes files this file mounts, each with the prefix a request carries into it: a group
/// whose argument is the file, a `require` / `include` / `loadRoutesFrom` inside a group, and
/// Laravel 11's `withRouting(api:, web:)`. `custom_loader` is set when `withRouting(using:)` loads
/// routes by code this pass cannot read.
fn route_file_mounts(safe: &str, file: &str, ctx: &Ctx) -> FileMounts {
    let mut mounts: Vec<Mount> = Vec::new();
    let with_args = route_groups_with_args(safe);
    let groups: Vec<RouteGroup> = with_args.iter().map(|(g, _)| RouteGroup { open: g.open, close: g.close, prefix: g.prefix.clone() }).collect();
    let add = |expr: &str, prefix: &str, mounts: &mut Vec<Mount>| {
        if let Some(target) = resolve_php_path(expr, file, ctx) {
            mounts.push(Mount { source: file.to_string(), target, prefix: prefix.to_string() });
        }
    };

    for (g, args) in &with_args {
        // ->group(base_path('routes/v1.php')), Route::group([...], base_path(…))
        let body = args.last().map(String::as_str).unwrap_or("");
        if !re!(r"^\s*(?:static\s+)?(?:function|fn)\b").is_match(body) {
            add(body, &prefix_at(&groups, g.open + 1), &mut mounts);
        }
    }
    let load = re!(r"\b(?:require|include)(?:_once)?\b([^;]*);|\bloadRoutesFrom\s*\(([^;]*)\)\s*;", "g");
    for m in load.captures_iter(safe) {
        let Some(whole) = m.get(0) else { continue };
        let mut expr = js_trim(m.text(1).or(m.text(2)).unwrap_or("")).to_string();
        // `require(__DIR__.'/x.php')` — drop parens only when they wrap the whole expression.
        if expr.starts_with('(') && match_paren(&expr, 0) == Some(expr.len() - 1) {
            expr = expr[1..expr.len() - 1].to_string();
        }
        add(&expr, &prefix_at(&groups, whole.start()), &mut mounts);
    }

    let mut custom_loader = false;
    let routing = re!(r"\bwithRouting\s*\(", "g");
    for m in routing.captures_iter(safe) {
        let Some(whole) = m.get(0) else { continue };
        let open = whole.end() - 1;
        let Some(close) = match_paren(safe, open) else { continue };
        let mut named: IndexMap<String, String> = IndexMap::new();
        for arg in split_args(&safe[open + 1..close]) {
            if let Some(kv) = re!(r"^\s*(\w+)\s*:\s*([\s\S]*)$").captures(&arg) {
                named.insert(kv.text(1).unwrap_or("").to_string(), kv.text(2).unwrap_or("").to_string());
            }
        }
        if named.contains_key("using") {
            custom_loader = true;
        }
        let api_prefix = match named.get("apiPrefix") {
            Some(value) => literal_or_placeholder(value),
            None => "api".to_string(),
        };
        for (key, prefix) in [("api", api_prefix.as_str()), ("web", "")] {
            let Some(value) = named.get(key).filter(|v| !v.is_empty()) else { continue };
            let items: Vec<String> = if re!(r"^\s*\[").is_match(value) {
                let t = js_trim(value);
                split_args(t.get(1..t.len().saturating_sub(1)).unwrap_or(""))
            } else {
                vec![value.clone()]
            };
            for item in items {
                add(&item, prefix, &mut mounts);
            }
        }
    }
    FileMounts { mounts, custom_loader }
}

/// A PHP path expression naming a file in the project: `base_path('routes/x.php')`,
/// `__DIR__.'/../routes/x.php'`, `dirname(__DIR__).'/routes/x.php'` or a bare `'routes/x.php'`
/// literal. `None` for anything computed, or not in the index.
fn resolve_php_path(expr: &str, file: &str, ctx: &Ctx) -> Option<String> {
    let e = js_trim(expr);
    let rel: String = if let Some(m) = re!(r#"^base_path\s*\(\s*(['"])([^'"]+)\1\s*\)$"#).captures(e) {
        m.text(2)?.to_string()
    } else if let Some(m) = re!(r#"^__DIR__\s*\.\s*(['"])([^'"]+)\1$"#).captures(e) {
        posix::join(&[&posix::dirname(file), m.text(2)?])
    } else if let Some(m) = re!(r#"^dirname\s*\(\s*__DIR__\s*\)\s*\.\s*(['"])([^'"]+)\1$"#).captures(e) {
        posix::join(&[&posix::dirname(&posix::dirname(file)), m.text(2)?])
    } else if let Some(m) = re!(r#"^(['"])([^'"]+\.php)\1$"#).captures(e) {
        m.text(2)?.to_string()
    } else {
        return None;
    };
    if rel.is_empty() {
        return None;
    }
    let normalized = posix::normalize(&rel);
    let target = re!(r"^/+").replace_first(&re!(r"^(\./)+").replace_first(&normalized, |_| String::new()), |_| String::new());
    if target.starts_with("..") || !target.ends_with(".php") {
        return None;
    }
    if ctx.file_exists(&target) {
        Some(target)
    } else {
        None
    }
}

/// `'admin'` → `admin`; anything that is not a plain string literal → `{prefix}`.
fn literal_or_placeholder(expr: &str) -> String {
    match re!(r#"^\s*(['"])([^'"]*)\1\s*$"#).captures(expr) {
        Some(m) => m.text(2).unwrap_or("").to_string(),
        None => "{prefix}".to_string(),
    }
}

/// Join route path segments the way Laravel does: `('api', 'v1/', '/users')` → `/api/v1/users`.
/// Leading and trailing slashes don't matter; the root is `/`.
fn join_route_path(parts: &[&str]) -> String {
    let segments: Vec<&str> = parts.iter().flat_map(|p| p.split('/')).filter(|s| !s.is_empty()).collect();
    format!("/{}", segments.join("/"))
}

/// Index of the `)` closing the `(` at `open`, skipping PHP strings; `None` if unbalanced.
fn match_paren(s: &str, open: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut i = open;
    while i < b.len() {
        let ch = b[i];
        if ch == b'"' || ch == b'\'' {
            i += 1;
            while i < b.len() && b[i] != ch {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if ch == b'(' {
            depth += 1;
        } else if ch == b')' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Top-level comma split of an argument list; strings and brackets respected.
fn split_args(args: &str) -> Vec<String> {
    let b = args.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut depth = 0i32;
    let mut from = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        let ch = b[i];
        if ch == b'"' || ch == b'\'' {
            i += 1;
            while i < b.len() && b[i] != ch {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if matches!(ch, b'(' | b'[' | b'{') {
            depth += 1;
        } else if matches!(ch, b')' | b']' | b'}') {
            depth -= 1;
        } else if ch == b',' && depth == 0 {
            out.push(args.get(from..i).unwrap_or("").to_string());
            from = i + 1;
        }
        i += 1;
    }
    let last = args.get(from..).unwrap_or("");
    if !js_trim(last).is_empty() || !out.is_empty() {
        out.push(last.to_string());
    }
    out
}

/// Parse a Laravel route handler expression and return the symbol to link.
///  - `[Class::class, 'method']`  -> `Class@method`
///  - `'Controller@method'`       -> `Controller@method`
///  - `Class::class`              -> `Class`
///  - anything else (closure etc) -> `None`
fn extract_laravel_handler(expr: &str) -> Option<String> {
    let trimmed = js_trim(expr);
    let short = |s: &str| s.rsplit('\\').next().unwrap_or("").to_string(); // strip namespace

    // [Class::class, 'method'] → `Class@method` (PRECISE — keep the controller, so common action
    // names like `index`/`show` resolve to the RIGHT controller, not whichever one name-matching
    // happens to pick first).
    if let Some(tuple) = re!(r#"^\[\s*([A-Za-z_\\][\w\\]*)::class\s*,\s*['"]([^'"]+)['"]\s*\]"#).captures(trimmed) {
        return Some(format!("{}@{}", short(tuple.text(1)?), tuple.text(2)?));
    }

    // 'Controller@method' → `Controller@method`, keeping the namespace below the controllers
    // root: akaunting writes 'Common\Uploads@inline' for App\Http\Controllers\Common\Uploads, and
    // a `Portal\Uploads` beside it is a different controller.
    if let Some(at_match) = re!(r#"^['"]([^'"@]+)@([^'"]+)['"]$"#).captures(trimmed) {
        return Some(format!("{}@{}", controller_path(at_match.text(1)?), at_match.text(2)?));
    }

    // Class::class (Route::resource controller) → `Class`
    if let Some(class_match) = re!(r"^([A-Za-z_\\][\w\\]*)::class").captures(trimmed) {
        return Some(short(class_match.text(1)?));
    }

    // A controller named by string — no `use` brings it in, so its namespace path below the
    // controllers root is what finds it.
    if let Some(string_class) = re!(r#"^['"]([A-Za-z_\\][\w\\]*)['"]$"#).captures(trimmed) {
        return Some(controller_path(string_class.text(1)?));
    }

    None
}

/// A string handler's class as a path under the controllers root, a leading `\` and
/// `App\Http\Controllers\` off.
fn controller_path(written: &str) -> String {
    let no_lead = written.trim_start_matches('\\');
    re!(r"^App\\Http\\Controllers\\", "i").replace_first(no_lead, |_| String::new())
}

/// Resolve a Model::method() call.
fn resolve_model_call(class_name: &str, method_name: &str, ctx: &Ctx) -> Option<String> {
    // Try app/Models/ first (Laravel 8+), then app/ (Laravel 7 and below)
    for model_path in [format!("app/Models/{class_name}.php"), format!("app/{class_name}.php")] {
        if ctx.file_exists(&model_path) {
            let nodes = ctx.get_nodes_in_file(&model_path);
            // Look for the method in this class
            if let Some(method_node) = nodes.iter().find(|n| n.kind == NodeKind::Method && n.name == method_name) {
                return Some(method_node.id.clone());
            }
            // Return the class itself if method not found
            if let Some(class_node) = nodes.iter().find(|n| n.kind == NodeKind::Class && n.name == class_name) {
                return Some(class_node.id.clone());
            }
        }
    }
    None
}

/// The controller class a resource route names: by its path under the controllers root, else by
/// name.
fn resolve_controller_class(controller: &str, ctx: &Ctx) -> Option<String> {
    let rel = controller.replace('\\', "/");
    let class_name = rel.rsplit('/').next().unwrap_or("");
    let by_name = ctx.get_nodes_by_name(class_name);
    let classes: Vec<_> = by_name.iter().filter(|n| n.kind == NodeKind::Class && n.file_path.contains("Controllers")).collect();
    let suffix = format!("/{}.php", rel.to_lowercase());
    if let Some(exact) = classes.iter().find(|n| format!("/{}", n.file_path.to_lowercase()).ends_with(&suffix)) {
        return Some(exact.id.clone());
    }
    if classes.len() == 1 {
        Some(classes[0].id.clone())
    } else {
        None
    }
}

/// Resolve a Controller@method reference: the file the path names under `app/Http/Controllers/`,
/// else a class of that name in a controllers directory — the one whose path ends with the written
/// namespace path when several share the name.
fn resolve_controller_method(controller: &str, method: &str, ctx: &Ctx) -> Option<String> {
    let rel = controller.replace('\\', "/");
    let method_in = |file_path: &str| -> Option<String> {
        ctx.get_nodes_in_file(file_path).iter().find(|n| n.kind == NodeKind::Method && n.name == method).map(|n| n.id.clone())
    };

    let controller_file = format!("app/Http/Controllers/{rel}.php");
    if ctx.file_exists(&controller_file) {
        if let Some(found) = method_in(&controller_file) {
            return Some(found);
        }
    }

    let class_name = rel.rsplit('/').next().unwrap_or("");
    let by_name = ctx.get_nodes_by_name(class_name);
    let classes: Vec<_> = by_name.iter().filter(|n| n.kind == NodeKind::Class && n.file_path.contains("Controllers")).collect();
    if rel.contains('/') {
        let suffix = format!("/{}.php", rel.to_lowercase());
        for ctrl in classes.iter().filter(|n| format!("/{}", n.file_path.to_lowercase()).ends_with(&suffix)) {
            if let Some(found) = method_in(&ctrl.file_path) {
                return Some(found);
            }
        }
        // A namespace path that names no file decides nothing by name alone.
        if classes.len() > 1 {
            return None;
        }
    }
    for ctrl in &classes {
        if let Some(found) = method_in(&ctrl.file_path) {
            return Some(found);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handlers_keep_the_controller() {
        assert_eq!(extract_laravel_handler("[PageController::class, 'list']").as_deref(), Some("PageController@list"));
        assert_eq!(extract_laravel_handler("'Common\\Uploads@inline'").as_deref(), Some("Common\\Uploads@inline"));
        assert_eq!(extract_laravel_handler("'App\\Http\\Controllers\\Common\\Uploads@inline'").as_deref(), Some("Common\\Uploads@inline"));
        assert_eq!(extract_laravel_handler("UserController::class").as_deref(), Some("UserController"));
        assert_eq!(extract_laravel_handler("function () {}"), None);
    }

    #[test]
    fn prefixes_nest_through_groups() {
        let src = "Route::prefix('admin')->middleware('auth')->group(function () {\n    Route::get('/users', [UserController::class, 'index']);\n    Route::resource('posts', PostController::class);\n});\nRoute::get('/', fn() => 1);\n";
        let result = LaravelResolver.extract("routes/web.php", src).unwrap();
        let names: Vec<&str> = result.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["GET /admin/users", "GET /", "resource:admin/posts"]);
        assert_eq!(result.nodes[0].id, "route:routes/web.php:2:GET:/users");
        assert_eq!(result.nodes[0].qualified_name, "routes/web.php::route:/admin/users");
    }

    #[test]
    fn route_paths_join() {
        assert_eq!(join_route_path(&["api", "v1/", "/users"]), "/api/v1/users");
        assert_eq!(join_route_path(&[]), "/");
    }
}
