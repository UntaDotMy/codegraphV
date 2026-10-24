//! Walking an object literal in source, for the framework resolvers whose route table IS an
//! object literal. Port of `src/resolution/frameworks/object-literal.ts`.
//!
//! Vue's `routes: [{ name, path, component }]`, TanStack Router's
//! `createRoute({ path, getParentRoute, component })` and its `createFileRoute('/x')({ component })`
//! all state a route as a JavaScript object, and reading one field out of a WINDOW around another
//! is wrong in a way that is silent: a Vue `name` is written above the `path` it belongs to, so a
//! window around each path hands an entry its predecessor's name.
//!
//! So each object is matched as a unit and only its own depth-1 fields are read; nested objects,
//! arrays, calls, strings and templates are stepped over rather than searched. This is a scanner,
//! not a parser: it knows brackets, quotes and template interpolation, and nothing else about
//! JavaScript.
//!
//! Offsets are byte offsets into the source; every position the scanners compare or slice at is
//! that of an ASCII byte, so they always sit on a character boundary.

use codegraph_types::jscompat::is_js_space;
use indexmap::IndexMap;

/// The byte at `i`, or `0` past the end (JavaScript's `undefined`).
pub fn byte_at(s: &str, i: usize) -> u8 {
    s.as_bytes().get(i).copied().unwrap_or(0)
}

/// Whether `pat` is the text at byte `i` (false, never a panic, when `i` is not a character boundary).
pub fn starts_at(s: &str, i: usize, pat: &str) -> bool {
    s.as_bytes().get(i..).is_some_and(|rest| rest.starts_with(pat.as_bytes()))
}

/// `s.slice(from)` with `from` clamped to the string and rounded up to a character boundary.
pub fn tail(s: &str, from: usize) -> &str {
    let mut from = from.min(s.len());
    while !s.is_char_boundary(from) {
        from += 1;
    }
    &s[from..]
}

/// `s.slice(from, to)` with both ends clamped and rounded up to character boundaries.
pub fn slice(s: &str, from: usize, to: usize) -> &str {
    let to = to.min(s.len());
    let mut from = from.min(to);
    while !s.is_char_boundary(from) {
        from += 1;
    }
    let mut to = to;
    while !s.is_char_boundary(to) {
        to += 1;
    }
    &s[from..to.max(from)]
}

/// The byte length of the JavaScript whitespace character at `i`, or 0.
pub fn space_len_at(s: &str, i: usize) -> usize {
    match s.as_bytes().get(i) {
        None => 0,
        Some(b) if b.is_ascii() => usize::from(is_js_space(*b as char)),
        Some(_) if !s.is_char_boundary(i) => 0,
        Some(_) => s[i..].chars().next().filter(|c| is_js_space(*c)).map_or(0, char::len_utf8),
    }
}

/// `i` past any whitespace at it.
pub fn skip_ws(s: &str, mut i: usize) -> usize {
    loop {
        let n = space_len_at(s, i);
        if n == 0 {
            return i;
        }
        i += n;
    }
}

/// The end of the identifier (`[A-Za-z_$][\w$]*`) starting at `i`, if one does.
pub fn ident_end(s: &str, i: usize) -> Option<usize> {
    let b = s.as_bytes();
    let first = *b.get(i)?;
    if !(first.is_ascii_alphabetic() || first == b'_' || first == b'$') {
        return None;
    }
    let mut j = i + 1;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'$') {
        j += 1;
    }
    Some(j)
}

/// Whether the byte is a `\w` character.
pub fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The index of the bracket, brace or paren closing the one at `open`, or `None`.
pub fn match_bracket(s: &str, open: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut i = open;
    while i < b.len() {
        let ch = b[i];
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            i = skip_string(s, i)? + 1;
            continue;
        }
        if ch == b'[' || ch == b'{' || ch == b'(' {
            depth += 1;
        } else if ch == b']' || ch == b'}' || ch == b')' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// The index of the quote closing the string or template opened at `at`, or `None`.
pub fn skip_string(s: &str, at: usize) -> Option<usize> {
    let b = s.as_bytes();
    let quote = *b.get(at)?;
    let mut i = at + 1;
    while i < b.len() {
        let ch = b[i];
        if ch == b'\\' {
            i += 2;
            continue;
        }
        if ch == quote {
            return Some(i);
        }
        if quote == b'`' && ch == b'$' && b.get(i + 1) == Some(&b'{') {
            i = match_bracket(s, i + 1)?;
        }
        i += 1;
    }
    None
}

/// Each `{…}` written directly in `[from, to)`, as its own extent (`start`, `end` of the braces).
pub fn top_level_objects(s: &str, from: usize, to: usize) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = from;
    while i < to.min(b.len()) {
        let ch = b[i];
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            match skip_string(s, i) {
                Some(end) => i = end + 1,
                None => return out,
            }
            continue;
        }
        if ch == b'{' {
            match match_bracket(s, i) {
                Some(end) => {
                    out.push((i, end));
                    i = end + 1;
                }
                None => return out,
            }
            continue;
        }
        i += 1;
    }
    out
}

/// One field of an object: the source text of its value, and where its key sits.
#[derive(Debug, Clone)]
pub struct Field {
    pub text: String,
    pub at: usize,
}

/// An object's own fields — key to its value text and where the key sits. Nested structures are
/// stepped over.
pub fn read_fields(s: &str, start: usize, end: usize) -> IndexMap<String, Field> {
    let b = s.as_bytes();
    let mut out: IndexMap<String, Field> = IndexMap::new();
    let mut i = start + 1;
    while i < end {
        let ch = b.get(i).copied().unwrap_or(0);
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            let Some(close) = skip_string(s, i) else { return out };
            i = close + 1;
            continue;
        }
        if ch == b'{' || ch == b'[' || ch == b'(' {
            let Some(close) = match_bracket(s, i) else { return out };
            i = close + 1;
            continue;
        }
        // `^([A-Za-z_$][\w$]*)\s*:` over a 64-unit window.
        let key = ident_end(s, i).and_then(|key_end| {
            let colon = skip_ws(s, key_end);
            (byte_at(s, colon) == b':' && colon + 1 - i <= 64).then_some((key_end, colon + 1))
        });
        let Some((key_end, value_at)) = key else {
            i += 1;
            continue;
        };
        let mut j = value_at;
        while j < end {
            let c = b.get(j).copied().unwrap_or(0);
            if c == b'"' || c == b'\'' || c == b'`' {
                let Some(close) = skip_string(s, j) else { return out };
                j = close + 1;
                continue;
            }
            if c == b'{' || c == b'[' || c == b'(' {
                let Some(close) = match_bracket(s, j) else { return out };
                j = close + 1;
                continue;
            }
            if c == b',' {
                break;
            }
            j += 1;
        }
        let name = s[i..key_end].to_string();
        if !out.contains_key(&name) {
            out.insert(name, Field { text: slice(s, value_at, j).to_string(), at: i });
        }
        i = j + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_nested_brackets_and_templates() {
        let s = "{ a: [1, `x${ {b: 1}.b }`], c: (2) } tail";
        assert_eq!(match_bracket(s, 0), Some(s.find(" tail").unwrap() - 1));
        assert_eq!(match_bracket("{ unterminated", 0), None);
    }

    #[test]
    fn reads_depth_one_fields() {
        let s = "{ name: 'a', path: '/x', nested: { path: '/y' }, component: Home }";
        let f = read_fields(s, 0, s.len() - 1);
        assert_eq!(f["path"].text.trim(), "'/x'");
        assert_eq!(f["component"].text.trim(), "Home");
        assert_eq!(f.len(), 4);
    }
}
