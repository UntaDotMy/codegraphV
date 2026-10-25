//! Written C++ types — port of the type machinery of `name-matcher.ts`: arrays and subscripts,
//! declared return types, aliases, data members, iterators and library containers, and the type
//! of an expression.

use super::cpp_decl::{
    build_declarator_regex, build_listed_declarator_regex, cpp_code_of, cpp_range_expression, esc, is_cpp_type_line, match_cpp_declarator,
    match_cpp_listed_declarator, normalize_cpp_type_name,
};
use super::cpp_scope::{cpp_class_decls, cpp_implicit_this_method, cpp_member_owner, cpp_scope_declarations, Receiver};
use super::cpp_text::split_cpp_top_level;
use super::cpp_method::{cpp_method_of, is_cpp_dependent_base, cpp_using_declares};
use super::cpp_recv::{infer_cpp_receiver_type, CppReceiverDecl};
use super::util::{after_col, before_col, col_byte, memo_map, MemoMap};
use crate::cpp_raw_strings::{cpp_literals_blanked, cpp_non_code_lines, NonCodeLines};
use crate::cpp_supertypes::{cpp_class_written, CppNameSite, CppWrittenClass};
use crate::cpp_type_aliases::{
    cpp_aliased_type_written, cpp_parent_scope, cpp_scopes_within, cpp_template_arguments, cpp_template_parameters, cpp_type_segments,
    is_cpp_pointer_type, strip_cpp_template_arguments,
};
use crate::ctx::Ctx;
use crate::types::{NodeRef, Tri, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_types::jscompat::js_trim;
use codegraph_types::{Language, Node, NodeKind};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::LazyLock;

/// `CppWrittenType`: a type as written, with where it was written.
#[derive(Debug, Clone)]
pub struct CppWrittenType {
    pub text: String,
    pub scope: String,
    pub file: String,
    pub line: usize,
    pub parameters: Rc<HashSet<String>>,
    /// Array dimensions the declarator adds.
    pub array: Option<u32>,
    /// The fixture class a gtest body looks names up in (carried from a call site).
    pub class_scope: Option<String>,
}

impl CppWrittenType {
    pub fn site(&self) -> CppNameSite {
        CppNameSite {
            scope: self.scope.clone(),
            file: self.file.clone(),
            line: self.line,
            parameters: Rc::clone(&self.parameters),
            self_id: None,
            class_scope: self.class_scope.clone(),
        }
    }

    /// `{ ...type, text }`.
    pub fn with_text(&self, text: impl Into<String>) -> CppWrittenType {
        CppWrittenType { text: text.into(), ..self.clone() }
    }
}

/// `{ ...site, text }`.
pub fn written_at_site(site: &CppNameSite, text: impl Into<String>) -> CppWrittenType {
    CppWrittenType {
        text: text.into(),
        scope: site.scope.clone(),
        file: site.file.clone(),
        line: site.line,
        parameters: Rc::clone(&site.parameters),
        array: None,
        class_scope: site.class_scope.clone(),
    }
}

pub fn is_cpp_class_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Class | NodeKind::Struct | NodeKind::Union)
}

pub fn is_cpp_function_node(n: &Node) -> bool {
    matches!(n.kind, NodeKind::Method | NodeKind::Function) && matches!(n.language, Language::Cpp | Language::C)
}

pub fn is_c_family_node(n: &Node) -> bool {
    matches!(n.language, Language::Cpp | Language::C)
}

fn set_of(words: &[&'static str]) -> HashSet<&'static str> {
    words.iter().copied().collect()
}

static CPP_LIBRARY_SEQUENCES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    set_of(&[
        "vector", "list", "forward_list", "deque", "set", "multiset", "unordered_set", "unordered_multiset", "array", "span",
        "initializer_list", "Span", "InlinedVector", "FixedArray", "flat_hash_set", "node_hash_set", "btree_set", "btree_multiset",
    ])
});
static CPP_PROJECT_SEQUENCES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| set_of(&["RepeatedField", "RepeatedPtrField", "autovector"]));
static CPP_LIBRARY_MAPS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    set_of(&["map", "multimap", "unordered_map", "unordered_multimap", "flat_hash_map", "node_hash_map", "btree_map", "btree_multimap"])
});
static CPP_LIBRARY_ADAPTORS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| set_of(&["stack", "queue", "priority_queue"]));
static CPP_HOLDERS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    set_of(&["std::unique_ptr", "std::shared_ptr", "std::optional", "absl::optional", "absl::StatusOr", "absl::NoDestructor"])
});
pub static CPP_TYPE_KINDS: [NodeKind; 6] =
    [NodeKind::Class, NodeKind::Struct, NodeKind::Union, NodeKind::Enum, NodeKind::Interface, NodeKind::TypeAlias];

fn iterator_member_kind(member: &str) -> Option<&'static str> {
    Some(match member {
        "begin" | "end" | "find" | "lower_bound" | "upper_bound" => "iterator",
        "cbegin" | "cend" => "const_iterator",
        "rbegin" | "rend" => "reverse_iterator",
        "crbegin" | "crend" => "const_reverse_iterator",
        _ => return None,
    })
}

/// `cppLastSegment`.
pub fn cpp_last_segment(name: &str) -> &str {
    name.split("::").filter(|p| !p.is_empty()).last().unwrap_or(name)
}

/// `cppWithoutNullability`.
pub fn cpp_without_nullability(text: &str) -> String {
    let stripped = re!(r"\b(?:absl_nonnull|absl_nullable|absl_nullability_unknown|_Nonnull|_Nullable|_Null_unspecified)\b", "g")
        .replace_all_str(text, " ");
    js_trim(&re!(r"\s+").replace_all_str(&stripped, " ")).to_string()
}

fn trim_ref_tail(text: &str) -> String {
    js_trim(&re!(r"&+\s*$").replace_all_str(text, "")).to_string()
}

// -- Bracket scanners --------------------------------------------------------------------------

/// `cppOpeningParen`: the `(` matching the `)` at `close`.
pub fn cpp_opening_paren(text: &str, close: usize) -> Option<usize> {
    let b = text.as_bytes();
    let mut depth = 0i64;
    let mut i = close as isize;
    while i >= 0 {
        let c = b[i as usize];
        if c == b')' {
            depth += 1;
        } else if c == b'(' {
            depth -= 1;
            if depth == 0 {
                return Some(i as usize);
            }
        }
        i -= 1;
    }
    None
}

/// `cppOpeningBracket`.
pub fn cpp_opening_bracket(text: &str, close: usize) -> Option<usize> {
    let b = text.as_bytes();
    let mut depth = 0i64;
    let mut i = close as isize;
    while i >= 0 {
        let c = b[i as usize];
        if c == b']' {
            depth += 1;
        } else if c == b'[' {
            depth -= 1;
            if depth == 0 {
                return Some(i as usize);
            }
        }
        i -= 1;
    }
    None
}

