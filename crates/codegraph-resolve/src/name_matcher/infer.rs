//! Receiver type inference: what class does `lg` in `lg.warn()` name? Read from the nearest
//! declaration of `lg` in the enclosing function, language by language.

use super::js_imports::*;
use super::js_store::{enclosing_scope_start_line, has_parameter_binding, import_shadowed_at};
use super::go::chain::{go_call_value_type, GoTypeText};
use super::go::{go_asserted_local_type, go_local_decl, go_named_type, go_written_type, GoProjectType};
use super::reach::is_lexically_reachable;
use super::util::*;
use crate::ambient_modules::is_merged_ambient_declaration;
use crate::ctx::Ctx;
use crate::js_builtins::TS_PRIMITIVE_TYPES;
use crate::types::{NodeRef, Tri, UnresolvedRef};
use codegraph_extract::strip_comments::{blank_string_contents, strip_comments_for_regex};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::utf16_len;
use codegraph_types::{EdgeKind, Language, Node, NodeKind, ReferenceKind};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

/// An optional generic argument list and array brackets after a type name.
pub const TYPE_ARGS: &str = r"(?:<[^;=(){}<>]*(?:<[^;=(){}<>]*>[^;=(){}<>]*)*>)?\s*(?:\[[\s,]*\]\s*)*";
/// A Swift call with balanced parentheses that is not chained on, or an unterminated one.
const SWIFT_UNCHAINED_CALL: &str = r"(?:\((?:[^()]|\((?:[^()]|\([^()]*\))*\))*\)(?!\s*\??\.)|\([^)]*$)";

pub(super) const NON_TYPE_RECEIVER_TOKENS: [&str; 16] =
    ["this", "self", "super", "new", "return", "await", "yield", "typeof", "null", "nil", "None", "true", "false", "True", "False", "undefined"];

/// The type name an inferred type reads as (`Foo<T>*` → `Foo`, `a.b.Foo` → `Foo`), or none for a keyword.
pub fn normalize_inferred_type_name(raw: &str) -> Option<String> {
    let cleaned = re!(r"[&*]", "g").replace_all_str(&re!(r"<[^>]*>", "g").replace_all_str(raw, ""), "");
    let cleaned = codegraph_types::jscompat::js_trim(&cleaned);
    let seg = re!(r"[.:]+").split(cleaned).into_iter().filter(|s| !s.is_empty()).last()?;
    if NON_TYPE_RECEIVER_TOKENS.contains(&seg) {
        None
    } else {
        Some(seg.to_string())
    }
}

/// `Item[]` → `Item`.
pub fn array_element_type(ty: &str) -> Option<String> {
    re!(r"^(.+?)\s*\[\s*\]$").captures(ty).and_then(|c| c.text(1).map(|s| s.to_string()))
}

fn at(template: &str, r: &str) -> String {
    template.replace("@R@", r)
}

fn pats(list: &[String], flags: &str) -> Vec<Arc<Re>> {
    list.iter().map(|p| Re::cached(p, flags)).collect()
}

thread_local! {
    static PATTERN_MEMO: RefCell<HashMap<String, Rc<Vec<Arc<Re>>>>> = RefCell::new(HashMap::new());
}

const PATTERN_MEMO_CAP: usize = 8192;

fn memo_patterns(key: String, build: impl FnOnce() -> Vec<Arc<Re>>) -> Rc<Vec<Arc<Re>>> {
    if let Some(hit) = PATTERN_MEMO.with(|m| m.borrow().get(&key).cloned()) {
        return hit;
    }
    let built = Rc::new(build());
    PATTERN_MEMO.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() >= PATTERN_MEMO_CAP {
            m.clear();
        }
        m.insert(key, built.clone());
    });
    built
}

/// The declaration patterns that bind `r` (already regex-escaped) to a type, for `language`.
pub fn local_receiver_type_patterns(language: Language, r: &str) -> Rc<Vec<Arc<Re>>> {
    memo_patterns(format!("{}|{r}", language.as_str()), || build_local_receiver_type_patterns(language, r))
}

