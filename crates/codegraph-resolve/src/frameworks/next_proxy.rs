//! Next.js proxy (`proxy.ts`, `middleware.ts` before Next 16): the one file of an app nothing
//! imports. It sits beside `app` / `pages` and runs before every route its `config.matcher`
//! accepts. Port of `src/resolution/frameworks/next-proxy.ts` (upstream 446d08c2).
//!
//! This file says which file Next loads for an app, what runs in it, and what its matcher
//! accepts. `next_proxy_edges` in `callback_synthesizer/routers.rs` draws the edges.
//!
//! Offsets here are byte offsets into the comment-stripped source.

use super::expo_router::{read_string_at, HOLE};
use super::object_literal::{match_bracket, read_fields, skip_string};
use crate::ctx::Ctx;
use crate::types::NodeRef;
use codegraph_jsre::{re, Re};
use codegraph_types::NodeKind;
use std::sync::Arc;

const PROXY_EXTENSIONS: [&str; 6] = ["ts", "tsx", "js", "jsx", "mjs", "cjs"];

/// A file named like a proxy. Whether Next loads it depends on where it sits (`next_proxy_dir`).
pub fn is_next_proxy_file(file_path: &str) -> bool {
    re!(r"(?:^|/)(?:proxy|middleware)\.(?:tsx?|jsx?|mjs|cjs)$").is_match(file_path)
}

fn is_entry_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Function | NodeKind::Constant | NodeKind::Variable)
}

/// The directory an app's proxy must sit in: beside `pages` when the app has one, else beside
/// `app`, each looked for at the app's root before its `src/` (`next build`'s
/// `join(pagesDir || appDir, '..')`). A `proxy.ts` anywhere else is an ordinary module.
pub fn next_proxy_dir(root: &str, route_files: &[&str]) -> String {
    for dir in ["pages/", "app/"] {
        if route_files.iter().any(|f| f.starts_with(&format!("{root}{dir}"))) {
            return root.to_string();
        }
        if route_files.iter().any(|f| f.starts_with(&format!("{root}src/{dir}"))) {
            return format!("{root}src/");
        }
    }
    root.to_string()
}

/// Whether the app's manifest, or failing that the repository's, pins `next` below 16, where
/// `proxy` isn't a convention yet.
fn pins_next_before_16(ctx: &Ctx, root: &str) -> bool {
    let mut manifests = vec![format!("{root}package.json")];
    if manifests[0] != "package.json" {
        manifests.push("package.json".to_string());
    }
    for manifest in manifests {
        let text = ctx.read_file(&manifest);
        let Ok(pkg) = serde_json::from_str::<serde_json::Value>(text.as_deref().unwrap_or("null")) else { continue };
        // `a ?? b ?? c`: the first one present, string or not.
        let range = ["dependencies", "devDependencies", "peerDependencies"]
            .iter()
            .find_map(|k| pkg.get(k).and_then(|d| d.get("next")).filter(|v| !v.is_null()));
        let Some(range) = range.and_then(|v| v.as_str()) else { continue };
        return re!(r"^[\s^~>=v]*(\d+)")
            .captures(range)
            .and_then(|m| m.text(1).and_then(|d| d.parse::<u64>().ok()))
            .is_some_and(|major| major < 16);
    }
    false
}

pub struct NextProxy {
    pub file: String,
    /// What runs on a matched request: the export named after the file, else the default export
    /// (Next's `mod.proxy || mod.default`), else the file itself, where a wrapper's calls
    /// (`export default clerkMiddleware(…)`) are recorded.
    pub entry: NodeRef,
}