/// `cppClosingBracket`.
pub fn cpp_closing_bracket(text: &str, open: usize) -> Option<usize> {
    let b = text.as_bytes();
    let mut depth = 0i64;
    for (i, &c) in b.iter().enumerate().skip(open) {
        if c == b'[' {
            depth += 1;
        } else if c == b']' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

// -- Arrays and subscripts ----------------------------------------------------------------------

/// `cppArrayDimensions`.
pub fn cpp_array_dimensions(rest: &str) -> u32 {
    let mut rest = rest;
    let mut dimensions = 0;
    while let Some(open) = re!(r"^\s*\[(?!\[)").find(rest) {
        dimensions += 1;
        let Some(close) = cpp_closing_bracket(rest, open.end() - 1) else { break };
        rest = &rest[close + 1..];
    }
    dimensions
}

/// `cppArrayElement`.
pub fn cpp_array_element(ty: &CppWrittenType) -> CppWrittenType {
    let mut element = ty.clone();
    element.array = match ty.array {
        Some(n) if n > 1 => Some(n - 1),
        _ => None,
    };
    element
}

/// `cppSubscriptElement`.
pub fn cpp_subscript_element(ty: &CppWrittenType, at: &UnresolvedRef, ctx: &Ctx) -> Option<CppWrittenType> {
    if ty.array.is_some_and(|n| n > 0) {
        return Some(cpp_array_element(ty));
    }
    if is_cpp_pointer_type(&ty.text) {
        return cpp_pointee(ty);
    }
    let segments = cpp_type_segments(&ty.text)?;
    let last = segments.last()?.clone();
    let library = segments[0] == "std" || segments[0] == "absl";
    if library && CPP_LIBRARY_MAPS.contains(last.as_str()) {
        let value = cpp_template_arguments(&ty.text).and_then(|a| a.get(1).cloned()).filter(|v| !v.is_empty())?;
        return Some(ty.with_text(cpp_without_nullability(&value)));
    }
    if (library && CPP_LIBRARY_SEQUENCES.contains(last.as_str())) || CPP_PROJECT_SEQUENCES.contains(last.as_str()) {
        return cpp_range_element_of(ty, at, ctx, 0);
    }
    if library {
        return None;
    }
    let cls = cpp_written_class(ty, at, ctx)?;
    let element = cpp_return_type_of(&cpp_methods_of(&cls, "operator[]", at, ctx), ctx)?;
    let text = trim_ref_tail(&element.text);
    Some(element.with_text(text))
}

// -- Declared return types ----------------------------------------------------------------------

/// `cppMacroMethodHead`: the signature of a method a macro declares, when it names the class.
pub fn cpp_macro_method_head(f: &Node) -> Option<String> {
    if f.kind != NodeKind::Method {
        return None;
    }
    let signature = f.signature.as_ref()?;
    if signature.is_empty() {
        return None;
    }
    let pattern = format!(r"::\s*{}\s*\(", esc(&f.name));
    if Re::cached(&pattern, "").is_match(signature) {
        Some(signature.clone())
    } else {
        None
    }
}

/// `cppWrittenReturnType`: the return type written at the head of a function's declaration.
pub fn cpp_written_return_type(f: &Node, ctx: &Ctx) -> Option<String> {
    let macro_head = cpp_macro_method_head(f);
    let head = match macro_head {
        Some(h) => h,
        None => {
            let lines = ctx.get_file_lines(&f.file_path)?;
            let from = (f.start_line as usize).saturating_sub(1).min(lines.len());
            let to = (f.start_line as usize + 2).min(lines.len());
            let joined = lines[from..to].join(" ");
            after_col(&joined, f.start_column).to_string()
        }
    };
    let escaped = esc(&f.name);
    let name = match escaped.strip_prefix("operator") {
        Some(rest) => format!(r"operator\s*{rest}"),
        None => escaped,
    };
    let pattern = format!(r"^(.*?)(?:\b[A-Za-z_]\w*\s*(?:<[^<>;{{}}]*>)?\s*::\s*)*\b{name}\s*\(");
    let declared = Re::cached(&pattern, "").captures(&head)?;
    let mut written = declared.text(1)?.to_string();
    if re!(r"^\s*template\b").is_match(&written) {
        let open = written.find('<');
        let mut depth = 0i64;
        let mut end: Option<usize> = None;
        if let Some(open) = open {
            for (i, c) in written.bytes().enumerate().skip(open) {
                if c == b'<' {
                    depth += 1;
                } else if c == b'>' {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i);
                        break;
                    }
                }
            }
        }
        let end = end?;
        written = written[end + 1..].to_string();
    }
    let cleaned = re!(r"\[\[[^\]]*\]\]", "g").replace_all_str(&written, " ");
    let cleaned = re!(r"\b(?:inline|static|virtual|constexpr|consteval|explicit|friend|extern)\b", "g").replace_all_str(&cleaned, " ");
    let cleaned = re!(r"^\s*(?:[A-Z][A-Z0-9]*_[A-Z0-9_]*(?:\([^()]*\))?\s+)+").replace_all_str(&cleaned, "");
    let cleaned = re!(r"(?:\s+[A-Z][A-Z0-9]*_[A-Z0-9_]*(?:\([^()]*\))?)+\s*$").replace_all_str(&cleaned, "");
    let written = cpp_without_nullability(&cleaned);
    if written.is_empty() || re!(r"^(?:void|auto)$").is_match(&written) || re!(r"\bdecltype\b").is_match(&written) {
        return None;
    }
    Some(written)
}

/// `cppOwnTemplateParameters`: the parameter names of the `template <…>` header of a function.
pub fn cpp_own_template_parameters(f: &Node, ctx: &Ctx) -> Vec<String> {
    let Some(lines) = ctx.get_file_lines(&f.file_path) else { return Vec::new() };
    let start = f.start_line as usize;
    let at = lines.get(start.wrapping_sub(1)).map(String::as_str).unwrap_or("");
    let mut parts: Vec<String> = Vec::new();
    let from = start.saturating_sub(9);
    let to = start.saturating_sub(1).min(lines.len());
    if from < to {
        parts.extend(lines[from..to].iter().map(|l| cpp_code_of(l).unwrap_or_default()));
    }
    parts.push(cpp_code_of(before_col(at, f.start_column)).unwrap_or_default());
    let before = parts.join(" ");
    let before = before.trim_end_matches(codegraph_types::jscompat::is_js_space);
    if !before.ends_with('>') {
        return Vec::new();
    }
    let b = before.as_bytes();
    let mut depth = 0i64;
    let mut i = b.len() as isize - 1;
    while i >= 0 {
        let c = b[i as usize];
        if c == b'>' {
            depth += 1;
        } else if c == b'<' {
            depth -= 1;
            if depth == 0 {
                if !re!(r"\btemplate\s*$").is_match(&before[..i as usize]) {
                    return Vec::new();
                }
                let inner = &before[i as usize + 1..before.len() - 1];
                return split_cpp_top_level(inner)
                    .into_iter()
                    .map(|p| {
                        let head = p.split('=').next().unwrap_or("");
                        re!(r"([A-Za-z_]\w*)$").captures(js_trim(head)).and_then(|c| c.text(1).map(String::from)).unwrap_or_default()
                    })
                    .collect();
            }
        }
        i -= 1;
    }
    Vec::new()
}

