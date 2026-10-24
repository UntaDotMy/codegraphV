//! Rust framework resolver: Actix-web, Rocket, Axum and common Rust patterns. Port of
//! `src/resolution/frameworks/rust.ts`.

use super::cargo_workspace::get_cargo_workspace_crate_map;
use super::name_heuristic::{pick_by_name_and_kind, NamePickOptions};
use super::now_ms;
use crate::ctx::Ctx;
use crate::name_matcher::rust::scope::is_rust_name_in_scope;
use crate::types::{FrameworkExtractionResult, FrameworkResolver, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::jscompat::{utf16_byte_index, utf16_len};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::rc::Rc;

pub struct RustResolver;

const LANGUAGES: [Language; 1] = [Language::Rust];

// Directory patterns
const HANDLER_DIRS: [&str; 5] = ["/handlers/", "/handler/", "/api/", "/routes/", "/controllers/"];
const SERVICE_DIRS: [&str; 4] = ["/services/", "/service/", "/repository/", "/domain/"];
const MODEL_DIRS: [&str; 6] = ["/models/", "/model/", "/entities/", "/entity/", "/domain/", "/types/"];

const FUNCTION_KINDS: [NodeKind; 1] = [NodeKind::Function];
const SERVICE_KINDS: [NodeKind; 2] = [NodeKind::Struct, NodeKind::Trait];
const STRUCT_KINDS: [NodeKind; 1] = [NodeKind::Struct];

/// The crate map, built once per resolution context.
#[derive(Default)]
struct CargoWorkspaceMap(RefCell<Option<Rc<IndexMap<String, String>>>>);

fn cached_cargo_workspace_crate_map(ctx: &Ctx) -> Rc<IndexMap<String, String>> {
    let memo = ctx.memo::<CargoWorkspaceMap>();
    if let Some(cached) = memo.0.borrow().as_ref() {
        return Rc::clone(cached);
    }
    let map = Rc::new(get_cargo_workspace_crate_map(ctx));
    *memo.0.borrow_mut() = Some(Rc::clone(&map));
    map
}

/// Whether the item a name heuristic found is one the reference can name: `Context<'_>` under
/// `use std::task::{Context, Poll}` is std's, not tokio's `runtime::context::Context`.
fn in_rust_scope(id: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    match ctx.node_by_id(id) {
        None => true,
        Some(node) => is_rust_name_in_scope(&node, r, ctx),
    }
}

impl FrameworkResolver for RustResolver {
    fn name(&self) -> &str {
        "rust"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        // Check for Cargo.toml (Rust project signature)
        ctx.file_exists("Cargo.toml")
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        let name = r.reference_name.as_str();

        // Pattern 1: Handler references
        if name.ends_with("_handler") || name.starts_with("handle_") {
            if let Some(result) = resolve_by_name_and_kind(r, &FUNCTION_KINDS, &HANDLER_DIRS, ctx) {
                if in_rust_scope(&result, r, ctx) {
                    return Some(ResolvedRef::new(r, result, 0.8, ResolvedBy::Framework));
                }
            }
        }

        // Pattern 2: Service/Repository trait implementations
        if name.ends_with("Service") || name.ends_with("Repository") {
            if let Some(result) = resolve_by_name_and_kind(r, &SERVICE_KINDS, &SERVICE_DIRS, ctx) {
                if in_rust_scope(&result, r, ctx) {
                    return Some(ResolvedRef::new(r, result, 0.8, ResolvedBy::Framework));
                }
            }
        }

        // Pattern 3: Struct references (PascalCase)
        if re!(r"^[A-Z][a-zA-Z]+$").is_match(name) {
            if let Some(result) = resolve_by_name_and_kind(r, &STRUCT_KINDS, &MODEL_DIRS, ctx) {
                if in_rust_scope(&result, r, ctx) {
                    return Some(ResolvedRef::new(r, result, 0.7, ResolvedBy::Framework));
                }
            }
        }

        // Pattern 4: Module references
        if re!(r"^[a-z_]+$").is_match(name) {
            if let Some(result) = resolve_module(name, ctx) {
                // Workspace-manifest hits are an exact crate-name -> crate-root mapping straight
                // from Cargo.toml, so we trust them above name-matcher self-file matches (which
                // otherwise win at 0.7 because every file containing `use foo::...` has its own
                // import node named `foo`).
                return Some(ResolvedRef::new(r, result.target_id, if result.from_workspace { 0.95 } else { 0.6 }, ResolvedBy::Framework));
            }
        }

        None
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<FrameworkExtractionResult> {
        if !file_path.ends_with(".rs") {
            return Some(FrameworkExtractionResult::default());
        }
        let mut nodes: Vec<Node> = Vec::new();
        let mut references: Vec<UnresolvedRef> = Vec::new();
        let now = now_ms();
        let safe = codegraph_text::strip_comments_for_regex(content, "rust");
        let safe: &str = &safe;

        let route_node = |line: u32, method: &str, route_path: &str, end_column: u32| {
            Node::new(
                format!("route:{file_path}:{line}:{method}:{route_path}"),
                NodeKind::Route,
                format!("{method} {route_path}"),
                format!("{file_path}::route:{route_path}"),
                file_path,
                Language::Rust,
                line,
                line,
                0,
                end_column,
                now,
            )
        };
        // Where a handler's own name is written, for the handler expression `expr` ending at
        // `expr_end`: the Rust scope gate reads the reference's line, and rustfmt wraps a long
        // `.route(` call so its handler sits below it (#2326).
        let handler_site = |expr_end: usize, expr: &str, handler: &str| -> (u32, u32) {
            let offset = expr_end - expr.len() + expr.rfind(handler).unwrap_or(0);
            let before = &safe[..offset];
            let line = before.matches('\n').count() as u32 + 1;
            let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
            (line, utf16_len(&before[line_start..]) as u32)
        };
        let line_of = |index: usize| safe[..index].matches('\n').count() as u32 + 1;

        // Actix-web / Rocket attribute: #[get("/path")] fn handler(..)
        // Capture the method, path, and the fn identifier that follows.
        let attr_regex = re!(r#"#\[(get|post|put|patch|delete|head|options)\s*\(\s*["']([^"']+)["'][^\]]*\)\]"#, "g");
        for m in attr_regex.captures_iter(safe) {
            let Some(whole) = m.get(0) else { continue };
            let route_path = m.text(2).unwrap_or("");
            let line = line_of(whole.start());
            let upper = m.text(1).unwrap_or("").to_uppercase();

            let node = route_node(line, &upper, route_path, utf16_len(whole.as_str()) as u32);
            let route_id = node.id.clone();
            nodes.push(node);

            let tail = &safe[whole.end()..];
            if let Some(fn_match) = re!(r"\n\s*(?:pub\s+)?(?:async\s+)?fn\s+(\w+)").captures(tail) {
                references.push(UnresolvedRef::new(route_id, fn_match.text(1).unwrap_or(""), EdgeKind::References, line, 0, file_path, Language::Rust));
            }
        }

        // Axum: .route("/path", get(h1).post(h2)…) — balanced-paren scan the route call, then
        // emit one route node per chained method. Handlers may be namespaced
        // (`get(module::handler)`, `get(self::list)`); take the last path segment so the ref names
        // the fn, not the module.
        let route_open_regex = re!(r"\.route\s*\(", "g");
        for m in route_open_regex.captures_iter(safe) {
            let Some(whole) = m.get(0) else { continue };
            let Some(open_rel) = safe[whole.start()..].find('(') else { continue };
            let open_idx = whole.start() + open_rel;
            let Some(close_idx) = find_matching_paren(safe, open_idx) else { continue };

            let args = &safe[open_idx + 1..close_idx];
            let Some(path_match) = re!(r#"^\s*"([^"]+)"\s*,"#).captures(args) else { continue };
            let route_path = path_match.text(1).unwrap_or("");
            let path_match_len = path_match.get(0).map(|w| w.end()).unwrap_or(0);
            let line = line_of(whole.start());

            let method_body = &args[path_match_len..];
            let body_at = open_idx + 1 + path_match_len;
            let method_handler_regex = re!(r"\b(get|post|put|patch|delete|head|options|trace)\s*\(\s*([A-Za-z_][\w:]*)", "g");
            // The method routers are the argument's top-level chain (`get(a).post(b)`); a `get(`
            // nested in it — `get(|| async { cache.get(key) })` — is a call inside a closure
            // handler, not a route.
            let mut depth: i32 = 0;
            let mut scanned = 0usize;
            let bytes = method_body.as_bytes();
            for mh in method_handler_regex.captures_iter(method_body) {
                let Some(mh_whole) = mh.get(0) else { continue };
                while scanned < mh_whole.start() {
                    match bytes[scanned] {
                        b'(' => depth += 1,
                        b')' => depth -= 1,
                        _ => {}
                    }
                    scanned += 1;
                }
                if depth > 0 {
                    continue;
                }
                let upper = mh.text(1).unwrap_or("").to_uppercase();
                let expr = mh.text(2).unwrap_or("");
                let Some(handler) = last_path_segment(expr) else { continue };
                let site = handler_site(body_at + mh_whole.end(), expr, handler);

                let node = route_node(line, &upper, route_path, 0);
                let route_id = node.id.clone();
                nodes.push(node);
                references.push(UnresolvedRef::new(route_id, handler, EdgeKind::References, site.0, site.1, file_path, Language::Rust));
            }
        }

        // Actix-web builder API (the dominant actix routing style; attribute macros are handled
        // above). The handler lives in `.to(handler)`, not `get(handler)`.
        let mut push_actix_route = |route_path: &str, method: &str, handler_expr: &str, line: u32, expr_end: usize| {
            let Some(handler) = last_path_segment(handler_expr) else { return };
            let site = handler_site(expr_end, handler_expr, handler);
            let upper = method.to_uppercase();
            let node = route_node(line, &upper, route_path, 0);
            let route_id = node.id.clone();
            nodes.push(node);
            references.push(UnresolvedRef::new(route_id, handler, EdgeKind::References, site.0, site.1, file_path, Language::Rust));
        };

        // web::resource("/path") { .route(web::METHOD().to(h)) | .to(h) } — possibly chained.
        let resource_regex = re!(r#"web::resource\s*\(\s*"([^"]+)"\s*\)"#, "g");
        for m in resource_regex.captures_iter(safe) {
            let Some(whole) = m.get(0) else { continue };
            let route_path = m.text(1).unwrap_or("");
            let start_line = line_of(whole.start());
            let after = whole.end();
            // Bound the resource's method chain at the next resource() to avoid bleed.
            let next_res = safe[after..].find("web::resource").map(|i| after + i);
            let limit = after + utf16_byte_index(&safe[after..], 500);
            let mut end = limit.min(next_res.unwrap_or(safe.len()));
            // ...and at the `)` of the call it is an argument of: in
            // `.service(web::resource("/a").to(a)).route("/b", web::get().to(b))`, `b` is not
            // `/a`'s.
            let mut depth: i32 = 0;
            for (i, byte) in safe.as_bytes()[after..end].iter().enumerate() {
                match byte {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth < 0 {
                            end = after + i;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let chain = &safe[after..end];

            let method_to = re!(r"web::(get|post|put|patch|delete|head)\s*\(\s*\)\s*\.to\s*\(\s*([A-Za-z_][\w:]*)", "g");
            let mut found = false;
            for m2 in method_to.captures_iter(chain) {
                let Some(m2_whole) = m2.get(0) else { continue };
                let m_line = start_line + chain[..m2_whole.start()].matches('\n').count() as u32;
                push_actix_route(route_path, m2.text(1).unwrap_or(""), m2.text(2).unwrap_or(""), m_line, after + m2_whole.end());
                found = true;
            }
            // Direct `.resource("/x").to(handler)` (all methods) when no explicit verb route.
            if !found {
                if let Some(direct) = re!(r"^\s*\.to\s*\(\s*([A-Za-z_][\w:]*)").captures(chain) {
                    let direct_end = direct.get(0).map(|w| w.end()).unwrap_or(0);
                    push_actix_route(route_path, "ANY", direct.text(1).unwrap_or(""), start_line, after + direct_end);
                }
            }
        }

        // App-level: .route("/path", web::METHOD().to(handler)).
        let app_route_regex = re!(
            r#"\.route\s*\(\s*"([^"]+)"\s*,\s*web::(get|post|put|patch|delete|head)\s*\(\s*\)\s*\.to\s*\(\s*([A-Za-z_][\w:]*)"#,
            "g"
        );
        for m in app_route_regex.captures_iter(safe) {
            let Some(whole) = m.get(0) else { continue };
            let line = line_of(whole.start());
            push_actix_route(m.text(1).unwrap_or(""), m.text(2).unwrap_or(""), m.text(3).unwrap_or(""), line, whole.end());
        }

        Some(FrameworkExtractionResult { nodes, references })
    }
}

/// `expr.split('::').filter(Boolean).pop()`.
fn last_path_segment(expr: &str) -> Option<&str> {
    expr.split("::").filter(|s| !s.is_empty()).last()
}

/// Index of the ')' that matches the '(' at `open_idx`, or `None` if unbalanced.
fn find_matching_paren(s: &str, open_idx: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (i, byte) in s.as_bytes()[open_idx..].iter().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open_idx + i);
                }
            }
            _ => {}
        }
    }
    None
}

/// A framework name heuristic's pick (see `name_heuristic`): in scope by its `use`s and module
/// paths.
fn resolve_by_name_and_kind(r: &UnresolvedRef, kinds: &[NodeKind], preferred_dir_patterns: &[&str], ctx: &Ctx) -> Option<String> {
    let accept = |n: &Node| is_rust_name_in_scope(n, r, ctx);
    pick_by_name_and_kind(
        r,
        kinds,
        &|f: &str| preferred_dir_patterns.iter().any(|d| f.contains(d)),
        ctx,
        // The file is a module: a sibling file is another one, reached only through a `use`.
        &NamePickOptions { same_directory: Some(false), accept: Some(&accept) },
    )
}

struct ModuleResolution {
    target_id: String,
    from_workspace: bool,
}

fn resolve_module(name: &str, ctx: &Ctx) -> Option<ModuleResolution> {
    // Rust modules can be either mod.rs in a directory or name.rs
    let local_paths = [format!("src/{name}.rs"), format!("src/{name}/mod.rs")];

    let workspace_crates = cached_cargo_workspace_crate_map(ctx);
    let workspace_paths: Vec<String> = match workspace_crates.get(name) {
        Some(crate_path) => vec![format!("{crate_path}/src/lib.rs"), format!("{crate_path}/src/main.rs")],
        None => Vec::new(),
    };

    let candidates = local_paths.iter().map(|p| (p, false)).chain(workspace_paths.iter().map(|p| (p, true)));
    for (mod_path, from_workspace) in candidates {
        if !ctx.file_exists(mod_path) {
            continue;
        }
        let nodes = ctx.get_nodes_in_file(mod_path);
        if let Some(mod_node) = nodes.iter().find(|n| n.kind == NodeKind::Module) {
            return Some(ModuleResolution { target_id: mod_node.id.clone(), from_workspace });
        }
        if let Some(first) = nodes.first() {
            return Some(ModuleResolution { target_id: first.id.clone(), from_workspace });
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_and_builder_routes_are_extracted() {
        let src = "#[get(\"/users\")]\nasync fn list() {}\n\nfn cfg(c: &mut web::ServiceConfig) {\n    c.service(web::resource(\"/a\").route(web::post().to(create_a)));\n    c.route(\"/b\", web::get().to(handlers::show_b));\n}\n\nfn router() -> Router {\n    Router::new().route(\"/c\", get(list_c).post(make_c))\n}\n";
        let result = RustResolver.extract("src/main.rs", src).unwrap();
        let names: Vec<&str> = result.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["GET /users", "GET /c", "POST /c", "POST /a", "GET /b"]);
        let refs: Vec<&str> = result.references.iter().map(|r| r.reference_name.as_str()).collect();
        assert_eq!(refs, vec!["list", "list_c", "make_c", "create_a", "show_b"]);
        // The handler's own site, not the `.route(` call's.
        assert_eq!(result.references[1].line, 10);
        assert_eq!(result.references[1].column, 34);
    }

    #[test]
    fn last_segment_of_a_path() {
        assert_eq!(last_path_segment("module::handler"), Some("handler"));
        assert_eq!(last_path_segment("self::list"), Some("list"));
        assert_eq!(last_path_segment("plain"), Some("plain"));
    }
}
