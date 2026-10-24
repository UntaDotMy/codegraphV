//! Dart scope rules: which members a receiver-less call reaches, which top-level declarations a
//! library sees, import-prefixed calls, annotations, getters read as members, and the member
//! lookup (supertypes, mixins, extensions) the call-chain typer is built on.

use super::dart_code::*;
use super::lang_shape::RefShape;
use super::members::{class_member_lines, class_member_type, declared_type_parameters};
use super::reach::is_lexically_reachable;
use super::util::*;
use crate::ctx::Ctx;
use crate::dart_libraries::{dart_import_prefixes, dart_library_sees, dart_prefix_sees, in_same_dart_library};
use crate::dart_local_scope::is_dart_locally_bound;
use crate::types::{is_inheritance_ref, NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use indexmap::IndexMap;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

/// An extension's rank: below every member of the receiver's own hierarchy.
pub const DART_EXTENSION_RANK: i64 = 1000;
/// The rank of an extension on a bare type parameter (`extension on T`), applicable to any project type.
pub const DART_UNIVERSAL_RANK: i64 = 100;
/// Stands for `Infinity`; far below `i64::MAX` so ranks can still be added to.
pub const INF: i64 = i64::MAX / 4;

/// `DART_TYPE_KINDS` (the Rust node kinds have no separate `mixin` / `extension`).
pub fn is_dart_type_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Class | NodeKind::Interface | NodeKind::Enum | NodeKind::Struct | NodeKind::Trait)
}

fn is_dart_type_decl(n: &Node) -> bool {
    n.language == Language::Dart && is_dart_type_kind(n.kind)
}

fn is_value_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Variable | NodeKind::Constant | NodeKind::Field | NodeKind::Property)
}

memo_map!(DartSupersMemo, String, Rc<Vec<String>>);
memo_map!(DartHierarchyMemo, String, Rc<HashMap<String, i64>>);
memo_map!(DartLineageMemo, String, Rc<IndexMap<String, i64>>);
memo_map!(DartExtensionOwnerMemo, String, Option<Rc<DartExtensionOwner>>);
memo_map!(DartGetterMemo, String, bool);
memo_map!(DartSetterMemo, String, bool);
memo_map!(DartConstructorMemo, String, bool);
memo_map!(DartReturnMemo, String, Option<Rc<(String, Vec<String>)>>);

/// The last `::`-separated segment of the owner path of `qualified_name` (the type a member belongs to).
fn dart_owner_name(qualified_name: &str) -> &str {
    let cut = qualified_name.rfind("::").unwrap_or(0);
    qualified_name[..cut].rsplit("::").next().unwrap_or("")
}

// -- Declaration heads -----------------------------------------------------------------------

pub struct DartHead {
    pub supers: Vec<String>,
    pub extension: bool,
}

const DART_HEAD_WORDS: [&str; 4] = ["extends", "with", "implements", "on"];

/// The types a Dart declaration lists after `extends` / `with` / `implements` / `on`, and
/// whether it is an extension.
pub fn dart_head_of(decl: &Node, ctx: &Ctx) -> DartHead {
    let lines = lines_of(ctx, &decl.file_path);
    let from = decl.start_line as i64 - 1;
    let to = decl.end_line.min(decl.start_line + 40) as i64;
    let joined = js_slice(&lines, from, to).join("\n");
    let text = re!(r"/\*[\s\S]*?\*/", "g").replace_all_str(&joined, " ");
    let text = re!(r"//[^\n]*", "g").replace_all_str(&text, " ");
    let mut depth = 0i32;
    let mut flat = String::new();
    for ch in text.chars() {
        if ch == '<' {
            depth += 1;
        } else if ch == '>' {
            depth = (depth - 1).max(0);
        } else if depth == 0 {
            if ch == '{' || ch == ';' {
                break;
            }
            flat.push(ch);
        }
    }
    let keyword = re!(r"\b(class|mixin|extension|enum)\b").captures(&flat);
    let head: &str = match &keyword {
        Some(k) => &flat[k.get(0).map_or(0, |g| g.start())..],
        None => &flat,
    };
    let clause = re!(r"(?:\b(?:extends|with|implements|on)\b|=)([\s\S]*)$").captures(head).and_then(|c| c.text(1)).unwrap_or("");
    let supers = re!(r"[A-Za-z_$][\w$]*", "g")
        .captures_iter(clause)
        .filter_map(|m| m.get(0).map(|g| g.as_str()))
        .filter(|w| !DART_HEAD_WORDS.contains(w))
        .map(String::from)
        .collect();
    let extension = keyword.as_ref().and_then(|k| k.text(1)) == Some("extension") && !re!(r"^extension\s+type\b").is_match(head);
    DartHead { supers, extension }
}

/// The types `type_name` declares as supertypes (extends, mixins, implements, `on`).
pub fn dart_supertypes_rc(type_name: &str, ctx: &Ctx) -> Rc<Vec<String>> {
    memoized::<DartSupersMemo>(ctx, type_name.to_string(), || {
        let mut names: Vec<String> = Vec::new();
        for decl in ctx.get_nodes_by_name(type_name).iter() {
            if is_dart_type_decl(decl) {
                names.extend(dart_head_of(decl, ctx).supers);
            }
        }
        Rc::new(names)
    })
}

pub fn dart_supertypes_of(type_name: &str, ctx: &Ctx) -> Vec<String> {
    dart_supertypes_rc(type_name, ctx).to_vec()
}

