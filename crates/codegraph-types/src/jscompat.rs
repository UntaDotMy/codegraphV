//! JavaScript string semantics the port has to reproduce.
//!
//! The TypeScript implementation measures, slices and splits strings in UTF-16
//! code units, treats `\s` / `trim()` as the ECMAScript WhiteSpace +
//! LineTerminator set, and its regexes' `\b` / `\w` are ASCII-only. Rust's
//! defaults differ on each point, and the differences surface exactly where
//! node IDs, columns and rankings are computed from non-ASCII source. These
//! helpers pin the JavaScript behavior in one place.

/// The ECMAScript `\s` set (WhiteSpace + LineTerminator), as the *inside* of a
/// regex character class, for splicing into patterns:
/// `format!("[{JS_SPACE_CLASS}]*")`. Rust's own `\s` differs: it includes
/// U+0085 and omits U+FEFF.
pub const JS_SPACE_CLASS: &str = r"\t\n\x0B\x0C\r \u{A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}";

/// A character `/\s/` matches in JavaScript.
pub fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
    )
}

/// `String.prototype.trim`.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

/// `String.prototype.trimStart`.
pub fn js_trim_start(s: &str) -> &str {
    s.trim_start_matches(is_js_space)
}

/// `String.prototype.trimEnd`.
pub fn js_trim_end(s: &str) -> &str {
    s.trim_end_matches(is_js_space)
}

/// `String.prototype.length`: UTF-16 code units.
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// `s.slice(0, n)` for `n` UTF-16 units, rounded down to a character boundary
/// (JavaScript would keep half of a surrogate pair; no caller can use that).
pub fn utf16_prefix(s: &str, n: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        units += c.len_utf16();
        if units > n {
            return &s[..i];
        }
    }
    s
}

/// The byte index of UTF-16 offset `n` in `s`, clamped to the string (an offset inside a
/// surrogate pair rounds down to the character).
pub fn utf16_byte_index(s: &str, n: usize) -> usize {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        if units >= n {
            return i;
        }
        units += c.len_utf16();
        if units > n {
            return i;
        }
    }
    s.len()
}

/// `s.slice(from, to)` for UTF-16 offsets (`to` clamped; `from > to` gives `""`).
pub fn utf16_slice(s: &str, from: usize, to: usize) -> &str {
    let a = utf16_byte_index(s, from);
    let b = utf16_byte_index(s, to);
    if a >= b {
        ""
    } else {
        &s[a..b]
    }
}

/// `s.slice(from)` for a UTF-16 offset.
pub fn utf16_from(s: &str, from: usize) -> &str {
    &s[utf16_byte_index(s, from)..]
}

/// The UTF-16 column of byte offset `byte_pos` within the line that starts at
/// byte offset `line_start` — what tree-sitter's byte column becomes under
/// JavaScript's string indexing.
pub fn utf16_col(src: &str, line_start: usize, byte_pos: usize) -> usize {
    utf16_len(&src[line_start..byte_pos])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_is_the_ecmascript_set() {
        assert!(is_js_space('\u{FEFF}'));
        assert!(is_js_space('\u{A0}'));
        assert!(!is_js_space('\u{85}'));
        assert_eq!(js_trim("\u{FEFF} a b \u{2028}"), "a b");
    }

    #[test]
    fn lengths_count_utf16_units() {
        assert_eq!(utf16_len("abc"), 3);
        assert_eq!(utf16_len("é"), 1);
        assert_eq!(utf16_len("😀"), 2);
        assert_eq!(utf16_prefix("a😀b", 2), "a");
        assert_eq!(utf16_prefix("a😀b", 3), "a😀");
        assert_eq!(utf16_prefix("abc", 10), "abc");
    }
}
