//! Svelte / SvelteKit framework resolver. Port of `src/resolution/frameworks/svelte.ts`.
//!
//! Handles Svelte component references, Svelte 5 runes, store auto-subscriptions, and SvelteKit
//! route/module patterns.

use super::now_ms;
use crate::ctx::Ctx;
use crate::types::{FrameworkExtractionResult, FrameworkResolver, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::{EdgeKind, Language, Node, NodeKind};

/// Svelte 5 runes — compiler-provided, not user code.
const SVELTE_RUNES: [&str; 13] = [
    "$state",
    "$state.raw",
    "$state.snapshot",
    "$derived",
    "$derived.by",
    "$effect",
    "$effect.pre",
    "$effect.root",
    "$effect.tracking",
    "$props",
    "$bindable",
    "$inspect",
    "$host",
];

/// SvelteKit framework-provided module prefixes.
const SVELTEKIT_MODULE_PREFIXES: [&str; 9] = [
    "$app/navigation",
    "$app/stores",
    "$app/environment",
    "$app/forms",
    "$app/paths",
    "$env/static/private",
    "$env/static/public",
    "$env/dynamic/private",
    "$env/dynamic/public",
];

pub struct SvelteResolver;

impl FrameworkResolver for SvelteResolver {
    fn name(&self) -> &str {
        "svelte"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&[Language::Svelte])
    }

    // Runes are written in `.svelte.ts` / `.svelte.js` modules too, and a SvelteKit `+page.ts` or
    // `hooks.server.ts` imports `$lib/…` and `$app/…`.
    fn resolve_languages(&self) -> Option<&[Language]> {
        Some(&[Language::Svelte, Language::TypeScript, Language::JavaScript])
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        // Check for svelte or @sveltejs/kit in package.json
        if super::root_manifest_declares(ctx, &["svelte", "@sveltejs/kit"]) {
            return true;
        }

        // Check for .svelte files in project
        ctx.get_all_files().iter().any(|f| f.ends_with(".svelte"))
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        // Pattern 1: Svelte runes ($state, $derived, $effect, etc.). Runes are compiler-provided —
        // return a high-confidence "framework" resolution so CodeGraph doesn't waste time searching
        // for user-defined symbols. The fromNodeId is the target since runes don't have real ones.
        if SVELTE_RUNES.contains(&r.reference_name.as_str()) {
            return Some(ResolvedRef::new(r, r.from_node_id.clone(), 1.0, ResolvedBy::Framework));
        }

        // Pattern 2: Store auto-subscriptions ($storeName) — a `.svelte` component's syntax only.
        // In any other script `$n` is a name of its own (jQuery's `$el`, a compiler's `$n`), never
        // the store `n`.
        if r.language == Language::Svelte && r.reference_name.starts_with('$') && !r.reference_name.starts_with("$$") {
            let store_name = &r.reference_name[1..];
            let store = ctx.get_nodes_by_name(store_name).iter().find(|n| n.kind == NodeKind::Variable || n.kind == NodeKind::Constant).cloned();
            if let Some(store) = store {
                return Some(ResolvedRef::new(r, store.id.clone(), 0.85, ResolvedBy::Framework));
            }
        }

        // Pattern 3: SvelteKit module imports ($app/*, $env/*, $lib/*)
        if r.is_kind(EdgeKind::Imports) && r.reference_name.starts_with('$') {
            // $lib/* resolves to src/lib/* — try to find the target file
            if r.reference_name.starts_with("$lib/") {
                let lib_path = r.reference_name.replacen("$lib/", "src/lib/", 1);
                // Try common extensions
                for ext in ["", ".ts", ".js", ".svelte", "/index.ts", "/index.js"] {
                    let full_path = format!("{lib_path}{ext}");
                    if ctx.file_exists(&full_path) {
                        let nodes = ctx.get_nodes_in_file(&full_path);
                        if let Some(first) = nodes.first() {
                            return Some(ResolvedRef::new(r, first.id.clone(), 0.9, ResolvedBy::Framework));
                        }
                    }
                }
            }

            // $app/* and $env/* are framework-provided
            if SVELTEKIT_MODULE_PREFIXES.iter().any(|prefix| r.reference_name.starts_with(prefix)) {
                return Some(ResolvedRef::new(r, r.from_node_id.clone(), 1.0, ResolvedBy::Framework));
            }
        }

        // Pattern 4: Component references (PascalCase) — resolve to .svelte files
        if is_pascal_case(&r.reference_name) && r.is_kind(EdgeKind::Calls) {
            if let Some(result) = resolve_component(&r.reference_name, &r.file_path, ctx) {
                return Some(ResolvedRef::new(r, result, 0.8, ResolvedBy::Framework));
            }
        }

        None
    }

    fn extract(&self, file_path: &str, _content: &str) -> Option<FrameworkExtractionResult> {
        let mut nodes: Vec<Node> = Vec::new();
        let now = now_ms();

        // Detect SvelteKit route files
        let file_name = file_path.rsplit(['/', '\\']).next().unwrap_or("");

        // Only a `+page.svelte` is a URL. A `+layout.svelte` and a `+error.svelte` sit at the same
        // path as the page beside them, so emitting a route for them put the same address in the
        // index two and three times over — one `/` for the page, one for the layout, one for the
        // error page — which the Screens picture then drew as three separate screens.
        if svelte_kit_route_info(file_name) == Some("page") {
            // Extract route path from directory structure,
            // e.g., src/routes/blog/[slug]/+page.svelte -> /blog/:slug
            if let Some(route_path) = file_path_to_svelte_kit_route(file_path).filter(|p| !p.is_empty()) {
                nodes.push(Node::new(
                    format!("route:{file_path}:{route_path}:1"),
                    NodeKind::Route,
                    route_path.clone(),
                    format!("{file_path}::route:{route_path}"),
                    file_path,
                    if file_path.ends_with(".svelte") { Language::Svelte } else { Language::TypeScript },
                    1,
                    1,
                    0,
                    0,
                    now,
                ));
            }
        }

        Some(FrameworkExtractionResult { nodes, references: Vec::new() })
    }
}

