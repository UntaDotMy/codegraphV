//! Tauri IPC — the hop between a desktop app's web frontend and its Rust core. Port of
//! `src/resolution/tauri-synthesizer.ts` (upstream 2008e2f3).
//!
//! A Tauri app is two programs in one window: the page calls `invoke('save_file', { path })` and
//! the Rust `#[tauri::command] fn save_file` answers, while the core sends `app.emit("saved", …)`
//! back to a page that wrote `listen('saved', …)`. Neither side names the other; the string is the
//! evidence, so this pass pairs the literals and draws the hop the runtime makes.
//!
//! - **`tauri-command`** — the function a frontend call is written in → the Rust function that
//!   answers it. A command answers only if a handler list registers it (`generate_handler![…]`,
//!   tauri-specta's `collect_commands![…]`). tauri-specta's generated client needs nothing of its
//!   own when it is indexed (`commands.saveFile()` reaches `bindings.ts`, whose body is an aliased
//!   `invoke`); when it is not, the call is read by its camelCase name.
//! - **`tauri-event`** — an `emit` → the listeners of that event, in either language and between
//!   windows (`calls`), and a listener → the Rust struct deriving tauri-specta's `Event`
//!   (`references`).
//!
//! The command table is per app: a monorepo's two `greet`s stay apart, and a tie is nothing.
//! Offsets are bytes into texts kept byte-aligned with the source.