/// `type_name` and everything it inherits from, with the number of hops to each.
pub fn dart_lineage(type_name: &str, ctx: &Ctx) -> Rc<IndexMap<String, i64>> {
    memoized::<DartLineageMemo>(ctx, type_name.to_string(), || {
        let mut depths: IndexMap<String, i64> = IndexMap::new();
        let mut queue: VecDeque<(String, i64)> = VecDeque::from([(type_name.to_string(), 0)]);
        while !queue.is_empty() && depths.len() < 40 {
            let (name, depth) = queue.pop_front().unwrap();
            if depths.contains_key(&name) {
                continue;
            }
            depths.insert(name.clone(), depth);
            for sup in dart_supertypes_rc(&name, ctx).iter() {
                queue.push_back((sup.clone(), depth + 1));
            }
        }
        Rc::new(depths)
    })
}

/// The types enclosing the reference's line, and everything they inherit from, with the hops to each.
pub fn dart_hierarchy_at(r: &UnresolvedRef, ctx: &Ctx) -> Rc<HashMap<String, i64>> {
    memoized::<DartHierarchyMemo>(ctx, format!("{}:{}", r.file_path, r.line), || {
        let mut depths: HashMap<String, i64> = HashMap::new();
        let mut queue: VecDeque<(String, i64)> = ctx
            .get_nodes_in_file(&r.file_path)
            .iter()
            .filter(|n| is_dart_type_kind(n.kind) && n.start_line <= r.line && n.end_line >= r.line)
            .map(|n| (n.name.clone(), 0))
            .collect();
        while !queue.is_empty() && depths.len() < 40 {
            let (name, depth) = queue.pop_front().unwrap();
            if depths.contains_key(&name) {
                continue;
            }
            depths.insert(name.clone(), depth);
            for sup in dart_supertypes_rc(&name, ctx).iter() {
                queue.push_back((sup.clone(), depth + 1));
            }
        }
        Rc::new(depths)
    })
}

// -- Extensions ------------------------------------------------------------------------------

/// `dartExtensionDecl`: for a class node that is a Dart `extension`, whether it is named.
pub fn dart_extension_decl(n: &Node, ctx: &Ctx) -> Option<bool> {
    if n.language != Language::Dart || n.kind != NodeKind::Class {
        return None;
    }
    let lines = lines_of(ctx, &n.file_path);
    let joined = js_slice(&lines, n.start_line as i64 - 1, n.end_line.min(n.start_line + 5) as i64).join("\n");
    let head = re!(r"^\s*(?:(?:@[\w$.]+(?:\s*\((?:[^()]|\([^()]*\))*\))?|//[^\n]*)\s*)*").replace_first(&joined, |_| String::new());
    if !re!(r"^extension\b(?!\s+type\b)").is_match(&head) {
        return None;
    }
    Some(!re!(r"^extension\s*(?:<(?:[^<>]|<(?:[^<>]|<[^<>]*>)*>)*>)?\s*on\b").is_match(&head))
}

/// `dartExtensionDecl(n)?.named`.
pub fn dart_extension_named(n: &Node, ctx: &Ctx) -> Option<bool> {
    dart_extension_decl(n, ctx)
}

/// The class node of the extension declaration a member sits in.
fn dart_extension_class_of(member: &Node, ctx: &Ctx) -> Option<NodeRef> {
    let cut = member.qualified_name.rfind("::").filter(|c| *c > 0)?;
    let owner_qn = &member.qualified_name[..cut];
    ctx.get_nodes_in_file(&member.file_path)
        .iter()
        .find(|n| n.qualified_name == owner_qn && n.kind == NodeKind::Class && n.start_line <= member.start_line && n.end_line >= member.start_line)
        .cloned()
}

/// Is the method a member of an `extension on T { … }` with no name?
pub fn is_dart_unnamed_extension_member(method: &Node, ctx: &Ctx) -> bool {
    if method.language != Language::Dart || method.kind != NodeKind::Method {
        return false;
    }
    dart_extension_class_of(method, ctx).is_some_and(|owner| dart_extension_decl(&owner, ctx) == Some(false))
}

pub struct DartExtensionOwner {
    pub on: Vec<String>,
    pub named: bool,
    pub universal: bool,
}

fn dart_extension_owner(member: &Node, ctx: &Ctx) -> Option<Rc<DartExtensionOwner>> {
    memoized::<DartExtensionOwnerMemo>(ctx, member.id.clone(), || {
        let owner = dart_extension_class_of(member, ctx)?;
        let named = dart_extension_decl(&owner, ctx)?;
        let on = dart_head_of(&owner, ctx).supers;
        let universal = on.len() == 1 && declared_type_parameters(&owner, ctx).contains(&on[0]);
        Some(Rc::new(DartExtensionOwner { on, named, universal }))
    })
}

// -- Member classification -------------------------------------------------------------------

/// Is the node a member (a method, or a function nested in a type)?
pub fn is_dart_member(n: &Node) -> bool {
    n.kind == NodeKind::Method || (n.kind == NodeKind::Function && n.qualified_name.contains("::"))
}

/// A field, constant or enum value of a type (reachable bare inside it).
pub fn is_dart_type_value(n: &Node) -> bool {
    n.language == Language::Dart && (n.kind == NodeKind::EnumMember || is_value_kind(n.kind)) && n.qualified_name.contains("::")
}

fn is_dart_library_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Function | NodeKind::Class | NodeKind::Enum | NodeKind::TypeAlias | NodeKind::Constant | NodeKind::Variable | NodeKind::Interface | NodeKind::Struct | NodeKind::Trait
    )
}

/// A top-level declaration (visible library-wide, subject to imports).
pub fn is_dart_library_decl(n: &Node) -> bool {
    n.language == Language::Dart && is_dart_library_kind(n.kind) && !n.qualified_name.contains("::")
}

