//! Import-mapping and re-export extraction — `extractImportMappings` / `extractReExports` of
//! `import-resolver.ts` and the per-language readers behind them.

use crate::types::{ImportMapping, ReExport};
use codegraph_extract::strip_comments::strip_comments_for_regex;
use codegraph_jsre::{re, Re};
use codegraph_types::jscompat::js_trim;
use codegraph_types::Language;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// Extract import mappings from a file.
pub fn extract_import_mappings(_file_path: &str, content: &str, language: Language) -> Vec<ImportMapping> {
    match language {
        Language::TypeScript | Language::JavaScript | Language::Tsx | Language::Jsx | Language::ArkTs => extract_js_imports(content),
        // Svelte/Vue single-file components import via plain ES6 inside their `<script>` block
        // (Astro: the `---` frontmatter); the ES6 import regex only matches `import … from '…'`,
        // so running it over the whole SFC (markup + styles included) is safe.
        Language::Svelte | Language::Vue | Language::Astro => extract_js_imports(content),
        Language::Python => extract_python_imports(&strip_comments_for_regex(content, "python")),
        Language::Go => extract_go_imports(content),
        Language::Java | Language::Kotlin => extract_java_imports(content),
        Language::Php => extract_php_imports(content),
        Language::C | Language::Cpp => extract_cpp_imports(content),
        _ => Vec::new(),
    }
}

/// A JavaScript identifier — `$`, `_` and Unicode letters included. Use with the `u` flag.
const JS_IDENT: &str = r"[$_\p{ID_Start}][$\u200c\u200d\p{ID_Continue}]*";
/// A quoted string, escapes included — an ES2022 string-named specifier.
const JS_STRING: &str = r#""(?:[^"\\\n]|\\.)*"|'(?:[^'\\\n]|\\.)*'"#;
/// TypeScript's `type` modifier, or Flow's `typeof`.
const TYPE_MODIFIER: &str = r"(?:type(?:of)?\s+)";
/// A `{ … }` specifier list: everything up to the closing `}`, with no length cap. A list never
/// holds a brace, so the scan also stops at a `{`.
const SPECIFIER_LIST: &str = "[^{}]+";

fn specifier_name() -> String {
    format!("(?:{JS_IDENT}|{JS_STRING})")
}

static SPECIFIER_ALIAS: LazyLock<Re> = LazyLock::new(|| {
    let n = specifier_name();
    Re::new(&format!(r"^{TYPE_MODIFIER}?({n})\s+as\s+({n})$"), "u")
});
static SPECIFIER_PLAIN: LazyLock<Re> = LazyLock::new(|| {
    let n = specifier_name();
    Re::new(&format!(r"^{TYPE_MODIFIER}?({n})$"), "u")
});

/// `import [type] [Default][, ]{ … }|* as ns from '…'`.
static ES_IMPORT_RE: LazyLock<Re> = LazyLock::new(|| {
    Re::new(
        &format!(
            r#"(?<![$.\p{{ID_Continue}}])import(?:\s+|(?=[{{*]))(?:(?:type(?:of)?|defer)(?:\s*(?=[{{*])|\s+(?!from(?![$\p{{ID_Continue}}]))(?=[$_\p{{ID_Start}}])))?(?:({JS_IDENT})(?![$\u200c\u200d\p{{ID_Continue}}{{])\s*(?:,\s*)?)?(?:\{{({SPECIFIER_LIST})\}}\s*)?(?:(\*)\s*as\s+({JS_IDENT})\s*)?from\s*['"]([^'"]+)['"]"#
        ),
        "gu",
    )
});

/// `export [type] * [as ns] from '…'`.
static WILDCARD_REEXPORT_RE: LazyLock<Re> = LazyLock::new(|| {
    Re::new(
        &format!(
            r#"(?<![$.\p{{ID_Continue}}])export(?![$\p{{ID_Continue}}])\s*(?:type(?![$\p{{ID_Continue}}])\s*)?\*(?:\s*as(?:\s+({JS_IDENT})|\s*({JS_STRING})))?\s*from\s*['"]([^'"]+)['"]"#
        ),
        "gu",
    )
});

