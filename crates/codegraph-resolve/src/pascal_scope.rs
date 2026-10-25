//! Pascal scope resolution — port of `src/resolution/pascal-scope.ts`.
//!
//! A parameterless routine is called without parentheses in Delphi (`Before;`,
//! `Result := FProvider.Value + 1`), and the same spelling reads a field, a property, a variable
//! or a constant. The extractor records such a name as a `references` ref named `{mode}.Name` or
//! `{mode}.Receiver.Name` (`{call}` a statement, `{read}` a value, `{ref}` a method pointer,
//! `{recv}` a receiver), the routine's class after an `@`, and a receiver the routine declares
//! itself as its type after a `:`.
//!
//! Such a ref links only to a method or function the name reaches by Delphi's scope rules — the
//! routine's class and its ancestors, then the unit, then the units it uses, last first — or,
//! through a receiver, a method of the receiver's declared type. The first declaration of the
//! name in that order decides: data there links nothing. The same lookup gates calls to the
//! System unit's names, calls to routines nested in others, and calls another strategy resolved
//! to data.

use crate::ctx::{js_lower, Ctx};
use crate::name_matcher::members::declared_supertypes;
use crate::name_matcher::util::{after_col, before_col, lines_of};
use crate::types::{NodeRef, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::js_trim;
use codegraph_types::{EdgeKind, Language, NodeKind, ReferenceKind};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Call,
    Read,
    Ref,
    Recv,
}

#[derive(Clone)]
struct Found {
    node: NodeRef,
    resolved_by: ResolvedBy,
    confidence: f64,
}

fn parenless() -> &'static Re {
    re!(r"^\{(call|read|ref|recv)(?:@([^}]+))?\}\.(.+)$")
}

fn is_class_kind(k: NodeKind) -> bool {
    matches!(k, NodeKind::Class | NodeKind::Struct | NodeKind::Interface)
}

/// What a unit declares by name at its top level.
fn is_unit_kind(k: NodeKind) -> bool {
    matches!(
        k,
        NodeKind::Function
            | NodeKind::Variable
            | NodeKind::Constant
            | NodeKind::Class
            | NodeKind::Struct
            | NodeKind::Interface
            | NodeKind::Enum
            | NodeKind::TypeAlias
            | NodeKind::EnumMember
    )
}

/// What a class declares as its members.
fn is_member_kind(k: NodeKind) -> bool {
    matches!(k, NodeKind::Method | NodeKind::Field | NodeKind::Property | NodeKind::Constant | NodeKind::Variable)
}

/// Declarations that hold a value rather than run code.
fn is_data_kind(k: NodeKind) -> bool {
    matches!(k, NodeKind::Field | NodeKind::Property | NodeKind::Variable | NodeKind::Constant | NodeKind::EnumMember)
}

/// The RTL's routine types whose names don't say so (`TProc`, `TFunc<T>`), lowercased.
fn is_rtl_procedural_type(lower: &str) -> bool {
    matches!(lower, "tproc" | "tfunc" | "tpredicate" | "tcomparison" | "tthreadprocedure" | "tlistsortcompare" | "tstringlistsortcompare")
}

const MAX_HIERARCHY_DEPTH: usize = 8;

fn is_routine(n: &NodeRef) -> bool {
    matches!(n.kind, NodeKind::Method | NodeKind::Function)
}

// ---------------------------------------------------------------------------
// Memos (one per context view; dropped with the context's caches)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Header {
    line: u32,
    class_qn: Option<String>,
    routine: bool,
}

struct Outline {
    classes: Vec<NodeRef>,
    imports: Vec<NodeRef>,
    /// `None` = no limit (TS `Infinity`).
    routine_column: Option<u32>,
}

struct ProjectFiles {
    by_name: HashMap<String, Vec<String>>,
    pascal: Vec<String>,
}

type Map<V> = RefCell<HashMap<String, V>>;

#[derive(Default)]
struct Memos {
    routine_names: RefCell<Option<Rc<HashSet<String>>>>,
    named: Map<Rc<Vec<NodeRef>>>,
    in_scope: Map<Option<Found>>,
    class_of_routine: Map<Option<NodeRef>>,
    class_by_qn: Map<Option<NodeRef>>,
    ancestors: Map<Rc<Vec<NodeRef>>>,
    used_files: Map<Rc<Vec<String>>>,
    class_by_name: Map<Option<NodeRef>>,
    units_by_name: RefCell<Option<Rc<HashMap<String, Vec<String>>>>>,
    generic_types: RefCell<Option<Rc<HashMap<String, Vec<NodeRef>>>>>,
    project_files: RefCell<Option<Rc<ProjectFiles>>>,
    includes: Map<Rc<Vec<String>>>,
    includers: Map<Option<String>>,
    own_files: Map<Rc<Vec<String>>>,
    unit_file_sets: Map<Rc<Vec<String>>>,
    unit_files: Map<Option<String>>,
    hierarchy: Map<Option<NodeRef>>,
    member_types: Map<Option<NodeRef>>,
    keywords: Map<Option<String>>,
    procedural: Map<bool>,
    header_classes: Map<Rc<Vec<Header>>>,
    outlines: Map<Rc<Outline>>,
    condition_depths: Map<Rc<Vec<u32>>>,
    nested_in_scope: Map<Option<Found>>,
    routine_nodes: Map<Option<NodeRef>>,
    routine_qns: Map<bool>,
}

fn memos(ctx: &Ctx) -> Rc<Memos> {
    ctx.memo::<Memos>()
}

fn get<V: Clone>(map: &Map<V>, key: &str) -> Option<V> {
    map.borrow().get(key).cloned()
}

