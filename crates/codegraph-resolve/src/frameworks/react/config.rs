//! Reading a constant's object literal where it is declared: a route's path built from
//! `paths.app.root.path`, and the href a route-config object names. Port of
//! `constantPathValue` / `configHrefExpression` and their readers in
//! `src/resolution/frameworks/react.ts`.

use super::script_language;
use crate::ctx::Ctx;
use crate::frameworks::object_literal::{byte_at, slice, tail};
use crate::import_resolver::resolve_import_path;
use crate::types::NodeRef;
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::js_trim;
use codegraph_types::{Language, NodeKind};

/// The mapping of `root` in `from_file`'s imports, resolved: the file the constant is declared
/// in, and the name it has there. `None` when the import leads nowhere in the project.
fn declaring_file(root: &str, from_file: &str, ctx: &Ctx) -> Option<(String, String)> {
    let mappings_tsx = ctx.get_import_mappings(from_file, Language::Tsx);
    let found = mappings_tsx.iter().find(|m| m.local_name == root).cloned().or_else(|| {
        ctx.get_import_mappings(from_file, Language::TypeScript).iter().find(|m| m.local_name == root).cloned()
    });
    let Some(mapping) = found else { return Some((from_file.to_string(), root.to_string())) };
    let resolved = resolve_import_path(&mapping.source, from_file, Language::TypeScript, ctx)?;
    let name = if !mapping.exported_name.is_empty() && mapping.exported_name != "default" && mapping.exported_name != "*" {
        mapping.exported_name.clone()
    } else {
        root.to_string()
    };
    Some((resolved, name))
}

/// The text of the constant `name` declared in `file`, and where its object literal opens in it.
fn declared_object(file: &str, name: &str, ctx: &Ctx) -> Option<(String, usize)> {
    let decl: NodeRef = ctx.get_nodes_in_file(file).iter().find(|n| n.name == name && matches!(n.kind, NodeKind::Constant | NodeKind::Variable))?.clone();
    let content = ctx.read_file(file);
    let lines: Vec<&str> = content.as_deref().map(|c| c.split('\n').collect()).unwrap_or_default();
    let from = (decl.start_line as usize).saturating_sub(1).min(lines.len());
    let to = (decl.end_line as usize).min(lines.len()).max(from);
    let text = lines[from..to].join("\n");
    let from_name = Re::new(&format!(r"\b{}\b", escape_regex(name)), "").find(&text).map_or(0, |m| m.start());
    let open = text[from_name..].find('{').map(|i| from_name + i)?;
    Some((text, open))
}

/// The string a member expression like `paths.app.root.path` reads from a constant object
/// literal — the constant found through the route file's import of its root name, or in the
/// file itself.
pub fn constant_path_value(expr: &str, from_file: &str, ctx: &Ctx) -> Option<String> {
    let mut parts = expr.split('.');
    let root = parts.next().filter(|r| !r.is_empty())?;
    let keys: Vec<&str> = parts.collect();
    if keys.is_empty() {
        return None;
    }
    let (file, name) = declaring_file(root, from_file, ctx)?;
    let (text, open) = declared_object(&file, &name, ctx)?;
    read_object_path(&text, open, &keys)
}

/// The href a route-config object names — `paths.app.discussion.getHref(id)` or
/// `paths.app.discussion.path` against `export const paths = { app: { discussion: { path:
/// 'discussions/:discussionId', getHref: (id: string) => \`/app/discussions/${id}\` } } }`
/// (bulletproof-react's `config/paths.ts`) — as the string or template literal it returns, ready
/// for the href reader. A `${…}` glued to a segment (a `?redirectTo=` suffix) is dropped; one that
/// is a whole segment stays a hole. `None` for anything else.
pub fn config_href_expression(expr: &str, from_file: &str, ctx: &Ctx) -> Option<String> {
    let m = re!(r"^\s*([A-Za-z_$][\w$]*)((?:\s*\??\.\s*[A-Za-z_$][\w$]*)+)\s*(\((?:[^()]|\([^()]*\))*\))?\s*$").captures(expr)?;
    let root = m.text(1)?;
    let keys: Vec<String> = m.text(2)?.split('.').map(|k| k.chars().filter(|c| *c != '?' && !codegraph_types::jscompat::is_js_space(*c)).collect::<String>()).filter(|k| !k.is_empty()).collect();
    let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let (file, name) = declaring_file(root, from_file, ctx)?;
    let (text, open) = declared_object(&file, &name, ctx)?;
    let mut value = read_object_value(&text, open, &key_refs)?;
    // A function's value is what it returns: `(id: string) => \`/app/…\``, `() => { return '/'; }`.
    if m.get(3).is_some() {
        let body = re!(r"^(?:async\s+)?(?:\([^()]*(?:\([^()]*\)[^()]*)*\)|[A-Za-z_$][\w$]*)\s*(?::\s*[^=]+?)?=>\s*").find(&value)?;
        value = js_trim(&value[body.end()..]).to_string();
        if value.starts_with('{') {
            value = re!(r#"\breturn\s+([`'"][\s\S]*?[`'"])\s*;?\s*}"#).captures(&value).and_then(|c| c.text(1).map(String::from)).unwrap_or_default();
        }
    }
    if !value.starts_with(['`', '\'', '"']) {
        return None;
    }
    // `/auth/login${redirectTo ? … : ''}`: a hole glued to a segment is a suffix, not a segment.
    Some(if value.starts_with('`') { drop_glued_template_holes(&value) } else { value })
}

/// Remove each `${…}` of a template literal that is not a whole path segment.
fn drop_glued_template_holes(template: &str) -> String {
    let b = template.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$' && b.get(i + 1) == Some(&b'{') {
            let mut depth = 0i32;
            let mut j = i + 1;
            while j < b.len() {
                if b[j] == b'{' {
                    depth += 1;
                } else if b[j] == b'}' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                j += 1;
            }
            let hole = slice(template, i, j + 1);
            let next = b.get(j + 1).copied();
            if out.ends_with('/') && matches!(next, Some(b'/') | Some(b'`') | Some(b'?') | None) {
                out.push_str(hole);
            }
            i = j + 1;
            continue;
        }
        let len = tail(template, i).chars().next().map_or(1, char::len_utf8);
        out.push_str(&template[i..i + len]);
        i += len;
    }
    out
}

