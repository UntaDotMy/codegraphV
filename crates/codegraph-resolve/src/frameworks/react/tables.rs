//! Routes written in a table that another file hands to the router. Port of the "Route tables
//! another file hands the router" half of `src/resolution/frameworks/react.ts`.
//!
//! The ASP.NET Core React template keeps them in `AppRoutes.js` — `[{ index: true, element: <Home
//! /> }, { path: '/counter', element: <Counter /> }, …]` — and renders them from `App.js` with
//! `AppRoutes.map(({ element, ...rest }) => <Route {...rest} element={element} />)`; codedthemes'
//! admin templates write one route object per file and pass `[MainRoutes, LoginRoutes]` to
//! `createBrowserRouter`; many apps import `routes` into `useRoutes(routes)`. The table's own file
//! never names the router, so its extraction cannot tell it from a menu's `{ path, element }`
//! list. The evidence is the file that hands it over: a `useRoutes` or `create*Router` argument,
//! or a `.map` whose callback renders a `<Route>` from the item's own fields. From there the
//! import leads to the table, which is read the way a data router's is, and the tables it names
//! in turn (`...ApiAuthorizationRoutes`, `children: adminRoutes`) are read under the path they sit
//! at — and inside the layouts around them when the table is written in the same file. A file
//! that makes a router itself had its route objects read when it was extracted; only the tables
//! it names are new here.

