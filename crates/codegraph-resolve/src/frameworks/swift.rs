//! Swift framework resolvers: SwiftUI, UIKit and Vapor (server-side Swift). Port of
//! `src/resolution/frameworks/swift.ts`.

use super::name_heuristic::{pick_by_name_and_kind, NamePickOptions};
use super::now_ms;
use crate::ctx::Ctx;
use crate::types::{FrameworkExtractionResult, FrameworkResolver, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_types::jscompat::{is_js_space, utf16_len, utf16_prefix};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use std::collections::{HashMap, HashSet};

const LANGUAGES: [Language; 1] = [Language::Swift];

// Directory patterns
const VIEW_DIRS: [&str; 5] = ["/Views/", "/View/", "/Screens/", "/Components/", "/UI/"];
const VIEWMODEL_DIRS: [&str; 5] = ["/ViewModels/", "/ViewModel/", "/Stores/", "/Managers/", "/Services/"];
const MODEL_DIRS: [&str; 4] = ["/Models/", "/Model/", "/Entities/", "/Domain/"];
const VC_DIRS: [&str; 4] = ["/ViewControllers/", "/ViewController/", "/Controllers/", "/Screens/"];
const UIVIEW_DIRS: [&str; 4] = ["/Views/", "/View/", "/UI/", "/Components/"];
const CELL_DIRS: [&str; 5] = ["/Cells/", "/Cell/", "/Views/", "/TableViewCells/", "/CollectionViewCells/"];
const VAPOR_CONTROLLER_DIRS: [&str; 3] = ["/Controllers/", "/Controller/", "/Routes/"];
const FLUENT_MODEL_DIRS: [&str; 4] = ["/Models/", "/Model/", "/Entities/", "/Database/"];
const VAPOR_MIDDLEWARE_DIRS: [&str; 2] = ["/Middleware/", "/Middlewares/"];

const SWIFT_TYPE_KINDS: [NodeKind; 4] = [NodeKind::Class, NodeKind::Struct, NodeKind::Enum, NodeKind::Protocol];

const VIEW_KINDS: [NodeKind; 1] = [NodeKind::Struct];
const CLASS_KINDS: [NodeKind; 1] = [NodeKind::Class];
const MODEL_KINDS: [NodeKind; 2] = [NodeKind::Struct, NodeKind::Class];
const PROTOCOL_KINDS: [NodeKind; 1] = [NodeKind::Protocol];
const VAPOR_CONTROLLER_KINDS: [NodeKind; 2] = [NodeKind::Class, NodeKind::Struct];

/// A framework name heuristic's pick (see `name_heuristic`), preferring these folders.
fn resolve_by_name_and_kind(r: &UnresolvedRef, kinds: &[NodeKind], preferred_dir_patterns: &[&str], ctx: &Ctx) -> Option<String> {
    pick_by_name_and_kind(r, kinds, &|f: &str| preferred_dir_patterns.iter().any(|d| f.contains(d)), ctx, &NamePickOptions::default())
}

fn line_of(text: &str, index: usize) -> u32 {
    text[..index].matches('\n').count() as u32 + 1
}

fn any_swift_file_contains(ctx: &Ctx, needles: &[&str]) -> bool {
    for file in ctx.get_all_files() {
        if file.ends_with(".swift") {
            if let Some(content) = ctx.read_file(&file).filter(|c| !c.is_empty()) {
                if needles.iter().any(|n| content.contains(n)) {
                    return true;
                }
            }
        }
    }
    false
}

// No extract(): a SwiftUI view is its own struct node, and a UIKit controller its class. A
// one-line `component`/`class` twin per `struct X: View` (and per `@main` app, view controller
// and UIView subclass) carried no edges and showed up in search and the symbol view as a dead-end
// duplicate.
pub struct SwiftUiResolver;

impl FrameworkResolver for SwiftUiResolver {
    fn name(&self) -> &str {
        "swiftui"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        // Check for SwiftUI imports in Swift files
        if any_swift_file_contains(ctx, &["import SwiftUI"]) {
            return true;
        }

        // Check for Xcode project with SwiftUI
        ctx.get_all_files().iter().any(|f| f.ends_with(".xcodeproj") || f.ends_with(".xcworkspace"))
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        // Swift's conventions, for Swift's refs: an Objective-C `@interface SDDiskCache :
        // NSObject <SDDiskCache>` is no SwiftUI view or model.
        if r.language != Language::Swift {
            return None;
        }
        let name = r.reference_name.as_str();
        // Pattern 1: View references (SwiftUI views are PascalCase ending in View)
        if name.ends_with("View") && name.starts_with(|c: char| c.is_ascii_uppercase()) {
            if let Some(result) = resolve_by_name_and_kind(r, &VIEW_KINDS, &VIEW_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.85, ResolvedBy::Framework));
            }
        }

        // Pattern 2: ViewModel/ObservableObject references
        if name.ends_with("ViewModel") || name.ends_with("Store") || name.ends_with("Manager") {
            if let Some(result) = resolve_by_name_and_kind(r, &CLASS_KINDS, &VIEWMODEL_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.85, ResolvedBy::Framework));
            }
        }

        // Pattern 3: Model references
        if re!(r"^[A-Z][a-zA-Z]+$").is_match(name) {
            if let Some(result) = resolve_by_name_and_kind(r, &MODEL_KINDS, &MODEL_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.7, ResolvedBy::Framework));
            }
        }

        None
    }
}

