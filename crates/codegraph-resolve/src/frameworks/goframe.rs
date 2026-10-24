//! GoFrame framework resolver (route metadata) — issue #747. Port of
//! `src/resolution/frameworks/goframe.ts`.
//!
//! GoFrame's "standard router" binds routes reflectively, so there is no literal path string at a
//! `.GET("/x", handler)` call site and no static edge from a route to the controller method that
//! serves it. The structural facts live in two places, joined only at runtime by GoFrame:
//!
//! ```text
//! // api/user/v1/user_sign_in.go — the route lives in a struct tag on the request type
//! type SignInReq struct {
//!     g.Meta `path:"/user/sign-in" method:"post" tags:"UserService" summary:"…"`
//!     …
//! }
//! // internal/controller/user/user_v1_sign_in.go — the handler takes *that* request type
//! func (c *ControllerV1) SignIn(ctx context.Context, req *v1.SignInReq) (res *v1.SignInRes, err error)
//! // internal/cmd/cmd.go — reflective binding (no path, no handler name)
//! group.Bind(user.NewV1())
//! ```
//!
//! This resolver handles the FIRST half: it reads the `g.Meta` struct tag on a request type into a
//! `route` node (`POST /user/sign-in`). The route → handler EDGE is the genuinely reflective part —
//! the method name is NOT derivable from the request type, so the only reliable join is the request
//! type appearing in the method's parameter signature. That whole-graph join is done by the
//! companion `goframe_route_edges` synthesizer, which reads the request type back out of the route
//! node's qualified name.
//!
//! Honesty note: the route node carries the `g.Meta` path verbatim. The group prefix from
//! `s.Group("/api", …)` / nested `group.Group("/v1", …)` is applied by reflective `Bind` at runtime
//! and is deliberately NOT reconstructed here.

use super::now_ms;
use crate::ctx::Ctx;
use crate::types::{FrameworkExtractionResult, FrameworkResolver, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_types::jscompat::utf16_len;
use codegraph_types::{Language, Node, NodeKind};

/// Marker embedded in a route node's qualified name so the synthesizer can read back the request
/// type to join on. The value after it is the package-qualified request type (`cash.ListReq`) — the
/// package disambiguates the many identical bare names (`ListReq`, `GetReq`) a large app defines,
/// one per module. Falls back to the bare type when no `package` declaration is found.
pub const GOFRAME_ROUTE_MARKER: &str = "::goframe-route:";

pub struct GoframeResolver;

const LANGUAGES: [Language; 1] = [Language::Go];

/// A request type carrying a routable `g.Meta` tag. `g.Meta` is, by GoFrame convention, the first
/// embedded field of the struct, so anchoring on `struct { g.Meta `…` }` is both precise and cheap.
/// Response types embed `g.Meta` too but tag it `mime:"…"` with no `path:` — the path requirement
/// filters them out.
fn goframe_meta_re() -> std::sync::Arc<Re> {
    Re::cached(r"\btype\s+([A-Z]\w*)\s+struct\s*\{\s*g\.Meta\s+`([^`]*)`", "g")
}

impl FrameworkResolver for GoframeResolver {
    fn name(&self) -> &str {
        "goframe"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        // GoFrame is `github.com/gogf/gf` (v1) or `github.com/gogf/gf/v2` (v2).
        ctx.read_file("go.mod").is_some_and(|m| !m.is_empty() && m.contains("github.com/gogf/gf"))
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<FrameworkExtractionResult> {
        if !file_path.ends_with(".go") {
            return Some(FrameworkExtractionResult::default());
        }
        // Cheap reject: the file must mention g.Meta at all.
        if !content.contains("g.Meta") {
            return Some(FrameworkExtractionResult::default());
        }

        let mut nodes: Vec<Node> = Vec::new();
        let now = now_ms();
        let safe = codegraph_text::strip_comments_for_regex(content, "go");
        let pkg = Re::cached(r"^\s*package\s+(\w+)", "m").captures(&safe).and_then(|c| c.text(1).map(String::from));

        for m in goframe_meta_re().captures_iter(&safe) {
            let request_type = m.text(1).unwrap_or("");
            let tag = m.text(2).unwrap_or("");
            let Some(path_match) = re!(r#"\bpath:"([^"]+)""#).captures(tag) else { continue }; // response `g.Meta `mime:…`` and other non-route metadata
            let route_path = path_match.text(1).unwrap_or("");
            // GoFrame defaults to all methods when `method:` is omitted.
            let method = match re!(r#"\bmethod:"([^"]+)""#).captures(tag) {
                Some(mm) => mm.text(1).unwrap_or("").to_uppercase(),
                None => "ANY".to_string(),
            };
            let whole = m.get(0)?;
            let line = safe.as_bytes()[..whole.start()].iter().filter(|c| **c == b'\n').count() as u32 + 1;
            // The handler's signature qualifies the request type with its package
            // (`req *cash.ListReq`); encode `pkg.Type` so the synthesizer can match it.
            let join_key = match &pkg {
                Some(pkg) => format!("{pkg}.{request_type}"),
                None => request_type.to_string(),
            };

            // The request type is the synthesizer's join key — encode it after the marker. The
            // path stays human-readable in `name`.
            nodes.push(Node::new(
                format!("route:{file_path}:{line}:{method}:{route_path}"),
                NodeKind::Route,
                format!("{method} {route_path}"),
                format!("{file_path}{GOFRAME_ROUTE_MARKER}{join_key}"),
                file_path,
                Language::Go,
                line,
                line,
                0,
                utf16_len(whole.as_str()) as u32,
                now,
            ));
        }

        Some(FrameworkExtractionResult { nodes, references: Vec::new() })
    }

    // The route → controller-method edge is reflective (request-type join across files) and is
    // built by the `goframe_route_edges` synthesizer after the graph is complete. This resolver
    // creates no references of its own.
    fn resolve(&self, _r: &UnresolvedRef, _ctx: &Ctx) -> Option<ResolvedRef> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_types_with_a_path_become_routes() {
        let src = "package v1\n\ntype SignInReq struct {\n\tg.Meta `path:\"/user/sign-in\" method:\"post\" tags:\"UserService\"`\n}\ntype SignInRes struct {\n\tg.Meta `mime:\"text/html\"`\n}\n";
        let result = GoframeResolver.extract("api/user/v1/user_sign_in.go", src).unwrap();
        assert_eq!(result.nodes.len(), 1);
        assert_eq!(result.nodes[0].name, "POST /user/sign-in");
        assert_eq!(result.nodes[0].qualified_name, "api/user/v1/user_sign_in.go::goframe-route:v1.SignInReq");
        assert_eq!(result.nodes[0].start_line, 3);
    }
}
