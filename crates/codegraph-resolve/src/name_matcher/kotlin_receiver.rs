//! A Kotlin call through a property or a receiver chain resolves on the receiver's declared type
//! (upstream 01e0ac3a, PR #1933 by danusha2345).
//!
//! Kotlin indexes a class property without a signature and a primary-constructor property
//! (`class A(private val repo: Repo)`) is no node at all, so the declared type is read from the
//! declaration itself: a property's own lines, the class header for a constructor property, a
//! callee's declared return type for a local bound to a call, an enum entry's enum, each segment
//! of a receiver chain in turn, a superclass's property, and an outer function's local inside an
//! anonymous object. A receiver whose type the project does not declare (`Regex`, `Thread`, a JDK
//! class) gets no edge instead of a name-only guess, unless the project extends that type.

use super::infer::infer_local_receiver_type;
use super::method::resolve_method_on_type;
use super::same_language_family;
use crate::ctx::Ctx;
use crate::types::{NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::js_trim;
use codegraph_types::{Language, Node, NodeKind};
use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;

/// Stands for "a type the project does not declare" — a library class, or the result of a call on
/// one — so the caller can refuse a name-only guess.
pub const KOTLIN_EXTERNAL_TYPE: &str = "\u{0}external";

/// Kotlin stdlib functions whose result is always a library type.
const KOTLIN_LIBRARY_FACTORIES: &[&str] = &[
    "listOf", "mutableListOf", "arrayListOf", "emptyList", "listOfNotNull",
    "setOf", "mutableSetOf", "hashSetOf", "linkedSetOf", "sortedSetOf", "emptySet",
    "mapOf", "mutableMapOf", "hashMapOf", "linkedMapOf", "sortedMapOf", "emptyMap",
    "arrayOf", "arrayOfNulls", "emptyArray", "intArrayOf", "longArrayOf", "byteArrayOf",
    "sequenceOf", "emptySequence", "buildList", "buildSet", "buildMap", "buildString",
    "lazy", "thread", "mutableStateOf", "Channel", "MutableStateFlow", "MutableSharedFlow",
];

/// Stdlib functions that return their (non-null) argument: `requireNotNull(x)`.
const KOTLIN_ARGUMENT_PASSTHROUGH: &[&str] = &["requireNotNull", "checkNotNull"];

/// Node kinds that declare a type a receiver can have.
fn is_declared_type_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Class | NodeKind::Interface | NodeKind::Struct | NodeKind::Enum | NodeKind::TypeAlias | NodeKind::Trait | NodeKind::Protocol | NodeKind::Union)
}

fn is_property_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Field | NodeKind::Constant | NodeKind::Variable)
}

/// How deep the declared-type readers may call each other. A property read through an alias of a
/// property that is an alias of the first (`val a = b` / `val b = a`) is no Kotlin that compiles,
/// but it must not take the indexer down with a stack overflow.
const MAX_DECLARED_DEPTH: u32 = 16;

thread_local! {
    static DECLARED_DEPTH: Cell<u32> = const { Cell::new(0) };
}

struct DepthGuard;

impl DepthGuard {
    fn enter() -> Option<DepthGuard> {
        DECLARED_DEPTH.with(|d| {
            if d.get() >= MAX_DECLARED_DEPTH {
                return None;
            }
            d.set(d.get() + 1);
            Some(DepthGuard)
        })
    }
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        DECLARED_DEPTH.with(|d| d.set(d.get() - 1));
    }
}

/// Whether the project declares `type_name` (simple, or `Outer::Inner`) in the ref's language
/// family. False for library types (`Regex`, `Properties`).
pub fn is_project_type(type_name: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let simple = type_name.rsplit("::").next().unwrap_or(type_name);
    let nested = type_name.contains("::");
    let suffix = format!("::{type_name}");
    ctx.get_nodes_by_name(simple).iter().any(|n| {
        is_declared_type_kind(n.kind) && same_language_family(n.language, r.language) && (!nested || n.qualified_name == type_name || n.qualified_name.ends_with(&suffix))
    })
}