pub struct UikitResolver;

impl FrameworkResolver for UikitResolver {
    fn name(&self) -> &str {
        "uikit"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        any_swift_file_contains(ctx, &["import UIKit", "UIViewController", "UIView"])
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        // Swift's conventions, for Swift's refs: an Objective-C `@interface SDDiskCache :
        // NSObject <SDDiskCache>` is no SwiftUI view or model.
        if r.language != Language::Swift {
            return None;
        }
        let name = r.reference_name.as_str();
        // Pattern 1: ViewController references
        if name.ends_with("ViewController") {
            if let Some(result) = resolve_by_name_and_kind(r, &CLASS_KINDS, &VC_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.85, ResolvedBy::Framework));
            }
        }

        // Pattern 2: UIView subclass references
        if name.ends_with("View") && !name.ends_with("ViewController") {
            if let Some(result) = resolve_by_name_and_kind(r, &CLASS_KINDS, &UIVIEW_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.8, ResolvedBy::Framework));
            }
        }

        // Pattern 3: Cell references
        if name.ends_with("Cell") {
            if let Some(result) = resolve_by_name_and_kind(r, &CLASS_KINDS, &CELL_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.85, ResolvedBy::Framework));
            }
        }

        // Pattern 4: Delegate/DataSource references
        if name.ends_with("Delegate") || name.ends_with("DataSource") {
            if let Some(result) = resolve_by_name_and_kind(r, &PROTOCOL_KINDS, &[], ctx) {
                return Some(ResolvedRef::new(r, result, 0.8, ResolvedBy::Framework));
            }
        }

        None
    }
}

/// A Vapor route's handler ref: `Type@method` (the type may be dotted), or `@method`.
fn vapor_handler(name: &str) -> Option<(Option<String>, String)> {
    let caps = re!(r"^((?:[A-Za-z_]\w*\.)*[A-Za-z_]\w*)?@([A-Za-z_]\w*)$").captures(name)?;
    Some((caps.text(1).map(String::from), caps.text(2).unwrap_or("").to_string()))
}

pub struct VaporResolver;

impl FrameworkResolver for VaporResolver {
    fn name(&self) -> &str {
        "vapor"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        // Check for Package.swift with Vapor dependency
        if ctx.read_file("Package.swift").is_some_and(|p| !p.is_empty() && p.contains("vapor")) {
            return true;
        }

        // Check for Vapor imports
        any_swift_file_contains(ctx, &["import Vapor"])
    }