/// The proxy Next runs for an app, or None. `middleware.*` wins when both files exist: Next 16
/// refuses to build with both, and before 16 a `proxy.ts` is an ordinary module, so a lone one
/// counts unless the app pins an older Next.
pub fn next_proxy_for(ctx: &Ctx, dir: &str, root: &str) -> Option<NextProxy> {
    let find = |base: &str| PROXY_EXTENSIONS.iter().map(|ext| format!("{dir}{base}.{ext}")).find(|f| ctx.file_exists(f));
    let middleware = find("middleware");
    let file = match &middleware {
        Some(f) => f.clone(),
        None => find("proxy").filter(|_| !pins_next_before_16(ctx, root))?,
    };
    let nodes = ctx.get_nodes_in_file(&file);
    let wanted = if middleware.is_some() { "middleware" } else { "proxy" };
    if let Some(named) = nodes.iter().find(|n| is_entry_kind(n.kind) && n.is_exported == Some(true) && n.name == wanted) {
        return Some(NextProxy { file, entry: named.clone() });
    }
    let source = ctx.read_file(&file).unwrap_or_else(|| "".into());
    let safe = codegraph_text::strip_comments_for_regex(&source, "typescript");
    let local = re!(r"\bexport\s+default\s+(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)")
        .captures(&safe)
        .or_else(|| re!(r"\bexport\s+default\s+([A-Za-z_$][\w$]*)\s*;?\s*$", "m").captures(&safe))
        .or_else(|| re!(r"\bexport\s*\{[^}]*?\b([A-Za-z_$][\w$]*)\s+as\s+(?:default|middleware|proxy)\b[^}]*\}(?!\s*from\b)").captures(&safe))
        .and_then(|m| m.text(1).map(String::from));
    let entry = local
        .and_then(|local| nodes.iter().find(|n| is_entry_kind(n.kind) && n.name == local).cloned())
        .or_else(|| nodes.iter().find(|n| n.kind == NodeKind::File).cloned())?;
    Some(NextProxy { file, entry })
}

// =============================================================================
// config.matcher
// =============================================================================

/// One path a proxy's matcher names, and the offset it is written at.
#[derive(Debug, Clone, PartialEq)]
pub struct ProxyMatcherEntry {
    pub source: String,
    pub index: usize,
}

/// What `export const config = { matcher }` names: `All` when the file sets no matcher (Next
/// runs the proxy on every request), else the entries. Next reads `config` statically and refuses
/// anything else (a variable, a spread, a template with holes), so such a matcher reads as no
/// entries, never as `All`.
#[derive(Debug, PartialEq)]
pub enum ProxyMatcher {
    All,
    Entries(Vec<ProxyMatcherEntry>),
}

pub fn read_proxy_matcher(safe: &str) -> ProxyMatcher {
    let none = ProxyMatcher::Entries(Vec::new());
    let Some(decl) = re!(r"\bexport\s+const\s+config\b[^=;]*=\s*").find(safe) else { return ProxyMatcher::All };
    let open = decl.end();
    let Some(close) = (safe.as_bytes().get(open) == Some(&b'{')).then(|| match_bracket(safe, open)).flatten() else { return none };
    let fields = read_fields(safe, open, close);
    let Some(field) = fields.get("matcher") else {
        // A shorthand or a quoted key is a matcher this can't see into.
        return if re!(r"\bmatcher\b").is_match(&safe[open..close]) { none } else { ProxyMatcher::All };
    };
    let at = value_after_key(safe, field.at);
    if safe.as_bytes().get(at) != Some(&b'[') {
        return ProxyMatcher::Entries(string_entry(safe, at).into_iter().collect());
    }
    let Some(end) = match_bracket(safe, at) else { return none };
    let mut entries = Vec::new();
    let mut i = at + 1;
    while i < end {
        let ch = safe.as_bytes()[i];
        if ch.is_ascii_whitespace() || ch == b',' {
            i += 1;
            continue;
        }
        let entry = if ch == b'{' { source_entry(safe, i) } else { string_entry(safe, i) };
        let Some(entry) = entry else { return none };
        entries.push(entry);
        match after_element(safe, i, end) {
            Some(next) => i = next,
            None => break,
        }
    }
    ProxyMatcher::Entries(entries)
}

fn source_entry(s: &str, open: usize) -> Option<ProxyMatcherEntry> {
    let close = match_bracket(s, open)?;
    let source = read_fields(s, open, close).get("source")?.at;
    string_entry(s, value_after_key(s, source))
}

fn string_entry(s: &str, at: usize) -> Option<ProxyMatcherEntry> {
    let source = read_string_at(s, at)?;
    (!source.contains(HOLE)).then_some(ProxyMatcherEntry { source, index: at })
}

