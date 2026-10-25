//! Field-qualified search query parser.
//!
//! Splits a raw query like
//!
//! ```text
//! kind:function name:auth path:src/api authenticate
//! ```
//!
//! into structured filters (kind=function, name="auth", path prefix
//! "src/api") plus the free-text portion ("authenticate") that goes to FTS.
//! Free text and filters compose: filters narrow the result set, FTS scores
//! within the narrowed set.
//!
//! Recognised fields (case-insensitive, value is the rest until whitespace):
//!
//! * `kind:`  one of function|method|class|interface|struct|...
//! * `lang:`  one of typescript|python|go|...   (alias: `language:`)
//! * `path:`  case-insensitive substring of file_path
//! * `name:`  case-insensitive substring of the symbol's name
//!
//! Unknown field prefixes (e.g. `foo:bar`) are passed through to FTS as plain
//! text — that's how someone searching for `TODO:` gets a result instead of a
//! parse error. A value may be double-quoted to keep whitespace
//! (`path:"my dir/file"`); the quotes are stripped, no nested escapes.

use codegraph_types::jscompat::is_js_space;
use codegraph_types::{Language, NodeKind};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedQuery {
    /// Free-text portion to feed to FTS / LIKE. May be empty.
    pub text: String,
    /// `kind:` filters (OR'd). Empty when none specified.
    pub kinds: Vec<NodeKind>,
    /// `lang:`/`language:` filters (OR'd). Empty when none specified.
    pub languages: Vec<Language>,
    /// `path:` filters (OR'd, case-insensitive substring of file_path).
    pub path_filters: Vec<String>,
    /// `name:` filters (OR'd, case-insensitive substring of node.name).
    pub name_filters: Vec<String>,
}

