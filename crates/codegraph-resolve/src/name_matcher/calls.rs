//! How a call is written: receiver-less or not, `this.`/`self.` chains, .NET type positions and
//! Python call shapes — the call-site readers of `src/resolution/name-matcher.ts`.

use super::python_scope::{is_fixture_in_reach, is_python_locally_bound, is_python_name_imported_from_outside};
use super::util::*;
use crate::ctx::Ctx;
use crate::types::UnresolvedRef;
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::{EdgeKind, Language, Node, NodeKind, ReferenceKind};

/// `bareCallReceiver`'s result: the text before `.name(` and the `this.a.b` links when it is one.
#[derive(Debug, Clone, PartialEq)]
pub struct BareReceiver {
    pub receiver: String,
    pub links: Vec<String>,
}

/// The last `n` UTF-16 units of `s` (`s.slice(-n)`), rounded to a character.
fn last_units(s: &str, n: usize) -> &str {
    let mut units = 0;
    let mut start = s.len();
    for (i, c) in s.char_indices().rev() {
        units += c.len_utf16();
        if units > n {
            break;
        }
        start = i;
    }
    &s[start..]
}

fn trim_end_js(s: &str) -> &str {
    s.trim_end_matches(codegraph_types::jscompat::is_js_space)
}

/// What a call `x.name(` is written on: the receiver text and, for `this.a.b.name(`, the links.
pub fn bare_call_receiver(r: &UnresolvedRef, ctx: &Ctx) -> Option<BareReceiver> {
    if !is_calls(r) || !is_js_ident(&r.reference_name) {
        return None;
    }
    let lines = ctx.get_file_lines(&r.file_path)?;
    let name = r.reference_name.replace('$', "\\$");
    let call = Re::cached(
        &format!(r"(?<![\w$]){name}\s*(?:<[^<>()]*>|\[(?:[^\[\]]|\[[^\[\]]*\])*\])?\s*[({{]"),
        "",
    );
    let from_line = (r.line as usize).saturating_sub(1);
    let span: Vec<&str> = lines.iter().skip(from_line).take(8).map(|s| s.as_str()).collect();
    let from = r.column;
    let mut text: String = span.first().map(|l| after_col(l, from).to_string()).unwrap_or_default();
    let mut at = call.find(&text).map(|m| m.start());
    let stale = match at {
        None => true,
        Some(i) => text.find(&r.reference_name).map_or(true, |t| t < i),
    };
    if stale {
        text = after_col(&span.join("\n"), from).to_string();
        at = call.find(&text).map(|m| m.start());
    }
    let at = at?;
    let before = trim_end_js(&text[..at]);
    if !re!(r"\??\.$").is_match(before) {
        return None;
    }
    let head = trim_end_js(&re!(r"\??\.$").replace_first(before, |_| String::new())).to_string();
    if re!(r"(?:^|[^\w$.])(?:this|self|super|Self)$").is_match(&head) || re!(r"(?:^|[^\w$.])super\s*\([^()]*\)$").is_match(&head) {
        return Some(BareReceiver { receiver: "self".into(), links: Vec::new() });
    }
    let chain = re!(r"(?:^|[^\w$.#])((?:this|super)(?:\s*\??\.\s*#?[\w$]+)+)$").captures(&head).and_then(|c| c.text(1).map(|s| s.to_string()));
    let links: Vec<String> = match chain {
        Some(chain) => chain.split('.').skip(1).map(|l| re!(r"[\s?]", "g").replace_all_str(l, "")).collect(),
        None => Vec::new(),
    };
    Some(BareReceiver { receiver: last_units(&head, 40).to_string(), links })
}

pub fn is_python_self_call(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    r.language == Language::Python && bare_call_receiver(r, ctx).is_some_and(|b| b.receiver == "self")
}

/// A `this.x.name()` call whose written receiver the graph collapsed to the caller's own class.
pub fn is_collapsed_non_recursion(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let Some(written) = bare_call_receiver(r, ctx) else { return false };
    if written.receiver == "self" {
        return false;
    }
    !(is_js_family(r.language) && is_collapsed_self_recursion("this", &written.links, r, ctx))
}