/// A Kotlin type written in source (`Lease?`, `HardwareLock.Lease`, `List<Foo>`) as a name
/// `resolve_method_on_type` matches: nullability and type arguments dropped, a nested type joined
/// with `::`, a package prefix (lowercase segments) dropped. `None` when it is not a type name.
fn kotlin_type_name(raw: &str) -> Option<String> {
    let no_args = re!(r"<[^>]*>", "g").replace_all_str(raw, "");
    let cleaned = js_trim(&no_args.replace('?', "")).to_string();
    let segments: Vec<&str> = cleaned.split('.').filter(|s| !s.is_empty()).collect();
    let mut first = segments.len();
    while first > 0 && re!(r"^[A-Z]\w*$").is_match(segments[first - 1]) {
        first -= 1;
    }
    let type_segments = &segments[first..];
    (!type_segments.is_empty()).then(|| type_segments.join("::"))
}

/// A file's lines through the context's per-file cache; `None` when unreadable.
fn kotlin_file_lines(file_path: &str, ctx: &Ctx) -> Option<Rc<Vec<String>>> {
    let lines = ctx.get_file_lines(file_path).or_else(|| ctx.read_file(file_path).map(|s| Rc::new(re!(r"\r?\n").split(&s).into_iter().map(String::from).collect())))?;
    (!lines.is_empty()).then_some(lines)
}

/// A Kotlin class's lines before its first member: its annotations, name, primary constructor and
/// supertype list.
fn kotlin_class_header(owner: &Node, in_file: &[NodeRef], lines: &[String]) -> String {
    let owner_end = owner.end_line;
    let mut first_member = owner_end + 1;
    for n in in_file {
        if n.id == owner.id || n.kind == NodeKind::File {
            continue;
        }
        if n.start_line > owner.start_line && n.start_line <= owner_end && n.start_line < first_member {
            first_member = n.start_line;
        }
    }
    let from = (owner.start_line as usize).saturating_sub(1).min(lines.len());
    let to = (first_member as usize).saturating_sub(1).min(lines.len());
    if from >= to {
        return String::new();
    }
    lines[from..to].join("\n")
}

/// Whether `s` starts with the word `where` at a word boundary.
fn starts_with_where(s: &[u8]) -> bool {
    s.starts_with(b"where") && s.get(5).map_or(true, |c| !(c.is_ascii_alphanumeric() || *c == b'_'))
}

/// The supertype names a Kotlin class header lists after its primary constructor (`class A(…) :
/// Base(…), Iface<T>, Api by impl {`), as [`kotlin_type_name`] gives them. Colons and commas
/// inside the constructor or type arguments are skipped; a `where` clause or the body ends the
/// list.
fn kotlin_supertype_names(header: &str, class_name: &str) -> Vec<String> {
    let decl = Re::cached(&format!(r"\b(?:class|object|interface)\s+{}\b", escape_regex(class_name)), "");
    let Some(decl) = decl.find(header) else { return Vec::new() };
    let bytes = header.as_bytes();
    let mut entries: Vec<&str> = Vec::new();
    let mut paren = 0i32;
    let mut angle = 0i32;
    let mut start: Option<usize> = None;
    let mut i = decl.end();
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'(' {
            paren += 1;
        } else if c == b')' {
            paren -= 1;
        } else if paren > 0 {
            i += 1;
            continue;
        } else if c == b'<' {
            angle += 1;
        } else if c == b'>' {
            angle -= 1;
        } else if angle > 0 {
            i += 1;
            continue;
        } else if c == b'{' {
            break;
        } else if start.is_some() && c == b'w' && starts_with_where(&bytes[i..]) && !bytes.get(i.wrapping_sub(1)).is_some_and(|p| p.is_ascii_alphanumeric() || *p == b'_') {
            break;
        } else if c == b':' && start.is_none() {
            start = Some(i + 1);
        } else if c == b',' && start.is_some() {
            entries.push(&header[start.unwrap()..i]);
            start = Some(i + 1);
        }
        i += 1;
    }
    if let Some(s) = start {
        entries.push(&header[s..i.min(bytes.len())]);
    }
    entries
        .into_iter()
        .filter_map(|entry| {
            let m = re!(r"^\s*(?:@\w+\s+)*([A-Za-z_][\w.]*)").captures(entry)?;
            kotlin_type_name(m.text(1)?)
        })
        .collect()
}

/// The tightest Kotlin class or interface enclosing `r`'s line.
fn kotlin_enclosing_class(r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    let mut best: Option<NodeRef> = None;
    for n in ctx.get_nodes_in_file(&r.file_path).iter() {
        if matches!(n.kind, NodeKind::Class | NodeKind::Interface) && n.language == Language::Kotlin && n.start_line <= r.line && n.end_line >= r.line && best.as_ref().map_or(true, |b| n.start_line > b.start_line) {
            best = Some(n.clone());
        }
    }
    best
}