/// `export [type] { … } from '…'`.
static NAMED_REEXPORT_RE: LazyLock<Re> = LazyLock::new(|| {
    Re::new(
        &format!(
            r#"(?<![$.\p{{ID_Continue}}])export(?![$\p{{ID_Continue}}])\s*(?:type(?![$\p{{ID_Continue}}])\s*)?\{{({SPECIFIER_LIST})\}}\s*from\s*['"]([^'"]+)['"]"#
        ),
        "gu",
    )
});

/// Split a comment-free specifier list on the commas between specifiers — not those inside a
/// string name (`"a,b" as c`).
pub fn split_specifier_list(list: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut quote: Option<char> = None;
    let mut chars = list.char_indices();
    while let Some((i, ch)) = chars.next() {
        if quote.is_some() {
            if ch == '\\' {
                chars.next();
            } else if Some(ch) == quote {
                quote = None;
            }
        } else if ch == '"' || ch == '\'' {
            quote = Some(ch);
        } else if ch == ',' {
            out.push(&list[start..i]);
            start = i + 1;
        }
    }
    out.push(&list[start..]);
    out
}

/// A captured `{ … }` list with its comments stripped (only when it has a `/` — most don't).
pub fn clean_specifier_list(list: &str) -> String {
    if list.contains('/') {
        strip_js_comments(list)
    } else {
        list.to_string()
    }
}

/// A multi-line JSDoc `@import { … }` list without its ` * ` line prefixes.
fn without_jsdoc_prefixes(list: &str) -> String {
    re!(r"^[ \t]*\*(?!\/)", "gm").replace_all_str(list, "")
}

/// A string-named specifier's name without its quotes, escapes decoded.
fn unquote_specifier(name: &str) -> String {
    if !re!(r#"^["']"#).is_match(name) {
        return name.to_string();
    }
    let inner: String = {
        let chars: Vec<char> = name.chars().collect();
        chars[1..chars.len() - 1].iter().collect()
    };
    re!(r"\\(u\{[0-9a-fA-F]+\}|u[0-9a-fA-F]{4}|x[0-9a-fA-F]{2}|[\s\S])", "g").replace_all(&inner, |c| {
        let esc = c.text(1).unwrap();
        let first = esc.chars().next().unwrap();
        if first == 'u' || first == 'x' {
            let hex = if esc.chars().nth(1) == Some('{') { &esc[2..esc.len() - 1] } else { &esc[1..] };
            let code = u32::from_str_radix(hex, 16).unwrap_or(u32::MAX);
            return Some(if code <= 0x10ffff { char::from_u32(code).map(|c| c.to_string()).unwrap_or_else(|| esc.to_string()) } else { esc.to_string() });
        }
        Some(match first {
            'n' => "\n".to_string(),
            't' => "\t".to_string(),
            'r' => "\r".to_string(),
            'b' => "\u{8}".to_string(),
            'f' => "\u{c}".to_string(),
            'v' => "\u{b}".to_string(),
            '0' => "\0".to_string(),
            _ => esc.to_string(),
        })
    })
}

/// One `{ … }` import/export specifier as the name it takes from the module and the name it
/// binds. `type` alone, or `type as t`, is a specifier NAMED `type`.
pub struct Specifier {
    pub imported: String,
    pub local: String,
}

pub fn parse_specifier(raw: &str) -> Option<Specifier> {
    let item = js_trim(raw);
    if let Some(alias) = SPECIFIER_ALIAS.captures(item) {
        return Some(Specifier { imported: unquote_specifier(alias.text(1).unwrap()), local: unquote_specifier(alias.text(2).unwrap()) });
    }
    if let Some(plain) = SPECIFIER_PLAIN.captures(item) {
        let name = unquote_specifier(plain.text(1).unwrap());
        return Some(Specifier { imported: name.clone(), local: name });
    }
    None
}