/// Check if string is PascalCase.
fn is_pascal_case(s: &str) -> bool {
    re!(r"^[A-Z][a-zA-Z0-9]*$").is_match(s)
}

/// Resolve a Svelte component reference using name-based lookup.
fn resolve_component(name: &str, from_file: &str, ctx: &Ctx) -> Option<String> {
    // Look for component nodes by name
    let candidates = ctx.get_nodes_by_name(name);
    let components: Vec<_> = candidates.iter().filter(|n| n.kind == NodeKind::Component).collect();

    if components.is_empty() {
        return None;
    }

    // Prefer same directory
    let from_dir = from_file.rfind('/').map_or("", |i| &from_file[..i]);
    if let Some(same) = components.iter().find(|n| n.file_path.starts_with(from_dir)) {
        return Some(same.id.clone());
    }

    // No positional signal: only an UNAMBIGUOUS name may resolve — picking components[0] chose an
    // arbitrary same-named component in a multi-app monorepo (#764). Ambiguity falls through to
    // the name-matcher, whose proximity scoring decides.
    if components.len() == 1 {
        Some(components[0].id.clone())
    } else {
        None
    }
}

/// SvelteKit route file patterns, and the role of each.
fn svelte_kit_route_info(file_name: &str) -> Option<&'static str> {
    Some(match file_name {
        "+page.svelte" => "page",
        "+page.ts" | "+page.js" => "page-load",
        "+page.server.ts" | "+page.server.js" => "page-server-load",
        "+layout.svelte" => "layout",
        "+layout.ts" | "+layout.js" => "layout-load",
        "+layout.server.ts" | "+layout.server.js" => "layout-server-load",
        "+server.ts" | "+server.js" => "api-endpoint",
        "+error.svelte" => "error-page",
        _ => return None,
    })
}

/// Convert a file path to a SvelteKit route path.
fn file_path_to_svelte_kit_route(file_path: &str) -> Option<String> {
    // Normalize to forward slashes
    let normalized = file_path.replace('\\', "/");

    // Find the routes directory
    let routes_index = normalized.find("/routes/")?;

    // Extract the path after routes/
    let after_routes = &normalized[routes_index + "/routes/".len()..];

    // Remove the file name
    let dir_path = after_routes.rfind('/').map_or("", |i| &after_routes[..i]);

    // A `(group)` directory shares a layout and never appears in the URL: `(app)/(layout)/blocks`
    // is `/blocks`. A parameter's `=matcher` checks its value and is no part of its name.
    let segments: Vec<&str> = dir_path.split('/').filter(|seg| !seg.is_empty() && !re!(r"^\(.*\)$").is_match(seg)).collect();
    let joined = segments.join("/");
    // [...rest] / [...rest=m] -> *rest
    let joined = re!(r"\[\.\.\.([^\]=]+)(?:=[^\]]*)?\]").replace_all(&joined, |c| Some(format!("*{}", c.text(1).unwrap_or(""))));
    // [[optional]] -> :optional?
    let joined = re!(r"\[{2}([^\]=]+)(?:=[^\]]*)?\]{2}").replace_all(&joined, |c| Some(format!(":{}?", c.text(1).unwrap_or(""))));
    // [param] / [param=m] -> :param
    let joined = re!(r"\[([^\]=]+)(?:=[^\]]*)?\]").replace_all(&joined, |c| Some(format!(":{}", c.text(1).unwrap_or(""))));
    let route = format!("/{joined}");

    if route == "/" {
        return Some("/".to_string());
    }
    // Remove trailing slash
    Some(re!(r"\/$").replace_first(&route, |_| String::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_follow_the_directory_tree() {
        let route = |p: &str| file_path_to_svelte_kit_route(p);
        assert_eq!(route("routes/+page.svelte").as_deref(), None); // no leading slash before `routes/`
        assert_eq!(route("src/routes/+page.svelte").as_deref(), Some("/"));
        assert_eq!(route("app/src/routes/+page.svelte").as_deref(), Some("/"));
        assert_eq!(route("app/src/routes/blog/[slug]/+page.svelte").as_deref(), Some("/blog/:slug"));
        assert_eq!(route("app/src/routes/(app)/docs/[...rest=matcher]/+page.svelte").as_deref(), Some("/docs/*rest"));
        assert_eq!(route("app/src/routes/[[lang]]/about/+page.svelte").as_deref(), Some("/:lang?/about"));
    }

    #[test]
    fn only_the_page_is_a_url() {
        assert_eq!(svelte_kit_route_info("+page.svelte"), Some("page"));
        assert_eq!(svelte_kit_route_info("+layout.svelte"), Some("layout"));
        assert_eq!(svelte_kit_route_info("Button.svelte"), None);
    }
}
