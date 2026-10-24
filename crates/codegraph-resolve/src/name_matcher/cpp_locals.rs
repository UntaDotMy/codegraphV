//! Names a C++ function body declares locally — port of `isCppLocalName` and
//! `isCppLocalCallableCall`: does a bare call resolve to a local (a parameter, a lambda, a
//! variable) instead of a function?

use super::cpp_decl::{
    build_declarator_regex, build_listed_declarator_regex, cpp_code_of, cpp_for_body_encloses, cpp_listed_declarator_on, cpp_range_for_declaration,
    cpp_rebinds_receiver, esc, match_cpp_listed_declarator, normalize_cpp_type_name, CPP_NOT_A_DECLARED_TYPE,
};
use super::cpp_scope::cpp_implicit_receiver;
use super::cpp_scope::Receiver;
use super::cpp_text::split_cpp_top_level;
use super::util::{col_byte, memo_map, MemoMap};
use crate::cpp_raw_strings::{cpp_literals_blanked, cpp_non_code_lines, NonCodeLines};
use crate::cpp_type_aliases::strip_cpp_template_arguments;
use crate::ctx::Ctx;
use crate::types::UnresolvedRef;
use codegraph_jsre::{re, Re};
use codegraph_types::jscompat::{js_trim, utf16_len};
use codegraph_types::{Node, NodeKind};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::LazyLock;

const CPP_LOCAL_SPECIFIERS: &str = r"(?:(?:const|constexpr|static|volatile|thread_local|register)\s+)*";
const CPP_LOCAL_TEMPLATE_ARGUMENTS: &str = r"<(?:[^<>;={}]|<(?:[^<>;={}]|<[^<>;={}]*>)*>)*>";
const CPP_STATEMENT_START: &str = r"(?:^|[;{}]|\b(?:for|if)\s*\()\s*";

static CPP_LOCAL_TYPE: LazyLock<String> = LazyLock::new(|| {
    format!(r"(?:::\s*)?[A-Za-z_]\w*(?:\s*::\s*[A-Za-z_]\w*)*(?:\s*{CPP_LOCAL_TEMPLATE_ARGUMENTS}(?:\s*::\s*[A-Za-z_]\w*)*)?")
});
static CPP_AUTO_LOCAL: LazyLock<Re> = LazyLock::new(|| {
    Re::new(&format!(r"{CPP_STATEMENT_START}{CPP_LOCAL_SPECIFIERS}auto(?:\s+|\s*[*&]+\s*)([A-Za-z_]\w*)\s*(?:=|\{{|\()"), "g")
});
static CPP_TYPED_LOCAL: LazyLock<Re> = LazyLock::new(|| {
    Re::new(
        &format!(r"{CPP_STATEMENT_START}{CPP_LOCAL_SPECIFIERS}({})(\s+|\s*[*&]+\s*)(?:const\s+)?([A-Za-z_]\w*)\s*([=({{;\[,])", &*CPP_LOCAL_TYPE),
        "g",
    )
});
static CPP_LOCAL_TYPE_DECLARATION: LazyLock<Re> = LazyLock::new(|| {
    Re::new(
        &format!(
            r"{CPP_STATEMENT_START}(?:using\s+([A-Za-z_]\w*)\s*=|typedef\b[^;]*?\b([A-Za-z_]\w*)\s*;|(?:struct|class|union|enum)(?:\s+class)?\s+([A-Za-z_]\w*)\b)"
        ),
        "g",
    )
});
static CPP_LAMBDA_HEAD: LazyLock<Re> = LazyLock::new(|| Re::new(r"\]\s*\(([^()]*)\)\s*(?:mutable\s*)?(?:->[^{;]*)?\{", "g"));