/// The offset of a field's value, from the offset of its key.
fn value_after_key(s: &str, key_at: usize) -> usize {
    let mut end = (key_at + 128).min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    key_at + re!(r"^[A-Za-z_$][\w$]*\s*:\s*").find(&s[key_at..end]).map_or(0, |m| m.end())
}

/// The offset of the comma that ends the array element at `at`, or of the array's end; None when
/// a string or bracket in it doesn't close.
fn after_element(s: &str, at: usize, end: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = at;
    while i < end && b[i] != b',' {
        match b[i] {
            b'"' | b'\'' | b'`' => i = skip_string(s, i)? + 1,
            b'{' | b'[' | b'(' => i = match_bracket(s, i)? + 1,
            _ => i += 1,
        }
    }
    Some(i)
}

/// Repeating parts one entry may carry, and how long it may be. Each repeat multiplies the work
/// of a match, and a pattern in a repository being indexed is not trusted to stay small.
const MAX_REPEATS: usize = 2;
const MAX_MATCHER_LENGTH: usize = 2048;

/// A route parameter's stand-in in a sample address: no literal in a matcher can equal it.
const ANY_SEGMENT: &str = "\u{1}";

/// What every pattern for a file with an extension reads as: a dot in the last segment.
const FILE_WITH_EXTENSION: &str = r".*\.[^/]*$";

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if ".*+?^${}()|[]\\/".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The offset of the `)` closing the `(` at `open`.
fn closing_paren(s: &str, open: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'[' => {
                while i < b.len() && b[i] != b']' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// `a|b|(c|d)` split at its top-level bars.
fn top_level_bars(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut from = 0;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'[' => {
                while i < b.len() && b[i] != b']' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'(' => match closing_paren(s, i) {
                Some(close) => i = close,
                None => return vec![s],
            },
            b'|' => {
                out.push(&s[from..i]);
                from = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(&s[from.min(s.len())..]);
    out
}

/// One branch of a negative lookahead, as a regex of our own: a literal path prefix (`api`,
/// `_next/static`, `api(?:/|$)`) or a pattern for files with an extension (`[^?]*\.(?:html?|css)`,
/// `.*\.(?:svg|png)$`), which all read the same way: a route with a dot in its last segment is
/// left out.
fn read_exclusion(branch: &str) -> Option<String> {
    if re!(r"^(?:\[\^\?\]\*|\.\*|\.\+|\[[^\]]*\]\+)\\\.").is_match(branch) {
        return Some(FILE_WITH_EXTENSION.to_string());
    }
    let literal = re!(r"^((?:[\w/~-]|\\?\.)+)(\$|\(\?:\/\|\$\))?$").captures(branch)?;
    Some(escape(&literal.text(1).unwrap_or("").replace("\\.", ".")) + literal.text(2).unwrap_or(""))
}

/// The inside of a matcher's group: `.*`, `.+`, `a|b` of literals, or `(?!a|b|….*\.png$).*`,
/// the form almost every real matcher takes.
fn read_group(body: &str) -> Option<String> {
    if body == ".*" || body == ".+" {
        return Some(body.to_string());
    }
    if body.starts_with("(?!") {
        let close = closing_paren(body, 0);
        let tail = close.map_or("", |c| &body[c + 1..]);
        if tail != ".*" && tail != ".+" {
            return None;
        }
        let close = close?;
        let mut branches: Vec<String> = Vec::new();
        for branch in top_level_bars(&body[3..close]) {
            let read = read_exclusion(branch)?;
            if !branches.contains(&read) {
                branches.push(read);
            }
        }
        return Some(format!("(?!{}){tail}", branches.join("|")));
    }
    let options = top_level_bars(body.strip_prefix("?:").unwrap_or(body));
    if !options.iter().all(|o| re!(r"^[\w./~-]+$").is_match(o)) {
        return None;
    }
    Some(format!("(?:{})", options.iter().map(|o| escape(o)).collect::<Vec<_>>().join("|")))
}

/// A matcher entry as a regular expression, or None for one this doesn't read, which links
/// nothing rather than a guess. Next compiles an entry with path-to-regexp, anchored at both ends.
/// The expression is built here from the entry's parts, never from its text: a matcher is the
/// indexed repository's to write, and compiling it as written would let one hang an index.
pub fn compile_matcher(source: &str) -> Option<Arc<Re>> {
    if !source.starts_with('/') || source.len() > MAX_MATCHER_LENGTH {
        return None;
    }
    let b = source.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if let Some(param) = re!(r"^/:[A-Za-z_]\w*").find(&source[i..]) {
            i += param.end();
            let mut body = "[^/]+".to_string();
            if b.get(i) == Some(&b'(') {
                let close = closing_paren(source, i)?;
                body = read_group(&source[i + 1..close])?;
                i = close + 1;
            }
            match b.get(i) {
                Some(&m @ (b'?' | b'*' | b'+')) => {
                    out.push_str(&format!("(?:/{body}){}", m as char));
                    i += 1;
                }
                _ => out.push_str(&format!("/{body}")),
            }
            continue;
        }
        let ch = b[i];
        if ch == b'(' {
            let close = closing_paren(source, i)?;
            let group = read_group(&source[i + 1..close])?;
            if matches!(b.get(close + 1), Some(b'?' | b'*' | b'+')) {
                return None;
            }
            out.push_str(&group);
            i = close + 1;
            continue;
        }
        if !(ch.is_ascii_alphanumeric() || b"_./~-".contains(&ch)) {
            return None;
        }
        out.push_str(&escape(&(ch as char).to_string()));
        i += 1;
    }
    let repeats = re!(r"\.[*+]|\)[*+?]", "g").captures_iter(&out.split(FILE_WITH_EXTENSION).collect::<String>()).count();
    if repeats > MAX_REPEATS {
        return None;
    }
    // `/?`: path-to-regexp's trailing delimiter, which is how `/:path*` takes `/`.
    Re::try_cached(&format!("^{out}/?$"), "")
}

/// Addresses a route serves, one of each shape: a parameter is a segment no literal stands for,
/// and the route's name writes `[...all]` and `[[...all]]` alike as `:all*`, so the file says
/// which may be empty.
fn route_addresses(path: &str, route_file: &str) -> Vec<String> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let filled: Vec<&str> = segments.iter().map(|s| if s.starts_with(':') { ANY_SEGMENT } else { s }).collect();
    let mut addresses = vec![format!("/{}", filled.join("/"))];
    if let Some(last) = segments.last().filter(|l| l.ends_with('*') && l.len() >= 2) {
        if route_file.contains(&format!("[[...{}]]", &last[1..last.len() - 1])) {
            addresses.push(format!("/{}", filled[..filled.len() - 1].join("/")));
        }
    }
    addresses
}

/// Whether a compiled matcher entry accepts an address the route serves.
pub fn matcher_covers(matcher: &Re, route_path: &str, route_file: &str) -> bool {
    route_addresses(route_path, route_file).iter().any(|a| matcher.is_match(a))
}

#[cfg(test)]
mod tests {
    //! Port of the matcher / config / directory cases of nextjs-proxy.test.ts (upstream 446d08c2).
    use super::*;

    fn covers(source: &str, route: &str, file: &str) -> Option<bool> {
        compile_matcher(source).map(|re| matcher_covers(&re, route, file))
    }
    fn covers_x(source: &str, route: &str) -> Option<bool> {
        covers(source, route, "app/x/page.tsx")
    }

    #[test]
    fn a_matcher_entry() {
        let table: &[(&str, &str, bool)] = &[
            ("/about", "/about", true),
            ("/about", "/about/team", false),
            ("/about", "/blog/about", false),
            ("/", "/", true),
            ("/", "/about", false),
            ("/about/:rest", "/about", false),
            ("/about/:rest", "/about/team", true),
            ("/about/:rest", "/about/team/lead", false),
            ("/about/:rest*", "/about", true),
            ("/about/:rest*", "/about/team/lead", true),
            ("/about/:rest*", "/aboutus", false),
            ("/about/:rest+", "/about", false),
            ("/about/:rest+", "/about/team/lead", true),
            ("/about/:rest?", "/about", true),
            ("/about/:rest?", "/about/team", true),
            ("/about/:rest?", "/about/team/lead", false),
            ("/:rest*", "/", true),
            ("/:rest*", "/a/b/c", true),
            ("/:locale/about", "/en/about", true),
            ("/blog/:slug", "/blog/:id", true),
            ("/api/:rest*", "/api/projects/:id", true),
        ];
        for &(source, route, expected) in table {
            assert_eq!(covers_x(source, route), Some(expected), "{source} against {route}");
        }
    }

    #[test]
    fn reads_the_forms_a_regular_expression_takes_in_real_apps() {
        let starter = "/((?!api|_next/static|_next/image|favicon.ico).*)";
        assert_eq!(covers_x(starter, "/"), Some(true));
        assert_eq!(covers_x(starter, "/dashboard/:id"), Some(true));
        assert_eq!(covers_x(starter, "/api/users"), Some(false));
        assert_eq!(covers_x(starter, "/apikeys"), Some(false));
        let clerk = r"/((?!_next|[^?]*\.(?:html?|css|js(?!on)|jpe?g|webp|png|gif|svg|ttf|woff2?|ico|csv|docx?|xlsx?|zip|webmanifest)).*)";
        assert_eq!(covers_x(clerk, "/dashboard"), Some(true));
        assert_eq!(covers_x(clerk, "/feed.xml"), Some(false));
        assert_eq!(covers_x("/(api|trpc)(.*)", "/api/users"), Some(true));
        assert_eq!(covers_x("/(api|trpc)(.*)", "/trpc"), Some(true));
        assert_eq!(covers_x("/(api|trpc)(.*)", "/users"), Some(false));
        assert_eq!(covers_x(r"/((?!.*\..*|_next).*)", "/pricing"), Some(true));
        assert_eq!(covers_x(r"/((?!_next|.*\.png$|.*\.jpg$|.*\.svg$).*)", "/pricing"), Some(true));
        assert_eq!(covers_x(r"/((?!api/|_next/|_static|_vercel|[\w-]+\.\w+).*)", "/api/users"), Some(false));
        assert_eq!(covers_x(r"/((?!api/|_next/|_static|_vercel|[\w-]+\.\w+).*)", "/app"), Some(true));
        assert_eq!(covers_x("/((?!api(?:/|$)|_next).*)", "/api"), Some(false));
        assert_eq!(covers_x("/((?!api(?:/|$)|_next).*)", "/apikeys"), Some(true));
        assert_eq!(covers_x("/((?!sitemap.xml$).*)", "/sitemap.xml"), Some(false));
        assert_eq!(covers_x("/((?!_next).+)", "/"), Some(false));
        assert_eq!(covers_x("/((?!_next).+)", "/x"), Some(true));
        assert_eq!(covers_x("/dashboard(.*)", "/dashboard"), Some(true));
        assert_eq!(covers_x("/dashboard(.*)", "/dashboard/settings"), Some(true));
        assert_eq!(covers_x("/about/(.*)", "/about/team"), Some(true));
        assert_eq!(covers_x("/:locale(en|fr)/about", "/en/about"), Some(true));
        assert_eq!(covers_x("/:locale(en|fr)/about", "/de/about"), Some(false));
    }

    #[test]
    fn a_literal_never_stands_in_for_a_route_parameter() {
        assert_eq!(covers("/blog/featured", "/blog/:slug", "app/blog/[slug]/page.tsx"), Some(false));
        assert_eq!(covers("/docs/intro", "/docs/:all*", "app/docs/[...all]/page.tsx"), Some(false));
    }

    #[test]
    fn tells_a_required_catch_all_from_an_optional_one_by_the_file() {
        assert_eq!(covers("/docs", "/docs/:all*", "app/docs/[...all]/page.tsx"), Some(false));
        assert_eq!(covers("/docs", "/docs/:all*", "app/docs/[[...all]]/page.tsx"), Some(true));
        assert_eq!(covers("/docs/:p", "/docs/:all*", "app/docs/[...all]/page.tsx"), Some(true));
    }

    #[test]
    fn an_unescaped_dot_is_not_a_file_extension() {
        assert!(compile_matcher("/((?!.*..*|_next).*)").is_none());
    }

    #[test]
    fn what_is_not_read_links_nothing() {
        let long = format!("/{}", "a".repeat(3000));
        for source in [r"/about/:rest(\d+)", r"/feed-(\d+)", "/api/*", "about", r"/((?!\d+).*)", "/(a+)+", "/(.*)(.*)(.*)", "/:a*/:b*/:c*", long.as_str()] {
            assert!(compile_matcher(source).is_none(), "{source} should not be read");
        }
    }

    #[test]
    fn holds_up_against_a_pattern_written_to_hang_a_regular_expression() {
        let started = std::time::Instant::now();
        let many = format!("/{}", "(.*)".repeat(40));
        let address = format!("/{}a", "a/".repeat(2000));
        for source in ["/(a|a)(a|a)(a|a)(a|a)", "/((?!(a+)+$).*)", many.as_str(), "/(.*)(.*)x", "/:a*/:b*x"] {
            if let Some(re) = compile_matcher(source) {
                matcher_covers(&re, &address, "app/x/page.tsx");
            }
        }
        assert!(started.elapsed().as_millis() < 500, "{:?}", started.elapsed());
    }

    fn read(src: &str) -> Option<Vec<String>> {
        match read_proxy_matcher(src) {
            ProxyMatcher::All => None,
            ProxyMatcher::Entries(e) => Some(e.into_iter().map(|e| e.source).collect()),
        }
    }

    #[test]
    fn config_matcher_is_every_path_without_one() {
        assert_eq!(read("export function proxy() {}\n"), None);
        assert_eq!(read("export const config = { runtime: 'nodejs' }\n"), None);
    }

    #[test]
    fn config_matcher_reads_a_string_and_an_array() {
        assert_eq!(read("export const config = { matcher: '/about/:rest*' }\n").unwrap(), ["/about/:rest*"]);
        let src = "export const config: MiddlewareConfig = {\n  matcher: [\n    '/dashboard/:rest*',\n    { source: '/api/:rest*', missing: [{ type: 'header', key: 'x-internal' }] },\n    `/account`,\n  ],\n}\n";
        assert_eq!(read(src).unwrap(), ["/dashboard/:rest*", "/api/:rest*", "/account"]);
    }

    #[test]
    fn config_matcher_unescapes_the_string() {
        assert_eq!(read("export const config = { matcher: ['/((?!.*\\\\..*|_next).*)'] }\n").unwrap(), [r"/((?!.*\..*|_next).*)"]);
    }

    #[test]
    fn config_matcher_reads_as_nothing_what_next_could_not_read() {
        for src in [
            "const extra = '/x'\nexport const config = { matcher: ['/kept', extra] }\n",
            "export const config = { matcher: ['/kept', ...more] }\n",
            "export const config = { matcher: ['/kept', { source: `/u/${id}` }] }\n",
            "export const config = { matcher: PROTECTED }\n",
            "const matcher = [\"/a\"]\nexport const config = { matcher }\n",
            "export const config = makeConfig()\n",
        ] {
            assert_eq!(read(src), Some(vec![]), "{src}");
        }
    }

    #[test]
    fn config_matcher_skips_one_in_a_comment() {
        let src = "// export const config = { matcher: '/old' }\nexport const config = {\n  /* '/older' */ matcher: '/new',\n}\n";
        assert_eq!(read(&codegraph_text::strip_comments_for_regex(src, "typescript")).unwrap(), ["/new"]);
    }

    #[test]
    fn config_matcher_says_where_each_entry_is_written() {
        let src = "export const config = {\n  matcher: ['/a', '/b'],\n}\n";
        let ProxyMatcher::Entries(out) = read_proxy_matcher(src) else { panic!("entries") };
        assert_eq!(out.iter().map(|e| &src[e.index..e.index + 4]).collect::<Vec<_>>(), ["'/a'", "'/b'"]);
    }

    #[test]
    fn where_next_loads_the_proxy_from() {
        assert_eq!(next_proxy_dir("", &["app/page.tsx"]), "");
        assert_eq!(next_proxy_dir("", &["src/app/page.tsx"]), "src/");
        assert_eq!(next_proxy_dir("apps/web/", &["apps/web/src/app/dashboard/page.tsx"]), "apps/web/src/");
        assert_eq!(next_proxy_dir("", &["pages/index.tsx", "src/app/page.tsx"]), "");
    }
}