/// Extract JS/TS import mappings.
fn extract_js_imports(content: &str) -> Vec<ImportMapping> {
    let mut mappings = Vec::new();

    for m in ES_IMPORT_RE.captures_iter(content) {
        let default_import = m.text(1).filter(|s| !s.is_empty());
        let named_imports = m.text(2).filter(|s| !s.is_empty());
        let star = m.text(3).filter(|s| !s.is_empty());
        let namespace_alias = m.text(4).filter(|s| !s.is_empty());
        let source = m.text(5).unwrap().to_string();

        if let Some(d) = default_import {
            mappings.push(ImportMapping::new(d, "default", source.clone(), true, false));
        }

        if let Some(named) = named_imports {
            // Comments inside the list are stripped first so a `,` in one can't split it. A JSDoc
            // `@import` tag's list carries ` * ` line prefixes when it wraps.
            let start = m.get(0).unwrap().start();
            let list = if content[..start].ends_with('@') { without_jsdoc_prefixes(named) } else { named.to_string() };
            let cleaned = clean_specifier_list(&list);
            for name in split_specifier_list(&cleaned) {
                if let Some(spec) = parse_specifier(name) {
                    // `{ default as X }` is the default import spelled as a named one.
                    let is_default = spec.imported == "default";
                    mappings.push(ImportMapping::new(spec.local, spec.imported, source.clone(), is_default, false));
                }
            }
        }

        if let (Some(_), Some(alias)) = (star, namespace_alias) {
            mappings.push(ImportMapping::new(alias, "*", source.clone(), false, true));
        }
    }

    // Require statements — each declarator of a list too, and a member of the module
    // (`require('./utils').methods`).
    let require_re = re!(
        r#"(?:\b(?:const|let|var)\s+|,\s*)(?:([A-Za-z_$][\w$]*)|{([^}]+)})\s*=\s*require\(\s*['"]([^'"]+)['"]\s*\)(?:\s*\.\s*([A-Za-z_$][\w$]*))?"#,
        "g"
    );
    for m in require_re.captures_iter(content) {
        let source = m.text(3).unwrap().to_string();
        let member = m.text(4);
        if let Some(default_name) = m.text(1).filter(|s| !s.is_empty()) {
            mappings.push(ImportMapping::new(default_name, member.unwrap_or("default"), source.clone(), member.is_none(), false));
        }
        if let Some(destructured) = m.text(2).filter(|s| !s.is_empty()) {
            for name in destructured.split(',').map(js_trim) {
                if let Some(alias) = re!(r"([A-Za-z_$][\w$]*)\s*:\s*([A-Za-z_$][\w$]*)").captures(name) {
                    mappings.push(ImportMapping::new(alias.text(2).unwrap(), alias.text(1).unwrap(), source.clone(), false, false));
                } else if !name.is_empty() {
                    mappings.push(ImportMapping::new(name, name, source.clone(), false, false));
                }
            }
        }
    }

    mappings
}

pub struct PythonFromImport {
    pub source: String,
    pub names: Vec<(String, String)>,
    pub star: bool,
}

/// The `from X import …` statements of a Python file, read over comment- and docstring-blanked
/// source so a usage example in a docstring binds nothing. Handles the parenthesised multi-line
/// list and backslash continuations.
pub fn python_from_imports(code: &str) -> Vec<PythonFromImport> {
    let mut out = Vec::new();
    let from_re = re!(r"^[ \t]*from[ \t]+(\.*[\w.]*)[ \t]+import[ \t]*(\([^)]*\)|(?:[^\n\\;]|\\\r?\n)+)", "gm");
    for m in from_re.captures_iter(code) {
        let source = m.text(1).unwrap();
        if source.is_empty() {
            continue;
        }
        let list = re!(r"\\\r?\n", "g").replace_all_str(&re!(r"^\(|\)$", "g").replace_all_str(m.text(2).unwrap(), ""), " ");
        let mut names = Vec::new();
        let mut star = false;
        for raw in list.split(',') {
            let item = js_trim(raw);
            if item == "*" {
                star = true;
                continue;
            }
            if let Some(im) = re!(r"^(\w+)(?:\s+as\s+(\w+))?$").captures(item) {
                let name = im.text(1).unwrap().to_string();
                let alias = im.text(2).map(String::from).unwrap_or_else(|| name.clone());
                names.push((name, alias));
            }
        }
        out.push(PythonFromImport { source: source.to_string(), names, star });
    }
    out
}