/// Is the candidate visible to the reference through the library's imports and parts?
pub fn is_dart_top_level_visible(candidate: &Node, r: &UnresolvedRef, ctx: &Ctx, prefix: Option<&str>) -> bool {
    if !is_dart_library_decl(candidate) {
        return true;
    }
    match prefix {
        None => dart_library_sees(&r.file_path, &candidate.file_path, &candidate.name, ctx),
        Some(p) => dart_prefix_sees(&r.file_path, p, &candidate.file_path, &candidate.name, ctx),
    }
}

fn file_line(ctx: &Ctx, file: &str, line: u32) -> Option<String> {
    line_at(ctx, file, line)
}

/// Is the node a constructor (`Owner.name(…)`, possibly `const` / `factory` / `external`)?
pub fn is_dart_constructor(n: &Node, ctx: &Ctx) -> bool {
    if n.language != Language::Dart || n.kind != NodeKind::Method {
        return false;
    }
    memoized::<DartConstructorMemo>(ctx, n.id.clone(), || {
        let owner = if n.qualified_name.rfind("::").is_some_and(|c| c > 0) { dart_owner_name(&n.qualified_name) } else { "" };
        let line = file_line(ctx, &n.file_path, n.start_line).unwrap_or_default();
        let head = after_col(&line, n.start_column);
        !owner.is_empty()
            && Re::cached(&format!(r"^(?:(?:const|factory|external)\s+)*{}\s*\.\s*{}(?![\w$])", escape_regex(owner), escape_regex(&n.name)), "").is_match(head)
    })
}

/// A getter (`T get name`)?
pub fn is_dart_getter(n: &Node, ctx: &Ctx) -> bool {
    if n.language != Language::Dart || n.kind != NodeKind::Method {
        return false;
    }
    memoized::<DartGetterMemo>(ctx, n.id.clone(), || {
        let lines = lines_of(ctx, &n.file_path);
        let first = lines.get(n.start_line as usize - 1).map(String::as_str).unwrap_or("");
        let next = lines.get(n.start_line as usize).map(String::as_str).unwrap_or("");
        let head = format!("{} {}", after_col(first, n.start_column), next);
        Re::cached(&format!(r"^[^={{;]*?\bget\s+{}(?![\w$])", escape_regex(&n.name)), "").is_match(&head)
    })
}

/// A setter (`set name(…)`)?
pub fn is_dart_setter(n: &Node, ctx: &Ctx) -> bool {
    if n.language != Language::Dart || n.kind != NodeKind::Method {
        return false;
    }
    memoized::<DartSetterMemo>(ctx, n.id.clone(), || {
        let line = file_line(ctx, &n.file_path, n.start_line).unwrap_or_default();
        Re::cached(&format!(r"^[^={{;]*?\bset\s+{}\s*\(", escape_regex(&n.name)), "").is_match(after_col(&line, n.start_column))
    })
}

/// A member that needs no instance: `static`, a constant, an enum value or a constructor.
fn is_dart_static_member(m: &Node, ctx: &Ctx) -> bool {
    m.is_static == Some(true) || m.kind == NodeKind::Constant || m.kind == NodeKind::EnumMember || is_dart_constructor(m, ctx)
}

// -- Scope of receiver-less calls ------------------------------------------------------------

/// How far up the hierarchy at the reference the method's owner is (`INF` when unreachable).
pub fn dart_member_depth(method: &Node, r: &UnresolvedRef, ctx: &Ctx) -> i64 {
    let Some(_cut) = method.qualified_name.rfind("::") else { return 0 };
    let owner = dart_owner_name(&method.qualified_name);
    let hierarchy = dart_hierarchy_at(r, ctx);
    if let Some(own) = hierarchy.get(owner) {
        return *own;
    }
    let decl = ctx
        .get_nodes_in_file(&method.file_path)
        .iter()
        .find(|n| n.name == owner && is_dart_type_kind(n.kind) && n.start_line <= method.start_line && n.end_line >= method.end_line)
        .cloned();
    let Some(decl) = decl else { return INF };
    let head = dart_head_of(&decl, ctx);
    if !head.extension {
        return INF;
    }
    let on = head.supers.iter().map(|t| hierarchy.get(t).copied().unwrap_or(INF)).min().unwrap_or(INF);
    if on >= INF {
        INF
    } else {
        DART_EXTENSION_RANK + on
    }
}

pub fn is_dart_method_in_scope(method: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    dart_member_depth(method, r, ctx) < INF
}

/// Of same-named members, the ones nearest the reference's own type.
pub fn nearest_dart_members(candidates: &[NodeRef], r: &UnresolvedRef, ctx: &Ctx) -> Vec<NodeRef> {
    let members: Vec<&NodeRef> = candidates.iter().filter(|n| is_dart_member(n)).collect();
    if members.len() < 2 {
        return candidates.to_vec();
    }
    let depth: HashMap<&str, i64> = members.iter().map(|n| (n.id.as_str(), dart_member_depth(n, r, ctx))).collect();
    let nearest = depth.values().copied().min().unwrap_or(INF);
    candidates.iter().filter(|n| depth.get(n.id.as_str()).is_none_or(|d| *d == nearest)).cloned().collect()
}

/// Of same-named top-level declarations, the ones in the reference's own library when it has one.
pub fn prefer_own_dart_library(candidates: &[NodeRef], r: &UnresolvedRef, ctx: &Ctx) -> Vec<NodeRef> {
    if candidates.iter().filter(|n| is_dart_library_decl(n)).count() < 2 {
        return candidates.to_vec();
    }
    let own: Vec<&NodeRef> = candidates
        .iter()
        .filter(|n| is_dart_library_decl(n) && in_same_dart_library(&r.file_path, &n.file_path, ctx) && is_lexically_reachable(n, r, ctx))
        .collect();
    if own.is_empty() {
        return candidates.to_vec();
    }
    candidates.iter().filter(|n| !is_dart_library_decl(n) || own.iter().any(|o| o.id == n.id)).cloned().collect()
}