/// The declared type of Kotlin property `name` in class `owner`: from the property's own
/// declaration lines when it is a node (`lateinit var repo: Repo`, `val repo = Repo(…)`),
/// otherwise from the class header, where a primary-constructor property lives. The header is the
/// class's lines before its first member, so a same-named local inside a method body is never read
/// as the property. `None` when no declaration names a type; `resolve_method_on_type` still
/// validates the method.
pub fn infer_kotlin_property_type(name: &str, property: Option<&Node>, owner: &Node, in_file: &[NodeRef], r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let lines = kotlin_file_lines(&r.file_path, ctx)?;
    if let Some(p) = property {
        let from = (p.start_line as usize).saturating_sub(1).min(lines.len());
        let to = (p.end_line as usize).min(lines.len());
        let text = if from < to { lines[from..to].join("\n") } else { String::new() };
        return kotlin_declared_type_in(&text, name, r, ctx, 0);
    }
    kotlin_declared_type_in(&kotlin_class_header(owner, in_file, &lines), name, r, ctx, 0)
}

/// The declared type of Kotlin property `name` if class `owner` declares it — as a member node or
/// a primary-constructor `val`/`var` — read in the owner's own file. `Undefined` when the owner
/// does not declare it; `Null` when it does without a type the declaration names.
fn kotlin_own_property_type(name: &str, owner: &Node, r: &UnresolvedRef, ctx: &Ctx) -> Tri<String> {
    let in_file = ctx.get_nodes_in_file(&owner.file_path);
    let qualified = format!("{}::{name}", owner.qualified_name);
    let property = in_file.iter().find(|n| is_property_kind(n.kind) && n.qualified_name == qualified).cloned();
    if property.is_none() {
        let declares = Re::cached(&format!(r"\b(?:val|var)\s+{}\b", escape_regex(name)), "");
        match kotlin_file_lines(&owner.file_path, ctx) {
            Some(lines) if declares.is_match(&kotlin_class_header(owner, &in_file, &lines)) => {}
            _ => return Tri::Undefined,
        }
    }
    // Read the declaration in the owner's file, as if the ref sat there.
    let mut at = r.clone();
    at.file_path = owner.file_path.clone();
    at.line = property.as_ref().map_or(owner.start_line, |p| p.start_line);
    match infer_kotlin_property_type(name, property.as_deref(), owner, &in_file, &at, ctx) {
        Some(t) => Tri::Value(t),
        None => Tri::Null,
    }
}

/// The declared type of Kotlin property `name` that class `owner` inherits: the nearest project
/// supertype (breadth-first, at most four levels) declaring it, with supertypes read from each
/// class header. A supertype the project does not declare, or whose name is ambiguous, is not
/// walked — the property may come from it — so the result is then `None` (unknown), never external.
fn kotlin_inherited_property_type(name: &str, owner: &NodeRef, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let mut seen: HashSet<String> = HashSet::from([owner.id.clone()]);
    let mut level: Vec<NodeRef> = vec![owner.clone()];
    for _ in 0..4 {
        if level.is_empty() {
            break;
        }
        let mut next: Vec<NodeRef> = Vec::new();
        for cls in &level {
            let Some(lines) = kotlin_file_lines(&cls.file_path, ctx) else { continue };
            let header = kotlin_class_header(cls, &ctx.get_nodes_in_file(&cls.file_path), &lines);
            for super_name in kotlin_supertype_names(&header, &cls.name) {
                let simple = super_name.rsplit("::").next().unwrap_or(&super_name);
                let suffix = format!("::{super_name}");
                let supers: Vec<NodeRef> = ctx
                    .get_nodes_by_name(simple)
                    .iter()
                    .filter(|n| matches!(n.kind, NodeKind::Class | NodeKind::Interface) && n.language == Language::Kotlin && (n.qualified_name == super_name || n.qualified_name.ends_with(&suffix)))
                    .cloned()
                    .collect();
                if supers.len() != 1 || seen.contains(&supers[0].id) {
                    continue;
                }
                let sup = supers[0].clone();
                seen.insert(sup.id.clone());
                match kotlin_own_property_type(name, &sup, r, ctx) {
                    Tri::Value(t) => return Some(t),
                    Tri::Null => return None,
                    Tri::Undefined => {}
                }
                next.push(sup);
            }
        }
        level = next;
    }
    None
}