/// Extract Python import mappings.
fn extract_python_imports(content: &str) -> Vec<ImportMapping> {
    let mut mappings = Vec::new();
    let code = strip_comments_for_regex(content, "python");

    // from X import Y [as Z], …
    for imp in python_from_imports(&code) {
        for (name, alias) in imp.names {
            mappings.push(ImportMapping::new(alias, name, imp.source.clone(), false, false));
        }
    }

    // import X [as Y], … at module level.
    let import_re = re!(r"^import[ \t]+((?:[^\n\\;]|\\\r?\n)+)", "gm");
    for m in import_re.captures_iter(&code) {
        let joined = re!(r"\\\r?\n", "g").replace_all_str(m.text(1).unwrap(), " ");
        for raw in joined.split(',') {
            let Some(im) = re!(r"^([\w.]+)(?:\s+as\s+(\w+))?$").captures(js_trim(raw)) else { continue };
            let module = im.text(1).unwrap();
            let local = im.text(2).filter(|s| !s.is_empty()).map(String::from).unwrap_or_else(|| module.rsplit('.').next().unwrap().to_string());
            mappings.push(ImportMapping::new(local, "*", module, false, true));
        }
    }
    mappings
}

/// Extract Go import mappings. An import is bound to the name written before its path, or else
/// to the name of the package it imports: an unaliased import keeps its last path element and
/// also takes the name goimports assumes for the path, unless another of the file's imports is
/// bound to that name.
fn extract_go_imports(content: &str) -> Vec<ImportMapping> {
    let mut specs: Vec<(Option<String>, String)> = Vec::new();
    // Only the imports, which Go puts before any other declaration, and without their comments.
    let end = go_imports_end(content);
    let code = strip_comments_for_regex(&content[..end], "go");

    // import "path" or import name "path"
    for m in re!(r#"\bimport\s+(?:([A-Za-z_]\w*)[ \t]*)?["']([^"']+)["']"#, "g").captures_iter(&code) {
        specs.push((m.text(1).map(String::from), m.text(2).unwrap().to_string()));
    }

    // import ( ... ) block
    for m in re!(r"\bimport\s*\(\s*([^)]+)\s*\)", "g").captures_iter(&code) {
        for l in re!(r#"(?:([A-Za-z_]\w*|\.)[ \t]*)?["']([^"']+)["']"#, "g").captures_iter(m.text(1).unwrap()) {
            specs.push((l.text(1).map(String::from), l.text(2).unwrap().to_string()));
        }
    }

    let mapping = |local: String, source: String| ImportMapping::new(local, "*", source, false, true);
    // Go binds a name to one import of a file, so a name another import is given explicitly is
    // not an unaliased import's.
    let explicit: HashSet<&str> = specs
        .iter()
        .filter_map(|(name, _)| name.as_deref().filter(|n| *n != "." && *n != "_"))
        .collect();
    // A dot import (`. "pkg"`) binds no name of its own; it stays listed under its last path element.
    let mut mappings: Vec<ImportMapping> = specs
        .iter()
        .map(|(name, source)| {
            let explicitly = name.as_deref().is_some_and(|n| n != ".");
            let local = match name.as_deref() {
                None | Some(".") => source.rsplit('/').next().unwrap().to_string(),
                Some(n) => n.to_string(),
            };
            (explicitly, local, source.clone())
        })
        .filter(|(explicitly, local, _)| *explicitly || !explicit.contains(local.as_str()))
        .map(|(_, local, source)| mapping(local, source))
        .collect();
    let bound: HashSet<String> = mappings.iter().map(|m| m.local_name.clone()).collect();
    let mut assumed: Vec<(String, Vec<String>)> = Vec::new();
    let mut assumed_idx: HashMap<String, usize> = HashMap::new();
    for (name, source) in &specs {
        if name.is_some() {
            continue;
        }
        let assumed_name = go_assumed_package_name(source);
        if !assumed_name.is_empty() && !bound.contains(&assumed_name) {
            match assumed_idx.get(&assumed_name) {
                Some(&i) => assumed[i].1.push(source.clone()),
                None => {
                    assumed_idx.insert(assumed_name.clone(), assumed.len());
                    assumed.push((assumed_name, vec![source.clone()]));
                }
            }
        }
    }
    // Two imports that assume one name can't both be that package.
    for (local, sources) in assumed {
        if sources.len() == 1 {
            mappings.push(mapping(local, sources.into_iter().next().unwrap()));
        }
    }
    mappings
}