/// `cppHasWord`: `name` occurs in `text` as a whole word.
fn cpp_has_word(text: &str, name: &str) -> bool {
    super::cpp_recv::has_cpp_word(text, name)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BindingKind {
    Parameter,
    Variable,
    Type,
    Function,
}

#[derive(Clone, Debug)]
pub struct CppLocalBinding {
    /// 0-based line.
    pub line: usize,
    /// UTF-16 column.
    pub column: usize,
    pub kind: BindingKind,
    /// Byte offset of the `(` of a range-for that binds the name.
    pub loop_paren: Option<usize>,
}

memo_map!(CppLocalBindings, String, Rc<Vec<CppLocalBinding>>);
memo_map!(CppCallerHeads, String, Rc<(HashSet<String>, usize)>);

/// `cppParameterNames`.
fn cpp_parameter_names(list: &str) -> Vec<String> {
    let mut names = Vec::new();
    for parameter in split_cpp_top_level(list) {
        let declarator = js_trim(&re!(r"=[\s\S]*$").replace_all_str(&parameter, "")).to_string();
        let Some(m) = re!(r"([A-Za-z_]\w*)\s*(?:\[[^\]]*\])*$").captures(&declarator) else { continue };
        let whole = m.get(0).unwrap();
        let before = re!(r"\b(?:const|volatile|struct|class|enum|union|typename|signed|unsigned)\b", "g").replace_all_str(&declarator[..whole.start()], "");
        let before = js_trim(&before);
        if !before.is_empty() && !re!(r"::\s*$").is_match(before) {
            names.push(m.text(1).unwrap_or("").to_string());
        }
    }
    names
}

/// The code of each line of a file (literals and comments blanked), memoized.
struct CodeLines<'a> {
    raw: &'a [String],
    non_code: &'a NonCodeLines,
    cache: RefCell<HashMap<usize, Rc<str>>>,
}

impl<'a> CodeLines<'a> {
    fn get(&self, l: usize) -> Rc<str> {
        if let Some(hit) = self.cache.borrow().get(&l) {
            return Rc::clone(hit);
        }
        let code: Rc<str> = cpp_code_of(&cpp_literals_blanked(self.raw.get(l).map(String::as_str).unwrap_or(""), l + 1, self.non_code)).unwrap_or_default().into();
        self.cache.borrow_mut().insert(l, Rc::clone(&code));
        code
    }
}

/// `cppParenTextFrom`: the text inside the parentheses opening at `column` of `line`.
fn cpp_paren_text_from(code: &CodeLines, line: usize, column: usize) -> Option<String> {
    let mut depth = 0i64;
    let mut text = String::new();
    for l in line..line + 12 {
        let c = code.get(l);
        let from = if l == line { column.min(c.len()) } else { 0 };
        for ch in super::util::slice_bytes(&c, from, c.len()).chars() {
            if ch == '(' {
                depth += 1;
            } else if ch == ')' {
                depth -= 1;
                if depth == 0 {
                    return Some(text.chars().skip(1).collect());
                }
            }
            text.push(ch);
        }
        text.push(' ');
    }
    None
}

/// `cppDeclaresLocalFunction`: `T name(args);` declares a function rather than an object.
fn cpp_declares_local_function(ty: &str, args: &str) -> bool {
    if ty == "void" || js_trim(args).is_empty() {
        return true;
    }
    split_cpp_top_level(args).iter().any(|a| {
        re!(r"^(?:int|char|bool|float|double|long|short|unsigned|signed|void|size_t)$").is_match(a)
            || (re!(r"^(?:(?:const|volatile|unsigned|signed)\s+)*[A-Za-z_][\w:]*(?:\s*<[^>]*>)?\s*[*&]*\s*(?:[A-Za-z_]\w*)?$").is_match(a)
                && re!(r"\s|[*&]").is_match(a))
    })
}

/// `cppCallerHead`: the parameter names of the caller and the line its body starts on.
fn cpp_caller_head(caller: &Node, code: &CodeLines, line_count: usize, ctx: &Ctx) -> Rc<(HashSet<String>, usize)> {
    let memo = ctx.memo::<CppCallerHeads>();
    if let Some(hit) = memo.lookup(&caller.id) {
        return hit;
    }
    let first = (caller.start_line as usize).saturating_sub(1);
    let mut head = (HashSet::new(), first);
    let named = Re::cached(&format!(r"\b{}\s*\(", esc(&caller.name)), "");
    for l in first..line_count.min(first + 13) {
        let c = code.get(l);
        let Some(at) = named.find(&c) else { continue };
        let list = cpp_paren_text_from(code, l, at.end() - 1);
        head = (list.map(|l| cpp_parameter_names(&l).into_iter().collect()).unwrap_or_default(), l);
        break;
    }
    let head = Rc::new(head);
    memo.store(caller.id.clone(), Rc::clone(&head));
    head
}