fn put<V>(map: &Map<V>, key: String, value: V) {
    map.borrow_mut().insert(key, value);
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Whether `r` is a paren-less Pascal name, which only this module resolves.
pub fn is_pascal_parenless_ref(r: &UnresolvedRef) -> bool {
    r.language == Language::Pascal && r.reference_kind == ReferenceKind::Edge(EdgeKind::References) && parenless().is_match(&r.reference_name)
}

/// The routine a paren-less Pascal name reaches in its scope, or None.
pub fn match_pascal_parenless_ref(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let m = parenless().captures(&r.reference_name)?;
    let mode = match m.text(1)? {
        "call" => Mode::Call,
        "read" => Mode::Read,
        "ref" => Mode::Ref,
        _ => Mode::Recv,
    };
    let path: Vec<&str> = m.text(3)?.split('.').collect();
    // Most such names read data, often the framework's: a name no Pascal routine has is settled
    // here, before any lookup in scope.
    if let Some(names) = routine_names(ctx) {
        if !names.contains(&js_lower(path[path.len() - 1])) {
            return None;
        }
    }
    let cls = match m.text(2) {
        Some(qn) => class_by_qualified_name(qn, &r.file_path, ctx),
        None => ctx.node_by_id(&r.from_node_id).and_then(|routine| class_of_routine(&routine, ctx)),
    };
    let found =
        if path.len() == 1 { name_in_scope(path[0], cls.as_ref(), r, ctx) } else { member_through_path(&path, cls.as_ref(), r, ctx) }?;
    // Written without arguments, a routine that needs some can only be a method pointer — save a
    // constructor, whose name then means an inherited one outside the project — and so is a
    // procedure read for a value, and an event's method; any other runs.
    let needs_args = needs_arguments(&found.node);
    if needs_args && routine_keyword(&found.node, ctx).as_deref() == Some("constructor") {
        return None;
    }
    let pointer = mode == Mode::Ref || needs_args || (mode != Mode::Call && !yields_value(&found.node, ctx));
    // A statement runs its routine, and a receiver is a routine's value.
    if pointer && (mode == Mode::Call || mode == Mode::Recv) {
        return None;
    }
    let mut res = ResolvedRef::new(r, found.node.id.clone(), found.confidence, found.resolved_by);
    res.edge_kind = Some(if pointer { EdgeKind::References } else { EdgeKind::Calls });
    if pointer {
        // A method pointer is a function used as a value, as `@Handler` is (#756).
        let mut md = codegraph_types::Metadata::new();
        md.insert("fnRef".to_string(), true.into());
        res.metadata = Some(md);
    }
    Some(res)
}

/// Whether `r` is a bare Pascal call to one of the System unit's names.
pub fn is_pascal_system_call(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    r.language == Language::Pascal
        && r.reference_kind == ReferenceKind::Edge(EdgeKind::Calls)
        && codegraph_extract::is_pascal_system_name(&js_lower(&r.reference_name))
        && is_bare_call(r, ctx)
}

/// Whether a Pascal call named by a bare name is written bare: its expression starts with the name.
fn is_bare_call(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if !re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name) {
        return false;
    }
    let lines = lines_of(ctx, &r.file_path);
    let line = (r.line as usize).checked_sub(1).and_then(|i| lines.get(i)).map(String::as_str).unwrap_or("");
    re!(r"^[A-Za-z_]\w*").find(after_col(line, r.column)).is_some_and(|w| js_lower(w.as_str()) == js_lower(&r.reference_name))
}

/// A call to one of the System unit's names is the System unit's unless a declaration in the
/// call's scope shadows it (see the TS module).
pub fn match_pascal_system_call(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let cls = class_of_call(r, ctx);
    let decl = declaration_in_scope(&js_lower(&r.reference_name), cls.as_ref(), r, ctx)?;
    if is_data_kind(decl.node.kind) && !is_procedural_data(&decl.node, ctx) {
        return None;
    }
    if !decl.node.qualified_name.contains("::") && declared_under_condition(&decl.node, ctx) {
        return None;
    }
    Some(ResolvedRef::new(r, decl.node.id.clone(), decl.confidence, decl.resolved_by))
}

/// What a Pascal call another strategy resolved stands for, when it landed on data or on a
/// routine nested in another.
pub fn gate_pascal_call(result: ResolvedRef, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let Some(target) = ctx.node_by_id(&result.target_node_id) else { return Some(result) };
    if is_data_kind(target.kind) {
        return gate_pascal_data_call(&target, result, r, ctx);
    }
    if is_nested_routine(&target, ctx) {
        return gate_pascal_nested_target(result, r, ctx);
    }
    Some(result)
}

fn retarget(mut result: ResolvedRef, found: &Found) -> ResolvedRef {
    result.target_node_id = found.node.id.clone();
    result.confidence = found.confidence;
    result.resolved_by = found.resolved_by;
    result
}

fn gate_pascal_data_call(target: &NodeRef, result: ResolvedRef, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    if !is_bare_call(r, ctx) {
        return (target.language == Language::Pascal && is_procedural_data(target, ctx)).then_some(result);
    }
    let lower = js_lower(&r.reference_name);
    if declared_in_routine(&lower, r, ctx) {
        return None;
    }
    let cls = class_of_call(r, ctx);
    let Some(decl) = declaration_in_scope(&lower, cls.as_ref(), r, ctx) else {
        let only = target.language == Language::Pascal
            && pascal_named(&lower, ctx).iter().all(|n| n.id == target.id || n.kind == NodeKind::Import);
        return (only && is_procedural_data(target, ctx)).then_some(result);
    };
    if !is_routine(&decl.node) && !(is_data_kind(decl.node.kind) && is_procedural_data(&decl.node, ctx)) {
        return None;
    }
    if decl.node.id == target.id {
        return Some(result);
    }
    Some(retarget(result, &decl))
}

/// Whether the routine making a call declares the called name itself, as a parameter, a local
/// or a nested routine: from the routine's header down to the call.
fn declared_in_routine(lower: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let lines = lines_of(ctx, &r.file_path);
    let from = routine_header_at(&r.file_path, r.line, ctx);
    let name = escape_regex(lower);
    let mentions = Re::cached(&format!(r"\b{name}\b"), "i");
    let declares = Re::cached(&format!(r"\b(?:procedure|function)\s+{name}\b|\b{name}\s*(?:,\s*[A-Za-z_]\w*\s*)*:(?!=)"), "i");
    let mut i = from.max(1);
    while i <= r.line && (i as usize) <= lines.len() {
        let text = &lines[i as usize - 1];
        if mentions.is_match(text) && declares.is_match(&strip_comments_and_strings(text)) {
            return true;
        }
        i += 1;
    }
    false
}

fn strip_comments_and_strings(text: &str) -> String {
    re!(r"//.*$|\{[^}]*\}|'[^']*'", "g").replace_all(text, |_| Some(" ".to_string()))
}

fn strip_comments(text: &str) -> String {
    re!(r"//.*$|\{[^}]*\}|\(\*.*?\*\)", "g").replace_all(text, |_| Some(" ".to_string()))
}

/// The line of the nearest routine header at column 0 at or above a line, else the line itself.
fn routine_header_at(file_path: &str, line: u32, ctx: &Ctx) -> u32 {
    let headers = header_classes(file_path, ctx);
    let mut found = line;
    for h in headers.iter() {
        if h.line > line {
            break;
        }
        if h.routine {
            found = h.line;
        }
    }
    found
}

/// Whether a declaration is inside an `{$IFDEF}`-family block, or its `{$ELSE}`.
fn declared_under_condition(node: &NodeRef, ctx: &Ctx) -> bool {
    let m = memos(ctx);
    let lines = lines_of(ctx, &node.file_path);
    let depths = match get(&m.condition_depths, &node.file_path) {
        Some(d) => d,
        None => {
            let mut depths = Vec::with_capacity(lines.len());
            let mut depth = 0;
            for text in lines.iter() {
                depths.push(depth);
                if text.contains("{$") {
                    depth = condition_depth_after(text, depth);
                }
            }
            let d = Rc::new(depths);
            put(&m.condition_depths, node.file_path.clone(), Rc::clone(&d));
            d
        }
    };
    let idx = (node.start_line as usize).wrapping_sub(1);
    let line = lines.get(idx).map(String::as_str).unwrap_or("");
    condition_depth_after(before_col(line, node.start_column), depths.get(idx).copied().unwrap_or(0)) > 0
}

