//! CommonJS `require('…')` as an import — port of `src/extraction/commonjs-requires.ts`.
//!
//! An ESM `import … from './application'` is an `import_statement` the walker
//! turns into an `imports` reference from the file. A `require('./application')`
//! is just a call, so a CommonJS file would depend on nothing at file level.
//! This reads the source after extraction (not in a walker) and emits the
//! module's own file reference; the names a binding is called through already
//! resolve through the require binding.

use crate::strip_comments::strip_comments_for_regex;
use codegraph_types::jscompat::{utf16_len, JS_SPACE_CLASS};
use codegraph_types::{EdgeKind, Language, ReferenceKind, UnresolvedReference};
use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;

/// `require('x')` with a literal specifier, not a member's (`foo.require(…)`) or a
/// property (`require.resolve(…)`). A template literal counts only when it has no
/// `${…}` hole. The JS original forbids a preceding `[\w$.]` with a lookbehind;
/// the `regex` crate has none, so the caller checks that character.
static REQUIRE_CALL: LazyLock<Regex> = LazyLock::new(|| {
    let s = format!("[{JS_SPACE_CLASS}]");
    Regex::new(&format!(
        r#"require{s}*\({s}*(?:'([^'\n]+)'|"([^"\n]+)"|`([^`$\n]+)`){s}*\)"#
    ))
    .unwrap()
});

/// A module of this project: a relative or rooted path, or a project alias
/// (`@/lib/x`, `~/x`, `#internal`). A package is outside the index.
static LOCAL_SPECIFIER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:\.{1,2}(?:/|$)|/|[@~]/|#)").unwrap());

/// `import x = require('y')` is TypeScript's import-equals: an `import_statement` already.
static IMPORT_EQUALS: LazyLock<Regex> = LazyLock::new(|| {
    let s = format!("[{JS_SPACE_CLASS}]");
    Regex::new(&format!(r"(?-u:\b)import{s}+(?:type{s}+)?[A-Za-z0-9_$]+{s}*={s}*$")).unwrap()
});

fn is_lookbehind_blocked(prev: Option<char>) -> bool {
    matches!(prev, Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '.')
}

/// The `imports` references a CommonJS file's `require('./x')` calls stand for.
pub fn commonjs_require_refs(file_path: &str, source: &str, language: Language) -> Vec<UnresolvedReference> {
    let lang = language.as_str();
    if !matches!(lang, "javascript" | "jsx" | "typescript" | "tsx") || !source.contains("require") {
        return Vec::new();
    }
    let safe = strip_comments_for_regex(
        source,
        if matches!(lang, "javascript" | "jsx") { "javascript" } else { "typescript" },
    );
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut pos = 0;
    while pos <= safe.len() {
        let Some(caps) = REQUIRE_CALL.captures_at(&safe, pos) else { break };
        let m = caps.get(0).unwrap();
        // JS's `(?<![\w$.])`: a blocked start doesn't consume the text, so retry one char on.
        if is_lookbehind_blocked(safe[..m.start()].chars().next_back()) {
            pos = m.start() + safe[m.start()..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        pos = m.end().max(pos + 1);
        let specifier = caps
            .get(1)
            .or_else(|| caps.get(2))
            .or_else(|| caps.get(3))
            .map_or("", |g| g.as_str())
            .trim_matches(codegraph_types::jscompat::is_js_space);
        if !LOCAL_SPECIFIER.is_match(specifier) || seen.contains(specifier) {
            continue;
        }
        let line_start = safe[..m.start()].rfind('\n').map_or(0, |i| i + 1);
        if IMPORT_EQUALS.is_match(&safe[line_start..m.start()]) {
            continue;
        }
        seen.insert(specifier.to_string());
        out.push(UnresolvedReference {
            from_node_id: format!("file:{file_path}"),
            reference_name: specifier.to_string(),
            reference_kind: ReferenceKind::Edge(EdgeKind::Imports),
            line: safe[..m.start()].bytes().filter(|b| *b == b'\n').count() as u32 + 1,
            column: utf16_len(&safe[line_start..m.start()]) as u32,
            file_path: Some(file_path.to_string()),
            language: Some(language),
            candidates: None,
            row_id: None,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(src: &str) -> Vec<(String, u32, u32)> {
        commonjs_require_refs("lib/x.js", src, Language::parse("javascript").unwrap())
            .into_iter()
            .map(|r| (r.reference_name, r.line, r.column))
            .collect()
    }

    #[test]
    fn local_requires_become_imports() {
        let r = refs("const a = require('./application');\nconst b = require(\"../util\");\nconst c = require('express');\n");
        assert_eq!(r, vec![("./application".to_string(), 1, 10), ("../util".to_string(), 2, 10)]);
    }

    #[test]
    fn members_comments_and_duplicates_are_skipped() {
        assert!(refs("foo.require('./a'); // require('./b')\nx.require('./c')").is_empty());
        assert_eq!(refs("require('./a'); require('./a');").len(), 1);
        assert!(refs("const t = require(`./${x}`);").is_empty());
        assert_eq!(refs("require(`./tpl`)").len(), 1);
    }

    #[test]
    fn import_equals_is_not_a_require() {
        let r = commonjs_require_refs("a.ts", "import fs = require('./fs');\nconst z = require('./z');", Language::parse("typescript").unwrap());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].reference_name, "./z");
    }
}
