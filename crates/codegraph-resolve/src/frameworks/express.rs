//! Express / Node.js framework resolver. Port of `src/resolution/frameworks/express.ts`.
//!
//! Offsets here are byte offsets into the comment-stripped source. Every character the scanners
//! look at is ASCII, so a byte scan finds the same places the JavaScript original finds with
//! UTF-16 indexes; the fixed-size lookaround windows are cut in UTF-16 units (`utf16_*`) and
//! columns are reported in them.

use super::now_ms;
use super::package_deps::depends_on;
use crate::ctx::Ctx;
use crate::import_resolver::resolve_import_path;
use crate::types::{FrameworkExtractionResult, FrameworkResolver, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_text::REGEX_START_BEFORE;
use codegraph_types::jscompat::{is_js_space, js_trim, utf16_len, utf16_prefix};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use indexmap::{IndexMap, IndexSet};
use std::collections::HashSet;

pub struct ExpressResolver;

const LANGUAGES: [Language; 2] = [Language::JavaScript, Language::TypeScript];

fn extract_tail_ident(expr: &str) -> Option<String> {
    let squeezed = re!(r"\s+", "g").replace_all_str(expr, "");
    let cleaned = re!(r"\(\)$").replace_first(&squeezed, |_| String::new());
    re!(r"(?:\.|^)([A-Za-z_][A-Za-z0-9_]*)$").captures(&cleaned).and_then(|m| m.text(1).map(String::from))
}

/// Index of the delimiter matching the one at `open`, skipping string/template literals so a `)`
/// or `}` inside a string doesn't throw off the balance.
fn match_delim(s: &str, open: usize, oc: u8, cc: u8) -> Option<usize> {
    let b = s.as_bytes();
    let n = b.len();
    let mut depth = 0i32;
    let mut i = open;
    while i < n {
        let ch = b[i];
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            let q = ch;
            i += 1;
            while i < n && b[i] != q {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if ch == oc {
            depth += 1;
        } else if ch == cc {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

// Express res/req methods + common JS builtins — calls to these inside a handler body are
// framework/noise, not the business flow we want to surface as route edges.
const RESERVED_CALLS: &[&str] = &[
    "json", "jsonp", "send", "sendStatus", "sendFile", "status", "end", "redirect", "render", "set", "get", "header", "type", "format",
    "attachment", "download", "cookie", "clearCookie", "append", "location", "vary", "links", "accepts", "is", "next", "then", "catch",
    "finally", "resolve", "reject", "all", "race", "map", "filter", "forEach", "reduce", "find", "push", "pop", "slice", "splice", "includes",
    "keys", "values", "entries", "assign", "parse", "stringify", "log", "error", "warn", "info", "String", "Number", "Boolean", "Array",
    "Object", "Date", "Math", "JSON", "Promise", "require", "fail",
];

// Keywords a `name(` scan over a handler body would otherwise read as calls.
const JS_KEYWORDS: [&str; 19] = [
    "if", "for", "while", "switch", "function", "async", "return", "typeof", "await", "new", "do", "else", "with", "yield", "void", "delete", "in",
    "super", "import",
];

/// The `n` UTF-16 units of `s` that end at byte `end`.
fn utf16_before(s: &str, end: usize, n: usize) -> &str {
    let mut units = 0;
    let mut start = end;
    for (i, c) in s[..end].char_indices().rev() {
        units += c.len_utf16();
        if units > n {
            break;
        }
        start = i;
    }
    &s[start..end]
}

/// The calls an inline handler's body makes, each once, framework noise aside. A member call keeps
/// its receiver (`userService.find`, `c.text`) so it resolves as one: bare, hono's `c.text('…')`
/// matched its client's `ClientResponse.text` 423 times. A member of an expression (`a.b().c(`)
/// names nothing this can follow.
fn handler_call_names(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let call_re = re!(r"((?:[A-Za-z_$][\w$]*\s*\??\.\s*)*)([A-Za-z_$][\w$]*)\s*\(", "g");
    for cm in call_re.captures_iter(body) {
        let Some(whole) = cm.get(0) else { continue };
        let method = cm.text(2).unwrap_or("");
        let receiver_raw: String = cm.text(1).unwrap_or("").chars().filter(|c| !is_js_space(*c) && *c != '?').collect();
        let receiver = receiver_raw.strip_suffix('.').unwrap_or(&receiver_raw);
        let before = &body[..whole.start()];
        if receiver.is_empty() && before.trim_end_matches(is_js_space).ends_with('.') {
            continue;
        }
        // A keyword is only a keyword when it isn't a member: `if (` is not a call,
        // `userService.delete(` is. `function helper(` declares helper.
        if receiver.is_empty() && JS_KEYWORDS.contains(&method) {
            continue;
        }
        if receiver.is_empty() && re!(r"\bfunction\s*\*?\s*$").is_match(utf16_before(body, whole.start(), 12)) {
            continue;
        }
        let name = if receiver.is_empty() { method.to_string() } else { format!("{receiver}.{method}") };
        if seen.contains(&name) || RESERVED_CALLS.contains(&method) {
            continue;
        }
        seen.insert(name.clone());
        out.push(name);
    }
    out
}

/// The replies an inline handler makes — `res.status(404).json({…})`, `res.json(user)`,
/// `reply.send(…)`, `ctx.body = …` aside — as references the Steps view's effect table reads at
/// their own line and column. The body's plain calls above skip these names as framework noise on
/// purpose (they are not the business flow); for the endpoint's contract they are the point.
fn reply_refs(safe: &str, body_start: usize, body_end: usize, from_node_id: &str, file_path: &str, language: Language) -> Vec<UnresolvedRef> {
    let mut out: Vec<UnresolvedRef> = Vec::new();
    let body = sub(safe, body_start, body_end);
    let reply_call =
        re!(r"\b(res|response|reply|rep|ctx)\s*\.\s*(?:[A-Za-z_$][\w$]*\s*\([^()]*\)\s*\.\s*)*([A-Za-z_$][\w$]*)\s*\(", "g");
    for m in reply_call.captures_iter(body) {
        let Some(whole) = m.get(0) else { continue };
        let at = body_start + whole.start();
        let before = &safe[..at];
        let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
        out.push(UnresolvedRef::new(
            from_node_id,
            format!("{}.{}", m.text(1).unwrap_or(""), m.text(2).unwrap_or("")),
            EdgeKind::Calls,
            before.matches('\n').count() as u32 + 1,
            utf16_len(&before[line_start..]) as u32,
            file_path,
            language,
        ));
    }
    out
}

/// `s[from..to]`, empty when the range is not one.
fn sub(s: &str, from: usize, to: usize) -> &str {
    s.get(from..to).unwrap_or("")
}

fn lang_name(language: Language) -> &'static str {
    if language == Language::TypeScript {
        "typescript"
    } else {
        "javascript"
    }
}

/// Detect language from file extension.
fn detect_language(file_path: &str) -> Language {
    if file_path.ends_with(".ts") || file_path.ends_with(".tsx") {
        Language::TypeScript
    } else {
        Language::JavaScript
    }
}

fn is_script_file(file_path: &str) -> bool {
    re!(r"\.(m?js|tsx?|cjs)$").is_match(file_path)
}

impl FrameworkResolver for ExpressResolver {
    fn name(&self) -> &str {
        "express"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        // Express in a package.json — the root's, or a workspace's (`backend/`, `apps/api/`).
        if depends_on(ctx, &["express", "fastify", "koa", "hapi", "@hapi/hapi"]) {
            return true;
        }

        // Check for common Express patterns
        for file in ctx.get_all_files() {
            if file.contains("routes") || file.contains("controllers") || file.contains("middleware") {
                if let Some(content) = ctx.read_file(&file).filter(|c| !c.is_empty()) {
                    if content.contains("express") || content.contains("app.get") || content.contains("router.get") {
                        return true;
                    }
                }
            }
        }

        false
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        // A JS/TS module reaches another file's middleware, controller or service only through an
        // `import` / `require` — the import resolver's — never by name: SvelteKit's remote
        // functions' `validate(arg)` went to the config loader's `validate` in another package. The
        // patterns below stay for the file's own declarations.
        let same_file = |id: Option<String>| -> Option<String> {
            let id = id?;
            match ctx.node_by_id(&id) {
                Some(node) if node.file_path == r.file_path => Some(id),
                _ => None,
            }
        };
        let name = r.reference_name.as_str();

        // Pattern 1: Middleware references
        if is_middleware_name(name) {
            if let Some(result) = same_file(resolve_middleware(name, ctx, Some(&r.file_path))) {
                return Some(ResolvedRef::new(r, result, 0.8, ResolvedBy::Framework));
            }
        }

        // Pattern 2: Controller method references
        if let Some(controller_match) = re!(r"^(\w+)Controller\.(\w+)$").captures(name) {
            let controller = controller_match.text(1).unwrap_or("");
            let method = controller_match.text(2).unwrap_or("");
            if let Some(result) = same_file(resolve_controller_method(controller, method, ctx)) {
                return Some(ResolvedRef::new(r, result, 0.85, ResolvedBy::Framework));
            }
        }

        // Pattern 3: Service/helper references
        if let Some(service_match) = re!(r"^(\w+)(Service|Helper|Utils?)\.(\w+)$").captures(name) {
            let service = format!("{}{}", service_match.text(1).unwrap_or(""), service_match.text(2).unwrap_or(""));
            let method = service_match.text(3).unwrap_or("");
            if let Some(result) = same_file(resolve_service_method(&service, method, ctx)) {
                return Some(ResolvedRef::new(r, result, 0.8, ResolvedBy::Framework));
            }
        }

        None
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<FrameworkExtractionResult> {
        if !is_script_file(file_path) {
            return Some(FrameworkExtractionResult::default());
        }
        let mut nodes: Vec<Node> = Vec::new();
        let mut references: Vec<UnresolvedRef> = Vec::new();
        let now = now_ms();
        let lang = detect_language(file_path);
        let safe = codegraph_text::strip_comments_for_regex(content, lang_name(lang));
        let safe: &str = &safe;

        let route_node = |line: u32, method: &str, route_path: &str, end_column: u32| {
            let upper = method.to_uppercase();
            Node::new(
                format!("route:{file_path}:{line}:{upper}:{route_path}"),
                NodeKind::Route,
                format!("{upper} {route_path}"),
                format!("{file_path}::{upper}:{route_path}"),
                file_path,
                lang,
                line,
                line,
                0,
                end_column,
                now,
            )
        };
        let line_of = |index: usize| safe[..index].matches('\n').count() as u32 + 1;

        // Match the route head up to the first arg: (app|router).METHOD('/path', (NOT the whole
        // call — handlers are often inline arrows whose `)`/`{}` the old single-regex couldn't
        // span, so inline-handler routes connected to nothing.)
        let head = re!(r#"\b(app|router)\s*\.\s*(get|post|put|patch|delete|all|use)\s*\(\s*['"]([^'"]+)['"]\s*,"#, "g");
        for m in head.captures_iter(safe) {
            let Some(whole) = m.get(0) else { continue };
            let method = m.text(2).unwrap_or("");
            let route_path = m.text(3).unwrap_or("");
            if method == "use" && !route_path.starts_with('/') {
                continue;
            }
            let line = line_of(whole.start());
            let node = route_node(line, method, route_path, utf16_len(whole.as_str()) as u32);
            let route_id = node.id.clone();
            nodes.push(node);

            // The full argument list = balanced parens from the route call's open paren.
            let Some(open_rel) = safe[whole.start()..].find('(') else { continue };
            let open_paren = whole.start() + open_rel;
            let call = scan_call(safe, open_paren);
            let args = call.as_ref().map(|c| inner(&c.structure)).unwrap_or("");
            references.extend(handler_refs(safe, open_paren + 1, args, &route_id, line, file_path, lang));
        }
        // The chained form: `router.route('/:id').get(getProduct).put(protect, updateProduct)` —
        // one path, several methods, each with its own handler. One route node per method, at the
        // line of its `.method(`, bound like the plain form.
        let chain_head = re!(r#"\b(?:app|router)\s*\.\s*route\s*\(\s*['"]([^'"]+)['"]\s*\)"#, "g");
        for m in chain_head.captures_iter(safe) {
            let Some(whole) = m.get(0) else { continue };
            let route_path = m.text(1).unwrap_or("");
            let mut at = whole.end();
            loop {
                let window = utf16_prefix(&safe[at..], 64);
                let Some(link) = re!(r"^\s*\.\s*(get|post|put|patch|delete|all)\s*\(").captures(window) else { break };
                let Some(link_whole) = link.get(0) else { break };
                let open_paren = at + link_whole.end() - 1;
                let Some(call) = scan_call(safe, open_paren) else { break };
                let method = link.text(1).unwrap_or("");
                let line = line_of(open_paren);
                let args = inner(&call.structure);
                let node = route_node(line, method, route_path, utf16_len(link_whole.as_str()) as u32);
                let route_id = node.id.clone();
                nodes.push(node);
                references.extend(handler_refs(safe, open_paren + 1, args, &route_id, line, file_path, lang));
                at = call.close + 1;
            }
        }
        Some(FrameworkExtractionResult { nodes, references })
    }

    /// Cross-file finalization for mounts. A router's routes are written relative to where it is
    /// mounted —
    ///
    /// ```text
    /// app.use('/api', routes)            // app.js
    /// router.use('/users', usersRouter)  // routes/index.js
    /// router.post('/', createUser)       // routes/users.js  → POST /api/users
    /// ```
    ///
    /// — and per-file `extract()` can only see `POST /`. This pass reads every
    /// `X.use('/prefix', …, router)` whose last argument names a router file (an import, or an
    /// inline `require('./x')`), composes the prefixes down the mount tree, and renames the routes
    /// of each mounted file to the path a request actually takes. A file mounted at two different
    /// prefixes is left alone: one name cannot be two paths.
    ///
    /// The route node's `id` and `qualifiedName` are preserved (`qualifiedName` still encodes the
    /// in-file `METHOD:path`), so the pass is idempotent on every sync, exactly as the NestJS
    /// `RouterModule` pass is.
    fn post_extract(&self, ctx: &Ctx) -> Option<Vec<Node>> {
        let files: Vec<String> = ctx.get_all_files().into_iter().filter(|f| is_script_file(f)).collect();
        let mut mounts: IndexMap<String, Vec<(String, String)>> = IndexMap::new();
        for file in &files {
            // Most files mount nothing: look for `.use(` before decoding the file.
            if !ctx.file_contains(file, ".use(") {
                continue;
            }
            let Some(content) = ctx.read_file(file).filter(|c| !c.is_empty() && c.contains(".use(")) else { continue };
            let lang = detect_language(file);
            let safe = codegraph_text::strip_comments_for_regex(&content, lang_name(lang));
            let safe: &str = &safe;
            let mount = re!(r#"\b[A-Za-z_$][\w$]*\.use\s*\(\s*(['"`])(/[^'"`]*)\1\s*,"#, "g");
            for m in mount.captures_iter(safe) {
                let Some(whole) = m.get(0) else { continue };
                let Some(open_rel) = safe[whole.start()..].find('(') else { continue };
                let open = whole.start() + open_rel;
                let Some(call) = scan_call(safe, open) else { continue };
                // Split where the blanked structure says, but read the target from `safe`: a
                // `require('./users')` target needs its string.
                let parts = top_level_args(inner(&call.structure));
                let last = match parts.last() {
                    Some(last_part) => js_trim(sub(safe, open + 1 + last_part.start, call.close)).to_string(),
                    None => String::new(),
                };
                let Some(target) = mount_target(&last, safe, file, lang, ctx) else { continue };
                if &target == file {
                    continue;
                }
                mounts.entry(file.clone()).or_default().push((m.text(2).unwrap_or("").to_string(), target));
            }
        }
        if mounts.is_empty() {
            return Some(Vec::new());
        }

        // Compose prefixes down the mount tree until nothing changes; a file reached at two
        // different paths is ambiguous and dropped.
        let mut prefix_of: IndexMap<String, String> = IndexMap::new();
        for _round in 0..8 {
            let mut next: IndexMap<String, String> = IndexMap::new();
            let mut ambiguous: IndexSet<String> = IndexSet::new();
            for (file, list) in &mounts {
                let base = prefix_of.get(file).map(String::as_str).unwrap_or("");
                for (prefix, target) in list {
                    let full = join_paths(base, prefix);
                    match next.get(target) {
                        Some(seen) if *seen != full => {
                            ambiguous.insert(target.clone());
                        }
                        _ => {
                            next.insert(target.clone(), full);
                        }
                    }
                }
            }
            for a in &ambiguous {
                next.shift_remove(a);
            }
            let mut changed = next.len() != prefix_of.len();
            if !changed {
                changed = next.iter().any(|(k, v)| prefix_of.get(k) != Some(v));
            }
            prefix_of = next;
            if !changed {
                break;
            }
        }

        let mut updates: Vec<Node> = Vec::new();
        for (file, prefix) in &prefix_of {
            if prefix.is_empty() || prefix == "/" {
                continue;
            }
            for route in ctx.get_nodes_in_file(file).iter() {
                if route.kind != NodeKind::Route {
                    continue;
                }
                let Some(sep) = route.qualified_name.find("::") else { continue };
                let Some(colon_rel) = route.qualified_name[sep + 2..].find(':') else { continue };
                let colon = sep + 2 + colon_rel;
                let method = &route.qualified_name[sep + 2..colon];
                let original = &route.qualified_name[colon + 1..];
                if !original.starts_with('/') {
                    continue;
                }
                let name = format!("{method} {}", join_paths(prefix, original));
                if name != route.name {
                    let mut updated = (**route).clone();
                    updated.name = name;
                    updates.push(updated);
                }
            }
        }
        Some(updates)
    }
}

/// `structure.slice(1, -1)`.
fn inner(structure: &str) -> &str {
    sub(structure, 1, structure.len().saturating_sub(1))
}

/// A scanned call: where its `)` is, and its text with string contents and regex-literal bodies
/// blanked (offsets kept).
struct Call {
    close: usize,
    structure: String,
}

/// The call whose `(` is at `open`, scanned from there to its matching `)`: its close, and its
/// text with string contents and regex-literal bodies blanked (offsets kept) so `/[^]]/`, `/}/`,
/// `'a, b)'` or a backtick in a regex can't end an argument or a body early. Scanned per call,
/// from the call's own paren — never from the top of the file — so nothing earlier in the file
/// can throw it off.
fn scan_call(safe: &str, open: usize) -> Option<Call> {
    let b = safe.as_bytes();
    let n = b.len();
    let mut out: Vec<u8> = Vec::new();
    let mut depth = 0i32;
    let mut i = open;
    while i < n {
        let c = b[i];
        if (c == b'"' || c == b'\'') && !closes_on_line(b, i) {
            // JSX text (`<p>Don't</p>`), not a string: an apostrophe with no closing quote on its
            // line must not blank the `)` after it.
            out.push(c);
            i += 1;
            continue;
        }
        if c == b'"' || c == b'\'' || c == b'`' {
            out.push(c);
            i += 1;
            while i < n && b[i] != c {
                if c != b'`' && b[i] == b'\n' {
                    break;
                }
                if b[i] == b'\\' && i + 1 < n {
                    out.push(b' ');
                    i += 1;
                }
                out.push(if b[i] == b'\n' { b'\n' } else { b' ' });
                i += 1;
            }
            if i < n {
                out.push(b[i]);
            }
            i += 1;
            continue;
        }
        if c == b'/' && b.get(i + 1) != Some(&b'/') && b.get(i + 1) != Some(&b'*') {
            if let Some(end) = regex_literal_end_at(safe, i) {
                out.push(b'/');
                out.extend(std::iter::repeat_n(b' ', end - i - 1));
                out.push(b'/');
                i = end + 1;
                continue;
            }
        }
        out.push(c);
        if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
            if depth == 0 {
                let structure = String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
                return Some(Call { close: i, structure });
            }
        }
        i += 1;
    }
    None
}

/// Whether the quote at `i` has an unescaped closing quote on the same line.
fn closes_on_line(b: &[u8], i: usize) -> bool {
    let q = b[i];
    let mut k = i + 1;
    while k < b.len() && b[k] != b'\n' {
        if b[k] == b'\\' {
            k += 1;
        } else if b[k] == q {
            return true;
        }
        k += 1;
    }
    false
}

/// The closing `/` of the regex literal the `/` at `i` opens, or `None` when it divides. A regex
/// opens after punctuation or a keyword (`REGEX_START_BEFORE`) but not after `)`, `]`, a postfix
/// `!` (`done! / total`), `++` or `--`; it closes on the same line; and what follows the close
/// must end a regex — flags, then `.test(`, `,`, `;`, `)`, … — since treating a division as one
/// (`(a - b) / total * 100) / 100`, JSX `</li>…</ul>`) would hide a bracket.
fn regex_literal_end_at(safe: &str, i: usize) -> Option<usize> {
    let b = safe.as_bytes();
    let n = b.len();
    let mut p = i as isize - 1;
    while p >= 0 && (b[p as usize] == b' ' || b[p as usize] == b'\t') {
        p -= 1;
    }
    if p >= 0 && matches!(b[p as usize], b')' | b']' | b'}') {
        return None;
    }
    if p >= 1 && b[p as usize] == b'!' {
        let prev = b[p as usize - 1];
        if prev.is_ascii_alphanumeric() || matches!(prev, b'_' | b'$' | b')' | b']') {
            return None;
        }
    }
    if p >= 1 && matches!(b[p as usize], b'+' | b'-') && b[p as usize - 1] == b[p as usize] {
        return None;
    }
    if !REGEX_START_BEFORE.is_match(utf16_before(safe, i, 32)) {
        return None;
    }
    let mut end = i + 1;
    let mut in_class = false;
    while end < n && b[end] != b'\n' {
        if b[end] == b'\\' {
            end += 2;
            continue;
        }
        if b[end] == b'[' {
            in_class = true;
        } else if b[end] == b']' {
            in_class = false;
        } else if b[end] == b'/' && !in_class {
            break;
        }
        end += 1;
    }
    if end >= n || b[end] != b'/' {
        return None;
    }
    // What ends a regex literal: its flags, then a member access, a separator or a closing
    // bracket, an operator, or the end of the line.
    let ends = re!(r"^[dgimsuyv]*[ \t]*(?:\.\s*(?:test|exec|source|flags|global|lastIndex)\b|[,;)\]}:?]|&&|\|\||\r?\n|$)")
        .is_match(utf16_prefix(&safe[end + 1..], 24));
    if ends {
        Some(end)
    } else {
        None
    }
}

fn function_body_end(text: &str, fn_at: usize) -> Option<usize> {
    let params_open = text[fn_at..].find('(').map(|i| fn_at + i)?;
    let params_close = match_delim(text, params_open, b'(', b')')?;
    let brace_at = text[params_close..].find('{').map(|i| params_close + i)?;
    match_delim(text, brace_at, b'{', b'}')
}

/// A top-level argument: its text and offset into the argument list.
#[derive(Clone)]
struct Arg {
    text: String,
    start: usize,
}

/// `args` split at its top-level commas, each piece with its offset into `args`.
fn top_level_args(args: &str) -> Vec<Arg> {
    let b = args.as_bytes();
    let n = b.len();
    let mut out: Vec<Arg> = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < n {
        let ch = b[i];
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            let q = ch;
            i += 1;
            while i < n && b[i] != q {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if ch == b'<' && i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_' || b[i - 1] == b'$') {
            // A type-argument list (`asyncHandler<Req, Res>(`): its commas split nothing.
            if let Some(generic) = re!(r"^<[^()<>]*(?:<[^()<>]*>[^()<>]*)*>\s*\(").captures(&args[i..]) {
                let len = generic.get(0).map(|w| w.end()).unwrap_or(0);
                i += len - 2;
                i += 1;
                continue;
            }
        }
        if ch == b'(' || ch == b'[' || ch == b'{' {
            depth += 1;
        } else if ch == b')' || ch == b']' || ch == b'}' {
            depth -= 1;
        } else if ch == b',' && depth == 0 {
            out.push(Arg { text: sub(args, start, i).to_string(), start });
            start = i + 1;
        }
        i += 1;
    }
    out.push(Arg { text: args.get(start..).unwrap_or("").to_string(), start });
    out
}

/// What a route call's handler contributes. The handler is the LAST argument — the ones before it
/// are middleware, and an arrow inside one of those (`rateLimit({ keyGenerator: (req) => req.ip
/// })`, `(req, res, next) => next()`) says nothing about the handler. A handler that is a function
/// — an arrow, a `function` expression, or one wrapped in a call (`asyncHandler(async (req, res)
/// => …)`) — is anonymous, so its body's calls are attributed to the route as `calls` edges and
/// `trace(route, service)` connects: the body is the balanced `{…}` after the arrow or parameter
/// list, or an arrow's expression tail. Anything else is a named handler: a `references` edge to
/// its last name (`listUsers`, `userController.list`).
///
/// `args_start` is where `args` begins in `safe`, for the reply-call offsets.
fn handler_refs(safe: &str, args_start: usize, args: &str, from_node_id: &str, line: u32, file_path: &str, language: Language) -> Vec<UnresolvedRef> {
    // A trailing options object (`…, handler, { cache: true }`) is not the handler; a trailing
    // array of handlers (`[validate, ctrl.create]`) is read as its last element.
    let mut parts: Vec<Arg> = top_level_args(args).into_iter().filter(|a| !js_trim(&a.text).is_empty()).collect();
    while parts.len() > 1 && parts.last().is_some_and(|p| p.text.trim_start_matches(is_js_space).starts_with('{')) {
        parts.pop();
    }
    if let Some(last_part) = parts.last().cloned() {
        let array_at = last_part.text.char_indices().find(|(_, c)| !is_js_space(*c)).map(|(i, _)| i);
        if let Some(array_at) = array_at.filter(|at| last_part.text.as_bytes()[*at] == b'[') {
            let close = match_delim(&last_part.text, array_at, b'[', b']').filter(|c| *c > array_at);
            let inner_arg = close.and_then(|close| {
                top_level_args(sub(&last_part.text, array_at + 1, close)).into_iter().rfind(|a| !js_trim(&a.text).is_empty())
            });
            match inner_arg {
                Some(inner_arg) => {
                    let idx = parts.len() - 1;
                    parts[idx] = Arg { text: inner_arg.text, start: last_part.start + array_at + 1 + inner_arg.start };
                }
                None => {
                    parts.pop();
                }
            }
        }
    }
    let Some(handler) = parts.last() else { return Vec::new() };
    let text = handler.text.as_str();
    // A `function` is the handler's own only when it opens the argument — directly, or inside
    // wrapper calls (`asyncHandler(function …)`); then its body starts after its parameter list,
    // not after an arrow inside it (`items.map((i) => …)`). A `function` anywhere else (a default
    // parameter, a wrapper's options object) is not the handler.
    let mut arrow_at: Option<usize> = text.find("=>");
    let opening = re!(r"^\s*(?:[A-Za-z_$][\w$.]*\s*\(\s*)*(?:async\s+)?function\b[^(]*\(").captures(text);
    let mut fn_at: Option<usize> = opening.as_ref().and_then(|o| o.get(0)).and_then(|w| w.as_str().rfind("function"));
    if let (Some(at), Some(opening)) = (fn_at, &opening) {
        // Inside a wrapper, a `function` followed by an arrow after its body is a callback
        // argument (`withErrors(function onErr(e) {…}, async (req, res) => …)`): the arrow is
        // the handler.
        let fn_body_end = function_body_end(text, at);
        let opening_text = opening.get(0).map(|w| w.as_str()).unwrap_or("");
        let inside_wrapper = opening_text[..at].contains('(');
        let later_arrow = match fn_body_end {
            Some(end) if inside_wrapper => text[end..].find("=>").map(|i| end + i),
            _ => None,
        };
        if later_arrow.is_some() {
            arrow_at = later_arrow;
            fn_at = None;
        } else if inside_wrapper && fn_body_end.is_some_and(|end| !re!(r"^[\s)]*$").is_match(&text[end + 1..])) {
            // A callback followed by more arguments (`withErrors(function onErr(e) {…},
            // ctrl.list)`) is not the handler.
            arrow_at = None;
            fn_at = None;
        } else {
            arrow_at = None;
        }
    }

    if arrow_at.is_none() && fn_at.is_none() {
        let tail = text.rfind(',').map(|i| &text[i + 1..]).unwrap_or(text);
        let handler_name = extract_tail_ident(js_trim(text)).or_else(|| extract_tail_ident(js_trim(tail)));
        return match handler_name {
            Some(name) => vec![UnresolvedRef::new(from_node_id, name, EdgeKind::References, line, 0, file_path, language)],
            None => Vec::new(),
        };
    }

    let body_from = match arrow_at {
        Some(at) => at + 2,
        None => {
            let params_open = text[fn_at.unwrap_or(0)..].find('(').map(|i| fn_at.unwrap_or(0) + i);
            let params_close = params_open.and_then(|open| match_delim(text, open, b'(', b')').map(|close| (open, close)));
            match params_close {
                Some((open, close)) if close > open => close + 1,
                _ => text.len(),
            }
        }
    };
    let after_params = text.get(body_from..).unwrap_or("");
    let mut body = after_params;
    let mut body_start = args_start + handler.start + body_from;
    if let Some(brace_at) = after_params.find('{') {
        if js_trim(&after_params[..brace_at]).is_empty() {
            if let Some(end) = match_delim(after_params, brace_at, b'{', b'}').filter(|e| *e > brace_at) {
                body = &after_params[brace_at + 1..end];
                body_start += brace_at + 1;
            }
        }
    }

    // Positions came from the blanked structure; the calls are read from `safe` at the same
    // offsets, so a call inside a template interpolation counts.
    let source = sub(safe, body_start, body_start + body.len());
    let mut refs: Vec<UnresolvedRef> = handler_call_names(source)
        .into_iter()
        .map(|name| UnresolvedRef::new(from_node_id, name, EdgeKind::Calls, line, 0, file_path, language))
        .collect();
    refs.extend(reply_refs(safe, body_start, body_start + body.len(), from_node_id, file_path, language));
    refs
}

/// `/api` + `/users` → `/api/users`; `/api/` + `/` → `/api`.
fn join_paths(prefix: &str, path: &str) -> String {
    let a = prefix.trim_end_matches('/');
    let b = path.trim_start_matches('/');
    let joined = if b.is_empty() { a.to_string() } else { format!("{a}/{b}") };
    if joined.is_empty() {
        "/".to_string()
    } else {
        joined
    }
}

/// The file a mount's last argument names: an inline `require('./x')`, an identifier imported from
/// a project file, or one bound to `require('./x')` in the same file. `x.default` / `x.router`
/// count as `x`.
fn mount_target(expr: &str, safe: &str, file: &str, lang: Language, ctx: &Ctx) -> Option<String> {
    if let Some(inline) = re!(r#"^require\s*\(\s*(['"])([^'"]+)\1\s*\)(?:\.\w+)?$"#).captures(expr) {
        return resolve_import_path(inline.text(2)?, file, lang, ctx);
    }
    let ident = re!(r"^([A-Za-z_$][\w$]*)(?:\.(?:default|router|routes))?$").captures(expr)?;
    let name = ident.text(1)?;
    let mappings = ctx.get_import_mappings(file, lang);
    if let Some(mapping) = mappings.iter().find(|im| im.local_name == name) {
        return resolve_import_path(&mapping.source, file, lang, ctx);
    }
    let pattern = format!(r#"\b(?:const|let|var)\s+{}\s*=\s*require\s*\(\s*(['"])([^'"]+)\1\s*\)"#, name.replace('$', r"\$"));
    let required = Re::cached(&pattern, "");
    let spec = required.captures(safe).and_then(|c| c.text(2).map(String::from))?;
    resolve_import_path(&spec, file, lang, ctx)
}

/// Check if a name looks like middleware.
fn is_middleware_name(name: &str) -> bool {
    Re::cached(
        r"^(?:auth|authenticate|authorization|cors|helmet|logger|errorHandler|notFound)$|^(?:validate|sanitize|rateLimit)|Middleware$",
        "i",
    )
    .is_match(name)
}

/// Node kinds a middleware can be declared as.
const MIDDLEWARE_KINDS: [NodeKind; 5] = [NodeKind::Function, NodeKind::Method, NodeKind::Variable, NodeKind::Constant, NodeKind::Class];

/// Resolve middleware reference using name-based lookup.
fn resolve_middleware(name: &str, ctx: &Ctx, from_file: Option<&str>) -> Option<String> {
    // Try exact name first — the calling file's own first. A middleware is a declaration (a
    // function, a value, a class), never the file's import of a package (`import cors from
    // 'cors'`) nor a heading or a module node.
    let by_name = ctx.get_nodes_by_name(name);
    let eligible = by_name.iter().filter(|n| MIDDLEWARE_KINDS.contains(&n.kind));
    let (own, other): (Vec<_>, Vec<_>) = eligible.partition(|n| Some(n.file_path.as_str()) == from_file);
    let suffix = Re::cached("Middleware$", "i");
    let lower = name.to_lowercase();
    let stripped = suffix.replace_first(name, |_| String::new()).to_lowercase();
    if let Some(found) = own.iter().chain(other.iter()).find(|n| {
        let n_lower = n.name.to_lowercase();
        n_lower == lower || n_lower == stripped
    }) {
        return Some(found.id.clone());
    }

    // Try without Middleware suffix
    let base_name = suffix.replace_first(name, |_| String::new());
    if base_name != name {
        let base_all = ctx.get_nodes_by_name(&base_name);
        let base_candidates: Vec<_> = base_all.iter().filter(|n| MIDDLEWARE_KINDS.contains(&n.kind)).collect();
        let preferred = base_candidates.iter().find(|n| ["/middleware/", "/middlewares/"].iter().any(|d| n.file_path.contains(d)));
        if let Some(preferred) = preferred {
            return Some(preferred.id.clone());
        }
        if let Some(first) = base_candidates.first() {
            return Some(first.id.clone());
        }
    }

    None
}

/// Resolve controller method using name-based lookup.
fn resolve_controller_method(controller: &str, method: &str, ctx: &Ctx) -> Option<String> {
    // Look for the method name directly
    let controller_lower = controller.to_lowercase();
    let method_nodes = ctx.get_nodes_by_name(method);
    if let Some(found) = method_nodes
        .iter()
        .find(|n| matches!(n.kind, NodeKind::Method | NodeKind::Function) && n.file_path.to_lowercase().contains(&controller_lower))
    {
        return Some(found.id.clone());
    }

    // Fall back: look for controller class, then find the method in its file
    let controller_name = format!("{controller}Controller");
    for ctrl in ctx.get_nodes_by_name(&controller_name).iter() {
        let in_file = ctx.get_nodes_in_file(&ctrl.file_path);
        if let Some(method_node) = in_file.iter().find(|n| matches!(n.kind, NodeKind::Method | NodeKind::Function) && n.name == method) {
            return Some(method_node.id.clone());
        }
    }

    None
}

/// Resolve service/helper method using name-based lookup.
fn resolve_service_method(service_name: &str, method: &str, ctx: &Ctx) -> Option<String> {
    // Look for the method in files matching the service name
    let stripped = Re::cached("(Service|Helper|Utils?)$", "i").replace_first(service_name, |_| String::new()).to_lowercase();
    let method_nodes = ctx.get_nodes_by_name(method);
    method_nodes
        .iter()
        .find(|n| matches!(n.kind, NodeKind::Method | NodeKind::Function) && n.file_path.to_lowercase().contains(&stripped))
        .map(|n| n.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(src: &str) -> FrameworkExtractionResult {
        ExpressResolver.extract("routes/users.js", src).unwrap()
    }

    #[test]
    fn named_and_inline_handlers() {
        let src = "const router = require('express').Router();\nrouter.get('/', listUsers);\nrouter.post('/', auth, async (req, res) => {\n  const u = await userService.create(req.body);\n  res.status(201).json(u);\n});\n";
        let result = extract(src);
        let names: Vec<&str> = result.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["GET /", "POST /"]);
        let refs: Vec<(&str, EdgeKind)> = result.references.iter().map(|r| (r.reference_name.as_str(), if r.is_kind(EdgeKind::Calls) { EdgeKind::Calls } else { EdgeKind::References })).collect();
        assert_eq!(refs[0], ("listUsers", EdgeKind::References));
        assert!(refs.contains(&("userService.create", EdgeKind::Calls)));
        assert!(refs.contains(&("res.json", EdgeKind::Calls)));
    }

    #[test]
    fn chained_route_gets_one_node_per_method() {
        let src = "router.route('/:id').get(getProduct).put(protect, updateProduct);\n";
        let result = extract(src);
        let names: Vec<&str> = result.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["GET /:id", "PUT /:id"]);
        let refs: Vec<&str> = result.references.iter().map(|r| r.reference_name.as_str()).collect();
        assert_eq!(refs, vec!["getProduct", "updateProduct"]);
    }

    #[test]
    fn mount_prefixes_join() {
        assert_eq!(join_paths("/api", "/users"), "/api/users");
        assert_eq!(join_paths("/api/", "/"), "/api");
        assert_eq!(join_paths("", "/"), "/");
    }

    #[test]
    fn regex_literals_do_not_hide_brackets() {
        let src = "app.get('/a', (req, res) => { if (/[)]/.test(req.url)) { go(); } });\napp.get('/b', other);\n";
        let result = extract(src);
        assert_eq!(result.nodes.len(), 2);
    }

    #[test]
    fn middleware_names() {
        assert!(is_middleware_name("Auth"));
        assert!(is_middleware_name("validateBody"));
        assert!(is_middleware_name("loggingMiddleware"));
        assert!(!is_middleware_name("authors"));
    }
}