/// The depth of conditional blocks after a line's directives.
fn condition_depth_after(text: &str, mut depth: u32) -> u32 {
    for m in re!(r"\{\$(ifdef|ifndef|ifopt|ifend|endif|if)\b", "gi").captures_iter(text) {
        let directive = js_lower(m.text(1).unwrap_or(""));
        depth = if directive == "endif" || directive == "ifend" { depth.saturating_sub(1) } else { depth + 1 };
    }
    depth
}

/// The class whose method makes a call: the one the nearest method header above the call names
/// in the source text, else the reference's method's class.
fn class_of_call(r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    if let Some(qn) = header_class_at(&r.file_path, r.line, ctx) {
        if let Some(cls) = class_by_qualified_name(&qn, &r.file_path, ctx) {
            return Some(cls);
        }
    }
    let routine = ctx.node_by_id(&r.from_node_id)?;
    if routine.kind == NodeKind::Method {
        class_of_routine(&routine, ctx)
    } else {
        None
    }
}

fn header_classes(file_path: &str, ctx: &Ctx) -> Rc<Vec<Header>> {
    let m = memos(ctx);
    if let Some(h) = get(&m.header_classes, file_path) {
        return h;
    }
    let mut headers = Vec::new();
    let lines = lines_of(ctx, file_path);
    let header = re!(r"^(?:class\s+)?(?:procedure|function|constructor|destructor|operator)\s+([A-Za-z_][\w.<>, ]*?)\s*[(:;]", "i");
    for (i, text) in lines.iter().enumerate() {
        // Each starts its line with one of c, d, e, f, i, o, p (either case).
        let first = text.encode_utf16().next().map_or(0x20, |u| u | 0x20);
        if !matches!(first, 0x63 | 0x64 | 0x65 | 0x66 | 0x69 | 0x6f | 0x70) {
            continue;
        }
        if re!(r"^(?:end;|(?:initialization|finalization)\b)", "i").is_match(text) {
            headers.push(Header { line: i as u32 + 1, class_qn: None, routine: false });
            continue;
        }
        let Some(c) = header.captures(text) else { continue };
        let raw = c.text(1).unwrap_or("");
        let cleaned = re!(r"\s+", "g").replace_all_str(&re!(r"<[^>]*>", "g").replace_all_str(raw, ""), "");
        let parts: Vec<&str> = cleaned.split('.').collect();
        let class_qn = (parts.len() > 1).then(|| parts[..parts.len() - 1].join("::"));
        headers.push(Header { line: i as u32 + 1, class_qn, routine: true });
    }
    let h = Rc::new(headers);
    put(&m.header_classes, file_path.to_string(), Rc::clone(&h));
    h
}

/// The class the nearest routine header at or above a line names, or None when it is a free
/// routine's, or the unit's code resumes there.
fn header_class_at(file_path: &str, line: u32, ctx: &Ctx) -> Option<String> {
    let headers = header_classes(file_path, ctx);
    let idx = headers.partition_point(|h| h.line <= line);
    if idx == 0 {
        None
    } else {
        headers[idx - 1].class_qn.clone()
    }
}

// ---------------------------------------------------------------------------
// Scope lookup
// ---------------------------------------------------------------------------

fn name_in_scope(name: &str, cls: Option<&NodeRef>, r: &UnresolvedRef, ctx: &Ctx) -> Option<Found> {
    declaration_in_scope(&js_lower(name), cls, r, ctx).filter(|d| is_routine(&d.node))
}

