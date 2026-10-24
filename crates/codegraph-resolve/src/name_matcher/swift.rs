//! Swift scope rules: how a call is written (bare, `self.`, `super.`, on a receiver), which
//! same-named member it can reach through the enclosing types and their supertypes, and which
//! stdlib-named chain a project method must be named after its receiver to match.

use super::lang_shape::RefShape;
use super::members::infer_member_receiver_type;
use super::method::shares_receiver_word;
use super::objc::objc_system_super;
use super::rust_go_shape::rust_go_receiver_name;
use super::std_methods::SWIFT_STD_METHODS;
use super::util::*;
use crate::ctx::Ctx;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::is_js_space;
use codegraph_types::{Language, Node, NodeKind};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SwiftShapeKind {
    Bare,
    SelfRef,
    Super,
    Chained,
}

/// How a Swift call is written.
#[derive(Clone, Debug)]
pub struct SwiftCallShape {
    pub shape: SwiftShapeKind,
    /// The name the call is chained on, call and index arguments dropped (`chained` only).
    pub receiver: String,
    /// The first argument label (`chained` only).
    pub label: String,
    /// Written as an index: `name[…]`.
    pub subscript: bool,
    /// The type constructed right before the dot: `Foo(…).name` → `Foo`.
    pub constructed: Option<String>,
}

pub fn swift_call_shape(r: &UnresolvedRef, ctx: &Ctx) -> Option<SwiftCallShape> {
    let line = line_at(ctx, &r.file_path, r.line).or_else(|| {
        let idx = (r.line as usize).checked_sub(1)?;
        ctx.read_file(&r.file_path).and_then(|s| s.split('\n').nth(idx).map(String::from))
    })?;
    let name = r.reference_name.as_str();
    let at_column = starts_with_at(&line, name, r.column) && !char_before(&line, col_byte(&line, r.column)).is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    let mut start: Option<usize> = if at_column { Some(col_byte(&line, r.column)) } else { None };
    if start.is_none() {
        start = Re::cached(&format!(r"(?<![\w$]){}\s*[({{<\[]", escape_regex(name)), "").find(&line).map(|m| m.start());
    }
    let start = start?;
    let before = &line[..start];
    let after = &line[start + name.len()..];
    let subscript = re!(r"^\s*\[").is_match(after);
    let base = |shape| SwiftCallShape { shape, receiver: String::new(), label: String::new(), subscript, constructed: None };
    if !re!(r"\.\s*$").is_match(before) {
        return Some(base(SwiftShapeKind::Bare));
    }
    // `self.name` / `super.name`, not `other.self.name`.
    if let Some(plain) = re!(r"(?:^|[^\w$.)\]])(self|Self|super)\s*[?!]?\s*\.\s*$").captures(before).and_then(|c| c.text(1).map(String::from)) {
        return Some(base(if plain == "super" { SwiftShapeKind::Super } else { SwiftShapeKind::SelfRef }));
    }
    Some(SwiftCallShape {
        shape: SwiftShapeKind::Chained,
        receiver: rust_go_receiver_name(&re!(r"[?!]\s*\.", "g").replace_all_str(before, ".")),
        label: re!(r"^\s*\(\s*([A-Za-z_]\w*)\s*:(?!:)").captures(after).and_then(|c| c.text(1).map(String::from)).unwrap_or_default(),
        subscript,
        constructed: re!(r"(?<![\w$.])([A-Z][\w$]*)\s*(?:<[^<>()]*>)?\s*\([^()]*\)\s*[?!]?\s*\.\s*$").captures(before).and_then(|c| c.text(1).map(String::from)),
    })
}

const SWIFT_STD_LABELS: &[&str] = &[
    "contentsOf", "where", "by", "at", "keepingCapacity", "separator", "forKey", "of", "into", "in", "options", "maxSplits", "omittingEmptySubsequences", "with",
    "after", "before", "upTo", "through", "offsetBy", "default",
];

