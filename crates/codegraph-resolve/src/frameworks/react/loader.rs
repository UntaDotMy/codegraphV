//! What a data router's `lazy` loader renders, and the small expression reader it rests on —
//! the "loader" half of `src/resolution/frameworks/react.ts`.

use super::scan::element_tag;
use crate::frameworks::object_literal::{byte_at, ident_end, match_bracket, skip_string, skip_ws, slice, space_len_at, tail};
use crate::types::Tri;
use codegraph_jsre::re;
use codegraph_types::jscompat::{is_js_space, js_trim};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::rc::Rc;

pub const LAZY_ROUTE_PREFIX: &str = "lazy-import:";
/// React Router's `<Outlet />`: what a route with no element of its own renders, the routes inside it.
pub const OUTLET: &str = "Outlet";

/// A value a lazy loader hands over, as far as its source says: the module an `import('./x')`
/// loads, one export of it, a name of the route's own file, an object literal (read in the scope
/// it is written in), or the array a `Promise.all([…])` resolves to.
#[derive(Clone)]
pub enum Loaded {
    Module { spec: String },
    Export { spec: String, name: String },
    Name { name: String },
    Object { text: String, scope: Scope },
    Array { items: Vec<Option<Loaded>> },
}

/// The names a loader binds, each to what it holds: `None` for a value it does not read. Shared
/// and mutable — an object literal keeps the scope it is written in, which later declarations
/// of the same block extend.
pub type Scope = Rc<RefCell<IndexMap<String, Option<Loaded>>>>;

pub fn new_scope() -> Scope {
    Rc::new(RefCell::new(IndexMap::new()))
}