/// Where a Go file's imports end: at its first other top-level declaration, a line opening with
/// `func`, `type`, `var` or `const` outside a block comment.
fn go_imports_end(content: &str) -> usize {
    for m in re!(r"^(?:func|type|var|const)\b", "gm").captures_iter(content) {
        let at = m.get(0).unwrap().start();
        let before = &content[..at];
        // `content.lastIndexOf('/*', at) <= content.lastIndexOf('*/', at)`: the search windows
        // include a match starting AT `at`, which cannot be (the text there is a keyword).
        let open = before.rfind("/*").map_or(-1i64, |i| i as i64);
        let close = before.rfind("*/").map_or(-1i64, |i| i as i64);
        if open <= close {
            return at;
        }
    }
    content.len()
}

/// The package name goimports assumes for an import path: its last element that isn't a major
/// version, without a `go-` prefix, cut where an identifier can't go on.
fn go_assumed_package_name(import_path: &str) -> String {
    let elements: Vec<&str> = import_path.split('/').collect();
    let mut base = *elements.last().unwrap();
    if elements.len() > 1 && re!(r"^v\d+$").is_match(base) {
        base = elements[elements.len() - 2];
    }
    if let Some(rest) = base.strip_prefix("go-") {
        base = rest;
    }
    re!(r"^[\p{L}\p{Nd}_]*", "u").find(base).map(|m| m.as_str().to_string()).unwrap_or_default()
}

/// Extract Java / Kotlin import mappings. `import static com.example.Foo.bar;` is parsed as a
/// local-name `bar` pointing at FQN `com.example.Foo.bar`.
fn extract_java_imports(content: &str) -> Vec<ImportMapping> {
    let mut mappings = Vec::new();
    // Strip line and block comments so `// import foo;` doesn't false-match.
    let stripped = re!(r"//[^\n]*", "g").replace_all_str(&re!(r"/\*[\s\S]*?\*/", "g").replace_all_str(content, ""), "");
    // `import [static] <fqn>[.*];` — and Kotlin's `import <fqn> [as Alias]`, with no `;`.
    let import_re = re!(r"^\s*import\s+(static\s+)?([\w.]+(?:\.\*)?)(?:\s+as\s+([\w]+))?\s*(?:;|$)", "gm");
    for m in import_re.captures_iter(&stripped) {
        let fqn = m.text(2).unwrap();
        // `import com.example.*;` — wildcard. We can't materialize a single local name.
        if fqn.ends_with(".*") {
            continue;
        }
        let parts: Vec<&str> = fqn.split('.').collect();
        let last = *parts.last().unwrap();
        let local = m.text(3).unwrap_or(last);
        if local.is_empty() {
            continue;
        }
        mappings.push(ImportMapping::new(local, last, fqn, false, false));
    }
    mappings
}

/// Extract PHP import mappings (use statements).
fn extract_php_imports(content: &str) -> Vec<ImportMapping> {
    let mut mappings = Vec::new();
    // use Namespace\Class; or use Namespace\Class as Alias;
    for m in re!(r"use\s+([\w\\]+)(?:\s+as\s+(\w+))?;", "g").captures_iter(content) {
        let full_path = m.text(1).unwrap();
        let class_name = full_path.rsplit('\\').next().unwrap();
        let local = m.text(2).filter(|s| !s.is_empty()).unwrap_or(class_name);
        mappings.push(ImportMapping::new(local, class_name, full_path, false, false));
    }
    mappings
}

/// Extract C/C++ import mappings from #include directives. `#include` brings all symbols from
/// the included header into scope (namespace import), so each mapping uses `is_namespace`.
fn extract_cpp_imports(content: &str) -> Vec<ImportMapping> {
    let mut mappings = Vec::new();
    for m in re!(r#"^\s*#\s*include\s+[<"]([^>"]+)[>"]"#, "gm").captures_iter(content) {
        let module_path = m.text(1).unwrap();
        let base = module_path.rsplit('/').next().unwrap();
        let basename = re!(r"\.(h|hpp|hxx|hh|inl|ipp|cxx|cc|cpp)$").replace_all_str(base, "");
        let local = if basename.is_empty() { module_path.to_string() } else { basename };
        mappings.push(ImportMapping::new(local, "*", module_path, false, true));
    }
    mappings
}

