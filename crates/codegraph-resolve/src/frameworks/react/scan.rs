//! The structural route scanner behind the React resolver: strings, comments, JSX and balanced
//! expressions are units. Port of `scanRoutes` and its helpers in
//! `src/resolution/frameworks/react.ts`.
//!
//! With `objects`, every route object in the file is read. With `table`, only the literal at that
//! offset is — a table another file hands the router — its routes sitting under `table.prefix`
//! and inside `table.layouts`, and the identifiers it names as more routes come back in `names`.
//! With `uses`, the places the file hands a table to the router come back too.
//!
//! A route that renders something around others — a route object with `children`, a
//! `<Route element>` with `<Route>`s inside — is the layout they render inside, and the address is
//! theirs when one of them claims it: an `index` route, or a `''` path. A `<Route path>` that
//! renders nothing only groups them; it is a route of its own until one of them claims its address.

use super::loader::{lazy_route_reference, top_level_entries, OUTLET};
use crate::frameworks::object_literal::{byte_at, ident_end, is_word_byte, match_bracket, skip_string, skip_ws, slice, space_len_at, starts_at, tail};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::js_trim;
use std::collections::{HashMap, HashSet};

/// One segment of a nested route's path: a literal, or a constant's member expression.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RoutePart {
    pub lit: Option<String>,
    pub expr: Option<String>,
}

impl RoutePart {
    pub fn lit(value: impl Into<String>) -> Self {
        RoutePart { lit: Some(value.into()), expr: None }
    }
    /// The part as `JSON.stringify` writes it.
    pub fn to_json(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        if let Some(lit) = &self.lit {
            map.insert("lit".into(), serde_json::Value::String(lit.clone()));
        }
        if let Some(expr) = &self.expr {
            map.insert("expr".into(), serde_json::Value::String(expr.clone()));
        }
        serde_json::Value::Object(map)
    }
    pub fn from_json(value: &serde_json::Value) -> Self {
        RoutePart {
            lit: value.get("lit").and_then(|v| v.as_str()).map(String::from),
            expr: value.get("expr").and_then(|v| v.as_str()).map(String::from),
        }
    }
}

/// React Router's nesting: a child path is relative unless it starts with `/`.
pub fn compose_route_path(parts: &[String]) -> String {
    let mut path = String::new();
    for part in parts {
        if part.starts_with('/') {
            path = part.clone();
        } else if !part.is_empty() {
            path = format!("{}/{}", re!(r"\/+$").replace_first(&path, |_| String::new()), part);
        }
    }
    let rooted = format!("/{}", re!(r"^\/+").replace_first(&path, |_| String::new()));
    let collapsed = re!(r"\/{2,}", "g").replace_all_str(&rooted, "/");
    re!(r"(.)\/$").replace_first(&collapsed, |c| c.text(1).unwrap_or("").to_string())
}

#[derive(Clone, Debug)]
pub struct RouteDeclaration {
    /// The route's full path as far as the file tells: nesting composed, a constant part shown as `{expr}`.
    pub path: String,
    /// Its path parts, outermost first.
    pub parts: Vec<RoutePart>,
    pub component: Option<String>,
    /// What its `lazy` loader renders, as the route's reference names it.
    pub lazy: Option<String>,
    /// The route objects around it that render something, outermost first: the layouts it renders
    /// inside, each a component name, `lazy-import:…` or `import:…#…`.
    pub layouts: Vec<String>,
    pub at: usize,
}

/// A path-bearing route object or `<Route>` element, with the extent its children sit in.
#[derive(Clone, Debug, Default)]
struct RouteScope {
    part: RoutePart,
    /// Where the route is named: its `path` (or `index`) property, or its `<Route` tag.
    at: usize,
    /// Its `{` or `<`: the scope holds every scope that opens after this and closes by `end`.
    start: usize,
    end: usize,
    component: Option<String>,
    lazy: Option<String>,
    /// A `<Route>` element: a route even with nothing to render.
    jsx: bool,
    /// A route object that renders React Router's `<Outlet />`.
    outlet: bool,
    /// A layout at no address of its own.
    pathless: bool,
    /// A `<Route index>` (or a `path=""` with nothing inside): the page at its parent's address.
    index: bool,
    /// A `<Route path={paths.home}>`: an address the file does not spell out, so no route of its own.
    opaque: bool,
}

/// An identifier written where React Router reads routes — `[MainRoutes, LoginRoutes]`,
/// `...ApiAuthorizationRoutes`, `children: adminRoutes`, `useRoutes(routes(isLoggedIn))` — naming
/// a table declared elsewhere.
#[derive(Clone, Debug)]
pub struct TableName {
    pub name: String,
    /// `routes(isLoggedIn)`: the table is what the function returns.
    pub call: bool,
    /// Where it is written; inside a table, the route objects around it are its mount.
    pub at: usize,
}

/// A table another table names as more of its routes, with the path parts and layouts it sits under.
#[derive(Clone, Debug)]
pub struct NamedTable {
    pub table: TableName,
    pub prefix: Vec<RoutePart>,
    pub layouts: Vec<String>,
}

