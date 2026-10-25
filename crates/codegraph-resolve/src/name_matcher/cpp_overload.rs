//! C++ overload and specialization selection — port of `cppOverloadFor`, `cppFunctionOverload`,
//! `cppSpecializationPool` and the argument-type scoring behind them, plus
//! `matchCppMacroNamespaced`.

use super::cpp_decl::{esc, is_word_byte, normalize_cpp_type_name};
use super::cpp_scope::cpp_class_decls;
use super::cpp_recv::{infer_cpp_receiver_type, CppReceiverDecl};
use super::cpp_text::{cpp_paren_list_after, cpp_paren_list_in, cpp_without_comments, split_cpp_top_level};
use super::cpp_tu::is_static_c_function;
use super::cpp_types::{cpp_expression_type, cpp_last_segment, cpp_macro_method_head, cpp_opening_paren, cpp_own_template_parameters};
use super::util::{find_best_match, memo_map, range_within, MemoMap};
use super::visibility::is_visible_across_files;
use crate::c_feature_variants::{is_c_feature_variant, is_off_by_default};
use crate::cpp_includers::cpp_includers;
use crate::cpp_namespaces::{cpp_macro_namespace_frames, cpp_namespace_aliases};
use crate::cpp_type_aliases::{cpp_parent_scope, cpp_template_parameters, strip_cpp_template_arguments};
use crate::ctx::Ctx;
use crate::types::{NodeRef, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_search::is_test_path;
use codegraph_types::jscompat::{js_trim, utf16_byte_index};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::LazyLock;

// -- Argument types --------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Category {
    String,
    CString,
    Char,
    Number,
    Bool,
    Pointer,
    Null,
    Class,
}

#[derive(Clone, Debug)]
enum ArgType {
    Char,
    Number,
    Zero,
    Bool,
    Null,
    Type { text: String, lvalue: Option<bool>, dependent: bool },
}

static CHAR_TYPES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| ["char", "wchar_t", "char8_t", "char16_t", "char32_t"].into_iter().collect());
static NUMBER_TYPES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "int", "unsigned", "signed", "short", "long", "float", "double", "size_t", "ssize_t", "ptrdiff_t", "int8_t", "int16_t", "int32_t",
        "int64_t", "uint8_t", "uint16_t", "uint32_t", "uint64_t", "intptr_t", "uintptr_t", "size_type", "difference_type",
    ]
    .into_iter()
    .collect()
});
static STRING_TYPES: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| ["string", "wstring", "string_view", "wstring_view", "basic_string", "basic_string_view"].into_iter().collect());

/// `cppTypeCategory`.
pub fn cpp_type_category(ty: &str) -> Category {
    let stripped = strip_cpp_template_arguments(ty);
    let flat = re!(r"\b(?:const|volatile|struct|class|typename|enum)\b", "g").replace_all_str(&stripped, " ");
    let flat = js_trim(&re!(r"&+", "g").replace_all_str(&flat, " ")).to_string();
    let stars = flat.replace('*', " ");
    let words: Vec<&str> = {
        let t = js_trim(&stars);
        if t.is_empty() { vec![""] } else { re!(r"\s+").split(t) }
    };
    let last = cpp_last_segment(words.last().copied().unwrap_or(""));
    if flat.contains('*') {
        return if CHAR_TYPES.contains(last) && flat.matches('*').count() == 1 { Category::CString } else { Category::Pointer };
    }
    if last == "nullptr_t" {
        return Category::Null;
    }
    if last == "bool" {
        return Category::Bool;
    }
    if CHAR_TYPES.contains(last) && words.iter().all(|w| CHAR_TYPES.contains(w) || *w == "signed" || *w == "unsigned") {
        return Category::Char;
    }
    if NUMBER_TYPES.contains(last) || words.iter().all(|w| NUMBER_TYPES.contains(w)) {
        return Category::Number;
    }
    if STRING_TYPES.contains(last) {
        Category::String
    } else {
        Category::Class
    }
}

fn literal_fit(kind: &ArgType, category: Category) -> i32 {
    use Category::*;
    match kind {
        ArgType::Char => match category {
            Char => 3,
            Number => 1,
            Bool => 0,
            String | CString | Pointer | Null => -3,
            Class => 0,
        },
        ArgType::Number => match category {
            Number => 2,
            Char => 1,
            Bool => 0,
            String | CString | Pointer | Null => -3,
            Class => 0,
        },
        ArgType::Zero => match category {
            Number => 2,
            Char => 1,
            Bool => 0,
            Pointer | CString | Null => 1,
            String => -3,
            Class => 0,
        },
        ArgType::Bool => match category {
            Bool => 3,
            Number => 1,
            Char => 0,
            String | CString | Pointer | Null => -3,
            Class => 0,
        },
        ArgType::Null => match category {
            Null => 4,
            Pointer | CString => 3,
            Class => 1,
            String | Char | Number => -3,
            Bool => -1,
        },
        ArgType::Type { .. } => 0,
    }
}