use super::config::constant_path_value;
use super::scan::{compose_route_path, router_call_re, scan_routes, RouteDeclaration, RoutePart, ScanOptions, TableName, TableScope};
use super::{allows_jsx, script_language, LAYOUT_PREFIX};
use crate::ctx::Ctx;
use crate::frameworks::now_ms;
use crate::frameworks::object_literal::{byte_at, ident_end, match_bracket, skip_string, space_len_at};
use crate::import_resolver::{python::js_cmp, resolve_import_path};
use crate::synth_utils::{is_fn_kind, make_line_at};
use crate::types::{FrameworkExtractionResult, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_search::query_utils::is_test_path;
use codegraph_types::jscompat::utf16_byte_index;
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use indexmap::IndexSet;
use std::collections::{HashSet, VecDeque};

/// The packages React Router's components and hooks are imported from.
const ROUTER_PACKAGES: [&str; 3] = ["react-router-dom", "react-router", "react-router-native"];

/// At most this many tables are read from one project's consumers — a bound, not a budget anyone reaches.
const MAX_TABLES: usize = 512;

/// True for a route the table pass made — `route:<file>:<line>:table:<path>` — and for no route
/// extraction makes.
pub fn is_table_route(node: &Node) -> bool {
    node.kind == NodeKind::Route && node.id.starts_with(&format!("route:{}:{}:table:", node.file_path, node.start_line))
}

/// The references that bind a route: what it renders, and the layouts around it.
pub fn route_references(route: &RouteDeclaration, from_node_id: &str, file_path: &str, line: u32) -> Vec<UnresolvedRef> {
    let language = if file_path.ends_with(".tsx") { Language::Tsx } else { Language::Jsx };
    let reference = |name: String| UnresolvedRef::new(from_node_id, name, EdgeKind::References, line, 0, file_path, language);
    let mut refs: Vec<UnresolvedRef> = Vec::new();
    if let Some(target) = route.component.as_ref().or(route.lazy.as_ref()) {
        refs.push(reference(target.clone()));
    }
    for layout in &route.layouts {
        refs.push(reference(format!("{LAYOUT_PREFIX}{layout}")));
    }
    refs
}

/// A table literal: the file it is written in, and where it opens.
#[derive(Clone)]
struct TableLiteral {
    file: String,
    at: usize,
}

/// A table to read: where its literal opens in the file at hand, or a function whose returned
/// tables to read.
#[derive(Clone)]
enum TableSite {
    Literal(usize),
    Named(TableName),
}

struct Queued {
    file: String,
    at: usize,
    prefix: Vec<String>,
    layouts: Vec<String>,
}

struct Queue {
    items: VecDeque<Queued>,
    queued: HashSet<String>,
}

impl Queue {
    /// A table renders inside the layouts around the place that names it. A route's `layout:`
    /// reference is resolved in the route's own file, so they go along only when that place is in
    /// the table's file.
    fn enqueue(&mut self, table: Option<TableLiteral>, prefix: &[String], from: &str, layouts: &[String]) {
        let Some(table) = table else { return };
        if self.queued.len() >= MAX_TABLES {
            return;
        }
        let key = format!("{}\0{}\0{}", table.file, table.at, prefix.join("\0"));
        if !self.queued.insert(key) {
            return;
        }
        let layouts = if table.file == from { layouts.to_vec() } else { Vec::new() };
        self.items.push_back(Queued { file: table.file, at: table.at, prefix: prefix.to_vec(), layouts });
    }
}

/// Routes written in a table that another file hands to the router.
pub fn table_routes(ctx: &Ctx) -> FrameworkExtractionResult {
    let mut nodes: Vec<Node> = Vec::new();
    let mut references: Vec<UnresolvedRef> = Vec::new();
    let mut consumers: IndexSet<String> = IndexSet::new();
    for pkg in ROUTER_PACKAGES {
        for n in ctx.get_nodes_by_name(pkg).iter() {
            if n.kind == NodeKind::Import && !is_test_path(&n.file_path) {
                consumers.insert(n.file_path.clone());
            }
        }
    }
    if consumers.is_empty() {
        return FrameworkExtractionResult { nodes, references };
    }
    let mut queue = Queue { items: VecDeque::new(), queued: HashSet::new() };
    let mut sorted: Vec<&String> = consumers.iter().collect();
    sorted.sort_by(|a, b| js_cmp(a, b));
    for file in sorted {
        let Some(source) = ctx.read_file(file) else { continue };
        // Most of the files that import React Router only link or navigate.
        if !re!(r"\buseRoutes\b|\bcreate(?:Browser|Hash|Memory)Router\b|<Route\b").is_match(&source) {
            continue;
        }
        let scan = scan_routes(&source, allows_jsx(file), ScanOptions { objects: router_call_re().is_match(&source), uses: true, ..Default::default() });
        for site in &scan.uses {
            let tables = match (site.literal, &site.name) {
                (Some(at), _) => vec![TableLiteral { file: file.clone(), at }],
                (None, Some(name)) => table_named(name, file, ctx, 0),
                (None, None) => Vec::new(),
            };
            let prefix = path_values(&site.prefix, file, ctx);
            for table in tables {
                queue.enqueue(Some(table), &prefix, file, &site.layouts);
            }
        }
    }
    let mut made: HashSet<String> = HashSet::new();
    let now = now_ms();
    while let Some(next) = queue.items.pop_front() {
        let Queued { file, at, prefix, layouts } = next;
        let Some(source) = ctx.read_file(&file) else { continue };
        let scan = scan_routes(
            &source,
            allows_jsx(&file),
            ScanOptions { table: Some(TableScope { at, prefix: prefix.iter().map(|lit| RoutePart::lit(lit.clone())).collect(), layouts }), ..Default::default() },
        );
        for named in &scan.names {
            let prefix = path_values(&named.prefix, &file, ctx);
            for table in table_named(&named.table, &file, ctx, 0) {
                queue.enqueue(Some(table), &prefix, &file, &named.layouts);
            }
        }
        if router_call_re().is_match(&source) {
            continue;
        }
        let line_at = make_line_at(&source, 1);
        for route in &scan.routes {
            let line = line_at.at(route.at);
            let id = format!("route:{file}:{line}:table:{}", route.path);
            if !made.insert(id.clone()) {
                continue;
            }
            let values: Vec<Option<String>> = route
                .parts
                .iter()
                .map(|p| p.lit.clone().or_else(|| p.expr.as_ref().and_then(|e| constant_path_value(e, &file, ctx))))
                .collect();
            let name = if values.iter().all(Option::is_some) {
                compose_route_path(&values.into_iter().flatten().collect::<Vec<_>>())
            } else {
                route.path.clone()
            };
            let language = if file.ends_with(".tsx") { Language::Tsx } else { Language::Jsx };
            nodes.push(Node::new(id.clone(), NodeKind::Route, name, format!("{file}::route:{}", route.path), file.clone(), language, line, line, 0, 0, now));
            references.extend(route_references(route, &id, &file, line));
        }
    }
    FrameworkExtractionResult { nodes, references }
}

/// Path parts as text: a constant read from where it is declared, or shown as `{expr}` when it
/// cannot be.
pub fn path_values(parts: &[RoutePart], from_file: &str, ctx: &Ctx) -> Vec<String> {
    parts
        .iter()
        .map(|p| {
            p.lit
                .clone()
                .or_else(|| p.expr.as_ref().and_then(|e| constant_path_value(e, from_file, ctx)))
                .unwrap_or_else(|| format!("{{{}}}", p.expr.clone().unwrap_or_default()))
        })
        .collect()
}

/// The literals a table name stands for in `file`: declared there, or imported. A value that
/// holds a call's result — Medusa's `const routes = getRouteMap({ … })` — stands for the tables
/// the function returns.
fn table_named(named: &TableName, file: &str, ctx: &Ctx, depth: usize) -> Vec<TableLiteral> {
    let Some(source) = ctx.read_file(file) else { return Vec::new() };
    let safe = codegraph_text::strip_comments_for_regex(&source, "typescript");
    let visible = visible_at(Some(named.at), &safe, file, ctx);
    let local = declared_table(&safe, &named.name, named.call, &visible);
    if !local.is_empty() {
        return tables_at(&local, file, ctx, depth);
    }
    let language = script_language(file);
    let mappings = ctx.get_import_mappings(file, language);
    let Some(mapping) = mappings.iter().find(|m| m.local_name == named.name) else { return Vec::new() };
    if mapping.is_namespace || depth > 3 {
        return Vec::new();
    }
    let Some(target) = resolve_import_path(&mapping.source, file, language, ctx) else { return Vec::new() };
    exported_table(&target, if mapping.is_default { "default" } else { &mapping.exported_name }, named.call, ctx, depth + 1)
}

/// The literals `sites` in `file` stand for: each literal written there, and the tables each
/// function named returns.
fn tables_at(sites: &[TableSite], file: &str, ctx: &Ctx, depth: usize) -> Vec<TableLiteral> {
    sites
        .iter()
        .flat_map(|site| match site {
            TableSite::Literal(at) => vec![TableLiteral { file: file.to_string(), at: *at }],
            TableSite::Named(_) if depth > 3 => Vec::new(),
            TableSite::Named(name) => table_named(name, file, ctx, depth + 1),
        })
        .collect()
}

/// The literals `file` exports as `exported`: `export default [ … ]`, `export default AppRoutes`,
/// `export const routes = [ … ]`, re-exports followed.
fn exported_table(file: &str, exported: &str, call: bool, ctx: &Ctx, depth: usize) -> Vec<TableLiteral> {
    let Some(source) = ctx.read_file(file) else { return Vec::new() };
    let safe = codegraph_text::strip_comments_for_regex(&source, "typescript");
    let mut local = exported.to_string();
    if exported == "default" {
        if let Some(m) = re!(r"\bexport\s+default\s+").find(&safe) {
            let after = m.end();
            // `export default [ … ]`, `export default buildRoutes()`, `export default function routes() { … }`.
            let inline = if call { returned_tables(&safe, after) } else { held_table(&safe, after) };
            if !inline.is_empty() {
                return tables_at(&inline, file, ctx, depth);
            }
            local = re!(r"^([A-Za-z_$][\w$]*)\s*(?:;|\n|$)").captures(&safe[after..]).and_then(|c| c.text(1).map(String::from)).unwrap_or_default();
        } else {
            local = re!(r"\bexport\s*\{[^}]*?\b([A-Za-z_$][\w$]*)\s+as\s+default\b").captures(&safe).and_then(|c| c.text(1).map(String::from)).unwrap_or_default();
        }
    } else {
        // `export { appRoutes as routes }`
        let alias = Re::new(&format!(r"\bexport\s*\{{[^}}]*?\b([A-Za-z_$][\w$]*)\s+as\s+{}\b", escape_regex(exported)), "");
        if let Some(a) = alias.captures(&safe) {
            local = a.text(1).unwrap_or("").to_string();
        }
    }
    // What a module exports is declared at its top level.
    let declared = if local.is_empty() { Vec::new() } else { declared_table(&safe, &local, call, &visible_at(None, &safe, file, ctx)) };
    if !declared.is_empty() {
        return tables_at(&declared, file, ctx, depth);
    }
    if depth > 3 {
        return Vec::new();
    }
    // `export { routes } from './routes'`, `export { default as routes } from './routes'`, `export * from './routes'`.
    let language = script_language(file);
    for re_export in ctx.get_re_exports(file, language).iter() {
        use crate::types::ReExport;
        let (source_spec, original) = match re_export {
            ReExport::Named { exported_name, original_name, source } => {
                if exported_name != exported {
                    continue;
                }
                (source, original_name.as_str())
            }
            ReExport::Wildcard { source } => (source, exported),
            ReExport::Namespace { source, .. } => (source, exported),
        };
        let Some(target) = resolve_import_path(source_spec, file, language, ctx) else { continue };
        let found = exported_table(&target, original, call, ctx, depth + 1);
        if !found.is_empty() {
            return found;
        }
    }
    Vec::new()
}

/// Picks, of a name's declarations in the file (where each keyword sits), the one in scope at the use.
type Visible<'a> = Box<dyn Fn(&[usize]) -> Option<usize> + 'a>;

/// Where `name`'s table literal opens, for `const name = [ … ]` (or the tables a function returns
/// when `call`) — or, for `const name = getRouteMap({ … })`, the function whose returned table
/// the value holds. Of several declarations of the name, the one `visible` picks.
fn declared_table(safe: &str, name: &str, call: bool, visible: &Visible) -> Vec<TableSite> {
    let id = escape_regex(name);
    let decl_re = Re::new(&format!(r"(?:^|[^\w$.])((?:const|let|var)\s+{id}\s*(?::[^=;]*)?=\s*)"), "g");
    let decls: Vec<(usize, usize)> = decl_re
        .captures_iter(safe)
        .filter_map(|d| Some((d.get(1)?.start(), d.get(0)?.end())))
        .collect();
    let chosen = if decls.is_empty() { None } else { visible(&decls.iter().map(|d| d.0).collect::<Vec<_>>()) };
    if let Some(&(_, after)) = chosen.and_then(|c| decls.get(c)) {
        return if call { returned_tables(safe, after) } else { held_table(safe, after) };
    }
    if !call {
        return Vec::new();
    }
    let func = Re::new(&format!(r"(?:^|[^\w$.])function\s+{id}\s*\("), "").captures(safe);
    match func.and_then(|f| f.text(0).map(|t| (f.get(0).map_or(0, |m| m.start()), t.find("function").unwrap_or(0)))) {
        Some((start, offset)) => returned_tables(safe, start + offset),
        None => Vec::new(),
    }
}

/// The tables a value written at `at` gives: the literal there, or the function a call there
/// names — `getRouteMap({ … })`, `await loadRoutes()` — whose returned table it is, and through
/// `useMemo` what its callback returns: `useMemo(() => getRoutes(user), [user])`,
/// `useMemo(getRoutes, [])`.
fn held_table(safe: &str, mut at: usize) -> Vec<TableSite> {
    // Parentheses around the value change nothing: `return ([ … ])`.
    while byte_at(safe, at) == b'(' {
        at = skip_space(safe, at + 1);
    }
    if matches!(byte_at(safe, at), b'[' | b'{') {
        return vec![TableSite::Literal(at)];
    }
    if let Some(memo) = re!(r"(?:React\s*\.\s*)?useMemo\s*(?:<[^()]*>)?\s*\(\s*", "y").captures_at(safe, at) {
        let memo_end = memo.get(0).map_or(at, |m| m.end());
        return match re!(r"([A-Za-z_$][\w$]*)\s*,", "y").captures_at(safe, memo_end) {
            Some(handed) => vec![TableSite::Named(TableName { name: handed.text(1).unwrap_or("").to_string(), call: true, at: memo_end })],
            None => returned_tables(safe, memo_end),
        };
    }
    match re!(r"(await\s+)?([A-Za-z_$][\w$]*)\s*\(", "y").captures_at(safe, at) {
        Some(m) => vec![TableSite::Named(TableName { name: m.text(2).unwrap_or("").to_string(), call: true, at: at + m.text(1).map_or(0, str::len) })],
        None => Vec::new(),
    }
}

/// Picks, of a name's declarations in `file` (where each keyword sits), the one in scope at
/// `use_at`: in the innermost function around the use that declares the name, the last declaration
/// before the use, else the first after it; else one at the module's top level. A declaration
/// inside another function is that function's own. With no use, it picks one at the top level,
/// which is what a module exports. The functions are the graph's — a component, a method, an
/// arrow function held in a `const` — so the blocks inside one function are not told apart.
fn visible_at<'a>(use_at: Option<usize>, safe: &'a str, file: &'a str, ctx: &'a Ctx) -> Visible<'a> {
    Box::new(move |keywords: &[usize]| {
        let mut line_starts: Vec<usize> = vec![0];
        for (i, b) in safe.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        let offset = |line: u32, column: u32| -> usize {
            let Some(&start) = line_starts.get((line as usize).saturating_sub(1)) else { return safe.len() + column as usize };
            let end = safe[start..].find('\n').map_or(safe.len(), |e| start + e);
            start + utf16_byte_index(&safe[start..end], column as usize)
        };
        let fns: Vec<(usize, usize)> = ctx
            .get_nodes_in_file(file)
            .iter()
            .filter(|n| is_fn_kind(n.kind))
            .map(|n| (offset(n.start_line, n.start_column), offset(n.end_line, n.end_column)))
            .collect();
        // The function a declaration's own value is (`const routes = (user) => [ … ]`) starts
        // after the keyword, so it is not around it.
        let around = |at: usize| -> Vec<usize> { (0..fns.len()).filter(|&f| fns[f].0 < at && fns[f].1 > at).collect() };
        let scopes: HashSet<usize> = use_at.map(|u| around(u).into_iter().collect()).unwrap_or_default();
        let mut best: Option<usize> = None;
        let mut depth: i64 = -1;
        for (i, &at) in keywords.iter().enumerate() {
            let encl = around(at);
            if !encl.iter().all(|f| scopes.contains(f)) {
                continue;
            }
            if encl.len() as i64 > depth || (encl.len() as i64 == depth && use_at.is_some_and(|u| at < u)) {
                best = Some(i);
                depth = encl.len() as i64;
            }
        }
        best
    })
}