/// The first `import('…')` in `code`: the module it loads.
pub fn first_import(code: &str) -> Option<String> {
    re!(r#"\bimport\s*\(\s*["']([^"']+)["']\s*\)"#).captures(code).and_then(|c| c.text(1).map(String::from))
}

/// What a data-router `lazy` loader renders, as the route's reference names it. The loader
/// resolves to the route's properties, and their `Component`, or the page their `element` shows,
/// is what the route renders:
///
/// - `() => import('./x')` hands over the module's exports: `lazy-import:./x`.
/// - A loader that picks the page names the export it picks, `import:./x#Page`.
/// - A component of the route's own file it returns is named as written.
///
/// `Outlet` when it hands back React Router's. `None` when it renders nothing, handing over only
/// a `loader`. A loader this does not read loads the module its first import names.
pub fn lazy_route_reference(loader: Option<&str>) -> Option<String> {
    let loader = loader.filter(|l| !l.is_empty())?;
    let stripped = codegraph_text::strip_comments_for_regex(loader, "typescript");
    let code = js_trim(&stripped);
    let rendered = if code.starts_with('{') {
        lazy_properties_render(code)
    } else {
        route_render(loader_value(code, &new_scope(), None).as_ref())
    };
    match rendered {
        Tri::Null => return None,
        Tri::Value(v) => return Some(v),
        Tri::Undefined => {}
    }
    first_import(code).map(|spec| format!("{LAZY_ROUTE_PREFIX}{spec}"))
}

/// What route properties a loader resolves to render: the reference for its page, `Outlet` for
/// React Router's, null for none, undefined for properties this does not read.
fn route_render(value: Option<&Loaded>) -> Tri<String> {
    if let Some(Loaded::Module { spec }) = value {
        return Tri::Value(format!("{LAZY_ROUTE_PREFIX}{spec}"));
    }
    let Some(Loaded::Object { text, scope }) = value else { return Tri::Undefined };
    let (props, spreads) = object_entries(text);
    let component = props.get("Component");
    let element = props.get("element");
    if let Some(shown) = component.or(element) {
        let held = component.and_then(|c| value_of(c, scope));
        if let Some(named) = page_reference(held.as_ref()) {
            return Tri::Value(named);
        }
        // The one export the loader imported that it shows, past a guard of the route file's own:
        // `<AgeGate><FireworksPage /></AgeGate>`, `() => <PrivateRoute component={<Favorites />} />`,
        // `withAuth(Page)`.
        let loaded = loaded_exports(shown, scope);
        if loaded.len() == 1 {
            return page_reference(Some(&loaded[0])).map_or(Tri::Undefined, Tri::Value);
        }
        let tag = if component.is_none() && loaded.is_empty() { element.and_then(|e| element_tag(Some(e))) } else { None };
        // `Component: Outlet`, `element: <Outlet />`: the routes inside render in its place.
        if matches!(&held, Some(Loaded::Name { name }) if name == OUTLET) || tag.as_deref() == Some(OUTLET) {
            return Tri::Value(OUTLET.to_string());
        }
        return match tag {
            Some(t) => page_reference(name_value(&t, scope).as_ref()).map_or(Tri::Undefined, Tri::Value),
            None => Tri::Undefined,
        };
    }
    // `{ ...module }`: the module's exports are the route's properties.
    if spreads.is_empty() {
        return Tri::Null;
    }
    let spread = if spreads.len() == 1 { value_of(&spreads[0], scope) } else { None };
    match spread {
        Some(Loaded::Module { spec }) => Tri::Value(format!("{LAZY_ROUTE_PREFIX}{spec}")),
        _ => Tri::Undefined,
    }
}

/// React Router 7's object of lazy properties, `lazy: { loader: …, Component: async () =>
/// (await import('./x')).Page }`: each loads on its own, and the `Component`'s loader resolves to
/// the page itself.
fn lazy_properties_render(code: &str) -> Tri<String> {
    if match_bracket(code, 0) != Some(code.len() - 1) {
        return Tri::Undefined;
    }
    let (props, _) = object_entries(&code[1..code.len() - 1]);
    let Some(component) = props.get("Component") else { return Tri::Null };
    if let Some(page) = page_reference(loader_value(component, &new_scope(), None).as_ref()) {
        return Tri::Value(page);
    }
    match first_import(component) {
        Some(spec) => Tri::Value(format!("{LAZY_ROUTE_PREFIX}{spec}")),
        None => Tri::Undefined,
    }
}

/// The distinct exports `text` uses through the names a loader bound them to: `Page`, `m.Page`.
fn loaded_exports(text: &str, scope: &Scope) -> Vec<Loaded> {
    let mut found: IndexMap<String, Loaded> = IndexMap::new();
    for m in re!(r"(?<![\w$.])([A-Za-z_$][\w$]*)(?:\s*\??\.\s*([A-Za-z_$][\w$]*))?", "g").captures_iter(text) {
        let bound = scope.borrow().get(m.text(1).unwrap_or("")).cloned().flatten();
        let value = match (&bound, m.text(2)) {
            (Some(module @ Loaded::Module { .. }), Some(member)) => member_of(Some(module), member),
            _ => bound,
        };
        if let Some(export @ Loaded::Export { spec, name }) = &value {
            found.insert(format!("{spec}#{name}"), export.clone());
        }
    }
    found.into_values().collect()
}

/// The reference for the page a value is: an export a loader picks, or a component name of the
/// route's file.
fn page_reference(value: Option<&Loaded>) -> Option<String> {
    match value {
        Some(Loaded::Export { spec, name }) => Some(format!("import:{spec}#{name}")),
        Some(Loaded::Name { name }) if name.starts_with(|c: char| c.is_ascii_uppercase()) && name != OUTLET => Some(name.clone()),
        _ => None,
    }
}

/// What a lazy wrapper's loader resolves to as a page: a module's default export, an export it
/// picks, or the `default` of the object `React.lazy` wants (`{ default: m.Chart }`).
pub fn loaded_page(value: Option<&Loaded>) -> Option<(String, String)> {
    match value? {
        Loaded::Module { spec } => Some((spec.clone(), "default".to_string())),
        Loaded::Export { spec, name } => Some((spec.clone(), name.clone())),
        Loaded::Object { text, scope } => {
            let (props, _) = object_entries(text);
            let inner = props.get("default")?;
            loaded_page(value_of(inner, scope).as_ref())
        }
        _ => None,
    }
}

/// What the function `text` resolves to when called, `param` handed to its first parameter: its
/// expression body's value, or what the first `return` of its body hands back. `None` for a
/// function this does not read.
pub fn loader_value(text: &str, outer: &Scope, param: Option<Loaded>) -> Option<Loaded> {
    let function = function_parts(text)?;
    let scope: Scope = Rc::new(RefCell::new(outer.borrow().clone()));
    for (i, p) in top_level_items(&function.params).iter().enumerate() {
        bind_pattern(&scope, &parameter_pattern(p), if i == 0 { param.as_ref() } else { None });
    }
    if function.block {
        block_value(&function.body, &scope)
    } else {
        value_of(&function.body, &scope)
    }
}

pub struct FunctionParts {
    pub params: String,
    pub body: String,
    pub block: bool,
}

/// An arrow function's or a `function`'s parameter list and body, its `async` and return type
/// skipped. `None` for any other expression.
pub fn function_parts(text: &str) -> Option<FunctionParts> {
    let trimmed = js_trim(text);
    let mut s: &str = match re!(r"^async\b\s*").find(trimmed) {
        Some(m) => &trimmed[m.end()..],
        None => trimmed,
    };
    let keyword = re!(r"^function\b\s*\*?\s*(?:[A-Za-z_$][\w$]*\s*)?").find(s);
    let is_keyword = keyword.is_some();
    if let Some(k) = keyword {
        s = &s[k.end()..];
    }
    let params: &str;
    let mut rest: &str;
    if s.starts_with('(') {
        let close = match_bracket(s, 0)?;
        params = &s[1..close];
        rest = s[close + 1..].trim_start_matches(is_js_space);
    } else {
        let id_end = if is_keyword { None } else { ident_end(s, 0) }?;
        params = &s[..id_end];
        rest = s[id_end..].trim_start_matches(is_js_space);
    }
    // `async (): Promise<RouteProps> => …`
    if !is_keyword && rest.starts_with(':') {
        let arrow = arrow_at(rest)?;
        rest = &rest[arrow..];
    }
    if is_keyword {
        return if rest.starts_with('{') && match_bracket(rest, 0) == Some(rest.len() - 1) {
            Some(FunctionParts { params: params.to_string(), body: rest[1..rest.len() - 1].to_string(), block: true })
        } else {
            None
        };
    }
    if !rest.starts_with("=>") {
        return None;
    }
    let body = js_trim(&rest[2..]);
    if !body.starts_with('{') {
        return Some(FunctionParts { params: params.to_string(), body: body.to_string(), block: false });
    }
    if match_bracket(body, 0) == Some(body.len() - 1) {
        Some(FunctionParts { params: params.to_string(), body: body[1..body.len() - 1].to_string(), block: true })
    } else {
        None
    }
}

/// Where the first `=>` outside brackets and strings is in `text`.
fn arrow_at(text: &str) -> Option<usize> {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let ch = b[i];
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            i = skip_string(text, i)?;
        } else if ch == b'{' || ch == b'(' || ch == b'[' {
            i = match_bracket(text, i)?;
        } else if ch == b'=' && b.get(i + 1) == Some(&b'>') {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// A parameter's binding pattern, without its type or default: `{ default: Page }` of `{ default:
/// Page }: Module`.
fn parameter_pattern(param: &str) -> String {
    let param = re!(r"^\.\.\.\s*").replace_first(param, |_| String::new());
    if param.starts_with('{') || param.starts_with('[') {
        return match match_bracket(&param, 0) {
            Some(close) => param[..=close].to_string(),
            None => String::new(),
        };
    }
    ident_end(&param, 0).map(|end| param[..end].to_string()).unwrap_or_default()
}

/// The value a function body hands back from its first top-level `return`, its declarations
/// bound on the way there. Nested blocks and functions are stepped over: what is declared or
/// returned in there is theirs.
fn block_value(body: &str, scope: &Scope) -> Option<Loaded> {
    let b = body.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let ch = b[i];
        if matches!(ch, b'"' | b'\'' | b'`' | b'{' | b'(' | b'[') {
            let end = if matches!(ch, b'{' | b'(' | b'[') { match_bracket(body, i) } else { skip_string(body, i) };
            i = end? + 1;
            continue;
        }
        let Some(word_end) = ident_end(body, i) else {
            i += 1;
            continue;
        };
        let w = &body[i..word_end];
        let after = word_end;
        let statement = i == 0 || !{
            let p = b[i - 1];
            p.is_ascii_alphanumeric() || p == b'_' || p == b'$' || p == b'.'
        };
        i = after;
        if !statement {
            continue;
        }
        if w == "return" {
            return value_of(slice(body, after, statement_end(body, after, false)), scope);
        }
        if w == "const" || w == "let" || w == "var" {
            i = declare(body, after, scope);
        } else if w == "function" || w == "class" {
            // `\s*\*?\s*([A-Za-z_$][\w$]*)` over a 128-unit window.
            let window = slice(body, after, after + 128);
            if let Some(name) = re!(r"^\s*\*?\s*([A-Za-z_$][\w$]*)").captures(window).and_then(|c| c.text(1).map(String::from)) {
                scope.borrow_mut().insert(name, None);
            }
        }
    }
    None
}

/// Bind each declarator of a `const`, `let` or `var` whose first pattern starts at `at`; where
/// the declaration ends.
fn declare(body: &str, at: usize, scope: &Scope) -> usize {
    let mut i = at;
    loop {
        i = skip_ws(body, i);
        let end = if matches!(byte_at(body, i), b'{' | b'[') {
            match_bracket(body, i).map_or(0, |close| close + 1)
        } else {
            i + ident_end(body, i).map_or(0, |e| (e - i).min(128))
        };
        if end <= i {
            return end.max(i + 1);
        }
        let pattern = slice(body, i, end);
        let init = initializer_at(body, end);
        let mut stop = end;
        match init {
            None => bind_pattern(scope, pattern, None),
            Some(init) => {
                stop = statement_end(body, init, true);
                let value = value_of(slice(body, init, stop), scope);
                bind_pattern(scope, pattern, value.as_ref());
            }
        }
        stop = skip_ws(body, stop);
        if byte_at(body, stop) != b',' {
            return stop;
        }
        i = stop + 1;
    }
}

/// Where a declarator's initializer starts, past its `: Type` and `=`; `None` when it has none.
pub fn initializer_at(body: &str, at: usize) -> Option<usize> {
    let b = body.as_bytes();
    let mut i = at;
    while i < b.len() {
        let ch = b[i];
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            i = skip_string(body, i)?;
        } else if ch == b'{' || ch == b'(' || ch == b'[' {
            i = match_bracket(body, i)?;
        } else if ch == b'=' {
            if matches!(b.get(i + 1), Some(b'>') | Some(b'=')) {
                i += 1;
            } else {
                return Some(i + 1);
            }
        } else if matches!(ch, b';' | b',' | b'}' | b')') {
            return None;
        }
        i += 1;
    }
    None
}