/// Does the `func` declared at `n` carry `label` as its first argument label (one a stdlib
/// method would not have)?
fn swift_declares_label(n: &Node, label: &str, ctx: &Ctx) -> bool {
    if label.is_empty() || SWIFT_STD_LABELS.contains(&label) {
        return false;
    }
    let lines = lines_of(ctx, &n.file_path);
    let head = js_slice(&lines, n.start_line as i64 - 1, n.start_line as i64 + 3).join(" ");
    Re::cached(&format!(r"\bfunc\s+{}\s*(?:<[^>]*>)?\s*\(\s*{}\b", escape_regex(&n.name), escape_regex(label)), "").is_match(&head)
}

/// The standard-library protocol hierarchy a project extension can add members to.
fn swift_std_supers(name: &str) -> Option<&'static [&'static str]> {
    Some(match name {
        "RandomAccessCollection" => &["BidirectionalCollection"],
        "BidirectionalCollection" => &["Collection"],
        "MutableCollection" => &["Collection"],
        "RangeReplaceableCollection" => &["Collection"],
        "Collection" => &["Sequence"],
        "LazySequenceProtocol" => &["Sequence"],
        "LazyCollectionProtocol" => &["Collection", "LazySequenceProtocol"],
        "StringProtocol" => &["BidirectionalCollection"],
        "Array" | "ArraySlice" | "ContiguousArray" => &["RandomAccessCollection", "MutableCollection", "RangeReplaceableCollection"],
        "String" | "Substring" => &["StringProtocol", "RangeReplaceableCollection"],
        "Dictionary" => &["Collection"],
        "Set" => &["Collection", "SetAlgebra"],
        "Range" | "ClosedRange" => &["RandomAccessCollection"],
        _ => return None,
    })
}

fn is_swift_type_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Class | NodeKind::Struct | NodeKind::Enum | NodeKind::Interface)
}

fn is_swift_member_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Method | NodeKind::Property | NodeKind::Field | NodeKind::EnumMember)
}

/// What the repo (and the platform tables) say about a Swift type name.
pub struct SwiftDecl {
    pub supers: Vec<String>,
    /// Declared (not only extended) by a project type.
    pub project_type: bool,
}

memo_map!(SwiftDeclMemo, String, Rc<SwiftDecl>);
memo_map!(SwiftHierarchyMemo, String, Rc<Vec<(String, u32)>>);

pub fn swift_decl_of(type_name: &str, ctx: &Ctx) -> Rc<SwiftDecl> {
    memoized::<SwiftDeclMemo>(ctx, type_name.to_string(), || {
        let mut supers: Vec<String> = match (swift_std_supers(type_name), objc_system_super(type_name)) {
            (Some(std), _) => std.iter().map(|s| s.to_string()).collect(),
            (None, Some(system)) => vec![system.to_string()],
            (None, None) => Vec::new(),
        };
        let mut project_type = false;
        for decl in ctx.get_nodes_by_name(type_name).iter() {
            if decl.language != Language::Swift || !is_swift_type_kind(decl.kind) {
                continue;
            }
            let (head_supers, extension) = swift_head_of(decl, ctx);
            supers.extend(head_supers);
            if !extension {
                project_type = true;
            }
        }
        Rc::new(SwiftDecl { supers, project_type })
    })
}

