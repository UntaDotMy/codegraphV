//! React Framework Resolver — port of `src/resolution/frameworks/react.ts`.
//!
//! Handles React patterns: React Router routes, components, hooks, contexts. Next.js pages, route
//! handlers and navigation are `nextjs`'s.

mod config;
mod loader;
mod scan;
mod tables;

pub use config::config_href_expression;
pub use tables::is_table_route;

use self::loader::{
    first_import, function_parts, initializer_at, loaded_page, loader_value, new_scope, object_entries, statement_end, top_level_items,
    LAZY_ROUTE_PREFIX,
};
use self::scan::{compose_route_path, scan_route_declarations, RoutePart};
use super::component_folder::in_referencing_folder;
use super::object_literal::match_bracket;
use super::object_literal::skip_string;
use super::package_deps::depends_on;
use super::now_ms;
use crate::alias_binding::{default_export_binding, extract_local_export_aliases};
use crate::ctx::{split_lines, Ctx};
use crate::import_resolver::{resolve_import, resolve_import_path};
use crate::types::{CrossFileNodes, FrameworkExtractionResult, FrameworkResolver, NodeRef, ReExport, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::{js_trim, utf16_from, utf16_prefix};
use codegraph_types::{EdgeKind, Language, Node, NodeKind, ReferenceKind};
use indexmap::IndexSet;
use std::rc::Rc;

/// The prefix of a route's `layout:MainLayout` reference: the component around it.
pub(crate) const LAYOUT_PREFIX: &str = "layout:";
const ROUTE_PARTS_PREFIX: &str = "route-parts:";

/// The languages React components, hooks and contexts are written and used in.
fn is_react_script_language(language: Language) -> bool {
    matches!(language, Language::TypeScript | Language::JavaScript | Language::Tsx | Language::Jsx)
}

/// The files a lookup reads, in the order it reads them.
type Reads = IndexSet<String>;

/// The export a lazy route's loader picks, named the way Vue Router and Angular name a lazily
/// loaded component: `import:./tabs#TestTab`, `import:./pages/login#default`. The module's path
/// may hold a `#` itself.
fn picked_export_re() -> &'static Re {
    re!(r"^import:(.+)#([A-Za-z_$][\w$]*)$")
}

/// `(module, export)` of a `import:…#…` reference.
fn picked_export_ref(name: &str) -> Option<(String, String)> {
    picked_export_re().captures(name).map(|c| (c.text(1).unwrap_or("").to_string(), c.text(2).unwrap_or("").to_string()))
}

pub struct ReactResolver;

const LANGUAGES: [Language; 4] = [Language::JavaScript, Language::TypeScript, Language::Tsx, Language::Jsx];

impl FrameworkResolver for ReactResolver {
    fn name(&self) -> &str {
        "react"
    }

