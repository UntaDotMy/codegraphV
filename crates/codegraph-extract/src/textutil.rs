//! Shared utilities for the TS/JS walker: compiled regexes, UTF-16 position
//! conversion, generated-file detection, and small text helpers — each
//! mirroring a specific helper in src/extraction/tree-sitter.ts (noted inline).

use regex::Regex;
use std::sync::OnceLock;
use std::collections::HashSet;

macro_rules! re {
    ($name:ident, $pat:expr) => {
        pub fn $name() -> &'static Regex {
            static RE: OnceLock<Regex> = OnceLock::new();
            RE.get_or_init(|| Regex::new($pat).expect(concat!("regex ", stringify!($name))))
        }
    };
}

// RTK_HOOK_NAME_RE (tree-sitter.ts)
re!(rtk_hook_name, r"^use[A-Z][A-Za-z0-9]*(?:Query|Mutation)$");
// reactComponentHoc's styled test
re!(styled_callee, r"^styled\b");
// PascalCase component gate (#841)
re!(pascal_case, r"^[A-Z]");
// extractCall parenthesized-conversion normalization
re!(paren_conversion, r"^\(\s*\*?\s*([A-Za-z_][\w.]*)\s*\)$");
// tsJsCalleeName's dotted chain (`a.b.c`); ASCII classes, as JS's `\w` is
re!(dotted_chain, r"^[A-Za-z_$][0-9A-Za-z_$]*(?:\.[A-Za-z_$][0-9A-Za-z_$]*)+$");
// flushFnRefCandidates SIMPLE_NAME
re!(simple_name, r"^[A-Za-z_$][A-Za-z0-9_$]*$");
// flushFnRefCandidates QUALIFIED_IMPORT
re!(qualified_import, r"^[A-Za-z_$][A-Za-z0-9_$.\\]*[.\\]([A-Za-z_$][A-Za-z0-9_$]*)$");
// captureFnRefCandidates rhs param-storage skip — trailing identifier of LHS
re!(lhs_last_name, r"([A-Za-z_$][A-Za-z0-9_$]*)\s*$");
// extractTsTupleContractNames identifier test
re!(ident_dollar, r"^[A-Za-z_$][A-Za-z0-9_$]*$");
// looksLikeVueStoreFile signal (VUE_STORE_FILE_SIGNAL)
re!(
    vue_store_signal,
    r"\bdefineStore\b|\bcreateStore\b|\bVuex\b|\bmutations\b|\bactions\b|\bgetters\b|\bnamespaced\b"
);
// value-ref target-name distinctiveness: /[A-Z_]/
re!(has_upper_or_underscore, r"[A-Z_]");

// isExportedLater's exact lexical rules, scanned once per file rather than
// compiling a name-specific regex for every candidate initializer. The list
// branch deliberately includes aliases/comment words, as the reference does.
// ASCII token boundaries preserve `$` names; JS regex semantics preserve CRLF,
// lone CR, Unicode line endings and JavaScript's precise whitespace set.

pub fn later_exported_names(source: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let masked = mask_js_noncode(source).unwrap_or_else(|| source.to_string());
    for caps in codegraph_jsre::re!(r"^[ \t]*export\s+(?:default\s+([A-Za-z_$][0-9A-Za-z_$]*)\s*;?[ \t]*$|\{([^}]*)\})", "gm").captures_iter(&masked) {
        if let Some(name) = caps.text(1) { names.insert(name.to_owned()); }
        if let Some(list) = caps.text(2) { names.extend(codegraph_jsre::re!(r"[0-9A-Za-z_$]+", "g").captures_iter(list).map(|c| c.text(0).unwrap().to_owned())); }
    }
    names
}

fn js_line_terminator(bytes: &[u8], at: usize) -> Option<usize> {
    match bytes.get(at) {
        Some(b'\n' | b'\r') => Some(1),
        Some(0xE2) if bytes.get(at + 1) == Some(&0x80) && matches!(bytes.get(at + 2), Some(&0xA8 | &0xA9)) => Some(3),
        _ => None,
    }
}