/// `cppParameterType`: a parameter declaration without its default value and name.
pub fn cpp_parameter_type(param: &str) -> String {
    let mut text = param.to_string();
    let mut depth = 0i64;
    for (i, c) in param.bytes().enumerate() {
        if matches!(c, b'<' | b'(' | b'[') {
            depth += 1;
        } else if matches!(c, b'>' | b')' | b']') {
            depth -= 1;
        } else if c == b'=' && depth == 0 {
            text = param[..i].to_string();
            break;
        }
    }
    let text = js_trim(&text).to_string();
    if let Some(named) = re!(r"^(.*[\w>*&\s])\b[A-Za-z_]\w*\s*(?:\[[^\]]*\])?$").captures(&text) {
        let head = named.text(1).unwrap_or("");
        if re!(r"[A-Za-z_]").is_match(head) && !re!(r"^(?:const|volatile|unsigned|signed|struct|class|enum)\s*$").is_match(js_trim(head)) {
            return js_trim(head).to_string();
        }
    }
    text
}

/// `cppRefersToConst`.
fn cpp_refers_to_const(ty: &str) -> Option<bool> {
    let stripped = strip_cpp_template_arguments(ty);
    let text = js_trim(&stripped);
    let stars = text.matches('*').count();
    if stars > 1 || (stars == 0 && !re!(r"[^&]&\s*$").is_match(text)) {
        return None;
    }
    let scope = if stars == 1 { &text[..text.find('*').unwrap_or(text.len())] } else { text };
    Some(re!(r"\bconst\b").is_match(scope))
}

fn cpp_is_const_object(ty: &str) -> bool {
    re!(r"\bconst\b").is_match(&strip_cpp_template_arguments(ty))
}

/// `cppArgumentConst`.
fn cpp_argument_const(arg: &str, param: &str) -> Option<bool> {
    let arg_pointer = strip_cpp_template_arguments(arg).contains('*');
    if strip_cpp_template_arguments(param).contains('*') {
        return if arg_pointer { cpp_refers_to_const(arg) } else { None };
    }
    if arg_pointer {
        None
    } else {
        Some(cpp_is_const_object(arg))
    }
}

/// `cppUnqualifiedTypeFit`.
fn cpp_unqualified_type_fit(arg: &str, param: &str, category: Category) -> i32 {
    let arg_category = cpp_type_category(arg);
    let pointer = |c: Category| c == Category::Pointer || c == Category::CString;
    let _ = param;
    if normalize_cpp_type_name(arg) == normalize_cpp_type_name(param) && pointer(arg_category) == pointer(category) {
        return 3;
    }
    if arg_category == category && arg_category != Category::Class {
        return 2;
    }
    let textual = |c: Category| c == Category::String || c == Category::CString;
    if matches!(arg_category, Category::Char | Category::Number | Category::Pointer) && category == Category::String {
        return -3;
    }
    if (textual(arg_category) || arg_category == Category::Pointer) && matches!(category, Category::Char | Category::Number) {
        return -3;
    }
    if matches!(arg_category, Category::Char | Category::Number) && matches!(category, Category::Pointer | Category::CString) {
        return -3;
    }
    0
}

/// `cppWrittenTypeFit`.
fn cpp_written_type_fit(arg: &str, param: &str, category: Category, lvalue: Option<bool>, dependent: bool, deduced: bool) -> i32 {
    let fit = cpp_unqualified_type_fit(arg, param, category);
    if fit < 0 || dependent {
        return fit;
    }
    let arg_category = cpp_type_category(arg);
    if category == Category::Pointer && arg_category == Category::Class {
        return -3;
    }
    if !deduced && category == Category::Pointer && arg_category == Category::CString && !re!(r"\bvoid\b").is_match(param) {
        return -3;
    }
    if category == Category::CString && arg_category == Category::Pointer {
        return -3;
    }
    if cpp_refers_to_const(param) != Some(false) {
        return fit;
    }
    if !deduced && cpp_argument_const(arg, param) == Some(true) {
        return -3;
    }
    if lvalue == Some(false) && !strip_cpp_template_arguments(param).contains('*') {
        -3
    } else {
        fit
    }
}

/// `isCppDependentArgument`.
fn is_cpp_dependent_argument(ty: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let Some(name) = normalize_cpp_type_name(ty) else { return false };
    match ctx.node_by_id(&r.from_node_id) {
        Some(caller) => cpp_template_parameters(&caller, ctx).contains(&name),
        None => false,
    }
}

/// `cppNamesObject`: does the expression name an lvalue?
fn cpp_names_object(expr: &str, ty: &str) -> Option<bool> {
    let mut text = js_trim(expr).to_string();
    while text.starts_with('(') && text.ends_with(')') && cpp_opening_paren(&text, text.len() - 1) == Some(0) {
        text = js_trim(&text[1..text.len() - 1]).to_string();
    }
    if text.starts_with('*') || text.ends_with(']') {
        return Some(true);
    }
    if !text.ends_with(')') {
        return if re!(r"^[A-Za-z_][\w\s.:>-]*$").is_match(&text) { Some(true) } else { None };
    }
    if re!(r"&&\s*$").is_match(ty) {
        return None;
    }
    Some(re!(r"&\s*$").is_match(ty))
}

