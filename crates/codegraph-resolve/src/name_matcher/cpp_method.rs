//! Looking a method up on a C++ class — port of `cppMethodOf` and the class-shape helpers it
//! leans on (`cppBaseSpecifiers`, `cppUsingDeclares`, `isCppDependentBase`, `cppBasesKnown`).

use super::cpp_decl::{cpp_code_of, esc};
use super::cpp_overload::{cpp_overload_for, cpp_qualifier_specialization_pool, cpp_specialization_pool};
use super::cpp_scope::cpp_class_decls;
use super::cpp_tu::cpp_visible_first;
use super::cpp_types::{cpp_last_segment, is_c_family_node, is_cpp_class_kind, is_cpp_library_type};
use super::cpp_text::split_cpp_top_level;
use super::exact::prefer_call_site_file;
use super::util::{col_byte, memo_map, MemoMap};
use crate::cpp_includers::cpp_includers;
use crate::cpp_raw_strings::{cpp_literals_blanked, cpp_non_code_lines};
use crate::cpp_type_aliases::{cpp_template_parameters, strip_cpp_template_arguments};
use crate::ctx::Ctx;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_types::jscompat::js_trim;
use codegraph_types::Node;
use std::collections::HashSet;
use std::rc::Rc;

memo_map!(CppMemberMacroArgs, String, Rc<Vec<String>>);
memo_map!(CppBaseSpecs, String, Rc<Vec<String>>);
memo_map!(CppUsingMembers, String, Rc<Vec<(String, String)>>);

/// `cppMethodOf`.
pub fn cpp_method_of(cls: &Node, name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    let mut seen = HashSet::new();
    cpp_method_in(cls, name, r, ctx, 0, &mut seen)
}

/// `cppMethodOf` with its recursion state.
pub fn cpp_method_in(cls: &Node, name: &str, r: &UnresolvedRef, ctx: &Ctx, depth: u32, seen: &mut HashSet<String>) -> Option<NodeRef> {
    let want = format!("{}::{}", cls.qualified_name, name);
    let named = ctx.get_method_matches(&cls.name, name, r.language);
    let family: Vec<NodeRef> = named.iter().filter(|n| n.qualified_name == want).cloned().collect();
    let own = cpp_qualifier_specialization_pool(cpp_specialization_pool(family.clone(), r, ctx), r, ctx);
    if own.is_empty() && !family.is_empty() {
        return None;
    }
    if !own.is_empty() {
        let ordered = prefer_call_site_file(cpp_visible_first(own, r, ctx), &r.file_path);
        if r.reference_name.contains("().") {
            return ordered.first().cloned();
        }
        let namesakes = cpp_class_decls(&cls.qualified_name, ctx).len() > 1;
        let in_body: Vec<NodeRef> = if namesakes {
            ordered.iter().filter(|n| n.file_path == cls.file_path && n.start_line >= cls.start_line && n.end_line <= cls.end_line).cloned().collect()
        } else {
            Vec::new()
        };
        let defined: Vec<NodeRef> = if !in_body.is_empty() {
            in_body
        } else if namesakes {
            ordered.iter().filter(|n| n.file_path == cls.file_path || cpp_includers(&cls.file_path, ctx).contains(&n.file_path)).cloned().collect()
        } else {
            ordered.clone()
        };
        return Some(cpp_overload_for(if defined.is_empty() { ordered } else { defined }, name, r, ctx));
    }
    seen.insert(cls.qualified_name.clone());
    if depth >= 4 {
        return None;
    }
    for decl in ctx.get_nodes_by_qualified_name(&cls.qualified_name).iter() {
        if !is_c_family_node(decl) || !is_cpp_class_kind(decl.kind) {
            continue;
        }
        for base in ctx.get_supertype_nodes(&decl.id).iter() {
            if !is_cpp_class_kind(base.kind) || seen.contains(&base.qualified_name) {
                continue;
            }
            if let Some(inherited) = cpp_method_in(base, name, r, ctx, depth + 1, seen) {
                return Some(inherited);
            }
        }
    }
    None
}

/// `cppLacksMethod`: the class has no template parameters and every base is known.
pub fn cpp_lacks_method(cls: &Node, ctx: &Ctx) -> bool {
    cpp_template_parameters(cls, ctx).is_empty() && cpp_bases_known(cls, ctx, 0, &mut HashSet::new())
}

/// `cppBasesKnown`.
pub fn cpp_bases_known(cls: &Node, ctx: &Ctx, depth: u32, seen: &mut HashSet<String>) -> bool {
    if seen.contains(&cls.qualified_name) {
        return true;
    }
    seen.insert(cls.qualified_name.clone());
    for decl in ctx.get_nodes_by_qualified_name(&cls.qualified_name).iter() {
        if !is_c_family_node(decl) || !is_cpp_class_kind(decl.kind) {
            continue;
        }
        let bases = ctx.get_supertype_nodes(&decl.id);
        let written: Vec<String> = cpp_base_specifiers(decl, ctx).iter().filter(|s| !is_cpp_library_type(s, ctx)).cloned().collect();
        if depth >= 4 {
            if bases.len() + written.len() > 0 {
                return false;
            }
        } else if written.len() > bases.len() {
            return false;
        }
        for base in bases.iter() {
            if !is_cpp_class_kind(base.kind) || !cpp_bases_known(base, ctx, depth + 1, seen) {
                return false;
            }
        }
    }
    true
}