/// The container of a qualified name as TS slices it: everything before the last `::`, or — when
/// there is none — the name without its last character (`slice(0, -1)`).
fn container_of(qualified_name: &str) -> &str {
    match qualified_name.rfind("::") {
        Some(i) => &qualified_name[..i],
        None => {
            let mut end = qualified_name.len();
            while end > 0 && !qualified_name.is_char_boundary(end - 1) {
                end -= 1;
            }
            &qualified_name[..end.saturating_sub(1)]
        }
    }
}

/// The type a Kotlin declaration of `name` in `text` gives it: `name: Type`, a constructor call
/// `name = Type(…)`, or a call `name = Owner.fn(…)` / `name = fn(…)`, typed by `fn`'s declared
/// return type. [`KOTLIN_EXTERNAL_TYPE`] when the owner is not a project type; `None` when nothing
/// names a type.
fn kotlin_declared_type_in(text: &str, name: &str, r: &UnresolvedRef, ctx: &Ctx, depth: u32) -> Option<String> {
    let rn = escape_regex(name);
    let typed = Re::cached(&format!(r"\b(?:val|var)\s+{rn}\s*:\s*([A-Z][\w.]*)"), "");
    if let Some(m) = typed.captures(text) {
        if let Some(t) = m.text(1) {
            return kotlin_type_name(t);
        }
    }
    // An alias of another value: `val old = ws`, `val s = current ?: return`, `val s = current!!` —
    // typed as that value.
    let alias = Re::cached(&format!(r"\b(?:val|var)\s+{rn}\s*=\s*([a-z_]\w*)\s*(?:!!|\?:\s*(?:return|continue|break|throw)\b[^\n]*)?\s*$"), "m");
    if let Some(m) = alias.captures(text) {
        if let Some(a) = m.text(1) {
            if a != name && depth < 3 {
                return kotlin_receiver_declared_type(a, r, ctx, depth + 1);
            }
        }
    }
    // `(` or a trailing lambda `{`: `Thread { … }`, `thread { … }`. Type arguments are skipped:
    // `WebRtcPlaneSession<IceCandidate>(plane)`.
    let assigned = Re::cached(&format!(r"\b(?:val|var)\s+{rn}\s*=\s*([A-Za-z_][\w.]*)\s*(?:<[\w\s.,?*<>]*>\s*)?[({{]"), "");
    let assigned = assigned.captures(text).and_then(|m| m.text(1));
    let Some(assigned) = assigned else {
        // An enum entry or other constant of a type: `var mode = GpsMode.ON`.
        let entry = Re::cached(&format!(r"\b(?:val|var)\s+{rn}\s*=\s*([A-Z]\w*(?:\.[A-Z]\w*)*)\.[A-Z][A-Z0-9_]*\b(?!\s*[.(])"), "");
        let written = entry.captures(text).and_then(|m| m.text(1))?;
        // Only an entry of a project enum names its type; `Limits.MAX` is an Int.
        let ty = kotlin_type_name(written);
        let simple = ty.as_deref().and_then(|t| t.rsplit("::").next());
        return match simple {
            Some(s) if ctx.get_nodes_by_name(s).iter().any(|n| n.kind == NodeKind::Enum && same_language_family(n.language, r.language)) => ty,
            _ => None,
        };
    };
    let mut segments: Vec<&str> = assigned.split('.').collect();
    let last = segments.pop().unwrap_or("");
    if last.starts_with(|c: char| c.is_ascii_uppercase()) {
        return kotlin_type_name(assigned);
    }
    // A call. Only an owner written as a type chain (`Lock.tryBegin`) or none at all
    // (`beginOp()`) can be typed; `a.b.fn()` goes through values.
    if !segments.is_empty() && !segments.iter().all(|seg| seg.starts_with(|c: char| c.is_ascii_uppercase())) {
        return None;
    }
    if segments.is_empty() && KOTLIN_ARGUMENT_PASSTHROUGH.contains(&last) {
        let arg = Re::cached(&format!(r"\b{}\s*\(\s*([A-Za-z_]\w*)\s*[,)]", escape_regex(last)), "");
        let arg = arg.captures(text).and_then(|m| m.text(1))?;
        if arg == name {
            return None;
        }
        // The argument is a parameter or local of the same scope.
        let arg_type = infer_local_receiver_type(arg, r, ctx, None)?;
        return arg_type.starts_with(|c: char| c.is_ascii_uppercase()).then_some(arg_type);
    }
    let owner = (!segments.is_empty()).then(|| segments.join("::"));
    kotlin_qualified_call_result_type(owner.as_deref(), last, r, ctx)
}