/// The tables the function written at `at` returns: its expression body's (`(user) => [ … ]`,
/// `() => buildRoutes()`), or what each `return` of its own hands back, an early guard (`if
/// (!user) return [];`) and every branch alike. A `return` in a function written inside it
/// (`.map((p) => { return [ … ]; })`, a method's) is that function's.
fn returned_tables(safe: &str, at: usize) -> Vec<TableSite> {
    let Some(body) = function_body(safe, at) else { return Vec::new() };
    if !body.block {
        return held_table(safe, body.start);
    }
    own_returns(safe, body.start, body.end).into_iter().flat_map(|value| held_table(safe, value)).collect()
}

struct FunctionBody {
    block: bool,
    start: usize,
    end: usize,
}

/// Where the function written at `at` keeps its body: a block, from its `{` to its `}`, or an
/// arrow's expression, at `start`. `None` for no function.
fn function_body(safe: &str, at: usize) -> Option<FunctionBody> {
    let mut i = skip_space(safe, at);
    let keyword = |word: &str, i: &mut usize| -> bool {
        let b = safe.as_bytes();
        if !safe[(*i).min(safe.len())..].starts_with(word) || b.get(*i + word.len()).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') {
            return false;
        }
        *i = skip_space(safe, *i + word.len());
        true
    };
    keyword("async", &mut i);
    let arrow = !keyword("function", &mut i);
    if !arrow {
        if let Some(name) = re!(r"\*?\s*[A-Za-z_$][\w$]*", "y").captures_at(safe, i) {
            i = skip_space(safe, name.get(0)?.end());
        }
    }
    if byte_at(safe, i) == b'(' {
        let close = match_bracket(safe, i)?;
        i = skip_space(safe, close + 1);
    } else {
        // `user => [ … ]`
        if !arrow {
            return None;
        }
        let param_end = ident_end(safe, i)?;
        i = skip_space(safe, param_end);
    }
    // A return type, up to the arrow or the body: `(): RouteObject[] => [ … ]`.
    if byte_at(safe, i) == b':' {
        let stop = safe[i..].find(if arrow { "=>" } else { "{" })?;
        i += stop;
    }
    if arrow {
        if !safe[i.min(safe.len())..].starts_with("=>") {
            return None;
        }
        i = skip_space(safe, i + 2);
        if byte_at(safe, i) != b'{' {
            return Some(FunctionBody { block: false, start: i, end: i });
        }
    }
    if byte_at(safe, i) != b'{' {
        return None;
    }
    let end = block_end(safe, i)?;
    Some(FunctionBody { block: true, start: i, end })
}

