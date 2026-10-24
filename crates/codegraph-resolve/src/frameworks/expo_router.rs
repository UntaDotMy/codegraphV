//! Expo Router (React Native) — file-based screens and string-keyed navigation. Port of
//! `src/resolution/frameworks/expo-router.ts`.
//!
//! Two things static extraction cannot see on its own, and that together are most of what "how
//! does the app flow" means in an Expo app:
//!
//! 1. **A screen is a file, not a symbol.** Every file under `app/` (or `src/app/`) is a route:
//!    `app/object-detail.tsx` is `/object-detail`, `app/capture/index.tsx` is `/capture`,
//!    `app/item/[id].tsx` is `/item/[id]`, and `(group)` directories are invisible in the URL.
//!    `extract()` emits one `route` node per screen file, named by its path, with a `calls` ref to
//!    the file's default export so the route reaches the component that renders it.
//!
//! 2. **Navigation is a string.** `router.push('/object-detail?…')`, `router.navigate({ pathname:
//!    '/item/[id]', params })`, a template literal with the params interpolated — the extractor
//!    records each as a `calls` ref named `router.push` that resolves to nothing, because the
//!    target is a path, not an identifier. `resolve()` claims those refs, reads the argument off
//!    the source lines, matches it against the route table, and returns a **`navigates`** edge to
//!    the route node, carrying the href it read.
//!
//! Precision rests on the string resolving to a real screen file, not on the receiver being called
//! `router`. Anything the resolver cannot bind to exactly one screen — a computed path, an
//! ambiguous dynamic match, a relative href from a non-screen file — is left unresolved rather
//! than guessed. Silent beats wrong.

use super::now_ms;
use super::package_deps::depends_on;
use crate::ctx::Ctx;
use crate::types::{AlsoTarget, FrameworkExtractionResult, FrameworkResolver, NavigationCalls, NodeList, NodeRef, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::{js_trim, utf16_byte_index};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::rc::Rc;

// -- Route files ----------------------------------------------------------------------------------

const ROUTE_LANGUAGES: [Language; 4] = [Language::TypeScript, Language::JavaScript, Language::Tsx, Language::Jsx];

/// The app directory: `app/` at the project root, or `src/app/`. First match wins.
fn app_dir_re() -> &'static Re {
    re!(r"(?:^|\/)(?:src\/)?app\/")
}

/// A screen file's extension, with an optional platform suffix (`.ios.tsx`).
fn route_ext_re() -> &'static Re {
    re!(r"\.(?:(?:ios|android|native|web)\.)?(tsx|ts|jsx|js|mjs|cjs)$")
}

/// Files under `app/` that define a screen (not a layout, not test, not html).
pub fn route_path_for_file(file_path: &str) -> Option<String> {
    let dir = app_dir_re().find(file_path)?;
    let rel = &file_path[dir.end()..];
    let ext = route_ext_re().find(rel)?;
    let bare = &rel[..ext.start()];
    if bare.ends_with(".d") || re!(r"\.(?:test|spec|stories)$").is_match(bare) {
        return None;
    }
    let segs: Vec<&str> = bare.split('/').collect();
    if segs.contains(&"__tests__") || segs.contains(&"__mocks__") {
        return None;
    }
    let base = segs[segs.len() - 1];
    // `hello+api.ts` is an endpoint (`api_route_path_for_file`), not a screen.
    if base.ends_with("+api") {
        return None;
    }
    // `_layout` (and any other `_`-prefixed file) is not navigable. `+not-found` is a real
    // screen; the other `+` files (`+html`, `+native-intent`) are not.
    if base.starts_with('_') {
        return None;
    }
    if base.starts_with('+') && base != "+not-found" {
        return None;
    }
    let mut kept: Vec<&str> = segs.into_iter().filter(|s| !(s.starts_with('(') && s.ends_with(')'))).collect();
    if kept.last() == Some(&"index") {
        kept.pop();
    }
    Some(format!("/{}", kept.join("/")))
}

/// An API route's path — `app/blog/og-image/[post]+api.ts` is `/blog/og-image/:post`, written the
/// way every server route is so a client's `fetch('/blog/og-image/…')` can find it. `None` for any
/// other file.
pub fn api_route_path_for_file(file_path: &str) -> Option<String> {
    let dir = app_dir_re().find(file_path)?;
    let rel = &file_path[dir.end()..];
    let ext = re!(r"\.(tsx|ts|jsx|js|mjs|cjs)$").find(rel)?;
    let bare = &rel[..ext.start()];
    let bare = bare.strip_suffix("+api")?;
    let mut segs: Vec<&str> = bare.split('/').filter(|seg| !seg.is_empty() && !(seg.starts_with('(') && seg.ends_with(')'))).collect();
    if segs.last() == Some(&"index") {
        segs.pop();
    }
    let mapped: Vec<String> = segs
        .iter()
        .map(|seg| {
            let rest = re!(r"^\[\.\.\.(.+)\]$").replace_first(seg, |c| format!(":{}*", c.text(1).unwrap_or("")));
            re!(r"^\[(.+)\]$").replace_first(&rest, |c| format!(":{}", c.text(1).unwrap_or("")))
        })
        .collect();
    Some(format!("/{}", mapped.join("/")))
}