/// The supertypes of a Swift class / struct / enum / extension declaration, and whether it is an
/// `extension`.
fn swift_head_of(decl: &Node, ctx: &Ctx) -> (Vec<String>, bool) {
    let lines = lines_of(ctx, &decl.file_path);
    let text = js_slice(&lines, decl.start_line as i64 - 1, (decl.end_line as i64).min(decl.start_line as i64 + 20)).join("\n");
    let text = re!(r"/\*[\s\S]*?\*/", "g").replace_all_str(&text, " ");
    let text = re!(r"//[^\n]*", "g").replace_all_str(&text, " ");
    let mut depth = 0i32;
    let mut flat = String::new();
    for ch in text.chars() {
        if ch == '<' || ch == '(' {
            depth += 1;
        } else if ch == '>' || ch == ')' {
            depth = (depth - 1).max(0);
        } else if depth == 0 {
            if ch == '{' {
                break;
            }
            flat.push(ch);
        }
    }
    let Some(head) = re!(r"\b(class|struct|enum|protocol|extension|actor)\s+[\w.]+\s*([\s\S]*)$").captures(&flat) else { return (Vec::new(), false) };
    let tail = head.text(2).unwrap_or("").trim_matches(is_js_space);
    let clause = re!(r"^:([\s\S]*?)(?:\bwhere\b|$)").captures(tail).and_then(|c| c.text(1).map(String::from)).unwrap_or_default();
    let supers = clause
        .split(',')
        .filter_map(|item| re!(r"([A-Za-z_]\w*)\s*$").captures(item.trim_matches(is_js_space)).and_then(|c| c.text(1).map(String::from)))
        .collect();
    (supers, head.text(1) == Some("extension"))
}

/// The declared supertypes of a Swift type name (`swiftDeclOf(name).supers`).
pub fn swift_decl_supers(name: &str, ctx: &Ctx) -> Vec<String> {
    swift_decl_of(name, ctx).supers.clone()
}

/// The types whose members a bare call at the reference can reach, nearest first (the enclosing
/// types at depth 0, their supertypes beyond).
pub fn swift_hierarchy_at(r: &UnresolvedRef, ctx: &Ctx) -> Rc<Vec<(String, u32)>> {
    memoized::<SwiftHierarchyMemo>(ctx, format!("{}:{}", r.file_path, r.line), || {
        let mut queue: VecDeque<(String, u32)> = ctx
            .get_nodes_in_file(&r.file_path)
            .iter()
            .filter(|n| is_swift_type_kind(n.kind) && n.start_line <= r.line && n.end_line >= r.line)
            .map(|n| (n.name.rsplit('.').next().unwrap_or("").to_string(), 0))
            .collect();
        let mut depths: Vec<(String, u32)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        while let Some((name, depth)) = queue.pop_front() {
            if depths.len() >= 40 {
                break;
            }
            if !seen.insert(name.clone()) {
                continue;
            }
            for sup in swift_decl_of(&name, ctx).supers.iter() {
                queue.push_back((sup.clone(), depth + 1));
            }
            depths.push((name, depth));
        }
        Rc::new(depths)
    })
}

/// A type and everything it derives from.
fn swift_type_closure(type_name: &str, ctx: &Ctx) -> HashSet<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::from([type_name.to_string()]);
    while let Some(name) = queue.pop_front() {
        if seen.len() >= 40 {
            break;
        }
        if !seen.insert(name.clone()) {
            continue;
        }
        queue.extend(swift_decl_of(&name, ctx).supers.iter().cloned());
    }
    seen
}

/// Of several same-named members, the ones on the nearest enclosing type or supertype.
pub fn nearest_swift_members(candidates: &[NodeRef], r: &UnresolvedRef, ctx: &Ctx) -> Vec<NodeRef> {
    let hierarchy: HashMap<String, u32> = swift_hierarchy_at(r, ctx).iter().cloned().collect();
    let depth_of = |n: &Node| -> Option<u32> {
        if !is_swift_member_kind(n.kind) && !matches!(n.kind, NodeKind::Constant | NodeKind::Variable) {
            return None;
        }
        let cut = n.qualified_name.rfind("::").filter(|c| *c > 0)?;
        hierarchy.get(owner_segment(&n.qualified_name[..cut])).copied()
    };
    let depths: Vec<u32> = candidates.iter().filter_map(|n| depth_of(n)).collect();
    if depths.len() < 2 {
        return candidates.to_vec();
    }
    let nearest = depths.into_iter().min().unwrap_or(0);
    candidates.iter().filter(|n| depth_of(n).map_or(true, |d| d == nearest)).cloned().collect()
}

/// The last `::` / `.` separated segment of an owner path.
fn owner_segment(path: &str) -> &str {
    let by_colons = path.rfind("::").map(|i| i + 2);
    let by_dot = path.rfind('.').map(|i| i + 1);
    &path[by_colons.into_iter().chain(by_dot).max().unwrap_or(0)..]
}