/// `cppMemberTemplateReturn`: the return type of a member template instantiated with `args`.
pub fn cpp_member_template_return(methods: &[NodeRef], args: &str, site: &CppWrittenType, ctx: &Ctx) -> Option<CppWrittenType> {
    struct Slot {
        index: usize,
        written: String,
        param: String,
    }
    let mut slot: Option<Slot> = None;
    for f in methods {
        let Some(written) = cpp_written_return_type(f, ctx) else { continue };
        let stripped = re!(r"\b(?:const|volatile)\b|[*&]", "g").replace_all_str(&written, " ");
        let param = js_trim(&stripped).to_string();
        let index = if param.is_empty() { None } else { cpp_own_template_parameters(f, ctx).iter().position(|p| *p == param) };
        match index {
            None => return None,
            Some(index) => {
                if slot.as_ref().is_some_and(|s| s.index != index) {
                    return None;
                }
                if slot.is_none() {
                    slot = Some(Slot { index, written, param });
                }
            }
        }
    }
    let slot = slot?;
    let inner = args.get(1..args.len().saturating_sub(1)).unwrap_or("");
    let arg = split_cpp_top_level(inner).into_iter().nth(slot.index).filter(|a| !a.is_empty())?;
    let replaced = Re::cached(&format!(r"\b{}\b", slot.param), "").replace_first(&slot.written, |_| arg.clone());
    Some(site.with_text(replaced))
}

/// `cppReturnTypeOf`: the written return type of the first of `fns` that has one.
pub fn cpp_return_type_of(fns: &[NodeRef], ctx: &Ctx) -> Option<CppWrittenType> {
    for f in fns {
        if let Some(text) = cpp_written_return_type(f, ctx).filter(|t| !t.is_empty()) {
            return Some(CppWrittenType {
                text,
                scope: cpp_parent_scope(&f.qualified_name),
                file: f.file_path.clone(),
                line: f.start_line as usize,
                parameters: cpp_template_parameters(f, ctx),
                array: None,
                class_scope: None,
            });
        }
    }
    None
}

/// `cppMethodsOf`: the methods `name` of `cls`, own first, otherwise inherited.
pub fn cpp_methods_of(cls: &Node, name: &str, at: &UnresolvedRef, ctx: &Ctx) -> Vec<NodeRef> {
    let own: Vec<NodeRef> =
        ctx.get_nodes_by_qualified_name(&format!("{}::{}", cls.qualified_name, name)).iter().filter(|n| is_cpp_function_node(n)).cloned().collect();
    if !own.is_empty() {
        return own;
    }
    cpp_method_of(cls, name, at, ctx).into_iter().collect()
}

/// `cppFollowWrittenAliases`.
pub fn cpp_follow_written_aliases(ty: &CppWrittenType, at: &UnresolvedRef, ctx: &Ctx) -> Option<CppWrittenType> {
    let mut current = ty.clone();
    for _ in 0..4 {
        match cpp_aliased_type_written(&current.text, &cpp_scopes_within(&current.scope), at, ctx) {
            Tri::Undefined => return Some(current),
            Tri::Null => return None,
            Tri::Value((text, alias)) => {
                current = CppWrittenType {
                    text: if is_cpp_pointer_type(&current.text) { format!("{text}*") } else { text },
                    scope: cpp_parent_scope(&alias.qualified_name),
                    file: alias.file_path.clone(),
                    line: alias.start_line as usize,
                    parameters: cpp_template_parameters(&alias, ctx),
                    array: current.array,
                    class_scope: None,
                };
            }
        }
    }
    None
}

/// `isCppTemplateParameterType`.
pub fn is_cpp_template_parameter_type(ty: &CppWrittenType) -> bool {
    cpp_type_segments(&ty.text).and_then(|s| s.into_iter().next()).is_some_and(|head| ty.parameters.contains(&head))
}

/// `cppClassWritten(type.text, type, at, context)`.
pub fn cpp_class_of_type(ty: &CppWrittenType, at: &UnresolvedRef, ctx: &Ctx) -> Tri<CppWrittenClass> {
    cpp_class_written(&ty.text, &ty.site(), at, ctx)
}

/// `cppWrittenClass`.
pub fn cpp_written_class(ty: &CppWrittenType, at: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    match cpp_class_of_type(ty, at, ctx) {
        Tri::Value(found) if is_cpp_class_kind(found.node.kind) => Some(found.node),
        _ => None,
    }
}

// -- Data members -------------------------------------------------------------------------------

memo_map!(CppDataMembers, String, Option<CppWrittenType>);
memo_map!(CppNestedSpans, String, Rc<Vec<(u32, u32)>>);
memo_map!(CppClassParenDepths, String, Rc<Vec<i64>>);
memo_map!(CppCallersMembers, String, Option<CppWrittenType>);

/// `cppDataMemberIn`: the type a class declares for the data member `name`.
pub fn cpp_data_member_in(decl: &Node, name: &str, ctx: &Ctx) -> Option<CppWrittenType> {
    if !is_c_family_node(decl) || !is_cpp_class_kind(decl.kind) {
        return None;
    }
    let memo = ctx.memo::<CppDataMembers>();
    let key = format!("{}\u{0}{}", decl.id, name);
    if let Some(hit) = memo.lookup(&key) {
        return hit;
    }
    let mut member: Option<CppWrittenType> = None;
    if let Some(lines) = ctx.get_file_lines(&decl.file_path) {
        let escaped = esc(name);
        let word = Re::cached(&format!(r"\b{escaped}\b"), "");
        let declarator = build_declarator_regex(&escaped);
        let listed = build_listed_declarator_regex(&escaped);
        let declared = |text: &str| match_cpp_declarator(text, &declarator).or_else(|| match_cpp_listed_declarator(text, name, &listed));
        let end = (decl.end_line.max(decl.start_line) as usize).min(lines.len());
        let nested = cpp_nested_spans(decl, end as u32, ctx);
        let raw_lines = cpp_non_code_lines(&decl.file_path, ctx);
        let depths = cpp_class_paren_depths(decl, &lines, end, &raw_lines, ctx);
        let mut l = decl.start_line as usize;
        while l <= end {
            let line_text = &lines[l - 1];
            if !word.is_match(line_text) || nested.iter().any(|&(start, stop)| l as u32 >= start && l as u32 <= stop) {
                l += 1;
                continue;
            }
            let mut code = cpp_code_of(&cpp_literals_blanked(line_text, l, &raw_lines));
            let mut from = l;
            let mut m = code.as_deref().and_then(&declared);
            if m.is_none() && code.as_deref().is_some_and(|c| !c.is_empty()) && l > decl.start_line as usize {
                let above = cpp_code_of(&cpp_literals_blanked(&lines[l - 2], l - 1, &raw_lines));
                if let Some(above) = above.filter(|a| !a.is_empty() && is_cpp_type_line(a)) {
                    let joined = format!("{} {}", above, code.as_deref().unwrap_or(""));
                    from = l - 1;
                    m = declared(&joined);
                    code = Some(joined);
                }
            }
            let Some(m) = m else {
                l += 1;
                continue;
            };
            let code_text = code.as_deref().unwrap_or("");
            let rest = m.rest(code_text);
            if rest.starts_with('(') {
                l += 1;
                continue;
            }
            let mut depth = depths.get(from - decl.start_line as usize).copied().unwrap_or(0);
            let lead = if from == decl.start_line as usize { col_byte(code_text, decl.start_column) } else { 0 };
            for c in super::util::slice_bytes(code_text, lead, m.index).chars() {
                if c == '(' {
                    depth += 1;
                } else if c == ')' {
                    depth -= 1;
                }
            }
            let ty = if depth <= 0 { normalize_cpp_type_name(&m.ty) } else { None };
            if let Some(ty) = ty.filter(|t| t != "auto") {
                let _ = ty;
                let array = cpp_array_dimensions(rest);
                member = Some(CppWrittenType {
                    text: m.ty.clone(),
                    scope: decl.qualified_name.clone(),
                    file: decl.file_path.clone(),
                    line: from,
                    parameters: cpp_template_parameters(decl, ctx),
                    array: if array > 0 { Some(array) } else { None },
                    class_scope: None,
                });
                break;
            }
            l += 1;
        }
    }
    memo.store(key, member.clone());
    member
}