/// A place a file hands routes to React Router.
#[derive(Clone, Debug)]
pub struct TableUse {
    /// The table as the site names it; absent when the literal is written there.
    pub name: Option<TableName>,
    /// The literal written there.
    pub literal: Option<usize>,
    /// The routes around the site, outermost first: where the table is mounted.
    pub prefix: Vec<RoutePart>,
    /// The layouts around the site, outermost first: what the table's routes render inside.
    pub layouts: Vec<String>,
}

pub struct RouteScan {
    pub routes: Vec<RouteDeclaration>,
    pub names: Vec<NamedTable>,
    pub uses: Vec<TableUse>,
}

#[derive(Default)]
pub struct TableScope {
    pub at: usize,
    pub prefix: Vec<RoutePart>,
    pub layouts: Vec<String>,
}

#[derive(Default)]
pub struct ScanOptions {
    pub objects: bool,
    pub table: Option<TableScope>,
    pub uses: bool,
}

/// The calls that hand React Router its routes: a data router, JSX routes made into one, or the
/// `useRoutes` hook. Every route object in a file that makes one is a route.
pub fn router_call_re() -> &'static Re {
    re!(r"\b(?:createBrowserRouter|createHashRouter|createMemoryRouter|createRoutesFromElements|useRoutes)\b")
}

/// A route list's entry that names more routes: `MainRoutes`, `...authRoutes`, `...routes(user)`.
fn named_entry_re() -> &'static Re {
    re!(r"^(?:\.\.\.\s*)?([A-Za-z_$][\w$]*)\s*(\([\s\S]*\))?$")
}

/// The routes a file's own text declares: its `<Route>` elements and, in a file that makes a
/// router, its route objects.
pub fn scan_route_declarations(source: &str, allow_jsx: bool) -> Vec<RouteDeclaration> {
    let objects = router_call_re().is_match(source);
    if !objects && !re!(r"<Route\b").is_match(source) {
        return Vec::new();
    }
    scan_routes(source, allow_jsx, ScanOptions { objects, ..Default::default() }).routes
}

fn literal(value: &str) -> Option<String> {
    let m = re!(r#"^(?:"((?:\\.|[^"\\])*)"|'((?:\\.|[^'\\])*)')$"#).captures(js_trim(value))?;
    m.text(1).or_else(|| m.text(2)).map(String::from)
}

fn is_identifier(s: &str) -> bool {
    ident_end(s, 0) == Some(s.len())
}

// -- Component names ------------------------------------------------------------------------------

/// A guard or boundary wrapping the screen (`<ProtectedRoute><AppRoot/></ProtectedRoute>`,
/// `<AdminWrapper><Users/></AdminWrapper>`, `<Suspense>`) is not what the route renders; the first
/// element inside it is.
fn is_wrapper(tag: &str) -> bool {
    re!(r"^(?:Suspense|ErrorBoundary|\w*(?:Guard|Provider|Wrapper|Route)|(?:Protected|Private|Auth|Require\w*)\w*)$").is_match(tag)
}

pub fn component_name(value: Option<&str>, jsx: bool) -> Option<String> {
    let value = value.filter(|v| !v.is_empty())?;
    if !jsx {
        return re!(r"^\s*([A-Z][\w]*)\s*$").captures(value).and_then(|c| c.text(1).map(String::from));
    }
    let inner = re!(r"^\s*(?:\(\s*)?").replace_first(value, |_| String::new());
    if !re!(r"^<\s*[A-Z]").is_match(&inner) {
        return None;
    }
    let (own, props) = element_tags(&inner);
    let content = |t: &&String| !is_wrapper(t) && t.as_str() != OUTLET;
    // Then a page handed to a guard as a prop (`<PrivateRoute component={<Profile />} />`), then
    // the innermost wrapper: `<RequireAuth><Outlet /></RequireAuth>` renders the guard.
    own.iter()
        .find(content)
        .or_else(|| props.iter().find(content))
        .or_else(|| own.iter().rev().find(|t| t.as_str() != OUTLET))
        .or_else(|| own.first())
        .cloned()
}

/// The component an `element` shows, `Outlet` included.
pub fn element_tag(value: Option<&str>) -> Option<String> {
    component_name(value, true)
}

/// The page an `element` shows: `element={<Outlet />}` renders the route inside it and nothing of
/// its own.
fn element_page(value: Option<&str>) -> Option<String> {
    element_tag(value).filter(|t| t != OUTLET)
}

/// The component tags of a JSX element and of what it nests, outermost first (`own`), and apart
/// from them a page handed over in a `component`, `element` or `page` attribute (`props`). Any
/// other attribute's elements are neither: `<Suspense fallback={<Loader />}><AdminPanel /></Suspense>`
/// renders AdminPanel, never Loader.
pub fn element_tags(text: &str) -> (Vec<String>, Vec<String>) {
    let b = text.as_bytes();
    let mut own: Vec<String> = Vec::new();
    let mut props: Vec<String> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        let Some(open) = re!(r"^<\s*([A-Za-z][\w.:-]*)(?=[\s/>])").captures(tail(text, i)) else {
            i += 1;
            continue;
        };
        let name = open.text(1).unwrap_or("");
        if re!(r"^[A-Z]\w*$").is_match(name) {
            own.push(name.to_string());
        }
        // Skip the tag's attributes, expressions and strings whole.
        let mut j = i + open.get(0).map_or(0, |m| m.end());
        let mut attr = String::new();
        while j < b.len() && b[j] != b'>' {
            let ch = b[j];
            if let Some(end) = attribute_name_end(text, j) {
                attr = text[j..end].to_string();
                j = end;
                continue;
            }
            let end = match ch {
                b'{' => match_bracket(text, j),
                b'"' | b'\'' => skip_string(text, j),
                _ => Some(j),
            };
            let Some(end) = end else { break };
            if ch == b'{' && re!(r"^(?:component|element|page)$", "i").is_match(&attr) {
                for m in re!(r"<\s*([A-Z]\w*)(?=[\s/>])", "g").captures_iter(slice(text, j, end)) {
                    props.push(m.text(1).unwrap_or("").to_string());
                }
            }
            if ch == b'{' || ch == b'"' || ch == b'\'' {
                attr.clear();
            }
            j = end + 1;
        }
        i = j + 1;
    }
    (own, props)
}