fn is_collapsed_self_recursion(root: &str, links: &[String], r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if root != "this" || links.len() != 1 || links[0].contains('(') {
        return false;
    }
    let field = links[0].strip_prefix('#').unwrap_or(&links[0]);
    let Some(caller) = ctx.node_by_id(&r.from_node_id) else { return false };
    let Some(cut) = caller.qualified_name.rfind("::").filter(|c| *c > 0) else { return false };
    let owner = caller.qualified_name[..cut].rsplit("::").next().unwrap_or("").to_string();
    let Some(cls) = ctx
        .get_nodes_in_file(&r.file_path)
        .iter()
        .find(|n| n.kind == NodeKind::Class && n.name == owner && n.start_line <= r.line && n.end_line >= r.line)
        .cloned()
    else {
        return false;
    };
    let lines = lines_of(ctx, &r.file_path);
    let body = lines.iter().skip(cls.start_line as usize - 1).take((cls.end_line - cls.start_line + 1) as usize).cloned().collect::<Vec<_>>().join("\n");
    let f = escape_regex(field);
    let declared = Re::cached(&format!(r"(?:^|[\s(,])#?{f}\s*[?!]?\s*:\s*([A-Za-z_$][\w$]*)"), "m")
        .captures(&body)
        .and_then(|c| c.text(1).map(|s| s.to_string()))
        .or_else(|| {
            Re::cached(&format!(r"\bthis\.{f}\s*=\s*new\s+([A-Za-z_$][\w$]*)"), "").captures(&body).and_then(|c| c.text(1).map(|s| s.to_string()))
        });
    declared.as_deref() == Some(owner.as_str())
}

pub fn is_bare_js_call(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    is_js_family(r.language) && is_receiver_less_call(r, ctx)
}

pub fn is_bare_go_call(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    r.language == Language::Go && is_receiver_less_call(r, ctx)
}

/// Is the Go reference a bare identifier (no package qualifier, not a field selector)?
pub fn is_go_bare_name(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let name = &r.reference_name;
    if r.language != Language::Go || !is_plain_ident(name) {
        return false;
    }
    if is_calls(r) && is_receiver_less_call(r, ctx) {
        return true;
    }
    let Some(line) = line_at(ctx, &r.file_path, r.line) else { return false };
    if is_calls(r) {
        let rest = after_col(&line, r.column);
        return rest.starts_with('(') && Re::cached(&format!(r"^\(\s*\*?\s*{name}\s*\)\s*\("), "").is_match(rest);
    }
    if starts_with_at(&line, name, r.column) {
        let head = before_col(&line, r.column);
        let trimmed = head.trim_end_matches(codegraph_types::jscompat::is_js_space);
        return !trimmed.ends_with('.') || trimmed.ends_with("...");
    }
    let code = re!(r#""(?:[^"\\]|\\.)*"|`[^`]*`"#, "g").replace_all(&line, |c| Some(" ".repeat(codegraph_types::jscompat::utf16_len(c.text(0)?))));
    Re::cached(&format!(r"(?<![\w.]){name}\b"), "").is_match(&code)
}

pub fn is_bare_r_call(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if r.language != Language::R || !is_calls(r) || !re!(r"^[A-Za-z_.][\w.]*$").is_match(&r.reference_name) {
        return false;
    }
    let Some(line) = line_at(ctx, &r.file_path, r.line).filter(|l| !l.is_empty()) else { return false };
    let name = r.reference_name.replace('.', "\\.");
    let Some(m) = Re::cached(&format!(r"(?<![\w.]){name}\s*\("), "").find(&line) else { return false };
    !re!(r"(?:\$|@|::)\s*$").is_match(&line[..m.start()])
}

pub fn is_bare_php_call(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if r.language != Language::Php || !is_calls(r) || !is_word_str(&r.reference_name) {
        return false;
    }
    let Some(line) = line_at(ctx, &r.file_path, r.line) else { return false };
    let at_col = if char_at_col(&line, r.column) == Some('\\') { r.column + 1 } else { r.column };
    if !starts_with_at(&line, &r.reference_name, at_col) {
        return false;
    }
    let after = col_byte(&line, at_col) + r.reference_name.len();
    if !re!(r"\s*(?:\?\.\s*)?[(<]", "y").captures_at(&line, after).is_some() {
        return false;
    }
    let head = trim_end_js(before_col(&line, at_col));
    !(head.ends_with("->") || head.ends_with("::")) || head.chars().count() <= 1
}

/// `isReceiverLessCall`: `name(` written with nothing but an operator or keyword before it.
pub fn is_receiver_less_call(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if !is_calls(r) || r.reference_name.contains('.') {
        return false;
    }
    let Some(line) = line_at(ctx, &r.file_path, r.line) else { return false };
    if !starts_with_at(&line, &r.reference_name, r.column) {
        return false;
    }
    let after = col_byte(&line, r.column) + r.reference_name.len();
    let opener = re!(r"\s*(?:\?\.\s*)?[(<]", "y").captures_at(&line, after).is_some();
    if !opener && !(is_js_family(r.language) && re!(r"\s*[!)]|\s+(?:as|satisfies)(?![\w$])", "y").captures_at(&line, after).is_some()) {
        return false;
    }
    let head = trim_end_js(before_col(&line, r.column));
    let Some(last) = head.chars().next_back() else { return true };
    if !(last == '.' || last == '$' || last == ']' || last == ')' || last.is_ascii_alphanumeric() || last == '_') {
        return true;
    }
    if last == ')' && is_js_family(r.language) {
        return true;
    }
    let word_start = head.trim_end_matches(|c: char| c.is_ascii_alphanumeric() || c == '_');
    BARE_CALL_KEYWORDS.contains(&&head[word_start.len()..])
}

const BARE_CALL_KEYWORDS: [&str; 19] = [
    "return", "await", "yield", "typeof", "void", "new", "else", "case", "throw", "in", "of", "instanceof", "go", "defer", "if", "switch", "for", "range", "do",
];

/// A C#/VB.NET reference in a type position (`new X`, `X<T>`, `X x`, `As X`).
pub fn is_dot_net_type_ref(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if !matches!(r.language, Language::CSharp | Language::VbNet) {
        return false;
    }
    let kind_ok = matches!(
        r.reference_kind,
        ReferenceKind::Edge(EdgeKind::References | EdgeKind::Instantiates | EdgeKind::TypeOf | EdgeKind::Returns)
    );
    if !kind_ok {
        return false;
    }
    let name = &r.reference_name;
    if !is_plain_ident(name) {
        return false;
    }
    if is_kind(r, EdgeKind::Instantiates) {
        return true;
    }
    let Some(line) = line_at(ctx, &r.file_path, r.line) else { return false };
    if !starts_with_at(&line, name, r.column) {
        return false;
    }
    let before = before_col(&line, r.column);
    let after = &after_col(&line, r.column)[name.len()..];
    if r.language == Language::VbNet {
        return re!(r"\b(?:As|New|Of)\s+$", "i").is_match(before);
    }
    if re!(r"\bnew\s+$").is_match(before) {
        return true;
    }
    if re!(r"^\s+@?[A-Za-z_]").is_match(after) {
        return !re!(r"^\s+(?:is|as|and|or|not|when|in|switch|with)\b").is_match(after);
    }
    if re!(r"^<|^\?(?![.?\[])|^\[\s*[,\]]").is_match(after) {
        return true;
    }
    re!(r"^\s*[,>]").is_match(after) && re!(r"<[^<>()]*$").is_match(before)
}

/// A node that cannot be the target of a name written in a type position.
pub fn can_name_in_type_position(n: &Node) -> bool {
    !matches!(n.kind, NodeKind::Property | NodeKind::Method | NodeKind::EnumMember | NodeKind::Field | NodeKind::Constant)
}

// -- Python call shapes -----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum PythonCallShape {
    Bare,
    Chained { owner: String },
}