/// The declared return type of Kotlin `owner.fn` (or of an unqualified `fn`, a method of the ref's
/// own class first, then one in the same file). Every candidate must declare the same return type.
/// A return type nested in the callee's owner keeps that owner (`Lease` in `HardwareLock` →
/// `HardwareLock::Lease`). [`KOTLIN_EXTERNAL_TYPE`] for a call on a type the project does not
/// declare (`Executors.newSingleThreadScheduledExecutor()`).
fn kotlin_qualified_call_result_type(owner: Option<&str>, fn_name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    if let Some(o) = owner {
        if !is_project_type(o, r, ctx) {
            return Some(KOTLIN_EXTERNAL_TYPE.to_string());
        }
    }
    let mut candidates: Vec<NodeRef> = ctx
        .get_nodes_by_name(fn_name)
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Method | NodeKind::Function) && n.language == Language::Kotlin && n.signature.is_some())
        .cloned()
        .collect();
    if let Some(o) = owner {
        let suffix = format!("{o}::{fn_name}");
        candidates.retain(|n| n.qualified_name.ends_with(&suffix));
    } else {
        let own_class = kotlin_enclosing_type_of_call(r, ctx);
        let in_own_class: Vec<NodeRef> = match &own_class {
            Some(c) => {
                let qualified = format!("{}::{fn_name}", c.qualified_name);
                candidates.iter().filter(|n| n.qualified_name == qualified).cloned().collect()
            }
            None => Vec::new(),
        };
        let in_file: Vec<NodeRef> = candidates.iter().filter(|n| n.file_path == r.file_path).cloned().collect();
        candidates = if !in_own_class.is_empty() {
            in_own_class
        } else if !in_file.is_empty() {
            in_file
        } else {
            candidates
        };
    }
    // No project function of that name. A known stdlib factory returns a library type; any other
    // library function (`requireNotNull`, `let`) may return a project value, so its result stays
    // unknown.
    if candidates.is_empty() {
        return (owner.is_none() && KOTLIN_LIBRARY_FACTORIES.contains(&fn_name)).then(|| KOTLIN_EXTERNAL_TYPE.to_string());
    }
    let mut result: Option<String> = None;
    for c in &candidates {
        let sig = c.signature.as_deref()?;
        let at = sig.rfind("):")?;
        let mut ty = kotlin_type_name(&sig[at + 2..])?;
        if !ty.contains("::") {
            let container = container_of(&c.qualified_name);
            let container_name = container.rsplit("::").next().unwrap_or("");
            if !container_name.is_empty() && ctx.get_nodes_by_qualified_name(&format!("{container}::{ty}")).iter().any(|n| is_declared_type_kind(n.kind)) {
                ty = format!("{container_name}::{ty}");
            }
        }
        if result.as_ref().is_some_and(|prev| *prev != ty) {
            return None;
        }
        result = Some(ty);
    }
    result
}

/// The tightest class or interface enclosing the call's line, in any language the file holds (the
/// call-result reader's "own class").
fn kotlin_enclosing_type_of_call(r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    let mut best: Option<NodeRef> = None;
    for n in ctx.get_nodes_in_file(&r.file_path).iter() {
        if matches!(n.kind, NodeKind::Class | NodeKind::Interface) && n.start_line <= r.line && n.end_line >= r.line && best.as_ref().map_or(true, |b| n.start_line > b.start_line) {
            best = Some(n.clone());
        }
    }
    best
}