/// `cppClassParenDepths`: the parenthesis depth at the start of each line of a class body.
fn cpp_class_paren_depths(decl: &Node, lines: &[String], end: usize, raw_lines: &NonCodeLines, ctx: &Ctx) -> Rc<Vec<i64>> {
    let memo = ctx.memo::<CppClassParenDepths>();
    if let Some(hit) = memo.lookup(&decl.id) {
        return hit;
    }
    let mut depths = Vec::new();
    let mut depth = 0i64;
    let mut l = decl.start_line as usize;
    while l <= end {
        depths.push(depth);
        let text = cpp_literals_blanked(lines.get(l - 1).map(String::as_str).unwrap_or(""), l, raw_lines);
        let b = text.as_bytes();
        let mut i = if l == decl.start_line as usize { col_byte(&text, decl.start_column) } else { 0 };
        while i < b.len() {
            let c = b[i];
            if c == b'/' && b.get(i + 1) == Some(&b'/') {
                break;
            } else if c == b'(' {
                depth += 1;
            } else if c == b')' && depth > 0 {
                depth -= 1;
            }
            i += 1;
        }
        l += 1;
    }
    let depths = Rc::new(depths);
    memo.store(decl.id.clone(), Rc::clone(&depths));
    depths
}

/// `isCppAnonymousMembers`.
fn is_cpp_anonymous_members(n: &Node, ctx: &Ctx) -> bool {
    if n.name != "<anonymous>" || !matches!(n.kind, NodeKind::Union | NodeKind::Struct) {
        return false;
    }
    let Some(lines) = ctx.get_file_lines(&n.file_path) else { return false };
    let head = lines.get((n.start_line as usize).wrapping_sub(1)).map(|l| after_col(l, n.start_column));
    let foot = lines.get((n.end_line.max(n.start_line) as usize).wrapping_sub(1));
    match (head, foot) {
        (Some(h), Some(f)) if !h.is_empty() && !f.is_empty() => {
            re!(r"^\s*(?:union|struct)\s*\{").is_match(h) && re!(r"\}\s*;\s*(?://.*)?$").is_match(f)
        }
        _ => false,
    }
}

/// `cppNestedSpans`: the line spans of the functions and classes nested in `decl`.
fn cpp_nested_spans(decl: &Node, end: u32, ctx: &Ctx) -> Rc<Vec<(u32, u32)>> {
    let memo = ctx.memo::<CppNestedSpans>();
    if let Some(hit) = memo.lookup(&decl.id) {
        return hit;
    }
    let spans: Vec<(u32, u32)> = ctx
        .get_nodes_in_file(&decl.file_path)
        .iter()
        .filter(|n| {
            n.id != decl.id
                && n.start_line >= decl.start_line
                && n.end_line.max(n.start_line) <= end
                && (is_cpp_function_node(n) || (is_cpp_class_kind(n.kind) && !is_cpp_anonymous_members(n, ctx)))
        })
        .map(|n| (n.start_line, n.end_line.max(n.start_line)))
        .collect();
    let spans = Rc::new(spans);
    memo.store(decl.id.clone(), Rc::clone(&spans));
    spans
}

/// `cppCallersMember`: the type of the data member `name` of the class the caller belongs to.
pub fn cpp_callers_member(name: &str, caller: &Node, r: &UnresolvedRef, ctx: &Ctx) -> Option<CppWrittenType> {
    let memo = ctx.memo::<CppCallersMembers>();
    let key = format!("{}\u{0}{}", caller.id, name);
    if let Some(hit) = memo.lookup(&key) {
        return hit;
    }
    let owner = cpp_member_owner(caller, ctx).or_else(|| cpp_test_fixture(caller, r, ctx));
    let member = owner.and_then(|owner| {
        let decls = cpp_scope_declarations(&owner, cpp_class_decls(&owner, ctx), caller, ctx).decls;
        let mut seen = HashSet::new();
        seen.insert(owner);
        cpp_inherited_data_member(&decls, name, ctx, 0, &mut seen)
    });
    memo.store(key, member.clone());
    member
}

/// `cppInheritedDataMember`.
pub fn cpp_inherited_data_member(decls: &[NodeRef], name: &str, ctx: &Ctx, depth: u32, seen: &mut HashSet<String>) -> Option<CppWrittenType> {
    for decl in decls {
        if let Some(own) = cpp_data_member_in(decl, name, ctx) {
            return Some(own);
        }
    }
    if depth >= 4 {
        return None;
    }
    for decl in decls {
        for base in ctx.get_supertype_nodes(&decl.id).iter() {
            if !is_cpp_class_kind(base.kind) || seen.contains(&base.qualified_name) {
                continue;
            }
            if is_cpp_dependent_base(decl, base, ctx) && !cpp_using_declares(decl, base, name, ctx) {
                continue;
            }
            seen.insert(base.qualified_name.clone());
            if let Some(inherited) = cpp_inherited_data_member(std::slice::from_ref(base), name, ctx, depth + 1, seen) {
                return Some(inherited);
            }
        }
    }
    None
}

/// `cppTestFixture`: the fixture class a `TEST_F`/`TEST_P` body belongs to.
pub fn cpp_test_fixture(caller: &Node, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    if caller.kind != NodeKind::Function || (caller.name != "TEST_F" && caller.name != "TEST_P") {
        return None;
    }
    let lines = ctx.get_file_lines(&caller.file_path);
    let from = (caller.start_line as usize).saturating_sub(1);
    let head = lines
        .as_ref()
        .map(|l| {
            let to = (caller.start_line as usize + 1).min(l.len());
            if from < to { l[from..to].join(" ") } else { String::new() }
        })
        .unwrap_or_default();
    let fixture = re!(r"\bTEST_[FP]\s*\(\s*((?:::\s*)?[A-Za-z_]\w*(?:\s*::\s*[A-Za-z_]\w*)*)\s*,").captures(&head)?.text(1)?.to_string();
    let site = CppNameSite {
        scope: cpp_parent_scope(&caller.qualified_name),
        file: caller.file_path.clone(),
        line: caller.start_line as usize,
        parameters: Rc::new(HashSet::new()),
        self_id: None,
        class_scope: None,
    };
    let written = re!(r"\s+", "g").replace_all_str(&fixture, "");
    match cpp_class_written(&written, &site, r, ctx) {
        Tri::Value(found) if found.exact && is_cpp_class_kind(found.node.kind) => Some(found.node.qualified_name.clone()),
        _ => None,
    }
}

// -- Pointees, ranges, iterators --------------------------------------------------------------------

/// `cppPointee`.
pub fn cpp_pointee(ty: &CppWrittenType) -> Option<CppWrittenType> {
    let text = trim_ref_tail(&ty.text);
    let b = text.as_bytes();
    let mut depth = 0i64;
    let mut i = b.len() as isize - 1;
    while i >= 0 {
        let ch = b[i as usize];
        if ch == b'>' {
            depth += 1;
        } else if ch == b'<' {
            depth -= 1;
        } else if ch == b'*' && depth == 0 {
            return Some(ty.with_text(js_trim(&text[..i as usize])));
        }
        i -= 1;
    }
    cpp_held_type(&text).map(|held| ty.with_text(held))
}