const API_METHODS: &str = "GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS";

fn language_for_file(file_path: &str) -> Language {
    match route_ext_re().captures(file_path).and_then(|c| c.text(1).map(String::from)).as_deref() {
        Some("tsx") => Language::Tsx,
        Some("jsx") => Language::Jsx,
        Some("ts") => Language::TypeScript,
        _ => Language::JavaScript,
    }
}

const IDENT: &str = r"[A-Za-z_$][\w$]*";

/// The name the file exports as its screen. Expo renders the DEFAULT export, so that is the only
/// binding that matters; a wrapper (`memo(Screen)`, `observer(Screen)`, `React.forwardRef(Screen)`)
/// is looked through to its first identifier argument. Anonymous defaults (`export default () =>
/// …`) have no name to bind and yield `None`.
pub fn default_export_name(stripped: &str) -> Option<(String, usize)> {
    let patterns: [(String, &str); 5] = [
        (format!(r"export\s+default\s+(?:async\s+)?function\s*\*?\s*({IDENT})"), ""),
        (format!(r"export\s+default\s+class\s+({IDENT})"), ""),
        (format!(r"export\s+default\s+(?:{IDENT}\.)?{IDENT}\s*\(\s*({IDENT})\s*[,)]"), ""),
        (format!(r"export\s+default\s+({IDENT})\s*;?\s*$"), "m"),
        (format!(r"export\s*\{{\s*({IDENT})\s+as\s+default\s*\}}"), ""),
    ];
    for (pattern, flags) in &patterns {
        let regex = Re::cached(pattern, flags);
        if let Some(m) = regex.captures(stripped) {
            if let Some(name) = m.text(1).filter(|n| !n.is_empty() && *n != "default") {
                return Some((name.to_string(), m.get(0).map_or(0, |w| w.start())));
            }
        }
    }
    None
}

// -- Reading the href out of a navigation call -------------------------------------------------------

/// The `router` methods that take a destination (`back`/`dismiss` take none), and a project's own
/// wrappers around them: `safePush('/x')`, `guardedNavigate('/x')` — a camelCase name ending in
/// the verb. A wrapper usually defers the real call through state the graph cannot follow, so its
/// NAME is the only static evidence; the argument resolving to a real screen is what makes the
/// claim safe. Second group: the verb, when it came from a wrapper.
pub fn nav_method_re() -> &'static Re {
    re!(r"(?:^|\.)(push|replace|navigate|dismissTo)$|^[a-z][A-Za-z]*(Push|Replace|Navigate)$")
}

/// The verb a NAV_METHOD match names, lower-cased: `safePush` → `push`.
pub fn nav_verb(name: &str) -> Option<String> {
    let m = nav_method_re().captures(name)?;
    // A router method is already the verb (`dismissTo`); a wrapper's suffix (`safePush` →
    // `Push`) is lower-cased to name the verb it stands in for.
    Some(m.text(1).map(String::from).unwrap_or_else(|| m.text(2).unwrap_or("").to_lowercase()))
}

/// Lines a single navigation call is allowed to span.
const MAX_CALL_LINES: usize = 12;

/// Placeholder for an interpolated `${…}` inside a template-literal href (shared with the
/// cross-tier synthesizer).
pub const HOLE: char = '\u{0}';