/// The declared type of Kotlin receiver `name` at `r`: the nearest local declaration in the
/// enclosing scope (typed, constructed, or bound to a call), else a property of the enclosing
/// class, else a parameter, else a property the enclosing class inherits. `None` when none names a
/// type.
pub fn kotlin_receiver_declared_type(name: &str, r: &UnresolvedRef, ctx: &Ctx, depth: u32) -> Option<String> {
    let _guard = DepthGuard::enter()?;
    let lines = kotlin_file_lines(&r.file_path, ctx);
    let mut local_declared = false;
    if let Some(lines) = &lines {
        let declares = Re::cached(&format!(r"\b(?:val|var)\s+{}\b", escape_regex(name)), "");
        for i in kotlin_local_scan_lines(r, ctx, lines.len()) {
            let line = &lines[i];
            if line.is_empty() || line.len() > 10_000 || !declares.is_match(line) {
                continue;
            }
            if let Some(local) = kotlin_declared_type_in(line, name, r, ctx, depth) {
                return Some(local);
            }
            local_declared = true;
            break;
        }
    }
    if let Some(property) = super::infer::infer_java_field_receiver_type(name, r, ctx) {
        return Some(property);
    }
    // A typed parameter of the enclosing function or lambda (`webSocket: WebSocket` in an
    // overridden callback).
    if let Some(param) = infer_local_receiver_type(name, r, ctx, None) {
        return param.starts_with(|c: char| c.is_ascii_uppercase()).then_some(param);
    }
    if local_declared {
        return None;
    }
    // A property of a superclass: `viewModel` declared in `BaseActivity`.
    let owner = kotlin_enclosing_class(r, ctx)?;
    if !kotlin_own_property_type(name, &owner, r, ctx).is_undefined() {
        return None;
    }
    kotlin_inherited_property_type(name, &owner, r, ctx)
}

/// The 0-based lines a Kotlin local visible at `r` may be declared on, nearest first: the enclosing
/// function above the call, then — when that function is nested in another (a member of an
/// anonymous object, a local function) — each outer function above the nested one, whose locals it
/// captures. Lines of a function nested in a scanned one that does not enclose `r` are skipped: its
/// locals are not visible. Outside any function, every line above the call.
fn kotlin_local_scan_lines(r: &UnresolvedRef, ctx: &Ctx, line_count: usize) -> Vec<usize> {
    let call_idx = (r.line as usize).saturating_sub(1).min(line_count.saturating_sub(1));
    let in_file = ctx.get_nodes_in_file(&r.file_path);
    let fns: Vec<&NodeRef> = in_file.iter().filter(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method) && n.language == Language::Kotlin).collect();
    let mut enclosing: Vec<&NodeRef> = fns.iter().copied().filter(|n| n.start_line <= r.line && n.end_line >= r.line).collect();
    enclosing.sort_by(|a, b| b.start_line.cmp(&a.start_line));
    let mut out: Vec<usize> = Vec::new();
    if enclosing.is_empty() {
        out.extend((0..=call_idx).rev());
        return out;
    }
    let mut hi = call_idx as i64;
    for f in enclosing {
        let hidden: Vec<&NodeRef> = fns
            .iter()
            .copied()
            .filter(|n| n.start_line > f.start_line && n.end_line <= f.end_line && !(n.start_line <= r.line && n.end_line >= r.line))
            .collect();
        let floor = f.start_line as i64 - 1;
        let mut i = hi;
        while i >= floor && i >= 0 {
            let line_no = (i + 1) as u32;
            if !hidden.iter().any(|n| line_no >= n.start_line && line_no <= n.end_line) {
                out.push(i as usize);
            }
            i -= 1;
        }
        hi = hi.min(f.start_line as i64 - 2);
    }
    out
}