    // Includes 'tsx'/'jsx' so route extraction runs on JSX files (where `<Route element={<X/>}>`
    // routes live) — without them the .tsx/.jsx grammars were filtered out of the extract pass and
    // those routes were never indexed.
    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        // React in a package.json — the root's, or a workspace's (`frontend/`, `apps/web/`).
        if depends_on(ctx, &["react", "next", "react-native"]) {
            return true;
        }
        // Check for .jsx/.tsx files
        ctx.get_all_files().iter().any(|f| f.ends_with(".jsx") || f.ends_with(".tsx"))
    }

    // A data-router `lazy: () => import('./routes/x')` route names a module, not a symbol, and
    // `import:./x#Page` the export its loader picks; a route's `layout:MainLayout` names the
    // component around it.
    fn claims_reference(&self, name: &str) -> bool {
        name.starts_with(LAZY_ROUTE_PREFIX) || name.starts_with(LAYOUT_PREFIX) || picked_export_re().is_match(name)
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        // Components, hooks and contexts are a script's: halo's Java `import
        // org.springframework…SecurityContext` is no React context.
        if !is_react_script_language(r.language) {
            return None;
        }
        let name = r.reference_name.as_str();
        let found = |target: String, confidence: f64| Some(ResolvedRef::new(r, target, confidence, ResolvedBy::Framework));
        if let Some(spec) = name.strip_prefix(LAZY_ROUTE_PREFIX) {
            return lazy_route_component(spec, &r.file_path, ctx, &mut Reads::new()).and_then(|t| found(t, 0.9));
        }
        // The export a lazy route's loader picks. Vue Router and Angular name a lazily loaded
        // component the same way, and their routes' are theirs.
        if let Some((spec, member)) = picked_export_ref(name) {
            let target = if is_react_route_ref(r) { picked_export(&spec, &member, &r.file_path, ctx, &mut Reads::new()) } else { None };
            return target.and_then(|t| found(t, 0.9));
        }
        // The layout a route renders inside: what happens in it — its header's links, its
        // logout — happens on the route's screen too.
        if let Some(spec) = name.strip_prefix(LAYOUT_PREFIX) {
            let target = layout_component(spec, r, ctx)?;
            let mut resolved = ResolvedRef::new(r, target, 0.9, ResolvedBy::Framework);
            let mut metadata = serde_json::Map::new();
            metadata.insert("layout".to_string(), serde_json::Value::Bool(true));
            resolved.metadata = Some(metadata);
            return Some(resolved);
        }
        // A component, hook or context the file IMPORTS is the import's: the package's
        // (`useQuery` from `@tanstack/react-query`, `<Button>` from a UI kit), or the module the
        // import names, which import resolution finds. Framework resolution runs first, and a
        // name lookup here bound trpc's tests' `useQuery` to a hook nested in one of trpc's own
        // factories.
        if ctx.get_import_mappings(&r.file_path, r.language).iter().any(|m| m.local_name == name) {
            return None;
        }

        // Pattern 1: Component references (PascalCase). Only from JSX-capable files — a component
        // is USED in markup, which only parses in .tsx/.jsx. Without this gate, every PascalCase
        // TYPE reference in plain .ts files went through component resolution: in a monorepo with
        // same-named classes per package (#764, amplication), a `.ts` GraphQL-types file's own
        // `Account` type alias lost to an arbitrary `Account` CLASS in another package (the
        // framework's 0.8 outranked the name-matcher's proximity-correct 0.7).
        if matches!(r.language, Language::Tsx | Language::Jsx) && is_pascal_case(name) && !is_built_in_type(name) {
            // A type annotation is name matching's, which scopes a type as TypeScript does — the
            // file's own, an import's or a global one — and never a component elsewhere. Only a
            // type alias or interface the file declares beside a value or a component of its name
            // is decided here, where both are the file's own: the annotation is the type, as
            // bigbluebutton's `type ModalRegistration` is beside its render-prop component `const
            // ModalRegistration = (…) =>`. (A class is a type and a value at once.)
            let own = own_declarations(name, &r.file_path, ctx);
            if names_type(r) {
                let type_decl = own.iter().find(|n| matches!(n.kind, NodeKind::TypeAlias | NodeKind::Interface));
                return match type_decl {
                    Some(t) if own.iter().any(|n| !matches!(n.kind, NodeKind::TypeAlias | NodeKind::Interface | NodeKind::Class)) => found(t.id.clone(), 0.8),
                    _ => None,
                };
            }
            // Anything else the file declares itself is that declaration, whatever refers to it —
            // a route's `element`, a page's default export — never a same-named component in
            // another app of the repository: the module a `const RegisterPage =
            // Loadable(lazy(() => import('pages/auth/Register')))` loads, the function a value
            // wraps, a function, class or component of that name, or else the declaration, which
            // name matching binds.
            if let Some(value) = declared_component(name, &r.file_path, ctx) {
                return value.component.and_then(|c| found(c.id.clone(), 0.9));
            }
            if let Some(component) = own.iter().find(|n| is_component_kind(n.kind)) {
                return found(component.id.clone(), 0.8);
            }
            if !own.is_empty() {
                return None;
            }
            if let Some(result) = resolve_component(name, &r.file_path, ctx) {
                return found(result, 0.8);
            }
        }

        // Pattern 2: Hook references (use*)
        if name.starts_with("use") && name.len() > 3 {
            if let Some(result) = resolve_hook(name, &r.file_path, ctx, r.language) {
                return found(result, 0.85);
            }
        }

        // Pattern 3: Context references
        if name.ends_with("Context") || name.ends_with("Provider") {
            if let Some(result) = resolve_context(name, r, ctx) {
                return found(result, 0.8);
            }
        }

        None
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<FrameworkExtractionResult> {
        let mut nodes: Vec<Node> = Vec::new();
        let mut references: Vec<UnresolvedRef> = Vec::new();
        let now = now_ms();

        // Components and custom hooks are NOT extracted here. The tree-sitter extractor already
        // emits them natively across .ts/.tsx/.js/.jsx — function and arrow components as
        // `function` nodes, HOC-wrapped components (`forwardRef`/`memo`/`styled`) as `component`
        // nodes (#841), and `useX` hooks as `function` nodes. This `extract` contributes only what
        // tree-sitter can't: route nodes (React Router conventions).

        // Read only each opening tag's own attributes, including expression values.
        for route in scan_route_declarations(content, allows_jsx(file_path)) {
            let line = content.as_bytes()[..route.at.min(content.len())].iter().filter(|b| **b == b'\n').count() as u32 + 1;
            let language = if file_path.ends_with(".tsx") { Language::Tsx } else { Language::Jsx };
            let mut route_node = Node::new(
                format!("route:{file_path}:{line}:{}", route.path),
                NodeKind::Route,
                route.path.clone(),
                format!("{file_path}::route:{}", route.path),
                file_path,
                language,
                line,
                line,
                0,
                0,
                now,
            );
            // A path built from a constant (`paths.app.root.path`) is named in post_extract.
            if route.parts.iter().any(|p| p.expr.is_some()) {
                let parts: Vec<serde_json::Value> = route.parts.iter().map(RoutePart::to_json).collect();
                route_node.signature = Some(format!("{ROUTE_PARTS_PREFIX}{}", serde_json::Value::Array(parts)));
            }
            let id = route_node.id.clone();
            nodes.push(route_node);
            references.extend(tables::route_references(&route, &id, file_path, line));
        }

        // Next.js pages and route handlers are `frameworks/nextjs`'s.
        Some(FrameworkExtractionResult { nodes, references })
    }

    /// Name the routes whose path is built from a constant — bulletproof-react's `path:
    /// paths.app.discussions.path` under `path: paths.app.root.path` is `/app/discussions` — by
    /// reading the constant's object literal where it is declared. Idempotent: the parts ride on
    /// the node's signature.
    fn post_extract(&self, ctx: &Ctx) -> Option<Vec<Node>> {
        let mut updates: Vec<Node> = Vec::new();
        for route in ctx.get_nodes_by_kind(NodeKind::Route).iter() {
            let Some(encoded) = route.signature.as_deref().and_then(|s| s.strip_prefix(ROUTE_PARTS_PREFIX)) else { continue };
            let Ok(serde_json::Value::Array(raw)) = serde_json::from_str::<serde_json::Value>(encoded) else { continue };
            let parts: Vec<RoutePart> = raw.iter().map(RoutePart::from_json).collect();
            let values: Vec<Option<String>> = parts
                .iter()
                .map(|p| p.lit.clone().or_else(|| p.expr.as_ref().and_then(|e| config::constant_path_value(e, &route.file_path, ctx))))
                .collect();
            if values.iter().any(Option::is_none) {
                continue;
            }
            let name = compose_route_path(&values.into_iter().flatten().collect::<Vec<_>>());
            if name != route.name {
                let mut updated = (**route).clone();
                updated.name = name;
                updates.push(updated);
            }
        }
        Some(updates)
    }

    /// The routes of tables another file hands the router (`table_routes`).
    fn cross_file_nodes(&self, ctx: &Ctx) -> Option<CrossFileNodes> {
        let FrameworkExtractionResult { nodes, references } = tables::table_routes(ctx);
        Some(CrossFileNodes { nodes, references, kind: NodeKind::Route, owns: Box::new(is_table_route) })
    }

    /// The modules a route's answer is read from: a lazily loaded page or layout (`lazy-import:`
    /// references, and `import:…#…` for the export a loader picks), or the one a same-file lazy
    /// value the route renders, or renders inside, loads (`declared_component`) — with the modules
    /// a barrel there forwards the lookup to.
    fn lazy_modules(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<Vec<String>> {
        let name = r.reference_name.as_str();
        let picked = if is_react_route_ref(r) { picked_export_ref(name.strip_prefix(LAYOUT_PREFIX).unwrap_or(name)) } else { None };
        if let Some((spec, member)) = picked {
            let mut reads = Reads::new();
            picked_export(&spec, &member, &r.file_path, ctx, &mut reads);
            return Some(reads.into_iter().collect());
        }
        let spec: Option<String> = if let Some(spec) = name.strip_prefix(LAZY_ROUTE_PREFIX) {
            Some(spec.to_string())
        } else if let Some(spec) = name.strip_prefix(LAYOUT_PREFIX).and_then(|rest| rest.strip_prefix(LAZY_ROUTE_PREFIX)) {
            Some(spec.to_string())
        } else if is_react_route_ref(r) {
            // A page or layout the route file declares as a value it loads lazily.
            let own = name.strip_prefix(LAYOUT_PREFIX).unwrap_or(name);
            let loaded = if is_pascal_case(own) && !is_built_in_type(own) { declared_loader(own, &r.file_path, ctx) } else { None };
            if let Some((spec, member)) = &loaded {
                if member != "default" {
                    let mut reads = Reads::new();
                    picked_export(spec, member, &r.file_path, ctx, &mut reads);
                    return Some(reads.into_iter().collect());
                }
            }
            loaded.map(|(spec, _)| spec)
        } else {
            None
        };
        Some(match spec {
            Some(spec) => lazy_route_files(&spec, &r.file_path, ctx),
            None => Vec::new(),
        })
    }
}

pub(crate) fn allows_jsx(file_path: &str) -> bool {
    !re!(r"\.(?:ts|mts|cts)$").is_match(file_path)
}

pub(crate) fn script_language(file_path: &str) -> Language {
    if re!(r"\.tsx$").is_match(file_path) {
        Language::Tsx
    } else if re!(r"\.[cm]?ts$").is_match(file_path) {
        Language::TypeScript
    } else if re!(r"\.jsx$").is_match(file_path) {
        Language::Jsx
    } else {
        Language::JavaScript
    }
}

/// The file a lazily loaded module is, from the file that loads it: by the extensions that file's
/// own imports resolve with (`./MainLayout` is `MainLayout/index.jsx` from a `.jsx` file), else by
/// TypeScript's.
fn lazy_module_file(spec: &str, from_file: &str, ctx: &Ctx) -> Option<String> {
    let language = script_language(from_file);
    resolve_import_path(spec, from_file, language, ctx).or_else(|| {
        if language == Language::TypeScript {
            None
        } else {
            resolve_import_path(spec, from_file, Language::TypeScript, ctx)
        }
    })
}

/// The component a lazy route module renders: its default export, else its `Component` export.
fn lazy_route_component(spec: &str, from_file: &str, ctx: &Ctx, reads: &mut Reads) -> Option<String> {
    let file = lazy_module_file(spec, from_file, ctx)?;
    exported_component(&file, "default", ctx, &IndexSet::new(), reads).or_else(|| exported_component(&file, "Component", ctx, &IndexSet::new(), reads))
}

/// The files `lazy_route_component` reads: the module, and the ones a barrel forwards it to.
fn lazy_route_files(spec: &str, from_file: &str, ctx: &Ctx) -> Vec<String> {
    let mut reads = Reads::new();
    lazy_route_component(spec, from_file, ctx, &mut reads);
    reads.into_iter().collect()
}

/// The component a lazy route's loader picks from the module `spec` loads: the export named
/// `name`, through barrels. `reads` collects the files it reads.
fn picked_export(spec: &str, name: &str, from_file: &str, ctx: &Ctx, reads: &mut Reads) -> Option<String> {
    let file = lazy_module_file(spec, from_file, ctx)?;
    exported_component(&file, name, ctx, &IndexSet::new(), reads)
}

/// True for a reference a React route makes: from a `route:<file>:…` node, in tsx or jsx, the
/// language React gives its routes even in a `.ts` routes file. Vue Router's and Angular's routes
/// are never in either.
fn is_react_route_ref(r: &UnresolvedRef) -> bool {
    matches!(r.language, Language::Tsx | Language::Jsx) && r.from_node_id.starts_with(&format!("route:{}:", r.file_path))
}

/// What a module can export as a component: a declaration, or a value holding one.
fn is_exported_component_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Function | NodeKind::Component | NodeKind::Class | NodeKind::Constant | NodeKind::Variable)
}