/// `cppMemberMacroArguments`: the identifiers a macro invocation inside a class body mentions
/// (`DECLARE_FOO(bar)` declares members the index cannot see).
pub fn cpp_member_macro_arguments(decl: &Node, ctx: &Ctx) -> Rc<Vec<String>> {
    let memo = ctx.memo::<CppMemberMacroArgs>();
    if let Some(hit) = memo.lookup(&decl.id) {
        return hit;
    }
    let mut words: Vec<String> = Vec::new();
    let lines = ctx.get_file_lines(&decl.file_path).unwrap_or_default();
    let non_code = cpp_non_code_lines(&decl.file_path, ctx);
    let mut depth = 0i64;
    let first = (decl.start_line as usize).saturating_sub(1);
    let last = lines.len().min(decl.end_line as usize);
    for i in first..last {
        let blanked = cpp_literals_blanked(&lines[i], i + 1, &non_code);
        let code = if i == first { cpp_code_of(&blanked[col_byte(&blanked, decl.start_column)..]) } else { cpp_code_of(&blanked) }.unwrap_or_default();
        if depth == 1 {
            if let Some(call) = re!(r"^\s*[A-Z][A-Z0-9_]*\s*\(([^;{}]*)\)\s*;?\s*$").captures(&code) {
                words.extend(re!(r"\W+").split(call.text(1).unwrap_or("")).into_iter().filter(|w| w.chars().count() >= 3).map(String::from));
            }
        }
        for c in code.chars() {
            if c == '{' {
                depth += 1;
            } else if c == '}' {
                depth -= 1;
            }
        }
    }
    let words = Rc::new(words);
    memo.store(decl.id.clone(), Rc::clone(&words));
    words
}

/// `cppBaseSpecifiers`: the base-class specifiers written in a class head.
pub fn cpp_base_specifiers(decl: &Node, ctx: &Ctx) -> Rc<Vec<String>> {
    let memo = ctx.memo::<CppBaseSpecs>();
    if let Some(hit) = memo.lookup(&decl.id) {
        return hit;
    }
    let mut specs: Vec<String> = Vec::new();
    let lines = ctx.get_file_lines(&decl.file_path);
    let non_code = cpp_non_code_lines(&decl.file_path, ctx);
    let first = (decl.start_line as usize).saturating_sub(1);
    let mut head: Vec<String> = match &lines {
        Some(l) => l
            .iter()
            .enumerate()
            .skip(first)
            .take(12)
            .map(|(k, line)| cpp_literals_blanked(line, k + 1, &non_code))
            .collect(),
        None => Vec::new(),
    };
    if !head.is_empty() {
        head[0] = head[0][col_byte(&head[0], decl.start_column)..].to_string();
        let text = head.iter().map(|l| cpp_code_of(l).unwrap_or_default()).collect::<Vec<_>>().join("\n");
        if let Some(brace) = text.find('{') {
            let before = &text[..brace];
            let colon = re!(r"(?<!:):(?!:)").find(before).map(|m| m.start());
            if let Some(colon) = colon {
                if !before.contains(';') {
                    for s in split_cpp_top_level(&text[colon + 1..brace]) {
                        let cleaned = re!(r"\b(?:public|protected|private|virtual)\b", "g").replace_all_str(&s, " ");
                        specs.push(js_trim(&cleaned).to_string());
                    }
                }
            }
        }
    }
    let specs = Rc::new(specs);
    memo.store(decl.id.clone(), Rc::clone(&specs));
    specs
}

/// `cppUsingDeclares`: the class re-exports `name` from `base` with a using-declaration.
pub fn cpp_using_declares(decl: &Node, base: &Node, name: &str, ctx: &Ctx) -> bool {
    let memo = ctx.memo::<CppUsingMembers>();
    let usings = match memo.lookup(&decl.id) {
        Some(u) => u,
        None => {
            let lines = ctx.get_file_lines(&decl.file_path).unwrap_or_default();
            let non_code = cpp_non_code_lines(&decl.file_path, ctx);
            let first = (decl.start_line as usize).saturating_sub(1).min(lines.len());
            let last = (decl.end_line.max(decl.start_line) as usize).min(lines.len());
            let body: String = (first..last)
                .map(|i| cpp_code_of(&cpp_literals_blanked(&lines[i], i + 1, &non_code)).unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n");
            let mut list: Vec<(String, String)> = Vec::new();
            for m in re!(r"\busing\s+(?!namespace\b)((?:::\s*)?[A-Za-z_][\w:]*(?:\s*<[^;{}]*>)?)\s*::\s*([A-Za-z_]\w*)\s*;", "g").captures_iter(&body) {
                let of = cpp_last_segment(js_trim(&strip_cpp_template_arguments(m.text(1).unwrap_or("")))).to_string();
                list.push((of, m.text(2).unwrap_or("").to_string()));
            }
            let list = Rc::new(list);
            memo.store(decl.id.clone(), Rc::clone(&list));
            list
        }
    };
    usings.iter().any(|(of, member)| *of == base.name && member == name)
}

/// `isCppDependentBase`: the base specifier mentions a template parameter of the derived class.
pub fn is_cpp_dependent_base(decl: &Node, base: &Node, ctx: &Ctx) -> bool {
    let parameters = cpp_template_parameters(decl, ctx);
    if parameters.is_empty() {
        return false;
    }
    let specs = cpp_base_specifiers(decl, ctx);
    let Some(spec) = specs.iter().find(|s| cpp_last_segment(js_trim(&strip_cpp_template_arguments(s))) == base.name) else { return false };
    parameters.iter().any(|p| Re::cached(&format!(r"(?<![\w$]){}(?![\w$])", esc(p)), "").is_match(spec))
}
