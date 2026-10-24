//! Which C/C++ declarations a translation unit can see — port of `isInCppTranslationUnit`,
//! `withinCppTranslationUnit`, `isCppTranslationUnitLocal`, `cppVisibleFirst` and
//! `isStaticCFunction`.

use super::cpp_overload::cpp_argument_count_fits;
use super::cpp_scope::{cpp_implicit_receiver, Receiver};
use super::cpp_types::{cpp_classes_in, is_c_family_node, is_cpp_class_kind, CPP_TYPE_KINDS};
use super::cpp_text::cpp_call_arguments;
use super::util::{memo_map, MemoMap};
use super::visibility::is_test_suite_path;
use crate::c_feature_variants::{is_c_feature_variant, is_off_by_default};
use crate::cpp_includers::{cpp_included_file, cpp_included_headers_beside, cpp_includers};
use crate::cpp_type_aliases::cpp_parent_scope;
use crate::ctx::Ctx;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::{EdgeKind, Language, Node, NodeKind, ReferenceKind};
use std::rc::Rc;

/// `C_SOURCE_EXT`.
pub fn c_source_ext(path: &str) -> bool {
    re!(r"\.(c|cc|cpp|cxx|c\+\+|m|mm)$", "i").is_match(path)
}

fn is_c_or_cpp(language: Language) -> bool {
    matches!(language, Language::C | Language::Cpp)
}