/// `cppRangeElementOf`: the element type of a range (container, array, begin/end class).
pub fn cpp_range_element_of(ty: &CppWrittenType, at: &UnresolvedRef, ctx: &Ctx, hops: u32) -> Option<CppWrittenType> {
    if ty.array.is_some_and(|n| n > 0) {
        return Some(cpp_array_element(ty));
    }
    let range = cpp_follow_written_aliases(ty, at, ctx)?;
    if is_cpp_pointer_type(&range.text) || hops > 2 {
        return None;
    }
    let segments = cpp_type_segments(&range.text)?;
    let last = segments.last()?.clone();
    if CPP_LIBRARY_SEQUENCES.contains(last.as_str()) || CPP_PROJECT_SEQUENCES.contains(last.as_str()) {
        if CPP_LIBRARY_SEQUENCES.contains(last.as_str()) && segments[0] != "std" && segments[0] != "absl" && !cpp_types_named(&last, ctx).is_empty() {
            return None;
        }
        let element = cpp_template_arguments(&range.text).and_then(|a| a.into_iter().next()).filter(|e| !e.is_empty())?;
        return Some(range.with_text(cpp_without_nullability(&element)));
    }
    if CPP_LIBRARY_MAPS.contains(last.as_str()) && (segments[0] == "std" || segments[0] == "absl") {
        let args = cpp_template_arguments(&range.text).unwrap_or_default();
        return match (args.first(), args.get(1)) {
            (Some(key), Some(value)) if !key.is_empty() && !value.is_empty() => Some(range.with_text(format!(
                "std::pair<const {}, {}>",
                cpp_without_nullability(key),
                cpp_without_nullability(value)
            ))),
            _ => None,
        };
    }
    let cls = cpp_written_class(&range, at, ctx);
    let begin = cls.and_then(|c| cpp_return_type_of(&cpp_methods_of(&c, "begin", at, ctx), ctx));
    begin.and_then(|b| cpp_iterator_element(&b, at, ctx, hops))
}

fn cpp_iterator_of(text: &str) -> Option<String> {
    re!(r"^(.*>)\s*::\s*(?:const_)?(?:reverse_)?iterator\s*$").captures(text).and_then(|c| c.text(1).map(String::from))
}

/// `cppWithoutTypeKeywords`.
pub fn cpp_without_type_keywords(text: &str) -> String {
    js_trim(&re!(r"\b(?:typename|const)\b|&", "g").replace_all_str(text, " ")).to_string()
}

/// `cppLibraryIteratorOf`.
pub fn cpp_library_iterator_of(object: &CppWrittenType, member: &str) -> Option<CppWrittenType> {
    let kind = iterator_member_kind(member)?;
    if is_cpp_pointer_type(&object.text) {
        return None;
    }
    let container = cpp_without_type_keywords(&object.text);
    let segments = cpp_type_segments(&container)?;
    let last = segments.last()?;
    if segments[0] != "std" && segments[0] != "absl" {
        return None;
    }
    if CPP_LIBRARY_SEQUENCES.contains(last.as_str()) || CPP_LIBRARY_MAPS.contains(last.as_str()) {
        Some(object.with_text(format!("{container}::{kind}")))
    } else {
        None
    }
}

/// `cppLibraryElementOf`.
pub fn cpp_library_element_of(object: &CppWrittenType, member: &str, at: &UnresolvedRef, ctx: &Ctx) -> Option<CppWrittenType> {
    if is_cpp_pointer_type(&object.text) {
        return None;
    }
    let segments = cpp_type_segments(&cpp_without_type_keywords(&object.text))?;
    let last = segments.last()?.clone();
    let text = re!(r"^\s*(?:typename\s+)?const\s+").replace_all_str(&object.text, "");
    let text = re!(r"(?:\s*\bconst\b)?\s*&+\s*$").replace_all_str(&text, "");
    let text = js_trim(&text).to_string();
    let library = segments[0] == "std" || segments[0] == "absl";
    if matches!(member, "front" | "back" | "at") {
        if (library && CPP_LIBRARY_SEQUENCES.contains(last.as_str())) || CPP_PROJECT_SEQUENCES.contains(last.as_str()) {
            return cpp_range_element_of(object, at, ctx, 0);
        }
        if member == "at" && library && CPP_LIBRARY_MAPS.contains(last.as_str()) {
            let value = cpp_template_arguments(&text).and_then(|a| a.get(1).cloned()).filter(|v| !v.is_empty())?;
            return Some(object.with_text(cpp_without_nullability(&value)));
        }
    }
    if segments[0] == "std"
        && CPP_LIBRARY_ADAPTORS.contains(last.as_str())
        && (member == "top" || ((member == "front" || member == "back") && last == "queue"))
    {
        let element = cpp_template_arguments(&text).and_then(|a| a.into_iter().next()).filter(|e| !e.is_empty())?;
        return Some(object.with_text(cpp_without_nullability(&element)));
    }
    let held = cpp_held_type(&text)?;
    if held.contains('[') {
        return None;
    }
    let holder = segments.join("::");
    if member == "get" && matches!(holder.as_str(), "std::unique_ptr" | "std::shared_ptr" | "absl::NoDestructor") {
        return Some(object.with_text(format!("{held}*")));
    }
    if (member == "value" || member == "value_or") && matches!(holder.as_str(), "std::optional" | "absl::optional" | "absl::StatusOr") {
        return Some(object.with_text(held));
    }
    None
}

/// `cppIteratorElement`.
pub fn cpp_iterator_element(ty: &CppWrittenType, at: &UnresolvedRef, ctx: &Ctx, hops: u32) -> Option<CppWrittenType> {
    let iterator = cpp_follow_written_aliases(ty, at, ctx)?;
    if is_cpp_pointer_type(&iterator.text) {
        return cpp_pointee(&iterator);
    }
    if let Some(nested) = cpp_iterator_of(&cpp_without_type_keywords(&iterator.text)) {
        return cpp_range_element_of(&iterator.with_text(nested), at, ctx, hops + 1);
    }
    let cls = cpp_written_class(&iterator, at, ctx);
    let star = cls.and_then(|c| cpp_return_type_of(&cpp_methods_of(&c, "operator*", at, ctx), ctx));
    star.map(|s| {
        let text = trim_ref_tail(&s.text);
        s.with_text(text)
    })
}

/// `cppMemberObject`.
pub fn cpp_member_object(object: &str, op: &str, at: &UnresolvedRef, ctx: &Ctx, depth: u32) -> Option<CppWrittenType> {
    let ty = cpp_expression_type(object, at, ctx, depth + 1)?;
    let followed = cpp_follow_written_aliases(&ty, at, ctx)?;
    if op == "->" {
        return cpp_dereferenced(&followed, at, ctx);
    }
    if is_cpp_pointer_type(&followed.text) || followed.array.is_some_and(|n| n > 0) {
        None
    } else {
        Some(followed)
    }
}