/// The import prefix written directly before the reference's name (`p.Foo` → `p`).
pub fn dart_import_prefix_of(r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let line = file_line(ctx, &r.file_path, r.line)?;
    let name = r.reference_name.as_str();
    let is_word = |at: usize| -> bool {
        line[at..].starts_with(name)
            && !char_before(&line, at).is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
            && !line[at + name.len()..].chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
    };
    let col = col_byte(&line, r.column);
    let mut start: Option<usize> = if col <= line.len() && line.is_char_boundary(col) && is_word(col) { Some(col) } else { None };
    if start.is_none() && !name.is_empty() {
        let mut from = 0;
        while let Some(rel) = line[from..].find(name) {
            let at = from + rel;
            if is_word(at) {
                start = Some(at);
                break;
            }
            from = at + line[at..].chars().next().map_or(1, |c| c.len_utf8());
        }
    }
    let start = start?;
    re!(r"([A-Za-z_$][\w$]*)\s*\.\s*$").captures(&line[..start]).and_then(|c| c.text(1).map(String::from))
}

/// Is the call written without a receiver (`foo()` rather than `x.foo()`)?
pub fn is_receiver_less_dart_call(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let Some(line) = file_line(ctx, &r.file_path, r.line) else { return true };
    let name = r.reference_name.as_str();
    let name_len = codegraph_types::jscompat::utf16_len(name) as u32;
    let start: Option<usize> = if starts_with_at(&line, name, r.column) {
        Some(col_byte(&line, r.column))
    } else if r.column >= name_len && starts_with_at(&line, name, r.column - name_len) {
        Some(col_byte(&line, r.column - name_len))
    } else {
        line.find(name)
    };
    let Some(start) = start else { return true };
    let before = &line[..start];
    !re!(r"\.\s*$").is_match(before) || re!(r"(?:^|[^.])\.\.\.\s*$").is_match(before)
}

// -- Import-prefixed names -------------------------------------------------------------------

/// The Dart import prefixes of the file that name a member: `p.name`.
pub fn dart_member_read_parts(name: &str) -> Option<(String, String)> {
    let c = re!(r"^([A-Za-z_$][\w$]*)\.([A-Za-z_$][\w$]*)$").captures(name)?;
    Some((c.text(1)?.to_string(), c.text(2)?.to_string()))
}