/// A barrel can forward another barrel; a few hops settle it.
const MAX_REEXPORT_HOPS: usize = 4;

/// The component `file` exports as `name`: one it declares — `export default Page`, `export
/// function Component`, `export { Page as Component }` — or one it forwards from the module that
/// declares it, as a barrel `index.ts` does: `export { default } from './page'`, `export { Page as
/// Component } from './page'`, `export * from './page'`, which forwards every name but the
/// default, or an import it exports again, `import { Page } from './page'; export { Page }`, or in
/// a value, outline's `const Avatar = AvatarWithHoverCard; export { Avatar }`. As in JavaScript, a
/// name the file exports itself hides one an `export *` would forward, and a name two `export *`
/// modules both forward is exported by neither. `reads` collects every file the lookup reads. A
/// JSX tag imported from the project renders it too (`jsxChild`).
pub fn exported_component(file: &str, name: &str, ctx: &Ctx, visited: &IndexSet<String>, reads: &mut Reads) -> Option<String> {
    if visited.contains(file) || visited.len() > MAX_REEXPORT_HOPS {
        return None;
    }
    reads.insert(file.to_string());
    let language = script_language(file);
    let mut through = visited.clone();
    through.insert(file.to_string());
    let content = ctx.read_file(file);
    let code = codegraph_text::strip_comments_for_regex(content.as_deref().unwrap_or(""), "typescript");
    let local = own_export(&code, name);
    if let Some(local) = local {
        let nodes = ctx.get_nodes_in_file(file);
        let declared_as = |binding: &str| -> Option<NodeRef> {
            let named: Vec<&NodeRef> = nodes.iter().filter(|n| n.name == binding && is_exported_component_kind(n.kind)).collect();
            named.iter().find(|n| n.qualified_name == binding).or(named.first()).map(|n| (*n).clone())
        };
        let exported = declared_as(&local);
        // A constant that holds another binding is that binding: outline's `Avatar/index.ts` is
        // `const Avatar = AvatarWithHoverCard; export { Avatar }`. (A `let` may be assigned
        // another component before it is used.)
        let mut binding = local.clone();
        let mut declared = exported.clone();
        let mut hop = 0;
        while let Some(current) = declared.clone() {
            if hop >= MAX_REEXPORT_HOPS {
                break;
            }
            let held = if current.kind == NodeKind::Constant { held_binding(&current, ctx) } else { None };
            let Some(held) = held else { break };
            declared = declared_as(&held);
            binding = held;
            hop += 1;
        }
        // A value is what it wraps: `export default Page` of `const Page = observer(function
        // Page_() {…})`. So is a component node minted for a wrapper around another component
        // (`const MemoizedBlurhash = memo(Blurhash)`), which has no body to render from.
        if let Some(declared) = &declared {
            let wrapped = match declared.kind {
                NodeKind::Constant | NodeKind::Variable => wrapped_component(declared, ctx),
                NodeKind::Component => handed_component(declared, ctx, None),
                _ => None,
            };
            return Some(wrapped.unwrap_or_else(|| declared.clone()).id.clone());
        }
        // A name the module imports and exports again — `import { Page } from './page'; export {
        // Page }`, `import Page from './Page'; export default Page` — is the page that import
        // names. `export default withAuth(Page)` hands over no binding of its own.
        if binding == local && name == "default" && default_export_binding(&code).as_deref() != Some(local.as_str()) {
            return None;
        }
        let mappings = ctx.get_import_mappings(file, language);
        let imported = mappings.iter().find(|m| m.local_name == binding);
        let id = match imported {
            Some(m) if !m.is_namespace => forwarded(&m.source, if m.is_default { "default" } else { &m.exported_name }, file, language, ctx, &through, reads),
            _ => None,
        };
        // A held binding that leads nowhere leaves the value itself the export.
        return id.or_else(|| exported.map(|e| e.id.clone()));
    }
    let re_exports = ctx.get_re_exports(file, language);
    for re_export in re_exports.iter() {
        if let ReExport::Named { exported_name, original_name, source } = re_export {
            if exported_name == name {
                return forwarded(source, original_name, file, language, ctx, &through, reads);
            }
        }
    }
    if name == "default" {
        return None;
    }
    let mut found: Option<String> = None;
    for re_export in re_exports.iter() {
        let ReExport::Wildcard { source } = re_export else { continue };
        let id = forwarded(source, name, file, language, ctx, &through, reads);
        if let (Some(id), Some(existing)) = (&id, &found) {
            if id != existing {
                return None;
            }
        }
        if found.is_none() {
            found = id;
        }
    }
    found
}

