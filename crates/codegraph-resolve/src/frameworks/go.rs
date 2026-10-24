//! Go framework resolver: Gin, Echo, Fiber, Chi and standard library patterns — port of
//! `src/resolution/frameworks/go.ts`.

use super::name_heuristic::{pick_by_name_and_kind, NamePickOptions};
use super::now_ms;
use crate::ctx::Ctx;
use crate::name_matcher::calls::is_go_bare_name;
use crate::name_matcher::GO_TYPE_KINDS;
use crate::types::{FrameworkExtractionResult, FrameworkResolver, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::jscompat::{is_js_space, js_trim, utf16_len};
use codegraph_types::{EdgeKind, Language, Node, NodeKind, ReferenceKind};

pub struct GoResolver;

const LANGUAGES: [Language; 1] = [Language::Go];

impl FrameworkResolver for GoResolver {
    fn name(&self) -> &str {
        "go"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        // Check for go.mod file (Go modules)
        if ctx.read_file("go.mod").is_some_and(|m| !m.is_empty()) {
            return true;
        }
        // Check for .go files
        ctx.get_all_files().iter().any(|f| f.ends_with(".go"))
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        // The patterns below guess a declaration from the shape of a name, but Go reads a name
        // from one package only: one written bare from the reference's own, `pkg.Node` from that
        // import's (the import resolver's job), `x[i].Error()` from whatever type `x[i]` has. So
        // they guess only for a bare name, only in its own package, and never for an embedded
        // type, which name matching and the import resolver work out.
        if matches!(r.reference_kind, ReferenceKind::Edge(EdgeKind::Extends) | ReferenceKind::Edge(EdgeKind::Implements)) {
            return None;
        }
        if !is_go_bare_name(r, ctx) {
            return None;
        }
        let name = r.reference_name.as_str();

        // Pattern 1: Handler references
        if name.ends_with("Handler") || name.starts_with("Handle") {
            if let Some(result) = resolve_in_own_package(r, &[NodeKind::Function], ctx) {
                return Some(ResolvedRef::new(r, result, 0.8, ResolvedBy::Framework));
            }
        }

        // Pattern 2: Service/Repository references
        if name.ends_with("Service") || name.ends_with("Repository") || name.ends_with("Store") {
            if let Some(result) = resolve_in_own_package(r, &[NodeKind::Struct, NodeKind::Interface], ctx) {
                return Some(ResolvedRef::new(r, result, 0.8, ResolvedBy::Framework));
            }
        }

        // Pattern 3: Middleware references
        if name.ends_with("Middleware") || name.starts_with("Auth") || name.starts_with("Log") {
            if let Some(result) = resolve_in_own_package(r, &[NodeKind::Function], ctx) {
                return Some(ResolvedRef::new(r, result, 0.75, ResolvedBy::Framework));
            }
        }

        // Pattern 4: Model/Entity references — a PascalCase type of any kind: the package's `Node`
        // interface or `Entry` type, not only its structs.
        if re!(r"^[A-Z][a-zA-Z]+$").is_match(name) {
            if let Some(result) = resolve_in_own_package(r, &GO_TYPE_KINDS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.7, ResolvedBy::Framework));
            }
        }

        None
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<FrameworkExtractionResult> {
        if !file_path.ends_with(".go") {
            return Some(FrameworkExtractionResult::default());
        }
        let mut nodes: Vec<Node> = Vec::new();
        let mut references: Vec<UnresolvedRef> = Vec::new();
        let now = now_ms();
        let safe = codegraph_text::strip_comments_for_regex(content, "go");

        // <anyVar>.METHOD("/path", handler) — Gin (GET/POST/...), Chi (Get/Post/...), net/http
        // (HandleFunc/Handle). The receiver is ANY identifier, not just router|r|mux|app|e: real
        // apps route on GROUP vars (`v1.GET`, `PublicGroup.GET`, `userRouter.POST`). The verb +
        // string-path + handler-arg gates keep it route-specific.
        let route_regex =
            re!(r#"\b\w+\.(GET|POST|PUT|PATCH|DELETE|OPTIONS|HEAD|Get|Post|Put|Patch|Delete|Handle|HandleFunc)\s*\(\s*"([^"]+)"\s*,\s*([^)]+)\)"#, "g");
        for m in route_regex.captures_iter(&safe) {
            let Some(whole) = m.get(0) else { continue };
            let raw_method = m.text(1).unwrap_or("");
            let route_path = m.text(2).unwrap_or("");
            let handler_expr = m.text(3).unwrap_or("");

            // The first argument must be URL-shaped, or this is just a method that happens to
            // share a verb name — `cache.Put("key", val)`, `store.Get(...)`, `bus.Handle(
            // "user.created", h)` all polluted the route index (#1259). Real registrations use
            // "/path" (every router), or net/http's Go 1.22 "METHOD /path" patterns on
            // Handle/HandleFunc.
            let method_prefix = match_go_122_method_pattern(route_path, raw_method);
            if !route_path.starts_with('/') && method_prefix.is_none() {
                continue;
            }

            let line = safe[..whole.start()].matches('\n').count() as u32 + 1;
            // "GET /users/{id}" -> method GET, path /users/{id}
            let path = match &method_prefix {
                Some(prefix) => route_path[prefix.len()..].trim_start_matches(is_js_space).to_string(),
                None => route_path.to_string(),
            };
            let method = match method_prefix {
                Some(prefix) => prefix,
                None if raw_method == "Handle" || raw_method == "HandleFunc" => "ANY".to_string(),
                None => raw_method.to_ascii_uppercase(),
            };

            let route_id = format!("route:{file_path}:{line}:{method}:{path}");
            nodes.push(Node::new(
                route_id.clone(),
                NodeKind::Route,
                format!("{method} {path}"),
                format!("{file_path}::route:{path}"),
                file_path,
                Language::Go,
                line,
                line,
                0,
                utf16_len(whole.as_str()) as u32,
                now,
            ));

            if let Some(handler_name) = extract_go_tail_ident(handler_expr) {
                references.push(UnresolvedRef::new(route_id, handler_name, EdgeKind::References, line, 0, file_path, Language::Go));
            }
        }

        Some(FrameworkExtractionResult { nodes, references })
    }
}

/// Go 1.22 net/http mux patterns: `mux.HandleFunc("GET /users/{id}", h)`. The HTTP method when the
/// pattern starts with one, none otherwise. Only Handle/HandleFunc take these — Gin/Chi verb
/// methods take a bare path.
fn match_go_122_method_pattern(route_path: &str, raw_method: &str) -> Option<String> {
    if raw_method != "Handle" && raw_method != "HandleFunc" {
        return None;
    }
    re!(r"^(GET|POST|PUT|PATCH|DELETE|OPTIONS|HEAD|CONNECT|TRACE)\s+\S").captures(route_path).and_then(|m| m.text(1).map(String::from))
}

/// Extract the last identifier from an expression like `pkg.Sub.handler` or `handler`.
fn extract_go_tail_ident(expr: &str) -> Option<String> {
    let squeezed = re!(r"\s+", "g").replace_all_str(js_trim(expr), "");
    let cleaned = re!(r"\(\)$").replace_first(&squeezed, |_| String::new());
    re!(r"(?:\.|^)([A-Za-z_][A-Za-z0-9_]*)$").captures(&cleaned).and_then(|m| m.text(1).map(String::from))
}

/// A framework name heuristic's pick (see `name_heuristic`) among the declarations of the
/// reference's own package — its directory — its own file's first. No folder convention applies:
/// Go never reads a bare name from another package, a dot import aside.
fn resolve_in_own_package(r: &UnresolvedRef, kinds: &[NodeKind], ctx: &Ctx) -> Option<String> {
    let dir = &r.file_path[..r.file_path.rfind('/').map(|i| i + 1).unwrap_or(0)];
    let in_package = |n: &Node| n.file_path.starts_with(dir) && !n.file_path[dir.len()..].contains('/');
    pick_by_name_and_kind(r, kinds, &|_| false, ctx, &NamePickOptions { same_directory: None, accept: Some(&in_package) })
}