/// `cppLocalBindings`: where in the caller's body `name` is declared.
fn cpp_local_bindings(name: &str, caller: &Node, head: &(HashSet<String>, usize), code: &CodeLines, raw_lines: &[String]) -> Vec<CppLocalBinding> {
    let first = (caller.start_line as usize).saturating_sub(1);
    let last = (raw_lines.len() as isize - 1).min(caller.end_line.max(caller.start_line) as isize - 1);
    let mut bindings: Vec<CppLocalBinding> = Vec::new();
    if head.0.contains(name) {
        bindings.push(CppLocalBinding { line: head.1, column: 0, kind: BindingKind::Parameter, loop_paren: None });
    }
    let starts_statement = |l: usize| -> bool {
        let mut k = l as isize - 1;
        while k >= first as isize {
            let above = code.get(k as usize);
            if !js_trim(&above).is_empty() {
                return re!(r"[;{}:]\s*$").is_match(&above) || re!(r"^\s*#").is_match(&above) || (k as usize) < head.1;
            }
            k -= 1;
        }
        true
    };
    let mut l = head.1 as isize;
    while l <= last {
        let li = l as usize;
        l += 1;
        if !cpp_has_word(raw_lines.get(li).map(String::as_str).unwrap_or(""), name) {
            continue;
        }
        let text = code.get(li);
        if !cpp_has_word(&text, name) {
            continue;
        }
        let col16 = |byte: usize| utf16_len(&text[..byte.min(text.len())]);
        let starts = |index: usize| index > 0 || li == head.1 || starts_statement(li);
        for m in CPP_LOCAL_TYPE_DECLARATION.captures_iter(&text) {
            let start = m.get(0).unwrap().start();
            let declared = m.text(1).or(m.text(2)).or(m.text(3));
            if declared == Some(name) && starts(col16(start)) {
                bindings.push(CppLocalBinding { line: li, column: col16(start), kind: BindingKind::Type, loop_paren: None });
            }
        }
        for m in CPP_AUTO_LOCAL.captures_iter(&text) {
            let start = m.get(0).unwrap().start();
            if m.text(1) == Some(name) && starts(col16(start)) {
                bindings.push(CppLocalBinding { line: li, column: col16(start), kind: BindingKind::Variable, loop_paren: None });
            }
        }
        for m in CPP_TYPED_LOCAL.captures_iter(&text) {
            let whole = m.get(0).unwrap();
            let Some(ty) = m.text(1) else { continue };
            if m.text(3) != Some(name) || !starts(col16(whole.start())) {
                continue;
            }
            let first_word = re!(r"[\s<:]").split(ty).into_iter().next().unwrap_or("");
            if CPP_NOT_A_DECLARED_TYPE.contains(first_word) || re!(r"^(?:using|typedef|struct|class|union|enum|auto)$").is_match(ty) {
                continue;
            }
            let mut kind = BindingKind::Variable;
            if m.text(4) == Some("(") {
                if re!(r"[*&]").is_match(m.text(2).unwrap_or("")) {
                    continue;
                }
                if cpp_declares_local_function(ty, &cpp_paren_text_from(code, li, whole.end() - 1).unwrap_or_default()) {
                    kind = BindingKind::Function;
                }
            }
            bindings.push(CppLocalBinding { line: li, column: col16(whole.start()), kind, loop_paren: None });
        }
        if text.contains(',') {
            if let Some(m) = cpp_listed_declarator_on(&text, name) {
                if starts(col16(m.index)) {
                    bindings.push(CppLocalBinding { line: li, column: col16(m.index), kind: BindingKind::Variable, loop_paren: None });
                }
            }
        }
        if let Some(lp) = cpp_range_for_declaration(&text, name) {
            bindings.push(CppLocalBinding { line: li, column: col16(lp.paren), kind: BindingKind::Variable, loop_paren: Some(lp.paren) });
        }
        for m in CPP_LAMBDA_HEAD.captures_iter(&text) {
            let whole = m.get(0).unwrap();
            if cpp_parameter_names(m.text(1).unwrap_or("")).iter().any(|n| n == name) {
                bindings.push(CppLocalBinding { line: li, column: col16(whole.end()), kind: BindingKind::Variable, loop_paren: None });
            }
        }
    }
    bindings
}