/// Where the statement or declarator from `from` ends: at a `;`, a closing bracket, a `,` when
/// `comma`, or at a line break neither line continues across, where JavaScript ends a statement
/// without a semicolon.
pub fn statement_end(text: &str, from: usize, comma: bool) -> usize {
    let b = text.as_bytes();
    let mut last = 0u8;
    let mut i = from;
    while i < b.len() {
        let ch = b[i];
        if matches!(ch, b'"' | b'\'' | b'`' | b'{' | b'(' | b'[') {
            let end = if matches!(ch, b'{' | b'(' | b'[') { match_bracket(text, i) } else { skip_string(text, i) };
            let Some(end) = end else { return b.len() };
            last = b[end];
            i = end + 1;
            continue;
        }
        if matches!(ch, b';' | b'}' | b')' | b']') || (comma && ch == b',') {
            return i;
        }
        if ch == b'\n' {
            let j = skip_ws(text, i + 1);
            let next = byte_at(text, j);
            if last != 0 && !b"=(,[{?:&|+-*/%<>!.".contains(&last) && !(next != 0 && b".?:&|+-*/%=<>,([".contains(&next)) {
                return i;
            }
            i += 1;
            continue;
        }
        let n = space_len_at(text, i);
        if n > 0 {
            i += n;
            continue;
        }
        last = ch;
        i += 1;
    }
    b.len()
}