fn file_stem(path: &str) -> &str {
    let name = &path[path.rfind('/').map(|i| i + 1).unwrap_or(0)..];
    match name.rfind('.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    }
}

memo_map!(CppStemHeaders, String, Rc<Vec<String>>);
memo_map!(TestSuiteFiles, String, bool);
memo_map!(CppTypeDecls, String, Rc<Vec<NodeRef>>);
memo_map!(CppIncludeLines, String, Rc<Vec<(u32, Option<String>)>>);
memo_map!(CppUnnamedNamespaces, String, Rc<Vec<(u32, u32)>>);
memo_map!(CStaticMemo, String, bool);

/// `cppSameStemHeaders`: the header(s) a source file `#include`s that share its file stem.
fn cpp_same_stem_headers(file: &str, ctx: &Ctx) -> Rc<Vec<String>> {
    let memo = ctx.memo::<CppStemHeaders>();
    if let Some(hit) = memo.lookup(&file.to_string()) {
        return hit;
    }
    let mut found: Vec<String> = Vec::new();
    let stem = file_stem(file);
    for include in ctx.get_nodes_in_file(file).iter() {
        if include.kind != NodeKind::Import || file_stem(&include.name) != stem {
            continue;
        }
        if let Some(header) = cpp_included_file(include, ctx) {
            if header != file && file_stem(&header) == stem {
                found.push(header);
            }
        }
    }
    let dir = &file[..file.rfind('/').map(|i| i + 1).unwrap_or(0)];
    let mut beside: Vec<String> =
        found.iter().filter(|h| h[..h.rfind('/').map(|i| i + 1).unwrap_or(0)] == *dir).cloned().collect();
    if beside.is_empty() {
        beside = cpp_included_headers_beside(file, ctx);
    }
    let headers = Rc::new(if beside.is_empty() { found } else { beside });
    memo.store(file.to_string(), Rc::clone(&headers));
    headers
}

/// `isTestSuiteFile`.
fn is_test_suite_file(file: &str, ctx: &Ctx) -> bool {
    let memo = ctx.memo::<TestSuiteFiles>();
    if let Some(hit) = memo.lookup(&file.to_string()) {
        return hit;
    }
    let hit = is_test_suite_path(file);
    memo.store(file.to_string(), hit);
    hit
}

/// `isIncludedCppTestSuite`: the candidate's test-suite file is `#include`d (directly, or via its
/// header) by the file the reference is in.
pub fn is_included_cpp_test_suite(candidate: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if !is_c_or_cpp(candidate.language) || !is_c_or_cpp(r.language) {
        return false;
    }
    if cpp_includers(&candidate.file_path, ctx).contains(&r.file_path) {
        return true;
    }
    cpp_same_stem_headers(&candidate.file_path, ctx).iter().any(|header| is_test_suite_file(header, ctx) && cpp_includers(header, ctx).contains(&r.file_path))
}

/// `isStaticCFunction`: declared `static` (file-local linkage).
pub fn is_static_c_function(candidate: &Node, ctx: &Ctx) -> bool {
    let memo = ctx.memo::<CStaticMemo>();
    if let Some(hit) = memo.lookup(&candidate.id) {
        return hit;
    }
    let lines = ctx.get_file_lines(&candidate.file_path).unwrap_or_default();
    let at = |n: i64| -> &str { if n >= 0 { lines.get(n as usize).map(String::as_str).unwrap_or("") } else { "" } };
    let head = format!("{}\n{}", at(candidate.start_line as i64 - 2), at(candidate.start_line as i64 - 1));
    let is_static = re!(r"(^|[\s;}])static\s").is_match(&head);
    memo.store(candidate.id.clone(), is_static);
    is_static
}

/// `isInCppTranslationUnit`.
fn is_in_cpp_translation_unit(candidate: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if candidate.file_path == r.file_path {
        return true;
    }
    if !is_c_or_cpp(candidate.language) {
        return false;
    }
    if cpp_includers(&candidate.file_path, ctx).contains(&r.file_path) {
        return true;
    }
    if !c_source_ext(&candidate.file_path)
        || is_cpp_translation_unit_local(candidate, r, ctx)
        || (matches!(candidate.kind, NodeKind::Function | NodeKind::Variable | NodeKind::Constant) && is_static_c_function(candidate, ctx))
    {
        return false;
    }
    let suite = is_test_suite_file(&candidate.file_path, ctx);
    cpp_same_stem_headers(&candidate.file_path, ctx)
        .iter()
        .any(|header| (!suite || is_test_suite_file(header, ctx)) && (*header == r.file_path || cpp_includers(header, ctx).contains(&r.file_path)))
}

/// `cppTypeDecls`.
fn cpp_type_decls(qualified_name: &str, ctx: &Ctx) -> Rc<Vec<NodeRef>> {
    if qualified_name.is_empty() {
        return Rc::new(Vec::new());
    }
    let memo = ctx.memo::<CppTypeDecls>();
    if let Some(hit) = memo.lookup(&qualified_name.to_string()) {
        return hit;
    }
    let decls: Vec<NodeRef> =
        ctx.get_nodes_by_qualified_name(qualified_name).iter().filter(|n| CPP_TYPE_KINDS.contains(&n.kind) && is_c_family_node(n)).cloned().collect();
    let decls = Rc::new(decls);
    memo.store(qualified_name.to_string(), Rc::clone(&decls));
    decls
}

/// `isCppNameAloneTarget`: for a name-only reference, may this candidate be reached by its bare name?
fn is_cpp_name_alone_target(n: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let constructor = n.kind == NodeKind::Method && n.qualified_name.ends_with(&format!("{}::{}", n.name, n.name));
    let is_type = CPP_TYPE_KINDS.contains(&n.kind);
    let fits = if r.reference_kind == ReferenceKind::Edge(EdgeKind::Calls) {
        n.kind == NodeKind::Function || constructor || is_type
    } else if r.reference_kind == ReferenceKind::Edge(EdgeKind::Instantiates) {
        constructor || is_type
    } else {
        constructor || !matches!(n.kind, NodeKind::Method | NodeKind::Field | NodeKind::Property)
    };
    if !fits {
        return false;
    }
    let scope = cpp_parent_scope(&if constructor { cpp_parent_scope(&n.qualified_name) } else { n.qualified_name.clone() });
    !cpp_type_decls(&scope, ctx).iter().any(|d| is_cpp_class_kind(d.kind))
}

/// `withinCppTranslationUnit`: keep the candidates the reference's translation unit sees.
pub fn within_cpp_translation_unit(candidates: Vec<NodeRef>, r: &UnresolvedRef, ctx: &Ctx, by_name: bool) -> Vec<NodeRef> {
    if candidates.len() < 2 || !is_c_or_cpp(r.language) || r.is_kind(EdgeKind::Imports) {
        return candidates;
    }
    let calls = r.is_kind(EdgeKind::Calls);
    if by_name && calls && cpp_implicit_receiver(r, ctx) != Some(Receiver::Bare) {
        return candidates;
    }
    if calls && ctx.node_by_id(&r.from_node_id).is_some_and(|n| n.kind == NodeKind::File) {
        return candidates;
    }
    let within: Vec<NodeRef> = candidates
        .iter()
        .filter(|n| is_in_cpp_translation_unit(n, r, ctx) && (!by_name || n.file_path == r.file_path || is_cpp_name_alone_target(n, r, ctx)))
        .cloned()
        .collect();
    if within.is_empty() || within.len() == candidates.len() {
        return candidates;
    }
    let in_within = |n: &NodeRef| within.iter().any(|w| w.id == n.id);
    if r.language == Language::C
        && calls
        && within.iter().all(|n| is_c_feature_variant(n, &r.file_path, r.line, ctx))
        && !is_off_by_default(&r.file_path, r.line as usize, ctx)
        && candidates.iter().any(|n| {
            !in_within(n) && n.kind == NodeKind::Function && !is_static_c_function(n, ctx) && !is_c_feature_variant(n, &r.file_path, r.line, ctx)
        })
    {
        return candidates;
    }
    if calls {
        if let Some(args) = cpp_call_arguments(r, &within[0].name, ctx) {
            if within.iter().all(|n| cpp_argument_count_fits(n, &n.name, &args, ctx) == Some(false))
                && candidates.iter().any(|n| !in_within(n) && cpp_argument_count_fits(n, &n.name, &args, ctx) == Some(true))
            {
                return candidates;
            }
        }
    }
    within
}

/// `cppEnclosingClassDecl`: the innermost class a member's declaration sits in.
fn cpp_enclosing_class_decl(member: &Node, ctx: &Ctx) -> Option<NodeRef> {
    let mut innermost: Option<NodeRef> = None;
    for cls in cpp_classes_in(&member.file_path, ctx).iter() {
        if cls.id == member.id || member.start_line < cls.start_line || member.start_line > cls.end_line.max(cls.start_line) {
            continue;
        }
        if innermost.as_ref().is_none_or(|i| cls.start_line >= i.start_line) {
            innermost = Some(Rc::clone(cls));
        }
    }
    innermost
}

/// `cppIncludesAfter`: does `source` include `header` after line `line`?
fn cpp_includes_after(source: &str, line: u32, header: &str, ctx: &Ctx) -> bool {
    let memo = ctx.memo::<CppIncludeLines>();
    let includes = match memo.lookup(&source.to_string()) {
        Some(i) => i,
        None => {
            let list: Vec<(u32, Option<String>)> = ctx
                .get_nodes_in_file(source)
                .iter()
                .filter(|n| n.kind == NodeKind::Import)
                .map(|n| (n.start_line, cpp_included_file(n, ctx)))
                .collect();
            let list = Rc::new(list);
            memo.store(source.to_string(), Rc::clone(&list));
            list
        }
    };
    includes.iter().any(|(at, file)| *at > line && file.as_deref() == Some(header))
}

/// `cppUnnamedNamespaceSpans`: the line spans of outermost unnamed namespaces in a file.
fn cpp_unnamed_namespace_spans(file: &str, ctx: &Ctx) -> Rc<Vec<(u32, u32)>> {
    let memo = ctx.memo::<CppUnnamedNamespaces>();
    if let Some(hit) = memo.lookup(&file.to_string()) {
        return hit;
    }
    let mut spans: Vec<(u32, u32)> = Vec::new();
    let source = ctx.read_file(file).map(|s| s.to_string()).unwrap_or_default();
    if re!(r"\bnamespace\s*\{").is_match(&source) {
        let text = source.as_bytes();
        let n = text.len();
        let mut open: Vec<(u32, bool)> = Vec::new();
        let mut line = 1u32;
        let mut line_start = true;
        let mut code: Vec<u8> = Vec::new();
        let mut i = 0usize;
        while i < n {
            let c = text[i];
            if c == b'\n' {
                line += 1;
                line_start = true;
                code.push(b' ');
                i += 1;
                continue;
            }
            if line_start && c == b'#' {
                let mut end = i;
                while end < n && (text[end] != b'\n' || text[end - 1] == b'\\' || (text[end - 1] == b'\r' && end >= 2 && text[end - 2] == b'\\')) {
                    if text[end] == b'\n' {
                        line += 1;
                    }
                    end += 1;
                }
                i = end;
                continue;
            }
            if !matches!(c, b' ' | b'\t' | b'\r' | 0x0b | 0x0c) {
                line_start = false;
            }
            if c == b'/' && text.get(i + 1) == Some(&b'/') {
                i = text[i..].iter().position(|&b| b == b'\n').map(|p| i + p).unwrap_or(n);
                continue;
            }
            if c == b'/' && text.get(i + 1) == Some(&b'*') {
                let stop = find_bytes(text, b"*/", i + 2).map(|e| e + 2).unwrap_or(n);
                line += text[i..stop].iter().filter(|&&b| b == b'\n').count() as u32;
                i = stop;
                continue;
            }
            if c == b'"' && {
                let from = i.saturating_sub(4);
                let window = String::from_utf8_lossy(&text[from..i]).into_owned();
                re!(r"(?:^|[^\w$])(?:u8|[uUL])?R$").is_match(&window)
            } {
                let paren = text[i..].iter().position(|&b| b == b'(').map(|p| i + p);
                let end = paren.and_then(|p| {
                    let mut needle = vec![b')'];
                    needle.extend_from_slice(&text[i + 1..p]);
                    needle.push(b'"');
                    find_bytes(text, &needle, p)
                });
                let stop = match (paren, end) {
                    (Some(p), Some(e)) => e + p - i + 1,
                    _ => n,
                };
                let upto = stop.min(n);
                line += text[i..upto].iter().filter(|&&b| b == b'\n').count() as u32;
                i = upto;
                continue;
            }
            if c == b'"' || c == b'\'' {
                let mut j = i + 1;
                while j < n && text[j] != c && text[j] != b'\n' {
                    j += if text[j] == b'\\' { 2 } else { 1 };
                }
                i = j + 1;
                continue;
            }
            if c == b'{' {
                let tail = String::from_utf8_lossy(&code).into_owned();
                open.push((line, re!(r"(?:^|[^\w$])namespace\s*$").is_match(&tail)));
                code.clear();
            } else if c == b'}' {
                if let Some((start, unnamed)) = open.pop() {
                    if unnamed && !open.iter().any(|o| o.1) {
                        spans.push((start, line));
                    }
                }
                code.clear();
            } else if c == b';' {
                code.clear();
            } else {
                code.push(c);
                if code.len() > 200 {
                    let cut = code.len() - 100;
                    code.drain(..cut);
                }
            }
            i += 1;
        }
    }
    let spans = Rc::new(spans);
    memo.store(file.to_string(), Rc::clone(&spans));
    spans
}

fn find_bytes(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > hay.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|p| from + p)
}