/// `cppBlockReaches`: is the block a declaration at `line:column` opens still open at the use?
fn cpp_block_reaches(code: &CodeLines, line: usize, column: usize, to_line: usize, to_column: usize) -> bool {
    let mut depth = 0i64;
    for l in line..=to_line {
        let text = code.get(l);
        let b = text.as_bytes();
        let end = if l == to_line { col_byte(&text, to_column as u32).min(b.len()) } else { b.len() };
        let mut c = if l == line { col_byte(&text, column as u32) } else { 0 };
        while c < end {
            if b[c] == b'{' {
                depth += 1;
            } else if b[c] == b'}' {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            c += 1;
        }
    }
    true
}

/// `isCppLocalCallableCall`: the call names a local variable/parameter that is callable.
pub fn is_cpp_local_callable_call(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let name = r.reference_name.as_str();
    if !re!(r"^[A-Za-z_]\w*$").is_match(name) || cpp_implicit_receiver(r, ctx) != Some(Receiver::Bare) {
        return false;
    }
    let Some(caller) = ctx.node_by_id(&r.from_node_id) else { return false };
    if !matches!(caller.kind, NodeKind::Function | NodeKind::Method)
        || caller.file_path != r.file_path
        || caller.name == name
        || r.line < caller.start_line
        || r.line > caller.end_line.max(caller.start_line)
    {
        return false;
    }
    let Some(raw_lines) = ctx.get_file_lines(&r.file_path) else { return false };
    let non_code = cpp_non_code_lines(&r.file_path, ctx);
    let code = CodeLines { raw: &raw_lines, non_code: &non_code, cache: RefCell::new(HashMap::new()) };
    let memo = ctx.memo::<CppLocalBindings>();
    let key = format!("{}\u{0}{}", caller.id, name);
    let bindings = match memo.lookup(&key) {
        Some(b) => b,
        None => {
            let head = cpp_caller_head(&caller, &code, raw_lines.len(), ctx);
            let b = Rc::new(cpp_local_bindings(name, &caller, &head, &code, &raw_lines));
            memo.store(key, Rc::clone(&b));
            b
        }
    };
    let call_line = r.line as usize - 1;
    let use_byte = raw_lines.get(call_line).map(|l| col_byte(l, r.column)).unwrap_or(0);
    let mut decided: Option<&CppLocalBinding> = None;
    for b in bindings.iter() {
        if b.line > call_line || (b.line == call_line && b.column >= r.column as usize) {
            continue;
        }
        let in_scope = b.kind == BindingKind::Parameter
            || match b.loop_paren {
                Some(paren) => cpp_for_body_encloses(&raw_lines, b.line, paren, call_line, use_byte),
                None => cpp_block_reaches(&code, b.line, b.column, call_line, r.column as usize),
            };
        if in_scope && decided.is_none_or(|d| b.line > d.line || (b.line == d.line && b.column > d.column)) {
            decided = Some(b);
        }
    }
    matches!(decided.map(|d| d.kind), Some(BindingKind::Parameter | BindingKind::Variable))
}

/// `isCppLocalName`: a declaration of `name` precedes the call inside the caller.
pub fn is_cpp_local_name(name: &str, caller: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if caller.file_path != r.file_path || matches!(caller.kind, NodeKind::Class | NodeKind::Struct | NodeKind::Union) {
        return false;
    }
    let Some(lines) = ctx.get_file_lines(&r.file_path) else { return false };
    let escaped = esc(name);
    let word = Re::cached(&format!(r"\b{escaped}\b"), "");
    let declarator = build_declarator_regex(&escaped);
    let listed = build_listed_declarator_regex(&escaped);
    let non_code = cpp_non_code_lines(&caller.file_path, ctx);
    let from = (caller.start_line as usize).saturating_sub(1);
    for i in from..lines.len().min(r.line as usize) {
        if !word.is_match(&lines[i]) {
            continue;
        }
        let Some(code) = cpp_code_of(&cpp_literals_blanked(&lines[i], i + 1, &non_code)).filter(|c| !c.is_empty() && word.is_match(c)) else {
            continue;
        };
        if cpp_rebinds_receiver(&code, &escaped) {
            return true;
        }
        for variant in [code.clone(), strip_cpp_template_arguments(&code)] {
            if let Some(ty) = declarator.captures(&variant).and_then(|c| c.text(1).map(String::from)) {
                if !re!(r"::\s*$").is_match(&ty) && normalize_cpp_type_name(&ty).is_some() {
                    return true;
                }
            }
        }
        if let Some(m) = match_cpp_listed_declarator(&code, name, &listed) {
            if normalize_cpp_type_name(&m.ty).is_some() {
                return true;
            }
        }
    }
    false
}