fn forwarded(source: &str, exported: &str, file: &str, language: Language, ctx: &Ctx, through: &IndexSet<String>, reads: &mut Reads) -> Option<String> {
    let target = resolve_import_path(source, file, language, ctx)?;
    exported_component(&target, exported, ctx, through, reads)
}

/// The local name a module's own `export` gives `name`: its default export
/// (`default_exported_name`: `export default Page`, `function Page`, `class Page`,
/// `observer(Page)`), `export function Component`, `export const Component`, `export { Page as
/// Component }`. `None` when it has none.
fn own_export(source: &str, name: &str) -> Option<String> {
    if name == "default" {
        if let Some(declared) = default_exported_name(source) {
            return Some(declared);
        }
    } else if Re::new(&format!(r"\bexport\s+(?:async\s+)?(?:const|let|var|function|class)\s+{}(?![\w$])", escape_regex(name)), "").is_match(source) {
        return Some(name.to_string());
    }
    extract_local_export_aliases(source).into_iter().find(|a| a.exported_name == name).map(|a| a.local_name)
}

/// The name a module's default export carries: `export default Login`, `export default function
/// Login`, `export default class Login`, `export default Login as FC`, or the component a wrapper
/// call hands over — `observer(Login)`, `React.memo(Login)`, `connect(mapState)(Login)`,
/// `withTranslation()(withRouter(Login))`.
fn default_exported_name(source: &str) -> Option<String> {
    let at = re!(r"\bexport\s+default\s+").find(source)?;
    let rest = &source[at.end()..];
    let named = re!(r"^(?:async\s+)?(?:function\s*\*?\s*|class\s+)([A-Za-z_$][\w$]*)")
        .captures(rest)
        .or_else(|| re!(r"^([A-Za-z_$][\w$]*)(?:[ \t]*(?:;|\r?\n|$)|\s+(?:as|satisfies)\s)").captures(rest));
    match named {
        Some(n) => n.text(1).map(String::from),
        None => wrapped_name(&rest[..statement_end(rest, 0, false).min(rest.len())]),
    }
}