fn build_local_receiver_type_patterns(language: Language, r: &str) -> Vec<Arc<Re>> {
    let s = |t: &str| at(t, r);
    let ta = TYPE_ARGS;
    match language {
        Language::TypeScript | Language::JavaScript | Language::Tsx | Language::Jsx | Language::ArkTs => pats(
            &[
                s(r"\b@R@\b\s*=\s*new\s+([A-Za-z_$][\w.$]*)"),
                s(r"\b@R@\b\s*=\s*(?:inject|(?:this\.)?(?:[\w$#]+\.)*createInstance)\s*<\s*([A-Z][\w.$]*)[^;=(){}]*>\s*\((?![^;]*\)\s*(?:\?\.|\.|\[))"),
                s(r"\b@R@\b\s*=\s*(?:inject|(?:this\.)?(?:[\w$#]+\.)*createInstance)\s*\(\s*([A-Z][\w.$]*)(?![^;]*\)\s*(?:\?\.|\.|\[))"),
                s(r"\b@R@\b\s*:\s*([A-Z][\w.$]*(?:(?:\s*<[^;=(){}<>]*(?:<[^;=(){}<>]*>[^;=(){}<>]*)*>)?\s*\[[^\]]*\])?)"),
            ],
            "",
        ),
        Language::Python => pats(
            &[
                s(r"(?:^|;)\s*@R@\s*=(?!=)\s*([A-Z]\w*)\.objects\.(?:create|get|first|last|latest|earliest|get_by_natural_key)\s*\("),
                s(r"(?:^|;)\s*@R@\s*=(?!=)\s*([A-Z][\w.]*)\s*\((?![^\n]*,\s*$)"),
                s(r#"\b@R@\b\s*:\s*["']([A-Z][\w.]*)["']"#),
                s(r"\b@R@\b\s*:\s*([A-Z][\w.]*)"),
            ],
            "",
        ),
        Language::Java => pats(
            &[
                s(r"\b@R@\b\s*=\s*new\s+([A-Za-z_][\w.]*)"),
                // A package written before the type is kept (`org.app.Message message`): it, not a
                // same-named import, says which type the local is.
                format!(r"\b((?:[a-z_$][\w$]*\.)*[A-Z][\w.]*)\s*{ta}\s+{r}\b\s*[=;,)]"),
                format!(r"\bfor\s*\(\s*(?:final\s+)?((?:[a-z_$][\w$]*\.)*[A-Z][\w.]*)\s*{ta}\s+{r}\s*:"),
            ],
            "",
        ),
        Language::Kotlin => pats(&[s(r"\b@R@\b\s*=\s*([A-Z][\w.]*)\s*\("), s(r"\b@R@\b\s*:\s*([A-Z][\w.]*)")], ""),
        Language::CSharp => pats(
            &[
                s(r"\b@R@\b\s*=\s*new\s+([A-Za-z_][\w.]*)"),
                format!(r"\b([A-Z][\w.]*)\s*{ta}\??\s+{r}\b\s*[=;,)]"),
                format!(r"\bforeach\s*\(\s*([A-Z][\w.]*)\s*{ta}\??\s+{r}\s+in\b"),
            ],
            "",
        ),
        Language::ObjC => pats(
            &[
                s(r"\b([A-Z]\w*)\s*(?:<[^<>;]*>\s*|[A-Z][A-Z0-9_]*\s*\([^;{}]*?\)\s*)?\*\s*(?:(?:_Nullable|_Nonnull|__strong|__weak|__unsafe_unretained|const)\s+)*@R@\b(?!\s*\()"),
                s(r"\(\s*(?:(?:nullable|nonnull|__kindof)\s+)*([A-Z]\w*)\s*(?:<[^<>)]*>\s*|[A-Z][A-Z0-9_]*\s*\([^()]*\)\s*)?\*[^)]*\)\s*@R@\b"),
            ],
            "",
        ),
        Language::Swift => pats(
            &[
                format!(r"\b{r}\b\s*=\s*([A-Z][\w.]*)\s*{ta}\("),
                format!(r"\b(?:let|var)\s+{r}\b\s*=\s*(?:try[!?]\s*|(?:try|await)\s+)+([A-Z][\w.]*)\s*{ta}{SWIFT_UNCHAINED_CALL}"),
                s(r"\b@R@\b\s*:\s*([A-Z][\w.]*)"),
            ],
            "",
        ),
        Language::Rust => pats(
            &[s(r"\blet\s+(?:mut\s+)?@R@\b(?:\s*:[^=]+)?=\s*&?(?:mut\s+)?([A-Z][\w]*)"), s(r"\b@R@\s*:\s*&?(?:mut\s+)?([A-Z][\w]*)")],
            "",
        ),
        Language::Go => pats(
            &[
                s(r"\b@R@\b\s*:=\s*&?([A-Za-z_][\w.]*)\s*\{"),
                s(r"\bvar\s+@R@\s+\*?([A-Za-z_][\w.]*)"),
                s(r"(?:^|[(,])\s*@R@\s+\*?([a-z_]\w*)\s*(?:\[[^\]]*\])?\s*(?:[,)]|$)"),
                s(r"\b@R@\s+\*?([A-Z][\w.]*)"),
            ],
            "",
        ),
        Language::Ruby => pats(&[s(r"\b@R@\b\s*=\s*([A-Z][\w:]*)\.new\b")], ""),
        Language::Scala => pats(&[s(r"\b@R@\b\s*=\s*(?:new\s+)?([A-Z][\w.]*)"), s(r"\b@R@\b\s*:\s*([A-Z][\w.]*)")], ""),
        Language::Dart => pats(&[s(r"\b@R@\b\s*=\s*([A-Z][\w.]*)\s*\("), s(r"\b([A-Z][\w.]*)\s+@R@\b\s*[=;,)]")], ""),
        Language::Php => pats(
            &[s(r"\$?@R@\b\s*=\s*new\s+([A-Za-z_\\][\w\\]*)"), s(r"\b([A-Za-z_\\][\w\\]*)\s+&?\$@R@\b")],
            "",
        ),
        Language::Lua | Language::Luau => pats(
            &[
                s(r"\b@R@\b\s*=\s*([A-Z][\w]*)\.new\b"),
                s(r"\b@R@\b\s*=\s*([A-Z][\w]*)\s*\("),
                s(r#"\b@R@\b\s*:\s*([A-Z][\w.]*)(?![\w.]|\s*[({"'\[])"#),
            ],
            "",
        ),
        Language::R => pats(&[s(r"\b@R@\b\s*(?:<-|<<-|=)\s*([A-Z][\w.]*)\$new\b")], ""),
        Language::Pascal => pats(&[s(r"\b@R@\b\s*:\s*([A-Z][\w]*)"), s(r"\b@R@\b\s*:=\s*([A-Z][\w.]*)\.Create\b")], ""),
        Language::Cfml | Language::CfScript => {
            let mut v = pats(
                &[
                    s(r"\b@R@\b\s*=\s*new\s+([A-Za-z_][\w.]*)"),
                    s(r#"\b@R@\b\s*=\s*[Cc]reate[Oo]bject\s*\(\s*["']component["']\s*,\s*["']([\w.]+)["']"#),
                    s(r#"\b@R@\b\s*=\s*[Cc]reate[Oo]bject\s*\(\s*["']([\w.]+)["']\s*\)"#),
                    s(r"\b([A-Z][\w.]*)\s+@R@\b\s*[=;,)]"),
                ],
                "",
            );
            v.extend(pats(
                &[
                    s(r#"\bcfargument[^>\n]*\bname\s*=\s*["']@R@["'][^>\n]*\btype\s*=\s*["']([\w.]+)["']"#),
                    s(r#"\bcfargument[^>\n]*\btype\s*=\s*["']([\w.]+)["'][^>\n]*\bname\s*=\s*["']@R@["']"#),
                    s(r#"\b(?:cf)?property\b[^;\n]*\bname\s*=\s*["']@R@["'][^;\n]*\b(?:type|inject)\s*=\s*["']([\w.]+)["']"#),
                    s(r#"\b(?:cf)?property\b[^;\n]*\b(?:type|inject)\s*=\s*["']([\w.]+)["'][^;\n]*\bname\s*=\s*["']@R@["']"#),
                ],
                "i",
            ));
            v
        }
        _ => Vec::new(),
    }
}

fn php_property_type_patterns(r: &str) -> Rc<Vec<Arc<Re>>> {
    memo_patterns(format!("php-prop|{r}"), || {
        pats(
            &[
                at(r"\b(?:(?:private|protected|public|readonly|static|final)(?:\(set\))?\s+)+\??([A-Za-z_\\][\w\\]*)\s+&?\$@R@\b", r),
                at(r"\$this->@R@\b\s*=\s*new\s+([A-Za-z_\\][\w\\]*)", r),
            ],
            "",
        )
    })
}

// -- The incremental scan -----------------------------------------------------------------------

#[derive(Default, Clone)]
pub struct InferScanState {
    hi: usize,
    ans_idx: i64,
    ans_type: Option<String>,
    ans_raw: Option<String>,
}

memo_map!(InferScanMemo, String, Rc<RefCell<InferScanState>>);

/// The type a Go local was asserted to (`pkg.Type`) and where that package lives.
#[derive(Default)]
pub struct GoReceiverDecl {
    /// The found type as the declaration spells it.
    pub raw: Option<String>,
    pub asserted: Tri<GoProjectType>,
    pub declared: Tri<String>,
    /// The line the inference read the declaration on.
    pub line: Option<u32>,
    /// The file `raw` is written in, when it is not the call's: a callee's signature spells a
    /// result's type.
    pub file: Option<String>,
    /// The type of the call's or conversion's value a Go local's declaration gives it; `Null` for
    /// one with no project method.
    pub valued: Tri<GoTypeText>,
}

/// Infer a receiver's type from its local declaration or initializer in the enclosing function.
pub fn infer_local_receiver_type(receiver_name: &str, r: &UnresolvedRef, ctx: &Ctx, mut decl: Option<&mut GoReceiverDecl>) -> Option<String> {
    let mut scan_receiver = receiver_name.to_string();
    let mut component_scoped = false;
    if matches!(r.language, Language::Cfml | Language::CfScript) {
        if let Some(scoped) = re!(r"^(variables|this|local|arguments)\.(.+)$", "i").captures(receiver_name) {
            scan_receiver = scoped.text(2).unwrap_or("").to_string();
            let scope = scoped.text(1).unwrap_or("").to_lowercase();
            component_scoped = scope == "variables" || scope == "this";
        }
    }
    let mut php_property = false;
    if r.language == Language::Php {
        if let Some(scoped) = re!(r"^this->(.+)$").captures(receiver_name) {
            scan_receiver = scoped.text(1).unwrap_or("").to_string();
            component_scoped = true;
            php_property = true;
        }
    }
    if r.language == Language::Go {
        if let Some(d) = decl.as_deref_mut() {
            let written = go_asserted_local_type(&scan_receiver, r, ctx);
            let asserted = match &written {
                Some(w) => go_written_type(w, r, ctx),
                None => Tri::Undefined,
            };
            if !asserted.is_undefined() {
                let written = written.unwrap();
                d.raw = Some(written.clone());
                d.asserted = asserted;
                return Some(normalize_inferred_type_name(&written).unwrap_or(written));
            }
            let local = if re!(r"^[A-Za-z_]\w*$").is_match(&scan_receiver) { go_local_decl(&scan_receiver, r, ctx) } else { None };
            let ty = local.as_ref().and_then(|l| l.ty.clone());
            if let Some(ty) = ty {
                let named = go_named_type(&ty);
                match named {
                    None => {
                        d.declared = Tri::Null;
                        return None;
                    }
                    Some(named) => {
                        d.declared = Tri::Value(named.clone());
                        d.raw = Some(named.clone());
                        return Some(named[named.find('.').map_or(0, |i| i + 1)..].to_string());
                    }
                }
            }
            // A local its declaration in scope gives the result of a call or a conversion —
            // `r := newRing()`, `e, err := NewEngine(1)`, `v1 := router.Group(…)`,
            // `list := web.Users(names)` — has the type of that value, read as the indexed and
            // chained calls read it (#2782): what the function or method returns (the k-th result
            // of several), or the type converted to. The patterns below read none of these. A
            // value of a type from outside the project has no project method, whether an outside
            // package hands it out (`c, _ := net.Dial(…)`) or a project function returns it
            // (`*os.File`, `http.RoundTripper`, a `reflect.Value`): a guess by the method's name
            // took a namesake's (#1954).
            if let Some(valued) = local.as_ref().and_then(|l| go_call_value_type(l, &r.file_path, ctx)) {
                if !valued.is_undefined() {
                    let named = valued.as_value().and_then(|v| go_named_type(&v.text));
                    let Some(named) = named else {
                        d.valued = Tri::Null;
                        return None;
                    };
                    d.file = valued.as_value().map(|v| v.file.clone());
                    d.valued = valued;
                    d.raw = Some(named.clone());
                    return Some(named[named.find('.').map_or(0, |i| i + 1)..].to_string());
                }
            }
        }
    }
    let escaped = escape_regex(&scan_receiver);
    let patterns = if php_property { php_property_type_patterns(&escaped) } else { local_receiver_type_patterns(r.language, &escaped) };
    if patterns.is_empty() {
        return None;
    }
    let lines = ctx.get_file_lines(&r.file_path)?;
    if lines.is_empty() {
        return None;
    }
    let call_idx = (r.line as usize).saturating_sub(1).min(lines.len() - 1);
    let start_idx = if component_scoped { 0 } else { (enclosing_scope_start_line(r, ctx) as usize).saturating_sub(1) };
    let last_raw: RefCell<Option<String>> = RefCell::new(None);
    let match_line = |i: usize| -> Option<String> {
        let line = lines.get(i)?;
        if line.is_empty() || (line.len() > 10_000 && utf16_len(line) > 10_000) {
            return None;
        }
        for re in patterns.iter() {
            if let Some(m) = re.captures(line) {
                if let Some(t) = m.text(1).filter(|t| !t.is_empty()) {
                    let ty = if r.language == Language::Java { super::java_receiver::java_type_name(t) } else { normalize_inferred_type_name(t) };
                    if let Some(ty) = ty {
                        *last_raw.borrow_mut() = Some(t.to_string());
                        return Some(ty);
                    }
                }
            }
        }
        None
    };
    if !component_scoped {
        let key = format!("{}|{}|{}|{}", r.file_path, start_idx, r.language.as_str(), scan_receiver);
        let existing = ctx.memo::<InferScanMemo>().lookup(&key);
        let Some(state) = existing else {
            let mut i = call_idx as i64;
            while i >= start_idx as i64 {
                if let Some(ty) = match_line(i as usize) {
                    let raw = last_raw.borrow().clone();
                    ctx.memo::<InferScanMemo>().store(
                        key,
                        Rc::new(RefCell::new(InferScanState { hi: call_idx, ans_idx: i, ans_type: Some(ty.clone()), ans_raw: raw.clone() })),
                    );
                    if let Some(d) = decl {
                        d.raw = raw;
                        d.line = Some(i as u32 + 1);
                    }
                    return Some(ty);
                }
                i -= 1;
            }
            ctx.memo::<InferScanMemo>().store(key, Rc::new(RefCell::new(InferScanState { hi: call_idx, ans_idx: -1, ans_type: None, ans_raw: None })));
            return None;
        };
        let hi = state.borrow().hi;
        if call_idx >= hi {
            let mut i = call_idx;
            while i > hi {
                if let Some(ty) = match_line(i) {
                    let mut s = state.borrow_mut();
                    s.ans_idx = i as i64;
                    s.ans_type = Some(ty);
                    s.ans_raw = last_raw.borrow().clone();
                    break;
                }
                i -= 1;
            }
            let mut s = state.borrow_mut();
            s.hi = call_idx;
            if s.ans_idx < start_idx as i64 {
                return None;
            }
            if let Some(d) = decl {
                d.raw = s.ans_raw.clone();
                d.line = Some(s.ans_idx as u32 + 1);
            }
            return s.ans_type.clone();
        }
        let mut i = call_idx as i64;
        while i >= start_idx as i64 {
            if let Some(ty) = match_line(i as usize) {
                if let Some(d) = decl {
                    d.raw = last_raw.borrow().clone();
                    d.line = Some(i as u32 + 1);
                }
                return Some(ty);
            }
            i -= 1;
        }
        return None;
    }
    let mut i = call_idx as i64;
    while i >= start_idx as i64 {
        if let Some(ty) = match_line(i as usize) {
            return Some(ty);
        }
        i -= 1;
    }
    for i in call_idx + 1..lines.len() {
        if let Some(ty) = match_line(i) {
            return Some(ty);
        }
    }
    if php_property {
        return infer_php_assigned_property_type(&escaped, &lines, call_idx);
    }
    None
}

/// `$this->prop = $var;` — the type `$var` was declared with, scanning back from the assignment.
fn infer_php_assigned_property_type(escaped_prop: &str, lines: &[String], call_idx: usize) -> Option<String> {
    let assign = Re::cached(&format!(r"\$this->{escaped_prop}\b\s*=\s*\$(\w+)\b"), "");
    let assign_at = |i: usize| -> Option<String> {
        let line = lines.get(i)?;
        if line.is_empty() || (line.len() > 10_000 && utf16_len(line) > 10_000) {
            return None;
        }
        assign.captures(line).and_then(|m| m.text(1).map(|s| s.to_string()))
    };
    let mut found: Option<(usize, String)> = None;
    for i in (0..=call_idx.min(lines.len().saturating_sub(1))).rev() {
        if let Some(v) = assign_at(i) {
            found = Some((i, v));
            break;
        }
    }
    if found.is_none() {
        for i in call_idx + 1..lines.len() {
            if let Some(v) = assign_at(i) {
                found = Some((i, v));
                break;
            }
        }
    }
    let (assign_idx, var_name) = found?;
    let var_patterns = local_receiver_type_patterns(Language::Php, &escape_regex(&var_name));
    for i in (0..=assign_idx).rev() {
        let line = &lines[i];
        if !line.is_empty() && !(line.len() > 10_000 && utf16_len(line) > 10_000) {
            for re in var_patterns.iter() {
                if let Some(m) = re.captures(line) {
                    if let Some(t) = m.text(1).filter(|t| !t.is_empty()) {
                        if let Some(ty) = normalize_inferred_type_name(t) {
                            return Some(ty);
                        }
                    }
                }
            }
        }
        if !line.is_empty() && re!(r"\bfunction\b").is_match(line) {
            break;
        }
    }
    None
}

// -- Imported type files ---------------------------------------------------------------------------------

memo_map!(ImportedTypeFileMemo, String, Option<String>);

/// The file the (possibly dotted) type `written` is imported from, in a TS/JS reference's file.
pub fn ts_imported_type_file(written: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let name = re!(r"^[A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*)*").find(codegraph_types::jscompat::js_trim(written))?.as_str().to_string();
    let key = format!("{}\0{}", r.file_path, name);
    memoized::<ImportedTypeFileMemo>(ctx, key, || {
        let binding = name.split('.').next().unwrap_or("");
        if !ctx.get_import_mappings(&r.file_path, r.language).iter().any(|i| i.local_name == binding) {
            return None;
        }
        let mut probe = r.clone();
        probe.reference_name = name.clone();
        probe.reference_kind = ReferenceKind::Edge(EdgeKind::References);
        let node = crate::import_resolver::resolve_import(&probe, ctx).and_then(|i| ctx.node_by_id(&i.target_node_id))?;
        if node.kind != NodeKind::File && node.file_path != r.file_path && !is_merged_ambient_declaration(&node, ctx) {
            Some(node.file_path.clone())
        } else {
            None
        }
    })
}

/// The module `type_name` was imported from in the reference's file.
pub fn imported_type_source(type_name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    ctx.get_import_mappings(&r.file_path, r.language).iter().find(|i| i.local_name == type_name).map(|i| i.source.clone())
}

/// A Java field declared as `Type name;` in the class around the reference.
pub fn infer_java_field_receiver_type(receiver_name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let in_file = ctx.get_nodes_in_file(&r.file_path);
    if in_file.is_empty() {
        return None;
    }
    if r.language == Language::Java {
        return super::java_receiver::java_enclosing_field_type(receiver_name, r, &in_file);
    }
    let mut enclosing: Option<&NodeRef> = None;
    for n in in_file.iter() {
        if !matches!(n.kind, NodeKind::Class | NodeKind::Interface) || n.language != r.language {
            continue;
        }
        if n.start_line <= r.line && n.end_line >= r.line && enclosing.map_or(true, |e| n.start_line >= e.start_line) {
            enclosing = Some(n);
        }
    }
    let enclosing = enclosing?;
    // Kotlin indexes a `val` property as a `constant`, a `var` as a `field`, and a
    // companion-object property as a `variable`.
    let field = in_file.iter().find(|n| {
        (n.kind == NodeKind::Field || (r.language == Language::Kotlin && matches!(n.kind, NodeKind::Constant | NodeKind::Variable)))
            && n.name == receiver_name
            && n.language == r.language
            && n.start_line >= enclosing.start_line
            && n.end_line <= enclosing.end_line
    });
    // Kotlin keeps no signature on a property, and a primary-constructor property (`class A(private
    // val repo: Repo)`) is no node at all, so the declared type has to be read from the declaration
    // itself.
    if r.language == Language::Kotlin {
        return super::kotlin_receiver::infer_kotlin_property_type(receiver_name, field.map(|f| &**f), enclosing, &in_file, r, ctx);
    }
    field_signature_type(field?)
}

/// The declared type of a Java/Kotlin field node, read from its signature ("<TypeName> <fieldName>",
/// set by the extractor): generics, a dotted package and array/varargs markers stripped — for Java,
/// in `java_type_name`'s form, which keeps a written package and outer class. `None` for a primitive
/// or a field without a signature.
pub fn field_signature_type(field: &Node) -> Option<String> {
    let sig = field.signature.as_ref()?;
    let before = &sig[..sig.rfind(&field.name).unwrap_or(sig.len())];
    let type_raw = codegraph_types::jscompat::js_trim(before);
    if type_raw.is_empty() {
        return None;
    }
    if field.language == Language::Java {
        return super::java_receiver::java_type_name(type_raw);
    }
    let no_generics = codegraph_types::jscompat::js_trim(&re!(r"<[^>]*>", "g").replace_all_str(type_raw, "")).to_string();
    let no_array = re!(r"\.\.\.$").replace_first(&re!(r"\[\s*\]", "g").replace_all_str(&no_generics, ""), |_| String::new());
    let no_array = codegraph_types::jscompat::js_trim(&no_array).to_string();
    let last = re!(r"[.\s]+").split(&no_array).into_iter().filter(|s| !s.is_empty()).last()?;
    if !re!(r"^[A-Z]").is_match(last) {
        return None;
    }
    Some(last.to_string())
}

// -- ESM awaited call types ---------------------------------------------------------------------------------

#[derive(Clone)]
pub struct AwaitedType {
    pub name: Option<String>,
    pub file_path: String,
}

struct Scope {
    start: i64,
    end: usize,
    parent: i64,
}

struct AwaitedFile {
    code: Vec<u16>,
    ready: bool,
    offsets: Vec<usize>,
    names: HashSet<String>,
    scopes: Vec<Scope>,
    declarations: HashMap<String, Vec<(usize, usize)>>,
    source: String,
}

type AwaitedFiles = Bounded<Option<Rc<RefCell<AwaitedFile>>>, 256>;
type AwaitedTypes = Bounded<Option<AwaitedType>, 8192>;

/// `const x = await make(); x.run()` — the class `make()` is declared to return, as a type to resolve `run` on.
pub fn infer_esm_awaited_call_type(receiver_name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<AwaitedType> {
    if !is_js_ident(receiver_name) {
        return None;
    }
    let file = ctx.memo::<AwaitedFiles>().get_or(&r.file_path, false, || {
        let source = ctx.read_file(&r.file_path).map(|s| s.to_string()).unwrap_or_default();
        let names: HashSet<String> =
            re!(r"\b(?:const|let|var)\s+([\w$]+)\s*=\s*await\s+[\w$]+\s*\(", "g").captures_iter(&source).filter_map(|m| m.text(1).map(|s| s.to_string())).collect();
        if names.is_empty() {
            None
        } else {
            Some(Rc::new(RefCell::new(AwaitedFile { code: Vec::new(), ready: false, offsets: Vec::new(), names, scopes: Vec::new(), declarations: HashMap::new(), source })))
        }
    })?;
    if !file.borrow().names.contains(receiver_name) {
        return None;
    }
    if !file.borrow().ready {
        let mut f = file.borrow_mut();
        let code_text = blank_string_contents(&strip_comments_for_regex(&f.source, "typescript"), None);
        let names: HashSet<String> = re!(r"\b(?:const|let|var)\s+([\w$]+)\s*=\s*await\s+[\w$]+\s*\(", "g")
            .captures_iter(&code_text)
            .filter_map(|m| m.text(1).map(|s| s.to_string()))
            .collect();
        let units: Vec<u16> = code_text.encode_utf16().collect();
        let bu = super::object_literal::ByteUnits::new(&code_text);
        let mut offsets = vec![0usize];
        let mut scopes = vec![Scope { start: -1, end: units.len(), parent: -1 }];
        let mut stack: Vec<usize> = vec![0];
        for (i, &c) in units.iter().enumerate() {
            if c == 10 {
                offsets.push(i + 1);
            }
            if c == 123 {
                scopes.push(Scope { start: i as i64, end: units.len(), parent: *stack.last().unwrap() as i64 });
                stack.push(scopes.len() - 1);
            } else if c == 125 && stack.len() > 1 {
                let s = stack.pop().unwrap();
                scopes[s].end = i;
            }
        }
        let mut declarations: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
        for m in re!(r"\b(?:const|let|var)\s+([\w$]+)\s*=\s*", "g").captures_iter(&code_text) {
            let name = m.text(1).unwrap_or("").to_string();
            if !names.contains(&name) {
                continue;
            }
            let g = m.get(0).unwrap();
            declarations.entry(name).or_default().push((bu.at(g.start()), utf16_len(g.as_str())));
        }
        f.code = units;
        f.ready = true;
        f.names = names;
        f.offsets = offsets;
        f.scopes = scopes;
        f.declarations = declarations;
        if !f.names.contains(receiver_name) {
            return None;
        }
    }
    let key = format!("{}|{}|{}|{}", r.file_path, r.line, r.column, receiver_name);
    ctx.memo::<AwaitedTypes>().get_or(&key, false, || resolve_awaited_call_type(receiver_name, &file.borrow(), r, ctx))
}

fn resolve_awaited_call_type(receiver_name: &str, file: &AwaitedFile, r: &UnresolvedRef, ctx: &Ctx) -> Option<AwaitedType> {
    let unknown = || Some(AwaitedType { name: None, file_path: r.file_path.clone() });
    let end = file.offsets.get(r.line as usize - 1).copied().unwrap_or(file.code.len()) + r.column as usize;
    let code_len = file.code.len();
    let text = |a: usize, b: usize| String::from_utf16_lossy(&file.code[a.min(code_len)..b.min(code_len).max(a.min(code_len))]);
    let _code = text(0, end);
    let escaped = escape_regex(receiver_name);
    let scope_at = |offset: i64| -> usize {
        let (mut lo, mut hi) = (0usize, file.scopes.len());
        while lo + 1 < hi {
            let mid = (lo + hi) >> 1;
            if file.scopes[mid].start < offset {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        while lo > 0 && (file.scopes[lo].end as i64) < offset {
            lo = file.scopes[lo].parent as usize;
        }
        lo
    };
    let visible_at = |declaration: usize, use_: usize| -> bool {
        let ancestor = scope_at(declaration as i64);
        let mut scope = scope_at(use_ as i64) as i64;
        while scope >= 0 {
            if scope as usize == ancestor {
                return true;
            }
            scope = file.scopes[scope as usize].parent;
        }
        false
    };
    let binding = file.declarations.get(receiver_name)?.iter().rev().find(|(index, _)| *index < end && visible_at(*index, end))?;
    let init = text(binding.0 + binding.1, end);
    if !re!(r"^await\b").is_match(&init) {
        return None;
    }
    let call = re!(r"^await\s+([A-Za-z_$][\w$]*)\s*\(").captures(&init)?;
    let bytes = init.as_bytes();
    let mut depth = 1;
    let mut call_end = call.get(0).unwrap().end();
    while call_end < bytes.len() && depth != 0 {
        if bytes[call_end] == b'(' {
            depth += 1;
        } else if bytes[call_end] == b')' {
            depth -= 1;
        }
        call_end += 1;
    }
    if depth != 0 {
        return unknown();
    }
    let tail = &init[call_end..];
    if !re!(r"^[ \t]*(?:;|\r?\n(?![ \t]*[.(\[?]))").is_match(tail) {
        return unknown();
    }
    let rest = tail;
    if Re::cached(&format!(r"\b(?:const|let|var|function|class)\s+(?:{escaped}\b|\{{[^}}]*\b{escaped}\b)"), "").is_match(rest)
        || Re::cached(&format!(r"\b{escaped}\s*=(?!=)"), "").is_match(rest)
        || has_parameter_binding(rest, &escaped)
    {
        return unknown();
    }
    let binding_line = text(0, binding.0).matches('\n').count() as u32 + 1;
    let mut binding_ref = r.clone();
    binding_ref.line = binding_line;
    binding_ref.column = (binding.0 - file.offsets[binding_line as usize - 1]) as u32;
    let callee = call.text(1).unwrap_or("").to_string();
    let callee_escaped = escape_regex(&callee);
    if ctx.get_nodes_in_file(&r.file_path).iter().any(|n| {
        matches!(n.kind, NodeKind::Function | NodeKind::Method)
            && n.start_line <= binding_line
            && n.end_line >= binding_line
            && n.signature.as_ref().is_some_and(|sig| has_parameter_binding(&format!("{sig} {{"), &callee_escaped))
    }) {
        return unknown();
    }
    let imported = ctx.get_import_mappings(&r.file_path, r.language).iter().any(|m| m.local_name == callee);
    let declaring: Option<NodeRef> = if imported {
        if import_shadowed_at(&callee, &binding_ref, ctx) {
            return unknown();
        }
        let mut probe = binding_ref.clone();
        probe.reference_name = callee.clone();
        probe.reference_kind = ReferenceKind::Edge(EdgeKind::Calls);
        crate::import_resolver::resolve_import(&probe, ctx).and_then(|i| ctx.node_by_id(&i.target_node_id))
    } else {
        let local: Vec<NodeRef> = ctx
            .get_nodes_by_name(&callee)
            .iter()
            .filter(|n| n.kind == NodeKind::Function && n.file_path == r.file_path && is_esm_family(n.language) && is_lexically_reachable(n, &binding_ref, ctx))
            .cloned()
            .collect();
        if local.len() == 1 { Some(local[0].clone()) } else { None }
    };
    let Some(declaring) = declaring.filter(|d| d.kind == NodeKind::Function && d.signature.is_some()) else { return unknown() };
    if !imported {
        let before_binding = text(0, binding.0);
        let shadows = Re::cached(&format!(r"\b(?:const|let|var)\s+{callee_escaped}\b"), "g");
        let bu = super::object_literal::ByteUnits::new(&before_binding);
        for shadow in shadows.captures_iter(&before_binding) {
            let index = bu.at(shadow.get(0).unwrap().start());
            if !visible_at(index, binding.0) {
                continue;
            }
            let line = text(0, index).matches('\n').count() as u32 + 1;
            if line != declaring.start_line || (index - file.offsets[line as usize - 1]) as u32 > declaring.start_column {
                return unknown();
            }
        }
    }
    let signature = declaring.signature.as_ref().unwrap();
    let after_paren = &signature[signature.rfind(')').map_or(0, |i| i + 1)..];
    let annotation = re!(r"^\s*:\s*([\s\S]+)$").captures(after_paren).and_then(|c| c.text(1).map(|s| codegraph_types::jscompat::js_trim(s).to_string()));
    let Some(annotation) = annotation.filter(|a| !a.is_empty()) else { return unknown() };
    let returned = re!(r"^Promise\s*<\s*([\w$]+)\s*>$").captures(&annotation).and_then(|c| c.text(1).map(|s| s.to_string())).unwrap_or(annotation);
    if !is_js_ident(&returned) {
        return unknown();
    }
    if TS_PRIMITIVE_TYPES.contains(returned.as_str()) {
        return Some(AwaitedType { name: Some(returned), file_path: declaring.file_path.clone() });
    }
    let mut type_ref = binding_ref.clone();
    type_ref.from_node_id = declaring.id.clone();
    type_ref.file_path = declaring.file_path.clone();
    type_ref.language = declaring.language;
    type_ref.line = declaring.start_line;
    type_ref.column = declaring.start_column;
    type_ref.reference_name = returned.clone();
    type_ref.reference_kind = ReferenceKind::Edge(EdgeKind::References);
    let type_import = ctx.get_import_mappings(&declaring.file_path, declaring.language).iter().any(|m| m.local_name == returned);
    let resolved = if type_import { crate::import_resolver::resolve_import(&type_ref, ctx) } else { None };
    let imported_type = resolved.as_ref().and_then(|x| ctx.node_by_id(&x.target_node_id));
    let type_node: Option<NodeRef> = if resolved.is_some() {
        imported_type.filter(|t| !is_merged_ambient_declaration(t, ctx))
    } else {
        ctx.get_nodes_by_name(&returned)
            .iter()
            .find(|n| n.file_path == declaring.file_path && is_esm_family(n.language) && matches!(n.kind, NodeKind::Class | NodeKind::Interface))
            .cloned()
    };
    match type_node {
        Some(t) if matches!(t.kind, NodeKind::Class | NodeKind::Interface) => Some(AwaitedType { name: Some(t.name.clone()), file_path: t.file_path.clone() }),
        _ => unknown(),
    }
}

#[allow(dead_code)]
fn _unused(_: &Node) {
    let _ = (is_lexically_reachable as fn(&Node, &UnresolvedRef, &Ctx) -> bool, is_this_call_in_own_file as fn(&Node, &UnresolvedRef, &Ctx) -> bool);
}