fn blank_code(out: &mut [u8], from: usize, to: usize) {
    let end = to.min(out.len());
    let mut i = from;
    while i < end {
        match js_line_terminator(out, i) {
            Some(width) => i += width,
            None => {
                out[i] = b' ';
                i += 1;
            }
        }
    }
}

fn js_regex_allowed(prev: Option<u8>, token: &str) -> bool {
    matches!(
        prev,
        None | Some(b'(' | b'=' | b':' | b',' | b'!' | b'&' | b'|' | b'?' | b'{' | b'}' | b';' | b'+' | b'-' | b'*' | b'%' | b'^' | b'~' | b'<' | b'>' | b'[')
    ) || matches!(token, "return" | "case" | "throw" | "typeof" | "instanceof" | "in" | "of" | "yield" | "await" | "else" | "do" | "delete" | "void" | "new")
}

fn skip_js_string(bytes: &[u8], start: usize) -> Option<usize> {
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        if js_line_terminator(bytes, i).is_some() {
            return None;
        }
        match bytes[i] {
            b'\\' => i = (i + 2).min(bytes.len()),
            b if b == quote => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

fn skip_js_regex(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    let mut class = false;
    while i < bytes.len() {
        if js_line_terminator(bytes, i).is_some() {
            return None;
        }
        match bytes[i] {
            b'\\' => i = (i + 2).min(bytes.len()),
            b'[' if !class => {
                class = true;
                i += 1;
            }
            b']' if class => {
                class = false;
                i += 1;
            }
            b'/' if !class => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Skip a template literal and its `${…}` interpolations (which may contain nested templates,
/// strings, regexes and braces), blanking everything but the line breaks.
fn skip_js_template(bytes: &[u8], start: usize, out: &mut [u8]) -> Option<usize> {
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i = (i + 2).min(bytes.len()),
            b'`' => {
                i += 1;
                blank_code(out, start, i);
                return Some(i);
            }
            b'$' if bytes.get(i + 1) == Some(&b'{') => {
                i = scan_js_code(bytes, i + 2, out, true)?;
            }
            _ => i += 1,
        }
    }
    None
}

/// Scan JS source from `start`, blanking strings, templates, regexes and comments. With
/// `in_expression`, stop after the `}` that closes the enclosing `${`; return `None` when a
/// construct never closes, so the caller can fall back to the raw source (upstream 2968).
fn scan_js_code(bytes: &[u8], start: usize, out: &mut [u8], in_expression: bool) -> Option<usize> {
    let mut i = start;
    let mut depth = if in_expression { 1i32 } else { 0 };
    let mut prev: Option<u8> = None;
    let mut token = String::new();
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
            // Comments keep their text: the reference's export-list branch deliberately includes
            // words written in one, and only templates/strings must not name a default.
            while i < bytes.len() && js_line_terminator(bytes, i).is_none() {
                i += 1;
            }
            continue;
        }
        if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        if c == b'\'' || c == b'"' {
            let from = i;
            i = skip_js_string(bytes, i)?;
            blank_code(out, from, i);
            prev = Some(b'x');
            token.clear();
            continue;
        }
        if c == b'`' {
            i = skip_js_template(bytes, i, out)?;
            prev = Some(b'x');
            token.clear();
            continue;
        }
        if c == b'/' && js_regex_allowed(prev, &token) {
            let from = i;
            i = skip_js_regex(bytes, i)?;
            blank_code(out, from, i);
            prev = Some(b'x');
            token.clear();
            continue;
        }
        if in_expression {
            match c {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                _ => {}
            }
        }
        if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' {
            token.push(c as char);
            prev = Some(c);
        } else if !c.is_ascii_whitespace() {
            token.clear();
            prev = Some(c);
        }
        i += 1;
    }
    (!in_expression).then_some(i)
}

/// JS source with strings, templates and regex literals blanked, preserving bytes and line
/// breaks. Comments are skipped but left as text (the reference's export-list branch reads
/// words in them). `None` when the scan cannot finish (an unterminated construct).
pub fn mask_js_noncode(source: &str) -> Option<String> {
    let bytes = source.as_bytes();
    let mut out = bytes.to_vec();
    scan_js_code(bytes, 0, &mut out, false)?;
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// isGeneratedFile (src/extraction/generated-detection.ts) — full pattern list
/// ported so future language walkers share it.
pub fn is_generated_file(file_path: &str) -> bool {
    static RES: OnceLock<Vec<Regex>> = OnceLock::new();
    let patterns = RES.get_or_init(|| {
        [
            r"\.pb\.go$",
            r"\.pulsar\.go$",
            r"_grpc\.pb\.go$",
            r"_mock\.go$",
            r"_mocks\.go$",
            r"^mock_[^/]+\.go$",
            r"\.generated\.[jt]sx?$",
            r"\.gen\.[jt]sx?$",
            r"\.pb\.[jt]s$",
            r"_pb\.[jt]s$",
            r"_grpc_pb\.[jt]s$",
            r"[.-]min\.m?js$",
            r"_pb2(_grpc)?\.py$",
            r"_pb2\.pyi$",
            r"\.pb\.(cc|h)$",
            r"\.g\.cs$",
            r"Grpc\.cs$",
            r"OuterClass\.java$",
            r"Grpc\.java$",
            r"\.pb\.swift$",
            r"\.g\.dart$",
            r"\.freezed\.dart$",
            r"\.pb\.dart$",
            r"\.pbgrpc\.dart$",
            r"\.chopper\.dart$",
            r"\.generated\.rs$",
        ]
        .iter()
        .map(|p| Regex::new(p).expect("generated pattern"))
        .collect()
    });
    patterns.iter().any(|p| p.is_match(file_path))
}

// isMinifiedContent's WEBPACK_RUNTIME (`\b` ASCII, as in JS).
re!(webpack_runtime, r"(?-u:\b)function __webpack_require__\s*\(");

/// isMinifiedContent (src/extraction/generated-detection.ts): a `.js` / `.mjs`
/// / `.cjs` file of at least 4000 UTF-16 units whose text sits mostly on lines
/// of 1000 units or more, dense with `;{}(),` — or that carries a webpack
/// runtime. Lengths are counted in UTF-16 units, as the JS string ops count
/// them, so both extractors agree on every file.
pub fn is_minified_content(file_path: &str, content: &str) -> bool {
    const LINE: usize = 1000;
    let lower = file_path.to_ascii_lowercase();
    if !(lower.ends_with(".js") || lower.ends_with(".mjs") || lower.ends_with(".cjs")) {
        return false;
    }
    // UTF-16 units never outnumber UTF-8 bytes: a short file is out at once.
    if content.len() < 4 * LINE {
        return false;
    }
    let total = content.encode_utf16().count();
    if total < 4 * LINE {
        return false;
    }
    if webpack_runtime().is_match(content) {
        return true;
    }
    let (mut long, mut punctuation) = (0usize, 0usize);
    let (mut line_len, mut line_punct) = (0usize, 0usize);
    for unit in content.encode_utf16() {
        if unit == 10 {
            if line_len >= LINE {
                long += line_len;
                punctuation += line_punct;
            }
            line_len = 0;
            line_punct = 0;
            continue;
        }
        line_len += 1;
        // ; { } ( ) ,
        if matches!(unit, 59 | 123 | 125 | 40 | 41 | 44) {
            line_punct += 1;
        }
    }
    if line_len >= LINE {
        long += line_len;
        punctuation += line_punct;
    }
    long as f64 >= total as f64 * 0.5 && punctuation as f64 >= long as f64 * 0.03
}

/// Byte offsets of each line start, for UTF-16 column conversion.
pub fn line_starts(src: &str) -> Vec<usize> {
    let mut out = vec![0usize];
    for (i, b) in src.bytes().enumerate() {
        if b == b'\n' {
            out.push(i + 1);
        }
    }
    out
}

/// UTF-16 code units in `s` — what web-tree-sitter (and JS string ops)
/// count, so kernel-emitted columns are byte-identical to the wasm path's.
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(|c| c.len_utf16()).sum()
}

/// Column (UTF-16 units) of `byte_pos` on line `row`, given `line_starts`.
pub fn col16(src: &str, starts: &[usize], row: usize, byte_pos: usize) -> u32 {
    let ls = starts.get(row).copied().unwrap_or(0);
    if byte_pos <= ls {
        return 0;
    }
    utf16_len(&src[ls..byte_pos]) as u32
}

/// JS `String.prototype.slice(0, n)` in UTF-16 units, without splitting a
/// surrogate pair (when the cut would split one, we stop one code unit short —
/// a lone surrogate isn't representable in Rust and never round-trips through
/// SQLite anyway). Returns (sliced, was_truncated_at_or_beyond_n).
pub fn slice_utf16(s: &str, n: usize) -> (String, bool) {
    let mut used = 0usize;
    let mut out = String::new();
    for c in s.chars() {
        let w = c.len_utf16();
        if used + w > n {
            return (out, true);
        }
        used += w;
        out.push(c);
        if used == n {
            // Exactly at the limit: truncated iff any source remains.
            let truncated = out.len() < s.len();
            return (out, truncated);
        }
    }
    (out, false)
}

/// objectKeyName (tree-sitter.ts): strip ONE leading and ONE trailing quote
/// character (`'`, `"`, or backtick).
pub fn object_key_name(s: &str) -> String {
    let mut out = s;
    if let Some(first) = out.chars().next() {
        if first == '\'' || first == '"' || first == '`' {
            out = &out[first.len_utf8()..];
        }
    }
    if let Some(last) = out.chars().last() {
        if last == '\'' || last == '"' || last == '`' {
            out = &out[..out.len() - last.len_utf8()];
        }
    }
    out.to_string()
}

/// The `= <first 100 UTF-16 units>[...]` initializer signature used by
/// extractVariable (its `.length >= 100` check fires exactly when the slice
/// hit the cap).
pub fn init_signature(value_text: &str) -> String {
    let (sliced, _) = slice_utf16(value_text, 100);
    if utf16_len(&sliced) >= 100 {
        format!("= {sliced}...")
    } else {
        format!("= {sliced}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_exports_match_reference_on_boundaries_aliases_and_line_endings() {
        let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!("../tests/golden/later-exports.json")).unwrap();
        for case in cases {
            let found = later_exported_names(case["source"].as_str().unwrap());
            for (name, expected) in case["expected"].as_object().unwrap() {
                assert_eq!(found.contains(name), expected.as_bool().unwrap(), "{name}: {}", case["source"]);
            }
        }
    }

    #[test]
    fn utf16_cols() {
        let src = "aé😀b";
        // 'a'=1, 'é'=1, '😀'=2 utf16 units; bytes: a=1, é=2, 😀=4
        assert_eq!(utf16_len(src), 5);
        let starts = line_starts(src);
        assert_eq!(col16(src, &starts, 0, 1), 1); // after 'a'
        assert_eq!(col16(src, &starts, 0, 3), 2); // after 'é'
        assert_eq!(col16(src, &starts, 0, 7), 4); // after '😀'
    }

    #[test]
    fn init_sig_short_and_long() {
        assert_eq!(init_signature("[1, 2]"), "= [1, 2]");
        let long = "x".repeat(150);
        let sig = init_signature(&long);
        assert!(sig.starts_with("= "));
        assert!(sig.ends_with("..."));
        assert_eq!(utf16_len(&sig[2..sig.len() - 3]), 100);
    }

    #[test]
    fn generated_patterns() {
        assert!(is_generated_file("src/api.generated.ts"));
        assert!(is_generated_file("vendor/jquery.min.js"));
        assert!(!is_generated_file("src/app.ts"));
    }
}