/// The component a wrapper call hands over — `observer(Login)`, `React.memo(Login)`,
/// `connect(mapState)(Login)`, `withTranslation()(withRouter(Login))`: its last argument list's
/// lone argument, or a component ahead of the options after it (`React.memo(TableBody, areEqual)`,
/// `withAuthZ(Page, { … })`), past a type assertion on the call (`as typeof Login`, or mastodon's
/// `forwardRef(ComboboxWithRef) as { … }`, which keeps the generics `forwardRef` drops).
fn wrapped_name(expression: &str) -> Option<String> {
    let s = js_trim(expression);
    let callee = re!(r"^[A-Za-z_$][\w$]*(?:\s*\.\s*[A-Za-z_$][\w$]*)*\s*(?:<[^()]*>)?\s*(?=\()").find(s)?;
    let mut end = callee.end();
    let mut args: Vec<String> = Vec::new();
    let mut open = end;
    while s.as_bytes().get(open) == Some(&b'(') {
        let close = match_bracket(s, open)?;
        args = top_level_items(&s[open + 1..close]);
        end = close + 1;
        open = end;
        while s[open.min(s.len())..].chars().next().is_some_and(codegraph_types::jscompat::is_js_space) {
            open += s[open..].chars().next().map_or(1, char::len_utf8);
        }
    }
    if !re!(r"^(?:\s+(?:as|satisfies)\s[\s\S]*)?$").is_match(&s[end..]) {
        return None;
    }
    let first = args.first().cloned().unwrap_or_default();
    // `connect(mapState, mapDispatch)` hands over no component yet.
    if args.len() > 1 && !first.starts_with(|c: char| c.is_ascii_uppercase()) {
        return None;
    }
    if re!(r"^[A-Za-z_$][\w$]*$").is_match(&first) {
        Some(first)
    } else if first.is_empty() {
        None
    } else {
        wrapped_name(&first)
    }
}

/// A value `file_path` declares itself under a component's name, and what it renders. JavaScript
/// scoping makes the name, used anywhere in that file — `<RegisterPage />`, a route's `element:
/// <RegisterPage />` — this declaration, never another file's same-named component: mantis's vite
/// app bound its `RegisterPage`, a `Loadable(lazy(() => import('pages/auth/Register')))`, to the
/// page of that name in the repository's Next.js app. `component` is the component of the module
/// a lazy declaration loads (`value_loader`), or the function the value wraps
/// (`observer(function Login() {…})`); `None` when there is none to find, and the declaration is
/// then the binding. The whole result is `None` when the file declares no such value (a function
/// or class component is found the usual way).
pub struct DeclaredComponent {
    pub declaration: NodeRef,
    pub component: Option<NodeRef>,
}

pub fn declared_component(name: &str, file_path: &str, ctx: &Ctx) -> Option<DeclaredComponent> {
    let declaration = declared_value(name, file_path, ctx)?;
    let Some((spec, member)) = value_loader(&declaration, ctx) else {
        let component = wrapped_component(&declaration, ctx);
        return Some(DeclaredComponent { declaration, component });
    };
    let mut reads = Reads::new();
    let id = if member == "default" {
        lazy_route_component(&spec, file_path, ctx, &mut reads)
    } else {
        picked_export(&spec, &member, file_path, ctx, &mut reads)
    };
    Some(DeclaredComponent { declaration, component: id.and_then(|id| ctx.node_by_id(&id)) })
}

/// The module a value `file_path` declares as `name` loads lazily, and the export its loader
/// picks (`value_loader`).
fn declared_loader(name: &str, file_path: &str, ctx: &Ctx) -> Option<(String, String)> {
    let declaration = declared_value(name, file_path, ctx)?;
    value_loader(&declaration, ctx)
}

/// The modules the values `file_path` declares load lazily, with the barrels the lookups for
/// their components pass through: what a JSX tag naming one of them is drawn from
/// (`declared_component`), so a sync that changes one — gives a page its default export, rewires
/// a barrel — redraws the tag.
pub fn lazy_value_modules(file_path: &str, ctx: &Ctx) -> Vec<String> {
    let mut reads = Reads::new();
    for node in ctx.get_nodes_in_file(file_path).iter() {
        if !matches!(node.kind, NodeKind::Constant | NodeKind::Variable) || node.qualified_name != node.name || !is_pascal_case(&node.name) {
            continue;
        }
        let Some((spec, member)) = value_loader(node, ctx) else { continue };
        if member == "default" {
            lazy_route_component(&spec, file_path, ctx, &mut reads);
        } else {
            picked_export(&spec, &member, file_path, ctx, &mut reads);
        }
    }
    reads.into_iter().collect()
}

/// The module-level `const`, `let` or `var` `file_path` declares as `name`.
fn declared_value(name: &str, file_path: &str, ctx: &Ctx) -> Option<NodeRef> {
    ctx.get_nodes_in_file_named(file_path, name)
        .into_iter()
        .find(|n| matches!(n.kind, NodeKind::Constant | NodeKind::Variable) && n.qualified_name == name)
}

/// The module a value's initializer hands a lazy loader, and the export the loader resolves to —
/// `default` unless it picks one: `lazy(() => import('./x'))`, wrapped or not (`Loadable(lazy(…))`,
/// `loadable(…)`, `dynamic(…, { ssr: false })`, `Loadable({ loader: () => import(…) })`),
/// `dynamic(async () => (await import('./x')).default)`, or a named export (`.then((m) => ({
/// default: m.Chart }))`, `.then((m) => m.Chart)`), in either branch of a conditional (`IS_DEV ?
/// React.lazy(…) : null`). The loader is read by `loader_value`, as a data router's is; one it
/// can't read loads the module its expression's first import names. A function the value wraps is
/// no loader (`withFallback('Dialog', (props) => { … import('mermaid') … })`): what it returns
/// renders, and what it imports is its own business.
fn value_loader(declaration: &Node, ctx: &Ctx) -> Option<(String, String)> {
    let code = codegraph_text::strip_comments_for_regex(&declaration_text(declaration, ctx), "typescript");
    let init = initializer_at(&code, declaration.name.len())?;
    let loader = loader_of(&code[init..])?;
    if let Some(page) = loaded_page(loader_value(&loader, &new_scope(), None).as_ref()) {
        return Some(page);
    }
    let function = function_parts(&loader)?;
    if function.block {
        return None;
    }
    first_import(&function.body).map(|spec| (spec, "default".to_string()))
}