    // A route's handler ref (`SearchController@show`, `@index`) names no declared symbol, so
    // resolveOne's pre-filter would drop it before resolve() runs.
    fn claims_reference(&self, name: &str) -> bool {
        vapor_handler(name).is_some()
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        // Swift's conventions, for Swift's refs: an Objective-C `@interface SDDiskCache :
        // NSObject <SDDiskCache>` is no SwiftUI view or model.
        if r.language != Language::Swift {
            return None;
        }
        let name = r.reference_name.as_str();
        // Pattern 0: a route's handler — `use: SearchController.show` arrives as
        // `SearchController@show`, `use: self.index` / `use: index` as `@index`. Resolved on the
        // type the route names, never by the method's name alone: every controller has a `show`.
        // No match, or two, is left unresolved.
        if let Some((type_path, method)) = vapor_handler(name) {
            let target = resolve_vapor_handler(type_path, &method, r, ctx);
            return target.map(|id| ResolvedRef::new(r, id, 0.9, ResolvedBy::Framework));
        }

        // Pattern 1: Controller references
        if name.ends_with("Controller") {
            if let Some(result) = resolve_by_name_and_kind(r, &VAPOR_CONTROLLER_KINDS, &VAPOR_CONTROLLER_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.85, ResolvedBy::Framework));
            }
        }

        // Pattern 2: Model references (Fluent)
        if re!(r"^[A-Z][a-zA-Z]+$").is_match(name) {
            if let Some(result) = resolve_by_name_and_kind(r, &CLASS_KINDS, &FLUENT_MODEL_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.75, ResolvedBy::Framework));
            }
        }

        // Pattern 3: Middleware references
        if name.ends_with("Middleware") {
            if let Some(result) = resolve_by_name_and_kind(r, &VAPOR_CONTROLLER_KINDS, &VAPOR_MIDDLEWARE_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.8, ResolvedBy::Framework));
            }
        }

        None
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<FrameworkExtractionResult> {
        if !file_path.ends_with(".swift") {
            return Some(FrameworkExtractionResult::default());
        }
        let mut nodes: Vec<Node> = Vec::new();
        let mut references: Vec<UnresolvedRef> = Vec::new();
        let now = now_ms();
        let safe = codegraph_text::strip_comments_for_regex(content, "swift");
        let safe: &str = &safe;

        let route_node = |id: String, name: String, route_path: &str, line: u32, end_column: usize| {
            Node::new(id, NodeKind::Route, name, format!("{file_path}::route:{route_path}"), file_path, Language::Swift, line, line, 0, end_column as u32, now)
        };

        // Build a group-var → path-prefix map first. Modern Vapor routes live on a grouped
        // builder (`let todos = routes.grouped("todos"); todos.get(use: index)` or
        // `routes.group("todos") { todos in todos.get(use: index) }`), so the path comes from the
        // group, not the call. Roots (app/routes/router) have no prefix.
        let mut group_prefix: HashMap<String, String> = HashMap::new();
        // let X = Y.grouped("a", "b")
        let grouped_regex = re!(r"\blet\s+(\w+)\s*=\s*(\w+)\.grouped\s*\(([^)]*)\)", "g");
        for gm in grouped_regex.captures_iter(safe) {
            let existing = group_prefix.get(gm.text(2).unwrap_or("")).cloned().unwrap_or_default();
            group_prefix.insert(gm.text(1).unwrap_or("").to_string(), seg_join(&existing, gm.text(3).unwrap_or("")));
        }
        // Y.group("a") { X in ... }
        let group_closure_regex = re!(r"\b(\w+)\.group\s*\(([^)]*)\)\s*\{\s*(\w+)\s+in", "g");
        for gm in group_closure_regex.captures_iter(safe) {
            let existing = group_prefix.get(gm.text(1).unwrap_or("")).cloned().unwrap_or_default();
            group_prefix.insert(gm.text(3).unwrap_or("").to_string(), seg_join(&existing, gm.text(2).unwrap_or("")));
        }
        let prefix_of = |receiver: &str| group_prefix.get(receiver).cloned().unwrap_or_default();
        let or_root = |p: String| if p.is_empty() { "/".to_string() } else { p };

        // Vapor: <builder>.METHOD([path segs,] use: handler). Any receiver (app, routes, or a
        // grouped var); path segments optional and may be non-string (`BlogUser.parameter`,
        // `:id`, a path constant) so accept any comma-separated args before `use:` — the label
        // keeps only the string parts. `use:` discriminates a real route from
        // Environment.get("X")/req.parameters.get("X"). Each arg repetition must end at a comma,
        // and `,` is outside the char class, so the split is unique and matching stays linear.
        // The earlier `(?:[^,()]+,\s*)*` was ambiguous — the trailing `\s*` and the next
        // iteration's `[^,()]+` could both claim the same spaces — which backtracked
        // exponentially on a long arg list that never reaches `use:`. The tail is `\s*` rather
        // than a lazy `[^,()]*?` on purpose: both are linear, but the lazy form drops the "`use:`
        // is preceded by a comma" requirement and widens the match set — `req.get(foo.use: bar)`
        // would then be indexed as a route (groups `["req","get","foo.","bar"]`) where both this
        // pattern and the original match nothing.
        let route_regex = re!(r"\b(\w+)\.(get|post|put|patch|delete|head|options)\s*\(\s*((?:[^,()]+,)*\s*)use:\s*([A-Za-z_][\w.]*)", "g");
        // `let todos = TodoController()` — what a `use: todos.index` receiver is.
        let mut receiver_types: HashMap<String, String> = HashMap::new();
        let receiver_regex = re!(r"\b(?:let|var)\s+([a-z_]\w*)\s*(?::\s*([A-Z]\w*)\s*)?=\s*([A-Z][\w.]*)\s*\(", "g");
        for gm in receiver_regex.captures_iter(safe) {
            receiver_types.insert(gm.text(1).unwrap_or("").to_string(), gm.text(2).or(gm.text(3)).unwrap_or("").to_string());
        }
        for m in route_regex.captures_iter(safe) {
            let Some(whole) = m.get(0) else { continue };
            let (receiver, method, segs_str, handler_expr) = (m.text(1).unwrap_or(""), m.text(2).unwrap_or(""), m.text(3).unwrap_or(""), m.text(4).unwrap_or(""));
            let line = line_of(safe, whole.start());
            let upper = method.to_uppercase();
            let route_path = or_root(prefix_of(receiver) + &seg_join("", segs_str));

            let node = route_node(format!("route:{file_path}:{line}:{upper}:{route_path}"), format!("{upper} {route_path}"), &route_path, line, utf16_len(whole.as_str()));
            let id = node.id.clone();
            nodes.push(node);

            if let Some(handler_name) = vapor_handler_ref(handler_expr, &receiver_types) {
                references.push(UnresolvedRef::new(id, handler_name, EdgeKind::References, line, 0, file_path, Language::Swift));
            }
        }

        // `routes.on(.POST, "x", use: handler)` names its method as the first argument.
        // Arguments may hold one level of parentheses (`body: .collect(maxSize: "1mb")`); only
        // unlabeled string arguments are path segments.
        let on_regex = re!(r"\b(\w+)\.on\s*\(\s*\.([A-Z]+)\s*,\s*((?:(?:[^,()]|\([^()]*\))+,)*\s*)use:\s*([A-Za-z_][\w.]*)", "g");
        for m in on_regex.captures_iter(safe) {
            let Some(whole) = m.get(0) else { continue };
            let (receiver, method, segs_str, handler_expr) = (m.text(1).unwrap_or(""), m.text(2).unwrap_or(""), m.text(3).unwrap_or(""), m.text(4).unwrap_or(""));
            let line = line_of(safe, whole.start());
            let route_path = or_root(prefix_of(receiver) + &seg_join("", &path_args(segs_str)));
            let id = format!("route:{file_path}:{line}:{method}:{route_path}");
            nodes.push(route_node(id.clone(), format!("{method} {route_path}"), &route_path, line, utf16_len(whole.as_str())));
            if let Some(handler_name) = vapor_handler_ref(handler_expr, &receiver_types) {
                references.push(UnresolvedRef::new(id, handler_name, EdgeKind::References, line, 0, file_path, Language::Swift));
            }
        }

        // A route whose handler is a trailing closure — `app.get("hello") { req in … }`,
        // `app.webSocket("chat") { req, ws in … }`, `routes.on(.GET, "x") { … }`. It has no
        // handler symbol; the closure's calls belong to the function registering it. An HTTP
        // client's `req.client.get("https://…") { … }` is not a route.
        let closure_regex = re!(r"\b(\w+)\.(get|post|put|patch|delete|head|options|webSocket|on)\s*\(([^()]*)\)\s*\{", "g");
        for m in closure_regex.captures_iter(safe) {
            let Some(whole) = m.get(0) else { continue };
            let (receiver, verb, args) = (m.text(1).unwrap_or(""), m.text(2).unwrap_or(""), m.text(3).unwrap_or(""));
            if re!(r"\buse:").is_match(args) || receiver == "client" || re!(r#"^\s*"https?:"#).is_match(args) {
                continue;
            }
            // A route registration is a statement: `if let v = req.parameters.get("x") {` opens
            // the `if` body, not a trailing closure.
            let line_start = safe[..whole.start()].rfind('\n').map(|i| i + 1).unwrap_or(0);
            if !re!(r"^\s*(?:(?:try|await)\s+)*$").is_match(&safe[line_start..whole.start()]) {
                continue;
            }
            let mut method = if verb == "webSocket" { "WS".to_string() } else { verb.to_uppercase() };
            let mut segs = args.to_string();
            if verb == "on" {
                let Some(on) = re!(r"^\s*\.([A-Z]+)\s*,?(.*)$", "s").captures(args) else { continue };
                method = on.text(1).unwrap_or("").to_string();
                segs = on.text(2).unwrap_or("").to_string();
            }
            let line = line_of(safe, whole.start());
            let route_path = or_root(prefix_of(receiver) + &seg_join("", &segs));
            let id = format!("route:{file_path}:{line}:{method}:{route_path}");
            nodes.push(route_node(id.clone(), format!("{method} {route_path}"), &route_path, line, utf16_len(whole.as_str())));
            // The closure IS the handler: its calls are the route's, as an Express inline
            // handler's are, so Steps draws what `GET hello` does.
            let open = whole.end() - 1;
            if let Some(close) = closing_brace(safe, open).filter(|close| *close > open) {
                for name in closure_call_names(&safe[open + 1..close]) {
                    references.push(UnresolvedRef::new(id.clone(), name, EdgeKind::Calls, line, 0, file_path, Language::Swift));
                }
            }
        }

        Some(FrameworkExtractionResult { nodes, references })
    }
}