/// The routine nested in the calling routine, or in one around it, that a bare name there names:
/// the innermost one declared above the call.
fn nested_routine_in_scope(lower: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<Found> {
    let named = pascal_named(lower, ctx);
    if !named.iter().any(|n| n.kind == NodeKind::Function && n.qualified_name.contains("::")) {
        return None;
    }
    let m = memos(ctx);
    let key = format!("{}|{}", r.from_node_id, lower);
    if let Some(hit) = get(&m.nested_in_scope, &key) {
        return hit;
    }
    let mut found = None;
    if let Some(caller) = routine_node(&r.from_node_id, ctx) {
        if caller.language == Language::Pascal {
            let chain = js_lower(&caller.qualified_name);
            let mut best: Option<NodeRef> = None;
            let mut depth: i64 = -1;
            for n in named.iter() {
                if n.file_path != caller.file_path || n.start_line > r.line || !is_nested_routine(n, ctx) {
                    continue;
                }
                let outer = owner_of(n);
                if chain != outer && !chain.starts_with(&format!("{outer}::")) {
                    continue;
                }
                let len = outer.encode_utf16().count() as i64;
                if len > depth || (len == depth && best.as_ref().is_some_and(|b| n.start_line < b.start_line)) {
                    best = Some(Rc::clone(n));
                    depth = len;
                }
            }
            found = best.map(|node| Found { node, resolved_by: ResolvedBy::ExactMatch, confidence: 0.95 });
        }
    }
    put(&m.nested_in_scope, key, found.clone());
    found
}

/// Whether a node is a Pascal function qualified under a routine.
fn is_nested_routine(node: &NodeRef, ctx: &Ctx) -> bool {
    if node.kind != NodeKind::Function || node.language != Language::Pascal {
        return false;
    }
    let Some(cut) = node.qualified_name.rfind("::") else { return false };
    let outer = &node.qualified_name[..cut];
    let m = memos(ctx);
    if let Some(hit) = get(&m.routine_qns, outer) {
        return hit;
    }
    let routine = ctx.get_nodes_by_qualified_name(outer).iter().any(|n| n.language == Language::Pascal && is_routine(n));
    put(&m.routine_qns, outer.to_string(), routine);
    routine
}

fn routine_node(id: &str, ctx: &Ctx) -> Option<NodeRef> {
    let m = memos(ctx);
    if let Some(hit) = get(&m.routine_nodes, id) {
        return hit;
    }
    let routine = ctx.node_by_id(id).filter(is_routine);
    put(&m.routine_nodes, id.to_string(), routine.clone());
    routine
}

/// A bare Pascal call to a routine nested where the call is; made inside a nested routine, it
/// reaches the rest of that routine's scope. None when that scope holds no routine of the name,
/// for the other strategies to resolve.
pub fn match_pascal_nested_call(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    if r.language != Language::Pascal || r.reference_kind != ReferenceKind::Edge(EdgeKind::Calls) || !is_bare_call(r, ctx) {
        return None;
    }
    let lower = js_lower(&r.reference_name);
    let found = match nested_routine_in_scope(&lower, r, ctx) {
        Some(f) => f,
        None => {
            let caller = routine_node(&r.from_node_id, ctx)?;
            if !is_nested_routine(&caller, ctx) {
                return None;
            }
            let cls = class_of_call(r, ctx);
            let decl = declaration_in_scope(&lower, cls.as_ref(), r, ctx)?;
            if !(is_routine(&decl.node) || (is_data_kind(decl.node.kind) && is_procedural_data(&decl.node, ctx))) {
                return None;
            }
            decl
        }
    };
    Some(ResolvedRef::new(r, found.node.id.clone(), found.confidence, found.resolved_by))
}

fn gate_pascal_nested_target(result: ResolvedRef, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    if !is_bare_call(r, ctx) {
        return None;
    }
    let lower = js_lower(&r.reference_name);
    let cls = class_of_call(r, ctx);
    let decl = declaration_in_scope(&lower, cls.as_ref(), r, ctx)?;
    if !is_routine(&decl.node) && !(is_data_kind(decl.node.kind) && is_procedural_data(&decl.node, ctx)) {
        return None;
    }
    if decl.node.id == result.target_node_id {
        return Some(result);
    }
    Some(retarget(result, &decl))
}

/// The first declaration a bare name reaches from a routine of class `cls` (None: a free
/// routine), whatever it declares.
fn declaration_in_scope(lower: &str, cls: Option<&NodeRef>, r: &UnresolvedRef, ctx: &Ctx) -> Option<Found> {
    if let Some(nested) = nested_routine_in_scope(lower, r, ctx) {
        return Some(nested);
    }
    let m = memos(ctx);
    let key = format!("{}|{}|{}", cls.map_or("", |c| c.id.as_str()), r.file_path, lower);
    if let Some(hit) = get(&m.in_scope, &key) {
        return hit;
    }
    let mut found = None;
    if let Some(member) = cls.and_then(|c| member_in_hierarchy(c, lower, ctx)) {
        found = Some(Found { node: member, resolved_by: ResolvedBy::InstanceMethod, confidence: 0.9 });
    } else if let Some(own) = own_unit_declaration(&r.file_path, lower, ctx) {
        found = Some(Found { node: own, resolved_by: ResolvedBy::ExactMatch, confidence: 0.9 });
    } else {
        let used = used_unit_files(&r.file_path, ctx);
        for file in used.iter().rev() {
            if let Some(decl) = unit_declaration(file, lower, ctx, &r.file_path) {
                found = Some(Found { node: decl, resolved_by: ResolvedBy::Import, confidence: 0.85 });
                break;
            }
        }
    }
    put(&m.in_scope, key, found.clone());
    found
}

/// A dotted name: the method its receiver's declared type has.
fn member_through_path(path: &[&str], cls: Option<&NodeRef>, r: &UnresolvedRef, ctx: &Ctx) -> Option<Found> {
    let (head_type, unit_file) = head_of(path[0], cls, r, ctx)?;
    let mut ty: Option<NodeRef>;
    let mut rest: &[&str] = &path[1..];
    if let Some(unit_file) = unit_file {
        let decl = unit_declaration(&unit_file, &js_lower(rest[0]), ctx, &r.file_path)?;
        if rest.len() == 1 {
            return is_routine(&decl).then(|| Found { node: decl, resolved_by: ResolvedBy::Import, confidence: 0.9 });
        }
        ty = type_of_member(&decl, ctx);
        rest = &rest[1..];
    } else {
        ty = head_type;
    }
    for link in &rest[..rest.len() - 1] {
        let member = member_in_hierarchy(ty.as_ref()?, &js_lower(link), ctx)?;
        ty = type_of_member(&member, ctx);
    }
    let member = member_in_hierarchy(ty.as_ref()?, &js_lower(rest[rest.len() - 1]), ctx)?;
    is_routine(&member).then(|| Found { node: member, resolved_by: ResolvedBy::InstanceMethod, confidence: 0.9 })
}

/// What a dotted name's first link is: a value of a known class, a class itself, or a unit.
/// `(type, unit_file)`.
fn head_of(name: &str, cls: Option<&NodeRef>, r: &UnresolvedRef, ctx: &Ctx) -> Option<(Option<NodeRef>, Option<String>)> {
    if let Some(type_name) = name.strip_prefix(':') {
        return Some((class_named(type_name, &r.file_path, ctx, 0), None));
    }
    let lower = js_lower(name);
    if lower == "self" {
        return cls.map(|c| (Some(Rc::clone(c)), None));
    }
    if let Some(c) = cls {
        if let Some(member) = member_in_hierarchy(c, &lower, ctx) {
            return Some((type_of_member(&member, ctx), None));
        }
    }
    let decl = own_unit_declaration(&r.file_path, &lower, ctx)
        .or_else(|| used_unit_files(&r.file_path, ctx).iter().rev().find_map(|file| unit_declaration(file, &lower, ctx, &r.file_path)));
    if let Some(decl) = decl {
        let ty = if is_class_kind(decl.kind) { Some(decl) } else { type_of_member(&decl, ctx) };
        return Some((ty, None));
    }
    let unit_file = unit_file_named(&lower, &r.file_path, ctx)?;
    Some((None, Some(unit_file)))
}

// ---------------------------------------------------------------------------
// Classes, units and declarations
// ---------------------------------------------------------------------------

/// Every Pascal method's and function's name, lowercased.
fn routine_names(ctx: &Ctx) -> Option<Rc<HashSet<String>>> {
    let m = memos(ctx);
    if let Some(hit) = m.routine_names.borrow().as_ref() {
        return Some(Rc::clone(hit));
    }
    let names: HashSet<String> =
        ctx.get_node_names_of_kinds(Language::Pascal, &[NodeKind::Method, NodeKind::Function]).iter().map(|n| js_lower(n)).collect();
    let names = Rc::new(names);
    *m.routine_names.borrow_mut() = Some(Rc::clone(&names));
    Some(names)
}

/// The Pascal nodes a name names, whatever their case.
fn pascal_named(lower: &str, ctx: &Ctx) -> Rc<Vec<NodeRef>> {
    let m = memos(ctx);
    if let Some(hit) = get(&m.named, lower) {
        return hit;
    }
    let nodes: Vec<NodeRef> = ctx.get_nodes_by_lower_name(lower).iter().filter(|n| n.language == Language::Pascal).cloned().collect();
    let nodes = Rc::new(nodes);
    put(&m.named, lower.to_string(), Rc::clone(&nodes));
    nodes
}

fn strip_generics(s: &str) -> String {
    re!(r"<[^>]*>", "g").replace_all_str(s, "")
}

/// The class (or record) an implementation header names: the one `file_path` declares, whatever
/// its case or parameters, else one the file sees.
fn class_by_qualified_name(qualified_name: &str, file_path: &str, ctx: &Ctx) -> Option<NodeRef> {
    let m = memos(ctx);
    let key = format!("{file_path}|{qualified_name}");
    if let Some(hit) = get(&m.class_by_qn, &key) {
        return hit;
    }
    let exact: Vec<NodeRef> = ctx
        .get_nodes_by_qualified_name(qualified_name)
        .iter()
        .filter(|n| n.language == Language::Pascal && is_class_kind(n.kind))
        .cloned()
        .collect();
    let lower = js_lower(qualified_name);
    let mut cls = None;
    for file in own_unit_files(file_path, ctx).iter() {
        let own: Vec<NodeRef> = exact.iter().filter(|n| &n.file_path == file).cloned().collect();
        cls = declaration_of(&own).or_else(|| {
            let outline = outline_of(file, ctx);
            let same: Vec<NodeRef> =
                outline.classes.iter().filter(|n| js_lower(&strip_generics(&n.qualified_name)) == lower).cloned().collect();
            declaration_of(&same)
        });
        if cls.is_some() {
            break;
        }
    }
    let cls = cls
        .or_else(|| declaration_of(&exact))
        .or_else(|| class_named(qualified_name.rsplit("::").next().unwrap_or(qualified_name), file_path, ctx, 0));
    put(&m.class_by_qn, key, cls.clone());
    cls
}

/// The class declaration among same-named nodes, not a forward declaration: the longest one.
fn declaration_of(classes: &[NodeRef]) -> Option<NodeRef> {
    let mut best: Option<&NodeRef> = None;
    for c in classes {
        let span = |n: &NodeRef| n.end_line as i64 - n.start_line as i64;
        if best.is_none_or(|b| span(c) > span(b)) {
            best = Some(c);
        }
    }
    best.cloned()
}

/// The class a method belongs to: its qualified name's owner, declared in the method's own file.
fn class_of_routine(routine: &NodeRef, ctx: &Ctx) -> Option<NodeRef> {
    if routine.kind != NodeKind::Method {
        return None;
    }
    let m = memos(ctx);
    if let Some(hit) = get(&m.class_of_routine, &routine.id) {
        return hit;
    }
    let owner = routine.qualified_name.rfind("::").map_or("", |cut| &routine.qualified_name[..cut]);
    let candidates: Vec<NodeRef> = if owner.is_empty() {
        Vec::new()
    } else {
        ctx.get_nodes_by_qualified_name(owner).iter().filter(|n| n.language == Language::Pascal && is_class_kind(n.kind)).cloned().collect()
    };
    let same_file: Vec<NodeRef> = candidates.iter().filter(|n| n.file_path == routine.file_path).cloned().collect();
    let cls = declaration_of(&same_file).or_else(|| declaration_of(&candidates));
    put(&m.class_of_routine, routine.id.clone(), cls.clone());
    cls
}

/// A class and its ancestors the project declares, nearest first.
fn ancestors_of(cls: &NodeRef, ctx: &Ctx) -> Rc<Vec<NodeRef>> {
    let m = memos(ctx);
    if let Some(hit) = get(&m.ancestors, &cls.id) {
        return hit;
    }
    let mut order: Vec<NodeRef> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut frontier = vec![Rc::clone(cls)];
    let mut depth = 0;
    while depth < MAX_HIERARCHY_DEPTH && !frontier.is_empty() {
        let mut next = Vec::new();
        for c in &frontier {
            if !seen.insert(c.id.clone()) {
                continue;
            }
            order.push(Rc::clone(c));
            for name in declared_supertypes(c, ctx).iter() {
                if let Some(sup) = class_named(name, &c.file_path, ctx, 0) {
                    next.push(sup);
                }
            }
        }
        frontier = next;
        depth += 1;
    }
    let order = Rc::new(order);
    put(&m.ancestors, cls.id.clone(), Rc::clone(&order));
    order
}

/// The qualified name of what declares a node (`TFoo::Bar` → `tfoo`), lowercased.
fn owner_of(node: &NodeRef) -> String {
    js_lower(node.qualified_name.rfind("::").map_or("", |cut| &node.qualified_name[..cut]))
}

/// The member a name reaches in a class or, failing that, in its ancestors, nearest first; a
/// parameterless overload before the others.
fn member_in_hierarchy(cls: &NodeRef, lower: &str, ctx: &Ctx) -> Option<NodeRef> {
    let m = memos(ctx);
    let key = format!("{}|{}", cls.id, lower);
    if let Some(hit) = get(&m.hierarchy, &key) {
        return hit;
    }
    let mut found = None;
    let candidates: Vec<NodeRef> = pascal_named(lower, ctx).iter().filter(|n| is_member_kind(n.kind)).cloned().collect();
    if !candidates.is_empty() {
        for c in ancestors_of(cls, ctx).iter() {
            let owner = js_lower(&c.qualified_name);
            let members: Vec<&NodeRef> = candidates.iter().filter(|n| n.file_path == c.file_path && owner_of(n) == owner).collect();
            if !members.is_empty() {
                found = Some(Rc::clone(members.iter().find(|n| !needs_arguments(n)).unwrap_or(&members[0])));
                break;
            }
        }
    }
    put(&m.hierarchy, key, found.clone());
    found
}

/// A unit's top-level declaration of a name (its enum values included), as code in `from_file`
/// sees it.
fn unit_declaration(file_path: &str, lower: &str, ctx: &Ctx, from_file: &str) -> Option<NodeRef> {
    let named = pascal_named(lower, ctx);
    let decls: Vec<&NodeRef> = named
        .iter()
        .filter(|n| {
            n.file_path == file_path
                && is_unit_kind(n.kind)
                && (n.kind == NodeKind::EnumMember || !n.qualified_name.contains("::"))
                && !(from_file != file_path
                    && n.kind == NodeKind::Function
                    && unit_routine_column(file_path, ctx).is_some_and(|col| n.start_column > col))
        })
        .collect();
    decls
        .iter()
        .find(|n| !is_routine(n))
        .or_else(|| decls.iter().find(|n| !needs_arguments(n)))
        .or_else(|| decls.first())
        .map(|n| Rc::clone(n))
}

fn unit_routine_column(file_path: &str, ctx: &Ctx) -> Option<u32> {
    outline_of(file_path, ctx).routine_column
}

/// What scope lookups need of a file's nodes, read once.
fn outline_of(file_path: &str, ctx: &Ctx) -> Rc<Outline> {
    let m = memos(ctx);
    if let Some(hit) = get(&m.outlines, file_path) {
        return hit;
    }
    let nodes = ctx.get_nodes_in_file(file_path);
    let mut routine_column: Option<u32> = None;
    if !nodes.iter().any(|n| n.kind == NodeKind::Module) {
        for n in nodes.iter() {
            if n.kind == NodeKind::Function && !n.qualified_name.contains("::") {
                routine_column = Some(routine_column.map_or(n.start_column, |c| c.min(n.start_column)));
            }
        }
    }
    let mut imports: Vec<NodeRef> = nodes.iter().filter(|n| n.kind == NodeKind::Import && n.language == Language::Pascal).cloned().collect();
    imports.sort_by(|a, b| a.start_line.cmp(&b.start_line).then(a.start_column.cmp(&b.start_column)));
    let outline = Rc::new(Outline { classes: nodes.iter().filter(|n| is_class_kind(n.kind)).cloned().collect(), imports, routine_column });
    put(&m.outlines, file_path.to_string(), Rc::clone(&outline));
    outline
}

/// The files of the units a file uses, in the order its `uses` clauses name them.
fn used_unit_files(from_file: &str, ctx: &Ctx) -> Rc<Vec<String>> {
    let m = memos(ctx);
    if let Some(hit) = get(&m.used_files, from_file) {
        return hit;
    }
    let unit = unit_files_of(from_file, ctx);
    let mut files: Vec<String> = Vec::new();
    for own in unit.iter() {
        for imp in outline_of(own, ctx).imports.iter() {
            if let Some(file) = unit_file_named(&js_lower(&imp.name), own, ctx) {
                if !unit.contains(&file) && !files.contains(&file) {
                    files.push(file);
                }
            }
        }
    }
    let files = Rc::new(files);
    put(&m.used_files, from_file.to_string(), Rc::clone(&files));
    files
}

fn own_unit_declaration(from_file: &str, lower: &str, ctx: &Ctx) -> Option<NodeRef> {
    own_unit_files(from_file, ctx).iter().find_map(|file| unit_declaration(file, lower, ctx, file))
}

/// The files whose declarations are a file's unit's own, the file first.
fn own_unit_files(file_path: &str, ctx: &Ctx) -> Rc<Vec<String>> {
    let m = memos(ctx);
    if let Some(hit) = get(&m.own_files, file_path) {
        return hit;
    }
    let unit = unit_files_of(file_path, ctx);
    let own = if unit.first().map(String::as_str) == Some(file_path) {
        unit
    } else {
        let mut v = vec![file_path.to_string()];
        v.extend(unit.iter().filter(|f| f.as_str() != file_path).cloned());
        Rc::new(v)
    };
    put(&m.own_files, file_path.to_string(), Rc::clone(&own));
    own
}

fn is_inc(path: &str) -> bool {
    re!(r"\.inc$", "i").is_match(path)
}

fn is_pascal_source(path: &str) -> bool {
    re!(r"\.(?:pas|dpr|dpk|lpr|inc)$", "i").is_match(path)
}

/// The files one unit is made of: its own file, then the include files it pulls in.
fn unit_files_of(file_path: &str, ctx: &Ctx) -> Rc<Vec<String>> {
    let m = memos(ctx);
    if let Some(hit) = get(&m.unit_file_sets, file_path) {
        return hit;
    }
    let mut root = file_path.to_string();
    let mut i = 0;
    while i < 3 && is_inc(&root) {
        match includer_of(&root, ctx) {
            Some(up) if up != file_path => root = up,
            _ => break,
        }
        i += 1;
    }
    let mut files = vec![root];
    let mut i = 0;
    while i < files.len() && files.len() < 32 {
        let incs = includes_of(&files[i].clone(), ctx);
        for inc in incs.iter() {
            if !files.contains(inc) {
                files.push(inc.clone());
            }
        }
        i += 1;
    }
    if !files.iter().any(|f| f == file_path) {
        files.push(file_path.to_string());
    }
    let files = Rc::new(files);
    put(&m.unit_file_sets, file_path.to_string(), Rc::clone(&files));
    files
}

/// The include files a Pascal file pulls in, each the one of that name nearest it.
fn includes_of(file_path: &str, ctx: &Ctx) -> Rc<Vec<String>> {
    let m = memos(ctx);
    if let Some(hit) = get(&m.includes, file_path) {
        return hit;
    }
    let mut includes: Vec<String> = Vec::new();
    if is_pascal_source(file_path) {
        for text in lines_of(ctx, file_path).iter() {
            if !text.contains("{$") || !re!(r"\{\$i(?:nclude)?\s", "i").is_match(text) {
                continue;
            }
            for c in re!(r#"\{\$(?:I|INCLUDE)\s+['"]?([^'"}\s*]+)"#, "gi").captures_iter(text) {
                let raw = c.text(1).unwrap_or("").replace('\\', "/");
                let mut name = js_lower(raw.rsplit('/').next().unwrap_or(""));
                if !name.contains('.') {
                    name.push_str(".inc");
                }
                let pf = project_files(ctx);
                let inc = nearest(pf.by_name.get(&name).map(Vec::as_slice).unwrap_or(&[]), file_path);
                if let Some(inc) = inc {
                    if inc != file_path && !includes.contains(&inc) {
                        includes.push(inc);
                    }
                }
            }
        }
    }
    let includes = Rc::new(includes);
    put(&m.includes, file_path.to_string(), Rc::clone(&includes));
    includes
}

/// The Pascal file that includes an include file: of the forty nearest it, the nearest whose text does.
fn includer_of(inc_file: &str, ctx: &Ctx) -> Option<String> {
    let m = memos(ctx);
    if let Some(hit) = get(&m.includers, inc_file) {
        return hit;
    }
    let inc_units: Vec<u16> = inc_file.encode_utf16().collect();
    let shared = |f: &str| -> i64 {
        let fu: Vec<u16> = f.encode_utf16().collect();
        let mut n = 0usize;
        while n < fu.len() && n < inc_units.len() && fu[n] == inc_units[n] {
            n += 1;
        }
        // `f.lastIndexOf('/', n - 1)`: a negative start searches from 0.
        let from = n.saturating_sub(1).min(fu.len().saturating_sub(1));
        let slash = if fu.is_empty() { -1 } else { (0..=from).rev().find(|&i| fu[i] == b'/' as u16).map_or(-1, |i| i as i64) };
        slash * 1000 + n as i64
    };
    let pf = project_files(ctx);
    let mut candidates: Vec<(String, i64)> = pf.pascal.iter().filter(|f| f.as_str() != inc_file).map(|f| (f.clone(), shared(f))).collect();
    candidates.sort_by(|a, b| b.1.cmp(&a.1));
    candidates.truncate(40);
    let includer = candidates.into_iter().map(|(f, _)| f).find(|f| includes_of(f, ctx).iter().any(|i| i == inc_file));
    put(&m.includers, inc_file.to_string(), includer.clone());
    includer
}

/// The project's files by lowercased name, and its Pascal sources.
fn project_files(ctx: &Ctx) -> Rc<ProjectFiles> {
    let m = memos(ctx);
    if let Some(hit) = m.project_files.borrow().as_ref() {
        return Rc::clone(hit);
    }
    let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
    let mut pascal = Vec::new();
    for file in ctx.get_all_files() {
        let base = js_lower(file.rsplit('/').next().unwrap_or(&file));
        by_name.entry(base).or_default().push(file.clone());
        if is_pascal_source(&file) {
            pascal.push(file);
        }
    }
    let pf = Rc::new(ProjectFiles { by_name, pascal });
    *m.project_files.borrow_mut() = Some(Rc::clone(&pf));
    pf
}

/// The file of the unit a name names, nearest to `from_file` when several units share it.
fn unit_file_named(lower: &str, from_file: &str, ctx: &Ctx) -> Option<String> {
    let m = memos(ctx);
    let key = format!("{from_file}|{lower}");
    if let Some(hit) = get(&m.unit_files, &key) {
        return hit;
    }
    let units = {
        let cached = m.units_by_name.borrow().as_ref().map(Rc::clone);
        match cached {
            Some(u) => u,
            None => {
                let mut units: HashMap<String, Vec<String>> = HashMap::new();
                let mut headed: HashSet<String> = HashSet::new();
                for n in ctx.get_nodes_by_kind(NodeKind::Module).iter() {
                    if n.language != Language::Pascal {
                        continue;
                    }
                    units.entry(js_lower(&n.name)).or_default().push(n.file_path.clone());
                    headed.insert(n.file_path.clone());
                }
                // A unit's file is named after it: one whose `unit` header the parser lost is
                // the unit its file names.
                for file in ctx.get_all_files() {
                    if headed.contains(&file) || !re!(r"\.pas$", "i").is_match(&file) {
                        continue;
                    }
                    let base = file.rsplit('/').next().unwrap_or(&file);
                    let stem = &base[..base.len() - 4];
                    units.entry(js_lower(stem)).or_default().push(file.clone());
                }
                let u = Rc::new(units);
                *m.units_by_name.borrow_mut() = Some(Rc::clone(&u));
                u
            }
        }
    };
    let file = nearest(units.get(lower).map(Vec::as_slice).unwrap_or(&[]), from_file);
    put(&m.unit_files, key, file.clone());
    file
}

/// A class (or record or interface) a type name names, seen from `from_file`.
fn class_named(type_name: &str, from_file: &str, ctx: &Ctx, depth: u32) -> Option<NodeRef> {
    let simple = js_lower(type_name.rsplit('.').next().unwrap_or(type_name));
    let m = memos(ctx);
    let key = format!("{from_file}|{simple}");
    if let Some(hit) = get(&m.class_by_name, &key) {
        return hit;
    }
    let named = pascal_named(&simple, ctx);
    let visible = |want: fn(NodeKind) -> bool| -> Vec<NodeRef> {
        let candidates: Vec<NodeRef> = named.iter().filter(|n| want(n.kind)).cloned().collect();
        for file in own_unit_files(from_file, ctx).iter() {
            let own: Vec<NodeRef> = candidates.iter().filter(|n| &n.file_path == file).cloned().collect();
            if !own.is_empty() {
                return own;
            }
        }
        let used = used_unit_files(from_file, ctx);
        for file in used.iter().rev() {
            let there: Vec<NodeRef> = candidates.iter().filter(|n| &n.file_path == file).cloned().collect();
            if !there.is_empty() {
                return there;
            }
        }
        if !candidates.is_empty() && candidates.iter().all(|n| n.file_path == candidates[0].file_path) {
            candidates
        } else {
            Vec::new()
        }
    };
    let mut cls = declaration_of(&visible(is_class_kind));
    if cls.is_none() && depth < 4 {
        // A type alias names the class it stands for: `TVTAncestor = TVTAncestorVcl;`.
        for alias in visible(|k| k == NodeKind::TypeAlias) {
            let lines = lines_of(ctx, &alias.file_path);
            let raw = (alias.start_line as usize).checked_sub(1).and_then(|i| lines.get(i)).map(String::as_str).unwrap_or("");
            let line = re!(r"//.*$|\{[^}]*\}", "g").replace_all_str(raw, "");
            let target = re!(r"=\s*([A-Za-z_][\w.]*)\s*;").captures(&line).and_then(|c| c.text(1).map(str::to_string));
            if let Some(target) = target {
                cls = class_named(&target, &alias.file_path, ctx, depth + 1);
                if cls.is_some() {
                    break;
                }
            }
        }
    }
    put(&m.class_by_name, key, cls.clone());
    cls
}

/// The file whose path shares the longest directory prefix with `from`.
fn nearest(files: &[String], from: &str) -> Option<String> {
    if files.len() <= 1 {
        return files.first().cloned();
    }
    let dirs: Vec<&str> = from.split('/').collect();
    let mut best: Option<&String> = None;
    let mut best_score: i64 = -1;
    for f in files {
        let parts: Vec<&str> = f.split('/').collect();
        let mut score = 0usize;
        while score < parts.len() && score < dirs.len() && parts[score] == dirs[score] {
            score += 1;
        }
        if score as i64 > best_score {
            best = Some(f);
            best_score = score as i64;
        }
    }
    best.cloned()
}

// ---------------------------------------------------------------------------
// Declared types
// ---------------------------------------------------------------------------

/// The class a member's value has: a field's or variable's declared type, a property's, a
/// function's result.
fn type_of_member(member: &NodeRef, ctx: &Ctx) -> Option<NodeRef> {
    let m = memos(ctx);
    if let Some(hit) = get(&m.member_types, &member.id) {
        return hit;
    }
    let type_name: Option<String> = if is_routine(member) {
        member.return_type.clone().or_else(|| {
            re!(r":\s*([A-Za-z_][\w.]*)\s*;?\s*$")
                .captures(member.signature.as_deref().unwrap_or(""))
                .and_then(|c| c.text(1).map(str::to_string))
        })
    } else {
        let text = declared_type_text(member, ctx);
        re!(r"^([A-Za-z_][\w.]*)").captures(&text).and_then(|c| c.text(1).map(str::to_string))
    };
    let ty = type_name.as_deref().and_then(|t| class_named(t, &member.file_path, ctx, 0));
    put(&m.member_types, member.id.clone(), ty.clone());
    ty
}

/// The text of the type a field, variable, constant or property declares, from its declaration on.
fn declared_type_text(node: &NodeRef, ctx: &Ctx) -> String {
    let lines = lines_of(ctx, &node.file_path);
    let name = escape_regex(&node.name);
    let property = node.kind == NodeKind::Property;
    let re = if property {
        Re::cached(&format!(r"\bproperty\s+{name}\s*(?:\[[^\]]*\])?\s*:\s*(.*)$"), "i")
    } else {
        Re::cached(&format!(r"\b{name}\b(?:\s*,\s*[A-Za-z_]\w*)*\s*,?\s*:(?!=)\s*(.*)$"), "i")
    };
    let mut text = String::new();
    let start = node.start_line as usize;
    let mut i = start.wrapping_sub(1);
    while i < start + 5 && i < lines.len() {
        text.push(' ');
        text.push_str(&strip_comments(&lines[i]));
        if let Some(c) = re.captures(&text) {
            return js_trim(c.text(1).unwrap_or("")).to_string();
        }
        // A list goes on only after a comma.
        if property || !re!(r",\s*$").is_match(&text) {
            break;
        }
        i += 1;
    }
    String::new()
}

/// Whether data holds a routine, by its declared type.
fn is_procedural_data(node: &NodeRef, ctx: &Ctx) -> bool {
    let m = memos(ctx);
    if let Some(hit) = get(&m.procedural, &node.id) {
        return hit;
    }
    let procedural = is_procedural_type_text(&declared_type_text(node, ctx), ctx, 0);
    put(&m.procedural, node.id.clone(), procedural);
    procedural
}

/// Whether a written type is a routine's: written out, or named, with the name's declaration
/// saying so, through an alias to another such name; a name the project doesn't declare when the
/// RTL's names say so.
fn is_procedural_type_text(text: &str, ctx: &Ctx, depth: u32) -> bool {
    if re!(r"^(?:reference\s+to\s+)?(?:procedure|function)\b", "i").is_match(text) {
        return true;
    }
    let Some(type_name) = re!(r"^(?:type\s+)?([A-Za-z_][\w.]*)", "i")
        .captures(text)
        .and_then(|c| c.text(1).map(|t| js_lower(t.rsplit('.').next().unwrap_or(t))))
    else {
        return false;
    };
    if depth > 3 {
        return false;
    }
    let types = types_named(&type_name, ctx);
    if types.is_empty() {
        return is_rtl_procedural_type(&type_name) || re!(r"^t\w*(?:event|proc|method|callback)$").is_match(&type_name);
    }
    types.iter().any(|t| t.kind == NodeKind::TypeAlias && is_procedural_type_text(&aliased_type_text(t, ctx), ctx, depth + 1))
}

/// The project's Pascal types a name names, generic ones by the name before their parameters.
fn types_named(lower: &str, ctx: &Ctx) -> Vec<NodeRef> {
    let m = memos(ctx);
    let generic = {
        let cached = m.generic_types.borrow().as_ref().map(Rc::clone);
        match cached {
            Some(g) => g,
            None => {
                let mut generic: HashMap<String, Vec<NodeRef>> = HashMap::new();
                let kinds = [NodeKind::TypeAlias, NodeKind::Class, NodeKind::Struct, NodeKind::Interface];
                let mut seen: HashSet<String> = HashSet::new();
                for name in ctx.get_node_names_of_kinds(Language::Pascal, &kinds) {
                    if !name.contains('<') || !seen.insert(name.clone()) {
                        continue;
                    }
                    for n in ctx.get_nodes_by_name(&name).iter() {
                        let Some(open) = n.name.find('<') else { continue };
                        if n.language != Language::Pascal || open == 0 || !(n.kind == NodeKind::TypeAlias || is_class_kind(n.kind)) {
                            continue;
                        }
                        let base = js_lower(js_trim(&n.name[..open]));
                        generic.entry(base).or_default().push(Rc::clone(n));
                    }
                }
                let g = Rc::new(generic);
                *m.generic_types.borrow_mut() = Some(Rc::clone(&g));
                g
            }
        }
    };
    let mut out: Vec<NodeRef> =
        pascal_named(lower, ctx).iter().filter(|t| t.kind == NodeKind::TypeAlias || is_class_kind(t.kind)).cloned().collect();
    if let Some(more) = generic.get(lower) {
        out.extend(more.iter().cloned());
    }
    out
}

/// What a type declaration says after its `=`, which may start on the next line.
fn aliased_type_text(alias: &NodeRef, ctx: &Ctx) -> String {
    let lines = lines_of(ctx, &alias.file_path);
    let mut text = String::new();
    let start = alias.start_line as usize;
    let mut i = start.wrapping_sub(1);
    while i < start + 2 && i < lines.len() {
        text.push(' ');
        text.push_str(&strip_comments(&lines[i]));
        if let Some(eq) = text.find('=') {
            let after = js_trim(&text[eq + 1..]);
            if !after.is_empty() {
                return after.to_string();
            }
        }
        i += 1;
    }
    String::new()
}

/// Whether a routine has a parameter without a default value, which a call must pass.
fn needs_arguments(routine: &NodeRef) -> bool {
    let Some(signature) = routine.signature.as_deref().map(js_trim) else { return false };
    if !signature.starts_with('(') {
        return false;
    }
    let mut depth = 0i32;
    let mut end = signature.len();
    for (i, ch) in signature.char_indices() {
        if ch == '(' {
            depth += 1;
        } else if ch == ')' {
            depth -= 1;
            if depth == 0 {
                end = i;
                break;
            }
        }
    }
    signature[1..end].split(';').any(|param| !js_trim(param).is_empty() && !param.contains('='))
}

/// Whether a routine gives a value — a function or a constructor — rather than a procedure.
fn yields_value(routine: &NodeRef, ctx: &Ctx) -> bool {
    if routine.return_type.as_deref().is_some_and(|t| !t.is_empty()) {
        return true;
    }
    matches!(routine_keyword(routine, ctx).as_deref(), Some("function" | "constructor"))
}

/// The keyword that declares a routine, lowercased, or None.
fn routine_keyword(routine: &NodeRef, ctx: &Ctx) -> Option<String> {
    let m = memos(ctx);
    if let Some(hit) = get(&m.keywords, &routine.id) {
        return hit;
    }
    let lines = lines_of(ctx, &routine.file_path);
    let line = (routine.start_line as usize).checked_sub(1).and_then(|i| lines.get(i)).map(String::as_str).unwrap_or("");
    let keyword =
        re!(r"\b(function|procedure|constructor|destructor|operator)\s", "i").captures(line).and_then(|c| c.text(1).map(js_lower));
    put(&m.keywords, routine.id.clone(), keyword.clone());
    keyword
}

#[cfg(test)]
mod tests {
    use super::*;
    use codegraph_types::Node;

    fn routine(signature: Option<&str>) -> NodeRef {
        let mut n = Node::new("id", NodeKind::Function, "F", "F", "a.pas", Language::Pascal, 1, 1, 0, 0, 0);
        n.signature = signature.map(str::to_string);
        Rc::new(n)
    }

    #[test]
    fn needs_arguments_reads_defaults_and_empty_lists() {
        assert!(!needs_arguments(&routine(None)));
        assert!(!needs_arguments(&routine(Some(": Integer"))));
        assert!(!needs_arguments(&routine(Some("()"))));
        assert!(!needs_arguments(&routine(Some("(const A: string = 'x'; B: Integer = 1)"))));
        assert!(needs_arguments(&routine(Some("(A: Integer; B: Integer = 1)"))));
        assert!(needs_arguments(&routine(Some("  (Sender: TObject)"))));
    }

    #[test]
    fn parenless_names_and_condition_depths() {
        let c = parenless().captures("{call@TConsumer}.Before").unwrap();
        assert_eq!((c.text(1), c.text(2), c.text(3)), (Some("call"), Some("TConsumer"), Some("Before")));
        let c = parenless().captures("{read}.:TProvider.Value").unwrap();
        assert_eq!((c.text(1), c.text(2), c.text(3)), (Some("read"), None, Some(":TProvider.Value")));
        assert!(parenless().captures("{x}.Name").is_none());
        assert_eq!(condition_depth_after("{$IFDEF A}{$IFNDEF B}", 0), 2);
        assert_eq!(condition_depth_after("{$ENDIF}{$endif}{$IFEND}", 1), 0);
        assert_eq!(nearest(&["a/x/u.pas".into(), "b/u.pas".into()], "b/c.pas").as_deref(), Some("b/u.pas"));
        assert_eq!(strip_comments_and_strings("A := 'x'; // c"), "A :=  ;  ");
    }
}