/// The loader function an initializer hands a lazy wrapper: the first function among its call's
/// arguments, through the wrappers around it (`Loadable(lazy(() => import('./x')))`) and before
/// the options after it (`dynamic(…, { ssr: false })`), or an options object's `loader`
/// (`Loadable({ loader: () => import(…) })`). `None` for an initializer that is no such call.
fn loader_of(text: &str) -> Option<String> {
    let s = js_trim(text);
    let Some(call) = re!(r"^[A-Za-z_$][\w$]*(?:\s*\.\s*[A-Za-z_$][\w$]*)*\s*(?:<[^()]*>)?\s*\(").find(s) else {
        return conditional_loader(s);
    };
    let open = call.end() - 1;
    let close = match_bracket(s, open)?;
    for arg in top_level_items(&s[open + 1..close]) {
        let item = js_trim(&arg);
        if item.is_empty() {
            continue;
        }
        if function_parts(item).is_some() {
            return Some(item.to_string());
        }
        if item.starts_with('{') {
            let loader = if match_bracket(item, 0) == Some(item.len() - 1) { object_entries(&item[1..item.len() - 1]).0.get("loader").cloned() } else { None };
            if let Some(loader) = loader.filter(|l| function_parts(l).is_some()) {
                return Some(loader);
            }
            continue;
        }
        if let Some(inner) = loader_of(item) {
            return Some(inner);
        }
    }
    conditional_loader(s)
}

/// The loader of a conditional's one branch that has one: SigNoz's `IS_DEV ? React.lazy(() =>
/// import(…)) : null`. `None` when neither branch loads, or both do — the value is then one or
/// the other.
fn conditional_loader(text: &str) -> Option<String> {
    let (yes, no) = conditional_branches(text)?;
    let mut loaders: Vec<String> = [yes, no].iter().filter_map(|branch| loader_of(branch)).collect();
    if loaders.len() == 1 {
        loaders.pop()
    } else {
        None
    }
}

/// The two branches of a conditional expression, `test ? yes : no`; `None` for any other expression.
fn conditional_branches(text: &str) -> Option<(String, String)> {
    let b = text.as_bytes();
    let mut question = 0usize;
    let mut open = 0usize;
    let mut i = 0;
    while i < b.len() {
        let ch = b[i];
        if matches!(ch, b'"' | b'\'' | b'`' | b'{' | b'(' | b'[') {
            i = if matches!(ch, b'{' | b'(' | b'[') { match_bracket(text, i)? } else { skip_string(text, i)? };
        } else if ch == b'?' {
            // `??` and `?.` are no conditional.
            if b.get(i + 1) == Some(&b'?') || (b.get(i + 1) == Some(&b'.') && !b.get(i + 2).is_some_and(|c| c.is_ascii_digit())) {
                i += 1;
            } else {
                if open == 0 {
                    question = i;
                }
                open += 1;
            }
        } else if ch == b':' && open > 0 {
            open -= 1;
            if open == 0 {
                return Some((text[question + 1..i].to_string(), text[i + 1..].to_string()));
            }
        }
        i += 1;
    }
    None
}

/// A declarator's own source text: `B = lazy(…)` in `const A = lazy(…), B = lazy(…)`.
fn declaration_text(node: &Node, ctx: &Ctx) -> String {
    let all: Rc<Vec<String>> = match ctx.get_file_lines(&node.file_path) {
        Some(lines) => lines,
        None => Rc::new(ctx.read_file(&node.file_path).map(|c| split_lines(&c)).unwrap_or_default()),
    };
    let from = (node.start_line as usize).saturating_sub(1).min(all.len());
    let to = (node.end_line as usize).min(all.len()).max(from);
    let mut lines: Vec<String> = all[from..to].to_vec();
    if lines.is_empty() {
        return String::new();
    }
    if node.end_column > 0 {
        let last = lines.len() - 1;
        lines[last] = utf16_prefix(&lines[last], node.end_column as usize).to_string();
    }
    lines[0] = utf16_from(&lines[0], node.start_column as usize).to_string();
    lines.join("\n")
}

/// The component a value wraps, which is what it renders: the function it holds (`const
/// Application = observer(function Application() {…})`), or the component of its own file a
/// wrapper call is handed (`const TableView = observer(TableViewInner)`). An inner function counts
/// when it has the value's name or ends where the value does — never a helper inside an arrow the
/// graph has no node for (`withFallback('X', () => { const load = () => …; … })`).
fn wrapped_component(value: &NodeRef, ctx: &Ctx) -> Option<NodeRef> {
    let nodes = ctx.get_nodes_in_file(&value.file_path);
    let inner: Vec<&NodeRef> = nodes
        .iter()
        .filter(|n| {
            is_component_kind(n.kind)
                && n.qualified_name == format!("{}::{}", value.qualified_name, n.name)
                && n.start_line >= value.start_line
                && n.end_line <= value.end_line
        })
        .collect();
    let held = inner
        .iter()
        .find(|n| n.name == value.name)
        .map(|n| (*n).clone())
        .or_else(|| if inner.len() == 1 && inner[0].end_line == value.end_line { Some(inner[0].clone()) } else { None });
    if held.is_some() || !inner.is_empty() {
        return held;
    }
    handed_component(value, ctx, Some(&nodes))
}

/// The binding a value holds and nothing more — `AvatarWithHoverCard` of outline's `const Avatar =
/// AvatarWithHoverCard`, past a type assertion (`= Page as FC`) — or `None` for any other initializer.
fn held_binding(value: &NodeRef, ctx: &Ctx) -> Option<String> {
    let code = codegraph_text::strip_comments_for_regex(&declaration_text(value, ctx), "typescript");
    let init = initializer_at(&code, value.name.len())?;
    let end = statement_end(&code, init, false);
    let held = re!(r"^\s*([A-Za-z_$][\w$]*)\s*(?:$|(?:as|satisfies)\s)").captures(&code[init..end.max(init)])?;
    let name = held.text(1)?;
    if name != value.name {
        Some(name.to_string())
    } else {
        None
    }
}