/// The statements that start a block of their own after a parenthesized head: never a method's.
const BLOCK_HEADS: [&str; 6] = ["if", "for", "while", "switch", "catch", "with"];

/// Where each `return` of the block opening at `open` hands back its value: the block's own,
/// past the functions written inside it — an arrow's block, a `function`, a method like
/// `lazy() { … }`.
fn own_returns(safe: &str, open: usize, close: usize) -> Vec<usize> {
    let b = safe.as_bytes();
    let mut values: Vec<usize> = Vec::new();
    // Past the body of a function whose parameters open at `paren`.
    let past_body = |paren: Option<usize>| -> Option<usize> {
        let params = match_bracket(safe, paren?)?;
        let mut i = skip_space(safe, params + 1);
        if byte_at(safe, i) == b':' {
            i += safe[i..].find('{')?;
        }
        if byte_at(safe, i) == b'{' {
            block_end(safe, i)
        } else {
            None
        }
    };
    let mut i = open + 1;
    while i < close {
        let ch = b[i];
        if opens_string(safe, i) {
            let Some(end) = skip_string(safe, i) else { break };
            i = end + 1;
            continue;
        }
        if ch == b'=' && b.get(i + 1) == Some(&b'>') {
            let body = skip_space(safe, i + 2);
            let end = if byte_at(safe, body) == b'{' { block_end(safe, body) } else { None };
            i = end.unwrap_or(i + 1) + 1;
            continue;
        }
        let prev = if i > 0 { b[i - 1] } else { 0 };
        if (ch.is_ascii_alphabetic() || ch == b'_' || ch == b'$') && !(prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'$' || prev == b'.') {
            let word_end = ident_end(safe, i).unwrap_or(i + 1);
            let name = &safe[i..word_end];
            let next = skip_space(safe, word_end);
            if name == "return" {
                values.push(next);
            } else if name == "function" {
                let paren = safe[next.min(safe.len())..].find('(').map(|p| next + p);
                if let Some(end) = past_body(paren).filter(|e| *e > 0) {
                    i = end + 1;
                    continue;
                }
            } else if byte_at(safe, next) == b'(' && !BLOCK_HEADS.contains(&name) {
                if let Some(end) = past_body(Some(next)).filter(|e| *e > 0) {
                    i = end + 1;
                    continue;
                }
            }
            i = word_end;
            continue;
        }
        i += 1;
    }
    values
}