/// The end of the `[A-Za-z_$][\w$:-]*` name at `j`, over a 64-unit window.
fn attribute_name_end(text: &str, j: usize) -> Option<usize> {
    let b = text.as_bytes();
    let first = *b.get(j)?;
    if !(first.is_ascii_alphabetic() || first == b'_' || first == b'$') {
        return None;
    }
    let mut k = j + 1;
    while k < b.len() && k - j < 64 && (is_word_byte(b[k]) || matches!(b[k], b'$' | b':' | b'-')) {
        k += 1;
    }
    Some(k)
}

/// True when a `.map` callback renders a `<Route>` from the item's own route fields — `{...route}`,
/// `{...rest}` destructured from it, `path={route.path}` or a `path` destructured from it — so the
/// array it maps is a route table. A `<Route path={item.layout + item.path}>` composes a path the
/// table does not hold, and a callback that renders links or menu entries maps a menu.
fn maps_route_fields(callback: &str, tags: &[RouteTag]) -> bool {
    let head = re!(r"^(?:async\s+)?(?:function\b[^(]*)?\(\s*").find(callback);
    let mut param: Option<String> = None;
    let mut rest: HashSet<String> = HashSet::new();
    let mut paths: HashSet<String> = HashSet::new();
    let mut bind = |pattern: &str| {
        for entry in top_level_entries(&pattern[1..pattern.len() - 1]) {
            if let Some(spread) = re!(r"^\.\.\.\s*([A-Za-z_$][\w$]*)$").captures(&entry) {
                rest.insert(spread.text(1).unwrap_or("").to_string());
            }
            if let Some(path) = re!(r"^path\s*(?::\s*([A-Za-z_$][\w$]*))?\s*(?:=[\s\S]*)?$").captures(&entry) {
                paths.insert(path.text(1).unwrap_or("path").to_string());
            }
        }
    };
    if let Some(head) = head {
        let at = head.end();
        if byte_at(callback, at) == b'{' {
            let Some(close) = match_bracket(callback, at) else { return false };
            bind(&callback[at..=close]);
        } else {
            let t = tail(callback, at);
            param = ident_end(t, 0).map(|e| t[..e].to_string());
        }
    } else {
        param = re!(r"^(?:async\s+)?([A-Za-z_$][\w$]*)\s*=>").captures(callback).and_then(|c| c.text(1).map(String::from));
    }
    if let Some(param) = &param {
        // `const { element, ...rest } = route;` in the body.
        let destructured = Re::new(&format!(r"\b(?:const|let|var)\s*(\{{[^{{}}]*\}})\s*=\s*{}\s*[;\n]", escape_regex(param)), "g");
        for m in destructured.captures_iter(callback) {
            bind(m.text(1).unwrap_or(""));
        }
    }
    let own_path = param.as_ref().map(|p| Re::new(&format!(r"^{}\s*\??\.\s*path$", escape_regex(p)), ""));
    tags.iter().any(|t| {
        t.spreads.iter().any(|s| Some(s) == param.as_ref() || rest.contains(s))
            || t.path.as_ref().is_some_and(|p| own_path.as_ref().is_some_and(|re| re.is_match(p)) || paths.contains(p))
    })
}

// -- The scanner ------------------------------------------------------------------------------------

/// Every `<Route>` tag: where it is, the props it spreads, its `path` expression.
#[derive(Clone)]
struct RouteTag {
    at: usize,
    spreads: Vec<String>,
    path: Option<String>,
}

struct Site {
    name: Option<TableName>,
    literal: Option<usize>,
    at: usize,
}

struct RouterCall {
    map: Option<TableName>,
}

struct Scanner<'a> {
    source: &'a str,
    allow_jsx: bool,
    objects: bool,
    uses: bool,
    scopes: Vec<RouteScope>,
    names: Vec<TableName>,
    sites: Vec<Site>,
    route_tags: Vec<RouteTag>,
    /// Where the router itself is written (`createRoutesFromElements(…)`, `<BrowserRouter>…</BrowserRouter>`): its routes start at `/`.
    routers: Vec<(usize, usize)>,
}