use super::common::*;
use crate::ctx::Ctx;
use crate::frameworks::expo_router::{read_string_at, HOLE};
use crate::import_resolver::resolve_import_path;
use crate::meta;
use crate::name_matcher::js_scope::js_code_binds_name;
use crate::synth_utils::{enclosing_fn, enclosing_value, is_fn_kind};
use crate::types::{NodeList, NodeRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_search::is_test_path;
use codegraph_text::{blank_string_contents, strip_comments_for_regex, utf16_to_byte_offset};
use codegraph_types::{Edge, EdgeKind, Language, NodeKind};
use indexmap::{IndexMap, IndexSet};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const FRONTEND_FILE: &str = r"\.(?:[cm]?[jt]sx?|vue|svelte)$";
const FRONTEND_GATE: &str = r"invoke|listen|once|\bemit|\bcommands\b|\bevents\b|__TAURI";
const RUST_GATE: &str = r"\btauri|generate_handler|collect_commands|\.\s*(?:emit|listen|once)";

/// One command's platform variants: more than this and the name is not a command's.
const MAX_VARIANTS: usize = 6;
/// Above this many emitters or listeners, an event is too generic to read as a flow.
const TAURI_EVENT_FANOUT_CAP: usize = 16;

const TIER_CLIENT_TO_SERVER: &str = "client→server";
const TIER_SERVER_TO_CLIENT: &str = "server→client";

const API_ROOT: &str = "@tauri-apps/api";
const EVENT_MODULE: &str = "@tauri-apps/api/event";

fn is_core_module(source: &str) -> bool {
    matches!(source, "@tauri-apps/api/core" | "@tauri-apps/api/tauri" | API_ROOT)
}

/// Modules whose handles (`getCurrentWindow()`, `appWindow`, …) have their own `listen` / `once` / `emit`.
fn is_handle_module(source: &str) -> bool {
    matches!(source, "@tauri-apps/api/window" | "@tauri-apps/api/webviewWindow" | "@tauri-apps/api/webview")
}

// ── Reading source ────────────────────────────────────────────────────────────

/// `rewritten` (a per-UTF-16-unit rewrite of `orig`: each unit kept or blanked) with every blanked
/// character widened back to its UTF-8 length, so byte offsets agree with `orig`.
fn align(orig: &str, rewritten: String) -> String {
    if orig.len() == rewritten.len() {
        return rewritten;
    }
    let mut out = String::with_capacity(orig.len());
    let mut it = rewritten.chars().peekable();
    for c in orig.chars() {
        if it.peek() == Some(&c) {
            it.next();
            out.push(c);
            continue;
        }
        for _ in 0..c.len_utf16() {
            it.next();
        }
        if c == '\n' {
            out.push('\n');
        } else {
            out.extend(std::iter::repeat_n(' ', c.len_utf8()));
        }
    }
    out
}

/// `&s[a..b]` with both ends moved forward onto a character boundary and clamped.
fn cut(s: &str, a: usize, b: usize) -> &str {
    let ceil = |mut i: usize| {
        i = i.min(s.len());
        while !s.is_char_boundary(i) {
            i += 1;
        }
        i
    };
    let a = ceil(a);
    let b = ceil(b).max(a);
    &s[a..b]
}

struct Lines {
    starts: Vec<usize>,
}

impl Lines {
    fn new(s: &str) -> Lines {
        let mut starts = vec![0];
        starts.extend(s.bytes().enumerate().filter(|(_, b)| *b == b'\n').map(|(i, _)| i + 1));
        Lines { starts }
    }

    /// The 1-based line of byte offset `i`.
    fn line_at(&self, i: usize) -> u32 {
        self.starts.partition_point(|&s| s <= i) as u32
    }

    /// The byte offset a 1-based line starts at.
    fn offset_of_line(&self, line: u32) -> usize {
        let l = (line as usize).clamp(1, self.starts.len());
        self.starts[l - 1]
    }

    /// The byte offset of a (line, UTF-16 column) position in `text`.
    fn offset(&self, text: &str, line: u32, column: u32) -> usize {
        let start = self.offset_of_line(line).min(text.len());
        let end = text[start..].find('\n').map_or(text.len(), |p| start + p);
        start + utf16_to_byte_offset(&text[start..end], column as usize)
    }
}

/// The UTF-16 column of byte offset `index`.
fn column_at(text: &str, index: usize) -> u32 {
    let index = index.min(text.len());
    let start = text[..index].rfind('\n').map_or(0, |p| p + 1);
    text[start..index].encode_utf16().count() as u32
}

fn closer(c: u8) -> Option<u8> {
    match c {
        b'(' => Some(b')'),
        b'[' => Some(b']'),
        b'{' => Some(b'}'),
        _ => None,
    }
}

/// Index of the bracket closing the one at `open`, over text with its strings blanked.
fn close_bracket(code: &[u8], open: usize) -> Option<usize> {
    let mut stack = vec![closer(*code.get(open)?)?];
    for (i, &c) in code.iter().enumerate().skip(open + 1) {
        if let Some(cl) = closer(c) {
            stack.push(cl);
        } else if Some(&c) == stack.last() {
            stack.pop();
            if stack.is_empty() {
                return Some(i);
            }
        }
    }
    None
}

/// Index of the bracket opening the one at `close`, over text with its strings blanked.
fn open_bracket(code: &[u8], close: usize) -> Option<usize> {
    let cl = code[close];
    let op = match cl {
        b')' => b'(',
        b']' => b'[',
        _ => b'{',
    };
    let mut depth = 0i32;
    for i in (0..=close).rev() {
        if code[i] == cl {
            depth += 1;
        } else if code[i] == op {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// The `[start, end)` spans of the arguments of the call whose `(` is at `open`.
fn argument_spans(code: &[u8], open: usize) -> Vec<(usize, usize)> {
    let Some(close) = close_bracket(code, open) else { return Vec::new() };
    let mut spans = Vec::new();
    let mut start = open + 1;
    let mut i = open + 1;
    while i < close {
        let c = code[i];
        if closer(c).is_some() {
            match close_bracket(code, i) {
                Some(end) => i = end,
                None => return spans,
            }
        } else if c == b',' {
            spans.push((start, i));
            start = i + 1;
        }
        i += 1;
    }
    if code[start..close].iter().any(|b| !b.is_ascii_whitespace()) {
        spans.push((start, close));
    }
    spans
}

fn skip_space(s: &[u8], mut i: usize) -> usize {
    while i < s.len() && s[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Past the `<…>` type arguments at `at`, an arrow's `>` not closing them; `at` when there are none.
fn skip_type_arguments(code: &[u8], at: usize) -> usize {
    if code.get(at) != Some(&b'<') {
        return at;
    }
    let mut depth = 0i32;
    let limit = code.len().min(at + 40_000);
    for i in at..limit {
        if code[i] == b'<' {
            depth += 1;
        } else if code[i] == b'>' && code[i - 1] != b'=' {
            depth -= 1;
            if depth == 0 {
                return skip_space(code, i + 1);
            }
        }
    }
    at
}

fn span_text(s: &str, span: (usize, usize)) -> &str {
    cut(s, span.0, span.1).trim()
}

// ── Names ─────────────────────────────────────────────────────────────────────

/// The words of a Rust name, the way `heck` (tauri-specta's casing) splits them.
fn words(name: &str) -> Vec<String> {
    name.split('_')
        .flat_map(|part| {
            re!(r"[A-Z]+(?![a-z])|[A-Z]?[a-z0-9]+|[0-9]+", "g")
                .captures_iter(part)
                .map(|m| m.get(0).unwrap().as_str().to_string())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// `save_file` / `SaveFile` → `saveFile`.
fn to_camel(name: &str) -> String {
    words(name)
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let lower = w.to_lowercase();
            if i == 0 {
                lower
            } else {
                let mut c = lower.chars();
                c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
            }
        })
        .collect()
}

/// `SaveFile` → `save-file`: an event struct's name on the wire.
fn to_kebab(name: &str) -> String {
    words(name).iter().map(|w| w.to_lowercase()).collect::<Vec<_>>().join("-")
}

// ── The Rust side ─────────────────────────────────────────────────────────────

#[derive(Clone)]
struct Command {
    /// One function, or one per platform.
    nodes: Vec<NodeRef>,
    /// Where it is wired in: `file:line`.
    registered_at: String,
}

/// A key with several answers is `None`: a tie links nothing.
type CommandTable = IndexMap<String, Option<Command>>;
type Constants = HashMap<String, Option<String>>;

struct TauriApp {
    /// The directory the app's code sits under; '' at the repository root.
    root: String,
    commands: CommandTable,
    /// Every declared command counts: a macro builds the handler list, so it can't be read.
    open_list: Option<String>,
}

#[derive(Clone)]
struct EventStruct {
    node: NodeRef,
    /// The name on the wire.
    event: String,
    scope: Option<String>,
}

#[derive(Clone)]
struct Endpoint {
    node: NodeRef,
    event: String,
    file: String,
    line: u32,
    column: u32,
    rust: bool,
    scope: Option<String>,
}

struct RustFile {
    file: String,
    /// Comments blanked, strings kept.
    text: String,
    /// Comments and string contents blanked.
    code: String,
    nodes: NodeList,
    lines: Lines,
}

struct ListEntry {
    /// The path as written, with no turbofish: `cmd::save_file`.
    segments: Vec<String>,
    line: u32,
}

struct HandlerList {
    file: String,
    line: u32,
    entries: Vec<ListEntry>,
    /// A `$…` metavariable in the list: the names are the macro's to fill in.
    opaque: bool,
    /// The plugin these commands are registered under, `plugin:<name>|<command>`.
    plugin: Option<String>,
}

const HANDLER_LIST: &str = r"\b(?:generate_handler|collect_commands)\s*!\s*[\[({]";
const PLUGIN_HEAD: &str = r#"\bBuilder\s*(?:::\s*<[^;()]*>\s*)?::\s*(?:new\s*\(\s*(?:"([^"]+)"|([A-Z][A-Z0-9_]*))|default\s*\()|\.\s*plugin_name\s*\(\s*(?:"([^"]+)"|([A-Z][A-Z0-9_]*))"#;
/// Rust's own attributes and the usual tracing ones: none of them makes a function a command.
const PLAIN_ATTRIBUTE: &str = r"^(?:cfg|cfg_attr|doc|allow|expect|warn|deny|forbid|inline|cold|must_use|deprecated|track_caller|test|ignore|no_mangle|rustfmt|clippy|instrument|tracing|specta|async_trait)\b";

fn read_handler_list(rust: &RustFile, constants: &Constants, at: usize, open: usize) -> Option<HandlerList> {
    let code = rust.code.as_bytes();
    close_bracket(code, open)?;
    let mut entries = Vec::new();
    let mut opaque = false;
    for (start, end) in argument_spans(code, open) {
        // A `#[cfg(…)]` on an entry is the entry's business, not part of its name.
        let stripped = re!(r"#\s*\[[^\]]*\]", "g").replace_all_str(cut(&rust.code, start, end), " ");
        let item = stripped.trim();
        if item.contains('$') {
            opaque = true;
            continue;
        }
        let path: String = re!(r"::\s*<[\s\S]*>$").replace_first(item, |_| String::new()).split_whitespace().collect();
        if !re!(r"^[A-Za-z_]\w*(?:::[A-Za-z_]\w*)*$").is_match(&path) {
            continue;
        }
        let segments: Vec<String> = path.split("::").map(str::to_string).collect();
        let last = segments.last().unwrap();
        let from = end.saturating_sub(item.len());
        let named = cut(&rust.code, from, rust.code.len()).find(last.as_str()).map(|p| from + p);
        let line_from = match named {
            Some(n) if n >= start && n < end => n,
            _ => start,
        };
        entries.push(ListEntry { line: rust.lines.line_at(line_from), segments });
    }
    Some(HandlerList { file: rust.file.clone(), line: rust.lines.line_at(at), entries, opaque, plugin: plugin_of(rust, constants, at) })
}

/// The plugin a handler list sits in: the nearest builder before it, when it names one.
fn plugin_of(rust: &RustFile, constants: &Constants, at: usize) -> Option<String> {
    let mut plugin = None;
    for m in Re::cached(PLUGIN_HEAD, "g").captures_iter(&rust.text) {
        if m.get(0).unwrap().start() >= at {
            break;
        }
        let literal = m.text(1).or(m.text(3));
        let constant = m.text(2).or(m.text(4));
        plugin = literal.map(str::to_string).or_else(|| constant.and_then(|c| constants.get(c).cloned().flatten()));
    }
    plugin
}

/// The attributes written on and directly above the line a function starts on.
fn attributes_above(lines: &[String], start_line: u32) -> Vec<String> {
    let start = start_line as usize;
    let mut out = vec![lines.get(start.wrapping_sub(1)).map_or("", |l| l.trim()).to_string()];
    let mut i = start as isize - 2;
    while i >= 0 {
        let line = lines[i as usize].trim();
        i -= 1;
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if line.starts_with("#[") || line.ends_with(']') || line.ends_with(',') || line.ends_with('(') {
            out.push(line.to_string());
            if out.len() > 16 {
                break;
            }
            continue;
        }
        break;
    }
    out
}

fn attribute_names(node: &NodeRef, ctx: &Ctx) -> Vec<String> {
    if node.decorators.as_ref().is_some_and(|d| d.iter().any(|x| x == "tauri::command")) {
        return vec!["tauri::command".to_string()];
    }
    let Some(lines) = ctx.get_file_lines(&node.file_path) else { return Vec::new() };
    attributes_above(&lines, node.start_line)
        .iter()
        .filter_map(|line| re!(r"^#\[\s*([A-Za-z_][\w:]*)").captures(line).and_then(|c| c.text(1).map(str::to_string)))
        .collect()
}

/// Whether an attribute made this function a command. `generate_handler!` reads what an attribute
/// macro generates, so a plain namesake of a list's entry is some other function.
fn is_command_function(node: &NodeRef, ctx: &Ctx) -> bool {
    attribute_names(node, ctx).iter().any(|name| !Re::cached(PLAIN_ATTRIBUTE, "").is_match(name))
}

/// The stricter reading, for a list nothing can be checked against: Tauri's own attribute.
fn is_declared_command(node: &NodeRef, ctx: &Ctx) -> bool {
    attribute_names(node, ctx).iter().any(|name| name == "command" || name == "tauri::command")
}

/// The names a Rust file answers to in a path: its crate and its modules.
/// `crates/my-api/src/cmd/network.rs` → my_api, cmd, network.
fn module_names(file: &str) -> HashSet<String> {
    let trimmed = file.strip_suffix(".rs").unwrap_or(file);
    let parts: Vec<&str> = trimmed.split('/').collect();
    let names: Vec<&str> = match parts.iter().rposition(|p| *p == "src") {
        Some(src) => {
            parts[src.saturating_sub(1)..].iter().enumerate().filter(|(i, _)| *i != 1 || src == 0).map(|(_, p)| *p).collect()
        }
        None => parts.last().copied().into_iter().collect(),
    };
    names.into_iter().map(|n| n.replace('-', "_")).collect()
}

/// How many segments of a registered path name `node`'s crate or modules.
fn path_score(segments: &[String], node: &NodeRef) -> usize {
    let names = module_names(&node.file_path);
    segments[..segments.len() - 1].iter().filter(|s| names.contains(s.as_str())).count()
}

/// Where a crate starts: the nearest directory above `file` holding a `Cargo.toml`.
fn crate_root(file: &str, ctx: &Ctx, memo: &mut HashMap<String, String>) -> String {
    let parts: Vec<&str> = file.split('/').collect();
    let parts = &parts[..parts.len() - 1];
    let mut seen: Vec<String> = Vec::new();
    for n in (0..=parts.len()).rev() {
        let dir = parts[..n].join("/");
        if let Some(known) = memo.get(&dir).cloned() {
            for d in seen {
                memo.insert(d, known.clone());
            }
            return known;
        }
        seen.push(dir.clone());
        let manifest = if dir.is_empty() { "Cargo.toml".to_string() } else { format!("{dir}/Cargo.toml") };
        if ctx.file_exists(&manifest) {
            for d in seen {
                memo.insert(d, dir.clone());
            }
            return dir;
        }
    }
    for d in seen {
        memo.insert(d, String::new());
    }
    String::new()
}

/// The directory above a `src-tauri` folder, the frontend's root by convention.
fn conventional_root(file: &str) -> Option<String> {
    if file.starts_with("src-tauri/") {
        return Some(String::new());
    }
    file.find("/src-tauri/").map(|at| file[..at].to_string())
}

fn is_within(file: &str, root: &str) -> bool {
    root.is_empty() || (file.starts_with(root) && file[root.len()..].starts_with('/'))
}

fn same_nodes(a: &[NodeRef], b: &[NodeRef]) -> bool {
    a.len() == b.len() && a.iter().all(|n| b.iter().any(|m| m.id == n.id))
}

fn add_command(table: &mut CommandTable, key: String, command: Command) {
    match table.get(&key) {
        None => {
            table.insert(key, Some(command));
        }
        Some(Some(known)) if !same_nodes(&known.nodes, &command.nodes) => {
            table.insert(key, None);
        }
        _ => {}
    }
}

/// The functions a list entry names: one, or one per platform; a path that tells them apart
/// decides. Too many is a name, not a command.
fn commands_for(entry: &ListEntry, list: &HandlerList, crates: &mut HashMap<String, String>, ctx: &Ctx) -> Vec<NodeRef> {
    let name = entry.segments.last().unwrap();
    let mut candidates: Vec<NodeRef> = ctx
        .get_nodes_by_name(name)
        .iter()
        .filter(|n| n.language == Language::Rust && n.kind == NodeKind::Function && !is_test_path(&n.file_path))
        .filter(|n| is_command_function(n, ctx))
        .cloned()
        .collect();
    if candidates.len() > 1 {
        let mut scope = |file: &str| conventional_root(file).unwrap_or_else(|| crate_root(file, ctx, crates));
        let own = scope(&list.file);
        let same: Vec<NodeRef> = candidates.iter().filter(|n| scope(&n.file_path) == own).cloned().collect();
        if !same.is_empty() {
            candidates = same;
        }
    }
    if candidates.len() > 1 {
        let scores: Vec<usize> = candidates.iter().map(|n| path_score(&entry.segments, n)).collect();
        let best = *scores.iter().max().unwrap();
        candidates = candidates.into_iter().zip(scores).filter(|(_, s)| *s == best).map(|(n, _)| n).collect();
    }
    if candidates.len() <= MAX_VARIANTS {
        candidates
    } else {
        Vec::new()
    }
}

#[derive(Default)]
struct RustSide {
    apps: Vec<TauriApp>,
    /// `plugin:<name>|<command>` → its command, wherever the plugin's crate sits.
    plugins: CommandTable,
    /// Whole-repository view, for a frontend no app's folder holds.
    any_app: CommandTable,
    /// Event structs by wire name, by type name, and by the camelCase name of their `events.x`.
    by_event: HashMap<String, Vec<EventStruct>>,
    by_type: HashMap<String, Vec<EventStruct>>,
    by_camel: HashMap<String, Vec<EventStruct>>,
    emitters: Vec<Endpoint>,
    listeners: Vec<Endpoint>,
    /// App folders, deepest first.
    roots: Vec<String>,
}

impl RustSide {
    /// The app folder `file` belongs to by convention; `None` outside every one, which pairs with any.
    fn scope_of(&self, file: &str) -> Option<String> {
        self.roots.iter().find(|root| is_within(file, root)).cloned()
    }
}

fn same_scope(a: &Option<String>, b: &Option<String>) -> bool {
    a.is_none() || b.is_none() || a == b
}

/// The struct of the file's own app, else one no app holds.
fn struct_in_scope(found: Option<&Vec<EventStruct>>, scope: &Option<String>) -> Option<EventStruct> {
    let found = found?;
    found.iter().find(|s| &s.scope == scope).or_else(|| found.iter().find(|s| same_scope(&s.scope, scope))).cloned()
}

const STR_CONST: &str = r#"\b(?:const|static)\s+([A-Z][A-Z0-9_]*)\s*:\s*&\s*(?:'static\s+)?str\s*=\s*"([^"\n]*)""#;
const EVENT_DERIVE: &str = r"#\s*\[\s*derive\s*\(([^)]*)\)\s*\]";
const EVENT_ITEM: &str = r"^(?:\s*#\s*\[(?:[^\]]|\][^\s])*\])*\s*(?:pub(?:\s*\([^)]*\))?\s+)?(?:struct|enum)\s+([A-Za-z_]\w*)";
const EVENT_RENAME: &str = r#"#\s*\[\s*tauri_specta\s*\(\s*event_name\s*=\s*"([^"]+)"\s*\)\s*\]"#;
const EMIT_CALL: &str = r"\.\s*(emit(?:_str|_all|_to|_str_to|_filter|_str_filter|_and_trigger)?)\s*(?:::\s*<[^()]*>\s*)?\(";
const LISTEN_CALL: &str = r"\.\s*(?:listen|once)(?:_any|_global)?\s*(?:::\s*<[^()]*>\s*)?\(";
/// tauri-specta's typed form: `Saved::listen(app, |e| …)`.
const TYPED_LISTEN_CALL: &str = r"\b([A-Z]\w*)\s*::\s*(?:listen|once)(?:_any)?\s*\(";

/// The string a Rust argument is: a literal, or a `&str` constant.
fn rust_string_at(rust: &RustFile, span: Option<&(usize, usize)>, constants: &Constants) -> Option<String> {
    let raw = span_text(&rust.text, *span?);
    if let Some(c) = re!(r#"^&?"([^"\\]*)"$"#).captures(raw) {
        return c.text(1).map(str::to_string);
    }
    let name = re!(r"^(?:[a-z_]\w*::)*([A-Z][A-Z0-9_]*)$").captures(raw)?.text(1)?.to_string();
    constants.get(&name).cloned().flatten()
}

/// The last segment of the path ending at `end` that names a type: `events::Saved` → `Saved`.
fn type_at(code: &str, end: usize) -> Option<String> {
    let window = cut(code, end.saturating_sub(200), end + 1);
    let head = re!(r"(?:[A-Za-z_]\w*\s*::\s*)*[A-Za-z_]\w*\s*$").find(window)?;
    head.as_str().rsplit("::").map(str::trim).find(|s| s.starts_with(|c: char| c.is_ascii_uppercase())).map(str::to_string)
}

/// The type `x` was given in the function around `at`: `let x = Saved { … }`, `let x: Saved`, `x: &Saved`.
fn local_type(code: &str, name: &str, from: usize, at: usize) -> Option<String> {
    let n = escape_regex(name);
    let pattern = Re::cached(
        &format!(
            r"\blet\s+(?:mut\s+)?{n}\s*(?::\s*&?\s*(?:[a-z_]\w*::)*([A-Z]\w*)[^=;]*)?=\s*&?\s*(?:[a-z_]\w*::)*([A-Z]\w*)|\b{n}\s*:\s*&?\s*(?:[a-z_]\w*::)*([A-Z]\w*)"
        ),
        "g",
    );
    let mut found = None;
    for m in pattern.captures_iter(cut(code, from, at)) {
        if let Some(t) = m.text(1).or(m.text(2)).or(m.text(3)) {
            found = Some(t.to_string());
        }
    }
    found
}

/// The type a `<receiver>.emit(…)` is called on, read back from its `.`.
fn receiver_type(code: &str, dot: usize, function_start: usize) -> Option<String> {
    let b = code.as_bytes();
    let mut i = dot.checked_sub(1)?;
    while b[i].is_ascii_whitespace() {
        i = i.checked_sub(1)?;
    }
    let c = b[i];
    if c == b')' || c == b'}' {
        let open = open_bracket(b, i)?;
        let mut before = open as isize - 1;
        while before >= 0 && b[before as usize].is_ascii_whitespace() {
            before -= 1;
        }
        if before >= 0 && is_word(b[before as usize]) {
            return type_at(code, before as usize);
        }
        // `(Saved { … })`: the parentheses only wrap it.
        return re!(r"^\s*&?\s*(?:[a-z_]\w*::)*([A-Z]\w*)").captures(cut(code, open + 1, i))?.text(1).map(str::to_string);
    }
    if !is_word(c) {
        return None;
    }
    let word = re!(r"[A-Za-z_]\w*$").find(cut(code, i.saturating_sub(120), i + 1))?.as_str().to_string();
    if word.starts_with(|c: char| c.is_ascii_uppercase()) {
        return Some(word);
    }
    local_type(code, &word, function_start, i)
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn owner_of(nodes: &[NodeRef], line: u32, column: u32) -> Option<NodeRef> {
    let mut best: Option<&NodeRef> = None;
    for n in nodes {
        if !is_fn_kind(n.kind) || n.start_line > line || n.end_line < line {
            continue;
        }
        if (n.start_line == line && n.start_column > column) || (n.end_line == line && n.end_column <= column) {
            continue;
        }
        if best.is_none_or(|b| n.start_line > b.start_line || (n.start_line == b.start_line && n.start_column >= b.start_column)) {
            best = Some(n);
        }
    }
    best.cloned().or_else(|| enclosing_value(nodes, line)).or_else(|| nodes.iter().find(|n| n.kind == NodeKind::File).cloned())
}

fn read_rust_file(file: &str, source: &str, ctx: &Ctx) -> RustFile {
    let text = align(source, strip_comments_for_regex(source, "rust"));
    let code = align(&text, blank_string_contents(&text, None));
    RustFile { file: file.to_string(), lines: Lines::new(source), text, code, nodes: ctx.get_nodes_in_file(file) }
}

fn add_struct(index: &mut HashMap<String, Vec<EventStruct>>, key: String, s: EventStruct) {
    index.entry(key).or_default().push(s);
}

fn read_event_structs(rust: &RustFile, source: &str, out: &mut RustSide) {
    if !source.contains("tauri_specta") {
        return;
    }
    for m in Re::cached(EVENT_DERIVE, "g").captures_iter(&rust.code) {
        if !re!(r"\bEvent\b").is_match(m.text(1).unwrap_or("")) {
            continue;
        }
        let whole = m.get(0).unwrap();
        let after = whole.end();
        let Some(item) = Re::cached(EVENT_ITEM, "").captures(cut(&rust.code, after, after + 4000)) else { continue };
        let name = item.text(1).unwrap_or("").to_string();
        let Some(node) = rust.nodes.iter().find(|n| n.name == name && matches!(n.kind, NodeKind::Struct | NodeKind::Enum)).cloned() else { continue };
        let span = cut(&rust.text, whole.start().saturating_sub(400), after + item.get(0).unwrap().as_str().len());
        let tail = &span[span.rfind('}').map_or(0, |p| p + 1)..];
        let event = Re::cached(EVENT_RENAME, "").captures(tail).and_then(|c| c.text(1).map(str::to_string)).unwrap_or_else(|| to_kebab(&name));
        let s = EventStruct { node, event: event.clone(), scope: out.scope_of(&rust.file) };
        add_struct(&mut out.by_event, event, s.clone());
        add_struct(&mut out.by_camel, to_camel(&name), s.clone());
        add_struct(&mut out.by_type, name, s);
    }
}

fn read_emitters(rust: &RustFile, constants: &Constants, out: &mut RustSide) {
    let code = rust.code.as_bytes();
    for m in Re::cached(EMIT_CALL, "g").captures_iter(&rust.code) {
        let whole = m.get(0).unwrap();
        let spans = argument_spans(code, whole.end() - 1);
        let line = rust.lines.line_at(whole.start());
        let column = column_at(&rust.text, whole.start());
        let Some(owner) = enclosing_fn(&rust.nodes, line) else { continue };
        // `app.emit_to("main", "saved", payload)` names its target first, and a typed
        // `Saved { … }.emit_to(&app, "main")` has no event among its arguments.
        let targeted = m.text(1).unwrap_or("").ends_with("_to");
        let mut event =
            if targeted && spans.len() < 3 { None } else { rust_string_at(rust, spans.get(if targeted { 1 } else { 0 }), constants) };
        if event.is_none() {
            let scope = out.scope_of(&rust.file);
            event = receiver_type(&rust.code, whole.start(), rust.lines.offset_of_line(owner.start_line))
                .and_then(|t| struct_in_scope(out.by_type.get(&t), &scope))
                .map(|s| s.event);
        }
        if let Some(event) = event {
            let scope = out.scope_of(&rust.file);
            out.emitters.push(Endpoint { node: owner, event, file: rust.file.clone(), line, column, rust: true, scope });
        }
    }
}

fn read_listeners(rust: &RustFile, constants: &Constants, out: &mut RustSide) {
    let code = rust.code.as_bytes();
    for m in Re::cached(LISTEN_CALL, "g").captures_iter(&rust.code) {
        let whole = m.get(0).unwrap();
        let spans = argument_spans(code, whole.end() - 1);
        let Some(event) = rust_string_at(rust, spans.first(), constants) else { continue };
        let line = rust.lines.line_at(whole.start());
        if let Some(owner) = enclosing_fn(&rust.nodes, line) {
            let scope = out.scope_of(&rust.file);
            out.listeners.push(Endpoint { node: owner, event, file: rust.file.clone(), line, column: column_at(&rust.text, whole.start()), rust: true, scope });
        }
    }
    for m in Re::cached(TYPED_LISTEN_CALL, "g").captures_iter(&rust.code) {
        let whole = m.get(0).unwrap();
        let scope = out.scope_of(&rust.file);
        let Some(event) = struct_in_scope(out.by_type.get(m.text(1).unwrap_or("")), &scope).map(|s| s.event) else { continue };
        let line = rust.lines.line_at(whole.start());
        if let Some(owner) = enclosing_fn(&rust.nodes, line) {
            out.listeners.push(Endpoint { node: owner, event, file: rust.file.clone(), line, column: column_at(&rust.text, whole.start()), rust: true, scope });
        }
    }
}

fn read_rust_side(ctx: &Ctx) -> RustSide {
    let mut files: Vec<(RustFile, std::rc::Rc<str>)> = Vec::new();
    let mut conventional: IndexSet<String> = IndexSet::new();
    let mut constants: Constants = HashMap::new();
    let gate = Re::cached(RUST_GATE, "");
    for file in ctx.get_all_files() {
        if !file.ends_with(".rs") {
            continue;
        }
        if let Some(root) = conventional_root(&file) {
            conventional.insert(root);
        }
        if is_test_path(&file) {
            continue;
        }
        let Some(source) = ctx.read_file(&file).filter(|s| !s.is_empty()) else { continue };
        if !gate.is_match(&source) {
            continue;
        }
        let rust = read_rust_file(&file, &source, ctx);
        for m in Re::cached(STR_CONST, "g").captures_iter(&rust.text) {
            let (name, value) = (m.text(1).unwrap_or("").to_string(), m.text(2).unwrap_or("").to_string());
            let next = match constants.get(&name) {
                None => Some(value),
                Some(Some(known)) if *known == value => Some(value),
                _ => None,
            };
            constants.insert(name, next);
        }
        files.push((rust, source));
    }

    let mut roots: Vec<String> = conventional.iter().cloned().collect();
    roots.sort_by_key(|r| std::cmp::Reverse(r.len()));
    let mut side = RustSide { roots, ..Default::default() };
    let mut apps: IndexMap<String, TauriApp> = IndexMap::new();
    let app_at = |apps: &mut IndexMap<String, TauriApp>, root: String| -> usize {
        let entry = apps.entry(root.clone());
        let index = entry.index();
        entry.or_insert_with(|| TauriApp { root, commands: IndexMap::new(), open_list: None });
        index
    };
    for root in &conventional {
        app_at(&mut apps, root.clone());
    }

    let mut crates: HashMap<String, String> = HashMap::new();
    for (rust, source) in &files {
        read_event_structs(rust, source, &mut side);
        for m in Re::cached(HANDLER_LIST, "g").captures_iter(&rust.code) {
            let whole = m.get(0).unwrap();
            let Some(list) = read_handler_list(rust, &constants, whole.start(), whole.end() - 1) else { continue };
            let root = conventional_root(&rust.file).unwrap_or_else(|| crate_root(&rust.file, ctx, &mut crates));
            let app = app_at(&mut apps, root);
            if list.opaque {
                apps[app].open_list = Some(format!("{}:{}", list.file, list.line));
            }
            for entry in &list.entries {
                let nodes = commands_for(entry, &list, &mut crates, ctx);
                if nodes.is_empty() {
                    continue;
                }
                let command = Command { nodes, registered_at: format!("{}:{}", list.file, entry.line) };
                let name = entry.segments.last().unwrap();
                match &list.plugin {
                    Some(plugin) => add_command(&mut side.plugins, format!("plugin:{plugin}|{name}"), command),
                    None => add_command(&mut apps[app].commands, name.clone(), command),
                }
            }
        }
    }

    // A list a macro builds names nothing here, so what the app declares answers.
    for app in apps.values_mut() {
        let Some(open_list) = app.open_list.clone() else { continue };
        let mut declared: IndexMap<String, Vec<NodeRef>> = IndexMap::new();
        for (rust, _) in &files {
            if !is_within(&rust.file, &app.root) {
                continue;
            }
            for n in rust.nodes.iter() {
                if n.kind == NodeKind::Function && is_declared_command(n, ctx) {
                    declared.entry(n.name.clone()).or_default().push(n.clone());
                }
            }
        }
        for (name, nodes) in declared {
            if nodes.len() <= MAX_VARIANTS {
                add_command(&mut app.commands, name, Command { nodes, registered_at: open_list.clone() });
            }
        }
    }

    // Emit sites come after every struct is known: a type can be used before its file comes up.
    for (rust, _) in &files {
        read_emitters(rust, &constants, &mut side);
        read_listeners(rust, &constants, &mut side);
    }

    let mut apps: Vec<TauriApp> = apps.into_values().collect();
    apps.sort_by_key(|a| std::cmp::Reverse(a.root.len()));
    for app in &apps {
        for (key, command) in &app.commands {
            match command {
                None => {
                    side.any_app.insert(key.clone(), None);
                }
                Some(c) => add_command(&mut side.any_app, key.clone(), c.clone()),
            }
        }
    }
    side.apps = apps;
    side
}

/// The deepest app whose folder holds `file`; a frontend beside its crate looks across every app.
fn table_for<'a>(file: &str, side: &'a RustSide) -> &'a CommandTable {
    side.apps
        .iter()
        .find(|app| is_within(file, &app.root) && (!app.root.is_empty() || side.apps.len() == 1 || !app.commands.is_empty()))
        .map_or(&side.any_app, |app| &app.commands)
}

// ── The frontend ──────────────────────────────────────────────────────────────

struct FrontFile {
    file: String,
    /// Comments blanked, strings kept.
    text: String,
    /// Comments and string contents blanked.
    code: String,
    nodes: NodeList,
    lines: Lines,
}

struct Imp {
    local: String,
    exported: String,
    source: String,
    is_namespace: bool,
}

struct FrontScan {
    front: FrontFile,
    imports: Vec<Imp>,
    language: Language,
    bindings: FrontBindings,
}

/// A project function that takes a command as its first argument and hands it to `invoke`.
#[derive(Clone)]
struct Forwarder {
    name: String,
    file: String,
}

#[derive(Clone, Copy)]
struct CallSite {
    /// Offset of the callee's first character.
    at: usize,
    /// Offset of its `(`.
    open: usize,
}

/// Every call of the callee `pattern` matches, from where the match ends; type arguments skipped.
fn calls_of(front: &FrontFile, pattern: &Re) -> Vec<CallSite> {
    let code = front.code.as_bytes();
    pattern
        .captures_iter(&front.code)
        .filter_map(|m| {
            let whole = m.get(0).unwrap();
            let open = skip_type_arguments(code, skip_space(code, whole.end()));
            (code.get(open) == Some(&b'(')).then_some(CallSite { at: whole.start(), open })
        })
        .collect()
}

/// The length of the string literal at `at`, its quotes included.
fn closed_length(text: &[u8], at: usize) -> usize {
    let q = text[at];
    let mut i = at + 1;
    while i < text.len() {
        if text[i] == b'\\' {
            i += 1;
        } else if text[i] == q {
            return i - at + 1;
        }
        i += 1;
    }
    text.len() - at
}

fn owner_at(front: &FrontFile, offset: usize) -> Option<NodeRef> {
    owner_of(&front.nodes, front.lines.line_at(offset), column_at(&front.text, offset))
}

/// What a JS argument is known to say: a literal with no holes, or a same-file constant holding one.
fn js_string_at(front: &FrontFile, span: Option<&(usize, usize)>) -> Option<String> {
    let &(start, end) = span?;
    let text = front.text.as_bytes();
    let at = skip_space(text, start);
    let c = *text.get(at)?;
    if c == b'"' || c == b'\'' || c == b'`' {
        let value = read_string_at(&front.text, at)?;
        if value.contains(HOLE) {
            return None;
        }
        // The whole argument, not the head of a concatenation.
        return (cut(&front.text, at, end).trim().len() == closed_length(text, at)).then_some(value);
    }
    let name = span_text(&front.text, (start, end));
    if !re!(r"^[A-Za-z_$][\w$]*$").is_match(name) {
        return None;
    }
    let constant = Re::cached(&format!(r#"\bconst\s+{}\s*(?::[^=\n]*)?=\s*(['"`])([^'"`\n$]*)\1"#, escape_regex(name)), "").captures(&front.text)?;
    // Another function's local is not this argument.
    let declared_in = owner_at(front, constant.get(0).unwrap().start());
    let local = declared_in.as_ref().is_some_and(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method));
    if local && declared_in.map(|n| n.id.clone()) != owner_at(front, start).map(|n| n.id.clone()) {
        return None;
    }
    constant.text(2).map(str::to_string)
}

/// A relative or aliased specifier, never a package from npm: a project's own bindings file.
const PROJECT_SPECIFIER: &str = r"^(?:\.{1,2}/|[~@]/|#/|[#$]\w)";

#[derive(Default)]
struct FrontBindings {
    /// Local names of Tauri's `invoke`.
    invoke: IndexSet<String>,
    /// Local names of a namespace holding `invoke`: `core.invoke(…)`.
    invoke_holders: IndexSet<String>,
    listen: IndexSet<String>,
    emit: IndexSet<String>,
    /// The names in `emit` that are `emitTo(target, 'x', payload)`.
    emit_to: IndexSet<String>,
    /// Names bound by `const { invoke } = await import(…)`: a local that is the binding.
    dynamic: IndexSet<String>,
    /// Local names of a namespace holding `listen` / `once` / `emit`.
    event_holders: IndexSet<String>,
    /// The file holds a window or webview handle: `appWindow.listen('x', …)`.
    handles: bool,
    /// tauri-specta's `commands`, its module not in the index.
    typed_commands: IndexSet<String>,
    /// tauri-specta's `events`.
    typed_events: IndexSet<String>,
}

const GLOBAL_API: &str = r"(?:window\s*\.\s*)?__TAURI(?:_INTERNALS)?__";

fn global_invoke() -> Arc<Re> {
    Re::cached(&format!(r"(?<![\w$.]){GLOBAL_API}\s*\.\s*(?:(?:core|tauri)\s*\.\s*)?invoke\b"), "g")
}

fn read_bindings(front: &FrontFile, imports: &[Imp], language: Language, ctx: &Ctx) -> FrontBindings {
    let mut b = FrontBindings::default();
    let mut dynamic: Vec<Imp> = Vec::new();
    for m in re!(r#"\{([^{}]*)\}\s*=\s*await\s+import\(\s*['"](@tauri-apps/api[^'"]*)['"]\s*\)"#, "g").captures_iter(&front.text) {
        for part in m.text(1).unwrap_or("").split(',') {
            let mut pieces = part.split(':').map(str::trim);
            let name = pieces.next().unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let local = pieces.next().filter(|l| !l.is_empty()).unwrap_or(name);
            dynamic.push(Imp { local: local.to_string(), exported: name.to_string(), source: m.text(2).unwrap_or("").to_string(), is_namespace: false });
            b.dynamic.insert(local.to_string());
        }
    }
    let project = Re::cached(PROJECT_SPECIFIER, "");
    for m in imports.iter().chain(dynamic.iter()) {
        if m.source == API_ROOT && m.exported == "tauri" {
            b.invoke_holders.insert(m.local.clone());
        } else if m.source == API_ROOT && m.exported == "event" {
            b.event_holders.insert(m.local.clone());
        } else if is_core_module(&m.source) {
            if m.is_namespace {
                b.invoke_holders.insert(m.local.clone());
            } else if m.exported == "invoke" {
                b.invoke.insert(m.local.clone());
            }
        } else if m.source == EVENT_MODULE {
            if m.is_namespace {
                b.event_holders.insert(m.local.clone());
            } else if m.exported == "listen" || m.exported == "once" {
                b.listen.insert(m.local.clone());
            } else if m.exported == "emit" || m.exported == "emitTo" {
                b.emit.insert(m.local.clone());
                if m.exported == "emitTo" {
                    b.emit_to.insert(m.local.clone());
                }
            }
        } else if is_handle_module(&m.source) {
            b.handles = true;
        } else if m.exported == "commands" {
            if project.is_match(&m.source) && resolve_import_path(&m.source, &front.file, language, ctx).is_none() {
                b.typed_commands.insert(m.local.clone());
            }
        } else if m.exported == "events"
            && (project.is_match(&m.source) || resolve_import_path(&m.source, &front.file, language, ctx).is_some())
        {
            b.typed_events.insert(m.local.clone());
        }
    }
    let invoke_binding = Re::cached(
        &format!(
            r"\b(?:const|let|var)\s*(?:\{{[^}}]*?\binvoke\b(?:\s*:\s*([\w$]+))?[^}}]*\}}\s*=\s*{GLOBAL_API}(?:\s*\.\s*(?:core|tauri))?\b|([\w$]+)\s*=\s*{GLOBAL_API}(?:\s*\.\s*(?:core|tauri))?\s*\.\s*invoke\b)"
        ),
        "g",
    );
    for m in invoke_binding.captures_iter(&front.code) {
        b.invoke.insert(m.text(1).or(m.text(2)).unwrap_or("invoke").to_string());
    }
    let event_binding = Re::cached(&format!(r"\b(?:const|let|var)\s*\{{([^}}]*)\}}\s*=\s*{GLOBAL_API}\s*\.\s*event\b"), "g");
    for m in event_binding.captures_iter(&front.code) {
        for part in m.text(1).unwrap_or("").split(',') {
            let mut pieces = part.split(':').map(str::trim);
            let name = pieces.next().unwrap_or("");
            let local = pieces.next().filter(|l| !l.is_empty()).unwrap_or(name).to_string();
            if name == "listen" || name == "once" {
                b.listen.insert(local);
            } else if name == "emit" || name == "emitTo" {
                b.emit.insert(local.clone());
                if name == "emitTo" {
                    b.emit_to.insert(local);
                }
            }
        }
    }
    b
}

/// Whether a local or parameter hides the imported name at the call.
fn shadowed(name: &str, owner: &NodeRef, front: &FrontFile, line: u32, column: u32, ctx: &Ctx) -> bool {
    if !matches!(owner.kind, NodeKind::Function | NodeKind::Method) {
        return false;
    }
    if js_code_binds_name(name, owner, &front.file, line, ctx) {
        return true;
    }
    // Destructured, which that check leaves alone: `const { invoke } = api`, `({ invoke }) => …`.
    let from = front.lines.offset(&front.code, owner.start_line, owner.start_column);
    let to = front.lines.offset(&front.code, line, column);
    let before = cut(&front.code, from, to);
    let n = escape_regex(name);
    Re::cached(&format!(r"(?:\b(?:const|let|var)\s*|\(\s*)[{{\[][^=;(){{}}]*?(?<![\w$:.]){n}(?![\w$])"), "").is_match(before)
}

/// The node a JS handler argument names, else the function the listener is written in.
fn handler_of(scan: &FrontScan, span: Option<&(usize, usize)>, owner: &NodeRef, ctx: &Ctx) -> NodeRef {
    let Some(&span) = span else { return owner.clone() };
    let Some(caps) = re!(r"^(?:this\s*\.\s*)?([A-Za-z_$][\w$]*)$").captures(span_text(&scan.front.text, span)) else { return owner.clone() };
    let name = caps.text(1).unwrap_or("");
    if let Some(here) = scan.front.nodes.iter().find(|n| n.name == name && is_fn_kind(n.kind)) {
        return here.clone();
    }
    let Some(imported) = scan.imports.iter().find(|m| m.local == name) else { return owner.clone() };
    let Some(path) = resolve_import_path(&imported.source, &scan.front.file, scan.language, ctx) else { return owner.clone() };
    ctx.get_nodes_in_file(&path).iter().find(|n| n.name == imported.exported && is_fn_kind(n.kind)).cloned().unwrap_or_else(|| owner.clone())
}

struct Subscription {
    owner: NodeRef,
    event_struct: EventStruct,
    line: u32,
    column: u32,
}

#[derive(Default)]
struct Collected {
    edges: Vec<Edge>,
    seen: HashSet<String>,
    emitters: Vec<Endpoint>,
    listeners: Vec<Endpoint>,
    /// The listening function → the event struct, once per pair.
    subscribed: IndexMap<String, Subscription>,
}

fn push_command(out: &mut Collected, owner: &NodeRef, key: &str, command: &Command, callee: &str, line: u32, column: u32) {
    for target in &command.nodes {
        if !out.seen.insert(format!("{}>{}", owner.id, target.id)) {
            continue;
        }
        let mut e = calls_edge(
            &owner.id,
            &target.id,
            Some(line),
            meta! {
                "synthesizedBy" => "tauri-command", "channel" => "ipc", "tier" => TIER_CLIENT_TO_SERVER,
                "callee" => callee, "command" => key, "registeredAt" => command.registered_at.as_str(),
            },
        );
        e.column = Some(column);
        out.edges.push(e);
    }
}

/// A callee's bare name, not a member of something else.
fn bare_callee(name: &str) -> Arc<Re> {
    Re::cached(&format!(r"(?<![\w$.]){}(?![\w$])", escape_regex(name)), "g")
}

fn holder_member(holder: &str, members: &str) -> Arc<Re> {
    Re::cached(&format!(r"(?<![\w$.]){}\s*\.\s*{members}(?![\w$])", escape_regex(holder)), "g")
}

fn any_member(name: &str) -> Arc<Re> {
    Re::cached(&format!(r"\.\s*{}(?![\w$])", escape_regex(name)), "g")
}

/// The first parameter `node` declares, read from its head.
fn first_parameter(front: &FrontFile, node: &NodeRef) -> Option<String> {
    let start = front.lines.offset(&front.code, node.start_line, node.start_column);
    let head = cut(&front.code, start, start + 600);
    re!(r"^[^(]*\(\s*([A-Za-z_$][\w$]*)").captures(head)?.text(1).map(str::to_string)
}

/// The name a forwarder has in this file: its own, or the one an import gives it.
fn local_forwarder(forwarder: &Forwarder, scan: &FrontScan, ctx: &Ctx) -> Option<String> {
    if forwarder.file == scan.front.file {
        return Some(forwarder.name.clone());
    }
    let imported = scan.imports.iter().find(|m| m.exported == forwarder.name)?;
    resolve_import_path(&imported.source, &scan.front.file, scan.language, ctx).map(|_| imported.local.clone())
}

fn forwarders_in(scan: &FrontScan, found: &IndexMap<String, Forwarder>, ctx: &Ctx) -> Vec<Forwarder> {
    let mut here: IndexMap<String, Forwarder> = IndexMap::new();
    let mut callees: Vec<Arc<Re>> = scan.bindings.invoke.iter().map(|n| bare_callee(n)).collect();
    callees.extend(scan.bindings.invoke_holders.iter().map(|h| holder_member(h, "invoke")));
    callees.push(global_invoke());
    for forwarder in found.values() {
        if let Some(local) = local_forwarder(forwarder, scan, ctx) {
            callees.push(bare_callee(&local));
        }
        callees.push(any_member(&forwarder.name));
    }
    let front = &scan.front;
    for callee in &callees {
        for call in calls_of(front, callee) {
            let argument = argument_spans(front.code.as_bytes(), call.open).first().map_or("", |s| span_text(&front.code, *s));
            if !re!(r"^[A-Za-z_$][\w$]*$").is_match(argument) {
                continue;
            }
            let Some(owner) = owner_at(front, call.at) else { continue };
            if !matches!(owner.kind, NodeKind::Function | NodeKind::Method) || owner.name == "<anonymous>" {
                continue;
            }
            if first_parameter(front, &owner).as_deref() == Some(argument) {
                here.insert(owner.name.clone(), Forwarder { name: owner.name.clone(), file: front.file.clone() });
            }
        }
    }
    here.into_values().collect()
}

/// The project's own functions around `invoke`, found by what they do: one that hands its first
/// parameter on as the command, or a field set to one (`invoke = tauriInvoke`).
fn find_forwarders(scans: &IndexMap<String, FrontScan>, ctx: &Ctx) -> Vec<Forwarder> {
    let mut found: IndexMap<String, Forwarder> = IndexMap::new();
    // A wrapper of a wrapper: a few rounds, since nobody writes a tall stack.
    for _ in 0..3 {
        let before = found.len();
        for scan in scans.values() {
            for forwarder in forwarders_in(scan, &found, ctx) {
                found.entry(forwarder.name.clone()).or_insert(forwarder);
            }
            let own: Vec<Forwarder> = found.values().filter(|f| f.file == scan.front.file).cloned().collect();
            for forwarder in own {
                let alias = Re::cached(
                    &format!(
                        r"(?:^|[\s;{{])(?:(?:public|private|protected|readonly|static)\s+)*([A-Za-z_$][\w$]*)\s*(?::[^=;\n]*)?=\s*{}\s*[;\n]",
                        escape_regex(&forwarder.name)
                    ),
                    "g",
                );
                for m in alias.captures_iter(&scan.front.code) {
                    let name = m.text(1).unwrap_or("").to_string();
                    found.entry(name.clone()).or_insert(Forwarder { name, file: forwarder.file.clone() });
                }
            }
        }
        if found.len() == before {
            break;
        }
    }
    found.into_values().collect()
}

struct Site {
    owner: NodeRef,
    line: u32,
    column: u32,
}

struct FrontReader<'a> {
    scan: &'a FrontScan,
    side: &'a RustSide,
    table: &'a CommandTable,
    scope: Option<String>,
    ctx: &'a Ctx,
}

impl FrontReader<'_> {
    fn site(&self, call: CallSite) -> Option<Site> {
        let front = &self.scan.front;
        let line = front.lines.line_at(call.at);
        let column = column_at(&front.text, call.at);
        owner_of(&front.nodes, line, column).map(|owner| Site { owner, line, column })
    }

    /// A local or parameter that hides `binding` where the call is.
    fn hidden(&self, binding: Option<&str>, at: &Site) -> bool {
        binding.is_some_and(|b| !self.scan.bindings.dynamic.contains(b) && shadowed(b, &at.owner, &self.scan.front, at.line, at.column, self.ctx))
    }

    fn command(&self, key: &str) -> Option<&Command> {
        let table = if key.starts_with("plugin:") { &self.side.plugins } else { self.table };
        table.get(key).and_then(Option::as_ref)
    }

    fn invoke_call(&self, out: &mut Collected, call: CallSite, binding: Option<&str>, callee: &str) {
        let front = &self.scan.front;
        let Some(key) = js_string_at(front, argument_spans(front.code.as_bytes(), call.open).first()) else { return };
        let Some(target) = self.command(&key) else { return };
        let Some(at) = self.site(call) else { return };
        if self.hidden(binding, &at) {
            return;
        }
        push_command(out, &at.owner, &key, target, callee, at.line, at.column);
    }

    fn listen_call(&self, out: &mut Collected, call: CallSite, binding: Option<&str>, event: Option<&str>, handler_index: usize) {
        let front = &self.scan.front;
        let spans = argument_spans(front.code.as_bytes(), call.open);
        let Some(name) = event.map(str::to_string).or_else(|| js_string_at(front, spans.first())) else { return };
        if name.starts_with("tauri://") {
            return;
        }
        let Some(at) = self.site(call) else { return };
        if self.hidden(binding, &at) {
            return;
        }
        out.listeners.push(Endpoint {
            node: handler_of(self.scan, spans.get(handler_index), &at.owner, self.ctx),
            event: name.clone(),
            file: front.file.clone(),
            line: at.line,
            column: at.column,
            rust: false,
            scope: self.scope.clone(),
        });
        if let Some(s) = struct_in_scope(self.side.by_event.get(&name), &self.scope) {
            let key = format!("{}>{}", at.owner.id, s.node.id);
            out.subscribed.entry(key).or_insert(Subscription { owner: at.owner, event_struct: s, line: at.line, column: at.column });
        }
    }

    fn emit_call(&self, out: &mut Collected, call: CallSite, binding: Option<&str>, event: Option<&str>, event_index: usize) {
        let front = &self.scan.front;
        let name = match event {
            Some(e) => e.to_string(),
            None => match js_string_at(front, argument_spans(front.code.as_bytes(), call.open).get(event_index)) {
                Some(n) => n,
                None => return,
            },
        };
        let Some(at) = self.site(call) else { return };
        if self.hidden(binding, &at) {
            return;
        }
        out.emitters.push(Endpoint { node: at.owner, event: name, file: front.file.clone(), line: at.line, column: at.column, rust: false, scope: self.scope.clone() });
    }
}

fn read_frontend(scan: &FrontScan, forwarders: &[Forwarder], side: &RustSide, ctx: &Ctx, out: &mut Collected) {
    let front = &scan.front;
    let b = &scan.bindings;
    let r = FrontReader { scan, side, table: table_for(&front.file, side), scope: side.scope_of(&front.file), ctx };

    for name in &b.invoke {
        for call in calls_of(front, &bare_callee(name)) {
            r.invoke_call(out, call, Some(name), name);
        }
    }
    for holder in &b.invoke_holders {
        for call in calls_of(front, &holder_member(holder, "invoke")) {
            r.invoke_call(out, call, Some(holder), &format!("{holder}.invoke"));
        }
    }
    for call in calls_of(front, &global_invoke()) {
        r.invoke_call(out, call, None, "invoke");
    }
    for forwarder in forwarders {
        if let Some(local) = local_forwarder(forwarder, scan, ctx) {
            for call in calls_of(front, &bare_callee(&local)) {
                r.invoke_call(out, call, Some(&local), &local);
            }
        }
        for call in calls_of(front, &any_member(&forwarder.name)) {
            r.invoke_call(out, call, None, &forwarder.name);
        }
    }

    // `commands.saveFile()` with no bindings file to walk through.
    let code = front.code.as_bytes();
    for holder in &b.typed_commands {
        let camel: HashMap<String, String> = r.table.keys().map(|k| (to_camel(k), k.clone())).collect();
        let pattern = Re::cached(&format!(r"(?<![\w$.]){}\s*\.\s*([A-Za-z_$][\w$]*)", escape_regex(holder)), "g");
        for m in pattern.captures_iter(&front.code) {
            let whole = m.get(0).unwrap();
            let member = m.text(1).unwrap_or("");
            let Some(key) = camel.get(member) else { continue };
            let open = skip_type_arguments(code, skip_space(code, whole.end()));
            if code.get(open) != Some(&b'(') {
                continue;
            }
            let Some(target) = r.table.get(key).and_then(Option::as_ref) else { continue };
            let Some(at) = r.site(CallSite { at: whole.start(), open }) else { continue };
            if shadowed(holder, &at.owner, front, at.line, at.column, ctx) {
                continue;
            }
            push_command(out, &at.owner, key, target, &format!("{holder}.{member}"), at.line, at.column);
        }
    }

    for name in &b.listen {
        for call in calls_of(front, &bare_callee(name)) {
            r.listen_call(out, call, Some(name), None, 1);
        }
    }
    for name in &b.emit {
        let index = if b.emit_to.contains(name) { 1 } else { 0 };
        for call in calls_of(front, &bare_callee(name)) {
            r.emit_call(out, call, Some(name), None, index);
        }
    }
    for holder in &b.event_holders {
        for call in calls_of(front, &holder_member(holder, "(?:listen|once)")) {
            r.listen_call(out, call, Some(holder), None, 1);
        }
        for call in calls_of(front, &holder_member(holder, "emit")) {
            r.emit_call(out, call, Some(holder), None, 0);
        }
    }
    if b.handles {
        // A window or webview handle has the same three, and is often a call away.
        for call in calls_of(front, re!(r"\.\s*(?:listen|once)(?![\w$])", "g")) {
            r.listen_call(out, call, None, None, 1);
        }
        for call in calls_of(front, re!(r"\.\s*emit(?![\w$])", "g")) {
            r.emit_call(out, call, None, None, 0);
        }
    }
    for holder in &b.typed_events {
        // `events.fileSaved.listen(fn)`, `events.fileSaved(appWindow).once(fn)`.
        let pattern = Re::cached(&format!(r"(?<![\w$.]){}\s*\.\s*([A-Za-z_$][\w$]*)", escape_regex(holder)), "g");
        for m in pattern.captures_iter(&front.code) {
            let whole = m.get(0).unwrap();
            let mut end = skip_space(code, whole.end());
            if code.get(end) == Some(&b'(') {
                let Some(close) = close_bracket(code, end) else { continue };
                end = skip_space(code, close + 1);
            }
            let Some(member) = re!(r"^\.\s*(listen|once|emit)(?![\w$])").captures(cut(&front.code, end, end + 32)) else { continue };
            let open = skip_space(code, end + member.get(0).unwrap().as_str().len());
            if code.get(open) != Some(&b'(') {
                continue;
            }
            let Some(s) = struct_in_scope(side.by_camel.get(m.text(1).unwrap_or("")), &r.scope) else { continue };
            let call = CallSite { at: whole.start(), open };
            if member.text(1) == Some("emit") {
                r.emit_call(out, call, Some(holder), Some(&s.event), 0);
            } else {
                r.listen_call(out, call, Some(holder), Some(&s.event), 0);
            }
        }
    }
}

fn language_of(file: &str) -> Language {
    if file.ends_with(".svelte") {
        Language::Svelte
    } else if file.ends_with(".vue") {
        Language::Vue
    } else {
        Language::TypeScript
    }
}

fn distinct_nodes(ends: &[&Endpoint]) -> usize {
    ends.iter().map(|e| e.node.id.as_str()).collect::<HashSet<_>>().len()
}

fn pair_events(out: &mut Collected) {
    let mut emitting: IndexMap<&str, Vec<&Endpoint>> = IndexMap::new();
    for e in &out.emitters {
        emitting.entry(e.event.as_str()).or_default().push(e);
    }
    let mut listening: HashMap<&str, Vec<&Endpoint>> = HashMap::new();
    for l in &out.listeners {
        listening.entry(l.event.as_str()).or_default().push(l);
    }
    let mut edges = Vec::new();
    for (event, from) in &emitting {
        let Some(to) = listening.get(event) else { continue };
        if distinct_nodes(from) > TAURI_EVENT_FANOUT_CAP || distinct_nodes(to) > TAURI_EVENT_FANOUT_CAP {
            continue;
        }
        for e in from {
            for l in to {
                if e.node.id == l.node.id || !same_scope(&e.scope, &l.scope) {
                    continue;
                }
                if !out.seen.insert(format!("{}>{}", e.node.id, l.node.id)) {
                    continue;
                }
                let mut metadata = meta! { "synthesizedBy" => "tauri-event", "channel" => "event" };
                if e.rust != l.rust {
                    metadata.insert("tier".into(), (if e.rust { TIER_SERVER_TO_CLIENT } else { TIER_CLIENT_TO_SERVER }).into());
                }
                metadata.insert("event".into(), (*event).into());
                metadata.insert("registeredAt".into(), format!("{}:{}", l.file, l.line).into());
                let mut edge = calls_edge(&e.node.id, &l.node.id, Some(e.line), metadata);
                edge.column = Some(e.column);
                edges.push(edge);
            }
        }
    }
    out.edges.extend(edges);
}

/// Whether this project has a Tauri core at all, from indexed lookups: a Rust `use tauri…` is an
/// import node, and a `#[tauri::command]` is a decorator.
fn has_tauri_core(ctx: &Ctx) -> bool {
    if ["tauri", "tauri_specta"]
        .iter()
        .any(|name| ctx.get_nodes_by_name(name).iter().any(|n| n.kind == NodeKind::Import && n.language == Language::Rust))
    {
        return true;
    }
    let mut found = false;
    let _ = ctx.store().for_each_node_by_language_with_decorator(Language::Rust, "tauri::command", |n| {
        found = n.kind == NodeKind::Function;
        !found
    });
    found
}

pub fn tauri_edges(ctx: &Ctx) -> Vec<Edge> {
    if !has_tauri_core(ctx) {
        return Vec::new();
    }
    let side = read_rust_side(ctx);
    let mut out = Collected { emitters: side.emitters.clone(), listeners: side.listeners.clone(), ..Default::default() };

    let frontend = Re::cached(FRONTEND_FILE, "");
    let frontend_files: Vec<String> = ctx.get_all_files().into_iter().filter(|f| frontend.is_match(f) && !is_test_path(f)).collect();
    let mut scans: IndexMap<String, FrontScan> = IndexMap::new();
    let scan_file = |file: &str, source: &str| -> FrontScan {
        let text = align(source, strip_comments_for_regex(source, "typescript"));
        let code = align(&text, blank_string_contents(&text, None));
        let language = language_of(file);
        let imports: Vec<Imp> = ctx
            .get_import_mappings(file, language)
            .iter()
            .map(|m| Imp { local: m.local_name.clone(), exported: m.exported_name.clone(), source: m.source.clone(), is_namespace: m.is_namespace })
            .collect();
        let front = FrontFile { file: file.to_string(), lines: Lines::new(source), text, code, nodes: ctx.get_nodes_in_file(file) };
        let bindings = read_bindings(&front, &imports, language, ctx);
        FrontScan { front, imports, language, bindings }
    };
    let gate = Re::cached(FRONTEND_GATE, "");
    for file in &frontend_files {
        if let Some(source) = ctx.read_file(file).filter(|s| gate.is_match(s)) {
            scans.insert(file.clone(), scan_file(file, &source));
        }
    }
    let forwarders = find_forwarders(&scans, ctx);
    if !forwarders.is_empty() {
        // A wrapper's callers need not mention Tauri.
        for file in &frontend_files {
            if scans.contains_key(file) {
                continue;
            }
            if let Some(source) = ctx.read_file(file).filter(|s| forwarders.iter().any(|f| s.contains(f.name.as_str()))) {
                scans.insert(file.clone(), scan_file(file, &source));
            }
        }
    }
    for scan in scans.values() {
        read_frontend(scan, &forwarders, &side, ctx, &mut out);
    }

    pair_events(&mut out);
    let subscribed = std::mem::take(&mut out.subscribed);
    for sub in subscribed.into_values() {
        let s = &sub.event_struct;
        let mut e = heuristic_edge(
            &sub.owner.id,
            &s.node.id,
            EdgeKind::References,
            Some(sub.line),
            meta! {
                "synthesizedBy" => "tauri-event", "channel" => "event", "event" => s.event.as_str(),
                "registeredAt" => format!("{}:{}", s.node.file_path, s.node.start_line),
            },
        );
        e.column = Some(sub.column);
        out.edges.push(e);
    }
    out.edges
}

/// Files this pass reads as input; a sync that changes one draws the links again.
pub fn has_tauri_pattern(file_path: &str, content: &str) -> bool {
    if file_path.ends_with(".rs") {
        return re!(r"\btauri|generate_handler|collect_commands|\.\s*(?:emit|listen|once)\w*\s*\(").is_match(content);
    }
    if !Re::cached(FRONTEND_FILE, "").is_match(file_path) {
        return false;
    }
    re!(r"@tauri-apps/|__TAURI|[iI]nvoke\w*\s*[<(]|\b(?:commands|events)\s*\.\s*\w").is_match(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cases_names_the_way_tauri_specta_does() {
        assert_eq!(to_camel("save_file"), "saveFile");
        assert_eq!(to_camel("AIModelReady"), "aiModelReady");
        assert_eq!(to_kebab("FileSaved"), "file-saved");
        assert_eq!(to_kebab("AIModelReady"), "ai-model-ready");
    }

    #[test]
    fn names_a_files_crate_and_modules() {
        let names = module_names("crates/my-api/src/cmd/network.rs");
        assert!(["my_api", "cmd", "network"].iter().all(|n| names.contains(*n)), "{names:?}");
        assert!(!names.contains("src"));
    }

    #[test]
    fn widens_blanked_characters_back_to_their_bytes() {
        let orig = "a\"é\"b";
        let blanked = "a\" \"b".to_string();
        assert_eq!(align(orig, blanked), "a\"  \"b");
    }

    #[test]
    fn splits_a_calls_arguments_at_the_top_level() {
        let code = b"f(a, g(b, c), [d, e])";
        assert_eq!(argument_spans(code, 1), vec![(2, 3), (4, 12), (13, 20)]);
    }
}