/// `isCppTranslationUnitLocal`: the candidate has internal linkage (or is declared after the
/// reference's translation unit stopped looking) so the reference's file cannot see it.
pub fn is_cpp_translation_unit_local(candidate: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if !is_c_or_cpp(r.language) {
        return false;
    }
    let seen_from = |decl: &Node, file: &str| -> bool {
        !c_source_ext(&decl.file_path) || decl.file_path == file || cpp_includers(&decl.file_path, ctx).contains(file) || cpp_includes_after(&decl.file_path, decl.start_line, file, ctx)
    };
    let seen = |decl: &Node| seen_from(decl, &r.file_path);
    if CPP_TYPE_KINDS.contains(&candidate.kind) {
        return !seen(candidate);
    }
    if let Some(enclosing) = cpp_enclosing_class_decl(candidate, ctx) {
        return !seen(&enclosing);
    }
    let decls: Vec<NodeRef> =
        cpp_type_decls(&cpp_parent_scope(&candidate.qualified_name), ctx).iter().filter(|d| seen_from(d, &candidate.file_path)).cloned().collect();
    if decls.iter().any(|d| d.kind == NodeKind::TypeAlias) {
        return false;
    }
    if !decls.is_empty() {
        return !decls.iter().any(|d| seen(d));
    }
    !seen(candidate) && cpp_unnamed_namespace_spans(&candidate.file_path, ctx).iter().any(|&(start, end)| candidate.start_line >= start && candidate.start_line <= end)
}

/// `cppVisibleFirst`: of several methods, those the reference's translation unit can see.
pub fn cpp_visible_first(methods: Vec<NodeRef>, r: &UnresolvedRef, ctx: &Ctx) -> Vec<NodeRef> {
    if methods.len() < 2 || !is_c_or_cpp(r.language) {
        return methods;
    }
    let visible: Vec<NodeRef> = methods.iter().filter(|m| !is_cpp_translation_unit_local(m, r, ctx)).cloned().collect();
    if visible.is_empty() {
        methods
    } else {
        visible
    }
}