/// `existing + segs.map(s => '/' + s).join('')` over the quoted strings in `segs_str`.
fn seg_join(existing: &str, segs_str: &str) -> String {
    let mut out = existing.to_string();
    for m in re!(r#""([^"]*)""#, "g").captures_iter(segs_str) {
        out.push('/');
        out.push_str(m.text(1).unwrap_or(""));
    }
    out
}

fn path_args(arg_text: &str) -> String {
    arg_text.split(',').filter(|a| re!(r#"^\s*"[^"]*"\s*$"#).is_match(a)).collect::<Vec<_>>().join(",")
}

/// The `}` matching the `{` at `open`; string literals are skipped. `None` when unbalanced.
fn closing_brace(s: &str, open: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut i = open;
    while i < b.len() {
        let ch = b[i];
        if ch == b'"' {
            i += 1;
            while i < b.len() && b[i] != b'"' {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if ch == b'{' {
            depth += 1;
        } else if ch == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Words Swift writes before a `(` that are not calls.
const SWIFT_NOT_CALLS: [&str; 14] = ["if", "guard", "switch", "while", "for", "return", "catch", "case", "in", "try", "await", "throw", "some", "any"];

/// The calls a route closure's body makes, each once, keeping the receiver (`Todo.query`,
/// `req.auth.require`) so they resolve as calls on it. A member of an expression (`a.b().c(`)
/// names nothing this can follow.
fn closure_call_names(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let call_re = re!(r"((?:[A-Za-z_]\w*\s*[?!]?\.\s*)*)([A-Za-z_]\w*)\s*\(", "g");
    for m in call_re.captures_iter(body) {
        let Some(whole) = m.get(0) else { continue };
        let callee = m.text(2).unwrap_or("");
        let receiver: String = m.text(1).unwrap_or("").chars().filter(|c| !is_js_space(*c) && *c != '?' && *c != '!').collect();
        let receiver = receiver.strip_suffix('.').unwrap_or(&receiver).to_string();
        if receiver.is_empty() {
            let before = body[..whole.start()].trim_end_matches(is_js_space);
            if SWIFT_NOT_CALLS.contains(&callee) || before.ends_with(['.', ')', ']']) {
                continue;
            }
        }
        let name = if receiver.is_empty() { callee.to_string() } else { format!("{receiver}.{callee}") };
        if !seen.insert(name.clone()) {
            continue;
        }
        out.push(name);
    }
    out
}

/// The handler ref for `use: <expr>`, keeping the type the method is on:
/// `API.PackageController.get` → `API.PackageController@get`; `self.index` and a bare `index` →
/// `@index` (the type the route is written in); `todos.index` → the type `todos` was made from,
/// when the file says.
fn vapor_handler_ref(expr: &str, receiver_types: &HashMap<String, String>) -> Option<String> {
    let mut segs: Vec<&str> = expr.split('.').filter(|s| !s.is_empty()).collect();
    let method = segs.pop()?;
    if segs.first().is_some_and(|s| *s == "self" || *s == "Self") {
        segs.remove(0);
    }
    if segs.is_empty() {
        return Some(format!("@{method}"));
    }
    if segs[0].starts_with(|c: char| c.is_ascii_uppercase()) {
        return Some(format!("{}@{method}", segs.join(".")));
    }
    let ty = if segs.len() == 1 { receiver_types.get(segs[0]) } else { None };
    Some(match ty {
        Some(ty) if !ty.is_empty() => format!("{ty}@{method}"),
        _ => format!("@{method}"),
    })
}

fn file_lines(ctx: &Ctx, file_path: &str) -> Vec<String> {
    match ctx.read_file(file_path) {
        Some(content) => re!(r"\r?\n").split(&content).into_iter().map(String::from).collect(),
        None => Vec::new(),
    }
}

/// One type's method among `candidates`, or `None` when they belong to different types. Of a
/// type's overloads, the one declared to take a `Request` — what Vapor calls a handler with —
/// else the earliest.
fn one_owners_method(candidates: &[&Node], owner_of: &dyn Fn(&Node) -> String, ctx: &Ctx) -> Option<String> {
    if candidates.is_empty() {
        return None;
    }
    let owners: HashSet<String> = candidates.iter().map(|n| owner_of(n)).collect();
    if owners.len() > 1 {
        return None;
    }
    let earliest = |nodes: &[&Node]| {
        let mut best = nodes[0];
        for n in &nodes[1..] {
            if best.start_line > n.start_line {
                best = n;
            }
        }
        best.id.clone()
    };
    if candidates.len() == 1 {
        return Some(candidates[0].id.clone());
    }
    let takes_request: Vec<&Node> = candidates
        .iter()
        .copied()
        .filter(|n| {
            let lines = file_lines(ctx, &n.file_path);
            let from = (n.start_line as usize).saturating_sub(1).min(lines.len());
            let to = (n.end_line.min(n.start_line + 2) as usize).min(lines.len());
            let head = if from < to { lines[from..to].join(" ") } else { String::new() };
            re!(r"\(\s*(?:\w+\s+)?\w+\s*:\s*Request\b").is_match(&head)
        })
        .collect();
    Some(earliest(if takes_request.is_empty() { candidates } else { &takes_request }))
}

/// The innermost type among `candidates` (later wins a tie on start line).
fn innermost<'a>(nodes: impl Iterator<Item = &'a Node>) -> Option<&'a Node> {
    let mut inner: Option<&Node> = None;
    for n in nodes {
        if inner.is_none_or(|i| n.start_line >= i.start_line) {
            inner = Some(n);
        }
    }
    inner
}

fn resolve_vapor_handler(type_path: Option<String>, method: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let all = ctx.get_nodes_by_name(method);
    let callables: Vec<&Node> = all.iter().map(|n| &**n).filter(|n| matches!(n.kind, NodeKind::Method | NodeKind::Function) && n.language == Language::Swift).collect();
    if callables.is_empty() {
        return None;
    }
    let method_len = utf16_len(method);
    let owner_of = move |n: &Node| -> String {
        let keep = utf16_len(&n.qualified_name).saturating_sub(method_len + 2);
        utf16_prefix(&n.qualified_name, keep).to_string()
    };

    let type_path = match type_path {
        Some(t) => t,
        None => {
            // `use: self.index` / `use: index`: the type whose body holds the route — its own
            // method in this file, else one declared in another extension of it.
            let in_file = ctx.get_nodes_in_file(&r.file_path);
            let owner = innermost(in_file.iter().map(|n| &**n).filter(|n| SWIFT_TYPE_KINDS.contains(&n.kind) && n.start_line <= r.line && n.end_line >= r.line));
            let Some(owner) = owner else {
                // A route in a top-level `func routes(_ app:)` names a function in scope.
                let functions: Vec<&Node> = callables.iter().copied().filter(|n| n.kind == NodeKind::Function).collect();
                let same_file: Vec<&Node> = functions.iter().copied().filter(|n| n.file_path == r.file_path).collect();
                if !same_file.is_empty() {
                    return one_owners_method(&same_file, &owner_of, ctx);
                }
                return if functions.len() == 1 { Some(functions[0].id.clone()) } else { None };
            };
            let own: Vec<&Node> = callables.iter().copied().filter(|n| n.file_path == r.file_path && n.start_line >= owner.start_line && n.end_line <= owner.end_line).collect();
            if !own.is_empty() {
                return one_owners_method(&own, &owner_of, ctx);
            }
            owner.qualified_name.replace("::", ".")
        }
    };

    let segs: Vec<&str> = type_path.split('.').collect();
    let ty = segs[segs.len() - 1];
    let full = segs.join("::");
    let exact: Vec<&Node> = callables.iter().copied().filter(|n| owner_of(n) == full).collect();
    if !exact.is_empty() {
        return one_owners_method(&exact, &owner_of, ctx);
    }
    let nested_suffix = format!("::{full}");
    let nested: Vec<&Node> = callables.iter().copied().filter(|n| owner_of(n).ends_with(&nested_suffix)).collect();
    if !nested.is_empty() {
        return one_owners_method(&nested, &owner_of, ctx);
    }
    if segs.len() > 1 {
        // `extension API.PackageController { static func get }` names its node by the last
        // segment (`PackageController::get`) — the same QN a top-level `PackageController`'s
        // method has, so the extension's own line decides.
        let declared = Re::try_cached(&format!(r"\bextension\s+{}\b", segs.join(r"\s*\.\s*")), "")?;
        let in_extension: Vec<&Node> = callables
            .iter()
            .copied()
            .filter(|n| {
                if owner_of(n) != ty {
                    return false;
                }
                let in_file = ctx.get_nodes_in_file(&n.file_path);
                let Some(owner) = innermost(in_file.iter().map(|o| &**o).filter(|o| SWIFT_TYPE_KINDS.contains(&o.kind) && o.name == ty && o.start_line <= n.start_line && o.end_line >= n.end_line)) else {
                    return false;
                };
                // The node starts at its attributes (`@available(…)`), the keyword a line or two on.
                let lines = file_lines(ctx, &n.file_path);
                let from = (owner.start_line as usize).saturating_sub(1).min(lines.len());
                let to = ((owner.start_line + 2) as usize).min(lines.len());
                let head = if from < to { lines[from..to].join(" ") } else { String::new() };
                declared.is_match(&head)
            })
            .collect();
        return one_owners_method(&in_extension, &owner_of, ctx);
    }
    // A type named without the namespace it is nested in — only when one type fits.
    let type_suffix = format!("::{ty}");
    let fits: Vec<&Node> = callables.iter().copied().filter(|n| owner_of(n).ends_with(&type_suffix)).collect();
    one_owners_method(&fits, &owner_of, ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vapor_routes_groups_and_closures() {
        let src = "func routes(_ app: Application) throws {\n  let todos = app.grouped(\"todos\")\n  let c = TodoController()\n  todos.get(use: c.index)\n  todos.post(\":id\", use: TodoController.create)\n  app.on(.PUT, \"x\", use: update)\n  app.get(\"hello\") { req in\n    return Todo.query(on: req.db)\n  }\n}\n";
        let result = VaporResolver.extract("Sources/routes.swift", src).unwrap();
        let names: Vec<&str> = result.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["GET /todos", "POST /todos/:id", "PUT /x", "GET /hello"]);
        let refs: Vec<(&str, bool)> = result.references.iter().map(|r| (r.reference_name.as_str(), r.is_kind(EdgeKind::Calls))).collect();
        assert_eq!(refs, vec![("TodoController@index", false), ("TodoController@create", false), ("@update", false), ("Todo.query", true)]);
    }

    #[test]
    fn handler_ref_shapes() {
        let types = HashMap::new();
        assert_eq!(vapor_handler_ref("self.index", &types).as_deref(), Some("@index"));
        assert_eq!(vapor_handler_ref("API.PackageController.get", &types).as_deref(), Some("API.PackageController@get"));
        assert!(vapor_handler("Foo.Bar@baz").is_some());
        assert!(vapor_handler("plain").is_none());
    }

    #[test]
    fn braces_skip_strings() {
        let s = "x { a(\"}\") { } } y";
        assert_eq!(closing_brace(s, 2), Some(15));
    }
}