pub fn dart_prefixed_decls(prefix: &str, name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Vec<NodeRef> {
    ctx.get_nodes_by_name(name)
        .iter()
        .filter(|n| is_dart_library_decl(n) && dart_prefix_sees(&r.file_path, prefix, &n.file_path, name, ctx))
        .cloned()
        .collect()
}

/// Where `prefix.name` starts in the file, or -1 when the name is not written directly after `prefix.`.
fn dart_prefixed_call_site(r: &UnresolvedRef, prefix: &str, name: &str, ctx: &Ctx) -> i64 {
    let Some(code) = dart_code_of(&r.file_path, ctx) else { return -1 };
    let start = dart_name_start(r, name, &code);
    if start < 0 {
        return -1;
    }
    let text = code.bytes();
    let mut k = start - 1;
    while k >= 0 && is_ws(tb(text, k)) {
        k -= 1;
    }
    if tb(text, k) != b'.' || tb(text, k - 1) == b'.' || tb(text, k - 1) == b'?' {
        return -1;
    }
    k -= 1;
    while k >= 0 && is_ws(tb(text, k)) {
        k -= 1;
    }
    let end = k + 1;
    while k >= 0 && is_wordish(tb(text, k)) {
        k -= 1;
    }
    if sl(&code.text, k + 1, end) != prefix {
        return -1;
    }
    while k >= 0 && is_ws(tb(text, k)) {
        k -= 1;
    }
    if tb(text, k) == b'.' {
        -1
    } else {
        end - prefix.len() as i64
    }
}

/// Is `name` bound where the reference is — by a local, or by a member of the enclosing type?
pub fn is_dart_name_hidden(name: &str, r: &UnresolvedRef, at: usize, ctx: &Ctx) -> bool {
    if is_dart_locally_bound(&r.file_path, name, at, ctx) {
        return true;
    }
    let Some(own) = dart_enclosing_type(r, ctx) else { return false };
    class_member_type(&own, name, ctx).is_some() || {
        let qn = format!("{}::{}", own.qualified_name, name);
        ctx.get_nodes_by_name(name).iter().any(|n| n.file_path == own.file_path && n.qualified_name == qn)
    }
}

/// `prefix.name(…)` where `prefix` is an import prefix: the one top-level declaration it names.
pub fn match_dart_prefixed_call(r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    let Some((prefix, name)) = dart_member_read_parts(&r.reference_name) else { return Tri::Undefined };
    if !dart_import_prefixes(&r.file_path, ctx).contains(&prefix) {
        return Tri::Undefined;
    }
    let at = dart_prefixed_call_site(r, &prefix, &name, ctx);
    if at < 0 {
        return Tri::Null;
    }
    if is_dart_name_hidden(&prefix, r, at as usize, ctx) {
        return Tri::Undefined;
    }
    let found = dart_prefixed_decls(&prefix, &name, r, ctx);
    if found.len() == 1 {
        Tri::Value(ResolvedRef::new(r, found[0].id.clone(), 0.9, ResolvedBy::Import))
    } else {
        Tri::Null
    }
}

/// The import prefix a `Type.member(…)` call is qualified with (`p.Type.member(…)`), if any.
fn dart_type_call_prefix(type_name: &str, member: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let prefixes = dart_import_prefixes(&r.file_path, ctx);
    if prefixes.is_empty() {
        return None;
    }
    let line = file_line(ctx, &r.file_path, r.line)?;
    let written = Re::cached(
        &format!(
            r"(?<![\w$.])([A-Za-z_$][\w$]*)\s*\.\s*{}\s*(?:<(?:[^<>]|<(?:[^<>]|<[^<>]*>)*>)*>)?\s*\.\s*{}(?![\w$])",
            escape_regex(type_name),
            escape_regex(member)
        ),
        "g",
    );
    let found: Vec<(usize, &str)> = written
        .captures_iter(&line)
        .filter_map(|m| Some((m.get(0)?.start(), m.text(1)?)))
        .filter(|(_, p)| prefixes.contains(*p))
        .collect();
    let col = col_byte(&line, r.column);
    found.iter().find(|(i, _)| *i == col).or(found.first()).map(|(_, p)| p.to_string())
}

// -- Members ---------------------------------------------------------------------------------

/// The innermost type whose body holds the reference.
pub fn dart_enclosing_type(r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    let mut own: Option<NodeRef> = None;
    for n in ctx.get_nodes_in_file(&r.file_path).iter() {
        if is_dart_type_kind(n.kind) && n.start_line <= r.line && n.end_line >= r.line && own.as_ref().is_none_or(|o| n.start_line >= o.start_line) {
            own = Some(n.clone());
        }
    }
    own
}

fn closest_to(r: &UnresolvedRef, best: &[NodeRef]) -> Option<NodeRef> {
    if let Some(same) = best.iter().find(|n| n.file_path == r.file_path) {
        return Some(same.clone());
    }
    let mut it = best.iter();
    let mut acc = it.next()?;
    for b in it {
        if compute_path_proximity(&r.file_path, &b.file_path) > compute_path_proximity(&r.file_path, &acc.file_path) {
            acc = b;
        }
    }
    Some(acc.clone())
}

/// The member `name` of `type_name` that a call on it reaches: its own, an inherited one, or an
/// extension's. `accept` filters candidates; hierarchy depths below `min_depth` are skipped.
pub fn dart_member_of(type_name: &str, name: &str, r: &UnresolvedRef, ctx: &Ctx, accept: &dyn Fn(&Node) -> bool, min_depth: i64) -> Option<(NodeRef, bool)> {
    let lineage = dart_lineage(type_name, ctx);
    let depth_of = |t: &str| -> i64 {
        match lineage.get(t) {
            Some(d) if *d >= min_depth => *d,
            _ => INF,
        }
    };
    let mut best: Vec<NodeRef> = Vec::new();
    let mut best_rank = INF;
    let mut is_project_type: Option<bool> = None;
    for m in ctx.get_nodes_by_name(name).iter() {
        if m.language != Language::Dart || !is_dart_member(m) || !accept(m) {
            continue;
        }
        let rank: i64;
        if let Some(extension) = dart_extension_owner(m, ctx) {
            if !extension.named && !in_same_dart_library(&r.file_path, &m.file_path, ctx) {
                continue;
            }
            let on = if extension.universal {
                let project = *is_project_type.get_or_insert_with(|| ctx.get_nodes_by_name(type_name).iter().any(|n| is_dart_type_decl(n)));
                if project {
                    DART_UNIVERSAL_RANK
                } else {
                    INF
                }
            } else {
                extension.on.iter().map(|t| depth_of(t)).min().unwrap_or(INF)
            };
            if on >= INF {
                continue;
            }
            rank = DART_EXTENSION_RANK + on;
        } else {
            let depth = depth_of(dart_owner_name(&m.qualified_name));
            if depth >= INF {
                continue;
            }
            rank = depth;
        }
        if rank < best_rank {
            best_rank = rank;
            best = vec![m.clone()];
        } else if rank == best_rank {
            best.push(m.clone());
        }
    }
    if best.is_empty() {
        return None;
    }
    let node = closest_to(r, &best)?;
    Some((node, best_rank >= DART_EXTENSION_RANK))
}

/// The extension method (not an own or inherited member) a call on `type_name` reaches.
pub fn dart_extension_member_of(type_name: &str, name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    match dart_member_of(type_name, name, r, ctx, &|_| true, 0) {
        Some((node, true)) => Some(node),
        _ => None,
    }
}

/// A static member (method, constant, enum value) of the type named `type_name`.
pub fn dart_static_member(type_name: &str, name: &str, r: &UnresolvedRef, ctx: &Ctx, is_owner: &dyn Fn(&Node) -> bool) -> Option<NodeRef> {
    let owners: Vec<NodeRef> = ctx.get_nodes_by_name(type_name).iter().filter(|n| is_dart_type_decl(n) && is_owner(n)).cloned().collect();
    if owners.is_empty() {
        return None;
    }
    let found: Vec<NodeRef> = ctx
        .get_nodes_by_name(name)
        .iter()
        .filter(|m| {
            m.language == Language::Dart
                && matches!(m.kind, NodeKind::Method | NodeKind::Function | NodeKind::Constant | NodeKind::EnumMember)
                && owners.iter().any(|o| o.file_path == m.file_path && m.qualified_name == format!("{}::{}", o.qualified_name, name))
                && !is_dart_setter(m, ctx)
                && is_dart_static_member(m, ctx)
        })
        .cloned()
        .collect();
    if found.len() <= 1 {
        return found.into_iter().next();
    }
    closest_to(r, &found)
}

/// The files of the library the file belongs to (itself, its parts, and the library it is a part of).
pub fn dart_library_files(file_path: &str, ctx: &Ctx) -> std::collections::HashSet<String> {
    let mut files: std::collections::HashSet<String> = std::collections::HashSet::from([file_path.to_string()]);
    let code = dart_code_of(file_path, ctx);
    if let Some(code) = &code {
        files.extend(code.parts.iter().cloned());
        if let Some(part_of) = &code.part_of {
            files.insert(part_of.clone());
            if let Some(owner) = dart_code_of(part_of, ctx) {
                files.extend(owner.parts.iter().cloned());
            }
        }
    }
    files
}

/// Of same-named top-level declarations, the one the reference means: its own file's, the
/// library's, or the only one.
pub fn dart_library_pick(nodes: &[NodeRef], r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    if let Some(own) = nodes.iter().find(|n| n.file_path == r.file_path) {
        return Some(own.clone());
    }
    let library = dart_library_files(&r.file_path, ctx);
    let shared: Vec<&NodeRef> = nodes.iter().filter(|n| library.contains(&n.file_path)).collect();
    if shared.len() == 1 {
        return Some(shared[0].clone());
    }
    if nodes.len() == 1 {
        Some(nodes[0].clone())
    } else {
        None
    }
}

/// The function `name(…)` a receiver-less call reaches: a member of the enclosing type, else a library function.
pub fn dart_function_named(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    let own = dart_enclosing_type(r, ctx);
    let member = own.and_then(|o| dart_member_of(&o.name, name, r, ctx, &|n| !is_dart_getter(n, ctx) && !is_dart_setter(n, ctx), 0)).map(|(n, _)| n);
    if member.is_some() {
        return member;
    }
    let functions: Vec<NodeRef> = ctx
        .get_nodes_by_name(name)
        .iter()
        .filter(|n| n.language == Language::Dart && n.kind == NodeKind::Function && !n.qualified_name.contains("::"))
        .cloned()
        .collect();
    dart_library_pick(&functions, r, ctx)
}

/// The return type (or getter / field type) of a function, resolved against `type_args` when it is a type parameter.
pub fn dart_return_type(f: &Node, type_args: &[String], ctx: &Ctx) -> Option<String> {
    let info = memoized::<DartReturnMemo>(ctx, f.id.clone(), || {
        let ty = f.return_type.as_deref().and_then(dart_simple_type_name)?;
        let own = declared_type_parameters(f, ctx);
        let cut = f.qualified_name.rfind("::");
        let owner = match cut {
            Some(c) if c > 0 && !own.contains(&ty) => {
                let owner_qn = &f.qualified_name[..c];
                ctx.get_nodes_in_file(&f.file_path)
                    .iter()
                    .find(|n| is_dart_type_kind(n.kind) && n.qualified_name == owner_qn && n.start_line <= f.start_line && n.end_line >= f.end_line)
                    .cloned()
            }
            _ => None,
        };
        match owner {
            Some(o) if declared_type_parameters(&o, ctx).contains(&ty) => None,
            _ => Some(Rc::new((ty, own))),
        }
    })?;
    let (ty, own) = (&info.0, &info.1);
    match own.iter().position(|p| p == ty) {
        None => Some(ty.clone()),
        Some(at) => type_args.get(at).filter(|a| !a.is_empty()).and_then(|a| dart_simple_type_name(a)),
    }
}

/// The declared type of the field `name` of `type_name` (or of an inherited type), from a
/// typed declaration, a getter, or the initializer of an inferred one.
pub fn dart_field_type(type_name: &str, name: &str, r: &UnresolvedRef, ctx: &Ctx, depth: usize) -> Option<String> {
    let lineage = dart_lineage(type_name, ctx);
    let mut ordered: Vec<(&String, &i64)> = lineage.iter().collect();
    ordered.sort_by_key(|(_, d)| **d);
    for (owner, _) in ordered {
        for decl in ctx.get_nodes_by_name(owner).iter() {
            if !is_dart_type_decl(decl) {
                continue;
            }
            if let Some(declared) = class_member_type(decl, name, ctx) {
                return dart_simple_type_name(&declared);
            }
            let getter = Re::cached(
                &format!(r"(?:^|[\s;{{}}])([A-Za-z_$][\w$]*)\s*(?:<[^;{{}}=]*>)?\s*\??\s+get\s+{}\s*;", escape_regex(name)),
                "",
            );
            for (text, _) in class_member_lines(decl, ctx).iter() {
                if !text.contains(name) {
                    continue;
                }
                if let Some(m) = getter.captures(text) {
                    let ty = m.text(1).unwrap_or("");
                    if !super::dart_chain::is_non_type_word(ty) {
                        return dart_simple_type_name(ty);
                    }
                }
            }
            let Some(code) = dart_code_of(&decl.file_path, ctx) else { continue };
            let Some(from) = code.line_start(decl.start_line) else { continue };
            let to = code.line_start_at(decl.end_line as usize).map_or(code.text.len(), |t| t);
            let init_re = Re::cached(&format!(r"\b(?:final|var|late)\s+{}\s*=(?![=>])", escape_regex(name)), "");
            if let Some(init) = init_re.find(sl(&code.text, from as i64, to as i64)) {
                let site = UnresolvedRef { file_path: decl.file_path.clone(), line: decl.start_line, column: 0, ..r.clone() };
                return super::dart_chain::dart_initializer_type(&code.text, from + init.end(), &site, ctx, depth + 1);
            }
        }
    }
    None
}

// -- Member reads (getters) -------------------------------------------------------------------

/// `a.b` written as a reference (not a call): a possible getter read.
pub fn is_dart_member_read(r: &UnresolvedRef) -> bool {
    r.language == Language::Dart && is_kind(r, EdgeKind::References) && dart_member_read_parts(&r.reference_name).is_some()
}

/// `receiver.member` where `member` is a getter of the receiver's type: a call to it.
pub fn match_dart_member_read(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let (receiver, member) = dart_member_read_parts(&r.reference_name)?;
    if !ctx.get_nodes_by_name(&member).iter().any(|n| is_dart_getter(n, ctx)) {
        return None;
    }
    let type_name = if receiver.starts_with(|c: char| c.is_ascii_uppercase()) && ctx.get_nodes_by_name(&receiver).iter().any(|n| is_dart_type_decl(n)) {
        Some(receiver.clone())
    } else {
        super::infer::infer_local_receiver_type(&receiver, r, ctx, None).or_else(|| super::members::infer_member_receiver_type(&receiver, r, ctx))
    }?;
    let (node, _) = dart_member_of(&type_name, &member, r, ctx, &|n| is_dart_getter(n, ctx), 0)?;
    let mut resolved = ResolvedRef::new(r, node.id.clone(), 0.9, ResolvedBy::InstanceMethod);
    resolved.edge_kind = Some(EdgeKind::Calls);
    Some(resolved)
}

// -- Annotations ------------------------------------------------------------------------------

pub fn is_dart_annotation(r: &UnresolvedRef) -> bool {
    r.language == Language::Dart && is_kind(r, EdgeKind::Decorates)
}

fn is_dart_annotation_class(n: &Node, ctx: &Ctx) -> bool {
    n.kind == NodeKind::Class && dart_extension_decl(n, ctx).is_none()
}

/// The constant or enum value `name` of the type the annotation sits in.
fn dart_enclosing_static(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    let mut own: Option<NodeRef> = None;
    for n in ctx.get_nodes_in_file(&r.file_path).iter() {
        if is_dart_type_kind(n.kind) && n.id != r.from_node_id && n.start_line <= r.line && n.end_line >= r.line && own.as_ref().is_none_or(|o| n.start_line >= o.start_line) {
            own = Some(n.clone());
        }
    }
    let own = own?;
    let qn = format!("{}::{}", own.qualified_name, name);
    ctx.get_nodes_by_name(name)
        .iter()
        .find(|m| m.language == Language::Dart && m.file_path == r.file_path && m.qualified_name == qn && matches!(m.kind, NodeKind::Constant | NodeKind::EnumMember))
        .cloned()
}

struct DartAnnotation {
    names: Vec<String>,
    type_args_at: i64,
    call: bool,
}

fn read_dart_annotation(text: &[u8], mut i: usize) -> Option<DartAnnotation> {
    let mut names: Vec<String> = Vec::new();
    let mut type_args_at: i64 = -1;
    let blank = |c: u8| c == b' ' || c == b'\t';
    while names.len() < 8 {
        while blank(tb(text, i as i64)) {
            i += 1;
        }
        let start = i;
        let first = tb(text, i as i64);
        if !(first.is_ascii_alphabetic() || first == b'_' || first == b'$') {
            return None;
        }
        while is_wordish(tb(text, i as i64)) {
            i += 1;
        }
        names.push(String::from_utf8_lossy(&text[start..i]).into_owned());
        while blank(tb(text, i as i64)) {
            i += 1;
        }
        if tb(text, i as i64) == b'<' && type_args_at < 0 {
            let close = dart_type_arguments_end(text, i);
            if close < 0 {
                return None;
            }
            type_args_at = names.len() as i64 - 1;
            i = close as usize + 1;
            while blank(tb(text, i as i64)) {
                i += 1;
            }
        }
        if tb(text, i as i64) != b'.' {
            break;
        }
        i += 1;
    }
    Some(DartAnnotation { names, type_args_at, call: tb(text, i as i64) == b'(' })
}

/// The annotation written at the reference: `@a.b.C(…)` → names `[a, b, C]`, with `call` set.
fn dart_annotation_at(r: &UnresolvedRef, ctx: &Ctx) -> Option<(Vec<String>, bool)> {
    let code = dart_code_of(&r.file_path, ctx)?;
    let from = code.line_start(r.line)?;
    let to = code.line_starts.get(r.line as usize).copied().unwrap_or(code.text.len());
    let mut ats: Vec<usize> = vec![from + r.column as usize];
    let text = code.bytes();
    let mut k = text[from.min(text.len())..].iter().position(|b| *b == b'@').map(|p| from + p);
    while let Some(pos) = k {
        if pos >= to {
            break;
        }
        if pos != from + r.column as usize {
            ats.push(pos);
        }
        k = text[pos + 1..].iter().position(|b| *b == b'@').map(|p| pos + 1 + p);
    }
    for at in ats {
        let read = if tb(text, at as i64) == b'@' { read_dart_annotation(text, at + 1) } else { None };
        let Some(read) = read else { continue };
        let end = if read.type_args_at >= 0 { read.type_args_at } else { read.names.len() as i64 - 1 } as usize;
        if read.names.get(end).is_some_and(|n| *n == r.reference_name) {
            return Some((read.names[..=end].to_vec(), read.call));
        }
    }
    None
}

/// `@Name`, `@Name(…)`, `@Type.name`, `@p.Name(…)`: the declaration the annotation names.
pub fn match_dart_annotation(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let name = r.reference_name.as_str();
    if !ctx.get_nodes_by_name(name).iter().any(|n| n.language == Language::Dart) {
        return None;
    }
    let (names, call) = dart_annotation_at(r, ctx)?;
    let prefixes = dart_import_prefixes(&r.file_path, ctx);
    let mut resolved_by = ResolvedBy::ExactMatch;
    let mut pool: Vec<NodeRef>;
    if names.len() == 1 || (names.len() == 2 && prefixes.contains(&names[0])) {
        let prefix: Option<&str> = if names.len() == 2 { Some(names[0].as_str()) } else { None };
        let own = if prefix.is_none() && !call { dart_enclosing_static(name, r, ctx) } else { None };
        if let Some(own) = own {
            return Some(ResolvedRef::new(r, own.id.clone(), 0.9, resolved_by));
        }
        pool = ctx
            .get_nodes_by_name(name)
            .iter()
            .filter(|n| {
                is_dart_library_decl(n)
                    && (if call { is_dart_annotation_class(n, ctx) } else { n.kind == NodeKind::Constant })
                    && is_dart_top_level_visible(n, r, ctx, prefix)
            })
            .cloned()
            .collect();
        if prefix.is_none() {
            pool = prefer_own_dart_library(&pool, r, ctx);
        }
    } else if names.len() == 2 || (names.len() == 3 && prefixes.contains(&names[0])) {
        let prefix: Option<&str> = if names.len() == 3 { Some(names[0].as_str()) } else { None };
        let mut owners: Vec<NodeRef> = ctx
            .get_nodes_by_name(&names[names.len() - 2])
            .iter()
            .filter(|n| is_dart_library_decl(n) && is_dart_type_kind(n.kind) && is_dart_top_level_visible(n, r, ctx, prefix))
            .cloned()
            .collect();
        if prefix.is_none() {
            owners = prefer_own_dart_library(&owners, r, ctx);
        }
        pool = ctx
            .get_nodes_by_name(name)
            .iter()
            .filter(|m| {
                m.language == Language::Dart
                    && owners.iter().any(|o| o.file_path == m.file_path && m.qualified_name == format!("{}::{}", o.qualified_name, name))
                    && (if call { is_dart_constructor(m, ctx) } else { matches!(m.kind, NodeKind::Constant | NodeKind::EnumMember) })
            })
            .cloned()
            .collect();
        resolved_by = ResolvedBy::QualifiedName;
    } else {
        return None;
    }
    if pool.len() <= 1 {
        return pool.first().map(|n| ResolvedRef::new(r, n.id.clone(), 0.9, resolved_by));
    }
    let best = find_best_match(r, &pool, ctx)?;
    let conf = if compute_path_proximity(&r.file_path, &best.file_path) >= 30 { 0.7 } else { 0.4 };
    Some(ResolvedRef::new(r, best.id.clone(), conf, resolved_by))
}

// -- `Type.member(…)` --------------------------------------------------------------------------

/// `Type.member(…)`: the type's own static member (or constructor), through an import prefix when
/// the call is written with one.
pub fn match_dart_type_member_call(type_name: &str, member: &str, r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    let types: Vec<NodeRef> = ctx.get_nodes_by_name(type_name).iter().filter(|n| is_dart_type_decl(n)).cloned().collect();
    let prefix = if !types.is_empty() || re!(r"^[_$]*[A-Z]").is_match(type_name) { dart_type_call_prefix(type_name, member, r, ctx) } else { None };
    let is_owner = |n: &Node| -> bool { prefix.as_deref().is_none_or(|p| dart_prefix_sees(&r.file_path, p, &n.file_path, type_name, ctx)) };
    if !types.iter().any(|n| is_owner(n)) {
        return if prefix.is_none() { Tri::Undefined } else { Tri::Null };
    }
    match dart_static_member(type_name, member, r, ctx, &is_owner) {
        Some(own) if own.kind != NodeKind::EnumMember => Tri::Value(ResolvedRef::new(r, own.id.clone(), 0.85, ResolvedBy::QualifiedName)),
        _ => Tri::Null,
    }
}

// -- The shape of a Dart reference ------------------------------------------------------------

/// The Dart contribution to [`super::lang_shape::LangShape`]: a receiver-less call, or a type
/// named in a reference / inheritance clause.
pub struct DartShape {
    bare: bool,
    type_ref: bool,
    prefix: Option<String>,
}

impl DartShape {
    pub fn of(r: &UnresolvedRef, ctx: &Ctx) -> Option<DartShape> {
        if r.language != Language::Dart {
            return None;
        }
        let plain = re!(r"^[A-Za-z_$][\w$]*$").is_match(&r.reference_name);
        let bare = is_calls(r) && plain && is_receiver_less_dart_call(r, ctx);
        let type_ref = (is_kind(r, EdgeKind::References) || is_inheritance_ref(r)) && plain;
        let prefix = if type_ref { dart_import_prefix_of(r, ctx) } else { None };
        Some(DartShape { bare, type_ref, prefix })
    }
}

impl RefShape for DartShape {
    fn excludes(&self, n: &NodeRef, r: &UnresolvedRef, ctx: &Ctx) -> bool {
        (self.bare && is_dart_member(n) && !is_dart_method_in_scope(n, r, ctx))
            || is_dart_constructor(n, ctx)
            || (self.bare && is_dart_type_value(n) && !is_dart_method_in_scope(n, r, ctx))
            || ((self.bare || self.type_ref) && !is_dart_top_level_visible(n, r, ctx, self.prefix.as_deref()))
    }

    fn rank_exact(&self, pool: &[NodeRef], r: &UnresolvedRef, ctx: &Ctx) -> Option<Vec<NodeRef>> {
        if self.bare {
            Some(nearest_dart_members(&prefer_own_dart_library(pool, r, ctx), r, ctx))
        } else if self.type_ref && self.prefix.is_none() {
            Some(prefer_own_dart_library(pool, r, ctx))
        } else {
            None
        }
    }

    fn rank_fuzzy(&self, pool: &[NodeRef], r: &UnresolvedRef, ctx: &Ctx) -> Option<Vec<NodeRef>> {
        if self.bare || (self.type_ref && self.prefix.is_none()) {
            Some(prefer_own_dart_library(pool, r, ctx))
        } else {
            None
        }
    }
}