/// Past the string opened at `j` (no template interpolation: the readers below step over values).
fn skip_quoted(text: &str, mut j: usize) -> usize {
    let b = text.as_bytes();
    let quote = b[j];
    j += 1;
    while j < b.len() && b[j] != quote {
        if b[j] == b'\\' {
            j += 1;
        }
        j += 1;
    }
    j + 1
}

/// Where the value starting at `j` ends: at the `,` or closing bracket after it.
fn skip_value(text: &str, mut j: usize) -> usize {
    let b = text.as_bytes();
    let mut depth = 0i32;
    while j < b.len() {
        let ch = b[j];
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            j = skip_quoted(text, j);
            continue;
        }
        if ch == b'{' || ch == b'[' || ch == b'(' {
            depth += 1;
        } else if ch == b'}' || ch == b']' || ch == b')' {
            if depth == 0 {
                return j;
            }
            depth -= 1;
        } else if ch == b',' && depth == 0 {
            return j;
        }
        j += 1;
    }
    j
}

/// `\s*(key|'key')\s*:\s*` at the start of `rest`: the key, and the length of the match.
fn entry_key(rest: &str) -> Option<(String, usize)> {
    let m = re!(r#"^\s*(?:([A-Za-z_$][\w$]*)|["']([^"']+)["'])\s*:\s*"#).captures(rest)?;
    let key = m.text(1).or_else(|| m.text(2))?.to_string();
    Some((key, m.get(0)?.end()))
}

/// Walk `keys` into the object literal opening at `at`; the final value's source text.
fn read_object_value(text: &str, at: usize, keys: &[&str]) -> Option<String> {
    let mut i = at + 1;
    while i < text.len() {
        let Some((key, len)) = entry_key(tail(text, i)) else {
            let next = skip_value(text, i);
            if byte_at(text, next) != b',' {
                return None;
            }
            i = next + 1;
            continue;
        };
        let value_at = i + len;
        let end = skip_value(text, value_at);
        if key == keys[0] {
            if keys.len() == 1 {
                return Some(js_trim(slice(text, value_at, end)).to_string());
            }
            return if byte_at(text, value_at) == b'{' { read_object_value(text, value_at, &keys[1..]) } else { None };
        }
        if byte_at(text, end) != b',' {
            return None;
        }
        i = end + 1;
    }
    None
}

/// Walk `keys` into the object literal opening at `at`; the string literal at the end.
fn read_object_path(text: &str, at: usize, keys: &[&str]) -> Option<String> {
    let mut i = at + 1;
    while i < text.len() {
        let Some((key, len)) = entry_key(tail(text, i)) else {
            let next = skip_value(text, i);
            if byte_at(text, next) != b',' {
                return None;
            }
            i = next + 1;
            continue;
        };
        let value_at = i + len;
        if key == keys[0] {
            if keys.len() == 1 {
                let lit = re!(r#"^(["'])((?:\\.|(?!\1).)*)\1"#).captures(tail(text, value_at));
                return lit.and_then(|l| l.text(2).map(String::from));
            }
            return if byte_at(text, value_at) == b'{' { read_object_path(text, value_at, &keys[1..]) } else { None };
        }
        let end = skip_value(text, value_at);
        if byte_at(text, end) != b',' {
            return None;
        }
        i = end + 1;
    }
    None
}

#[allow(dead_code)]
fn _language_of(file: &str) -> Language {
    script_language(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_nested_string_paths() {
        let text = "export const paths = { app: { root: { path: '/app' }, discussions: { path: 'discussions' } } }";
        let open = text.find('{').unwrap();
        assert_eq!(read_object_path(text, open, &["app", "discussions", "path"]).as_deref(), Some("discussions"));
        assert_eq!(read_object_path(text, open, &["app", "missing", "path"]), None);
    }

    #[test]
    fn drops_glued_holes_only() {
        assert_eq!(drop_glued_template_holes("`/auth/login${redirect ? 'x' : ''}`"), "`/auth/login`");
        assert_eq!(drop_glued_template_holes("`/app/${id}`"), "`/app/${id}`");
    }
}
