//! C++ receiver typing — port of `inferCppReceiverType` and its helpers: where a receiver was
//! declared, what it was declared as, and the names a file brings into scope.

use super::cpp_decl::{
    build_declarator_regex, build_listed_declarator_regex, cpp_code_of, cpp_declaration_begun_above, cpp_ends_declaration_begun_above,
    cpp_for_body_encloses, cpp_rebinds_receiver, cpp_range_for_declaration, cpp_type_began_above, esc, match_cpp_declarator,
    match_cpp_declarator_from, match_cpp_listed_declarator, normalize_cpp_type_name,
};
use super::cpp_types::{
    cpp_array_dimensions, cpp_auto_local_type, cpp_auto_loop_element, cpp_binding_elements, cpp_callers_member, cpp_follow_written_aliases,
    cpp_last_segment, cpp_statement_end, cpp_types_named, is_cpp_callers_declaration, is_cpp_template_dependent_type, CppWrittenType,
};
use super::method::is_undeclared_type_name;
use super::util::{col_byte, memo_map};
use crate::cpp_raw_strings::{cpp_literals_blanked, cpp_non_code_lines, NonCodeLines};
use crate::cpp_type_aliases::{
    cpp_aliased_type_name, cpp_type_names_declared, is_cpp_pointer_type, resolve_cpp_aliased_type, CppAliasedType,
};
use crate::ctx::Ctx;
use crate::types::{NodeRef, Tri, UnresolvedRef};
use codegraph_extract::strip_comments::strip_comments_for_regex;
use codegraph_jsre::{re, Re};
use codegraph_types::jscompat::js_trim;
use codegraph_types::{Language, Node, NodeKind};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// What `inferCppReceiverType` learned about a receiver's declaration (`CppReceiverDeclaration`).
#[derive(Default, Clone, Debug)]
pub struct CppReceiverDecl {
    /// The receiver's type was reached through a `using`/`typedef` alias (followed or not).
    pub aliased: bool,
    pub alias: Option<CppAliasedType>,
    /// The type as written, before aliases.
    pub written: Option<String>,
    pub pointer: bool,
    pub class_template: bool,
    /// The declared type text.
    pub raw: Option<String>,
    pub constant: bool,
    pub array: u32,
    /// Declared in the calling function (or its class), not somewhere it cannot see.
    pub callers: bool,
    /// A later declaration of the same name hides the one found.
    pub shadowed: bool,
    /// The full written type, with where it was written.
    pub declared: Option<CppWrittenType>,
    /// The `= init` of an `auto` declaration: its line and statement text.
    pub initializer: Option<(usize, String)>,
}

/// `noteCppDeclaration`.
fn note_cpp_declaration(found: &mut CppReceiverDecl, raw: &str, callers: bool, array: u32, constant: bool) {
    found.raw = Some(raw.to_string());
    found.callers = callers;
    found.pointer = is_cpp_pointer_type(raw);
    found.array = array;
    found.constant = constant;
}

/// `cppDeclaredType`: the type name to look methods up on, following a `using`/`typedef`.
pub fn cpp_declared_type(
    raw: &str,
    normalized: &str,
    in_caller_scope: bool,
    r: &UnresolvedRef,
    ctx: &Ctx,
    found: Option<&mut CppReceiverDecl>,
) -> Option<String> {
    if !in_caller_scope {
        return Some(normalized.to_string());
    }
    let aliased = resolve_cpp_aliased_type(raw, r, ctx);
    if aliased.is_undefined() {
        return Some(normalized.to_string());
    }
    if let Some(found) = found {
        found.aliased = true;
        if let Tri::Value(a) = &aliased {
            found.alias = Some(a.clone());
        }
        found.written = Some(normalized.to_string());
        found.pointer = match &aliased {
            Tri::Value(a) => a.pointer,
            _ => is_cpp_pointer_type(raw),
        };
        found.class_template = matches!(&aliased, Tri::Value(a) if a.class_template);
    }
    match aliased {
        Tri::Value(a) => Some(cpp_aliased_type_name(&a)),
        _ => None,
    }
}

