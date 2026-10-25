//! Which of a class's same-named methods overrides an overload of a type it implements or
//! extends (#2663) — port of `src/resolution/override-parameters.ts`.
//!
//! Java, Kotlin, C# and Scala make an override repeat the parameter list of the method it
//! overrides, so of two `putTairValue`s, `putTairValue(String)` is the one a call to
//! `ExampleService.putTairValue(String)` runs. Parameter lists come from the stored signatures;
//! C# methods store none, so theirs is read from the declaration.

use crate::ctx::Ctx;
use crate::type_parameters::type_parameters_in_scope;
use crate::types::NodeRef;
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::{is_js_space, js_trim, utf16_from};
use codegraph_types::{Language, Node};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// The languages whose overrides repeat the parameter list of the method they override.
pub fn is_parameter_override_language(language: Language) -> bool {
    matches!(language, Language::Java | Language::Kotlin | Language::CSharp | Language::Scala)
}

/// How far a C# declaration is read for its parameter list.
const CSHARP_HEAD_LINES: u32 = 40;

/// C#'s keyword for each BCL type it aliases.
fn csharp_keyword_type(w: &str) -> Option<&'static str> {
    Some(match w {
        "String" => "string",
        "Object" => "object",
        "Boolean" => "bool",
        "Byte" => "byte",
        "SByte" => "sbyte",
        "Char" => "char",
        "Decimal" => "decimal",
        "Double" => "double",
        "Single" => "float",
        "Int16" => "short",
        "UInt16" => "ushort",
        "Int32" => "int",
        "UInt32" => "uint",
        "Int64" => "long",
        "UInt64" => "ulong",
        "IntPtr" => "nint",
        "UIntPtr" => "nuint",
        _ => return None,
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Fit {
    Same,
    Maybe,
    Differs,
}

/// A filter for one run of the override pass: `filter(base, methods)` keeps the methods that can
/// override `base`, in their order (overrideFilter).
pub struct OverrideFilter<'a> {
    ctx: &'a Ctx,
    types: RefCell<HashMap<String, Option<Rc<Vec<String>>>>>,
    scopes: RefCell<HashMap<String, Rc<HashSet<String>>>>,
}

impl<'a> OverrideFilter<'a> {
    pub fn new(ctx: &'a Ctx) -> Self {
        OverrideFilter { ctx, types: RefCell::new(HashMap::new()), scopes: RefCell::new(HashMap::new()) }
    }

    fn types_of(&self, m: &Node) -> Option<Rc<Vec<String>>> {
        if let Some(found) = self.types.borrow().get(&m.id) {
            return found.clone();
        }
        let found = parameter_types(m, self.ctx).map(Rc::new);
        self.types.borrow_mut().insert(m.id.clone(), found.clone());
        found
    }

    fn scope_of(&self, m: &Node) -> Rc<HashSet<String>> {
        if let Some(found) = self.scopes.borrow().get(&m.id) {
            return Rc::clone(found);
        }
        let found = Rc::new(type_parameters_in_scope(m, self.ctx));
        self.scopes.borrow_mut().insert(m.id.clone(), Rc::clone(&found));
        found
    }

    pub fn filter(&self, base: &Node, methods: &[NodeRef]) -> Vec<NodeRef> {
        let want = if is_parameter_override_language(base.language) { self.types_of(base) } else { None };
        let Some(want) = want else { return methods.to_vec() };
        let mut same: Vec<NodeRef> = Vec::new();
        let mut maybe: Vec<NodeRef> = Vec::new();
        for m in methods {
            let have = if is_parameter_override_language(m.language) { self.types_of(m) } else { None };
            let Some(have) = have else {
                maybe.push(Rc::clone(m));
                continue;
            };
            if have.len() != want.len() {
                continue;
            }
            if m.language != base.language {
                maybe.push(Rc::clone(m));
                continue;
            }
            let mut fit = Fit::Same;
            for i in 0..want.len() {
                if fit == Fit::Differs {
                    break;
                }
                let f = compare_types(&want[i], || self.scope_of(base), &have[i], base.language);
                if f != Fit::Same {
                    fit = f;
                }
            }
            match fit {
                Fit::Same => same.push(Rc::clone(m)),
                Fit::Maybe => maybe.push(Rc::clone(m)),
                Fit::Differs => {}
            }
        }
        if same.is_empty() {
            maybe
        } else {
            same
        }
    }
}

fn compare_types(a: &str, a_scope: impl FnOnce() -> Rc<HashSet<String>>, b: &str, language: Language) -> Fit {
    if a == b {
        return Fit::Same;
    }
    if loose_type(a, language) == loose_type(b, language) {
        return Fit::Maybe;
    }
    if fits_with_type_parameters(a, &a_scope(), b) {
        Fit::Maybe
    } else {
        Fit::Differs
    }
}

/// Past this length a type is not matched against a pattern.
const MAX_PATTERN_TYPE_LENGTH: usize = 200;

fn len16(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Whether `ty` is `pattern` with each type parameter it names filled in.
fn fits_with_type_parameters(pattern: &str, type_parameters: &HashSet<String>, ty: &str) -> bool {
    if type_parameters.is_empty() {
        return false;
    }
    if len16(pattern) > MAX_PATTERN_TYPE_LENGTH || len16(ty) > MAX_PATTERN_TYPE_LENGTH {
        return type_parameters.iter().any(|name| Re::cached(&format!(r"(?<![\w$]){}(?![\w$])", escape_regex(name)), "").is_match(pattern));
    }
    let mut named = false;
    let mut source = String::new();
    let mut last = 0;
    for m in re!(r"[A-Za-z_$][\w$]*", "g").captures_iter(pattern) {
        let g = m.get(0).unwrap();
        source.push_str(&escape_regex(&pattern[last..g.start()]));
        if type_parameters.contains(g.as_str()) {
            named = true;
            source.push_str(".+");
        } else {
            source.push_str(&escape_regex(g.as_str()));
        }
        last = g.end();
    }
    source.push_str(&escape_regex(&pattern[last..]));
    named && Re::cached(&format!("^{source}$"), "").is_match(ty)
}

/// A type without its type arguments and nullable marks: `List<String>?` is `List`.
fn loose_type(ty: &str, language: Language) -> String {
    let args = if language == Language::Scala { re!(r"\[[^\[\]]*\]", "g") } else { re!(r"<[^<>]*>", "g") };
    let mut t = ty.to_string();
    loop {
        let next = args.replace_all_str(&t, "");
        if next == t {
            break;
        }
        t = next;
    }
    t.replace('?', "")
}

/// The parameter types `method` declares, each normalized; `None` when they can't be read.
fn parameter_types(method: &Node, ctx: &Ctx) -> Option<Vec<String>> {
    let list = parameter_list(method, ctx)?;
    let mut types = Vec::new();
    for param in split_parameters(&list) {
        match parameter_type(&param, method.language) {
            ParamType::Receiver => continue,
            ParamType::Unknown => return None,
            ParamType::Type(t) => types.push(normalize_type(&t, method.language)),
        }
    }
    Some(types)
}

fn chars(s: &str) -> Vec<char> {
    s.chars().collect()
}

fn text(c: &[char]) -> String {
    c.iter().collect()
}

/// The text of `method`'s parameter list, without its parentheses.
fn parameter_list(method: &Node, ctx: &Ctx) -> Option<String> {
    if method.language == Language::CSharp {
        return csharp_parameter_list(method, ctx);
    }
    let sig = js_trim(method.signature.as_deref()?);
    if sig.is_empty() {
        return None;
    }
    let sig = chars(sig);
    if method.language == Language::Java {
        // The return type, then the list: `Object (List<String> tairKey, double count)`.
        let groups = paren_groups(&sig);
        let last = groups.last()?;
        return (last.1 == sig.len() - 1).then(|| text(&sig[last.0 + 1..last.1]));
    }
    // Kotlin and Scala: the list, then the return type. A Scala `def size: Int` takes none.
    if method.language == Language::Scala && sig.first() == Some(&':') {
        return Some(String::new());
    }
    let first = *paren_groups(&sig).first()?;
    (first.0 == 0).then(|| text(&sig[1..first.1]))
}

fn file_lines(file_path: &str, ctx: &Ctx) -> Option<Rc<Vec<String>>> {
    ctx.get_file_lines(file_path)
        .or_else(|| ctx.read_file(file_path).map(|s| Rc::new(re!(r"\r?\n").split(&s).into_iter().map(String::from).collect())))
}

/// The parameter list of a C# method, read from its declaration: past its attribute sections,
/// the `( … )` after its name, with comments and string literals blanked.
fn csharp_parameter_list(method: &Node, ctx: &Ctx) -> Option<String> {
    let lines = file_lines(&method.file_path, ctx)?;
    if method.start_line < 1 || method.start_line as usize > lines.len() {
        return None;
    }
    let end = (lines.len() as u32).min(method.end_line.max(method.start_line)).min(method.start_line - 1 + CSHARP_HEAD_LINES) as usize;
    let first = &lines[method.start_line as usize - 1];
    let mut parts: Vec<&str> = vec![utf16_from(first, method.start_column as usize)];
    if (method.start_line as usize) < end {
        parts.extend(lines[method.start_line as usize..end].iter().map(String::as_str));
    }
    let head = chars(&blank_csharp_trivia(&parts.join("\n")));
    let mut at = 0usize;
    loop {
        while at < head.len() && is_js_space(head[at]) {
            at += 1;
        }
        if head.get(at) != Some(&'[') {
            break;
        }
        at = closing_bracket(&head, at)? + 1;
    }
    let head_text = text(&head);
    let byte_at: usize = head[..at.min(head.len())].iter().map(|c| c.len_utf8()).sum();
    let name = Re::cached(&format!(r"(?<![\w@])@?{}\s*(?:<[^<>()]*>)?\s*\(", escape_regex(&method.name)), "g");
    let found = name.captures_at(&head_text, byte_at)?;
    let g = found.get(0)?;
    let open = head_text[..g.end()].chars().count() - 1;
    let close = closing_bracket(&head, open)?;
    Some(text(&head[open + 1..close]))
}

/// C# source with its comments and the contents of its string and character literals blanked,
/// newlines kept.
fn blank_csharp_trivia(source: &str) -> String {
    let t = chars(source);
    let blank = |s: &[char]| -> String { s.iter().map(|&c| if c == '\n' { '\n' } else { ' ' }).collect() };
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    while i < t.len() {
        let c = t[i];
        if c == '/' && matches!(t.get(i + 1), Some('/') | Some('*')) {
            let line = t[i + 1] == '/';
            let close = if line { t[i..].iter().position(|&x| x == '\n').map(|p| p + i) } else { find_seq(&t, &['*', '/'], i + 2) };
            let stop = match close {
                None => t.len(),
                Some(c) => {
                    if line {
                        c
                    } else {
                        c + 2
                    }
                }
            };
            out.push_str(&blank(&t[i..stop]));
            i = stop;
            continue;
        }
        if c == '"' || c == '\'' {
            // `@"…"` and `$@"…"` escape a quote by doubling it; the rest by a backslash.
            let verbatim = c == '"' && (i >= 1 && t[i - 1] == '@' || (i >= 2 && t[i - 2] == '@' && t[i - 1] == '$'));
            let mut j = i + 1;
            while j < t.len() {
                if if verbatim { t[j] == '"' && t.get(j + 1) == Some(&'"') } else { t[j] == '\\' } {
                    j += 2;
                    continue;
                }
                if t[j] == c || (!verbatim && t[j] == '\n') {
                    break;
                }
                j += 1;
            }
            out.push(c);
            out.push_str(&blank(&t[i + 1..j.min(t.len())]));
            if let Some(&end) = t.get(j) {
                out.push(end);
            }
            i = j + 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

fn find_seq(t: &[char], seq: &[char], from: usize) -> Option<usize> {
    (from..t.len().saturating_sub(seq.len() - 1)).find(|&i| t[i..].starts_with(seq))
}

enum ParamType {
    /// Java's receiver, `void m(Foo this, int x)`: no parameter.
    Receiver,
    Unknown,
    Type(String),
}

/// The type text a parameter declares, as written.
fn parameter_type(param: &str, language: Language) -> ParamType {
    if language == Language::CSharp {
        return csharp_parameter_type(param).map_or(ParamType::Unknown, ParamType::Type);
    }
    let stripped = strip_annotations(param);
    let bare = js_trim(&stripped);
    if language == Language::Java {
        // `final List<String> lines`, `String... parts`, a C-style `int data[]`.
        let unfinal = re!(r"\bfinal\b", "g").replace_all_str(bare, " ");
        let Some(m) = re!(r"^([\s\S]*?)([A-Za-z_$][\w$]*)((?:\s*\[\s*\])*)$").captures(js_trim(&unfinal)) else { return ParamType::Unknown };
        let head = m.text(1).unwrap_or("");
        if js_trim(head).is_empty() {
            return ParamType::Unknown;
        }
        if m.text(2) == Some("this") {
            return ParamType::Receiver;
        }
        return ParamType::Type(format!("{head}{}", re!(r"\s+", "g").replace_all_str(m.text(3).unwrap_or(""), "")));
    }
    // Kotlin and Scala: `vararg keys: String`, `count: Double = 1.0`, `f: => Unit`.
    let b = chars(bare);
    let Some(colon) = top_level_index(&b, |i, t| t[i] == ':') else { return ParamType::Unknown };
    let mut ty: Vec<char> = b[colon + 1..].to_vec();
    if let Some(assign) = top_level_index(&ty, |i, t| t[i] == '=' && t.get(i + 1) != Some(&'>')) {
        ty.truncate(assign);
    }
    let ty = text(&ty);
    if js_trim(&ty).is_empty() {
        return ParamType::Unknown;
    }
    if re!(r"\bvararg\b").is_match(&text(&b[..colon])) {
        ParamType::Type(format!("vararg {ty}"))
    } else {
        ParamType::Type(ty)
    }
}

/// A C# parameter's type: past its attributes and the modifiers that are no part of it (`this`,
/// `params`, `scoped`), keeping `ref`, `out` and `in`, and before its name and default value.
fn csharp_parameter_type(param: &str) -> Option<String> {
    let mut p = chars(js_trim(param));
    while p.first() == Some(&'[') {
        let close = closing_bracket(&p, 0)?;
        p = chars(js_trim(&text(&p[close + 1..])));
    }
    let is = |c: Option<&char>, set: &str| c.is_some_and(|c| set.contains(*c));
    if let Some(assign) = top_level_index(&p, |i, t| t[i] == '=' && !is(i.checked_sub(1).and_then(|k| t.get(k)), "=!<>") && !is(t.get(i + 1), "=>")) {
        p = chars(js_trim(&text(&p[..assign])));
    }
    let p = text(&p);
    let m = re!(r"^([\s\S]*?\S)\s+@?[A-Za-z_]\w*$").captures(&p)?;
    let mut ty = js_trim(m.text(1).unwrap_or("")).to_string();
    let mut kept: Vec<String> = Vec::new();
    loop {
        let Some((word, len)) = re!(r"^(this|params|scoped|ref|out|in|readonly)\s+").captures(&ty).map(|md| (md.text(1).unwrap_or("").to_string(), md.text(0).unwrap_or("").len())) else { break };
        if word != "this" && word != "params" && word != "scoped" {
            kept.push(word);
        }
        ty = ty[len..].to_string();
    }
    if ty.is_empty() {
        return None;
    }
    kept.push(ty);
    Some(kept.join(" "))
}

/// A parameter type as the languages compare it: qualifiers dropped, Java's varargs an array,
/// C#'s BCL names its keywords, and whitespace kept only between two words.
fn normalize_type(ty: &str, language: Language) -> String {
    let collapsed = re!(r"\s+", "g").replace_all_str(ty, " ");
    let mut t = js_trim(&collapsed).to_string();
    if language == Language::Java {
        t = t.replace("...", "[]");
    }
    t = re!(r"\b[A-Za-z_]\w*\s*::\s*", "g").replace_all_str(&t, "");
    t = re!(r"\b(?:[A-Za-z_$][\w$]*\s*\.\s*)+(?=[A-Za-z_$])", "g").replace_all_str(&t, "");
    if language == Language::CSharp {
        t = re!(r"\b[A-Z][A-Za-z0-9]*\b", "g").replace_all(&t, |c| Some(csharp_keyword_type(&c[0]).map_or_else(|| c[0].to_string(), String::from)));
    }
    re!(r" (?![\w$])|(?<![\w$]) ", "g").replace_all_str(&t, "")
}

/// Java, Kotlin and Scala annotations taken out of a parameter.
fn strip_annotations(source: &str) -> String {
    let t = chars(source);
    let mut out = String::new();
    let mut i = 0;
    while i < t.len() {
        let name = if t[i] == '@' {
            re!(r"^@\s*[A-Za-z_$][\w$]*(?:\s*:\s*[A-Za-z_$][\w$]*)?(?:\s*\.\s*[A-Za-z_$][\w$]*)*").captures(&text(&t[i..])).map(|m| m.text(0).unwrap_or("").chars().count())
        } else {
            None
        };
        let Some(name_len) = name else {
            out.push(t[i]);
            i += 1;
            continue;
        };
        let mut next = i + name_len;
        let mut open = next;
        while open < t.len() && is_js_space(t[open]) {
            open += 1;
        }
        if t.get(open) == Some(&'(') {
            next = closing_bracket(&t, open).map_or(t.len(), |c| c + 1);
        }
        out.push(' ');
        i = next;
    }
    out
}

/// The parameters of a list, at its top-level commas.
fn split_parameters(list: &str) -> Vec<String> {
    let l = chars(list);
    let mut params = Vec::new();
    let mut start = 0;
    loop {
        let comma = top_level_index(&l[start..], |i, t| t[i] == ',');
        let end = comma.map_or(l.len(), |c| start + c);
        let param = text(&l[start..end]);
        let param = js_trim(&param);
        if !param.is_empty() {
            params.push(param.to_string());
        }
        match comma {
            None => return params,
            Some(c) => start += c + 1,
        }
    }
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The first index outside brackets and string literals where `at` holds. A `<` opens type
/// arguments when written right after a name (`List<`); the `>` of `->` and `=>` closes none.
fn top_level_index(t: &[char], at: impl Fn(usize, &[char]) -> bool) -> Option<usize> {
    let mut depth = 0i64;
    let mut angles = 0i64;
    let mut i = 0;
    while i < t.len() {
        let c = t[i];
        if c == '"' || c == '\'' {
            i = closing_quote(t, i) + 1;
            continue;
        }
        if depth == 0 && angles == 0 && at(i, t) {
            return Some(i);
        }
        let prev = if i > 0 { Some(t[i - 1]) } else { None };
        if c == '(' || c == '[' || c == '{' {
            depth += 1;
        } else if c == ')' || c == ']' || c == '}' {
            depth = (depth - 1).max(0);
        } else if c == '<'
            && prev.is_some_and(|p| is_word(p) || matches!(p, '$' | '?' | ']' | '>'))
            && t.get(i + 1) != Some(&'<')
            && t.get(i + 1) != Some(&'=')
        {
            angles += 1;
        } else if c == '>' && angles > 0 && prev != Some('-') && prev != Some('=') {
            angles -= 1;
        }
        i += 1;
    }
    None
}

/// The top-level `( … )` groups of `t`, as (open, close) pairs; string literals skipped.
fn paren_groups(t: &[char]) -> Vec<(usize, usize)> {
    let mut groups = Vec::new();
    let mut i = 0;
    while i < t.len() {
        let c = t[i];
        if c == '"' || c == '\'' {
            i = closing_quote(t, i);
        } else if c == '(' {
            let Some(close) = closing_bracket(t, i) else { return groups };
            groups.push((i, close));
            i = close;
        }
        i += 1;
    }
    groups
}

/// The index of the bracket closing the one at `open`; string literals skipped.
fn closing_bracket(t: &[char], open: usize) -> Option<usize> {
    let mut depth = 0i64;
    let mut i = open;
    while i < t.len() {
        let c = t[i];
        if c == '"' || c == '\'' {
            i = closing_quote(t, i);
        } else if c == '(' || c == '[' || c == '{' {
            depth += 1;
        } else if c == ')' || c == ']' || c == '}' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// The index of the quote closing the literal opening at `open`, or the text's end.
fn closing_quote(t: &[char], open: usize) -> usize {
    let quote = t[open];
    let mut j = open + 1;
    while j < t.len() && t[j] != quote {
        j += if t[j] == '\\' { 2 } else { 1 };
    }
    j.min(t.len() - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameter_lists_split_at_top_level_commas() {
        assert_eq!(split_parameters("Map<String, Integer> m, int x"), vec!["Map<String, Integer> m", "int x"]);
        assert_eq!(split_parameters(""), Vec::<String>::new());
    }

    #[test]
    fn types_normalize_as_the_languages_compare_them() {
        assert_eq!(normalize_type("java.util.List<java.lang.String>", Language::Java), "List<String>");
        assert_eq!(normalize_type("String...", Language::Java), "String[]");
        assert_eq!(normalize_type("out Int32", Language::CSharp), "out int");
    }

    #[test]
    fn java_and_kotlin_parameter_types() {
        assert!(matches!(parameter_type("final List<String> lines", Language::Java), ParamType::Type(t) if t == "List<String> "));
        assert!(matches!(parameter_type("Foo this", Language::Java), ParamType::Receiver));
        assert!(matches!(parameter_type("vararg keys: String", Language::Kotlin), ParamType::Type(t) if t == "vararg  String"));
        assert!(matches!(parameter_type("count: Double = 1.0", Language::Kotlin), ParamType::Type(t) if t == " Double "));
        assert_eq!(csharp_parameter_type("[FromBody] ref string key = null").as_deref(), Some("ref string"));
    }

    #[test]
    fn type_parameters_fill_a_pattern() {
        let tps: HashSet<String> = ["T".to_string()].into_iter().collect();
        assert!(fits_with_type_parameters("List<T>", &tps, "List<String>"));
        assert!(!fits_with_type_parameters("List<T>", &tps, "Set<String>"));
        assert_eq!(loose_type("List<Map<K, V>>?", Language::Kotlin), "List");
    }
}