pub fn scan_routes(source: &str, allow_jsx: bool, options: ScanOptions) -> RouteScan {
    let objects = options.objects || options.table.is_some();
    let mut scanner = Scanner {
        source,
        allow_jsx,
        objects,
        uses: options.uses,
        scopes: Vec::new(),
        names: Vec::new(),
        sites: Vec::new(),
        route_tags: Vec::new(),
        routers: Vec::new(),
    };
    if let Some(table) = &options.table {
        let at = scanner.trivia(table.at);
        scanner.unit(at, true);
    } else {
        let mut at = 0;
        loop {
            at = scanner.trivia(at);
            if at >= source.len() {
                break;
            }
            at = scanner.unit(at, false);
        }
    }
    scanner.finish(&options)
}

impl Scanner<'_> {
    /// The few characters before `at`, trailing whitespace skipped: enough for the end-anchored
    /// checks below without copying the whole prefix per `/` or `<`.
    fn token_before(&self, at: usize) -> &str {
        let src = self.source;
        let mut j = at;
        while j > 0 {
            // Step back over one whitespace character.
            let mut k = j - 1;
            while !src.is_char_boundary(k) {
                k -= 1;
            }
            if space_len_at(src, k) == 0 {
                break;
            }
            j = k;
        }
        slice(src, j.saturating_sub(12), j)
    }

    fn trivia(&self, mut at: usize) -> usize {
        let src = self.source;
        while at < src.len() {
            let n = space_len_at(src, at);
            if n > 0 {
                at += n;
                continue;
            }
            if starts_at(src, at, "//") {
                at = src[at + 2..].find('\n').map_or(src.len(), |e| at + 2 + e);
            } else if starts_at(src, at, "/*") {
                at = src[at + 2..].find("*/").map_or(src.len(), |e| at + 2 + e + 2);
            } else {
                break;
            }
        }
        at
    }

    /// `useRoutes(…)` / `createBrowserRouter(…)` hand the router a table; so does `AppRoutes.map(…)`
    /// when its callback renders a `<Route>` from each item.
    fn router_call(&self, at: usize) -> Option<RouterCall> {
        if !self.uses {
            return None;
        }
        let src = self.source;
        let mut j = at;
        while j > 0 && space_len_at(src, j - 1) > 0 {
            j -= 1;
        }
        let last = if j > 0 { src.as_bytes()[j - 1] } else { 0 };
        if last != b's' && last != b'r' && last != b'p' {
            return None;
        }
        let before = slice(src, at.saturating_sub(64), at);
        if re!(r"(?:^|[^\w$.])(?:useRoutes|create(?:Browser|Hash|Memory)Router)\s*$").is_match(before) {
            return Some(RouterCall { map: None });
        }
        let map = re!(r"(?:^|[^\w$.])([A-Za-z_$][\w$]*)\s*\??\.\s*map\s*$").captures(before)?;
        let name = map.get(1)?;
        Some(RouterCall { map: Some(TableName { name: name.as_str().to_string(), call: false, at: at - before.len() + name.start() }) })
    }

    fn routes_from_elements(&self, at: usize) -> bool {
        let src = self.source;
        let mut j = at;
        while j > 0 && space_len_at(src, j - 1) > 0 {
            j -= 1;
        }
        let last = if j > 0 { src.as_bytes()[j - 1] } else { 0 };
        if last != b's' && last != b'n' {
            return false;
        }
        re!(r"(?:^|[^\w$.])createRoutesFrom(?:Elements|Children)\s*$").is_match(slice(src, at.saturating_sub(40), at))
    }

    fn settle(&mut self, call: RouterCall, open: usize, close: usize) {
        let src = self.source;
        let arg_at = self.trivia(open + 1);
        if arg_at >= close {
            return;
        }
        if let Some(map) = call.map {
            let tags: Vec<RouteTag> = self.route_tags.iter().filter(|t| t.at > arg_at && t.at < close).cloned().collect();
            if !tags.is_empty() && maps_route_fields(slice(src, arg_at, close), &tags) {
                self.sites.push(Site { name: Some(map), literal: None, at: open });
            }
            return;
        }
        if matches!(byte_at(src, arg_at), b'[' | b'{') {
            self.sites.push(Site { name: None, literal: Some(arg_at), at: open });
            return;
        }
        let Some(id_end) = ident_end(slice(src, arg_at, close), 0).map(|e| arg_at + e) else { return };
        let id = &src[arg_at..id_end];
        if re!(r"^createRoutesFrom(?:Elements|Children)$").is_match(id) {
            return;
        }
        let next = self.trivia(id_end);
        if next == close || byte_at(src, next) == b',' {
            self.sites.push(Site { name: Some(TableName { name: id.to_string(), call: false, at: arg_at }), literal: None, at: open });
        } else if byte_at(src, next) == b'(' {
            self.sites.push(Site { name: Some(TableName { name: id.to_string(), call: true, at: arg_at }), literal: None, at: open });
        }
    }

    /// `{name}:` or `'name':` at `i`, in an object: the name, and the length of the key with its colon.
    fn object_key(&self, i: usize) -> Option<(String, usize)> {
        let src = self.source;
        if let Some(e) = ident_end(src, i) {
            let colon = skip_ws(src, e);
            if byte_at(src, colon) == b':' {
                return Some((src[i..e].to_string(), colon + 1 - i));
            }
            return None;
        }
        let quote = byte_at(src, i);
        if quote == b'"' || quote == b'\'' {
            let rel = src.as_bytes()[i + 1..].iter().position(|&c| c == b'"' || c == b'\'')?;
            if rel == 0 {
                return None;
            }
            let k = i + 1 + rel;
            let colon = skip_ws(src, k + 1);
            if byte_at(src, colon) == b':' {
                return Some((src[i + 1..k].to_string(), colon + 1 - i));
            }
        }
        None
    }

    /// The end of the unit starting at `at`: a string, a regex, JSX, a bracketed expression or one character.
    fn unit(&mut self, at: usize, route_list: bool) -> usize {
        let src = self.source;
        let b = src.as_bytes();
        let ch = byte_at(src, at);
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            let mut i = at + 1;
            while i < b.len() {
                if b[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if b[i] == ch {
                    return i + 1;
                }
                if ch == b'`' && b[i] == b'$' && b.get(i + 1) == Some(&b'{') {
                    i = self.unit(i + 1, false);
                    continue;
                }
                i += 1;
            }
            return i;
        }
        // A regex can contain braces or JSX-looking text without ending an expression.
        if ch == b'/' {
            let before = self.token_before(at);
            let after_operator = before.is_empty()
                || before.as_bytes().last().is_some_and(|c| b"=(:,[!&|?{};".contains(c))
                || re!(r"\b(?:return|throw|case|yield)\s*$").is_match(before);
            if after_operator {
                let mut in_class = false;
                let mut i = at + 1;
                while i < b.len() && b[i] != b'\n' {
                    if b[i] == b'\\' {
                        i += 1;
                    } else if b[i] == b'[' {
                        in_class = true;
                    } else if b[i] == b']' {
                        in_class = false;
                    } else if b[i] == b'/' && !in_class {
                        i += 1;
                        while i < b.len() && b[i].is_ascii_alphabetic() {
                            i += 1;
                        }
                        return i;
                    }
                    i += 1;
                }
            }
        }
        if self.allow_jsx && ch == b'<' && (b.get(at + 1).is_some_and(|c| c.is_ascii_alphabetic()) || b.get(at + 1) == Some(&b'>')) {
            let before = self.token_before(at);
            // `count<limit` and `factory<Type>()` are not JSX opening tags.
            let after_value = before.as_bytes().last().is_some_and(|c| is_word_byte(*c) || matches!(c, b'$' | b')' | b']' | b'\'' | b'"'));
            if !after_value || before.ends_with("return") && !before[..before.len() - 6].ends_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
                return self.jsx(at);
            }
        }
        let close = match ch {
            b'{' => b'}',
            b'[' => b']',
            b'(' => b')',
            _ => return at + tail(src, at).chars().next().map_or(1, char::len_utf8),
        };
        let call = if ch == b'(' { self.router_call(at) } else { None };
        let router = ch == b'(' && self.routes_from_elements(at);
        let mut i = at + 1;
        let mut fields: HashMap<String, (String, usize)> = HashMap::new();
        loop {
            i = self.trivia(i);
            if i >= b.len() || b[i] == close {
                break;
            }
            // A property must start at an object entry, never inside its value.
            let key = if ch == b'{' { self.object_key(i) } else { None };
            if let Some((name, key_len)) = key {
                let start = i;
                let value_at = self.trivia(i + key_len);
                // A route's children are more routes, written in place or named.
                let children = self.objects && name == "children";
                i = value_at;
                let mut value_end = i;
                loop {
                    i = self.trivia(i);
                    if i >= b.len() || b[i] == b',' || b[i] == close {
                        break;
                    }
                    i = self.unit(i, children);
                    value_end = i;
                }
                let value = js_trim(slice(src, value_at, value_end)).to_string();
                if children && is_identifier(&value) {
                    self.names.push(TableName { name: value.clone(), call: false, at: value_at });
                }
                fields.insert(name, (value, start));
            } else {
                // Skip a whole entry (spread, method, shorthand), but still visit nested units.
                let entry = i;
                loop {
                    i = self.trivia(i);
                    if i >= b.len() || b[i] == b',' || b[i] == close {
                        break;
                    }
                    i = self.unit(i, false);
                }
                // `async lazy() { … }`: a route's loader written as a method.
                if self.objects && ch == b'{' {
                    if let Some(method) = re!(r"^(async\s+)?lazy\s*\(").captures(slice(src, entry, entry + 32)) {
                        let paren = entry + method.get(0).map_or(0, |m| m.end()) - 1;
                        let value = format!("{}function {}", method.text(1).unwrap_or(""), js_trim(slice(src, paren, i)));
                        fields.insert("lazy".to_string(), (value, entry));
                    }
                }
                if route_list && ch == b'[' {
                    let stripped = re!(r"\/\*[\s\S]*?\*\/|\/\/[^\n]*", "g").replace_all_str(slice(src, entry, i), "");
                    let text = js_trim(&stripped);
                    let named = named_entry_re().captures(text);
                    if let Some(named) = named {
                        let args = named.text(2).map(|g| text.len() - g.len());
                        if args.is_none_or(|a| match_bracket(text, a) == Some(text.len() - 1)) {
                            self.names.push(TableName { name: named.text(1).unwrap_or("").to_string(), call: args.is_some(), at: entry });
                        }
                    }
                }
            }
            if byte_at(src, i) == b',' {
                i += 1;
            }
        }
        if self.objects && ch == b'{' && byte_at(src, i) == close {
            let path_field = fields.get("path");
            let path = path_field.and_then(|(value, _)| literal(value));
            let expr = path_field
                .filter(|(value, _)| path.is_none() && re!(r"^[A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*)+$").is_match(value))
                .map(|(value, _)| value.clone());
            let shown = element_tag(fields.get("element").map(|f| f.0.as_str())).or_else(|| component_name(fields.get("Component").map(|f| f.0.as_str()), false));
            let loaded = lazy_route_reference(fields.get("lazy").map(|f| f.0.as_str()));
            // A route with no element renders an `<Outlet />`, so one written with it renders
            // nothing of its own either: a route at its address, and no layout around the routes
            // inside it. At its parent's address (`path: ''`) it only groups them, the parent's
            // page staying the one there.
            let outlet = shown.as_ref().or(loaded.as_ref()).is_some_and(|v| v == OUTLET) && path.as_deref() != Some("");
            let component = shown.filter(|s| s != OUTLET);
            let lazy = loaded.filter(|l| l != OUTLET);
            let index = fields.get("index");
            if let Some((_, path_at)) = path_field.filter(|_| path.is_some() || expr.is_some()) {
                let part = match &path {
                    Some(p) => RoutePart::lit(p.clone()),
                    None => RoutePart { lit: None, expr: expr.clone() },
                };
                self.scopes.push(RouteScope { part, at: *path_at, start: at, end: i, component, lazy, outlet, ..Default::default() });
            } else if path_field.is_none() && index.is_some_and(|(value, _)| value == "true") {
                // `{ index: true, element: <Home /> }` is the page at its parent's address.
                self.scopes.push(RouteScope { part: RoutePart::lit(""), at: index.unwrap().1, start: at, end: i, component, lazy, ..Default::default() });
            } else if path_field.is_none() && (component.is_some() || lazy.is_some()) && fields.contains_key("children") {
                self.scopes.push(RouteScope { part: RoutePart::lit(""), at, start: at, end: i, component, lazy, pathless: true, ..Default::default() });
            }
        }
        if let Some(call) = call {
            if i < b.len() {
                self.settle(call, at, i);
            }
        }
        if router {
            self.routers.push((at, i));
        }
        if i < b.len() {
            i + 1
        } else {
            i
        }
    }

    fn jsx(&mut self, at: usize) -> usize {
        let src = self.source;
        let b = src.as_bytes();
        let mut tag_end = at + 1;
        while tag_end < b.len() && (is_word_byte(b[tag_end]) || matches!(b[tag_end], b'.' | b':' | b'-')) {
            tag_end += 1;
        }
        let tag = &src[at + 1..tag_end];
        let mut i = tag_end;
        let mut attrs: HashMap<String, String> = HashMap::new();
        let mut spreads: Vec<String> = Vec::new();
        loop {
            i = self.trivia(i);
            if i >= b.len() || b[i] == b'>' || starts_at(src, i, "/>") {
                break;
            }
            let mut attr_end = i;
            while attr_end < b.len() && (is_word_byte(b[attr_end]) || matches!(b[attr_end], b':' | b'-')) {
                attr_end += 1;
            }
            if attr_end == i {
                let from = i;
                i = self.unit(i, false);
                if let Some(spread) = re!(r"^\{\s*\.\.\.\s*([A-Za-z_$][\w$]*)\s*\}$").captures(slice(src, from, i)) {
                    spreads.push(spread.text(1).unwrap_or("").to_string());
                }
                continue;
            }
            let attr = src[i..attr_end].to_string();
            i = self.trivia(attr_end);
            // A bare attribute is `true`: `<Route index element={<Home/>}/>`.
            if byte_at(src, i) != b'=' {
                attrs.insert(attr, "{true}".to_string());
                continue;
            }
            i = self.trivia(i + 1);
            let start = i;
            i = self.unit(i, false);
            attrs.insert(attr, slice(src, start, i).to_string());
        }
        let mut scope_idx: Option<usize> = None;
        // A `<Route>` with no address of its own, settled once its children are read.
        let mut addressless: Option<(RouteScope, usize, bool)> = None;
        // `<BrowserRouter>…</BrowserRouter>`: the routes at its top are at `/`.
        let router_idx = if re!(r"^(?:Browser|Hash|Memory|Native|Static|History|unstable_History)?Router$").is_match(tag) {
            self.routers.push((at, b.len()));
            Some(self.routers.len() - 1)
        } else {
            None
        };
        if tag == "Route" && i < b.len() {
            let path_text = attrs.get("path");
            let expression = |name: &str| -> Option<String> {
                attrs.get(name).map(|v| re!(r"^\{([\s\S]*)\}$").replace_first(v, |c| c.text(1).unwrap_or("").to_string()))
            };
            // `path="team"`, or the same string in braces: `path={"team"}`.
            let path = literal(path_text.map_or("", String::as_str)).or_else(|| literal(&expression("path").unwrap_or_default()));
            self.route_tags.push(RouteTag { at, spreads, path: expression("path").map(|p| js_trim(&p).to_string()) });
            let element = element_page(expression("element").as_deref());
            let component = component_name(expression("component").as_deref(), false).or_else(|| element.clone());
            let loaded = lazy_route_reference(expression("lazy").as_deref());
            // A `<Route>` is a route with nothing of its own to render already.
            let lazy = loaded.filter(|l| l != OUTLET);
            let route = RouteScope {
                part: RoutePart::lit(path.clone().unwrap_or_default()),
                at,
                start: at,
                end: b.len(),
                component,
                lazy,
                jsx: true,
                ..Default::default()
            };
            let mut pushed: Option<RouteScope> = None;
            if path.as_deref().is_some_and(|p| !p.is_empty()) {
                pushed = Some(route);
            } else if path_text.is_none() || path.as_deref() == Some("") {
                // `<Route index element={<Home/>}/>` is the page at its parent's address.
                if re!(r"^\{\s*true\s*\}$").is_match(attrs.get("index").map_or("", String::as_str)) {
                    if route.component.is_some() || route.lazy.is_some() {
                        pushed = Some(RouteScope { index: true, ..route });
                    }
                } else {
                    // Otherwise it is a layout at no address of its own when routes sit inside it
                    // (`<Route element={<RequireAuth/>}>`), and `path=""` with an `element` and
                    // nothing inside is the page at its parent's address. Version 5's
                    // `<Route path="" component={NotFound}/>` is a catch-all, which no address names.
                    let page = path.as_deref() == Some("") && (element.is_some() || route.lazy.is_some());
                    addressless = Some((route, self.route_tags.len(), page));
                }
            } else {
                // `path={paths.home}`: an address the file does not spell out.
                pushed = Some(RouteScope { opaque: true, ..route });
            }
            if let Some(scope) = pushed {
                self.scopes.push(scope);
                scope_idx = Some(self.scopes.len() - 1);
            }
        }
        if starts_at(src, i, "/>") {
            return self.close_jsx(i + 2, scope_idx, router_idx, addressless);
        }
        i += 1;
        while i < b.len() {
            if starts_at(src, i, "</") {
                let end = src[i + 2..].find('>').map(|e| i + 2 + e);
                return self.close_jsx(end.map_or(b.len(), |e| e + 1), scope_idx, router_idx, addressless);
            }
            if b[i] == b'<' && b.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic() || *c == b'>') {
                i = self.jsx(i);
            } else if b[i] == b'{' {
                i = self.unit(i, false);
            } else {
                i += 1;
            }
        }
        self.close_jsx(i, scope_idx, router_idx, addressless)
    }

    fn close_jsx(&mut self, end: usize, scope_idx: Option<usize>, router_idx: Option<usize>, addressless: Option<(RouteScope, usize, bool)>) -> usize {
        if let Some(idx) = scope_idx {
            self.scopes[idx].end = end;
        }
        if let Some(idx) = router_idx {
            self.routers[idx].1 = end;
        }
        if let Some((mut route, tags, page)) = addressless {
            route.end = end;
            if self.route_tags.len() > tags {
                if route.component.is_some() || route.lazy.is_some() {
                    self.scopes.push(RouteScope { pathless: true, ..route });
                }
            } else if page {
                self.scopes.push(RouteScope { index: true, ..route });
            }
        }
        end
    }

    /// A child route's path is relative to the routes around it: compose each rendering route's
    /// path from the path-bearing scopes that contain it.
    fn finish(self, options: &ScanOptions) -> RouteScan {
        let scopes = &self.scopes;
        let prefix: &[RoutePart] = options.table.as_ref().map_or(&[], |t| &t.prefix);
        let inherited: &[String] = options.table.as_ref().map_or(&[], |t| &t.layouts);
        let renders = |s: &RouteScope| s.component.is_some() || s.lazy.is_some();
        let holds = |outer: &RouteScope, start: usize, end: usize| outer.start < start && outer.end >= end;
        // The scopes around a span, outermost first.
        let around = |start: usize, end: usize| -> Vec<usize> {
            let mut found: Vec<usize> = (0..scopes.len()).filter(|&n| holds(&scopes[n], start, end)).collect();
            found.sort_by_key(|&n| scopes[n].start);
            found
        };
        let shown = |p: &RoutePart| p.lit.clone().unwrap_or_else(|| format!("{{{}}}", p.expr.clone().unwrap_or_default()));
        let layout_of = |s: &RouteScope| s.component.clone().or_else(|| s.lazy.clone()).unwrap_or_default();
        let mut parts_of: Vec<Vec<RoutePart>> = Vec::with_capacity(scopes.len());
        let mut path_of: Vec<String> = Vec::with_capacity(scopes.len());
        for scope in scopes {
            let mut parts: Vec<RoutePart> = prefix.to_vec();
            parts.extend(around(scope.start, scope.end).into_iter().map(|n| scopes[n].part.clone()));
            parts.push(scope.part.clone());
            path_of.push(compose_route_path(&parts.iter().map(shown).collect::<Vec<_>>()));
            parts_of.push(parts);
        }
        // The page at its parent's address needs that address written down: a route around it
        // whose path the file spells out, or the router itself. At the top of a component's own
        // `<Routes>` it is wherever another route mounts the component.
        let in_router = |s: &RouteScope| self.routers.iter().any(|&(start, end)| start < s.start && end >= s.end);
        let addressed = |s: &RouteScope| -> bool {
            if !s.index {
                return true;
            }
            let outer = around(s.start, s.end);
            !outer.iter().any(|&n| scopes[n].opaque)
                && (outer.iter().any(|&n| scopes[n].part.lit.as_deref().is_some_and(|l| !l.is_empty()) || scopes[n].part.expr.as_deref().is_some_and(|e| !e.is_empty())) || in_router(s))
        };
        let pages: Vec<usize> = (0..scopes.len())
            .filter(|&n| {
                let s = &scopes[n];
                !s.pathless && !s.opaque && (s.jsx || s.outlet || renders(s)) && addressed(s)
            })
            .collect();
        let layout = |c: &RouteScope| renders(c) && !c.opaque;
        let mut routes: Vec<RouteDeclaration> = Vec::new();
        for &n in &pages {
            let scope = &scopes[n];
            let path = &path_of[n];
            // A route around others is the layout they render inside; when one of them claims
            // its address, the page there is that one — not the layout, nor a `<Route path>`
            // that only groups them.
            let claimed = (0..scopes.len()).any(|m| {
                let inner = &scopes[m];
                pages.contains(&m) && inner.jsx == scope.jsx && renders(inner) && holds(scope, inner.start, inner.end) && &path_of[m] == path
            });
            if claimed {
                continue;
            }
            // A route object renders inside the route objects around it; a `<Route>`, inside the
            // routes around it of either kind.
            let mut layouts: Vec<String> = inherited.to_vec();
            layouts.extend(around(scope.start, scope.end).into_iter().map(|m| &scopes[m]).filter(|c| layout(c) && (scope.jsx || !c.jsx)).map(layout_of));
            routes.push(RouteDeclaration {
                path: path.clone(),
                parts: parts_of[n].clone(),
                component: scope.component.clone(),
                lazy: scope.lazy.clone(),
                layouts,
                at: scope.at,
            });
        }
        routes.sort_by_key(|r| r.at);
        let mount = |at: usize| -> (Vec<RoutePart>, Vec<String>) {
            let outer = around(at, at);
            (
                outer.iter().map(|&n| scopes[n].part.clone()).collect(),
                outer.iter().map(|&n| &scopes[n]).filter(|c| layout(c)).map(layout_of).collect(),
            )
        };
        let names = self
            .names
            .iter()
            .map(|n| {
                let (parts, layouts) = mount(n.at);
                let mut full_prefix = prefix.to_vec();
                full_prefix.extend(parts);
                let mut full_layouts = inherited.to_vec();
                full_layouts.extend(layouts);
                NamedTable { table: n.clone(), prefix: full_prefix, layouts: full_layouts }
            })
            .collect();
        let uses = self
            .sites
            .iter()
            .map(|s| {
                let (parts, layouts) = mount(s.at);
                TableUse { name: s.name.clone(), literal: s.literal, prefix: parts, layouts }
            })
            .collect();
        RouteScan { routes, names, uses }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composes_nested_route_paths() {
        assert_eq!(compose_route_path(&["/app".into(), "users".into(), ":id".into()]), "/app/users/:id");
        assert_eq!(compose_route_path(&["/a".into(), "/b".into()]), "/b");
        assert_eq!(compose_route_path(&["".into()]), "/");
        assert_eq!(compose_route_path(&["/a/".into()]), "/a");
    }

    #[test]
    fn jsx_routes_nest_under_their_parents() {
        let src = r#"<Routes><Route path="/" element={<Layout />}><Route index element={<Home />} /><Route path="about" element={<About />} /></Route></Routes>"#;
        let routes = scan_route_declarations(src, true);
        let summary: Vec<(String, Option<String>)> = routes.iter().map(|r| (r.path.clone(), r.component.clone())).collect();
        assert_eq!(summary, vec![("/".to_string(), Some("Home".to_string())), ("/about".to_string(), Some("About".to_string()))]);
        assert_eq!(routes[1].layouts, vec!["Layout".to_string()]);
    }

    #[test]
    fn route_objects_are_read_in_a_router_file() {
        let src = "const router = createBrowserRouter([{ path: '/', element: <Root />, children: [{ path: 'team', element: <Team /> }] }]);";
        let routes = scan_route_declarations(src, true);
        assert!(routes.iter().any(|r| r.path == "/team" && r.component.as_deref() == Some("Team")));
    }
}