/// Strip JS line + block comments from `content` while preserving string literals. A block
/// comment becomes one space, so the tokens either side stay apart.
pub fn strip_js_comments(content: &str) -> String {
    let chars: Vec<char> = content.chars().collect();
    let mut out = String::with_capacity(content.len());
    let mut i = 0;
    let mut string: Option<char> = None;
    while i < chars.len() {
        let ch = chars[i];
        if let Some(q) = string {
            out.push(ch);
            if ch == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if ch == q {
                string = None;
            }
            i += 1;
            continue;
        }
        if ch == '"' || ch == '\'' || ch == '`' {
            string = Some(ch);
            out.push(ch);
            i += 1;
            continue;
        }
        if ch == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if ch == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
            // A space, not nothing: `A/**/as B` must stay two words.
            out.push(' ');
            continue;
        }
        out.push(ch);
        i += 1;
    }
    out
}

/// A Python module's imports ARE its re-exports: `from .impl import foo` makes `foo` an
/// attribute of the module, which is exactly how a package `__init__.py` publishes its API.
fn extract_python_re_exports(content: &str) -> Vec<ReExport> {
    let mut out = Vec::new();
    for imp in python_from_imports(&strip_comments_for_regex(content, "python")) {
        if imp.star {
            out.push(ReExport::Wildcard { source: imp.source.clone() });
        }
        for (name, alias) in imp.names {
            out.push(ReExport::Named { exported_name: alias, original_name: name, source: imp.source.clone() });
        }
    }
    out
}

/// Extract JS/TS re-export declarations from `content`: `export { foo } from './a'`,
/// `export { foo as bar } from './a'`, `export * from './a'`, `export * as ns from './a'`, and
/// TypeScript's `type` forms of each.
pub fn extract_re_exports(content: &str, language: Language) -> Vec<ReExport> {
    if language == Language::Python {
        return extract_python_re_exports(content);
    }
    if !matches!(language, Language::TypeScript | Language::JavaScript | Language::Tsx | Language::Jsx | Language::ArkTs) {
        return Vec::new();
    }
    let mut out = Vec::new();

    // Pre-strip block comments + line comments so a commented-out `// export { x } from '...'`
    // doesn't produce a phantom edge.
    let cleaned = strip_js_comments(content);

    // Wildcard: `export * from '...'`, optionally `export type *`; `export * as ns from '...'`
    // exports `ns` alone.
    for m in WILDCARD_REEXPORT_RE.captures_iter(&cleaned) {
        // `export * as ns` / `export * as "my ns"` (ES2022) exports the namespace alone.
        let ns: Option<String> = m.text(1).map(String::from).or_else(|| {
            m.text(2).filter(|s| !s.is_empty()).map(|s| {
                let c: Vec<char> = s.chars().collect();
                c[1..c.len() - 1].iter().collect()
            })
        });
        let source = m.text(3).unwrap().to_string();
        match ns.filter(|n| !n.is_empty()) {
            Some(exported_name) => out.push(ReExport::Namespace { exported_name, source }),
            None => out.push(ReExport::Wildcard { source }),
        }
    }

    // Named: `export { a, b as c } from '...'`, with TypeScript's `type` modifier on the list or
    // an item. `[^{}]`, not `[^}]`: a specifier list never holds a brace.
    for m in NAMED_REEXPORT_RE.captures_iter(&cleaned) {
        let inner = m.text(1).unwrap();
        let source = m.text(2).unwrap();
        // Stripped again here: the file-wide pass above can lose its place (a JSX `Don't`, a
        // regex literal holding a quote) and leave a comment in the list.
        let cleaned_list = clean_specifier_list(inner);
        for raw in split_specifier_list(&cleaned_list) {
            let Some(spec) = parse_specifier(raw) else { continue };
            out.push(ReExport::Named { exported_name: spec.local, original_name: spec.imported, source: source.to_string() });
        }
    }

    out
}