/// The declared type of a Kotlin receiver chain `a.b.c` / `this.a.b`: the first value typed as a
/// single receiver (`this` is the enclosing class, a type name its object or companion), each next
/// segment as a property or enum entry declared in the class of the type before it, read from that
/// class's own file. [`KOTLIN_EXTERNAL_TYPE`] as soon as a segment has a type the project does not
/// declare; `None` when any segment's type is unknown (an inherited property, an ambiguous class
/// name).
fn kotlin_chain_declared_type(receiver: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let segments: Vec<&str> = receiver.split('.').collect();
    if segments.len() < 2 || segments.len() > 4 {
        return None;
    }
    let mut owner: Option<NodeRef> = None;
    if segments[0] == "this" {
        owner = Some(kotlin_enclosing_class(r, ctx)?);
    }
    let mut ty: Option<String> = if owner.is_some() { None } else { kotlin_receiver_declared_type(segments[0], r, ctx, 0) };
    if owner.is_none() && ty.is_none() && segments[0].starts_with(|c: char| c.is_ascii_uppercase()) {
        // An object or a class's companion: `Registry.current.load()`, `Mode.ON.next()`.
        let classes: Vec<NodeRef> = ctx.get_nodes_by_name(segments[0]).iter().filter(|n| is_declared_type_kind(n.kind) && n.language == Language::Kotlin).cloned().collect();
        if classes.len() != 1 {
            return None;
        }
        owner = Some(classes[0].clone());
    }
    for segment in &segments[1..] {
        let current = match owner.take() {
            Some(o) => o,
            None => {
                let t = ty.clone()?;
                if t == KOTLIN_EXTERNAL_TYPE || !is_project_type(&t, r, ctx) {
                    return Some(KOTLIN_EXTERNAL_TYPE.to_string());
                }
                let simple = t.rsplit("::").next().unwrap_or(&t);
                let suffix = format!("::{t}");
                let classes: Vec<NodeRef> = ctx
                    .get_nodes_by_name(simple)
                    .iter()
                    .filter(|n| is_declared_type_kind(n.kind) && n.language == Language::Kotlin && (n.qualified_name == t || n.qualified_name.ends_with(&suffix)))
                    .cloned()
                    .collect();
                if classes.len() != 1 {
                    return None;
                }
                classes[0].clone()
            }
        };
        let in_file = ctx.get_nodes_in_file(&current.file_path);
        let qualified = format!("{}::{segment}", current.qualified_name);
        if in_file.iter().any(|n| n.kind == NodeKind::EnumMember && n.qualified_name == qualified) {
            // An enum entry has its enum's type (`Outer::Mode`, package dropped).
            ty = kotlin_type_name(&current.qualified_name.replace("::", "."));
            continue;
        }
        ty = match kotlin_own_property_type(segment, &current, r, ctx) {
            Tri::Value(t) => Some(t),
            Tri::Null => None,
            Tri::Undefined => kotlin_inherited_property_type(segment, &current, r, ctx),
        };
    }
    ty
}

/// What a Kotlin call through a receiver chain resolves to.
pub enum KotlinChain {
    /// The reference is not a receiver chain.
    NotAChain,
    /// The chain ends in (or passes through) a type the project does not declare: no edge.
    NoEdge,
    /// The chain's type is unknown: resolve this bare method name, the ref the extractor emitted
    /// before it kept the chain.
    Method(String),
    Resolved(ResolvedRef),
}

/// A Kotlin call through a receiver chain (`engine.pump.drain()`), resolved on the chain's declared
/// type. No edge when the chain ends in a type the project does not declare (or passes through
/// one) and the project has no extension of that name on a library type.
pub fn match_kotlin_receiver_chain(r: &UnresolvedRef, ctx: &Ctx) -> KotlinChain {
    let Some(m) = re!(r"^((?:this|[A-Za-z_]\w*)(?:\.[A-Za-z_]\w*){1,3})\.(\w+)$").captures(&r.reference_name) else {
        return KotlinChain::NotAChain;
    };
    let method = m.text(2).unwrap_or("").to_string();
    let declared = kotlin_chain_declared_type(m.text(1).unwrap_or(""), r, ctx).filter(|d| !d.is_empty());
    let Some(declared) = declared else { return KotlinChain::Method(method) };
    if declared != KOTLIN_EXTERNAL_TYPE {
        if let Some(typed) = resolve_method_on_type(&declared, &method, r, ctx, 0.9, ResolvedBy::InstanceMethod, None, 0) {
            return KotlinChain::Resolved(typed);
        }
        if is_project_type(&declared, r, ctx) {
            return KotlinChain::Method(method);
        }
    }
    // A library type still reaches the project's own extensions of library types
    // (`view.context.dp(8)` → `fun Context.dp()`).
    if kotlin_declares_library_extension(&method, r, ctx) {
        KotlinChain::Method(method)
    } else {
        KotlinChain::NoEdge
    }
}

/// Whether the project declares a Kotlin extension `method` on a type it does not declare itself
/// (`fun Context.dp(…)`, indexed as `Context::dp`).
fn kotlin_declares_library_extension(method: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    ctx.get_nodes_by_name(method).iter().any(|n| {
        if !matches!(n.kind, NodeKind::Method | NodeKind::Function) || n.language != Language::Kotlin {
            return false;
        }
        let parts: Vec<&str> = n.qualified_name.split("::").collect();
        let owner = if parts.len() >= 2 { parts[parts.len() - 2] } else { "" };
        !owner.is_empty() && owner.starts_with(|c: char| c.is_ascii_uppercase()) && !is_project_type(owner, r, ctx)
    })
}
