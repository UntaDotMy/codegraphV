//! Cross-tier channels — the web's equivalent of the React Native bridge. Port of
//! `src/resolution/tier-synthesizer.ts`.
//!
//! A web app is two programs that talk over a wire the graph cannot see: the page calls
//! `fetch('/api/users', { method: 'POST' })` and the API's `app.post('/api/users', createUser)`
//! answers; a service puts `'welcome'` on the `email` queue and a `@Process('welcome')` method
//! picks it up; a gateway's `this.server.emit('message')` lands in the component that wrote
//! `socket.on('message', …)`. Each hop is a string on both sides, which is the evidence that lets
//! a synthesizer close it.
//!
//! Three channels, one scan — `http-client`, `queue-job`, `event-bus` — plus a fourth for tests: a
//! Spring / Laravel test's request onto the route it reaches.

use super::common::*;
use crate::ctx::Ctx;
use crate::frameworks::expo_router::{read_string_at, HOLE};
use crate::import_resolver::resolve_import_path;
use crate::meta;
use crate::synth_utils::{enclosing_fn, enclosing_value};
use crate::types::{NodeList, NodeRef};
use codegraph_jsre::{re, Re};
use codegraph_search::{is_generated_file, is_test_path};
use codegraph_types::jscompat::{is_js_space, js_trim, utf16_len, utf16_prefix};
use codegraph_types::{Edge, Language, NodeKind};
use indexmap::IndexMap;
use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

const TIER_CLIENT_TO_SERVER: &str = "client→server";
const TIER_SERVER_TO_CLIENT: &str = "server→client";

const HTTP_VERBS: [&str; 9] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "ALL", "ANY"];

// ── Source reading ────────────────────────────────────────────────────────────

/// Index of the backtick closing the template opening at `open`.
fn template_end(s: &[u8], open: usize) -> usize {
    let mut i = open + 1;
    while i < s.len() {
        let ch = s[i];
        if ch == b'\\' {
            i += 2;
            continue;
        }
        if ch == b'`' {
            return i;
        }
        if ch == b'$' && s.get(i + 1) == Some(&b'{') {
            let mut depth = 0;
            i += 1;
            while i < s.len() {
                if s[i] == b'{' {
                    depth += 1;
                } else if s[i] == b'}' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                } else if s[i] == b'`' {
                    i = template_end(s, i);
                }
                i += 1;
            }
        }
        i += 1;
    }
    s.len()
}

/// Skip the quoted string whose opening quote `q` sits at `i`; returns the index of the closing
/// quote (or the end).
fn skip_quoted(s: &[u8], mut i: usize, q: u8) -> usize {
    i += 1;
    while i < s.len() && s[i] != q {
        if s[i] == b'\\' {
            i += 1;
        }
        i += 1;
    }
    i
}