/// `cppArgumentType`.
fn cpp_argument_type(arg: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<ArgType> {
    let text = js_trim(arg);
    if re!(r"^(?:u8|u|U|L)?'(?:[^'\\]|\\.)+'$").is_match(text) {
        return Some(ArgType::Char);
    }
    if text == "true" || text == "false" {
        return Some(ArgType::Bool);
    }
    if text == "nullptr" || text == "NULL" {
        return Some(ArgType::Null);
    }
    if re!(r"^0+[uUlL]*$").is_match(text) {
        return Some(ArgType::Zero);
    }
    if re!(r"^[-+]?(?:0[xX][\da-fA-F']+|\d[\d']*(?:\.\d*)?(?:[eE][-+]?\d+)?|\.\d+(?:[eE][-+]?\d+)?)[uUlLfFzZ]*$").is_match(text) {
        return Some(ArgType::Number);
    }
    let address = re!(r"^&\s*([A-Za-z_]\w*)$").captures(text).and_then(|c| c.text(1).map(String::from));
    let name: Option<String> = address.clone().or_else(|| if re!(r"^[A-Za-z_]\w*$").is_match(text) { Some(text.to_string()) } else { None });
    if let Some(name) = name {
        let mut decl = CppReceiverDecl::default();
        infer_cpp_receiver_type(&name, r, ctx, 0, &mut decl);
        let declared = decl.declared.as_ref().map(|d| d.text.clone()).or_else(|| decl.raw.clone()).filter(|d| !d.is_empty())?;
        if !decl.callers || decl.shadowed {
            return None;
        }
        let written = if decl.constant && !re!(r"^\s*const\b").is_match(&declared) { format!("const {declared}") } else { declared };
        let typed = if address.is_some() || decl.array > 0 {
            format!("{}*", re!(r"\s*&+\s*$").replace_all_str(&written, ""))
        } else {
            written
        };
        let dependent = is_cpp_dependent_argument(&typed, r, ctx);
        return Some(ArgType::Type { text: typed, lvalue: Some(address.is_none()), dependent });
    }
    let typed = if re!(r#"^(?:u8|u|U|L)?R?["']"#).is_match(text) { None } else { cpp_expression_type(text, r, ctx, 0) };
    typed.map(|t| {
        let dependent = is_cpp_dependent_argument(&t.text, r, ctx);
        ArgType::Type { lvalue: cpp_names_object(text, &t.text), text: t.text, dependent }
    })
}

// -- Parameter lists -------------------------------------------------------------------------------

memo_map!(CppOverloadParameters, String, Option<Rc<str>>);

/// `isCppParameterPack`.
pub fn is_cpp_parameter_pack(p: &str) -> bool {
    let mut flat = p.to_string();
    loop {
        let next = re!(r"<[^<>]*>", "g").replace_all_str(&flat, "");
        if next == flat {
            break;
        }
        flat = next;
    }
    flat.contains("...")
}

/// `cppOverloadParameters`: the parameter list of a function declaration (for a method defined
/// out of line, the in-class declaration when only that one spells the default arguments).
pub fn cpp_overload_parameters(n: &Node, name: &str, ctx: &Ctx) -> Option<String> {
    let macro_head = cpp_macro_method_head(n);
    let own = match &macro_head {
        Some(m) => cpp_paren_list_in(m, 0, name),
        None => cpp_paren_list_after(&n.file_path, n.start_line, 0, name, ctx),
    };
    let own = own?;
    if n.kind != NodeKind::Method {
        return Some(own);
    }
    let memo = ctx.memo::<CppOverloadParameters>();
    if let Some(hit) = memo.lookup(&n.id) {
        return hit.map(|h| h.to_string());
    }
    let mut found = own.clone();
    let classes = cpp_class_decls(&cpp_parent_scope(&n.qualified_name), ctx);
    if !classes.iter().any(|c| c.file_path == n.file_path && range_within(n, c)) {
        let types = |list: &str| -> Vec<String> {
            split_cpp_top_level(&cpp_without_comments(list))
                .into_iter()
                .filter(|p| p != "void")
                .map(|p| normalize_cpp_type_name(&cpp_parameter_type(&p)).unwrap_or(p))
                .collect()
        };
        let want = types(&own);
        let word = Re::cached(&format!(r"\b{}\s*\(", esc(name)), "");
        'search: for cls in &classes {
            let lines = ctx.get_file_lines(&cls.file_path).unwrap_or_default();
            let last = lines.len().min(cls.end_line.max(cls.start_line) as usize);
            let mut line = cls.start_line as usize;
            while line <= last {
                if !word.is_match(&lines[line - 1]) {
                    line += 1;
                    continue;
                }
                let list = cpp_paren_list_after(&cls.file_path, line as u32, 0, name, ctx);
                if let Some(list) = list.filter(|l| l.contains('=')) {
                    let have = types(&list);
                    if have.len() == want.len() && have.iter().zip(want.iter()).all(|(a, b)| a == b) {
                        found = list;
                        break 'search;
                    }
                }
                line += 1;
            }
        }
    }
    memo.store(n.id.clone(), Some(Rc::from(found.as_str())));
    Some(found)
}

fn params_of(list: &str) -> Vec<String> {
    split_cpp_top_level(&cpp_without_comments(list)).into_iter().filter(|p| p != "void").collect()
}

/// `cppArgumentCountFits`.
pub fn cpp_argument_count_fits(n: &Node, name: &str, args: &[String], ctx: &Ctx) -> Option<bool> {
    if !matches!(n.kind, NodeKind::Function | NodeKind::Method) {
        return None;
    }
    let list = cpp_overload_parameters(n, name, ctx)?;
    let params = params_of(&list);
    let required = params.iter().filter(|p| !is_cpp_parameter_pack(p) && !p.contains('=')).count();
    Some(args.len() >= required && (params.iter().any(|p| is_cpp_parameter_pack(p)) || args.len() <= params.len()))
}

/// `cppOverloadFit`.
pub fn cpp_overload_fit(n: &Node, name: &str, args: &[String], ctx: &Ctx) -> i32 {
    if !matches!(n.kind, NodeKind::Function | NodeKind::Method) {
        return -1;
    }
    let Some(list) = cpp_overload_parameters(n, name, ctx) else { return 0 };
    let params = params_of(&list);
    let variadic = params.iter().any(|p| is_cpp_parameter_pack(p));
    let required = params.iter().filter(|p| !is_cpp_parameter_pack(p) && !p.contains('=')).count();
    let mut score = 0i32;
    if args.len() < required || (!variadic && args.len() > params.len()) {
        score -= 3;
    }
    for i in 0..args.len().min(params.len()) {
        let arg = &args[i];
        let param = &params[i];
        if is_cpp_parameter_pack(param) {
            break;
        }
        if !re!(r#"^(?:u8|u|U|L)?R?"|^FMT_STRING\s*\("#).is_match(arg) {
            continue;
        }
        let wide_arg = re!(r#"^LR?""#).is_match(arg);
        let wide_param = re!(r"\bw(?:string|char_t|format|string_view)|wchar_t").is_match(param);
        let category = cpp_type_category(&cpp_parameter_type(param));
        if wide_arg != wide_param && re!(r"string|char|Char|format").is_match(param) {
            score -= 2;
        } else if matches!(category, Category::Pointer | Category::Number | Category::Bool | Category::Char) {
            score -= 3;
        } else {
            score += if re!(r"string|char|Char|\bstr\b").is_match(param) {
                3
            } else if re!(r"^(?:const\s+)?[A-Z]\w{0,2}\s*[&*]{0,2}\s*\w*$").is_match(param) {
                1
            } else {
                -2
            };
        }
    }
    score
}

/// `cppArgumentTypeFit`.
fn cpp_argument_type_fit(n: &Node, name: &str, types: &[Option<ArgType>], ctx: &Ctx) -> i32 {
    let Some(list) = cpp_overload_parameters(n, name, ctx) else { return 0 };
    let params = params_of(&list);
    let own = if types.iter().any(|t| matches!(t, Some(ArgType::Type { .. }))) { cpp_own_template_parameters(n, ctx) } else { Vec::new() };
    let mut score = 0i32;
    for i in 0..types.len().min(params.len()) {
        let Some(arg) = &types[i] else { continue };
        let param = cpp_parameter_type(&params[i]);
        if param.contains("...") {
            continue;
        }
        let category = cpp_type_category(&param);
        let deduced = !own.is_empty() && own.contains(&normalize_cpp_type_name(&param).unwrap_or_default());
        score += match arg {
            ArgType::Type { text, lvalue, dependent } => cpp_written_type_fit(text, &param, category, *lvalue, *dependent, deduced),
            lit => literal_fit(lit, category),
        };
    }
    score
}

/// `cppConstlessParameters`.
fn cpp_constless_parameters(n: &Node, name: &str, ctx: &Ctx) -> Option<Vec<String>> {
    let list = cpp_overload_parameters(n, name, ctx)?;
    Some(
        params_of(&list)
            .into_iter()
            .map(|p| {
                let t = re!(r"\bconst\b", "g").replace_all_str(&cpp_parameter_type(&p), " ");
                re!(r"\s+", "g").replace_all_str(&t, "")
            })
            .collect(),
    )
}

/// `cppConstAdded`: how many arguments would need `const` added to bind.
fn cpp_const_added(n: &Node, name: &str, types: &[Option<ArgType>], ctx: &Ctx) -> usize {
    let Some(list) = cpp_overload_parameters(n, name, ctx) else { return 0 };
    let params = params_of(&list);
    let mut count = 0;
    for i in 0..types.len().min(params.len()) {
        let param = cpp_parameter_type(&params[i]);
        if let Some(ArgType::Type { text, .. }) = &types[i] {
            if cpp_refers_to_const(&param) == Some(true) && cpp_argument_const(text, &param) == Some(false) {
                count += 1;
            }
        }
    }
    count
}

/// `cppConstTwinChoice`.
fn cpp_const_twin_choice(best: NodeRef, others: Vec<NodeRef>, name: &str, types: &[Option<ArgType>], ctx: &Ctx) -> NodeRef {
    if others.is_empty() {
        return best;
    }
    let Some(shape) = cpp_constless_parameters(&best, name, ctx).map(|p| p.join(",")) else { return best };
    let mut added = cpp_const_added(&best, name, types, ctx);
    let mut choice = best;
    for n in others {
        if cpp_constless_parameters(&n, name, ctx).map(|p| p.join(",")).as_deref() != Some(shape.as_str()) {
            continue;
        }
        let count = cpp_const_added(&n, name, types, ctx);
        if count < added {
            choice = n;
            added = count;
        }
    }
    choice
}

/// `cppArgumentTypeChoice`: among equally-fitting overloads, the one the argument types select.
pub fn cpp_argument_type_choice(tied: Vec<NodeRef>, name: &str, args: &[String], r: &UnresolvedRef, ctx: &Ctx) -> NodeRef {
    if tied.len() == 1 || args.is_empty() {
        return tied[0].clone();
    }
    let types: Vec<Option<ArgType>> = args.iter().map(|a| cpp_argument_type(a, r, ctx)).collect();
    if types.iter().all(Option::is_none) {
        return tied[0].clone();
    }
    let fits: Vec<i32> = tied.iter().map(|n| cpp_argument_type_fit(n, name, &types, ctx)).collect();
    let top = *fits.iter().max().unwrap();
    let best = tied[fits.iter().position(|f| *f == top).unwrap()].clone();
    let others: Vec<NodeRef> = tied.iter().enumerate().filter(|(i, n)| fits[*i] == top && n.id != best.id).map(|(_, n)| n.clone()).collect();
    cpp_const_twin_choice(best, others, name, &types, ctx)
}

memo_map!(CppDeletedFunctions, String, bool);

/// `isCppDeletedFunction`.
fn is_cpp_deleted_function(n: &Node, name: &str, ctx: &Ctx) -> bool {
    if cpp_macro_method_head(n).is_some() {
        return false;
    }
    let memo = ctx.memo::<CppDeletedFunctions>();
    if let Some(hit) = memo.lookup(&n.id) {
        return hit;
    }
    let lines = ctx.get_file_lines(&n.file_path).unwrap_or_default();
    let from = (n.start_line as usize).saturating_sub(1).min(lines.len());
    let to = lines.len().min(n.start_line as usize + 11);
    let text = if from < to { lines[from..to].join("\n") } else { String::new() };
    let at = Re::cached(&format!(r"\b{}\s*\(", esc(name)), "").find(&text).map(|m| m.start());
    let declaration = match at {
        Some(at) => text[at..].split(|c| c == ';' || c == '{').next().unwrap_or("").to_string(),
        None => String::new(),
    };
    let deleted = re!(r"=\s*delete\b").is_match(&declaration);
    memo.store(n.id.clone(), deleted);
    deleted
}

/// `cppOverloadFor`: the overload a call selects from `candidates`.
pub fn cpp_overload_for(candidates: Vec<NodeRef>, name: &str, r: &UnresolvedRef, ctx: &Ctx) -> NodeRef {
    if candidates.len() == 1 {
        return candidates[0].clone();
    }
    let callable: Vec<NodeRef> = candidates.iter().filter(|n| !is_cpp_deleted_function(n, name, ctx)).cloned().collect();
    let overloads = if callable.is_empty() { candidates } else { callable };
    if overloads.len() == 1 {
        return overloads[0].clone();
    }
    let Some(args) = super::cpp_text::cpp_call_arguments(r, name, ctx) else { return overloads[0].clone() };
    let fits: Vec<i32> = overloads.iter().map(|n| cpp_overload_fit(n, name, &args, ctx)).collect();
    let top = *fits.iter().max().unwrap();
    let tied: Vec<NodeRef> = overloads.iter().enumerate().filter(|(i, _)| fits[*i] == top).map(|(_, n)| n.clone()).collect();
    cpp_argument_type_choice(tied, name, &args, r, ctx)
}

/// `cppFunctionOverload`: of the free functions sharing a name, the one a call selects.
pub fn cpp_function_overload(chosen: &NodeRef, candidates: &[NodeRef], r: &UnresolvedRef, ctx: &Ctx) -> NodeRef {
    if !matches!(r.language, Language::Cpp | Language::C) || !r.is_kind(EdgeKind::Calls) {
        return chosen.clone();
    }
    let overloads: Vec<NodeRef> = candidates
        .iter()
        .filter(|n| n.kind == NodeKind::Function && n.qualified_name == chosen.qualified_name && is_visible_across_files(n, r, ctx))
        .cloned()
        .collect();
    if overloads.is_empty() {
        return chosen.clone();
    }
    if r.language == Language::C {
        let own: Vec<NodeRef> =
            overloads.iter().filter(|n| n.file_path == r.file_path || cpp_includers(&n.file_path, ctx).contains(&r.file_path)).cloned().collect();
        let external: Vec<NodeRef> = if own.is_empty() {
            Vec::new()
        } else {
            overloads.iter().filter(|n| !own.iter().any(|o| o.id == n.id) && !is_static_c_function(n, ctx)).cloned().collect()
        };
        let outside: Vec<NodeRef> = if !external.is_empty()
            && own.iter().all(|n| is_c_feature_variant(n, &r.file_path, r.line, ctx))
            && !is_off_by_default(&r.file_path, r.line as usize, ctx)
        {
            external.iter().filter(|n| !is_c_feature_variant(n, &r.file_path, r.line, ctx)).cloned().collect()
        } else {
            Vec::new()
        };
        let pool = if !outside.is_empty() {
            &outside
        } else if !own.is_empty() {
            &own
        } else {
            &overloads
        };
        return find_best_match(r, pool, ctx).unwrap_or_else(|| chosen.clone());
    }
    let ordered: Vec<NodeRef> = if overloads.iter().any(|n| n.id == chosen.id) {
        let mut v = vec![chosen.clone()];
        v.extend(overloads.iter().filter(|n| n.id != chosen.id).cloned());
        v
    } else {
        overloads
    };
    cpp_overload_for(ordered, &chosen.name, r, ctx)
}

// -- Specialization pools -------------------------------------------------------------------------

memo_map!(CppSpecializationOwnerArguments, String, Option<Rc<Vec<String>>>);
memo_map!(CppSpecializationArgumentsMemo, String, Option<Rc<Vec<String>>>);

/// `cppTemplateArgumentsEnd` over bytes: the `>` closing the list that opens at `at`.
fn template_arguments_end(text: &str, at: usize) -> Option<usize> {
    let b = text.as_bytes();
    if !(at > 0 && is_word_byte(b[at - 1])) || b.get(at + 1) == Some(&b'<') || b.get(at + 1) == Some(&b'=') {
        return None;
    }
    let (mut angles, mut others) = (0i64, 0i64);
    let mut i = at;
    while i < b.len() {
        let ch = b[i];
        if ch == b'"' || ch == b'\'' {
            let mut j = i + 1;
            while j < b.len() && b[j] != ch {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            i = j;
        } else if matches!(ch, b'(' | b'[' | b'{') {
            others += 1;
        } else if matches!(ch, b')' | b']' | b'}') {
            others -= 1;
            if others < 0 {
                return None;
            }
        } else if others > 0 {
        } else if ch == b'<' {
            angles += 1;
        } else if ch == b'>' && i > 0 && b[i - 1] != b'-' {
            angles -= 1;
            if angles == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// `cppTemplateArgumentKey`.
fn cpp_template_argument_key(arg: &str) -> String {
    let t = re!(r"\s*([^\w\s])\s*", "g").replace_all(arg, |c| c.text(1).map(String::from));
    re!(r"\s+", "g").replace_all_str(&t, " ")
}

/// `cppTemplateArgumentsAfter`: the template arguments written after the first whole-word `name`
/// at or after byte `from`.
fn cpp_template_arguments_after(text: &str, name: &str, from: usize) -> Option<Vec<String>> {
    let b = text.as_bytes();
    let mut search = from.min(text.len());
    while let Some(p) = text.get(search..).and_then(|s| s.find(name)) {
        let at = search + p;
        let open = at + name.len();
        let word_before = at > 0 && (is_word_byte(b[at - 1]) || b[at - 1] == b'$');
        let word_after = b.get(open).is_some_and(|c| is_word_byte(*c) || *c == b'$');
        if word_before || word_after {
            search = at + text[at..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
            continue;
        }
        let end = if b.get(open) == Some(&b'<') { template_arguments_end(text, open) } else { None };
        let args: Vec<String> = match end {
            Some(end) => split_cpp_top_level(&text[open + 1..end]).iter().map(|a| cpp_template_argument_key(a)).collect(),
            None => Vec::new(),
        };
        return if args.is_empty() { None } else { Some(args) };
    }
    None
}

/// `cppTemplateArgumentsMatch`.
fn cpp_template_arguments_match(written: &[String], call: &[String]) -> bool {
    written.len() == call.len()
        && written.iter().zip(call.iter()).all(|(w, c)| w == c || w.ends_with(&format!("::{c}")) || c.ends_with(&format!("::{w}")))
}

fn joined_lines(ctx: &Ctx, file: &str, from_line: usize, to_line: usize) -> String {
    let lines = ctx.get_file_lines(file).unwrap_or_default();
    let from = from_line.min(lines.len());
    let to = to_line.min(lines.len());
    if from < to {
        lines[from..to].join("\n")
    } else {
        String::new()
    }
}

/// `cppSpecializationArguments`: the template arguments a specialization spells after its name.
pub fn cpp_specialization_arguments(n: &Node, ctx: &Ctx) -> Option<Rc<Vec<String>>> {
    if n.language != Language::Cpp || !matches!(n.kind, NodeKind::Function | NodeKind::Method) || n.signature.as_deref().is_some_and(|s| s.starts_with('(')) {
        return None;
    }
    let memo = ctx.memo::<CppSpecializationArgumentsMemo>();
    if let Some(hit) = memo.lookup(&n.id) {
        return hit;
    }
    let head = match n.signature.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => {
            let text = joined_lines(ctx, &n.file_path, (n.start_line as usize).saturating_sub(1), n.start_line as usize + 11);
            super::util::after_col(&text, n.start_column).to_string()
        }
    };
    let args = cpp_template_arguments_after(&head, &n.name, 0).map(Rc::new);
    memo.store(n.id.clone(), args.clone());
    args
}

/// `cppCallTemplateArguments`.
fn cpp_call_template_arguments(r: &UnresolvedRef, ctx: &Ctx) -> Option<Vec<String>> {
    let text = joined_lines(ctx, &r.file_path, (r.line as usize).saturating_sub(1), r.line as usize + 3);
    let name = re!(r"::|\.|->").split(&r.reference_name).into_iter().last().unwrap_or("").to_string();
    cpp_template_arguments_after(&text, &name, utf16_byte_index(&text, r.column as usize))
}

/// `cppSpecializationPool`: of same-named candidates, the specializations a call's explicit
/// template arguments select.
pub fn cpp_specialization_pool(candidates: Vec<NodeRef>, r: &UnresolvedRef, ctx: &Ctx) -> Vec<NodeRef> {
    if r.language != Language::Cpp {
        return candidates;
    }
    let written: Vec<Option<Rc<Vec<String>>>> = candidates.iter().map(|n| cpp_specialization_arguments(n, ctx)).collect();
    if written.iter().all(Option::is_none) {
        return candidates;
    }
    let others: Vec<NodeRef> = candidates.iter().enumerate().filter(|(i, _)| written[*i].is_none()).map(|(_, n)| n.clone()).collect();
    let Some(args) = cpp_call_template_arguments(r, ctx) else {
        return if others.is_empty() { candidates } else { others };
    };
    let named: Vec<NodeRef> = candidates
        .iter()
        .enumerate()
        .filter(|(i, _)| written[*i].as_ref().is_some_and(|w| cpp_template_arguments_match(w, &args)))
        .map(|(_, n)| n.clone())
        .collect();
    if named.is_empty() {
        others
    } else {
        named
    }
}

/// `cppQualifierArguments`: the template arguments written on the qualifier of a `A<T>::f` call.
pub fn cpp_qualifier_arguments(r: &UnresolvedRef, ctx: &Ctx) -> Option<Vec<String>> {
    let segments: Vec<&str> = r.reference_name.split("::").collect();
    if segments.len() < 2 {
        return None;
    }
    let text = joined_lines(ctx, &r.file_path, (r.line as usize).saturating_sub(1), r.line as usize + 3);
    cpp_template_arguments_after(&text, segments[segments.len() - 2], utf16_byte_index(&text, r.column as usize))
}

/// `cppSpecializationOwnerArguments`: the arguments an out-of-line member specialization spells on its class.
fn cpp_specialization_owner_arguments(n: &Node, ctx: &Ctx) -> Option<Rc<Vec<String>>> {
    if n.language != Language::Cpp || !matches!(n.kind, NodeKind::Method | NodeKind::Function) {
        return None;
    }
    let segments: Vec<&str> = n.qualified_name.split("::").collect();
    if segments.len() < 2 {
        return None;
    }
    let memo = ctx.memo::<CppSpecializationOwnerArguments>();
    if let Some(hit) = memo.lookup(&n.id) {
        return hit;
    }
    let owner = esc(segments[segments.len() - 2]);
    let name = esc(&n.name);
    let lines = ctx.get_file_lines(&n.file_path).unwrap_or_default();
    let head_all = joined_lines(ctx, &n.file_path, (n.start_line as usize).saturating_sub(1), n.start_line as usize + 11);
    let head = super::util::after_col(&head_all, n.start_column).to_string();
    let mut found: Option<Rc<Vec<String>>> = None;
    for m in Re::cached(&format!(r"(?<![\w$]){owner}<"), "g").captures_iter(&head) {
        let whole = m.get(0).unwrap();
        let open = whole.end() - 1;
        let Some(end) = template_arguments_end(&head, open) else { continue };
        if !Re::cached(&format!(r"^\s*::\s*~?{name}(?![\w$])"), "").is_match(&head[end + 1..]) {
            continue;
        }
        let args: Vec<String> = split_cpp_top_level(&head[open + 1..end]).iter().map(|a| cpp_template_argument_key(a)).collect();
        let from = (n.start_line as usize).saturating_sub(3);
        let to = (n.start_line as usize).saturating_sub(1).min(lines.len());
        let above = if from.min(lines.len()) < to { lines[from.min(lines.len())..to].join("\n") } else { String::new() };
        let before = format!("{}\n{}", above, &head[..whole.start()]);
        let header = re!(r"\btemplate\s*<\s*(>)?(?![\s\S]*\btemplate\s*<)").captures(&before);
        let parameters: Vec<String> = cpp_template_parameters(n, ctx).iter().cloned().collect();
        let dependent = header.as_ref().is_some_and(|h| h.text(1).is_none())
            || args.iter().any(|a| {
                parameters
                    .iter()
                    .any(|p| Re::try_cached(&format!(r"(?<![\w$]){p}(?![\w$])"), "").is_some_and(|re| re.is_match(a)))
            });
        found = if dependent || args.is_empty() { None } else { Some(Rc::new(args)) };
        break;
    }
    memo.store(n.id.clone(), found.clone());
    found
}

/// `cppQualifierSpecializationPool`.
pub fn cpp_qualifier_specialization_pool(candidates: Vec<NodeRef>, r: &UnresolvedRef, ctx: &Ctx) -> Vec<NodeRef> {
    if r.language != Language::Cpp || candidates.is_empty() {
        return candidates;
    }
    let Some(args) = cpp_qualifier_arguments(r, ctx) else { return candidates };
    candidates
        .into_iter()
        .filter(|n| match cpp_specialization_owner_arguments(n, ctx) {
            None => true,
            Some(owner) => cpp_template_arguments_match(&owner, &args),
        })
        .collect()
}

// -- Macro-namespaced names -------------------------------------------------------------------------

/// `matchCppMacroNamespaced`: `ns::name` where `ns` is opened by a namespace macro or alias.
pub fn match_cpp_macro_namespaced(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let mut target = r.reference_name.strip_prefix("::").unwrap_or(&r.reference_name).to_string();
    if !target.contains("::") {
        return None;
    }
    let head = target[..target.find("::").unwrap()].to_string();
    let alias = cpp_namespace_aliases(ctx).get(&head).cloned();
    if let Some(alias) = &alias {
        target = format!("{}{}", alias, &target[head.len()..]);
    }
    let name = target[target.rfind("::").unwrap() + 2..].to_string();
    if !re!(r"^[A-Za-z_~]\w*$").is_match(&name) {
        return None;
    }
    let mut matches: Vec<NodeRef> = Vec::new();
    for n in ctx.get_nodes_by_name(&name).iter() {
        if !matches!(n.language, Language::Cpp | Language::C) {
            continue;
        }
        if !matches!(
            n.kind,
            NodeKind::Function | NodeKind::Method | NodeKind::Class | NodeKind::Struct | NodeKind::Enum | NodeKind::TypeAlias | NodeKind::Union | NodeKind::Variable | NodeKind::Constant
        ) {
            continue;
        }
        let mut frames: Vec<_> = cpp_macro_namespace_frames(&n.file_path, ctx)
            .iter()
            .filter(|f| f.start <= n.start_line as usize && f.end >= n.start_line as usize)
            .cloned()
            .collect();
        frames.sort_by_key(|f| f.start);
        let prefix: Vec<String> = frames.into_iter().flat_map(|f| f.path).collect();
        let effective = if !prefix.is_empty() {
            format!("{}::{}", prefix.join("::"), n.qualified_name)
        } else if alias.is_some() {
            n.qualified_name.clone()
        } else {
            String::new()
        };
        if effective == target {
            matches.push(n.clone());
        }
    }
    let pool = cpp_specialization_pool(matches, r, ctx);
    if pool.is_empty() {
        return None;
    }
    let args = if pool.len() > 1 && r.is_kind(EdgeKind::Calls) { super::cpp_text::cpp_call_arguments(r, &name, ctx) } else { None };
    let scores: Vec<i32> =
        pool.iter().map(|n| (if is_test_path(&n.file_path) { -10 } else { 0 }) + args.as_ref().map(|a| cpp_overload_fit(n, &name, a, ctx)).unwrap_or(0)).collect();
    let top = *scores.iter().max().unwrap();
    let tied: Vec<NodeRef> = pool.iter().enumerate().filter(|(i, _)| scores[*i] == top).map(|(_, n)| n.clone()).collect();
    let best = match &args {
        Some(a) => cpp_argument_type_choice(tied, &name, a, r, ctx),
        None => tied[0].clone(),
    };
    Some(ResolvedRef::new(r, best.id.clone(), 0.8, ResolvedBy::QualifiedName))
}