/// `cppExpressionType`: the written type an expression has.
pub fn cpp_expression_type(expr: &str, at: &UnresolvedRef, ctx: &Ctx, depth: u32) -> Option<CppWrittenType> {
    if depth > 4 {
        return None;
    }
    let mut text = js_trim(expr).to_string();
    while text.starts_with('(') && text.ends_with(')') && cpp_opening_paren(&text, text.len() - 1) == Some(0) {
        text = js_trim(&text[1..text.len() - 1]).to_string();
    }
    if let Some(rest) = text.strip_prefix('*') {
        let pointer = cpp_expression_type(rest, at, ctx, depth + 1)?;
        let followed = cpp_follow_written_aliases(&pointer, at, ctx)?;
        return cpp_dereferenced(&followed, at, ctx);
    }
    let caller = ctx.node_by_id(&at.from_node_id)?;
    let callers = CppWrittenType {
        text: String::new(),
        scope: caller.qualified_name.clone(),
        file: at.file_path.clone(),
        line: at.line as usize,
        parameters: cpp_template_parameters(&caller, ctx),
        array: None,
        class_scope: None,
    };
    if text == "this" {
        let owner = cpp_member_owner(&caller, ctx).or_else(|| cpp_test_fixture(&caller, at, ctx));
        return owner.map(|o| callers.with_text(format!("{o}*")));
    }
    if let Some(name) = re!(r"^(?:this\s*->\s*)?([A-Za-z_]\w*)$").captures(&text).and_then(|c| c.text(1).map(String::from)) {
        let mut decl = CppReceiverDecl::default();
        infer_cpp_receiver_type(&name, at, ctx, depth + 1, &mut decl);
        if let Some(declared) = decl.declared.clone() {
            return Some(declared);
        }
        if let Some((line, init)) = decl.initializer.clone() {
            if is_cpp_callers_declaration(&at.file_path, line, at, ctx) {
                let site = UnresolvedRef { line: line as u32, column: 0, ..at.clone() };
                return cpp_expression_type(&init, &site, ctx, depth + 1);
            }
        }
        let raw = decl.raw.clone()?;
        if !decl.callers || decl.shadowed {
            return None;
        }
        let mut ty = callers.with_text(raw);
        if decl.array > 0 {
            ty.array = Some(decl.array);
        }
        return Some(ty);
    }
    if let Some(created) = re!(r"^new\s+((?:::\s*)?[A-Za-z_][\w:]*(?:\s*<[^;{}()]*>)?)\s*(?:[({\[][\s\S]*)?$").captures(&text) {
        return Some(callers.with_text(format!("{}*", js_trim(created.text(1).unwrap_or("")))));
    }
    if text.ends_with(']') {
        let open = cpp_opening_bracket(&text, text.len() - 1);
        let container = match open {
            Some(open) if open > 0 => cpp_expression_type(&text[..open], at, ctx, depth + 1),
            _ => None,
        };
        let followed = container.and_then(|c| cpp_follow_written_aliases(&c, at, ctx))?;
        return cpp_subscript_element(&followed, at, ctx);
    }
    if !text.ends_with(')') {
        let field = re!(r"^(.+?)\s*(\.|->)\s*([A-Za-z_]\w*)$").captures(&text)?;
        let object = cpp_member_object(field.text(1)?, field.text(2)?, at, ctx, depth);
        let member = field.text(3)?;
        let element = object.as_ref().and_then(|o| cpp_pair_member(o, member, at, ctx));
        if element.is_some() {
            return element;
        }
        let cls = object.as_ref().and_then(|o| cpp_written_class(o, at, ctx))?;
        let mut seen = HashSet::new();
        seen.insert(cls.qualified_name.clone());
        return cpp_inherited_data_member(std::slice::from_ref(&cls), member, ctx, 0, &mut seen);
    }
    let open = cpp_opening_paren(&text, text.len() - 1);
    let callee = match open {
        Some(open) if open > 0 => js_trim(&text[..open]).to_string(),
        _ => String::new(),
    };
    let open = open.unwrap_or(0);
    if let Some(cast) = re!(r"^(?:static|dynamic|const|reinterpret)_cast\s*<([\s\S]+)>$").captures(&callee) {
        return Some(callers.with_text(js_trim(cast.text(1).unwrap_or(""))));
    }
    if let Some(made) = re!(r"^(?:::\s*)?(?:std|absl)\s*::\s*make_(unique|shared)\s*<([\s\S]+)>$").captures(&callee) {
        return Some(callers.with_text(format!("std::{}_ptr<{}>", made.text(1).unwrap_or(""), js_trim(made.text(2).unwrap_or("")))));
    }
    if cpp_held_type(&callee).is_some() {
        return Some(callers.with_text(callee));
    }
    if re!(r"^(?:::\s*)?std\s*::\s*(?:find|find_if|find_if_not|lower_bound|upper_bound|min_element|max_element|partition_point|adjacent_find|next|prev)$")
        .is_match(&callee)
    {
        let first = split_cpp_top_level(&text[open + 1..text.len() - 1]).into_iter().next().filter(|f| !f.is_empty());
        return first.and_then(|f| cpp_expression_type(&f, at, ctx, depth + 1));
    }
    if re!(r"^[A-Za-z_]\w*$").is_match(&callee) {
        let wrapper = cpp_expression_type(&callee, at, ctx, depth + 1);
        if let Some(returned) = wrapper.and_then(|w| cpp_wrapper_return_type(&w)) {
            return Some(returned);
        }
    }
    if let Some(member) = re!(r"^(.+?)\s*(\.|->)\s*([A-Za-z_]\w*)\s*(<[\s\S]*>)?$").captures(&callee) {
        let object = cpp_member_object(member.text(1)?, member.text(2)?, at, ctx, depth);
        let name = member.text(3)?;
        let args = member.text(4);
        if let Some(o) = object.as_ref().filter(|_| args.is_none()) {
            if let Some(iterator) = cpp_library_iterator_of(o, name) {
                return Some(iterator);
            }
            if let Some(element) = cpp_library_element_of(o, name, at, ctx) {
                return Some(element);
            }
        }
        let cls = object.as_ref().and_then(|o| cpp_written_class(o, at, ctx))?;
        let methods = cpp_methods_of(&cls, name, at, ctx);
        let Some(args) = args else { return cpp_return_type_of(&methods, ctx) };
        let templates: Vec<NodeRef> = methods.iter().filter(|f| !cpp_own_template_parameters(f, ctx).is_empty()).cloned().collect();
        return cpp_member_template_return(&templates, args, &callers, ctx).or_else(|| cpp_return_type_of(&templates, ctx));
    }
    if !re!(r"^(?:[A-Za-z_]\w*\s*::\s*)*[A-Za-z_]\w*(?:\s*<[\s\S]*>)?$").is_match(&callee) {
        return None;
    }
    let spelled = re!(r"\s+", "g").replace_all_str(&strip_cpp_template_arguments(&callee), "");
    let own = if spelled.contains("::") { None } else { cpp_implicit_this_method(&spelled, Receiver::This, &caller, at, ctx) };
    if let Some(own) = own {
        return cpp_return_type_of(&[own], ctx);
    }
    for scope in cpp_scopes_within(&cpp_parent_scope(&caller.qualified_name)) {
        let qualified = if scope.is_empty() { spelled.clone() } else { format!("{scope}::{spelled}") };
        let fns: Vec<NodeRef> = ctx.get_nodes_by_qualified_name(&qualified).iter().filter(|n| is_cpp_function_node(n)).cloned().collect();
        if !fns.is_empty() {
            return cpp_return_type_of(&fns, ctx);
        }
    }
    match cpp_class_written(&callee, &callers.site(), at, ctx) {
        Tri::Value(c) if c.exact && is_cpp_class_kind(c.node.kind) => Some(callers.with_text(callee)),
        _ => None,
    }
}