/// Index of the `)` that closes the `(` at `open`, skipping strings; `None` if unbalanced.
fn close_paren(s: &str, open: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0;
    let mut i = open;
    while i < b.len() {
        let ch = b[i];
        if ch == b'"' || ch == b'\'' {
            i = skip_quoted(b, i, ch) + 1;
            continue;
        }
        if ch == b'`' {
            i = template_end(b, i) + 1;
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

/// The arguments of the call whose `(` is at `open`, split at depth-0 commas.
fn arguments_at(s: &str, open: usize) -> Option<Vec<String>> {
    let close = close_paren(s, open)?;
    let inner = &s[open + 1..close];
    let b = inner.as_bytes();
    let mut out: Vec<&str> = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'"' || c == b'\'' {
            i = skip_quoted(b, i, c) + 1;
            continue;
        }
        if c == b'`' {
            i = template_end(b, i) + 1;
            continue;
        }
        if c == b'(' || c == b'[' || c == b'{' {
            depth += 1;
        } else if c == b')' || c == b']' || c == b'}' {
            depth -= 1;
        } else if c == b',' && depth == 0 {
            out.push(&inner[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    out.push(&inner[start.min(inner.len())..]);
    Some(out.into_iter().enumerate().map(|(i, a)| (i, js_trim(a).to_string())).filter(|(i, a)| !a.is_empty() || *i == 0).map(|(_, a)| a).collect())
}

/// The first string literal in `text`, holes kept; `None` when it opens with anything else.
fn leading_string(text: &str) -> Option<String> {
    let t = js_trim(text);
    let t = re!(r"^\(\s*").replace_first(t, |_| String::new());
    // `new URL('/x', base)` — the path is the first argument of the URL.
    if let Some(url) = re!(r"^new\s+URL\s*\(").captures(&t) {
        let args = arguments_at(&t, url.get(0)?.end() - 1)?;
        return args.first().filter(|a| !a.is_empty()).and_then(|a| leading_string(a));
    }
    match t.as_bytes().first() {
        Some(b'"' | b'\'' | b'`') => read_string_at(&t, 0),
        _ => None,
    }
}

/// `method: 'POST'` inside an options object / config, upper-cased; `None` when absent or computed.
fn method_in(text: &str) -> Option<String> {
    re!(r#"\bmethod\s*:\s*(['"`])([A-Za-z]+)\1"#).captures(text).and_then(|m| m.text(2).map(str::to_uppercase))
}

/// The first string literal anywhere in a decorator's arguments (`'x'`, `{ name: 'x' }`).
fn first_literal(args: &str) -> Option<String> {
    re!(r#"(['"`])([^'"`]+)\1"#).captures(args).and_then(|m| m.text(2).map(String::from))
}

/// Every string literal in a decorator's arguments, for `@OnEvent(['a', 'b'])`.
fn all_literals(args: &str) -> Vec<String> {
    re!(r#"(['"`])([^'"`]+)\1"#, "g").captures_iter(args).filter_map(|m| m.text(2).map(String::from)).collect()
}

/// The name of the method a decorator sits on: skip further stacked decorators and modifiers
/// after the decorator's `)`, then take the identifier before `(`. Returns the name and where it
/// starts.
fn method_name_after(safe: &str, from: usize) -> Option<(String, usize)> {
    let ws = re!(r"\s*", "y");
    let deco = re!(r"@[\w.]+", "y");
    let modifier = re!(r"(?:public|private|protected|async|static|readonly|override)\b", "y");
    let ident = re!(r"([A-Za-z_$][\w$]*)\s*[<(]", "y");
    let mut i = from;
    let eat = |i: &mut usize| {
        if let Some(m) = ws.captures_at(safe, *i) {
            *i = m.get(0).unwrap().end();
        }
    };
    let at = |i: usize| safe.as_bytes().get(i).copied();
    loop {
        eat(&mut i);
        if at(i) != Some(b'@') {
            break;
        }
        let Some(d) = deco.captures_at(safe, i) else { break };
        i = d.get(0).unwrap().end();
        eat(&mut i);
        if at(i) == Some(b'(') {
            let close = close_paren(safe, i)?;
            i = close + 1;
        }
    }
    loop {
        eat(&mut i);
        if let Some(m) = modifier.captures_at(safe, i) {
            let end = m.get(0).unwrap().end();
            if end > i {
                i = end;
                continue;
            }
        }
        break;
    }
    eat(&mut i);
    ident.captures_at(safe, i).map(|m| (m.text(1).unwrap_or("").to_string(), i))
}

struct Deco {
    args: String,
    index: usize,
    end: usize,
}

/// Every `@Name(` decorator in `safe` with its arguments and where it ends.
fn decorators(safe: &str, name: &str) -> Vec<Deco> {
    let mut out = Vec::new();
    let pat = Re::cached(&format!(r"@{name}\s*\("), "g");
    let mut pos = 0;
    while let Some(m) = pat.captures_at(safe, pos) {
        let whole = m.get(0).unwrap();
        let open = whole.end() - 1;
        match close_paren(safe, open) {
            None => pos = whole.end(),
            Some(close) => {
                out.push(Deco { args: safe[open + 1..close].to_string(), index: whole.start(), end: close + 1 });
                pos = close + 1;
            }
        }
    }
    out
}

// ── Per-file facts, read once ─────────────────────────────────────────────────

#[derive(Clone)]
struct Client {
    base_url: Option<String>,
}

struct FileFacts {
    file: String,
    safe: String,
    /// Read on first use: most files that pass a gate hold no site, and never need them.
    nodes: OnceCell<NodeList>,
    newlines: OnceCell<Vec<usize>>,
    /// Lines a framework resolver made a route node on — registrations, never client calls.
    route_lines: OnceCell<HashSet<u32>>,
    /// Local names bound to an HTTP client instance, with their literal base URL when written.
    clients: HashMap<String, Client>,
    /// The module's default export is a client instance.
    default_client: Option<Client>,
    /// Local names bound to a named queue (`new Queue('email')`, `@InjectQueue('email') x`).
    queues: HashMap<String, String>,
    /// The file holds a socket server (a gateway, `io.on('connection')`, `new Server(…)`).
    socket_server: bool,
}

impl FileFacts {
    fn nodes(&self, ctx: &Ctx) -> NodeList {
        self.nodes.get_or_init(|| ctx.get_nodes_in_file(&self.file)).clone()
    }

    fn line_of(&self, idx: usize) -> u32 {
        let nl = self.newlines.get_or_init(|| self.safe.bytes().enumerate().filter(|(_, b)| *b == b'\n').map(|(i, _)| i).collect());
        1 + nl.partition_point(|&p| p < idx) as u32
    }

    /// The 0-based column of an index on its line — where the site reader looks for the call.
    fn column_of(&self, idx: usize) -> u32 {
        column_of(&self.safe, idx)
    }

    fn route_lines(&self, ctx: &Ctx) -> &HashSet<u32> {
        self.route_lines.get_or_init(|| self.nodes(ctx).iter().filter(|n| n.kind == NodeKind::Route).map(|n| n.start_line).collect())
    }
}

/// `idx - (safe.lastIndexOf('\n', idx - 1) + 1)`, in UTF-16 units.
fn column_of(safe: &str, idx: usize) -> u32 {
    let line_start = safe[..idx].rfind('\n').map_or(0, |p| p + 1);
    utf16_len(&safe[line_start..idx]) as u32
}

const CLIENT_FACTORY: &str = r"\b(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=;]*?)?=\s*(?:await\s+)?(?:(?:axios|ky|got|ofetch|\$fetch|wretch|redaxios)\s*\.\s*(?:create|extend)|new\s+Axios|wretch)\s*\(";
const DEFAULT_CLIENT_FACTORY: &str = r"\bexport\s+default\s+(?:(?:axios|ky|got|ofetch|\$fetch|wretch|redaxios)\s*\.\s*(?:create|extend)|new\s+Axios|wretch)\s*\(";
const QUEUE_BINDING: &str = r#"\b(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=;]*?)?=\s*(?:new\s+)?(?:Queue|Bull|BullQueue)\s*(?:<[^>]*>)?\s*\(\s*(['"`])([^'"`]+)\2"#;
const INJECT_QUEUE: &str = r#"@InjectQueue\s*\(\s*(['"`])([^'"`]+)\1\s*\)\s*(?:(?:private|public|protected|readonly)\s+)*([A-Za-z_$][\w$]*)"#;
const SOCKET_SERVER_FILE: &str = r#"@WebSocketGateway\s*\(|@SubscribeMessage\s*\(|@WebSocketServer\s*\(|\bnew\s+(?:Server|SocketIOServer|WebSocketServer|WebSocket\.Server|WSServer)\b|\.on\s*\(\s*['"]connection['"]"#;

fn base_url_in(safe: &str, open: usize) -> Option<String> {
    let args = arguments_at(safe, open);
    let config = args.as_ref().and_then(|a| a.first()).map(String::as_str).unwrap_or("");
    let m = re!(r"\b(?:baseURL|baseUrl|prefixUrl|baseURI)\s*:\s*").captures(config)?;
    read_string_at(config, m.get(0)?.end())
}

fn read_facts(ctx: &Ctx, file: &str) -> Option<Rc<FileFacts>> {
    let content = ctx.read_file(file).filter(|c| !c.is_empty())?;
    let safe = codegraph_text::strip_comments_for_regex(&content, "typescript");
    let mut clients: HashMap<String, Client> = HashMap::new();
    for m in Re::cached(CLIENT_FACTORY, "g").captures_iter(&safe) {
        let open = m.get(0).unwrap().end() - 1;
        clients.insert(m.text(1).unwrap_or("").to_string(), Client { base_url: base_url_in(&safe, open) });
    }
    let default_client =
        Re::cached(DEFAULT_CLIENT_FACTORY, "").captures(&safe).map(|dm| Client { base_url: base_url_in(&safe, dm.get(0).unwrap().end() - 1) });
    let mut queues: HashMap<String, String> = HashMap::new();
    for m in Re::cached(QUEUE_BINDING, "g").captures_iter(&safe) {
        queues.insert(m.text(1).unwrap_or("").to_string(), m.text(3).unwrap_or("").to_string());
    }
    for m in Re::cached(INJECT_QUEUE, "g").captures_iter(&safe) {
        queues.insert(m.text(3).unwrap_or("").to_string(), m.text(2).unwrap_or("").to_string());
    }
    let socket_server = Re::cached(SOCKET_SERVER_FILE, "").is_match(&safe);
    Some(Rc::new(FileFacts {
        file: file.to_string(),
        safe,
        nodes: OnceCell::new(),
        newlines: OnceCell::new(),
        route_lines: OnceCell::new(),
        clients,
        default_client,
        queues,
        socket_server,
    }))
}

type FactsCache = HashMap<String, Option<Rc<FileFacts>>>;

struct ImportedFacts {
    facts: Rc<FileFacts>,
    exported_name: String,
    is_default: bool,
}

/// Facts for the file a local name is imported from, when the import resolves to a project file.
fn imported_facts(ctx: &Ctx, facts: &FileFacts, local_name: &str, cache: &mut FactsCache) -> Option<ImportedFacts> {
    let lang = if facts.file.ends_with('x') { Language::Tsx } else { Language::TypeScript };
    let mappings = ctx.get_import_mappings(&facts.file, lang);
    let im = mappings.iter().find(|i| i.local_name == local_name)?;
    // The mappings name the module as written; the file it is comes from the same resolution the
    // import resolver uses (aliases, extensions, index files).
    let resolved = im.resolved_path.clone().or_else(|| resolve_import_path(&im.source, &facts.file, lang, ctx))?;
    if !cache.contains_key(&resolved) {
        let target = if re!(r"\.(?:[cm]?[jt]sx?)$").is_match(&resolved) { read_facts(ctx, &resolved) } else { None };
        cache.insert(resolved.clone(), target);
    }
    cache.get(&resolved)?.clone().map(|facts| ImportedFacts { facts, exported_name: im.exported_name.clone(), is_default: im.is_default })
}

// ── 1. HTTP client → route ────────────────────────────────────────────────────

/// A type argument between the callee and its `(` — `useSWR<TeamData>('/api/team')`.
const GENERIC: &str = r"(?:<[^()<>]*(?:<[^()<>]*>[^()<>]*)*>)?";
/// Where a receiver chain (`this.api.client`) may begin: not after an earlier identifier-start
/// character of the same word. The second lookbehind is the whole rule; the first is its
/// one-character fast path. (PCRE2 needs it bounded; 200 is past any real identifier.)
const CHAIN_START: &str = r"(?<![A-Za-z_$])(?<![A-Za-z_$][\w$]{1,200})";
/// A dotted receiver chain: `this.api.client`.
const RECEIVER_CHAIN: &str = r"((?:this\s*\.\s*)?[A-Za-z_$][\w$]*(?:\s*\.\s*[A-Za-z_$][\w$]*)*)";

struct HttpRoute {
    node: NodeRef,
    method: String,
    segs: Vec<String>,
}

struct HttpSite {
    func: NodeRef,
    #[allow(dead_code)]
    file: String,
    line: u32,
    column: u32,
    /// The call as written, `fetch` / `api.get` — what the Steps walk must not also draw as an effect.
    callee: String,
    method: String,
    segs: Vec<String>,
    /// The path began with a hole — a base URL — and matches a route by its tail.
    suffix: bool,
    display: String,
}

fn http_routes(ctx: &Ctx) -> Vec<HttpRoute> {
    let mut out = Vec::new();
    for node in ctx.get_nodes_by_kind(NodeKind::Route).iter() {
        let Some(space) = node.name.find(' ').filter(|s| *s > 0) else { continue };
        let method = node.name[..space].to_uppercase();
        if !HTTP_VERBS.contains(&method.as_str()) {
            continue;
        }
        let path = js_trim(&node.name[space + 1..]);
        if !path.starts_with('/') {
            continue;
        }
        out.push(HttpRoute { node: node.clone(), method, segs: path.split('/').filter(|s| !s.is_empty()).map(String::from).collect() });
    }
    out
}

/// How well the client's segments match a route's; `None` when they do not. A literal match beats
/// a parameter's.
fn score_path(client: &[String], route: &[String]) -> Option<i32> {
    let param_seg = re!(r"^(?::|\{|\[|<|\*)|\?$");
    let catch_all = re!(r"^(?:\*|\[\.\.\.|\{\*|:[\w$]+\*$|\{[\w$]+:\*\}|\*[\w$]*$)");
    let mut score = 0;
    let mut i = 0;
    for seg in route {
        if catch_all.is_match(seg) {
            if i >= client.len() {
                return None;
            }
            score += (client.len() - i) as i32;
            i = client.len();
            continue;
        }
        if i >= client.len() {
            return None;
        }
        let c = &client[i];
        // A hole (`${id}`) fills a route's parameter; it never stands in for a literal segment.
        if param_seg.is_match(seg) {
            score += 2;
        } else if c == seg {
            score += 3;
        } else {
            return None;
        }
        i += 1;
    }
    if i == client.len() {
        Some(score)
    } else {
        None
    }
}

fn match_http<'a>(site: &HttpSite, routes: &'a [HttpRoute]) -> Option<&'a HttpRoute> {
    let mut best: Option<&HttpRoute> = None;
    let mut best_score = -1;
    let mut tied = false;
    for r in routes {
        if r.method != "ALL" && r.method != "ANY" && r.method != site.method {
            continue;
        }
        let score = if site.suffix {
            // A base URL hides a prefix the route may spell out (`${API}/users` for
            // `GET /api/users`) — but a one-segment tail names half the routes in an index, and a
            // long hidden prefix is a different API. Two segments of tail at least, two of prefix
            // at most.
            if site.segs.len() < 2 || r.segs.len() < site.segs.len() || r.segs.len() - site.segs.len() > 2 {
                continue;
            }
            score_path(&site.segs, &r.segs[r.segs.len() - site.segs.len()..])
        } else {
            score_path(&site.segs, &r.segs)
        };
        let Some(score) = score else { continue };
        if score > best_score {
            best = Some(r);
            best_score = score;
            tied = false;
        } else if score == best_score {
            tied = true;
        }
    }
    if tied {
        None
    } else {
        best
    }
}

struct ClientPath {
    segs: Vec<String>,
    suffix: bool,
    display: String,
}

fn join_segments<'a>(parts: impl Iterator<Item = &'a str>) -> String {
    format!("/{}", parts.filter(|s| !s.is_empty()).collect::<Vec<_>>().join("/"))
}

/// The path a client call names, as segments, with `${…}` as `*`; `suffix` when a base URL came
/// first. `None` when the path is not literal enough: a relative path with no base, or nothing
/// but holes.
fn client_path(raw: &str, base_url: Option<&str>) -> Option<ClientPath> {
    let mut p: String = raw.to_string();
    if let Some(cut) = p.find(['?', '#']) {
        p.truncate(cut);
    }
    let mut suffix = false;
    let absolute = re!(r"^(?:[a-z][a-z0-9+.-]*:)?//[^/]*(/.*)?$", "i").captures(&p).map(|m| m.text(1).map(String::from).unwrap_or_else(|| "/".to_string()));
    if let Some(a) = absolute {
        p = a;
    } else if !p.starts_with('/') {
        if p.starts_with(HOLE) {
            let rest = p[HOLE.len_utf8()..].to_string();
            if !rest.starts_with('/') {
                return None;
            }
            p = rest;
            suffix = true;
        } else if let Some(base_url) = base_url {
            match client_path(base_url, None) {
                None => {
                    // A base that is itself a hole: match by the tail.
                    if !base_url.contains(HOLE) {
                        return None;
                    }
                    suffix = true;
                    p = format!("/{p}");
                }
                Some(base) => {
                    suffix = base.suffix;
                    p = join_segments(base.segs.iter().map(String::as_str).chain(p.split('/')));
                }
            }
        } else {
            return None;
        }
    } else if let Some(base_url) = base_url {
        // An instance with a literal path base: axios joins `baseURL + url`.
        match client_path(base_url, None) {
            Some(base) if !base.segs.is_empty() => {
                suffix = base.suffix;
                p = join_segments(base.segs.iter().map(String::as_str).chain(p.split('/')));
            }
            None if base_url.contains(HOLE) => suffix = true,
            _ => {}
        }
    }
    let segs: Vec<String> = p.split('/').filter(|s| !s.is_empty()).map(|s| if s.contains(HOLE) { "*".to_string() } else { s.to_string() }).collect();
    if !segs.is_empty() && segs.iter().all(|s| s == "*") {
        return None;
    }
    let display = format!("/{}", segs.iter().map(|s| if s == "*" { "${…}" } else { s.as_str() }).collect::<Vec<_>>().join("/"));
    Some(ClientPath { segs, suffix, display })
}

/// What a member-call receiver is: a client (with its base URL), or nothing.
fn client_for(ctx: &Ctx, facts: &FileFacts, receiver: &str, cache: &mut FactsCache) -> Option<Client> {
    let squashed = re!(r"\s+", "g").replace_all_str(receiver, "");
    let squashed = squashed.strip_prefix("this.").unwrap_or(&squashed);
    let chain: Vec<&str> = squashed.split('.').collect();
    let head = chain[0];
    let last = chain[chain.len() - 1];
    if let Some(local) = facts.clients.get(head) {
        return Some(local.clone());
    }
    if let Some(imported) = imported_facts(ctx, facts, head, cache) {
        let bound = if imported.is_default { imported.facts.default_client.clone() } else { imported.facts.clients.get(&imported.exported_name).cloned() };
        if bound.is_some() {
            return bound;
        }
    }
    let server_names = re!(r"^(?:app|router|route|routes|express|fastify|koa|hono|elysia|apiRouter|v1|v2|r)$");
    if server_names.is_match(last) || server_names.is_match(head) {
        return None;
    }
    if re!(r"^(?:axios|ky|got|superagent|http|https|httpClient|httpService|api|apiClient|client|restClient|request|agent|fetcher|instance|\$api|\$http|\$axios|axiosInstance|Axios|HttpClient|backend|server)$")
        .is_match(last)
    {
        return Some(Client { base_url: None });
    }
    None
}

fn collect_http_sites(ctx: &Ctx, facts: &FileFacts, sites: &mut Vec<HttpSite>, cache: &mut FactsCache) {
    let safe = facts.safe.as_str();
    let bare_call = Re::cached(
        &format!(r"(?:(?:window|globalThis|global)\s*\.\s*)?\b(fetch|\$fetch|ofetch|axios|ky|got|useFetch|useSWR)\s*{GENERIC}\s*\("),
        "g",
    );
    let member_call = Re::cached(
        &format!(
            r"{CHAIN_START}{RECEIVER_CHAIN}\s*\.\s*(get|post|put|patch|delete|head|options|request|\$get|\$post|\$put|\$patch|\$delete)\s*{GENERIC}\s*\("
        ),
        "g",
    );

    let mut add = |index: usize, open: usize, verb: Option<String>, base_url: Option<&str>| {
        let line = facts.line_of(index);
        let callee = {
            let squashed = re!(r"\s+", "g").replace_all_str(&safe[index..open], "");
            re!(r"<.*>$").replace_first(&squashed, |_| String::new())
        };
        if facts.route_lines(ctx).contains(&line) {
            return; // a registration the resolver already read
        }
        let nodes = facts.nodes(ctx);
        let Some(func) = enclosing_fn(&nodes, line) else { return };
        let Some(args) = arguments_at(safe, open) else { return };
        let Some(first_arg) = args.first().filter(|a| !a.is_empty()) else { return };
        let mut first = first_arg.clone();
        let mut method = verb;
        // `axios({ url, method })`, `.request({ url, method })`, `ky(url, { method })`.
        if first.trim_start_matches(is_js_space).starts_with('{') {
            let Some(url) = re!(r"\burl\s*:\s*").captures(&first).and_then(|m| m.get(0)) else { return };
            if method.is_none() {
                method = Some(method_in(&first).unwrap_or_else(|| "GET".to_string()));
            }
            first = first[url.end()..].to_string();
        } else if method.is_none() {
            method = Some(method_in(&args[1..].join(",")).unwrap_or_else(|| "GET".to_string()));
        }
        let Some(literal) = leading_string(&first) else { return };
        let Some(path) = client_path(&literal, base_url) else { return };
        sites.push(HttpSite {
            func,
            file: facts.file.clone(),
            line,
            column: facts.column_of(index),
            callee,
            method: method.unwrap_or_else(|| "GET".to_string()),
            segs: path.segs,
            suffix: path.suffix,
            display: path.display,
        });
    };

    for m in bare_call.captures_iter(safe) {
        let whole = m.get(0).unwrap();
        // `this.fetch(…)` / `repo.fetch(…)` is a project method, not the platform's.
        let before = whole.start().checked_sub(1).and_then(|p| safe.as_bytes().get(p)).copied();
        if before == Some(b'.') && !re!(r"^(?:window|globalThis|global)\s*\.").is_match(whole.as_str()) {
            continue;
        }
        add(whole.start(), whole.end() - 1, None, None);
    }
    for m in member_call.captures_iter(safe) {
        let whole = m.get(0).unwrap();
        let Some(client) = client_for(ctx, facts, m.text(1).unwrap_or(""), cache) else { continue };
        let verb = m.text(2).unwrap_or("").trim_start_matches('$').to_uppercase();
        let verb = if verb == "REQUEST" { None } else { Some(verb) };
        add(whole.start(), whole.end() - 1, verb, client.base_url.as_deref());
    }
}

// ── 2. Queue job → consumer ───────────────────────────────────────────────────

struct QueueProducer {
    func: NodeRef,
    line: u32,
    column: u32,
    callee: String,
    queue: Option<String>,
    job: String,
}

struct QueueConsumer {
    node: NodeRef,
    file: String,
    line: u32,
    queue: Option<String>,
    /// `None`: every job on the queue (`@Process()` with no name, a WorkerHost's `process`, `new Worker`).
    job: Option<String>,
}

/// The queue a receiver is bound to, by its binding in this file or the file it is imported from.
fn queue_for(ctx: &Ctx, facts: &FileFacts, receiver: &str, cache: &mut FactsCache) -> Option<String> {
    let squashed = re!(r"\s+", "g").replace_all_str(receiver, "");
    let squashed = squashed.strip_prefix("this.").unwrap_or(&squashed);
    let head = squashed.split('.').next().unwrap_or("");
    if let Some(own) = facts.queues.get(head).filter(|q| !q.is_empty()) {
        return Some(own.clone());
    }
    if let Some(imported) = imported_facts(ctx, facts, head, cache) {
        if let Some(bound) = imported.facts.queues.get(&imported.exported_name).filter(|q| !q.is_empty()) {
            return Some(bound.clone());
        }
    }
    None
}

/// The function a handler argument names or encloses; `None` when it is neither.
fn handler_node(ctx: &Ctx, facts: &FileFacts, text: &str, line: u32, cache: &mut FactsCache) -> Option<NodeRef> {
    let handler_arg =
        re!(r"^(?:(?:async\s+)?([A-Za-z_$][\w$.]*)\s*(?:[,)]|$)|(?:async\s*)?(?:\(|function\b|[A-Za-z_$][\w$]*\s*=>))");
    let m = handler_arg.captures(text.trim_start_matches(is_js_space))?;
    if let Some(named) = m.text(1).filter(|n| !n.is_empty()) {
        let name = named.rsplit('.').next().unwrap_or(named);
        let candidates: Vec<NodeRef> =
            ctx.get_nodes_by_name(name).iter().filter(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method)).cloned().collect();
        let local: Vec<&NodeRef> = candidates.iter().filter(|n| n.file_path == facts.file).collect();
        if local.len() == 1 {
            return Some(local[0].clone());
        }
        if local.len() > 1 {
            return None;
        }
        if let Some(imported) = imported_facts(ctx, facts, named.split('.').next().unwrap_or(""), cache) {
            let via_import: Vec<&NodeRef> = candidates.iter().filter(|n| n.file_path == imported.facts.file).collect();
            if via_import.len() == 1 {
                return Some(via_import[0].clone());
            }
        }
        return if candidates.len() == 1 { Some(candidates[0].clone()) } else { None };
    }
    let nodes = facts.nodes(ctx);
    enclosing_fn(&nodes, line).or_else(|| enclosing_value(&nodes, line))
}

/// The nearest class declared at or after `index`, as its node.
fn class_after(ctx: &Ctx, facts: &FileFacts, index: usize) -> Option<NodeRef> {
    let hit = re!(r"\bclass\s+([A-Za-z_$][\w$]*)", "g").captures_at(&facts.safe, index)?;
    let line = facts.line_of(hit.get(0)?.start()) as i64;
    let name = hit.text(1)?;
    facts.nodes(ctx).iter().find(|n| n.kind == NodeKind::Class && n.name == name && n.start_line as i64 >= line - 2).cloned()
}

/// The method node a decorator at `end` sits on, inside `cls` when given.
fn decorated_method(ctx: &Ctx, facts: &FileFacts, end: usize, cls: Option<&NodeRef>) -> Option<NodeRef> {
    let (name, index) = method_name_after(&facts.safe, end)?;
    let line = facts.line_of(index) as i64;
    facts
        .nodes(ctx)
        .iter()
        .find(|n| {
            matches!(n.kind, NodeKind::Method | NodeKind::Function)
                && n.name == name
                && n.start_line as i64 >= line - 1
                && n.start_line as i64 <= line + 1
                && cls.is_none_or(|c| n.start_line >= c.start_line && n.end_line <= c.end_line)
        })
        .cloned()
}

fn collect_queue(
    ctx: &Ctx,
    facts: &FileFacts,
    producers: &mut Vec<QueueProducer>,
    consumers: &mut Vec<QueueConsumer>,
    cache: &mut FactsCache,
) {
    let safe = facts.safe.as_str();
    let queue_add = Re::cached(&format!(r#"{CHAIN_START}{RECEIVER_CHAIN}\s*\.\s*add\s*\(\s*(['"`])([^'"`]+)\2"#), "g");
    let queue_shaped = re!(r"queue|jobs?$|worker|bull|flow|producer", "i");
    for m in queue_add.captures_iter(safe) {
        let receiver = m.text(1).unwrap_or("");
        let queue = queue_for(ctx, facts, receiver, cache);
        let squashed = re!(r"\s+", "g").replace_all_str(receiver, "");
        let last = squashed.rsplit('.').next().unwrap_or("");
        if queue.is_none() && !queue_shaped.is_match(last) {
            continue;
        }
        let whole = m.get(0).unwrap();
        let line = facts.line_of(whole.start());
        let nodes = facts.nodes(ctx);
        let Some(func) = enclosing_fn(&nodes, line) else { continue };
        producers.push(QueueProducer {
            func,
            line,
            column: facts.column_of(whole.start()),
            callee: format!("{squashed}.add"),
            queue,
            job: m.text(3).unwrap_or("").to_string(),
        });
    }

    // Nest: `@Processor('email')` on a class; `@Process('welcome')` on its methods, or a
    // WorkerHost's `process(job)`.
    for proc in decorators(safe, "Processor") {
        let queue = first_literal(&proc.args);
        let Some(cls) = class_after(ctx, facts, proc.end) else { continue };
        let mut any = false;
        for job in decorators(safe, "Process") {
            let line = facts.line_of(job.index);
            if line < cls.start_line || line > cls.end_line {
                continue;
            }
            let Some(method) = decorated_method(ctx, facts, job.end, Some(&cls)) else { continue };
            any = true;
            consumers.push(QueueConsumer { node: method, file: facts.file.clone(), line, queue: queue.clone(), job: first_literal(&job.args) });
        }
        if !any {
            let nodes = facts.nodes(ctx);
            let process = nodes.iter().find(|n| n.kind == NodeKind::Method && n.name == "process" && n.start_line >= cls.start_line && n.end_line <= cls.end_line);
            if let Some(process) = process {
                consumers.push(QueueConsumer { node: process.clone(), file: facts.file.clone(), line: process.start_line, queue: queue.clone(), job: None });
            }
        }
    }

    // BullMQ: `new Worker('email', handler)`.
    let new_worker = re!(r#"\bnew\s+Worker\s*(?:<[^>]*>)?\s*\(\s*(['"`])([^'"`]+)\1\s*,\s*"#, "g");
    for m in new_worker.captures_iter(safe) {
        let whole = m.get(0).unwrap();
        let line = facts.line_of(whole.start());
        let rest = utf16_prefix(&safe[whole.end()..], 200);
        let Some(node) = handler_node(ctx, facts, rest, line, cache) else { continue };
        consumers.push(QueueConsumer { node, file: facts.file.clone(), line, queue: m.text(2).map(String::from), job: None });
    }

    // Bull: `queue.process('welcome', handler)` / `queue.process(handler)`.
    let queue_process = Re::cached(&format!(r#"{CHAIN_START}{RECEIVER_CHAIN}\s*\.\s*process\s*\(\s*(?:(['"`])([^'"`]+)\2\s*,\s*)?(?:\d+\s*,\s*)?"#), "g");
    for m in queue_process.captures_iter(safe) {
        let receiver = m.text(1).unwrap_or("");
        let queue = queue_for(ctx, facts, receiver, cache);
        let squashed = re!(r"\s+", "g").replace_all_str(receiver, "");
        let last = squashed.rsplit('.').next().unwrap_or("");
        if queue.is_none() && !queue_shaped.is_match(last) {
            continue;
        }
        let whole = m.get(0).unwrap();
        let line = facts.line_of(whole.start());
        let rest = utf16_prefix(&safe[whole.end()..], 200);
        let Some(node) = handler_node(ctx, facts, rest, line, cache) else { continue };
        consumers.push(QueueConsumer { node, file: facts.file.clone(), line, queue, job: m.text(3).map(String::from) });
    }
}

fn pair_queue(producers: &[QueueProducer], consumers: &[QueueConsumer], edges: &mut Vec<Edge>, seen: &mut HashSet<String>) {
    for p in producers {
        let mut candidates: Vec<&QueueConsumer> = consumers
            .iter()
            .filter(|c| (p.queue.is_none() || c.queue.is_none() || c.queue == p.queue) && (c.job.is_none() || c.job.as_deref() == Some(p.job.as_str())))
            .collect();
        // The most specific pairing wins: the job by name on the named queue, then the job by
        // name, then the queue's default consumer.
        let exact: Vec<&QueueConsumer> =
            candidates.iter().copied().filter(|c| c.job.as_deref() == Some(p.job.as_str()) && c.queue == p.queue && p.queue.is_some()).collect();
        if !exact.is_empty() {
            candidates = exact;
        } else {
            let by_job: Vec<&QueueConsumer> = candidates.iter().copied().filter(|c| c.job.as_deref() == Some(p.job.as_str())).collect();
            if !by_job.is_empty() {
                candidates = by_job;
            } else if p.queue.is_none() {
                continue; // an unnamed queue and no consumer naming the job: a guess
            } else {
                candidates = candidates.into_iter().filter(|c| c.queue == p.queue).collect();
            }
        }
        if candidates.is_empty() || candidates.len() > EVENT_FANOUT_CAP {
            continue;
        }
        for c in candidates {
            if c.node.id == p.func.id {
                continue;
            }
            let key = format!("{}>{}", p.func.id, c.node.id);
            if !seen.insert(key) {
                continue;
            }
            let mut md = meta! { "synthesizedBy" => "queue-job", "channel" => "queue", "callee" => p.callee.as_str(), "event" => p.job.as_str() };
            if let Some(q) = p.queue.as_ref().or(c.queue.as_ref()).filter(|q| !q.is_empty()) {
                md.insert("queue".to_string(), q.as_str().into());
            }
            md.insert("registeredAt".to_string(), format!("{}:{}", c.file, c.line).into());
            let mut e = calls_edge(&p.func.id, &c.node.id, Some(p.line), md);
            e.column = Some(p.column);
            edges.push(e);
        }
    }
}

// ── 3. Events: a bus, and sockets both ways ───────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Shape {
    Bus,
    Socket,
}

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Server,
    Client,
}

struct Dispatch {
    func: NodeRef,
    line: u32,
    column: u32,
    callee: String,
    event: String,
    shape: Shape,
    side: Side,
}

struct Handler {
    node: NodeRef,
    file: String,
    line: u32,
    /// An event name, or an `@OnEvent` glob.
    pattern: String,
    kind: Shape,
    side: Side,
}

fn shape_of(receiver: &str) -> Option<Shape> {
    let stripped = re!(r"\([^()]*\)", "g").replace_all_str(receiver, "");
    let stripped = re!(r"\s+", "g").replace_all_str(&stripped, "");
    let segs: Vec<&str> = stripped.split('.').filter(|s| *s != "this").collect();
    let socket_words = re!(r"^(?:socket|io|ws|wss|client|server|namespace|nsp|conn|connection|gateway|broadcast|to|in|of|except|volatile|local|sockets|socketServer|wsServer|room|channel|pusher|ably|ioClient|socketClient|sock)$");
    let bus_words = re!(r"^(?:eventEmitter|emitter|events|eventBus|bus|dispatcher|pubsub|publisher|eventPublisher|ee|hub|mediator|broker|messageBus|appEvents|domainEvents|eventsService|eventService)$");
    if segs.iter().any(|s| socket_words.is_match(s)) {
        return Some(Shape::Socket);
    }
    if segs.iter().any(|s| bus_words.is_match(s)) {
        return Some(Shape::Bus);
    }
    None
}

/// The transport's own events — every socket emits and handles them; pairing them says nothing.
fn is_generic_event(event: &str) -> bool {
    re!(r"^(?:error|connect|connect_error|connect_failed|connection|disconnect|disconnecting|reconnect|reconnect_attempt|reconnecting|reconnect_error|reconnect_failed|close|open|end|data|ready|drain|finish|pipe|unpipe|listening|timeout|ping|pong|upgrade|newListener|removeListener)$")
        .is_match(event)
}

fn collect_events(ctx: &Ctx, facts: &FileFacts, dispatches: &mut Vec<Dispatch>, handlers: &mut Vec<Handler>, cache: &mut FactsCache) {
    let safe = facts.safe.as_str();
    let side = if facts.socket_server { Side::Server } else { Side::Client };
    // A word character opens these chains, so a word's first character is the only start that can
    // match (same argument as CHAIN_START).
    let emit = re!(r#"(?<![\w$])((?:[\w$]+(?:\([^()]*\))?\s*\.\s*)*[\w$]+)\s*\.\s*(emit|emitAsync)\s*\(\s*(['"`])([^'"`\n]+)\3"#, "g");
    for m in emit.captures_iter(safe) {
        let receiver = m.text(1).unwrap_or("");
        let event = m.text(4).unwrap_or("");
        let Some(shape) = shape_of(receiver) else { continue };
        if is_generic_event(event) {
            continue;
        }
        let whole = m.get(0).unwrap();
        let line = facts.line_of(whole.start());
        let nodes = facts.nodes(ctx);
        let Some(func) = enclosing_fn(&nodes, line) else { continue };
        let squashed = re!(r"\s+", "g").replace_all_str(receiver, "");
        dispatches.push(Dispatch {
            func,
            line,
            column: facts.column_of(whole.start()),
            callee: format!("{squashed}.{}", m.text(2).unwrap_or("")),
            event: event.to_string(),
            shape,
            side,
        });
    }
    for d in decorators(safe, "OnEvent") {
        let Some(method) = decorated_method(ctx, facts, d.end, None) else { continue };
        let patterns: Vec<String> =
            if d.args.trim_start_matches(is_js_space).starts_with('[') { all_literals(&d.args) } else { first_literal(&d.args).into_iter().collect() };
        for pattern in patterns {
            handlers.push(Handler { node: method.clone(), file: facts.file.clone(), line: facts.line_of(d.index), pattern, kind: Shape::Bus, side });
        }
    }
    for d in decorators(safe, "SubscribeMessage") {
        let method = decorated_method(ctx, facts, d.end, None);
        let event = first_literal(&d.args);
        let (Some(method), Some(event)) = (method, event) else { continue };
        handlers.push(Handler { node: method, file: facts.file.clone(), line: facts.line_of(d.index), pattern: event, kind: Shape::Socket, side: Side::Server });
    }
    let socket_on = re!(r#"(?<![\w$])((?:[\w$]+(?:\([^()]*\))?\s*\.\s*)*[\w$]+)\s*\.\s*(?:on|once)\s*\(\s*(['"`])([^'"`\n]+)\2\s*,\s*"#, "g");
    for m in socket_on.captures_iter(safe) {
        let event = m.text(3).unwrap_or("");
        if shape_of(m.text(1).unwrap_or("")) != Some(Shape::Socket) || is_generic_event(event) {
            continue;
        }
        let whole = m.get(0).unwrap();
        let line = facts.line_of(whole.start());
        let rest = utf16_prefix(&safe[whole.end()..], 200);
        let Some(node) = handler_node(ctx, facts, rest, line, cache) else { continue };
        handlers.push(Handler { node, file: facts.file.clone(), line, pattern: event.to_string(), kind: Shape::Socket, side });
    }
}

fn escape_re(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if ".*+?^${}()|[]\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `user.*` matches one segment, `**` any; anything else is exact.
fn event_matches(pattern: &str, event: &str) -> bool {
    if pattern == event {
        return true;
    }
    if !pattern.contains('*') {
        return false;
    }
    let body = pattern.split("**").map(|part| part.split('*').map(escape_re).collect::<Vec<_>>().join("[^.]+")).collect::<Vec<_>>().join(".*");
    Re::cached(&format!("^{body}$"), "").is_match(event)
}

fn shape_key(shape: Shape) -> &'static str {
    match shape {
        Shape::Bus => "bus",
        Shape::Socket => "socket",
    }
}

fn pair_events(dispatches: &[Dispatch], handlers: &[Handler], edges: &mut Vec<Edge>, seen: &mut HashSet<String>) {
    // Fan-out is judged per event name on each side, as the emitter pass does.
    let mut by_event: IndexMap<String, Vec<&Dispatch>> = IndexMap::new();
    for d in dispatches {
        by_event.entry(format!("{}:{}", shape_key(d.shape), d.event)).or_default().push(d);
    }
    for group in by_event.values() {
        if group.len() > EVENT_FANOUT_CAP {
            continue;
        }
        for d in group {
            let mut tier: Option<&str> = None;
            let matched: Vec<&Handler> = if d.shape == Shape::Bus {
                handlers.iter().filter(|h| h.kind == Shape::Bus && event_matches(&h.pattern, &d.event)).collect()
            } else if d.side == Side::Client {
                tier = Some(TIER_CLIENT_TO_SERVER);
                handlers.iter().filter(|h| h.kind == Shape::Socket && h.side == Side::Server && h.pattern == d.event).collect()
            } else {
                tier = Some(TIER_SERVER_TO_CLIENT);
                handlers.iter().filter(|h| h.kind == Shape::Socket && h.side == Side::Client && h.pattern == d.event).collect()
            };
            if matched.is_empty() || matched.len() > EVENT_FANOUT_CAP {
                continue;
            }
            for h in matched {
                if h.node.id == d.func.id {
                    continue;
                }
                let key = format!("{}>{}", d.func.id, h.node.id);
                if !seen.insert(key) {
                    continue;
                }
                let mut md = meta! {
                    "synthesizedBy" => "event-bus", "channel" => if d.shape == Shape::Bus { "event" } else { "socket" },
                    "callee" => d.callee.as_str(), "event" => d.event.as_str()
                };
                if let Some(t) = tier {
                    md.insert("tier".to_string(), t.into());
                }
                md.insert("registeredAt".to_string(), format!("{}:{}", h.file, h.line).into());
                let mut e = calls_edge(&d.func.id, &h.node.id, Some(d.line), md);
                e.column = Some(d.column);
                edges.push(e);
            }
        }
    }
}

// ── The pass ──────────────────────────────────────────────────────────────────

const HTTP_GATE: &str = r"\b(?:fetch|\$fetch|ofetch|axios|ky|got|useFetch|useSWR)\b|\.\s*(?:get|post|put|patch|delete|head|options|request|\$get|\$post)\s*[<(]";
const QUEUE_GATE: &str = r"\.\s*add\s*\(|@Processor\s*\(|\bnew\s+Worker\s*[<(]|\.\s*process\s*\(";
const EVENT_GATE: &str = r"\.\s*(?:emit|emitAsync|on|once)\s*\(|@OnEvent\s*\(|@SubscribeMessage\s*\(";

pub fn has_cross_tier_pattern(content: &str) -> bool {
    Re::cached(HTTP_GATE, "").is_match(content) || Re::cached(QUEUE_GATE, "").is_match(content) || Re::cached(EVENT_GATE, "").is_match(content)
}

pub fn cross_tier_edges(ctx: &Ctx) -> Vec<Edge> {
    let routes = http_routes(ctx);
    let mut http_sites: Vec<HttpSite> = Vec::new();
    let mut producers: Vec<QueueProducer> = Vec::new();
    let mut consumers: Vec<QueueConsumer> = Vec::new();
    let mut dispatches: Vec<Dispatch> = Vec::new();
    let mut handlers: Vec<Handler> = Vec::new();
    let mut cache: FactsCache = HashMap::new();

    let js_file = re!(r"\.(?:[cm]?[jt]sx?)$");
    let (http_gate, queue_gate, event_gate) = (Re::cached(HTTP_GATE, ""), Re::cached(QUEUE_GATE, ""), Re::cached(EVENT_GATE, ""));
    for file in ctx.get_all_files() {
        if !js_file.is_match(&file) || is_test_path(&file) || is_generated_file(&file) {
            continue;
        }
        let Some(content) = ctx.read_file(&file).filter(|c| !c.is_empty()) else { continue };
        let wants_http = !routes.is_empty() && http_gate.is_match(&content);
        let wants_queue = queue_gate.is_match(&content);
        let wants_events = event_gate.is_match(&content);
        if !wants_http && !wants_queue && !wants_events {
            continue;
        }
        if !cache.contains_key(&file) {
            let facts = read_facts(ctx, &file);
            cache.insert(file.clone(), facts);
        }
        let Some(facts) = cache.get(&file).and_then(|f| f.clone()) else { continue };
        if wants_http {
            collect_http_sites(ctx, &facts, &mut http_sites, &mut cache);
        }
        if wants_queue {
            collect_queue(ctx, &facts, &mut producers, &mut consumers, &mut cache);
        }
        if wants_events {
            collect_events(ctx, &facts, &mut dispatches, &mut handlers, &mut cache);
        }
    }

    let mut edges: Vec<Edge> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for site in &http_sites {
        let Some(route) = match_http(site, &routes) else { continue };
        if route.node.id == site.func.id {
            continue;
        }
        let key = format!("{}>{}", site.func.id, route.node.id);
        if !seen.insert(key) {
            continue;
        }
        let mut e = calls_edge(
            &site.func.id,
            &route.node.id,
            Some(site.line),
            meta! {
                "synthesizedBy" => "http-client", "channel" => "http", "callee" => site.callee.as_str(), "tier" => TIER_CLIENT_TO_SERVER,
                "method" => site.method.as_str(), "href" => site.display.as_str(),
                "registeredAt" => format!("{}:{}", route.node.file_path, route.node.start_line),
            },
        );
        e.column = Some(site.column);
        edges.push(e);
    }
    pair_queue(&producers, &consumers, &mut edges, &mut seen);
    pair_events(&dispatches, &handlers, &mut edges, &mut seen);
    edges
}

// ── 4. Test request → route ───────────────────────────────────────────────────
//
// Spring and Laravel tests reach a controller by URL, never by a call: MockMvc's
// `perform(post("/owners/new"))`, WebTestClient's `.get().uri(…)`, TestRestTemplate, RestAssured,
// and Laravel's `$this->postJson('api/me')`. One edge per (test, route), from the test method (the
// file, for a Pest closure) to the route; the route's own edge reaches the handler.

const TEST_VERBS: &str = "get|post|put|patch|delete|head|options";
const TEST_REQUEST_GATE: &str = r"[mM]ockMvc|[wW]ebTestClient\b|[rR]estTemplate\b|\bRestAssured\b|\.\s*when\s*\(\s*\)|\$this\s*->|Pest\\Laravel";

pub fn has_test_request_pattern(file_path: &str, content: &str) -> bool {
    re!(r"\.(?:java|kt|kts|php)$").is_match(file_path) && is_test_path(file_path) && Re::cached(TEST_REQUEST_GATE, "").is_match(content)
}

struct TestSite {
    from: NodeRef,
    line: u32,
    column: u32,
    callee: String,
    method: String,
    segs: Vec<String>,
    display: String,
}

#[derive(Clone, Copy, PartialEq)]
enum RequestLang {
    Java,
    Kotlin,
    Php,
}

fn byte_at(s: &str, i: usize) -> Option<u8> {
    s.as_bytes().get(i).copied()
}

fn is_word_byte(b: Option<u8>) -> bool {
    b.is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// The char starting at byte `i` and its length.
fn char_at(s: &str, i: usize) -> Option<(char, usize)> {
    s.get(i..)?.chars().next().map(|c| (c, c.len_utf8()))
}

fn skip_whitespace(s: &str, mut i: usize) -> usize {
    while let Some((c, len)) = char_at(s, i) {
        if !is_js_space(c) {
            break;
        }
        i += len;
    }
    i
}

/// Index of the `}` closing the `{` at `open` (or the end of `s`).
fn close_brace(s: &str, open: usize) -> usize {
    let mut depth = 0;
    for (i, b) in s.bytes().enumerate().skip(open) {
        if b == b'{' {
            depth += 1;
        } else if b == b'}' {
            depth -= 1;
            if depth == 0 {
                return i;
            }
        }
    }
    s.len().saturating_sub(1)
}

/// End of a concatenated operand starting at `i`: the next top-level operator, `,` or `)`.
fn operand_end(s: &str, mut i: usize, concat: u8) -> usize {
    let b = s.as_bytes();
    let mut depth = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'"' || c == b'\'' {
            i += 1;
            while i < b.len() && b[i] != c {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if c == b'(' || c == b'[' {
            depth += 1;
        } else if c == b')' || c == b']' {
            if depth == 0 {
                return i;
            }
            depth -= 1;
        } else if depth == 0 && (c == b',' || c == b';' || (c == concat && !(concat == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit)))) {
            return i;
        }
        // `$user->id`: an arrow is part of the operand, not a concatenation.
        if c == b'-' && b.get(i + 1) == Some(&b'>') {
            i += 1;
        }
        i += 1;
    }
    i
}

/// The string a request's path argument builds, starting at `i`: literals verbatim, each computed
/// piece (a concatenated operand, an interpolation, a Spring `{var}` template) as one HOLE. `None`
/// unless it starts with a literal — `route('songs.index')` or `$url` names no path this pass can
/// read.
fn read_request_path(s: &str, mut i: usize, lang: RequestLang) -> Option<String> {
    let concat = if lang == RequestLang::Php { b'.' } else { b'+' };
    let b = s.as_bytes();
    let mut out = String::new();
    let mut literal = false;
    loop {
        i = skip_whitespace(s, i);
        let ch = byte_at(s, i);
        if ch == Some(b'"') || ch == Some(b'\'') {
            let q = ch.unwrap();
            let interpolates = q == b'"' && lang != RequestLang::Java;
            i += 1;
            while i < b.len() && b[i] != q {
                let (c, len) = char_at(s, i)?;
                if c == '\\' {
                    i += 1;
                    if let Some((next, nlen)) = char_at(s, i) {
                        out.push(next);
                        i += nlen;
                    }
                    continue;
                }
                if interpolates && c == '$' && matches!(byte_at(s, i + 1), Some(b'{')) | is_word_byte(byte_at(s, i + 1)) {
                    // `${expr}`, `$name`, PHP's `$user->id` / `$row['k']`
                    if byte_at(s, i + 1) == Some(b'{') {
                        i = close_brace(s, i + 1);
                    } else {
                        while is_word_byte(byte_at(s, i + 1)) {
                            i += 1;
                        }
                        if lang == RequestLang::Php {
                            while s[i + 1..].starts_with("->") && is_word_byte(byte_at(s, i + 3)) {
                                i += 2;
                                while is_word_byte(byte_at(s, i + 1)) {
                                    i += 1;
                                }
                            }
                        }
                    }
                    out.push(HOLE);
                    i += 1;
                    continue;
                }
                if interpolates && lang == RequestLang::Php && c == '{' && byte_at(s, i + 1) == Some(b'$') {
                    i = close_brace(s, i);
                    out.push(HOLE);
                    i += 1;
                    continue;
                }
                out.push(c);
                i += len;
            }
            if i >= b.len() {
                return None;
            }
            i += 1;
            literal = true;
        } else {
            if !literal {
                return None;
            }
            let end = operand_end(s, i, concat);
            if end == i {
                break;
            }
            out.push(HOLE);
            i = end;
        }
        i = skip_whitespace(s, i);
        if byte_at(s, i) == Some(concat) && !(concat == b'.' && byte_at(s, i + 1).is_some_and(|d| d.is_ascii_digit())) {
            i += 1;
            continue;
        }
        break;
    }
    if !literal {
        return None;
    }
    // Spring's URI templates: `get("/owners/{ownerId}", id)`.
    if lang == RequestLang::Php {
        Some(out)
    } else {
        Some(re!(r"\{[^{}/]*\}", "g").replace_all(&out, |_| Some(HOLE.to_string())))
    }
}

struct TestPath {
    segs: Vec<String>,
    display: String,
}

/// A test URL as route segments: app-relative, holes as `*`; `None` when nothing literal is left.
fn test_request_path(raw: &str) -> Option<TestPath> {
    let mut p = raw.to_string();
    if let Some(cut) = p.find(['?', '#']) {
        p.truncate(cut);
    }
    if let Some(a) = re!(r"^(?:[a-z][a-z0-9+.-]*:)?//[^/]*(/.*)?$", "i").captures(&p).map(|m| m.text(1).map(String::from).unwrap_or_else(|| "/".to_string())) {
        p = a;
    }
    let segs: Vec<String> = p.split('/').filter(|s| !s.is_empty()).map(|s| if s.contains(HOLE) { "*".to_string() } else { s.to_string() }).collect();
    if !segs.iter().any(|s| s != "*") && !segs.is_empty() {
        return None;
    }
    let display = format!("/{}", segs.iter().map(|s| if s == "*" { "${…}" } else { s.as_str() }).collect::<Vec<_>>().join("/"));
    Some(TestPath { segs, display })
}

/// Whether `name` is a request helper in the project's tests: a PHP method defined in a test file
/// whose body makes a Laravel request, directly or through one more `$this->helper(…)`. The name
/// alone never decides it.
fn is_request_helper(ctx: &Ctx, name: &str, memo: &mut HashMap<String, bool>, depth: u32) -> bool {
    if let Some(known) = memo.get(name) {
        return *known;
    }
    memo.insert(name.to_string(), false); // cycle guard
    let mut ok = false;
    let request_in_body = Re::cached(&format!(r"->\s*(?:(?:{TEST_VERBS})(?:Json)?|json|call)\s*\("), "");
    for n in ctx.get_nodes_by_name(name).iter() {
        if n.kind != NodeKind::Method || n.language != Language::Php || !is_test_path(&n.file_path) {
            continue;
        }
        let Some(lines) = ctx.get_file_lines(&n.file_path) else { continue };
        let from = (n.start_line as usize).saturating_sub(1).min(lines.len());
        let to = (n.end_line as usize).min(lines.len()).max(from);
        let body = lines[from..to].join("\n");
        if request_in_body.is_match(&body) {
            ok = true;
        } else if depth == 0 {
            for call in re!(r"\$this\s*->\s*(\w+)\s*\(", "g").captures_iter(&body) {
                let callee = call.text(1).unwrap_or("");
                if callee != name && is_request_helper(ctx, callee, memo, depth + 1) {
                    ok = true;
                    break;
                }
            }
        }
        if ok {
            break;
        }
    }
    // A depth-limited "no" is not final; only remember it at the top level.
    if ok || depth == 0 {
        memo.insert(name.to_string(), ok);
    } else {
        memo.remove(name);
    }
    ok
}

/// Whether the `->` at `arrow` ends a chain rooted at `$this` that only CALLS methods —
/// `$this->actingAs($u)->withHeaders([…])->` — so `$this->app->get('config')` (a container lookup
/// through a property) is never a request.
fn this_rooted_calls(s: &str, arrow: usize) -> bool {
    let mut from = arrow;
    for _ in 0..4 {
        let Some(root) = s[..from.min(s.len())].rfind("$this") else { return false };
        let mut i = root + "$this".len();
        loop {
            i = skip_whitespace(s, i);
            if i == arrow {
                return true;
            }
            if !s[i..].starts_with("->") {
                break;
            }
            i += 2;
            i = skip_whitespace(s, i);
            let ident_len = s[i..].bytes().take(64).take_while(|b| b.is_ascii_alphanumeric() || *b == b'_').count();
            if ident_len == 0 {
                break;
            }
            i += ident_len;
            i = skip_whitespace(s, i);
            if byte_at(s, i) != Some(b'(') {
                break;
            }
            let Some(close) = close_paren(s, i) else { break };
            i = close + 1;
        }
        from = root;
    }
    false
}

fn collect_test_requests(ctx: &Ctx, file: &str, sites: &mut Vec<TestSite>, helpers: &mut HashMap<String, bool>) {
    let Some(content) = ctx.read_file(file).filter(|c| !c.is_empty()) else { return };
    if !Re::cached(TEST_REQUEST_GATE, "").is_match(&content) {
        return;
    }
    let lang = if file.ends_with(".php") {
        RequestLang::Php
    } else if re!(r"\.kts?$").is_match(file) {
        RequestLang::Kotlin
    } else {
        RequestLang::Java
    };
    let safe = codegraph_text::strip_comments_for_regex(&content, if lang == RequestLang::Php { "php" } else { "java" });
    let newlines: Vec<usize> = safe.bytes().enumerate().filter(|(_, b)| *b == b'\n').map(|(i, _)| i).collect();
    let line_of = |idx: usize| 1 + newlines.partition_point(|&p| p < idx) as u32;
    let mut nodes: Option<NodeList> = None;
    let mut add = |index: usize, arg_at: usize, method: &str, callee: String| {
        let Some(raw) = read_request_path(&safe, arg_at, lang) else { return };
        let Some(path) = test_request_path(&raw) else { return };
        let line = line_of(index);
        let nodes = nodes.get_or_insert_with(|| ctx.get_nodes_in_file(file));
        let Some(from) = enclosing_fn(nodes, line).or_else(|| nodes.iter().find(|n| n.kind == NodeKind::File).cloned()) else { return };
        sites.push(TestSite { from, line, column: column_of(&safe, index), callee, method: method.to_uppercase(), segs: path.segs, display: path.display });
    };

    if lang == RequestLang::Php {
        let laravel_request = Re::cached(&format!(r"->\s*({TEST_VERBS})(Json)?\s*\("), "g");
        for m in laravel_request.captures_iter(&safe) {
            let whole = m.get(0).unwrap();
            if this_rooted_calls(&safe, whole.start()) {
                add(whole.start(), whole.end(), m.text(1).unwrap_or(""), format!("$this->{}{}", m.text(1).unwrap_or(""), m.text(2).unwrap_or("")));
            }
        }
        for m in re!(r#"->\s*(?:json|call)\s*\(\s*(['"])([A-Za-z]+)\1\s*,"#, "g").captures_iter(&safe) {
            let whole = m.get(0).unwrap();
            if this_rooted_calls(&safe, whole.start()) {
                add(whole.start(), whole.end(), m.text(2).unwrap_or(""), "$this->json".to_string());
            }
        }
        // A project's own request helper, named for its verb: koel's `getAs($url, $user)`.
        let helper = Re::cached(&format!(r"->\s*(({TEST_VERBS})[A-Z]\w*)\s*\("), "g");
        let builtin_json = Re::cached(&format!(r"^(?:{TEST_VERBS})Json$"), "");
        for m in helper.captures_iter(&safe) {
            let name = m.text(1).unwrap_or("");
            if builtin_json.is_match(name) {
                continue; // getJson / postJson, read above
            }
            let whole = m.get(0).unwrap();
            if this_rooted_calls(&safe, whole.start()) && is_request_helper(ctx, name, helpers, 0) {
                add(whole.start(), whole.end(), m.text(2).unwrap_or(""), format!("$this->{name}"));
            }
        }
        if re!(r"Pest\\Laravel").is_match(&safe) {
            let pest = Re::cached(&format!(r"(?<![\w$>:\\])({TEST_VERBS})(Json)?\s*\("), "g");
            for m in pest.captures_iter(&safe) {
                let whole = m.get(0).unwrap();
                add(whole.start(), whole.end(), m.text(1).unwrap_or(""), format!("{}{}", m.text(1).unwrap_or(""), m.text(2).unwrap_or("")));
            }
        }
        return;
    }
    if re!(r"\bMockMvcRequestBuilders\b").is_match(&safe) {
        // A bare `get("/x")` is MockMvc's only where the file imports its builders.
        let builder = Re::cached(&format!(r"(?<![\w$.])(?:MockMvcRequestBuilders\s*\.\s*)?({TEST_VERBS}|multipart)\s*\("), "g");
        for m in builder.captures_iter(&safe) {
            let whole = m.get(0).unwrap();
            let verb = m.text(1).unwrap_or("");
            add(whole.start(), whole.end(), if verb == "multipart" { "post" } else { verb }, verb.to_string());
        }
        for m in re!(r"(?<![\w$.])(?:MockMvcRequestBuilders\s*\.\s*)?request\s*\(\s*HttpMethod\s*\.\s*([A-Z]+)\s*,", "g").captures_iter(&safe) {
            let whole = m.get(0).unwrap();
            add(whole.start(), whole.end(), m.text(1).unwrap_or(""), "request".to_string());
        }
    }
    for m in Re::cached(&format!(r"\b\w*[mM]ockMvc\s*\.\s*({TEST_VERBS})\s*\("), "g").captures_iter(&safe) {
        let whole = m.get(0).unwrap();
        add(whole.start(), whole.end(), m.text(1).unwrap_or(""), format!("mockMvc.{}", m.text(1).unwrap_or("")));
    }
    for m in Re::cached(&format!(r"\.\s*({TEST_VERBS})\s*\(\s*\)\s*\.\s*uri\s*\("), "g").captures_iter(&safe) {
        let whole = m.get(0).unwrap();
        add(whole.start(), whole.end(), m.text(1).unwrap_or(""), format!("{}().uri", m.text(1).unwrap_or("")));
    }
    for m in Re::cached(&format!(r"(?:\.\s*when\s*\(\s*\)\s*|\bRestAssured\s*)\.\s*({TEST_VERBS})\s*\("), "g").captures_iter(&safe) {
        let whole = m.get(0).unwrap();
        add(whole.start(), whole.end(), m.text(1).unwrap_or(""), m.text(1).unwrap_or("").to_string());
    }
    let rest_template = re!(r"\b\w*[Rr]estTemplate\s*\.\s*(getForEntity|getForObject|postForEntity|postForObject|postForLocation|put|delete|patchForObject|exchange)\s*\(", "g");
    for m in rest_template.captures_iter(&safe) {
        let whole = m.get(0).unwrap();
        let name = m.text(1).unwrap_or("");
        let mut method: Option<String> = if name.starts_with("get") {
            Some("GET".into())
        } else if name.starts_with("post") {
            Some("POST".into())
        } else if name == "put" {
            Some("PUT".into())
        } else if name == "delete" {
            Some("DELETE".into())
        } else if name.starts_with("patch") {
            Some("PATCH".into())
        } else {
            None
        };
        if name == "exchange" {
            let args = arguments_at(&safe, whole.end() - 1);
            method = args
                .as_ref()
                .and_then(|a| a.get(1))
                .and_then(|a| re!(r"HttpMethod\s*\.\s*([A-Z]+)").captures(a).and_then(|c| c.text(1).map(String::from)));
        }
        if let Some(method) = method {
            add(whole.start(), whole.end(), &method, format!("restTemplate.{name}"));
        }
    }
}

/// Edges from a test's request to the route it reaches.
pub fn test_request_edges(ctx: &Ctx) -> Vec<Edge> {
    let routes = http_routes(ctx);
    if routes.is_empty() {
        return Vec::new();
    }
    let mut sites: Vec<TestSite> = Vec::new();
    let mut helpers: HashMap<String, bool> = HashMap::new();
    let test_file = re!(r"\.(?:java|kt|kts|php)$");
    for file in ctx.get_all_files() {
        if !test_file.is_match(&file) || !is_test_path(&file) {
            continue;
        }
        collect_test_requests(ctx, &file, &mut sites, &mut helpers);
    }
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for site in sites {
        let probe = HttpSite {
            func: site.from.clone(),
            file: site.from.file_path.clone(),
            line: site.line,
            column: site.column,
            callee: site.callee.clone(),
            method: site.method.clone(),
            segs: site.segs.clone(),
            suffix: false,
            display: site.display.clone(),
        };
        let Some(route) = match_http(&probe, &routes) else { continue };
        if !seen.insert(format!("{}>{}", site.from.id, route.node.id)) {
            continue;
        }
        let mut e = calls_edge(
            &site.from.id,
            &route.node.id,
            Some(site.line),
            meta! {
                "synthesizedBy" => "test-request", "channel" => "http", "callee" => site.callee.as_str(), "method" => site.method.as_str(),
                "href" => site.display.as_str(), "registeredAt" => format!("{}:{}", route.node.file_path, route.node.start_line),
            },
        );
        e.column = Some(site.column);
        edges.push(e);
    }
    edges
}