/// The component of its own file a wrapper call around a declaration hands over: `const TableView
/// = observer(TableViewInner)`, or mastodon's `const MemoizedBlurhash = memo(Blurhash)`, whose
/// component node has no body of its own to render from.
fn handed_component(value: &NodeRef, ctx: &Ctx, nodes: Option<&Rc<Vec<NodeRef>>>) -> Option<NodeRef> {
    let code = codegraph_text::strip_comments_for_regex(&declaration_text(value, ctx), "typescript");
    let init = initializer_at(&code, value.name.len())?;
    let end = statement_end(&code, init, false);
    let handed = wrapped_name(&code[init..end.max(init)])?;
    if !is_pascal_case(&handed) || handed == value.name {
        return None;
    }
    let fetched;
    let nodes = match nodes {
        Some(n) => n,
        None => {
            fetched = ctx.get_nodes_in_file(&value.file_path);
            &fetched
        }
    };
    nodes.iter().find(|n| n.name == handed && n.qualified_name == handed && is_component_kind(n.kind)).cloned()
}

/// The name of the component a wrapper component's declaration hands its rendering to: what
/// `styled(…)` is given (outline's `styled(ActionButton)`, whatever `.attrs(…)`, `.withConfig(…)`,
/// type arguments or template follow), or what a `memo` or `forwardRef` call wraps (`memo(Button)`,
/// `React.memo(Row, areEqual)`, `memo(forwardRef(Input))`; `wrapped_name`). React renders it
/// whenever it renders the wrapper, so a JSX tag's render flow goes on to it. `None` for a wrapper
/// of an element (`styled.div`, `styled('a')`) and for one around an inline render function, which
/// has a body of its own.
pub fn wrapped_component_name(component: &Node, ctx: &Ctx) -> Option<String> {
    let code = codegraph_text::strip_comments_for_regex(&declaration_text(component, ctx), "typescript");
    let init = initializer_at(&code, component.name.len())?;
    let end = statement_end(&code, init, false);
    let expression = js_trim(&code[init..end.max(init)]);
    let name = if let Some(styled) = re!(r"^styled\s*\(\s*([A-Za-z_$][\w$]*)\s*[,)]").captures(expression) {
        styled.text(1).map(String::from)
    } else if re!(r"^(?:React\s*\.\s*)?(?:memo|forwardRef)\b").is_match(expression) {
        wrapped_name(expression)
    } else {
        None
    };
    name.filter(|n| is_pascal_case(n) && n != &component.name)
}

/// The component a route's `layout:` reference names, found the way the route's own component is:
/// through the route file's import of it, else declared in reach — or a lazily loaded layout
/// module's component, or the export its loader picks.
fn layout_component(spec: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    if let Some(module) = spec.strip_prefix(LAZY_ROUTE_PREFIX) {
        return lazy_route_component(module, &r.file_path, ctx, &mut Reads::new());
    }
    if is_react_route_ref(r) {
        if let Some((module, member)) = picked_export_ref(spec) {
            return picked_export(&module, &member, &r.file_path, ctx, &mut Reads::new());
        }
    }
    let mappings = ctx.get_import_mappings(&r.file_path, r.language);
    let Some(mapping) = mappings.iter().find(|m| m.local_name == spec) else {
        // A layout the route file declares itself (`const MainLayout = Loadable(lazy(…))`): no
        // name match follows a `layout:` reference, so the declaration binds here when there is
        // no component to find.
        if let Some(own) = declared_component(spec, &r.file_path, ctx) {
            return Some(own.component.unwrap_or(own.declaration).id.clone());
        }
        return resolve_component(spec, &r.file_path, ctx);
    };
    let mut named = r.clone();
    named.reference_name = spec.to_string();
    if let Some(via_import) = resolve_import(&named, ctx) {
        return Some(via_import.target_node_id);
    }
    // `import { default as AppRoot } from './routes/app/root'`: read the module the import names —
    // never a same-named component somewhere else.
    if mapping.is_default || mapping.exported_name == "default" {
        return lazy_route_component(&mapping.source, &r.file_path, ctx, &mut Reads::new());
    }
    let file = resolve_import_path(&mapping.source, &r.file_path, r.language, ctx)?;
    ctx.get_nodes_in_file(&file).iter().find(|n| n.name == mapping.exported_name && is_component_kind(n.kind)).map(|n| n.id.clone())
}

/// Check if string is PascalCase
fn is_pascal_case(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase()) && chars.all(|c| c.is_ascii_alphanumeric())
}

/// Check if name is a built-in type
fn is_built_in_type(name: &str) -> bool {
    matches!(
        name,
        "Array"
            | "Boolean"
            | "Date"
            | "Error"
            | "Function"
            | "JSON"
            | "Math"
            | "Number"
            | "Object"
            | "Promise"
            | "RegExp"
            | "String"
            | "Symbol"
            | "Map"
            | "Set"
            | "WeakMap"
            | "WeakSet"
            | "React"
            | "Component"
            | "Fragment"
            | "Suspense"
            | "StrictMode"
    )
}

fn is_component_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Component | NodeKind::Function | NodeKind::Class)
}

/// What `file_path` declares at its top level under `name`: a type alias, an interface, a class,
/// an enum, a function, a component or a value. A declaration nested in another
/// (`EmojiMenu::renderMenuItem`) carries its owner in its qualified name; an import is no
/// declaration.
fn own_declarations(name: &str, file_path: &str, ctx: &Ctx) -> Vec<NodeRef> {
    ctx.get_nodes_in_file_named(file_path, name)
        .into_iter()
        .filter(|n| n.qualified_name == name && !matches!(n.kind, NodeKind::Import | NodeKind::Export | NodeKind::File))
        .collect()
}

