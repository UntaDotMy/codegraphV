//! Astro framework resolver. Port of `src/resolution/frameworks/astro.ts`.
//!
//! Handles Astro component references, the `Astro` global, `astro:*` virtual module imports, and
//! Astro's `src/pages/` file-based routing.

use super::component_folder::in_referencing_folder;
use super::now_ms;
use super::page_component::{page_component_ref, resolve_page_component};
use super::root_manifest_declares;
use crate::ctx::Ctx;
use crate::types::{FrameworkExtractionResult, FrameworkResolver, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};

/// Astro virtual module prefixes — framework-provided, not user code.
const ASTRO_VIRTUAL_MODULES: [&str; 9] = [
    "astro:content",
    "astro:assets",
    "astro:actions",
    "astro:env",
    "astro:i18n",
    "astro:middleware",
    "astro:transitions",
    "astro:components",
    "astro:schema",
];

pub struct AstroResolver;

impl FrameworkResolver for AstroResolver {
    fn name(&self) -> &str {
        "astro"
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        // Check for astro in package.json
        if root_manifest_declares(ctx, &["astro"]) {
            return true;
        }

        // Check for .astro files in project
        ctx.get_all_files().iter().any(|f| f.ends_with(".astro"))
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        // A page route names the component its file is.
        if let Some(page) = resolve_page_component(r, ctx) {
            return Some(page);
        }

        // Pattern 1: the `Astro` global (Astro.props, Astro.url, Astro.params, …) — runtime-provided
        // in every component's frontmatter. Resolving it as framework-provided keeps it from
        // name-matching a user symbol named Astro.
        if r.reference_name == "Astro" || r.reference_name.starts_with("Astro.") {
            return Some(ResolvedRef::new(r, r.from_node_id.clone(), 1.0, ResolvedBy::Framework));
        }

        // Pattern 2: astro:* virtual module imports (astro:content, astro:assets, …)
        if r.is_kind(EdgeKind::Imports)
            && r.reference_name.starts_with("astro:")
            && ASTRO_VIRTUAL_MODULES.iter().any(|prefix| r.reference_name.starts_with(prefix))
        {
            return Some(ResolvedRef::new(r, r.from_node_id.clone(), 1.0, ResolvedBy::Framework));
        }

        // Pattern 3: Component references (PascalCase) — resolve to component nodes. Template tags
        // arrive as `references`, frontmatter expression usages as `calls`. Only from Astro markup:
        // a `.ts` file's `Page` (Playwright's) or a declaration file's `image?: Image` is no
        // `Page.astro` / `Image.astro`.
        if r.file_path.ends_with(".astro") && is_pascal_case(&r.reference_name) && (r.is_kind(EdgeKind::References) || r.is_kind(EdgeKind::Calls)) {
            if let Some(result) = resolve_component(&r.reference_name, &r.file_path, ctx) {
                return Some(ResolvedRef::new(r, result, 0.8, ResolvedBy::Framework));
            }
        }

        None
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<FrameworkExtractionResult> {
        let mut nodes: Vec<Node> = Vec::new();
        let mut references: Vec<UnresolvedRef> = Vec::new();
        let now = now_ms();

        // Normalize to forward slashes
        let normalized = file_path.replace('\\', "/");

        // Astro file-based routing lives under src/pages/ — .astro files are pages, .ts/.js files
        // are API endpoints. (.md/.mdx pages exist too but aren't indexed as source.)
        // Underscore-prefixed segments are excluded from routing by Astro.
        if let Some(pages_match) = re!(r"(?:^|\/)src\/pages\/").find(&normalized) {
            if re!(r"\.(astro|ts|js|mjs)$").is_match(&normalized) {
                let after_pages = &normalized[pages_match.end()..];
                let base = after_pages.rsplit('/').next().unwrap_or("");

                // Underscore-prefixed segments are excluded from routing by Astro; a stray
                // `*.config.*` in a pages dir is never a route.
                if !after_pages.split('/').any(|segment| segment.starts_with('_')) && !re!(r"\.config\.[a-z]+$").is_match(base) {
                    let route_path = file_path_to_astro_route(after_pages);
                    let is_page = normalized.ends_with(".astro");
                    let language = if is_page { Language::Astro } else { Language::TypeScript };
                    let route = Node::new(
                        format!("route:{file_path}:{route_path}:1"),
                        NodeKind::Route,
                        route_path.clone(),
                        format!("{file_path}::route:{route_path}"),
                        file_path,
                        language,
                        1,
                        1,
                        0,
                        0,
                        now,
                    );
                    if is_page {
                        references.push(page_component_ref(&route, ".astro", Language::Astro));
                    } else {
                        // An endpoint is served by the verbs it exports: `export const GET:
                        // APIRoute = …`, `export async function POST(…)`.
                        for m in endpoint_export_re().captures_iter(content) {
                            let verb = m.text(1).or_else(|| m.text(2)).unwrap_or("").to_string();
                            let at = m.get(0).map_or(0, |w| w.start());
                            let line = content.as_bytes()[..at].iter().filter(|c| **c == b'\n').count() as u32 + 1;
                            let mut r = UnresolvedRef::new(route.id.clone(), verb.clone(), EdgeKind::References, line, 0, file_path, language);
                            r.candidates = Some(vec![verb]);
                            references.push(r);
                        }
                    }
                    nodes.push(route);
                }
            }
        }

        Some(FrameworkExtractionResult { nodes, references })
    }
}

/// An Astro endpoint's exported handler: `export const GET`, `export async function POST`.
fn endpoint_export_re() -> std::sync::Arc<Re> {
    Re::cached(
        r"^\s*export\s+(?:(?:async\s+)?function\s+(GET|POST|PUT|PATCH|DELETE|OPTIONS|HEAD|ALL)\b|const\s+(GET|POST|PUT|PATCH|DELETE|OPTIONS|HEAD|ALL)\b)",
        "gm",
    )
}

/// Check if string is PascalCase.
fn is_pascal_case(s: &str) -> bool {
    re!(r"^[A-Z][a-zA-Z0-9]*$").is_match(s)
}

/// Resolve an Astro component reference using name-based lookup.
fn resolve_component(name: &str, from_file: &str, ctx: &Ctx) -> Option<String> {
    // Look for component nodes by name
    let candidates = ctx.get_nodes_by_name(name);
    let components: Vec<_> = candidates.iter().filter(|n| n.kind == NodeKind::Component).collect();

    if components.is_empty() {
        return None;
    }

    // Prefer the referencing file's folder, or one below it
    if let Some(same) = in_referencing_folder(from_file, components.iter(), |n| n.file_path.as_str()).next() {
        return Some(same.id.clone());
    }

    // No positional signal: only an UNAMBIGUOUS name may resolve — picking components[0] would
    // choose an arbitrary same-named component in a multi-app monorepo (#764). Ambiguity falls
    // through to the name-matcher, whose proximity scoring decides.
    if components.len() == 1 {
        Some(components[0].id.clone())
    } else {
        None
    }
}

/// Convert a path under src/pages/ to an Astro route path.
///
/// ```text
/// blog/[slug].astro        -> /blog/:slug
/// blog/[...path].astro     -> /blog/*path
/// api/posts.ts             -> /api/posts
/// index.astro              -> /
/// ```
fn file_path_to_astro_route(after_pages: &str) -> String {
    // Remove the extension
    let without_ext = re!(r"\.(astro|ts|js|mjs)$").replace_first(after_pages, |_| String::new());

    // index files map to their parent path (index -> /, blog/index -> /blog)
    let without_index = re!(r"(^|\/)index$").replace_first(&without_ext, |c| c.text(1).unwrap_or("").to_string());
    let without_index = re!(r"\/$").replace_first(&without_index, |_| String::new());

    // Convert Astro param syntax
    let converted = re!(r"\[\.\.\.([^\]]+)\]").replace_all(&without_index, |c| Some(format!("*{}", c.text(1).unwrap_or("")))); // [...rest] -> *rest (catch-all)
    let converted = re!(r"\[([^\]]+)\]").replace_all(&converted, |c| Some(format!(":{}", c.text(1).unwrap_or("")))); // [param] -> :param
    let route = format!("/{converted}");

    if route == "/" {
        return "/".to_string();
    }
    // Remove trailing slash
    re!(r"\/$").replace_first(&route, |_| String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_follow_the_pages_tree() {
        assert_eq!(file_path_to_astro_route("index.astro"), "/");
        assert_eq!(file_path_to_astro_route("blog/index.astro"), "/blog");
        assert_eq!(file_path_to_astro_route("blog/[slug].astro"), "/blog/:slug");
        assert_eq!(file_path_to_astro_route("blog/[...path].astro"), "/blog/*path");
        assert_eq!(file_path_to_astro_route("api/posts.ts"), "/api/posts");
    }

    #[test]
    fn endpoint_verbs_are_found_on_their_own_lines() {
        let src = "import x from 'y'\n\nexport const GET = () => {}\nexport async function POST() {}\n";
        let verbs: Vec<String> = endpoint_export_re().captures_iter(src).map(|m| m.text(1).or_else(|| m.text(2)).unwrap_or("").to_string()).collect();
        assert_eq!(verbs, vec!["GET", "POST"]);
    }
}
