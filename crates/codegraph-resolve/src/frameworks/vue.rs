//! Vue / Nuxt framework resolver. Port of `src/resolution/frameworks/vue.ts`.
//!
//! Handles Vue component references, compiler macros (`defineProps`, etc.) and Nuxt auto-imports;
//! [`NuxtResolver`] reads Nuxt's file-based routes.

use super::component_folder::in_referencing_folder;
use super::now_ms;
use super::package_deps::depends_on;
use super::page_component::{page_component_ref, resolve_page_component};
use crate::ctx::Ctx;
use crate::import_resolver::resolve_import_path;
use crate::import_resolver::via_import::is_js_path_import_ref;
use crate::types::{FrameworkExtractionResult, FrameworkResolver, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::{EdgeKind, Language, Node, NodeKind};

/// The languages a Vue app's scripts are written in.
const VUE_SCRIPT_LANGUAGES: [Language; 5] = [Language::Vue, Language::JavaScript, Language::TypeScript, Language::Tsx, Language::Jsx];

/// Vue 3 compiler macros — compiler-provided, not user code.
const VUE_COMPILER_MACROS: [&str; 7] =
    ["defineProps", "defineEmits", "defineExpose", "defineOptions", "defineSlots", "defineModel", "withDefaults"];

/// Nuxt auto-imported composables and utilities. `navigateTo` is not one of them here:
/// `navigateTo('/x')` is a navigation, which Vue Router's resolver links to the page it opens, and
/// Nuxt owns the call when it can't (`NuxtResolver::owns_reference`, #2634).
const NUXT_AUTO_IMPORTS: [&str; 29] = [
    // Routing
    "useRoute",
    "useRouter",
    "abortNavigation",
    // Data fetching
    "useFetch",
    "useAsyncData",
    "useLazyFetch",
    "useLazyAsyncData",
    "refreshNuxtData",
    // State
    "useState",
    "clearNuxtState",
    // Head
    "useHead",
    "useSeoMeta",
    "useServerSeoMeta",
    // Runtime
    "useRuntimeConfig",
    "useAppConfig",
    "useNuxtApp",
    // Cookies
    "useCookie",
    // Error
    "useError",
    "createError",
    "showError",
    "clearError",
    // Page/layout
    "definePageMeta",
    "defineNuxtConfig",
    "defineNuxtPlugin",
    "defineNuxtRouteMiddleware",
    // Request
    "useRequestHeaders",
    "useRequestEvent",
    "useRequestFetch",
    "useRequestURL",
];

/// Nuxt virtual module prefixes (auto-import namespaces).
const NUXT_VIRTUAL_MODULES: [&str; 5] = ["#imports", "#components", "#app", "#build", "#head"];

/// The alias prefixes a Vue app's imports name `src/` by, as Vue CLI and Nuxt set them up.
const VUE_ALIAS_PREFIXES: [&str; 2] = ["@/", "~/"];

pub struct VueResolver;

impl FrameworkResolver for VueResolver {
    fn name(&self) -> &str {
        "vue"
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        // Check for vue or nuxt in package.json
        if super::root_manifest_declares(ctx, &["vue", "nuxt", "@nuxt/kit"]) {
            return true;
        }

        // Check for .vue files in project
        ctx.get_all_files().iter().any(|f| f.ends_with(".vue"))
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        // Vue's macros, auto-imports and components are a script's, never a backend's: mealie's
        // Python `QueryFilterBuilder(...)` is not the `QueryFilterBuilder.vue` component.
        if !VUE_SCRIPT_LANGUAGES.contains(&r.language) {
            return None;
        }

        // Pattern 1: Vue compiler macros (defineProps, defineEmits, etc.)
        if VUE_COMPILER_MACROS.contains(&r.reference_name.as_str()) {
            return Some(ResolvedRef::new(r, r.from_node_id.clone(), 1.0, ResolvedBy::Framework));
        }

        // Pattern 2: Nuxt auto-imported composables — unless the file declares its own function of
        // that name, which shadows the auto-import: elk's lists page calls its own
        // `clearError(true)`, not Nuxt's (#2340).
        if NUXT_AUTO_IMPORTS.contains(&r.reference_name.as_str()) && !declares_own_function(r, ctx) {
            return Some(ResolvedRef::new(r, r.from_node_id.clone(), 1.0, ResolvedBy::Framework));
        }

        // A module an import names by path is the file the path names: the import resolver's to
        // find, through tsconfig paths, workspace packages and the `@/` and `~/` conventions, and
        // to link.
        if is_js_path_import_ref(r) {
            if module_file_node(resolve_import_path(&r.reference_name, &r.file_path, r.language, ctx), r, ctx).is_some() {
                return None;
            }
            // What only a Vue app's bundler finds from a plain script: a component named without
            // `.vue`, or a folder's `index.vue` (`import Layout from '@/layout'` is
            // `src/layout/index.vue`; the path alone names the folder). An SFC's own imports
            // already look for those. Linked to the file, as the import resolver links one.
            if r.language != Language::Vue && VUE_ALIAS_PREFIXES.iter().any(|prefix| r.reference_name.starts_with(prefix)) {
                if let Some(file_node) = module_file_node(resolve_import_path(&r.reference_name, &r.file_path, Language::Vue, ctx), r, ctx) {
                    return Some(ResolvedRef::new(r, file_node.id.clone(), 0.9, ResolvedBy::Framework));
                }
            }
        }

        // Pattern 3: Nuxt virtual module imports (#imports, #components, etc.), which no file of
        // the project is.
        if r.is_kind(EdgeKind::Imports)
            && r.reference_name.starts_with('#')
            && NUXT_VIRTUAL_MODULES.iter().any(|prefix| r.reference_name.starts_with(prefix))
        {
            return Some(ResolvedRef::new(r, r.from_node_id.clone(), 1.0, ResolvedBy::Framework));
        }

        // Pattern 6: Component references (PascalCase) — resolve to .vue files
        if is_pascal_case(&r.reference_name) && r.is_kind(EdgeKind::Calls) {
            if let Some(result) = resolve_component(&r.reference_name, &r.file_path, ctx) {
                return Some(ResolvedRef::new(r, result, 0.8, ResolvedBy::Framework));
            }
        }

        None
    }
}

/// Nuxt's file-based routes: `pages/` screens, `server/api/` endpoints and `middleware/`. Its own
/// resolver, detected only in a Nuxt app: a plain Vue app keeps its views in a `pages/` folder
/// just as often (halo's console does), and those are components a router config names, not
/// addresses.
pub struct NuxtResolver;

impl FrameworkResolver for NuxtResolver {
    fn name(&self) -> &str {
        "nuxt"
    }

    fn app_dependencies(&self) -> Option<&[&'static str]> {
        Some(&["nuxt", "nuxt3", "@nuxt/kit"])
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        if depends_on(ctx, &["nuxt", "nuxt3", "@nuxt/kit"]) {
            return true;
        }
        ctx.get_all_files().iter().any(|f| re!(r"(?:^|\/)nuxt\.config\.(?:[cm]?[jt]s)$").is_match(f))
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        // A page route names the component its file is.
        resolve_page_component(r, ctx)
    }

    /// Nuxt's auto-imported `navigateTo` (#2634). Vue Router's resolver links a call to the page
    /// it opens; one it can't link still calls Nuxt's, never a project function that shares the
    /// name. A file that declares or imports a `navigateTo` calls that one.
    fn owns_reference(&self, r: &UnresolvedRef, ctx: &Ctx) -> bool {
        r.is_kind(EdgeKind::Calls)
            && r.reference_name == "navigateTo"
            && VUE_SCRIPT_LANGUAGES.contains(&r.language)
            && !declares_own_function(r, ctx)
            && !ctx.get_import_mappings(&r.file_path, r.language).iter().any(|m| m.local_name == r.reference_name)
    }

    fn extract(&self, file_path: &str, _content: &str) -> Option<FrameworkExtractionResult> {
        let mut nodes: Vec<Node> = Vec::new();
        let mut references: Vec<UnresolvedRef> = Vec::new();
        let now = now_ms();

        // Forward slashes, and a leading `/` so an app at the repository root (`pages/index.vue`)
        // is found by the same `/pages/` search as a nested one.
        let normalized = format!("/{}", file_path.replace('\\', "/"));
        let is_vue = normalized.ends_with(".vue");

        // Detect Nuxt page routes (pages/ directory)
        if let Some(pages_index) = normalized.find("/pages/") {
            if is_vue {
                if let Some(route_path) = file_path_to_nuxt_route(&normalized, pages_index + "/pages/".len()) {
                    let route = Node::new(
                        format!("route:{file_path}:{route_path}:1"),
                        NodeKind::Route,
                        route_path.clone(),
                        format!("{file_path}::route:{route_path}"),
                        file_path,
                        Language::Vue,
                        1,
                        1,
                        0,
                        0,
                        now,
                    );
                    references.push(page_component_ref(&route, ".vue", Language::Vue));
                    nodes.push(route);
                }
            }
        }

        // Detect Nuxt API routes (server/api/ directory)
        if let Some(api_index) = normalized.find("/server/api/") {
            let after_api = &normalized[api_index + "/server/api/".len()..];
            let route_name = re!(r"\.[^/.]+$").replace_first(after_api, |_| String::new()); // Remove extension
            let route_name = re!(r"(?:^|\/)index$").replace_first(&route_name, |_| String::new()); // index -> parent path
            let route_name = re!(r"\[\.\.\.([^\]]+)\]").replace_all(&route_name, |c| Some(format!("*{}", c.text(1).unwrap_or("")))); // [...slug] -> *slug
            let route_name = re!(r"\[([^\]]+)\]").replace_all(&route_name, |c| Some(format!(":{}", c.text(1).unwrap_or("")))); // [id] -> :id, as a page's params are
            let api_route = if route_name.is_empty() { "/api".to_string() } else { format!("/api/{route_name}") };

            nodes.push(Node::new(
                format!("route:{file_path}:{api_route}:1"),
                NodeKind::Route,
                api_route.clone(),
                format!("{file_path}::route:{api_route}"),
                file_path,
                if is_vue { Language::Vue } else { Language::TypeScript },
                1,
                1,
                0,
                0,
                now,
            ));
        }

        // Detect Nuxt middleware (middleware/ directory)
        if let Some(middleware_index) = normalized.find("/middleware/") {
            let after_middleware = &normalized[middleware_index + "/middleware/".len()..];
            let middleware_name = re!(r"\.[^/.]+$").replace_first(after_middleware, |_| String::new());

            nodes.push(Node::new(
                format!("middleware:{file_path}:{middleware_name}:1"),
                NodeKind::Function,
                middleware_name.clone(),
                format!("{file_path}::middleware:{middleware_name}"),
                file_path,
                if is_vue { Language::Vue } else { Language::TypeScript },
                1,
                1,
                0,
                0,
                now,
            ));
        }

        Some(FrameworkExtractionResult { nodes, references })
    }
}

/// The file node of `file`, a path an import resolved to — none for a folder, an unindexed file or
/// the importing file itself.
fn module_file_node(file: Option<String>, r: &UnresolvedRef, ctx: &Ctx) -> Option<crate::types::NodeRef> {
    let file = file.filter(|f| *f != r.file_path)?;
    ctx.get_nodes_in_file(&file).iter().find(|n| n.kind == NodeKind::File).cloned()
}

/// Does the reference's own file declare a function (or a const holding one) by that name?
fn declares_own_function(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    ctx.get_nodes_in_file(&r.file_path)
        .iter()
        .any(|n| n.name == r.reference_name && matches!(n.kind, NodeKind::Function | NodeKind::Constant | NodeKind::Variable))
}

/// Check if string is PascalCase.
fn is_pascal_case(s: &str) -> bool {
    re!(r"^[A-Z][a-zA-Z0-9]*$").is_match(s)
}

/// Resolve a Vue component reference to its .vue file.
fn resolve_component(name: &str, from_file: &str, ctx: &Ctx) -> Option<String> {
    // Collect ALL basename matches first. The previous version returned the FIRST `Button.vue`
    // found anywhere in the tree, so a multi-app monorepo with one `Button.vue` per app resolved
    // to an arbitrary one (#764).
    let mut matches: Vec<String> = Vec::new();
    for file in ctx.get_all_files().iter() {
        if !file.ends_with(".vue") {
            continue;
        }
        let file_name = file.rsplit(['/', '\\']).next().unwrap_or("");
        if file_name.strip_suffix(".vue").unwrap_or(file_name) == name {
            matches.push(file.clone());
        }
    }
    if matches.is_empty() {
        return None;
    }

    let component_in = |file: &str| -> Option<String> {
        let nodes = ctx.get_nodes_in_file(file);
        nodes.iter().find(|n| n.kind == NodeKind::Component && n.name == name).map(|n| n.id.clone())
    };

    // The referencing file's folder, or one below it, first for specificity
    if let Some(same) = in_referencing_folder(from_file, matches.iter(), |f| f.as_str()).next() {
        return component_in(same);
    }

    // No positional signal: only an UNAMBIGUOUS basename may resolve; ambiguity falls through to
    // the name-matcher's proximity scoring.
    if matches.len() == 1 {
        component_in(&matches[0])
    } else {
        None
    }
}

/// Convert a file path to a Nuxt route path.
fn file_path_to_nuxt_route(normalized: &str, after_pages_start: usize) -> Option<String> {
    let after_pages = &normalized[after_pages_start..];

    // Remove the .vue extension
    let without_ext = re!(r"\.vue$").replace_first(after_pages, |_| String::new());

    // Remove /index suffix (index.vue -> parent route)
    let without_index = re!(r"(?:^|\/)index$").replace_first(&without_ext, |_| String::new());

    // Convert Nuxt param syntax [param] to :param
    let converted = re!(r"\[\.\.\.([^\]]+)\]").replace_all(&without_index, |c| Some(format!("*{}", c.text(1).unwrap_or("")))); // [...slug] -> *slug (catch-all)
    let converted = re!(r"\[{2}([^\]]+)\]{2}").replace_all(&converted, |c| Some(format!(":{}?", c.text(1).unwrap_or("")))); // [[optional]] -> :optional?
    let converted = re!(r"\[([^\]]+)\]").replace_all(&converted, |c| Some(format!(":{}", c.text(1).unwrap_or("")))); // [param] -> :param
    let route = format!("/{converted}");

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
    fn nuxt_routes_follow_the_pages_tree() {
        let route = |p: &str| {
            let n = format!("/{p}");
            file_path_to_nuxt_route(&n, n.find("/pages/").unwrap() + 7)
        };
        assert_eq!(route("pages/index.vue").as_deref(), Some("/"));
        assert_eq!(route("pages/orders/index.vue").as_deref(), Some("/orders"));
        assert_eq!(route("pages/blog/[...slug].vue").as_deref(), Some("/blog/*slug"));
        assert_eq!(route("pages/users/[[id]].vue").as_deref(), Some("/users/:id?"));
        assert_eq!(route("app/pages/users/[id].vue").as_deref(), Some("/users/:id"));
    }

    #[test]
    fn pascal_case_components() {
        assert!(is_pascal_case("Button"));
        assert!(!is_pascal_case("button"));
        assert!(!is_pascal_case("My_Button"));
    }
}