/// Index of the `)` matching the `(` at `open`, skipping string bodies.
fn match_paren(s: &str, open: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut i = open;
    while i < b.len() {
        let ch = b[i];
        if ch == b'"' || ch == b'\'' {
            let q = ch;
            i += 1;
            while i < b.len() && b[i] != q {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if ch == b'`' {
            i = skip_template(s, i) + 1;
            continue;
        }
        if ch == b'(' {
            depth += 1;
        } else if ch == b')' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Index of the closing backtick for the template starting at `open` (or the end of `s`).
fn skip_template(s: &str, open: usize) -> usize {
    let b = s.as_bytes();
    let mut i = open + 1;
    while i < b.len() {
        let ch = b[i];
        if ch == b'\\' {
            i += 2;
            continue;
        }
        if ch == b'`' {
            return i;
        }
        if ch == b'$' && b.get(i + 1) == Some(&b'{') {
            let mut depth = 0i32;
            i += 1;
            while i < b.len() {
                if b[i] == b'{' {
                    depth += 1;
                } else if b[i] == b'}' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                } else if b[i] == b'`' {
                    i = skip_template(s, i);
                }
                i += 1;
            }
        }
        i += 1;
    }
    b.len()
}

/// Index of the quote that closes the string literal opening at `at` (or the end of `s`).
pub fn string_end(s: &str, at: usize) -> usize {
    let b = s.as_bytes();
    let q = b.get(at).copied().unwrap_or(0);
    if q == b'`' {
        return skip_template(s, at);
    }
    let mut i = at + 1;
    while i < b.len() {
        if b[i] == b'\\' {
            i += 1;
        } else if b[i] == q {
            return i;
        }
        i += 1;
    }
    b.len()
}

/// The char at byte `i`, and its length.
fn char_at(s: &str, i: usize) -> Option<(char, usize)> {
    s.get(i..)?.chars().next().map(|c| (c, c.len_utf8()))
}

/// A string literal (`'…'`, `"…"`, or a template) starting at `at`, as text with every `${…}`
/// replaced by [`HOLE`]. `None` when `at` is not a string.
pub fn read_string_at(s: &str, at: usize) -> Option<String> {
    let b = s.as_bytes();
    let q = b.get(at).copied()?;
    if q == b'"' || q == b'\'' {
        let mut out = String::new();
        let mut i = at + 1;
        while let Some((ch, len)) = char_at(s, i) {
            if ch == '\\' {
                if let Some((next, next_len)) = char_at(s, i + len) {
                    out.push(next);
                    i += len + next_len;
                } else {
                    i += len;
                }
                continue;
            }
            if ch == q as char {
                return Some(out);
            }
            out.push(ch);
            i += len;
        }
        return None;
    }
    if q == b'`' {
        let end = skip_template(s, at);
        let mut out = String::new();
        let mut i = at + 1;
        while i < end {
            let Some((ch, len)) = char_at(s, i) else { break };
            if ch == '\\' {
                if let Some((next, next_len)) = char_at(s, i + len) {
                    out.push(next);
                    i += len + next_len;
                } else {
                    i += len;
                }
                continue;
            }
            if ch == '$' && b.get(i + 1) == Some(&b'{') {
                let mut depth = 0i32;
                i += 1;
                while i < end {
                    if b[i] == b'{' {
                        depth += 1;
                    } else if b[i] == b'}' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    i += 1;
                }
                out.push(HOLE);
                i += 1;
                continue;
            }
            out.push(ch);
            i += len;
        }
        return Some(out);
    }
    None
}

#[derive(Debug, Clone, PartialEq)]
pub struct HrefLiteral {
    /// The path part, `${…}` holes kept as [`HOLE`]; query and hash removed.
    pub path: String,
    /// The literal as written, holes rendered as `${…}` — for the edge metadata.
    pub display: String,
    /// The OTHER destinations, when the argument was a conditional. A link written `!isAdmin ?
    /// keyword ? '/search/…' : '/page/…' : '/admin/…'` names three places a user can end up, and
    /// each is drawn.
    pub alternates: Option<Vec<HrefLiteral>>,
}

/// Every destination an href names — itself first, then its other arms.
pub fn href_arms(href: &HrefLiteral) -> Vec<&HrefLiteral> {
    let mut arms = vec![href];
    if let Some(alternates) = href.alternates.as_ref().filter(|a| !a.is_empty()) {
        arms.extend(alternates.iter());
    }
    arms
}

/// Index of the first `ch` at bracket depth 0 and outside strings.
fn index_at_depth0(s: &str, ch: u8, from: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut i = from;
    while i < b.len() {
        let c = b[i];
        if c == b'"' || c == b'\'' {
            let q = c;
            i += 1;
            while i < b.len() && b[i] != q {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if c == b'`' {
            i = skip_template(s, i) + 1;
            continue;
        }
        if c == b'(' || c == b'[' || c == b'{' {
            depth += 1;
        } else if c == b')' || c == b']' || c == b'}' {
            depth -= 1;
        } else if c == b'?' && matches!(b.get(i + 1), Some(b'.') | Some(b'?')) {
            i += 1; // `?.` / `??`
        } else if depth == 0 && c == ch {
            return Some(i);
        }
        i += 1;
    }
    None
}

pub fn to_href(literal: Option<String>) -> Option<HrefLiteral> {
    let literal = literal.filter(|l| !l.is_empty())?;
    let cut = literal.find(['?', '#']);
    let path = match cut {
        Some(c) => literal[..c].to_string(),
        None => literal.clone(),
    };
    if path.is_empty() {
        return None;
    }
    Some(HrefLiteral { path, display: literal.replace(HOLE, "${…}"), alternates: None })
}

/// The `:` that closes the ternary opened at `q`, honouring nested ones.
///
/// Taking the FIRST `:` splits `a ? b ? '/x' : '/y' : '/z'` between `b` and `'/y'`, which reads as
/// `'/y'` — a real path, from the wrong arm. With the arms paired correctly the expression is a
/// three-way fork, and a fork is nothing.
fn ternary_colon(s: &str, q: usize) -> Option<usize> {
    let mut depth = 0;
    let mut i = q + 1;
    for _ in 0..64 {
        let next_q = index_at_depth0(s, b'?', i);
        let next_colon = index_at_depth0(s, b':', i)?;
        if let Some(nq) = next_q {
            if nq < next_colon {
                depth += 1;
                i = nq + 1;
                continue;
            }
        }
        if depth == 0 {
            return Some(next_colon);
        }
        depth -= 1;
        i = next_colon + 1;
    }
    None
}

fn strip_cast(s: &str) -> String {
    re!(r"\s+(?:as|satisfies)\s+[\w$.<>[\]|&\s]+$").replace_first(s, |_| String::new())
}

/// The href in one argument expression: a string, a template, an `Href` object with a literal
/// `pathname`, or a conditional whose two arms are each one of those (`cond ? \`/x?id=${id}\` :
/// '/x'`). Anything else is not static.
pub fn parse_href_expression(expr: &str) -> Option<HrefLiteral> {
    // `expr as any` / `expr satisfies Href` — a cast says nothing about the value.
    let mut args = strip_cast(js_trim(expr));
    // `(a ? b : c)` — unwrap one layer of grouping parens.
    while args.starts_with('(') && match_paren(&args, 0) == Some(args.len() - 1) {
        args = strip_cast(js_trim(&args[1..args.len() - 1]));
    }
    if args.is_empty() {
        return None;
    }
    if let Some(q) = index_at_depth0(&args, b'?', 0).filter(|q| *q > 0) {
        if let Some(colon) = ternary_colon(&args, q).filter(|c| *c > q) {
            let yes = parse_href_expression(&args[q + 1..colon]);
            let no = parse_href_expression(&args[colon + 1..]);
            // Every arm is a destination, flattened — an arm that is itself a conditional
            // contributes its own arms rather than being reduced to one.
            let mut arms: Vec<HrefLiteral> = Vec::new();
            for side in [yes, no].into_iter().flatten() {
                arms.extend(href_arms(&side).into_iter().map(|a| HrefLiteral { alternates: None, ..a.clone() }));
            }
            if arms.is_empty() {
                return None;
            }
            let head = arms.remove(0);
            return Some(if arms.is_empty() { head } else { HrefLiteral { alternates: Some(arms), ..head } });
        }
    }
    if args.starts_with('{') {
        let key = re!(r"\bpathname\s*:\s*").find(&args)?;
        return to_href(read_string_at(&args, key.end()));
    }
    to_href(read_string_at(&args, 0))
}

/// The destination of the navigation call at (`line`, `column`) in `lines`.
///
/// Handles the three shapes Expo Router accepts: a string, a template literal (static prefix kept,
/// interpolations become holes), and an `Href` object whose `pathname` is one of those. Anything
/// else — a variable, a call, a spread — is not a literal and returns `None`.
pub fn read_href_argument(lines: &[String], line: u32, column: u32, method: &str) -> Option<HrefLiteral> {
    let arg = first_argument_text(lines, line, column, method)?;
    parse_href_expression(&arg)
}

/// The source text of the navigation call's first argument, or `None` when there is no call there.
pub fn first_argument_text(lines: &[String], line: u32, column: u32, method: &str) -> Option<String> {
    nth_argument_text(lines, line, column, method, 0)
}

/// The source text of a call's nth argument (0-based), or `None` when there is no call there or it
/// has too few arguments.
///
/// Most navigation calls put the destination first; SvelteKit's `redirect(303, '/login')` puts the
/// status there, so the reader has to be able to take the second. A `,` at depth 0 separates
/// arguments — inside parens, brackets, braces or a template it is part of one.
pub fn nth_argument_text(lines: &[String], line: u32, column: u32, method: &str, index: usize) -> Option<String> {
    let first = (line as usize).checked_sub(1)?;
    if first >= lines.len() {
        return None;
    }
    let text = lines[first..(first + MAX_CALL_LINES).min(lines.len())].join("\n");
    let from = utf16_byte_index(&text, column as usize);
    let name_at = from + text[from..].find(method)?;
    let mut open = name_at + method.len();
    while let Some((c, len)) = char_at(&text, open) {
        if !codegraph_types::jscompat::is_js_space(c) {
            break;
        }
        open += len;
    }
    if text.as_bytes().get(open) != Some(&b'(') {
        return None;
    }
    let close = match_paren(&text, open);
    let mut args: &str = &text[open + 1..close.unwrap_or(text.len())];
    for _ in 0..index {
        let comma = index_at_depth0(args, b',', 0)?;
        args = &args[comma + 1..];
    }
    Some(match index_at_depth0(args, b',', 0) {
        Some(comma) => args[..comma].to_string(),
        None => args.to_string(),
    })
}

/// A line that continues the previous statement rather than starting a new one.
fn continuation_re() -> &'static Re {
    re!(r#"^[?:.)\]}`'"+&|]"#)
}

/// The href a navigation call reaches through a local variable:
///
/// ```text
/// const href = params.length ? `/barcode-scan?${q}` : '/barcode-scan'
/// router.navigate(href as any)
/// ```
///
/// When the argument is a bare identifier, its most recent `const`/`let` declaration between
/// `enclosing_start` and the call is read and its initializer parsed exactly like a literal
/// argument would be. A reassignment in between, or an initializer that is not static, yields `None`.
pub fn read_href_via_local(lines: &[String], line: u32, column: u32, method: &str, enclosing_start: u32) -> Option<HrefLiteral> {
    let arg = first_argument_text(lines, line, column, method)?;
    let ident = strip_cast(js_trim(&arg));
    if !re!(r"^[A-Za-z_$][\w$]*$").is_match(&ident) {
        return None;
    }
    let decl = Re::new(&format!(r"\b(?:const|let|var)\s+{}\s*(?::[^=]*?)?=(?!=)", escape_regex(&ident)), "");
    let reassign = Re::new(&format!(r"(?:^|[^.\w$]){}\s*=(?!=)", escape_regex(&ident)), "");
    let from = (enclosing_start as i64 - 1).max(0);
    let mut i = line as i64 - 2;
    while i >= from {
        let text = &lines[i as usize];
        let Some(m) = decl.find(text) else {
            // The variable assigned again between declaration and use — not static.
            if reassign.is_match(text) {
                return None;
            }
            i -= 1;
            continue;
        };
        // The initializer: the rest of this line, plus continuation lines.
        let mut init = text[m.end()..].to_string();
        let mut j = (i + 1) as usize;
        while (j as i64) < line as i64 - 1 && j < i as usize + MAX_CALL_LINES {
            let next = &lines[j];
            if !continuation_re().is_match(next.trim_start_matches(codegraph_types::jscompat::is_js_space)) && balanced(&init) {
                break;
            }
            init.push('\n');
            init.push_str(next);
            j += 1;
        }
        return parse_href_expression(&init);
    }
    None
}

/// True when every bracket and template opened in `s` is closed.
fn balanced(s: &str) -> bool {
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'"' || c == b'\'' || c == b'`' {
            let end = string_end(s, i);
            if end >= b.len() {
                return false;
            }
            i = end + 1;
            continue;
        }
        if c == b'(' || c == b'[' || c == b'{' {
            depth += 1;
        } else if c == b')' || c == b']' || c == b'}' {
            depth -= 1;
        }
        i += 1;
    }
    depth <= 0
}

// -- Route tables --------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct RouteEntry {
    pub node: NodeRef,
    pub segs: Vec<String>,
}

#[derive(Clone)]
pub struct RouteTable {
    /// Identity of the node array the table was built from — rebuild when it changes.
    pub source: NodeList,
    pub exact: IndexMap<String, NodeRef>,
    pub dynamic: Vec<RouteEntry>,
}

impl RouteTable {
    pub fn new(source: NodeList) -> Self {
        RouteTable { source, exact: IndexMap::new(), dynamic: Vec::new() }
    }
}

/// A route table split by the app each route belongs to.
///
/// One table for a whole repository is wrong the moment the repository holds more than one app:
/// every app has a `/`, most have a `/login`, and a global `exact` map keeps whichever was indexed
/// first — so a `<Link to="/posts">` in one app resolves to another app's `/posts`. Gating on the
/// roots decides only WHETHER to resolve; the table has to decide WHICH app's routes to match.
pub struct RootedRouteTable<T = RouteTable> {
    /// Identity of the node array the table was built from — rebuild when it changes.
    pub source: NodeList,
    /// App root (`apps/web/`, `''`) → the routes that app serves.
    pub by_root: IndexMap<String, T>,
}

/// The routes of the app `file_path` belongs to, or `None` when it is under none.
///
/// Longest root wins, so an app nested inside another resolves to the nested one; a root of `''`
/// is a single-app repo, and covers every file.
pub fn routes_for_file<'a, T>(table: &'a RootedRouteTable<T>, file_path: &str) -> Option<&'a T> {
    let mut best: Option<&T> = None;
    let mut best_len: i64 = -1;
    for (root, routes) in &table.by_root {
        if root.len() as i64 > best_len && file_path.starts_with(root.as_str()) {
            best = Some(routes);
            best_len = root.len() as i64;
        }
    }
    best
}

/// Register `path` → `node` in one app's table. The first route to claim an address keeps it.
pub fn add_route_to(table: &mut RouteTable, path: &str, node: &NodeRef) {
    if !table.exact.contains_key(path) {
        table.exact.insert(path.to_string(), node.clone());
    }
    if path.contains(':') {
        table.dynamic.push(RouteEntry { node: node.clone(), segs: path.split('/').skip(1).map(String::from).collect() });
    }
}

/// The directory the app owning `file_path` lives in — what a navigation call is gated on, so a
/// `push` in one package of a monorepo cannot name another package's routes.
///
/// The first conventional source directory ends it: proshop keeps its routes in
/// `frontend/src/App.js` and its screens in `frontend/src/screens/`, so the root is `frontend/`;
/// `src/routes/login/+page.svelte` and `pages/index.vue` are both a repo-root app, whose root is
/// `''` — every file, exactly as a Next app at the repo root is. A file under no such directory
/// owns only its own folder.
pub fn app_root_for(file_path: &str) -> String {
    if let Some(m) = re!(r"^((?:[^/]+\/)*?)(?:src|pages|app|routes)\/").captures(file_path) {
        return m.text(1).unwrap_or("").to_string();
    }
    match file_path.rfind('/') {
        None => String::new(),
        Some(slash) => file_path[..=slash].to_string(),
    }
}

/// A cache of the table built from one snapshot of the route nodes.
#[derive(Default)]
pub struct ExpoTableCache(RefCell<Option<Rc<RouteTable>>>);

/// True for this framework's own route nodes: the ones whose name IS the path derived from their
/// file. Express/SvelteKit routes in the same project name themselves differently and never match.
fn is_screen_route(node: &Node) -> bool {
    route_path_for_file(&node.file_path).as_deref() == Some(node.name.as_str())
}

pub fn route_table(ctx: &Ctx) -> Rc<RouteTable> {
    let all = ctx.get_nodes_by_kind(NodeKind::Route);
    let cache = ctx.memo::<ExpoTableCache>();
    if let Some(cached) = cache.0.borrow().as_ref() {
        if Rc::ptr_eq(&cached.source, &all) {
            return Rc::clone(cached);
        }
    }
    let mut table = RouteTable::new(all.clone());
    for node in all.iter() {
        if !is_screen_route(node) {
            continue;
        }
        table.exact.insert(node.name.clone(), node.clone());
        if node.name.contains('[') {
            table.dynamic.push(RouteEntry { node: node.clone(), segs: node.name.split('/').skip(1).map(String::from).collect() });
        }
    }
    let table = Rc::new(table);
    *cache.0.borrow_mut() = Some(Rc::clone(&table));
    table
}

/// Normalize an href path to the form route names use: leading `/`, no trailing `/`, no `(group)`
/// segments, holes as a whole-segment `*`. A relative href is resolved against the screen the call
/// sits in; from a non-screen file it has no base and returns `None`.
pub fn normalize_href_path(path: &str, from_file: &str) -> Option<Vec<String>> {
    let mut p = path.to_string();
    if !p.starts_with('/') {
        // Expo resolves `./x` and bare `x` against the DIRECTORY of the screen file the call sits
        // in — `/capture` for both `capture/index.tsx` and `capture/review.tsx` — which is the
        // route of that directory's index.
        route_path_for_file(from_file)?;
        let dir_end = from_file.rfind('/').map_or(0, |i| i + 1);
        let dir_route = route_path_for_file(&format!("{}index.tsx", &from_file[..dir_end]))?;
        let parent: Vec<String> = dir_route.split('/').skip(1).map(String::from).collect();
        if let Some(rest) = p.strip_prefix("./") {
            let mut parts = parent.clone();
            parts.push(rest.to_string());
            p = format!("/{}", parts.join("/"));
        } else if p.starts_with("../") {
            let mut up = parent.clone();
            let mut rest = p.as_str();
            while let Some(r) = rest.strip_prefix("../") {
                up.pop();
                rest = r;
            }
            up.push(rest.to_string());
            p = format!("/{}", up.join("/"));
        } else {
            let mut parts = parent.clone();
            parts.push(p.clone());
            p = format!("/{}", parts.join("/"));
        }
    }
    Some(
        p.split('/')
            .skip(1)
            .filter(|s| !s.is_empty() && !(s.starts_with('(') && s.ends_with(')')))
            .map(|s| if s.contains(HOLE) { "*".to_string() } else { decode_segment(s) })
            .collect(),
    )
}

/// `decodeURIComponent`, or the segment as written when it is not valid percent-encoding.
pub fn decode_segment(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s
                .get(i + 1..i + 3)
                .filter(|h| h.bytes().all(|c| c.is_ascii_hexdigit()))
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            match hex {
                Some(v) => {
                    out.push(v);
                    i += 3;
                }
                None => return s.to_string(),
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// The single route the href segments denote, or `None` when none or several do.
///
/// Scored so that a literal segment beats a wildcard for the same slot and a `[param]` slot
/// accepts either; a tie between two routes is ambiguity, and ambiguity is a `None`, not a coin flip.
pub fn match_route(segs: &[String], table: &RouteTable) -> Option<NodeRef> {
    if let Some(exact) = table.exact.get(&format!("/{}", segs.join("/"))) {
        return Some(exact.clone());
    }
    let mut best: Option<(NodeRef, i64)> = None;
    let mut tied = false;
    for entry in &table.dynamic {
        let Some(score) = score_match(segs, &entry.segs) else { continue };
        match &best {
            Some((_, best_score)) if score <= *best_score => {
                if score == *best_score {
                    tied = true;
                }
            }
            _ => {
                best = Some((entry.node.clone(), score));
                tied = false;
            }
        }
    }
    match best {
        Some((node, _)) if !tied => Some(node),
        _ => None,
    }
}

/// A route segment that takes a value: Expo's `[id]`, or the `:id` every other framework's routes use.
fn is_param_segment(seg: &str) -> bool {
    (seg.starts_with('[') && seg.ends_with(']')) || (seg.starts_with(':') && !seg.ends_with('*'))
}

/// A route segment that takes the rest of the path: `[...slug]`, or `:slug*`.
fn is_catch_all_segment(seg: &str) -> bool {
    (seg.starts_with("[...") && seg.ends_with(']')) || (seg.starts_with(':') && seg.ends_with('*'))
}

fn score_match(href: &[String], route: &[String]) -> Option<i64> {
    let mut score = 0i64;
    let mut i = 0usize;
    for seg in route {
        if is_catch_all_segment(seg) {
            // Catch-all: needs at least one segment and takes the rest.
            if i >= href.len() {
                return None;
            }
            score += (href.len() - i) as i64;
            i = href.len();
            continue;
        }
        if i >= href.len() {
            return None;
        }
        let h = &href[i];
        if is_param_segment(seg) {
            score += 2;
        } else if h == seg {
            score += 3;
        } else if h == "*" {
            score += 1;
        } else {
            return None;
        }
        i += 1;
    }
    if i == href.len() {
        Some(score)
    } else {
        None
    }
}

// -- The resolver -----------------------------------------------------------------------------------------

pub struct ExpoRouterResolver {
    nav: NavigationCalls,
}

impl ExpoRouterResolver {
    pub fn new() -> Self {
        ExpoRouterResolver {
            nav: NavigationCalls {
                // A project's own wrapper (`safePush`) is left out: its tail is its own name,
                // which the failed-tail index cannot find by suffix.
                tails: vec!["push", "replace", "navigate", "dismissTo"],
                // One table for the whole project, and any file's call matches against it.
                scope: Box::new(|route, _| if is_screen_route(route) { Some(vec![String::new()]) } else { None }),
            },
        }
    }
}

impl Default for ExpoRouterResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameworkResolver for ExpoRouterResolver {
    fn name(&self) -> &str {
        "expo-router"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&ROUTE_LANGUAGES)
    }

    fn app_dependencies(&self) -> Option<&[&'static str]> {
        Some(&["expo-router"])
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        if depends_on(ctx, &["expo-router"]) {
            return true;
        }
        let files = ctx.get_all_files();
        let has_layout = files.iter().any(|f| re!(r"(?:^|\/)(?:src\/)?app\/_layout\.(?:tsx|jsx|ts|js)$").is_match(f));
        let has_expo_config = files.iter().any(|f| re!(r"^app\.(?:json|config\.(?:js|ts))$").is_match(f));
        has_layout && has_expo_config
    }

    fn claims_reference(&self, name: &str) -> bool {
        nav_method_re().is_match(name)
    }

    fn navigation(&self) -> Option<&NavigationCalls> {
        Some(&self.nav)
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<FrameworkExtractionResult> {
        if let Some(api_path) = api_route_path_for_file(file_path) {
            // `export async function GET(request) {…}` / `export const POST = …` — one endpoint per method.
            let language = language_for_file(file_path);
            let stripped = codegraph_text::strip_comments_for_regex(content, "typescript");
            let mut nodes: Vec<Node> = Vec::new();
            let mut references: Vec<UnresolvedRef> = Vec::new();
            let mut seen: Vec<String> = Vec::new();
            let decl = Re::cached(
                &format!(r"\bexport\s+(?:async\s+)?function\s+({API_METHODS})\b|\bexport\s+(?:const|let)\s+({API_METHODS})\s*="),
                "g",
            );
            let now = now_ms();
            for m in decl.captures_iter(&stripped) {
                let method = m.text(1).or_else(|| m.text(2)).unwrap_or("").to_string();
                if seen.contains(&method) {
                    continue;
                }
                seen.push(method.clone());
                let whole = m.get(0)?;
                let line = stripped.as_bytes()[..whole.start()].iter().filter(|c| **c == b'\n').count() as u32 + 1;
                let mut node = Node::new(
                    format!("route:{file_path}:{line}:{method}:{api_path}"),
                    NodeKind::Route,
                    format!("{method} {api_path}"),
                    format!("{file_path}::{method}:{api_path}"),
                    file_path,
                    language,
                    line,
                    line,
                    0,
                    codegraph_types::jscompat::utf16_len(whole.as_str()) as u32,
                    now,
                );
                node.is_exported = Some(true);
                let mut r = UnresolvedRef::new(node.id.clone(), method.clone(), EdgeKind::References, line, 0, file_path, language);
                r.candidates = Some(vec![method]);
                references.push(r);
                nodes.push(node);
            }
            return Some(FrameworkExtractionResult { nodes, references });
        }
        let Some(route_path) = route_path_for_file(file_path) else { return Some(FrameworkExtractionResult::default()) };
        let language = language_for_file(file_path);
        let mut node = Node::new(
            format!("route:{file_path}:{route_path}"),
            NodeKind::Route,
            route_path.clone(),
            format!("{file_path}::route:{route_path}"),
            file_path,
            language,
            1,
            1,
            0,
            0,
            now_ms(),
        );
        node.is_exported = Some(true);
        let mut references: Vec<UnresolvedRef> = Vec::new();
        let stripped = codegraph_text::strip_comments_for_regex(content, "typescript");
        if let Some((name, index)) = default_export_name(&stripped) {
            let line = stripped.as_bytes()[..index].iter().filter(|c| **c == b'\n').count() as u32 + 1;
            let mut r = UnresolvedRef::new(node.id.clone(), name.clone(), EdgeKind::Calls, line, 0, file_path, language);
            r.candidates = Some(vec![name]);
            references.push(r);
        }
        Some(FrameworkExtractionResult { nodes: vec![node], references })
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        if !r.is_kind(EdgeKind::Calls) {
            return None;
        }
        let method = nav_verb(&r.reference_name)?;
        if !ROUTE_LANGUAGES.contains(&r.language) {
            return None;
        }
        // The name to find on the line: `navigate` in `router.navigate(`, or the wrapper's own
        // name in `safePush(`.
        let callee = &r.reference_name[r.reference_name.rfind('.').map_or(0, |i| i + 1)..];

        let lines = ctx.get_file_lines(&r.file_path)?;
        let mut href = read_href_argument(&lines, r.line, r.column, callee);
        if href.is_none() {
            let start = match ctx.node_by_id(&r.from_node_id) {
                Some(enclosing) if enclosing.file_path == r.file_path => enclosing.start_line,
                _ => r.line.saturating_sub(40).max(1),
            };
            href = read_href_via_local(&lines, r.line, r.column, callee, start);
        }
        let href = href?;
        let table = route_table(ctx);
        // `cond ? a : b` names a screen per arm, and the user reaches every one of them; the extra
        // arms ride along as `alsoTargets` and become edges of their own. A relative href is
        // resolved against the screen it sits in, which is why this matches its own way rather
        // than through `pagesForHref`.
        let mut targets: Vec<NodeRef> = Vec::new();
        for arm in href_arms(&href) {
            let Some(segs) = normalize_href_path(&arm.path, &r.file_path) else { continue };
            let Some(hit) = match_route(&segs, &table) else { continue };
            if targets.iter().any(|t| t.id == hit.id) {
                continue;
            }
            targets.push(hit);
        }
        let target = targets.first()?;

        let mut resolved = ResolvedRef::new(r, target.id.clone(), 0.95, ResolvedBy::Framework);
        if targets.len() > 1 {
            resolved.also_targets = Some(
                targets[1..]
                    .iter()
                    .map(|t| AlsoTarget { target_node_id: t.id.clone(), metadata: Some(nav_metadata(&href.display, &method, None)) })
                    .collect(),
            );
        }
        resolved.edge_kind = Some(EdgeKind::Navigates);
        resolved.metadata = Some(nav_metadata(&href.display, &method, if callee != method { Some(callee) } else { None }));
        Some(resolved)
    }
}

/// An edge's `{ href, navMethod }` metadata, plus the wrapper that stood in for the verb.
pub fn nav_metadata(href: &str, nav_method: &str, via: Option<&str>) -> codegraph_types::Metadata {
    let mut m = serde_json::Map::new();
    m.insert("href".into(), serde_json::Value::String(href.to_string()));
    m.insert("navMethod".into(), serde_json::Value::String(nav_method.to_string()));
    if let Some(via) = via {
        m.insert("via".into(), serde_json::Value::String(via.to_string()));
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_paths_follow_the_file_tree() {
        assert_eq!(route_path_for_file("app/object-detail.tsx").as_deref(), Some("/object-detail"));
        assert_eq!(route_path_for_file("app/capture/index.tsx").as_deref(), Some("/capture"));
        assert_eq!(route_path_for_file("src/app/(tabs)/home.tsx").as_deref(), Some("/home"));
        assert_eq!(route_path_for_file("app/_layout.tsx"), None);
        assert_eq!(api_route_path_for_file("app/blog/[post]+api.ts").as_deref(), Some("/blog/:post"));
    }

    #[test]
    fn hrefs_parse_strings_templates_and_forks() {
        let href = parse_href_expression("'/users?tab=1'").unwrap();
        assert_eq!(href.path, "/users");
        let tpl = parse_href_expression("`/users/${id}`").unwrap();
        assert_eq!(tpl.display, "/users/${…}");
        let fork = parse_href_expression("a ? '/x' : '/y'").unwrap();
        assert_eq!(fork.path, "/x");
        assert_eq!(fork.alternates.as_ref().map(|a| a.len()), Some(1));
        assert!(parse_href_expression("someVariable").is_none());
    }

    #[test]
    fn argument_text_stops_at_the_top_level_comma() {
        let lines = vec!["router.push({ pathname: '/a', params: { x: 1 } }, extra);".to_string()];
        assert_eq!(nth_argument_text(&lines, 1, 0, "push", 0).as_deref(), Some("{ pathname: '/a', params: { x: 1 } }"));
        assert_eq!(nth_argument_text(&lines, 1, 0, "push", 1).as_deref(), Some(" extra"));
    }

    #[test]
    fn dynamic_routes_match_by_score() {
        let mk = |name: &str| -> NodeRef { Rc::new(Node::new(name, NodeKind::Route, name, name, "app/x.tsx", Language::Tsx, 1, 1, 0, 0, 0)) };
        let mut table = RouteTable::new(Rc::new(Vec::new()));
        let item = mk("/item/[id]");
        table.exact.insert("/item/[id]".into(), item.clone());
        table.dynamic.push(RouteEntry { node: item.clone(), segs: vec!["item".into(), "[id]".into()] });
        let hit = match_route(&["item".to_string(), "5".to_string()], &table);
        assert_eq!(hit.map(|n| n.id.clone()), Some("/item/[id]".to_string()));
    }
}