/// A declaration inside a `*Tests` target or a `.playground` is visible only from inside it.
fn is_swift_target_visible(decl_file: &str, from_file: &str) -> bool {
    let segments: Vec<&str> = decl_file.split('/').collect();
    let segments = &segments[..segments.len().saturating_sub(1)];
    let Some(target) = segments.iter().position(|seg| re!(r"Tests$|\.playground$").is_match(seg)) else { return true };
    let prefix = format!("{}/", segments[..=target].join("/"));
    from_file.starts_with(&prefix)
}

/// Whether a same-named candidate is one a call of this shape can reach.
pub fn is_swift_call_target(n: &Node, shape: &SwiftCallShape, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    // A file-scope `let` / `var` of another file is reached by name only through a subscript.
    if matches!(n.kind, NodeKind::Constant | NodeKind::Variable) && n.file_path != r.file_path && !shape.subscript {
        return false;
    }
    if !is_swift_target_visible(&n.file_path, &r.file_path) {
        return false;
    }
    let owner: Option<&str> = n.qualified_name.rfind("::").filter(|c| *c > 0).map(|c| owner_segment(&n.qualified_name[..c]));
    if !is_swift_member_kind(n.kind) && !(owner.is_some() && matches!(n.kind, NodeKind::Constant | NodeKind::Variable)) {
        if n.kind != NodeKind::Function {
            return true;
        }
        return shape.shape == SwiftShapeKind::Bare || owner.is_some() || (shape.shape == SwiftShapeKind::Chained && re!(r"^[A-Z]").is_match(&shape.receiver));
    }
    let Some(owner) = owner else { return true };
    if shape.shape == SwiftShapeKind::Chained {
        // `Foo(…).name`: the constructed type's own members and its supertypes'.
        if let Some(constructed) = &shape.constructed {
            if !ctx.get_nodes_by_name(constructed).iter().any(|f| f.kind == NodeKind::Function) {
                return swift_type_closure(constructed, ctx).contains(owner);
            }
        }
        let typed = if re!(r"^(?:self\.)?[A-Za-z_]\w*$").is_match(&shape.receiver) { infer_member_receiver_type(&shape.receiver, r, ctx) } else { None };
        if let Some(typed) = typed.filter(|t| re!(r"^[A-Z]").is_match(t)) {
            return swift_type_closure(&typed, ctx).contains(owner);
        }
        if shape.receiver.is_empty() || !SWIFT_STD_METHODS.contains(n.name.as_str()) {
            return true;
        }
        return shares_receiver_word(shape.receiver.rsplit('.').next().unwrap_or(""), &n.qualified_name)
            || !swift_decl_of(owner, ctx).project_type
            || swift_declares_label(n, &shape.label, ctx);
    }
    let hierarchy = swift_hierarchy_at(r, ctx);
    match hierarchy.iter().find(|(name, _)| name == owner) {
        Some((_, depth)) => shape.shape != SwiftShapeKind::Super || *depth > 0,
        None => false,
    }
}

/// The Swift contribution to [`super::lang_shape::LangShape`].
pub struct SwiftShape(SwiftCallShape);

impl SwiftShape {
    pub fn of(r: &UnresolvedRef, ctx: &Ctx) -> Option<SwiftShape> {
        if r.language != Language::Swift || !is_calls(r) || !re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name) {
            return None;
        }
        swift_call_shape(r, ctx).map(SwiftShape)
    }
}

impl RefShape for SwiftShape {
    fn excludes(&self, n: &NodeRef, r: &UnresolvedRef, ctx: &Ctx) -> bool {
        !is_swift_call_target(n, &self.0, r, ctx)
    }

    fn rank_exact(&self, pool: &[NodeRef], r: &UnresolvedRef, ctx: &Ctx) -> Option<Vec<NodeRef>> {
        (self.0.shape != SwiftShapeKind::Chained).then(|| nearest_swift_members(pool, r, ctx))
    }
}