/// Whether a reference is written where TypeScript reads a type: what a class implements, or a
/// type annotation's name — the extractor writes a `references` ref only for a name in a type, and
/// a route's reference to what it renders comes from the router's own extraction.
fn names_type(r: &UnresolvedRef) -> bool {
    r.reference_kind == ReferenceKind::Edge(EdgeKind::Implements) || (r.reference_kind == ReferenceKind::Edge(EdgeKind::References) && !r.from_node_id.starts_with("route:"))
}

/// Resolve a component reference using name-based lookup
fn resolve_component(name: &str, from_file: &str, ctx: &Ctx) -> Option<String> {
    let candidates = ctx.get_nodes_by_name(name);
    let components: Vec<&NodeRef> = candidates.iter().filter(|n| is_component_kind(n.kind)).collect();
    if components.is_empty() {
        return None;
    }

    // Prefer the referencing file's folder, or one below it
    if let Some(same_dir) = in_referencing_folder(from_file, components.iter(), |n| n.file_path.as_str()).next() {
        return Some(same_dir.id.clone());
    }

    // Prefer component directories
    const COMPONENT_DIRS: [&str; 7] = ["/components/", "/src/components/", "/app/components/", "/pages/", "/src/pages/", "/views/", "/src/views/"];
    if let Some(preferred) = components.iter().find(|n| COMPONENT_DIRS.iter().any(|d| n.file_path.contains(d))) {
        return Some(preferred.id.clone());
    }

    // No positional signal: only an UNAMBIGUOUS name may resolve. Returning components[0] here
    // picked an arbitrary same-named class anywhere in the repo (#764) — let the name-matcher's
    // proximity scoring decide instead.
    if components.len() == 1 {
        Some(components[0].id.clone())
    } else {
        None
    }
}

/// JS/TS (and their JSX dialects): modules where a cross-file name needs an import.
fn is_esm_language(language: Language) -> bool {
    matches!(language, Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx)
}

/// Resolve a custom hook reference using name-based lookup
fn resolve_hook(name: &str, from_file: &str, ctx: &Ctx, language: Language) -> Option<String> {
    let candidates = ctx.get_nodes_by_name(name);
    // A hook nested inside another function is only callable in there.
    let nested = |n: &Node| -> bool {
        ctx.get_nodes_in_file(&n.file_path).iter().any(|f| {
            f.id != n.id
                && matches!(f.kind, NodeKind::Function | NodeKind::Method)
                && f.start_line <= n.start_line
                && f.end_line >= n.end_line
                && (f.start_line < n.start_line || f.end_line > n.end_line)
        })
    };
    let hooks: Vec<&NodeRef> = candidates.iter().filter(|n| n.kind == NodeKind::Function && n.name.starts_with("use") && !nested(n)).collect();
    if hooks.is_empty() {
        return None;
    }
    if let Some(same_file) = hooks.iter().find(|n| n.file_path == from_file) {
        return Some(same_file.id.clone());
    }
    // A JS/TS module reaches another file's hook only by importing it — the import resolver's to
    // follow (an imported name never gets here) — never by name alone.
    if is_esm_language(language) {
        return None;
    }

    // Prefer hooks directories
    const HOOK_DIRS: [&str; 4] = ["/hooks/", "/src/hooks/", "/lib/hooks/", "/utils/hooks/"];
    if let Some(preferred) = hooks.iter().find(|n| HOOK_DIRS.iter().any(|d| n.file_path.contains(d))) {
        return Some(preferred.id.clone());
    }

    Some(hooks[0].id.clone())
}

/// Resolve a context reference using name-based lookup
fn resolve_context(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    // In a JS/TS module only the file's own context is in reach by name; another file's comes
    // through an import (trpc's adapters' `createContext?.(…)` is an option, not an example app's
    // `createContext`).
    if is_esm_language(r.language) {
        return ctx.get_nodes_by_name(name).iter().find(|n| n.file_path == r.file_path).map(|n| n.id.clone());
    }
    let candidates = ctx.get_nodes_by_name(name);
    if candidates.is_empty() {
        // Try without Context/Provider suffix
        let base_name = name.strip_suffix("Context").or_else(|| name.strip_suffix("Provider")).unwrap_or(name);
        if base_name != name {
            return ctx.get_nodes_by_name(base_name).first().map(|n| n.id.clone());
        }
        return None;
    }

    // Prefer context directories
    const CONTEXT_DIRS: [&str; 6] = ["/context/", "/contexts/", "/src/context/", "/src/contexts/", "/providers/", "/src/providers/"];
    if let Some(preferred) = candidates.iter().find(|n| CONTEXT_DIRS.iter().any(|d| n.file_path.contains(d))) {
        return Some(preferred.id.clone());
    }

    Some(candidates[0].id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_export_names() {
        assert_eq!(default_exported_name("export default Login;").as_deref(), Some("Login"));
        assert_eq!(default_exported_name("export default function Login() {}").as_deref(), Some("Login"));
        assert_eq!(default_exported_name("export default observer(Login);").as_deref(), Some("Login"));
        assert_eq!(default_exported_name("export default connect(mapState)(Login);").as_deref(), Some("Login"));
        assert_eq!(default_exported_name("export default () => null;"), None);
    }

    #[test]
    fn wrapped_names_skip_option_arguments() {
        assert_eq!(wrapped_name("React.memo(TableBody, areEqual)").as_deref(), Some("TableBody"));
        assert_eq!(wrapped_name("connect(mapState, mapDispatch)"), None);
        assert_eq!(wrapped_name("forwardRef(Input) as typeof Input").as_deref(), Some("Input"));
    }

    #[test]
    fn conditional_branches_split_at_the_colon() {
        let (yes, no) = conditional_branches("IS_DEV ? React.lazy(() => import('./x')) : null").unwrap();
        assert_eq!(js_trim(&yes), "React.lazy(() => import('./x'))");
        assert_eq!(js_trim(&no), "null");
        assert!(conditional_branches("a ?? b").is_none());
    }

    #[test]
    fn pascal_case_names() {
        assert!(is_pascal_case("Page2"));
        assert!(!is_pascal_case("page"));
        assert!(!is_pascal_case("Page_One"));
    }
}