pub fn python_call_shape(r: &UnresolvedRef, ctx: &Ctx) -> Option<PythonCallShape> {
    if r.language != Language::Python || !is_calls(r) {
        return None;
    }
    let name = &r.reference_name;
    if !is_plain_ident(name) {
        return None;
    }
    let line = line_at(ctx, &r.file_path, r.line)?;
    let text = after_col(&line, r.column);
    if text.starts_with(name.as_str()) && re!(r"^\s*\(").is_match(&text[name.len()..]) {
        return Some(PythonCallShape::Bare);
    }
    if Re::cached(&format!(r"^(?:self|cls)\s*\.\s*{name}\s*\("), "").is_match(text) {
        return None;
    }
    let chain = Re::cached(&format!(r"^(.*?)\.\s*{name}\s*\("), "").captures(text)?;
    let head = chain.text(1)?;
    let owner = re!(r"(\w+)\s*(?:\([^()]*\)|\[[^\[\]]*\])?\s*$").captures(head).and_then(|c| c.text(1).map(|s| s.to_string()))?;
    Some(PythonCallShape::Chained { owner })
}

pub fn fits_python_call_shape(n: &Node, shape: &PythonCallShape, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    match shape {
        PythonCallShape::Bare => {
            if n.kind == NodeKind::Method {
                return false;
            }
            if n.file_path == r.file_path {
                return true;
            }
            if is_python_name_imported_from_outside(&r.reference_name, r, ctx) {
                return false;
            }
            is_fixture_in_reach(n, &r.file_path, ctx) || !is_python_locally_bound(&r.reference_name, r, ctx)
        }
        PythonCallShape::Chained { owner } => {
            if n.kind == NodeKind::Method {
                let cut = n.qualified_name.rfind("::");
                let own = cut.map(|c| n.qualified_name[..c].rsplit("::").next().unwrap_or("").to_string()).unwrap_or_default();
                let plain = |s: &str| s.replace('_', "").to_lowercase();
                return !own.is_empty() && plain(&own) == plain(owner);
            }
            let parts: Vec<&str> = n.file_path.split('/').collect();
            let stem = re!(r"\.pyi?$").replace_first(parts[parts.len() - 1], |_| String::new());
            stem == *owner || (stem == "__init__" && parts.len() >= 2 && parts[parts.len() - 2] == owner)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_units_takes_the_tail() {
        assert_eq!(last_units("abcdef", 3), "def");
        assert_eq!(last_units("ab", 5), "ab");
    }

    #[test]
    fn bare_call_keywords_are_recognised() {
        assert!(BARE_CALL_KEYWORDS.contains(&"await"));
        assert!(!BARE_CALL_KEYWORDS.contains(&"obj"));
    }
}