/// The index of the brace closing the block at `open`. A quote right after a word is an
/// apostrophe in JSX text (`<p>Don't</p>`), not a string.
fn block_end(safe: &str, open: usize) -> Option<usize> {
    let b = safe.as_bytes();
    let mut depth = 0i32;
    let mut i = open;
    while i < b.len() {
        if opens_string(safe, i) {
            i = skip_string(safe, i)? + 1;
            continue;
        }
        let ch = b[i];
        if ch == b'{' || ch == b'(' || ch == b'[' {
            depth += 1;
        } else if ch == b'}' || ch == b')' || ch == b']' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// True for a quote that opens a string or template, not an apostrophe in JSX text.
fn opens_string(safe: &str, i: usize) -> bool {
    let ch = byte_at(safe, i);
    ch == b'`' || ((ch == b'"' || ch == b'\'') && !(i > 0 && matches!(safe.as_bytes()[i - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'$')))
}

fn skip_space(safe: &str, mut i: usize) -> usize {
    loop {
        let n = space_len_at(safe, i);
        if n == 0 {
            return i;
        }
        i += n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_literal_is_held_directly() {
        let safe = "const routes = [{ path: '/' }];";
        let at = safe.find('[').unwrap();
        assert!(matches!(held_table(safe, at).as_slice(), [TableSite::Literal(n)] if *n == at));
    }

    #[test]
    fn a_function_returns_its_tables() {
        let safe = "function buildRoutes(user) { if (!user) return []; return [{ path: '/a' }]; }";
        let tables = returned_tables(safe, 0);
        assert_eq!(tables.len(), 2);
    }
}