// -- Names a file brings into scope --------------------------------------------------------------

/// `CppFileNames`: the type and namespace names a file declares or macro-expands.
#[derive(Default, Debug)]
pub struct CppFileNames {
    pub types: HashSet<String>,
    pub namespaces: HashSet<String>,
}

memo_map!(CppFileTypeNames, String, Rc<CppFileNames>);

#[derive(Default)]
struct CppMacroTypeNamesMemo(std::cell::RefCell<Option<Rc<HashMap<String, HashSet<String>>>>>);

/// `hasCppWord`.
pub fn has_cpp_word(text: &str, word: &str) -> bool {
    if word.is_empty() {
        return text.contains(word) && !text.is_empty() && false;
    }
    let b = text.as_bytes();
    let mut from = 0usize;
    while let Some(p) = text[from..].find(word) {
        let at = from + p;
        let before_ok = at == 0 || !super::cpp_decl::is_word_byte(b[at - 1]);
        let after_ok = at + word.len() >= b.len() || !super::cpp_decl::is_word_byte(b[at + word.len()]);
        if before_ok && after_ok {
            return true;
        }
        from = at + text[at..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
    }
    false
}

/// `cppMacroTypeNames`: the type names a header `#define` declares, by macro name.
fn cpp_macro_type_names(ctx: &Ctx) -> Rc<HashMap<String, HashSet<String>>> {
    let memo = ctx.memo::<CppMacroTypeNamesMemo>();
    if let Some(hit) = memo.0.borrow().as_ref() {
        return Rc::clone(hit);
    }
    let mut macros: HashMap<String, HashSet<String>> = HashMap::new();
    for file in ctx.get_all_files() {
        if !re!(r"\.(?:h|hh|hpp|hxx|h\+\+|inl|inc|ipp|tcc)$", "i").is_match(&file) {
            continue;
        }
        let Some(raw) = ctx.read_file(&file) else { continue };
        if raw.is_empty() || !raw.contains("define") {
            continue;
        }
        for m in re!(r"^[ \t]*#[ \t]*define[ \t]+([A-Za-z_]\w*)(?:\([^)\n]*\))?((?:[^\n\\]|\\\r?\n|\\(?!\r?\n))*)", "gm").captures_iter(&raw) {
            let (Some(name), Some(body)) = (m.text(1), m.text(2)) else { continue };
            if macros.contains_key(name) || !re!(r"\b(?:using|typedef|template)\b").is_match(body) {
                continue;
            }
            let flat = re!(r"\\\r?\n", "g").replace_all_str(body, " ");
            let names = cpp_type_names_declared(&strip_comments_for_regex(&flat, "cpp"));
            if !names.is_empty() {
                macros.insert(name.to_string(), names);
            }
        }
    }
    let macros = Rc::new(macros);
    *memo.0.borrow_mut() = Some(Rc::clone(&macros));
    macros
}

/// `cppNamesBroughtIn`: the types and namespaces `file` declares or expands macros into.
pub fn cpp_names_brought_in(file: &str, ctx: &Ctx) -> Rc<CppFileNames> {
    use super::util::MemoMap;
    let memo = ctx.memo::<CppFileTypeNames>();
    if let Some(hit) = memo.lookup(&file.to_string()) {
        return hit;
    }
    let source = ctx.read_file(file).map(|s| s.to_string()).unwrap_or_default();
    let flat = re!(r"\\\r?\n", "g").replace_all_str(&source, " ");
    let text = strip_comments_for_regex(&flat, "cpp");
    let mut types = cpp_type_names_declared(&text);
    let mut namespaces = HashSet::new();
    for m in re!(r"\bnamespace\s+([^;{}()]{1,200}?)\s*[;{]", "g").captures_iter(&text) {
        for name in re!(r"[A-Za-z_]\w*", "g").captures_iter(m.text(1).unwrap_or("")) {
            namespaces.insert(name.text(0).unwrap_or("").to_string());
        }
    }
    for (macro_name, declared) in cpp_macro_type_names(ctx).iter() {
        if has_cpp_word(&text, macro_name) {
            types.extend(declared.iter().cloned());
        }
    }
    if re!(r"\bTYPED_TEST(?:_P)?\s*\(").is_match(&text) {
        types.insert("TypeParam".to_string());
        types.insert("TestFixture".to_string());
    }
    let names = Rc::new(CppFileNames { types, namespaces });
    memo.store(file.to_string(), Rc::clone(&names));
    names
}

/// `isCppUndeclaredQualifier`: `Foo::bar` where `Foo` names nothing the project or file declares.
pub fn is_cpp_undeclared_qualifier(qualifier: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if !is_undeclared_type_name(qualifier, r, ctx)
        || !cpp_types_named(qualifier, ctx).is_empty()
        || ctx.get_nodes_by_name(qualifier).iter().any(|n| {
            matches!(n.language, Language::Cpp | Language::C) && matches!(n.kind, NodeKind::Namespace | NodeKind::Module)
        })
    {
        return false;
    }
    let names = cpp_names_brought_in(&r.file_path, ctx);
    !names.types.contains(qualifier) && !names.namespaces.contains(qualifier)
}

/// `isCppNamespaceQualifier`: a lowercase qualifier that is not a type.
pub fn is_cpp_namespace_qualifier(qualifier: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    re!(r"^[a-z_]").is_match(qualifier)
        && cpp_types_named(qualifier, ctx).is_empty()
        && !cpp_names_brought_in(&r.file_path, ctx).types.contains(qualifier)
}

/// `isCppConstructorThrough`: `namespace::Type::Type` — a constructor reached through a namespace.
pub fn is_cpp_constructor_through(namespace: &str, method: &Node, ctx: &Ctx) -> bool {
    let parts: Vec<&str> = method.qualified_name.split("::").collect();
    if parts.len() < 3 || parts[parts.len() - 3] != namespace {
        return false;
    }
    let owner = parts[parts.len() - 2];
    if owner == method.name {
        return true;
    }
    if owner != "<anonymous>" {
        return false;
    }
    let holder_name = parts[..parts.len() - 1].join("::");
    let holder = ctx
        .get_nodes_by_qualified_name(&holder_name)
        .iter()
        .find(|n| n.file_path == method.file_path && n.start_line <= method.start_line && n.end_line.max(n.start_line) >= method.start_line)
        .cloned();
    let head = holder
        .and_then(|h| {
            ctx.get_file_lines(&h.file_path).map(|lines| {
                let from = (h.start_line as usize).saturating_sub(1).min(lines.len());
                let to = (h.start_line as usize + 1).min(lines.len());
                lines[from..to].join(" ")
            })
        })
        .unwrap_or_default();
    Re::cached(&format!(r"\b(?:class|struct|union)\s+(?:[A-Za-z_]\w*\s+)*{}\s*<", esc(&method.name)), "").is_match(&head)
}

// -- Inference --------------------------------------------------------------------------------------

fn leading_const(before: &str) -> bool {
    re!(r"\b(?:const|constexpr)\s+$").is_match(before)
}

/// `inferCppReceiverType`: the type name a receiver was declared as, filling in `found`.
pub fn infer_cpp_receiver_type(receiver_name: &str, r: &UnresolvedRef, ctx: &Ctx, depth: u32, found: &mut CppReceiverDecl) -> Option<String> {
    let lines = ctx.get_file_lines(&r.file_path)?;
    if lines.is_empty() {
        return None;
    }
    let call_line_index = ((r.line as i64 - 1).min(lines.len() as i64 - 1)).max(0) as usize;
    let escaped = esc(receiver_name);
    let receiver_pattern = Re::cached(&format!(r"\b{escaped}\b"), "");
    let declarator_regex = build_declarator_regex(&escaped);
    let listed_regex = build_listed_declarator_regex(&escaped);
    let raw_lines = cpp_non_code_lines(&r.file_path, ctx);
    let caller = ctx.node_by_id(&r.from_node_id);
    let member_caller: Option<NodeRef> =
        caller.filter(|c| c.file_path == r.file_path && matches!(c.kind, NodeKind::Method | NodeKind::Function));
    let mut member_looked = member_caller.is_none();
    let use_column = col_byte(&lines[call_line_index], r.column);

    let mut i = call_line_index as isize;
    while i >= 0 {
        let idx = i as usize;
        i -= 1;
        if !member_looked {
            if let Some(mc) = &member_caller {
                if idx < (mc.start_line as usize).saturating_sub(1) {
                    member_looked = true;
                    if let Some(member) = cpp_member_receiver_type(receiver_name, mc, r, ctx, found) {
                        return member;
                    }
                }
            }
        }
        let text = &lines[idx];
        if text.is_empty() || !receiver_pattern.is_match(text) {
            continue;
        }
        let Some(line) = cpp_code_of(&cpp_literals_blanked(text, idx + 1, &raw_lines)).filter(|l| !l.is_empty() && receiver_pattern.is_match(l)) else {
            continue;
        };
        let mut declarator_match = match_cpp_declarator(&line, &declarator_regex).or_else(|| match_cpp_listed_declarator(&line, receiver_name, &listed_regex));
        if let Some(m) = &declarator_match {
            if member_caller.as_ref().is_some_and(|c| c.name == receiver_name) && line.as_bytes().get(m.end) == Some(&b'(') {
                let end = m.end;
                declarator_match = match_cpp_declarator_from(&line, &declarator_regex, end).or(declarator_match);
            }
        }
        if let Some(m) = declarator_match {
            let normalized = normalize_cpp_type_name(&m.ty);
            if normalized.as_deref() == Some("auto") {
                let init = cpp_auto_initializer(&line, receiver_name);
                let statement = cpp_auto_initializer_statement(&line, &lines, idx, receiver_name, &raw_lines);
                if let Some(init) = &init {
                    if !found.shadowed && found.initializer.is_none() {
                        found.initializer = Some((idx + 1, statement.clone().unwrap_or_else(|| init.clone())));
                    }
                }
                let local = match &statement {
                    Some(st) if !found.shadowed && is_cpp_callers_declaration(&r.file_path, idx + 1, r, ctx) => {
                        cpp_auto_local_type(st, idx + 1, receiver_name, r, ctx)
                    }
                    _ => None,
                };
                if let Some(local) = local {
                    if let Some(local_type) = normalize_cpp_type_name(&local.text).filter(|t| t != "auto") {
                        note_cpp_declaration(found, &local.text, true, 0, false);
                        found.declared = Some(local.clone());
                        return cpp_declared_type(&local.text, &local_type, true, r, ctx, Some(found));
                    }
                }
                let init_type = init.as_deref().and_then(|init| infer_cpp_auto_initializer_type(init, r, ctx, depth));
                if init_type.is_some() {
                    return init_type;
                }
                found.shadowed = true;
            } else if let Some(normalized) = normalized {
                let in_caller_scope = is_cpp_callers_declaration(&r.file_path, idx + 1, r, ctx);
                let array = cpp_array_dimensions(m.rest(&line));
                note_cpp_declaration(found, &m.ty, in_caller_scope, array, leading_const(&line[..m.index.min(line.len())]));
                return cpp_declared_type(&m.ty, &normalized, in_caller_scope, r, ctx, Some(found));
            } else if cpp_type_began_above(&m.ty) {
                found.shadowed = true;
            }
        } else {
            let loop_var = cpp_range_for_declaration(&line, receiver_name);
            if let Some(loop_var) = loop_var.filter(|lv| cpp_for_body_encloses(&lines, idx, lv.paren, call_line_index, use_column)) {
                let normalized = normalize_cpp_type_name(&loop_var.ty);
                if let Some(n) = normalized.as_deref().filter(|n| *n != "auto") {
                    let in_caller_scope = is_cpp_callers_declaration(&r.file_path, idx + 1, r, ctx);
                    note_cpp_declaration(found, &loop_var.ty, in_caller_scope, 0, false);
                    return cpp_declared_type(&loop_var.ty, n, in_caller_scope, r, ctx, Some(found));
                }
                let element = if normalized.as_deref() == Some("auto") { cpp_auto_loop_element(&lines, idx, loop_var.colon, r, ctx) } else { None };
                if let Some(element) = element {
                    if let Some(element_type) = normalize_cpp_type_name(&element.text).filter(|t| t != "auto") {
                        note_cpp_declaration(found, &element.text, true, 0, false);
                        found.declared = Some(element.clone());
                        return cpp_declared_type(&element.text, &element_type, true, r, ctx, Some(found));
                    }
                }
            }
            let begun = cpp_declaration_begun_above(&lines, idx, &line, &receiver_pattern, &declarator_regex, &raw_lines);
            if let Some(begun) = begun.as_deref() {
                if let Some(begun_type) = normalize_cpp_type_name(begun).filter(|t| t != "auto") {
                    let in_caller_scope = is_cpp_callers_declaration(&r.file_path, idx + 1, r, ctx);
                    note_cpp_declaration(found, begun, in_caller_scope, 0, false);
                    return cpp_declared_type(begun, &begun_type, in_caller_scope, r, ctx, Some(found));
                }
            }
            if cpp_ends_declaration_begun_above(&line, &escaped) {
                found.shadowed = true;
            }
            let bound = if !found.shadowed {
                cpp_structured_binding_type(&line, &lines, idx, receiver_name, call_line_index, use_column, r, ctx, &raw_lines)
            } else {
                None
            };
            if let Some(bound) = bound {
                if let Some(bound_type) = normalize_cpp_type_name(&bound.text).filter(|t| t != "auto") {
                    note_cpp_declaration(found, &bound.text, true, 0, false);
                    found.declared = Some(bound.clone());
                    return cpp_declared_type(&bound.text, &bound_type, true, r, ctx, Some(found));
                }
            }
            if cpp_rebinds_receiver(&line, &escaped) {
                found.shadowed = true;
            }
        }
    }

    if !member_looked {
        if let Some(mc) = &member_caller {
            if let Some(member) = cpp_member_receiver_type(receiver_name, mc, r, ctx, found) {
                return member;
            }
        }
    }

    let swap = |ext: &str| re!(r"\.(?:c|cc|cpp|cxx)$", "i").replace_all_str(&r.file_path, ext);
    let mut header_candidates: Vec<String> = Vec::new();
    for candidate in [swap(".h"), swap(".hpp"), swap(".hxx")] {
        if candidate != r.file_path && !header_candidates.contains(&candidate) {
            header_candidates.push(candidate);
        }
    }
    for header_path in header_candidates {
        if !ctx.file_exists(&header_path) {
            continue;
        }
        let Some(header_lines) = ctx.get_file_lines(&header_path) else { continue };
        let header_raw = cpp_non_code_lines(&header_path, ctx);
        for (i, text) in header_lines.iter().enumerate() {
            if !receiver_pattern.is_match(text) {
                continue;
            }
            let Some(line) = cpp_code_of(&cpp_literals_blanked(text, i + 1, &header_raw)).filter(|l| !l.is_empty()) else { continue };
            let Some(m) = match_cpp_declarator(&line, &declarator_regex).or_else(|| match_cpp_listed_declarator(&line, receiver_name, &listed_regex)) else {
                continue;
            };
            if let Some(normalized) = normalize_cpp_type_name(&m.ty).filter(|n| n != "auto") {
                let in_caller_scope = is_cpp_callers_declaration(&header_path, i + 1, r, ctx);
                let array = cpp_array_dimensions(m.rest(&line));
                note_cpp_declaration(found, &m.ty, in_caller_scope, array, leading_const(&line[..m.index.min(line.len())]));
                return cpp_declared_type(&m.ty, &normalized, in_caller_scope, r, ctx, Some(found));
            }
        }
    }
    None
}

/// `cppMemberReceiverType`: the receiver is a data member of the caller's class. `None` is
/// "undefined" (keep looking); `Some(None)` is a decided "no type".
fn cpp_member_receiver_type(receiver_name: &str, caller: &Node, r: &UnresolvedRef, ctx: &Ctx, found: &mut CppReceiverDecl) -> Option<Option<String>> {
    if found.shadowed {
        return None;
    }
    let member = cpp_callers_member(receiver_name, caller, r, ctx)?;
    let normalized = normalize_cpp_type_name(&member.text)?;
    let followed = cpp_follow_written_aliases(&member, r, ctx);
    if is_cpp_template_dependent_type(followed.as_ref().unwrap_or(&member), ctx) || matches!(resolve_cpp_aliased_type(&member.text, r, ctx), Tri::Null) {
        return None;
    }
    note_cpp_declaration(found, &member.text, true, 0, false);
    found.declared = Some(member.clone());
    Some(cpp_declared_type(&member.text, &normalized, true, r, ctx, Some(found)))
}

/// `cppClassExists`.
fn cpp_class_exists(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let last = cpp_last_segment(name);
    ctx.get_nodes_by_name(last).iter().any(|n| matches!(n.kind, NodeKind::Class | NodeKind::Struct | NodeKind::Union) && n.language == r.language)
}

/// `resolveCppCallResultType`: the type a call expression returns.
pub fn resolve_cpp_call_result_type(inner: &str, r: &UnresolvedRef, ctx: &Ctx, depth: u32) -> Option<String> {
    if depth > 3 {
        return None;
    }
    let expr = js_trim(inner);
    if let Some(make) = re!(r"(?:^|::)(?:make_unique|make_shared)\s*<\s*([A-Za-z_]\w*)").captures(expr) {
        return make.text(1).map(String::from);
    }
    if let Some(dot) = expr.rfind('.').filter(|d| *d > 0) {
        let recv = &expr[..dot];
        let method = &expr[dot + 1..];
        if recv.contains('.') || recv.contains('(') || recv.contains("::") {
            return None;
        }
        let mut scratch = CppReceiverDecl::default();
        let recv_type = infer_cpp_receiver_type(recv, r, ctx, depth + 1, &mut scratch).filter(|t| !t.is_empty())?;
        return super::call_chain::lookup_callee_return_type(&format!("{recv_type}::{method}"), r, ctx);
    }
    if let Some(ret) = super::call_chain::lookup_callee_return_type(expr, r, ctx) {
        return Some(ret);
    }
    if cpp_class_exists(expr, r, ctx) {
        return Some(cpp_last_segment(expr).to_string());
    }
    None
}

/// `inferCppAutoInitializerType`.
fn infer_cpp_auto_initializer_type(init: &str, r: &UnresolvedRef, ctx: &Ctx, depth: u32) -> Option<String> {
    if let Some(neu) = re!(r"^new\s+([A-Za-z_][\w:]*)").captures(init) {
        if let Some(name) = neu.text(1).filter(|n| !n.is_empty()) {
            return Some(cpp_last_segment(name).to_string());
        }
    }
    if let Some(call) = re!(r"^([A-Za-z_][\w:]*(?:\s*<[^>;]*>)?)\s*\(").captures(init) {
        if let Some(callee) = call.text(1).filter(|c| !c.is_empty()) {
            return resolve_cpp_call_result_type(&re!(r"\s+", "g").replace_all_str(callee, ""), r, ctx, depth + 1);
        }
    }
    None
}

/// `cppAutoInitializer`.
fn cpp_auto_initializer(line: &str, receiver_name: &str) -> Option<String> {
    let m = Re::cached(&format!(r"\b{}\b\s*=\s*([^;]+)", esc(receiver_name)), "").captures(line)?;
    let text = js_trim(m.text(1)?);
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// `cppAutoInitializerStatement`.
fn cpp_auto_initializer_statement(line: &str, lines: &[String], index: usize, receiver_name: &str, raw_lines: &NonCodeLines) -> Option<String> {
    let start = Re::cached(&format!(r"\b{}\b\s*=(?!=)", esc(receiver_name)), "").find(line)?;
    cpp_statement_from(&line[start.end()..], lines, index, raw_lines)
}

/// `cppStatementFrom`: the statement text from `text` up to its `;`, continuing on later lines.
pub fn cpp_statement_from(text: &str, lines: &[String], index: usize, raw_lines: &NonCodeLines) -> Option<String> {
    let spliced = |code: &str| re!(r"\\\s*$").replace_all_str(code, " ");
    let mut text = spliced(text);
    let mut next = index + 1;
    loop {
        if let Some(end) = cpp_statement_end(&text) {
            let t = js_trim(&text[..end]);
            return if t.is_empty() { None } else { Some(t.to_string()) };
        }
        if next >= lines.len() || next > index + 4 {
            return None;
        }
        let more = cpp_code_of(&cpp_literals_blanked(&lines[next], next + 1, raw_lines)).unwrap_or_default();
        text = format!("{} {}", text, spliced(&more));
        next += 1;
    }
}

/// `cppStructuredBindingType`: the element type a structured binding `auto [a, b]` gives the receiver.
#[allow(clippy::too_many_arguments)]
fn cpp_structured_binding_type(
    line: &str,
    lines: &[String],
    index: usize,
    receiver_name: &str,
    call_line_index: usize,
    use_column: usize,
    r: &UnresolvedRef,
    ctx: &Ctx,
    raw_lines: &NonCodeLines,
) -> Option<CppWrittenType> {
    if !line.contains('[') {
        return None;
    }
    for m in re!(r"\bauto\s*&{0,2}\s*\[([^\]]*)\]", "g").captures_iter(line) {
        let whole = m.get(0)?;
        let place = m.text(1)?.split(',').map(|n| js_trim(n)).position(|n| n == receiver_name);
        let Some(place) = place else { continue };
        let end = whole.end();
        let loop_head = re!(r"\bfor\s*\(\s*(?:(?:const|volatile)\s+)*$").find(&line[..whole.start()]);
        let colon = re!(r"^\s*:(?!:)").find(&line[end..]);
        let value: Option<CppWrittenType>;
        if let (Some(loop_head), Some(colon)) = (loop_head, colon) {
            let paren = loop_head.start() + line[loop_head.start()..].find('(')?;
            if !cpp_for_body_encloses(lines, index, paren, call_line_index, use_column) {
                return None;
            }
            value = cpp_auto_loop_element(lines, index, end + colon.end(), r, ctx);
        } else {
            let assign = re!(r"^\s*=(?!=)").find(&line[end..]);
            let Some(assign) = assign else { return None };
            if !is_cpp_callers_declaration(&r.file_path, index + 1, r, ctx) {
                return None;
            }
            let init = cpp_statement_from(&line[end + assign.end()..], lines, index, raw_lines);
            value = init.and_then(|init| cpp_auto_local_type(&init, index + 1, whole.as_str(), r, ctx));
        }
        return value.and_then(|v| cpp_binding_elements(&v, r, ctx)).and_then(|e| e.into_iter().nth(place));
    }
    None
}