/// What expression `text` evaluates to in a loader, or `None` when it is not one this reads.
pub fn value_of(text: &str, scope: &Scope) -> Option<Loaded> {
    let mut s = js_trim(text).to_string();
    // `await`, and parentheses around the whole expression, change nothing here.
    let mut before = String::new();
    while before != s {
        before = s.clone();
        s = re!(r"^await\b\s*").replace_first(&s, |_| String::new());
        if s.starts_with('(') && match_bracket(&s, 0) == Some(s.len() - 1) {
            s = js_trim(&s[1..s.len() - 1]).to_string();
        }
    }
    if s.starts_with('{') || s.starts_with('[') {
        if match_bracket(&s, 0) != Some(s.len() - 1) {
            return None;
        }
        let inside = &s[1..s.len() - 1];
        return Some(if s.starts_with('{') {
            Loaded::Object { text: inside.to_string(), scope: Rc::clone(scope) }
        } else {
            Loaded::Array { items: top_level_items(inside).iter().map(|item| if item.is_empty() { None } else { value_of(item, scope) }).collect() }
        });
    }
    // The head: `import('./x')`, `Promise.all([…])`, a name, or a parenthesized expression.
    let head = re!(r"^(?:(import)\s*\(|(Promise)\s*\.\s*all\s*\(|([A-Za-z_$][\w$]*)|\()").captures(&s)?;
    let head_len = head.get(0)?.end();
    let mut value: Option<Loaded>;
    let mut i: usize;
    if let Some(name) = head.text(3) {
        value = name_value(name, scope);
        i = head_len;
    } else {
        let open = head_len - 1;
        let close = match_bracket(&s, open)?;
        let inside = &s[open + 1..close];
        value = if head.get(1).is_some() {
            re!(r#"^\s*(["'])([^"'`]+)\1\s*$"#).captures(inside).and_then(|c| c.text(2).map(|spec| Loaded::Module { spec: spec.to_string() }))
        } else if head.get(2).is_some() {
            value_of(inside, scope).filter(|all| matches!(all, Loaded::Array { .. }))
        } else {
            value_of(inside, scope)
        };
        i = close + 1;
    }
    // Then each `.name`, `?.name`, `['name']` and `.then(…)` on it.
    while let Some(current) = value.clone() {
        if i >= s.len() {
            break;
        }
        let rest = &s[i..];
        if let Some(then) = re!(r"^\s*\??\.\s*then\s*\(").captures(rest) {
            let open = i + then.get(0)?.end() - 1;
            let close = match_bracket(&s, open)?;
            if !matches!(current, Loaded::Module { .. }) {
                return None;
            }
            let callback = top_level_items(&s[open + 1..close]).into_iter().next().unwrap_or_default();
            value = if callback.is_empty() { None } else { loader_value(&callback, scope, Some(current)) };
            i = close + 1;
            continue;
        }
        let dot = re!(r"^\s*\??\.\s*([A-Za-z_$][\w$]*)").captures(rest);
        let bracket = if dot.is_none() { re!(r#"^\s*\[\s*(["'])([A-Za-z_$][\w$]*)\1\s*\]"#).captures(rest) } else { None };
        let (name, len) = match (&dot, &bracket) {
            (Some(d), _) => (d.text(1)?, d.get(0)?.end()),
            (None, Some(br)) => (br.text(2)?, br.get(0)?.end()),
            (None, None) => return None,
        };
        value = member_of(Some(&current), name);
        i += len;
    }
    value
}

/// What a name holds in a loader: what it binds there, or else a name of the route's own file.
pub fn name_value(name: &str, scope: &Scope) -> Option<Loaded> {
    match scope.borrow().get(name) {
        Some(bound) => bound.clone(),
        None => Some(Loaded::Name { name: name.to_string() }),
    }
}

/// A property of a value: an export of a module, or what an object literal holds under `key`.
pub fn member_of(value: Option<&Loaded>, key: &str) -> Option<Loaded> {
    match value? {
        Loaded::Module { spec } => Some(Loaded::Export { spec: spec.clone(), name: key.to_string() }),
        Loaded::Object { text, scope } => {
            let (props, _) = object_entries(text);
            value_of(props.get(key)?, scope)
        }
        _ => None,
    }
}

/// Bind the names `pattern` declares to the parts of `value` they take: `{ Page, default: Home }`
/// of a module to its exports, `[{ Page }]` to the first item of a `Promise.all`. A name whose
/// value this does not read is bound to `None`, so it never counts as a name of the route's file.
fn bind_pattern(scope: &Scope, pattern: &str, value: Option<&Loaded>) {
    let p = js_trim(pattern);
    if p.is_empty() {
        return;
    }
    if ident_end(p, 0) == Some(p.len()) {
        scope.borrow_mut().insert(p.to_string(), value.cloned());
        return;
    }
    let close = if p.starts_with('{') || p.starts_with('[') { match_bracket(p, 0) } else { None };
    if close != Some(p.len() - 1) {
        for name in re!(r"[A-Za-z_$][\w$]*", "g").captures_iter(p) {
            scope.borrow_mut().insert(name.text(0).unwrap_or("").to_string(), None);
        }
        return;
    }
    let items = top_level_items(&p[1..p.len() - 1]);
    for (index, item) in items.iter().enumerate() {
        if item.is_empty() {
            continue;
        }
        if let Some(rest) = re!(r"^\.\.\.\s*([\s\S]*)$").captures(item) {
            bind_pattern(scope, rest.text(1).unwrap_or(""), None);
            continue;
        }
        if p.starts_with('[') {
            let element = match value {
                Some(Loaded::Array { items }) => items.get(index).cloned().flatten(),
                _ => None,
            };
            bind_pattern(scope, &re!(r"\s*=(?![=>])[\s\S]*$").replace_first(item, |_| String::new()), element.as_ref());
            continue;
        }
        // `Page`, `Page = Fallback`, `default: Home`, `'default': Home`.
        let prop = re!(r#"^(?:([A-Za-z_$][\w$]*)|(["'])([^"']*)\2)\s*(?::\s*([\s\S]+?))?\s*(?:=(?![=>])[\s\S]*)?$"#).captures(item);
        let Some(prop) = prop else {
            bind_pattern(scope, &re!(r"^[^:]*:").replace_first(item, |_| String::new()), None);
            continue;
        };
        let key = prop.text(1).or_else(|| prop.text(3)).unwrap_or("");
        bind_pattern(scope, prop.text(4).unwrap_or(key), member_of(value, key).as_ref());
    }
}

/// An object literal's own properties, each name to its value's text — shorthand and methods
/// included — and its spreads.
pub fn object_entries(text: &str) -> (IndexMap<String, String>, Vec<String>) {
    let mut props: IndexMap<String, String> = IndexMap::new();
    let mut spreads: Vec<String> = Vec::new();
    for entry in top_level_entries(text) {
        if let Some(spread) = re!(r"^\.\.\.\s*([\s\S]+)$").captures(&entry) {
            spreads.push(spread.text(1).unwrap_or("").to_string());
            continue;
        }
        if let Some(pair) = re!(r#"^(?:([A-Za-z_$][\w$]*)|(["'])([A-Za-z_$][\w$]*)\2)\s*:\s*([\s\S]+)$"#).captures(&entry) {
            let key = pair.text(1).or_else(|| pair.text(3)).unwrap_or("");
            props.insert(key.to_string(), pair.text(4).unwrap_or("").to_string());
            continue;
        }
        if let Some(method) = re!(r"^(async\s+)?([A-Za-z_$][\w$]*)\s*\(").captures(&entry) {
            let paren = method.get(0).map_or(0, |m| m.end()) - 1;
            props.insert(method.text(2).unwrap_or("").to_string(), format!("{}function {}", method.text(1).unwrap_or(""), tail(&entry, paren)));
            continue;
        }
        if ident_end(&entry, 0) == Some(entry.len()) {
            props.insert(entry.clone(), entry.clone());
        }
    }
    (props, spreads)
}

/// The comma-separated entries of a list's inside, nested brackets kept whole.
pub fn top_level_entries(text: &str) -> Vec<String> {
    top_level_items(text).into_iter().filter(|s| !s.is_empty()).collect()
}

/// The same, an empty entry kept for each hole: `[, second]` is `['', 'second']`.
pub fn top_level_items(text: &str) -> Vec<String> {
    let b = text.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut depth = 0i32;
    let mut from = 0;
    let mut i = 0;
    while i < b.len() {
        let ch = b[i];
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            match skip_string(text, i) {
                Some(end) => i = end,
                None => break,
            }
        } else if matches!(ch, b'{' | b'[' | b'(') {
            depth += 1;
        } else if matches!(ch, b'}' | b']' | b')') {
            depth -= 1;
        } else if ch == b',' && depth == 0 {
            out.push(js_trim(slice(text, from, i)).to_string());
            from = i + 1;
        }
        i += 1;
    }
    // A trailing comma ends a list without adding a hole.
    let last = js_trim(tail(text, from));
    if !last.is_empty() {
        out.push(last.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lazy_import_loader_names_the_module() {
        assert_eq!(lazy_route_reference(Some("() => import('./routes/x')")).as_deref(), Some("lazy-import:./routes/x"));
    }

    #[test]
    fn loader_picking_an_export_names_it() {
        assert_eq!(
            lazy_route_reference(Some("async () => { const { Page } = await import('./x'); return { Component: Page }; }")).as_deref(),
            Some("import:./x#Page")
        );
        assert_eq!(
            lazy_route_reference(Some("() => import('./x').then((m) => ({ Component: m.Page }))")).as_deref(),
            Some("import:./x#Page")
        );
    }

    #[test]
    fn statement_ends_at_line_breaks_without_semicolons() {
        let text = "a = b\nc = d";
        assert_eq!(statement_end(text, 0, false), 5);
        let text = "a = b +\n c";
        assert_eq!(statement_end(text, 0, false), text.len());
    }
}