/// Strip a surrounding pair of double quotes from `s`.
fn unquote(s: &str) -> &str {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Parse a raw query into structured filters + remaining text. Never fails.
pub fn parse_query(raw: &str) -> ParsedQuery {
    let mut out = ParsedQuery::default();

    // Tokenise on whitespace, preserving quoted spans as part of the current
    // token. Quotes can appear at the start (`"…"`) OR mid-token (`path:"…"`);
    // in both cases everything from the opening `"` to the matching `"` is
    // included in the token, whitespace and all.
    let mut tokens: Vec<&str> = Vec::new();
    let next_char = |at: usize| raw[at..].chars().next();
    let mut i = 0;
    while i < raw.len() {
        while let Some(c) = next_char(i).filter(|&c| is_js_space(c)) {
            i += c.len_utf8();
        }
        if i >= raw.len() {
            break;
        }
        let start = i;
        while let Some(c) = next_char(i) {
            if is_js_space(c) {
                break;
            }
            if c == '"' {
                match raw[i + 1..].find('"') {
                    // Unterminated quote — swallow the rest of the input as
                    // one token. Forgiving rather than throwing.
                    None => {
                        i = raw.len();
                        break;
                    }
                    Some(rel) => {
                        i += 1 + rel + 1;
                        continue;
                    }
                }
            }
            i += c.len_utf8();
        }
        tokens.push(&raw[start..i]);
    }

    let mut text_parts: Vec<&str> = Vec::new();
    for tok in tokens {
        let colon = match tok.find(':') {
            Some(c) if c > 0 && c != tok.len() - 1 => c,
            _ => {
                text_parts.push(tok);
                continue;
            }
        };
        let key = tok[..colon].to_lowercase();
        let value = unquote(&tok[colon + 1..]);
        if value.is_empty() {
            text_parts.push(tok);
            continue;
        }
        match key.as_str() {
            "kind" => match NodeKind::parse(value) {
                Some(k) => out.kinds.push(k),
                None => text_parts.push(tok),
            },
            "lang" | "language" => match Language::parse(&value.to_lowercase()) {
                Some(l) => out.languages.push(l),
                None => text_parts.push(tok),
            },
            "path" => out.path_filters.push(value.to_string()),
            "name" => out.name_filters.push(value.to_string()),
            _ => text_parts.push(tok),
        }
    }

    out.text = codegraph_types::jscompat::js_trim(&text_parts.join(" ")).to_string();
    out
}

/// Bounded edit distance (insert / delete / substitute). Returns `max_dist + 1`
/// as soon as the distance is known to exceed `max_dist`; that early exit makes
/// the fuzzy fallback cheap even over tens of thousands of names.
///
/// Pure DP, O(min(len(a), len(b))) memory. Compares UTF-16 code units, like the
/// TypeScript original, and is case-sensitive: callers pass lowercased names.
pub fn bounded_edit_distance(a: &str, b: &str, max_dist: usize) -> usize {
    if a == b {
        return 0;
    }
    let a: Vec<u16> = a.encode_utf16().collect();
    let b: Vec<u16> = b.encode_utf16().collect();
    let (al, bl) = (a.len(), b.len());
    if al.abs_diff(bl) > max_dist {
        return max_dist + 1;
    }
    if al == 0 {
        return bl;
    }
    if bl == 0 {
        return al;
    }

    let mut prev: Vec<usize> = (0..=bl).collect();
    let mut cur: Vec<usize> = vec![0; bl + 1];
    for i in 1..=al {
        cur[0] = i;
        let mut row_min = cur[0];
        for j in 1..=bl {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let insertion = cur[j - 1] + 1;
            let deletion = prev[j] + 1;
            let substitution = prev[j - 1] + cost;
            cur[j] = insertion.min(deletion).min(substitution);
            row_min = row_min.min(cur[j]);
        }
        if row_min > max_dist {
            return max_dist + 1;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[bl]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_has_no_filters() {
        let r = parse_query("authenticate user");
        assert_eq!(r.text, "authenticate user");
        assert!(r.kinds.is_empty() && r.languages.is_empty());
        assert!(r.path_filters.is_empty() && r.name_filters.is_empty());
    }

    #[test]
    fn kind_filter_is_removed_from_text() {
        let r = parse_query("kind:function auth");
        assert_eq!(r.kinds, [NodeKind::Function]);
        assert_eq!(r.text, "auth");
    }

    #[test]
    fn lang_and_language_are_one_family() {
        assert_eq!(parse_query("lang:typescript foo").languages, [Language::TypeScript]);
        assert_eq!(parse_query("language:typescript foo").languages, [Language::TypeScript]);
    }

    #[test]
    fn multiple_kinds_are_an_or_set() {
        let mut kinds = parse_query("kind:function kind:method auth").kinds;
        kinds.sort();
        assert_eq!(kinds, [NodeKind::Function, NodeKind::Method]);
    }

    #[test]
    fn path_and_name_are_kept_verbatim() {
        let r = parse_query("path:src/api name:Handler");
        assert_eq!(r.path_filters, ["src/api"]);
        assert_eq!(r.name_filters, ["Handler"]);
    }

    #[test]
    fn quoted_spans_stay_one_token() {
        let r = parse_query("path:\"my dir/file\" foo");
        assert_eq!(r.path_filters, ["my dir/file"]);
        assert_eq!(r.text, "foo");
    }

    #[test]
    fn urls_and_unknown_prefixes_pass_through() {
        assert_eq!(parse_query("http://example.com").text, "http://example.com");
        assert_eq!(parse_query("TODO: needs review").text, "TODO: needs review");
    }

    #[test]
    fn empty_and_invalid_values_pass_through() {
        let r = parse_query("kind: foo");
        assert!(r.kinds.is_empty());
        assert!(r.text.contains("kind:"));
        let r = parse_query("kind:invalid foo");
        assert!(r.kinds.is_empty());
        assert!(r.text.contains("kind:invalid"));
    }

    #[test]
    fn all_filters_no_text() {
        let r = parse_query("kind:function lang:typescript");
        assert_eq!(r.kinds, [NodeKind::Function]);
        assert_eq!(r.languages, [Language::TypeScript]);
        assert_eq!(r.text, "");
    }

    #[test]
    fn survives_empty_and_huge_input() {
        assert_eq!(parse_query("").text, "");
        assert!(!parse_query(&"foo ".repeat(5000)).text.is_empty());
    }

    #[test]
    fn unterminated_quote_swallows_the_rest() {
        // Verified against the TypeScript parser: the unquote() no-ops without a
        // closing quote, so the value keeps its stray `"`.
        let r = parse_query("path:\"a b c");
        assert_eq!(r.path_filters, ["\"a b c"]);
        assert_eq!(r.text, "");
    }

    #[test]
    fn edit_distance_matches_the_typescript_cases() {
        assert_eq!(bounded_edit_distance("user", "user", 2), 0);
        assert_eq!(bounded_edit_distance("user", "usar", 2), 1);
        assert_eq!(bounded_edit_distance("user", "users", 2), 1);
        assert_eq!(bounded_edit_distance("users", "user", 2), 1);
        assert_eq!(bounded_edit_distance("confg", "configX", 2), 2);
        assert_eq!(bounded_edit_distance("foo", "completely-different", 2), 3);
        assert_eq!(bounded_edit_distance("a", "aaaaaaa", 2), 3);
        assert_eq!(bounded_edit_distance("", "", 2), 0);
        assert_eq!(bounded_edit_distance("a", "", 2), 1);
        assert_eq!(bounded_edit_distance("", "abc", 2), 3);
        assert_eq!(bounded_edit_distance("Foo", "foo", 2), 1);
        assert_eq!(bounded_edit_distance("aaaaa", "bbbbb", 2), 3);
    }
}