// -- `auto` locals ------------------------------------------------------------------------------

memo_map!(CppLoopElements, String, Option<CppWrittenType>);
memo_map!(CppAutoLocals, String, Option<CppWrittenType>);

/// `cppAutoLoopElement`: the element type of the range an `auto` loop variable iterates.
/// `colon` is a byte offset into `lines[line]`; `line` is the 0-based line index of the header.
pub fn cpp_auto_loop_element(lines: &[String], line: usize, colon: usize, r: &UnresolvedRef, ctx: &Ctx) -> Option<CppWrittenType> {
    let memo = ctx.memo::<CppLoopElements>();
    let key = format!("{}\u{0}{}\u{0}{}:{}", r.from_node_id, r.file_path, line, colon);
    if let Some(hit) = memo.lookup(&key) {
        return hit;
    }
    memo.store(key.clone(), None);
    let range = cpp_range_expression(lines, line, colon);
    let column = lines.get(line).map(|l| codegraph_types::jscompat::utf16_len(super::util::slice_bytes(l, 0, colon))).unwrap_or(colon);
    let at = UnresolvedRef { line: line as u32 + 1, column: column as u32, ..r.clone() };
    let ty = range.and_then(|range| cpp_expression_type(&range, &at, ctx, 0));
    let element = ty.and_then(|t| cpp_range_element_of(&t, &at, ctx, 0));
    let followed = element.and_then(|e| cpp_follow_written_aliases(&e, &at, ctx));
    let result = followed.filter(|f| !is_cpp_template_parameter_type(f));
    memo.store(key, result.clone());
    result
}

/// `cppAutoLocalType`: the type an `auto` local's initializer has.
pub fn cpp_auto_local_type(init: &str, line: usize, name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<CppWrittenType> {
    let memo = ctx.memo::<CppAutoLocals>();
    let key = format!("{}\u{0}{}\u{0}{}\u{0}{}", r.from_node_id, r.file_path, line, name);
    if let Some(hit) = memo.lookup(&key) {
        return hit;
    }
    memo.store(key.clone(), None);
    let at = UnresolvedRef { line: line as u32, column: 0, ..r.clone() };
    let ty = cpp_expression_type(init, &at, ctx, 0);
    let followed = ty.and_then(|t| cpp_follow_written_aliases(&t, &at, ctx));
    let result = followed.filter(|f| !is_cpp_template_dependent_type(f, ctx));
    memo.store(key, result.clone());
    result
}

/// `isCppTemplateDependentType`.
pub fn is_cpp_template_dependent_type(ty: &CppWrittenType, ctx: &Ctx) -> bool {
    if ty.parameters.is_empty() {
        return false;
    }
    if is_cpp_template_parameter_type(ty) {
        return true;
    }
    if let Some(held) = cpp_held_type(&ty.text) {
        return is_cpp_template_dependent_type(&ty.with_text(held), ctx);
    }
    (is_cpp_library_type(&ty.text, ctx) || cpp_type_segments(&ty.text).is_some_and(|s| s[0] == "std"))
        && re!(r"[A-Za-z_]\w*", "g").captures_iter(&ty.text).any(|m| m.text(0).is_some_and(|w| ty.parameters.contains(w)))
}

// -- Holders, library types, arrows ----------------------------------------------------------------

memo_map!(CppClassesInFile, String, Rc<Vec<NodeRef>>);

/// `cppClassesIn`.
pub fn cpp_classes_in(file: &str, ctx: &Ctx) -> Rc<Vec<NodeRef>> {
    let memo = ctx.memo::<CppClassesInFile>();
    if let Some(hit) = memo.lookup(&file.to_string()) {
        return hit;
    }
    let classes: Vec<NodeRef> = ctx.get_nodes_in_file(file).iter().filter(|n| is_cpp_class_kind(n.kind)).cloned().collect();
    let classes = Rc::new(classes);
    memo.store(file.to_string(), Rc::clone(&classes));
    classes
}

/// `isCppCallersDeclaration`: was the declaration at `file:line` made in the caller's own scope?
pub fn is_cpp_callers_declaration(file: &str, line: usize, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let Some(caller) = ctx.node_by_id(&r.from_node_id) else { return false };
    if !matches!(caller.kind, NodeKind::Method | NodeKind::Function) {
        return false;
    }
    if file == r.file_path && line >= caller.start_line as usize && line <= r.line as usize {
        return true;
    }
    let Some(cut) = caller.qualified_name.rfind("::") else { return false };
    let mut innermost: Option<NodeRef> = None;
    for cls in cpp_classes_in(file, ctx).iter() {
        if line < cls.start_line as usize || line > cls.end_line.max(cls.start_line) as usize {
            continue;
        }
        if innermost.as_ref().is_none_or(|i| cls.start_line >= i.start_line) {
            innermost = Some(Rc::clone(cls));
        }
    }
    innermost.is_some_and(|i| i.qualified_name == caller.qualified_name[..cut])
}

/// `cppTypesNamed`.
pub fn cpp_types_named(name: &str, ctx: &Ctx) -> Vec<NodeRef> {
    ctx.get_nodes_by_name(name).iter().filter(|n| is_c_family_node(n) && CPP_TYPE_KINDS.contains(&n.kind)).cloned().collect()
}

fn cpp_iterator_name(name: &str) -> bool {
    re!(r"^(?:const_)?(?:reverse_|local_)?iterator$").is_match(name)
}

/// `isCppSpecializationMember`: `Type<Args>::member`.
fn is_cpp_specialization_member(raw: &str) -> bool {
    let mut depth = 0i64;
    for (i, c) in raw.bytes().enumerate() {
        if c == b'<' {
            depth += 1;
        } else if c == b'>' {
            depth -= 1;
            if depth == 0 && re!(r"^\s*::").is_match(&raw[i + 1..]) {
                return true;
            }
        }
    }
    false
}

/// `isCppLibraryType`: a standard-library (or otherwise undeclared lowercase) type.
pub fn is_cpp_library_type(raw: &str, ctx: &Ctx) -> bool {
    if raw.matches('<').count() != raw.matches('>').count() {
        return false;
    }
    let Some(segments) = cpp_type_segments(raw) else { return false };
    if segments.len() < 2 {
        return false;
    }
    let name = segments.last().unwrap();
    if !re!(r"^[a-z]").is_match(name) || re!(r"_t$").is_match(name) {
        return false;
    }
    if !cpp_iterator_name(name) && is_cpp_specialization_member(raw) {
        return false;
    }
    if segments[..segments.len() - 1].iter().any(|s| !cpp_types_named(s, ctx).is_empty()) {
        return false;
    }
    let declared = cpp_types_named(name, ctx);
    if segments[0] != "std" {
        return declared.is_empty() || super::cpp_call::cpp_finds_only_namesakes(raw, ctx, None);
    }
    let spelled = segments.join("::");
    !declared.iter().any(|n| n.qualified_name == spelled || n.qualified_name.ends_with(&format!("::{spelled}")))
}

/// `cppHeldType`: the `T` of `std::unique_ptr<T>` and friends.
pub fn cpp_held_type(raw: &str) -> Option<String> {
    let holder = cpp_type_segments(raw)?;
    if holder.len() != 2 || !CPP_HOLDERS.contains(holder.join("::").as_str()) {
        return None;
    }
    cpp_template_arguments(raw).and_then(|a| a.into_iter().next()).filter(|a| !a.is_empty())
}

/// `isCppLibraryHolder`.
fn is_cpp_library_holder(raw: &str, ctx: &Ctx) -> bool {
    let spelled = cpp_type_segments(raw).map(|s| s.join("::")).unwrap_or_default();
    !cpp_types_named(cpp_last_segment(&spelled), ctx)
        .iter()
        .any(|n| n.qualified_name == spelled || n.qualified_name.ends_with(&format!("::{spelled}")))
}

/// `cppArrowTarget`.
pub fn cpp_arrow_target(
    raw: &str,
    site: &CppNameSite,
    declared: Option<&CppWrittenType>,
    r: &UnresolvedRef,
    ctx: &Ctx,
) -> Option<CppWrittenType> {
    if let Some(held) = cpp_held_type(raw) {
        return if is_cpp_library_holder(raw, ctx) { Some(written_at_site(site, held)) } else { None };
    }
    let base = declared.cloned().unwrap_or_else(|| written_at_site(site, ""));
    cpp_iterator_target(&base.with_text(raw), r, ctx)
}

/// `cppIteratorTarget`.
pub fn cpp_iterator_target(iterator: &CppWrittenType, at: &UnresolvedRef, ctx: &Ctx) -> Option<CppWrittenType> {
    let container = cpp_iterator_of(&cpp_without_type_keywords(&iterator.text));
    let element = match container {
        Some(c) => cpp_range_element_of(&iterator.with_text(c), at, ctx, 1),
        None => cpp_operator_target(iterator, at, ctx, 0),
    };
    element.filter(|e| e.scope == iterator.scope || !is_cpp_template_parameter_type(e))
}

/// `cppOperatorTarget`.
pub fn cpp_operator_target(ty: &CppWrittenType, at: &UnresolvedRef, ctx: &Ctx, hops: u32) -> Option<CppWrittenType> {
    let written = cpp_without_type_keywords(&ty.text);
    if hops > 2 || written.is_empty() || is_cpp_pointer_type(&written) || is_cpp_library_type(&written, ctx) {
        return None;
    }
    let found = match cpp_class_written(&written, &ty.site(), at, ctx) {
        Tri::Value(f) if f.exact && is_cpp_class_kind(f.node.kind) => f,
        _ => return None,
    };
    let arrow = cpp_methods_of(&found.node, "operator->", at, ctx);
    let returned = cpp_return_type_of(
        &if arrow.is_empty() { cpp_methods_of(&found.node, "operator*", at, ctx) } else { arrow.clone() },
        ctx,
    )?;
    let bound = cpp_template_bound(&returned, &found.node, &ty.with_text(written), at, ctx)?;
    if arrow.is_empty() {
        let text = trim_ref_tail(&bound.text);
        return Some(bound.with_text(text));
    }
    cpp_pointee(&bound).or_else(|| cpp_operator_target(&bound, at, ctx, hops + 1))
}

/// `cppTemplateBound`.
pub fn cpp_template_bound(returned: &CppWrittenType, cls: &Node, ty: &CppWrittenType, at: &UnresolvedRef, ctx: &Ctx) -> Option<CppWrittenType> {
    let params = cpp_own_template_parameters(cls, ctx);
    if params.is_empty() {
        return Some(returned.clone());
    }
    let args = cpp_template_arguments(&ty.text).unwrap_or_default();
    let followed = cpp_follow_written_aliases(returned, at, ctx).unwrap_or_else(|| returned.clone());
    let stripped = re!(r"\b(?:const|volatile|typename)\b|[*&]", "g").replace_all_str(&followed.text, " ");
    let core = js_trim(&stripped).to_string();
    if let Some(index) = params.iter().position(|p| *p == core) {
        let arg = args.get(index).map(|a| js_trim(a).to_string()).filter(|a| !a.is_empty())?;
        let replaced = Re::cached(&format!(r"\b{core}\b"), "").replace_first(&followed.text, |_| arg.clone());
        return Some(ty.with_text(replaced));
    }
    let mut text = followed.text.clone();
    for (i, param) in params.iter().enumerate() {
        let arg = args.get(i).map(|a| js_trim(a).to_string()).filter(|a| !a.is_empty());
        if let (false, Some(arg)) = (param.is_empty(), arg) {
            text = Re::cached(&format!(r"\b{param}\b"), "g").replace_all(&text, |_| Some(arg.clone()));
        }
    }
    Some(followed.with_text(text))
}

/// `cppDereferenced`.
pub fn cpp_dereferenced(ty: &CppWrittenType, at: &UnresolvedRef, ctx: &Ctx) -> Option<CppWrittenType> {
    if ty.array.is_some_and(|n| n > 0) {
        return Some(cpp_array_element(ty));
    }
    cpp_pointee(ty).or_else(|| cpp_iterator_target(ty, at, ctx))
}

// -- Wrappers and pairs --------------------------------------------------------------------------------

/// `cppWrapperReturnType`: the return type of a `std::function<R(Args)>`.
pub fn cpp_wrapper_return_type(ty: &CppWrittenType) -> Option<CppWrittenType> {
    let signature = cpp_template_arguments(&ty.text).and_then(|a| a.into_iter().next()).unwrap_or_default();
    let mut depth = 0i64;
    for (i, c) in signature.bytes().enumerate() {
        if c == b'<' {
            depth += 1;
        } else if c == b'>' {
            depth -= 1;
        } else if c == b'(' && depth == 0 {
            let returned = js_trim(&signature[..i]);
            return if returned.is_empty() { None } else { Some(ty.with_text(returned)) };
        }
    }
    None
}

/// `cppBindingElements`: the element types a structured binding of a pair/tuple destructures.
pub fn cpp_binding_elements(ty: &CppWrittenType, at: &UnresolvedRef, ctx: &Ctx) -> Option<Vec<CppWrittenType>> {
    let followed = cpp_follow_written_aliases(ty, at, ctx)?;
    let segments = cpp_type_segments(&followed.text)?;
    if segments.len() != 2 || segments[0] != "std" || (segments[1] != "pair" && segments[1] != "tuple") {
        return None;
    }
    cpp_template_arguments(&followed.text).map(|args| args.iter().map(|t| followed.with_text(cpp_without_nullability(t))).collect())
}

/// `cppPairMember`: `.first` / `.second` of a `std::pair`.
pub fn cpp_pair_member(pair: &CppWrittenType, member: &str, at: &UnresolvedRef, ctx: &Ctx) -> Option<CppWrittenType> {
    let index = match member {
        "first" => 0usize,
        "second" => 1,
        _ => return None,
    };
    let followed = cpp_follow_written_aliases(pair, at, ctx)?;
    if cpp_type_segments(&followed.text).map(|s| s.join("::")).as_deref() != Some("std::pair") {
        return None;
    }
    let element = cpp_template_arguments(&followed.text).and_then(|a| a.get(index).cloned()).filter(|e| !e.is_empty())?;
    Some(followed.with_text(cpp_without_nullability(&element)))
}

/// `cppStatementEnd`.
pub fn cpp_statement_end(text: &str) -> Option<usize> {
    let mut depth = 0i64;
    for (i, c) in text.bytes().enumerate() {
        match c {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b';' if depth <= 0 => return Some(i),
            _ => {}
        }
    }
    None
}

