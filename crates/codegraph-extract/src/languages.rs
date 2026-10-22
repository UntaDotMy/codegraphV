//! Language detection — port of the extension map and `detectLanguage` family in
//! `src/extraction/grammars.ts`.
//!
//! JavaScript regexes are rewritten with explicit ASCII classes (`\w` is
//! ASCII-only in JS, Unicode in the `regex` crate) and the ECMAScript space set
//! (`jscompat::JS_SPACE_CLASS`) wherever the original relied on `\s`.

use codegraph_types::jscompat::{utf16_prefix, JS_SPACE_CLASS};
use codegraph_types::Language;
use regex::Regex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

/// File extension (lower-cased, with the dot) → language.
const EXTENSIONS: &[(&str, &str)] = &[
    (".ts", "typescript"),
    (".tsx", "tsx"),
    // ESM/CJS TypeScript module extensions — parsed as TS (no JSX). (#366)
    (".mts", "typescript"),
    (".cts", "typescript"),
    // ArkTS (HarmonyOS / OpenHarmony) — a TypeScript superset with declarative UI. (#648)
    (".ets", "arkts"),
    (".js", "javascript"),
    (".mjs", "javascript"),
    (".cjs", "javascript"),
    // SAP HANA XS Classic server-side JavaScript. (#556)
    (".xsjs", "javascript"),
    (".xsjslib", "javascript"),
    (".jsx", "jsx"),
    (".py", "python"),
    (".pyw", "python"),
    (".go", "go"),
    (".rs", "rust"),
    (".java", "java"),
    (".c", "c"),
    (".h", "c"), // Could also be C++, defaulting to C
    (".cpp", "cpp"),
    (".cc", "cpp"),
    (".cxx", "cpp"),
    (".hpp", "cpp"),
    (".hxx", "cpp"),
    (".cs", "csharp"),
    // ASP.NET Razor / Blazor markup — custom RazorExtractor.
    (".cshtml", "razor"),
    (".razor", "razor"),
    (".php", "php"),
    // Drupal-specific PHP file extensions
    (".module", "php"),
    (".install", "php"),
    (".theme", "php"),
    (".inc", "php"),
    // YAML (Drupal routing files; no symbol extraction, file-level tracking only)
    (".yml", "yaml"),
    (".yaml", "yaml"),
    // Twig templates (file-level tracking only, no symbol extraction)
    (".twig", "twig"),
    (".rb", "ruby"),
    (".rake", "ruby"),
    (".swift", "swift"),
    (".kt", "kotlin"),
    (".kts", "kotlin"),
    (".dart", "dart"),
    (".liquid", "liquid"),
    (".svelte", "svelte"),
    (".vue", "vue"),
    (".astro", "astro"),
    (".r", "r"),
    (".pas", "pascal"),
    (".dpr", "pascal"),
    (".dpk", "pascal"),
    (".lpr", "pascal"),
    (".dfm", "pascal"),
    (".fmx", "pascal"),
    (".scala", "scala"),
    (".sc", "scala"),
    (".lua", "lua"),
    (".luau", "luau"),
    (".m", "objc"),
    (".mm", "objc"),
    (".sol", "solidity"),
    // CFML: .cfc/.cfm parse with the tag-aware `cfml` grammar; .cfs is pure CFScript.
    (".cfc", "cfml"),
    (".cfm", "cfml"),
    (".cfs", "cfscript"),
    // Metal Shading Language ≈ C++14 (#1121); CUDA ≈ C++ plus execution-space specifiers (#387).
    (".metal", "cpp"),
    (".cu", "cpp"),
    (".cuh", "cpp"),
    (".nix", "nix"),
    // XML: file-level tracking; the MyBatis extractor emits SQL-statement nodes.
    (".xml", "xml"),
    // COBOL: programs (.cbl/.cob) and copybooks (.cpy).
    (".cbl", "cobol"),
    (".cob", "cobol"),
    (".cobol", "cobol"),
    (".cpy", "cobol"),
    (".vb", "vbnet"),
    // Erlang: modules (.erl), header files (.hrl), escripts.
    (".erl", "erlang"),
    (".hrl", "erlang"),
    (".escript", "erlang"),
    // Spring config: `application.properties` / `application-*.properties`.
    (".properties", "properties"),
    // Terraform / OpenTofu / HCL config.
    (".tf", "terraform"),
    (".tfvars", "terraform"),
    (".tofu", "terraform"),
];

static EXTENSION_MAP: LazyLock<HashMap<&'static str, Language>> = LazyLock::new(|| {
    EXTENSIONS.iter().map(|(ext, lang)| (*ext, Language::parse(lang).expect("language in EXTENSIONS"))).collect()
});

/// The language an extension maps to, if any (the built-in map only).
pub fn language_for_extension(ext: &str) -> Option<Language> {
    EXTENSION_MAP.get(ext).copied()
}

/// All built-in extensions.
pub fn known_extensions() -> impl Iterator<Item = &'static str> {
    EXTENSIONS.iter().map(|(e, _)| *e)
}

const MPEG_TS_PACKET_SIZE: usize = 188;
const MPEG_TS_SYNC_BYTE: u8 = 0x47;
const MPEG_TS_MIN_PACKETS: usize = 16;
/// How many bytes of a file's head [`is_mpeg_transport_stream`] needs.
pub const MPEG_TS_SNIFF_BYTES: usize = MPEG_TS_PACKET_SIZE * MPEG_TS_MIN_PACKETS;

/// Whether these leading bytes are an MPEG transport stream — the other thing a
/// `.ts` file can be (#1910). The sync byte must sit at every packet boundary of
/// the first 16 packets, and the head must be binary (≥ 1/64 control bytes).
pub fn is_mpeg_transport_stream(head: &[u8]) -> bool {
    let last_sync = MPEG_TS_PACKET_SIZE * (MPEG_TS_MIN_PACKETS - 1);
    if head.len() <= last_sync {
        return false;
    }
    let mut off = 0;
    while off <= last_sync {
        if head[off] != MPEG_TS_SYNC_BYTE {
            return false;
        }
        off += MPEG_TS_PACKET_SIZE;
    }
    // Tab, newline, vertical tab, form feed and carriage return are text.
    let control = head.iter().filter(|&&b| b < 0x20 && !(0x09..=0x0d).contains(&b)).count();
    control as f64 >= head.len() as f64 * (1.0 / 64.0)
}

/// Whether `file_path` carries the one extension MPEG-TS shares with a language.
pub fn has_mpeg_ts_extension(file_path: &str) -> bool {
    file_path.len() > 3 && file_path.is_char_boundary(file_path.len() - 3) && file_path[file_path.len() - 3..].eq_ignore_ascii_case(".ts")
}

/// Play Framework routes file: the extensionless `conf/routes` (and `conf/*.routes`).
pub fn is_play_routes_file(file_path: &str) -> bool {
    file_path == "conf/routes" || file_path.ends_with("/conf/routes") || file_path.ends_with(".routes")
}

/// OTP application resource file: `<app>.app.src` or its compiled `<app>.app`.
pub fn is_erlang_app_file(file_path: &str) -> bool {
    let lower = file_path.to_ascii_lowercase();
    lower.ends_with(".app") || lower.ends_with(".app.src")
}

const SHOPIFY_THEME_MARKERS: [&str; 2] = ["layout/theme.liquid", "config/settings_schema.json"];

static SHOPIFY_THEME_FOLDERS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(assets|blocks|config|layout|locales|sections|snippets|templates)$").unwrap());
static SHOPIFY_TEMPLATE_FOLDERS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(templates|sections)$").unwrap());
static SHOPIFY_JSON: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)(^|/)(templates|sections)/.+\.json$").unwrap());

/// The root of the Shopify theme `file_path` is in, when one of its parent theme
/// folders sits beside a theme marker. Project-relative, `/`-separated, `""` for
/// the project root.
pub fn shopify_theme_root(file_path: &str, exists: &dyn Fn(&str) -> bool, folders: &Regex) -> Option<String> {
    let segments: Vec<&str> = file_path.split('/').collect();
    if segments.len() < 2 {
        return None;
    }
    for i in (0..=segments.len() - 2).rev() {
        if !folders.is_match(segments[i]) {
            continue;
        }
        let theme_dir = segments[..i].join("/");
        let found = SHOPIFY_THEME_MARKERS.iter().any(|marker| {
            if theme_dir.is_empty() {
                exists(marker)
            } else {
                exists(&format!("{theme_dir}/{marker}"))
            }
        });
        if found {
            return Some(theme_dir);
        }
    }
    None
}

/// Shopify OS 2.0 JSON template (`templates/*.json`) or section group — only
/// inside a Shopify theme, and only with a project root to look for the theme in.
pub fn is_shopify_liquid_json(file_path: &str, root_dir: Option<&Path>) -> bool {
    let Some(root) = root_dir else { return false };
    if !SHOPIFY_JSON.is_match(file_path) {
        return false;
    }
    let exists = |rel: &str| root.join(rel).exists();
    shopify_theme_root(file_path, &exists, &SHOPIFY_TEMPLATE_FOLDERS).is_some()
}

/// Whether a file is one CodeGraph can parse, based purely on its extension —
/// the single source of truth for "should we index this file".
pub fn is_source_file(file_path: &str, overrides: Option<&HashMap<String, Language>>, root_dir: Option<&Path>) -> bool {
    if is_play_routes_file(file_path) || is_shopify_liquid_json(file_path, root_dir) || is_erlang_app_file(file_path) {
        return true;
    }
    let Some(dot) = file_path.rfind('.') else { return false };
    let ext = file_path[dot..].to_lowercase();
    EXTENSION_MAP.contains_key(ext.as_str()) || overrides.is_some_and(|o| o.contains_key(&ext))
}

/// A PHP open tag: `<?php` in any case, or the short echo `<?=` (never `<?xml`).
static PHP_OPEN_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<\?(?:php|=)").unwrap());

// Building blocks for the Pascal line shapes below. A line start allows
// indentation and the BOM Windows editors write.
const W: &str = "[A-Za-z0-9_]";
fn pas_name() -> String {
    format!(r"[a-z_]{W}*(?:<[^>\n]*>)?")
}
fn pas_qname() -> String {
    format!(r"[a-z_][{W_INNER}.]*(?:<[^>\n]*>)?", W_INNER = "A-Za-z0-9_")
}
fn pas_comment() -> String {
    r"(?:\/\/[^\n]*|\{[^$}\n][^}\n]*\}[ \t]*)?".to_string()
}
fn pas_section_break() -> String {
    let c = pas_comment();
    format!(r"[ \t]*{c}(?:\r?\n[ \t]*{c})+")
}
fn pascal_line(shape: &str) -> Regex {
    // `R`: CRLF mode, so `$` ends a line before `\r\n` as JavaScript's multiline `$` does.
    Regex::new(&format!(r"(?imR)^[ \t\u{{FEFF}}]*{shape}")).expect("pascal include signal")
}

static PASCAL_INCLUDE_SIGNALS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    let name = pas_name();
    let qname = pas_qname();
    let brk = pas_section_break();
    let s = format!("[{JS_SPACE_CLASS}]");
    vec![
        // A compiler directive, a bare `{$ELSE}` / `{$ENDIF}` / `{$IFEND}`, a switch
        // (`{$R-}`), or Lazarus's `{%MainUnit x.pp}` line.
        pascal_line(&format!(
            r"\{{(?:\$(?:[a-z]{W}*[ \t]+[^{JS_SPACE_CLASS}}}]|(?:else|endif|ifend)[ \t]*\}}|[a-z][+-][,}}])|%MainUnit\b)"
        )),
        // A routine header, closed by `;`.
        pascal_line(&format!(
            r"(?:class[ \t]+)?(?:(?:procedure|constructor|destructor)[ \t]+{qname}[ \t]*(?:\([^()]*\)[ \t]*)?|function[ \t]+{qname}[ \t]*(?:(?:\([^()]*\)[ \t]*)?:[ \t]*[A-Za-z0-9_.]+(?:<[^>\n]*>)?[ \t]*)?);"
        )),
        // `unit Foo;` and a `uses A, B;` clause.
        pascal_line(r"unit[ \t]+[a-z_][A-Za-z0-9_.]*[ \t]*;"),
        pascal_line(&format!(
            r"uses{s}+[a-z_][A-Za-z0-9_.]*(?:{s}*,{s}*[a-z_][A-Za-z0-9_.]*)*{s}*;"
        )),
        // A `const` section, then `X = …` / `X: T = …`; or a typed constant on one line.
        pascal_line(&format!(
            r"(?:const|resourcestring)(?:{brk}[a-z_]{W}*[ \t]*(?::[^=;:\n]+)?=|[ \t]+[a-z_]{W}*[ \t]*:[^=;:\n]+=)"
        )),
        // A `var` section: `G, H: Integer;`, on the keyword's line or below it.
        pascal_line(&format!(
            r"(?:var|threadvar)(?:{brk}|[ \t]+)[a-z_]{W}*(?:[ \t]*,[ \t]*[a-z_]{W}*)*[ \t]*:(?:;|[^:;\n][^;\n]*;)"
        )),
        // A `type` section, then `TFoo =`; or `type TFoo = class…` on one line.
        pascal_line(&format!(
            r"type(?:{brk}{name}[ \t]*=|[ \t]+{name}[ \t]*=[ \t]*(?:packed[ \t]+)?(?:class|record|object|interface|dispinterface|set[ \t]+of|array|reference[ \t]+to|procedure|function)\b)"
        )),
    ]
});

static PASCAL_BEGIN: LazyLock<Regex> = LazyLock::new(|| pascal_line(r"begin\b"));
static PASCAL_END: LazyLock<Regex> = LazyLock::new(|| pascal_line(r"end[ \t]*[;.][ \t]*$"));

/// Whether an `.inc` file is a Pascal include rather than a PHP one (#2279).
fn looks_like_pascal_include(source: &str) -> bool {
    if PHP_OPEN_TAG.is_match(source) {
        return false;
    }
    if PASCAL_INCLUDE_SIGNALS.iter().any(|re| re.is_match(source)) {
        return true;
    }
    PASCAL_BEGIN.is_match(source) && PASCAL_END.is_match(source)
}

static FLAG_LEAD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"^(?:[{JS_SPACE_CLASS}]*(?://[^\n]*|/\*(?s:.*?)\*/))*")).unwrap()
});
static FLAG_PRAGMA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"@flow\b").unwrap());
static NOFLOW_PRAGMA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"@noflow\b").unwrap());

/// Whether a JavaScript file's leading comments carry Flow's `@flow` pragma
/// (and not `@noflow`).
pub fn has_flow_pragma(source: &str) -> bool {
    let head = utf16_prefix(source, 4096);
    // `replace(/^#![^\n]*\n/, '')`
    let head = match head.strip_prefix("#!").and_then(|rest| rest.find('\n').map(|i| &rest[i + 1..])) {
        Some(rest) => rest,
        None => head,
    };
    let lead = FLAG_LEAD.find(head).map(|m| m.as_str()).unwrap_or("");
    FLAG_PRAGMA.is_match(lead) && !NOFLOW_PRAGMA.is_match(lead)
}

/// A class/struct BASE CLAUSE — never valid C.
static CPP_BASE_CLAUSE: LazyLock<Regex> = LazyLock::new(|| {
    let s = format!("[{JS_SPACE_CLASS}]");
    Regex::new(&format!(
        r"\b(?:class|struct){s}+[A-Za-z0-9_]+{s}*(?:final{s}*)?:{s}*(?:(?:public|protected|private|virtual){s}+)*[A-Za-z_][A-Za-z0-9_:]*(?:{s}*<[^{{}};]*>)?{s}*[{{,]"
    ))
    .unwrap()
});
static C_COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/\*(?s:.*?)\*/|//[^\n]*").unwrap());

static CPP_SIGNALS: LazyLock<Regex> = LazyLock::new(|| {
    let s = format!("[{JS_SPACE_CLASS}]");
    Regex::new(&format!(
        r"\bnamespace\b|\bclass{s}+[A-Za-z0-9_]+{s}*[:{{]|\b(?:class|struct){s}+[A-Z][A-Z0-9_]+{s}+[A-Za-z0-9_]+{s}*(?:final{s}*)?[:{{]|\btemplate{s}*<|\b(?:public|private|protected){s}*:|\bvirtual\b|\busing{s}+(?:namespace\b|[A-Za-z0-9_]+{s}*=)"
    ))
    .unwrap()
});

/// Heuristic: does a `.h` file contain C++ constructs? The first ~8 KB is checked
/// for patterns unique to C++; the whole source is then scanned for a class/struct
/// base clause (#1592).
fn looks_like_cpp(source: &str) -> bool {
    let sample = utf16_prefix(source, 8192);
    if CPP_SIGNALS.is_match(sample) {
        return true;
    }
    CPP_BASE_CLAUSE.is_match(&C_COMMENT.replace_all(source, " "))
}

static OBJC_SIGNALS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"@(?:interface|implementation|protocol|synthesize)\b").unwrap());

/// Heuristic: does a `.h` file contain Objective-C constructs?
fn looks_like_objc(source: &str) -> bool {
    OBJC_SIGNALS.is_match(utf16_prefix(source, 8192))
}

/// Detect a file's language from its path (and content, where the extension is
/// ambiguous).
///
/// `overrides` is the project's validated custom extension → language map (from
/// `codegraph.json`); its mappings take precedence over the built-ins.
/// `root_dir` is the project root `file_path` is relative to: a Shopify theme's
/// JSON templates are Liquid only with it.
pub fn detect_language(
    file_path: &str,
    source: Option<&str>,
    overrides: Option<&HashMap<String, Language>>,
    root_dir: Option<&Path>,
) -> Language {
    let unknown = Language::Unknown;
    // Play `conf/routes` has no grammar — route through the no-symbol path.
    if is_play_routes_file(file_path) {
        return Language::parse("yaml").unwrap();
    }
    // `filePath.substring(filePath.lastIndexOf('.'))`: with no dot that is the whole path.
    let ext = match file_path.rfind('.') {
        Some(i) => file_path[i..].to_lowercase(),
        None => file_path.to_lowercase(),
    };
    if is_shopify_liquid_json(file_path, root_dir) {
        return Language::parse("liquid").unwrap();
    }
    if is_erlang_app_file(file_path) {
        return Language::parse("erlang").unwrap();
    }
    let lang = overrides
        .and_then(|o| o.get(&ext).copied())
        .or_else(|| EXTENSION_MAP.get(ext.as_str()).copied())
        .unwrap_or(unknown);
    let name = lang.as_str();

    let non_empty = source.filter(|s| !s.is_empty());

    // A Flow-typed `.js` (`// @flow`) parses as TSX.
    if matches!(name, "javascript" | "jsx") {
        if let Some(src) = non_empty {
            if has_flow_pragma(src) {
                return Language::parse("tsx").unwrap();
            }
        }
    }
    // .h files could be C, C++, or Objective-C — check source content.
    if name == "c" && ext == ".h" {
        if let Some(src) = non_empty {
            if looks_like_cpp(src) {
                return Language::parse("cpp").unwrap();
            }
            if looks_like_objc(src) {
                return Language::parse("objc").unwrap();
            }
        }
    }
    // `.inc` is PHP's include extension and Pascal/Delphi's too (#2279).
    if name == "php" && ext == ".inc" && !overrides.is_some_and(|o| o.contains_key(&ext)) {
        if let Some(src) = non_empty {
            if looks_like_pascal_include(src) {
                return Language::parse("pascal").unwrap();
            }
        }
    }
    lang
}

/// Languages tracked at the file-record level only: parsing emits zero symbol
/// nodes, but the file is still stored (`xml` is excluded: its MyBatis extractor
/// emits a file node).
pub fn is_file_level_only_language(language: Language) -> bool {
    matches!(language.as_str(), "yaml" | "twig" | "properties")
}

/// Whether a language has a tree-sitter grammar of its own (narrower than
/// [`is_language_supported`], which also answers true for custom-extractor formats).
pub fn has_tree_sitter_grammar(language: &str) -> bool {
    matches!(
        language,
        "typescript" | "tsx" | "javascript" | "jsx" | "python" | "go" | "rust" | "java" | "c" | "cpp" | "csharp" | "php"
            | "ruby" | "swift" | "kotlin" | "dart" | "pascal" | "scala" | "lua" | "r" | "luau" | "objc" | "cfml"
            | "cfscript" | "cfquery" | "cobol" | "vbnet" | "erlang" | "solidity" | "terraform" | "arkts" | "nix"
    )
}

/// Whether a language is supported (has a grammar or a custom extractor).
pub fn is_language_supported(language: Language) -> bool {
    let name = language.as_str();
    match name {
        "svelte" | "vue" | "astro" | "liquid" | "razor" | "yaml" | "twig" | "xml" | "properties" => true,
        "unknown" => false,
        _ => has_tree_sitter_grammar(name),
    }
}

/// Human-readable language name.
pub fn language_display_name(language: Language) -> &'static str {
    match language.as_str() {
        "typescript" => "TypeScript",
        "javascript" => "JavaScript",
        "tsx" => "TypeScript (TSX)",
        "jsx" => "JavaScript (JSX)",
        "python" => "Python",
        "go" => "Go",
        "rust" => "Rust",
        "r" => "R",
        "java" => "Java",
        "c" => "C",
        "cpp" => "C++",
        "csharp" => "C#",
        "razor" => "Razor/Blazor",
        "php" => "PHP",
        "ruby" => "Ruby",
        "swift" => "Swift",
        "kotlin" => "Kotlin",
        "dart" => "Dart",
        "svelte" => "Svelte",
        "vue" => "Vue",
        "astro" => "Astro",
        "liquid" => "Liquid",
        "pascal" => "Pascal / Delphi",
        "scala" => "Scala",
        "lua" => "Lua",
        "luau" => "Luau",
        "objc" => "Objective-C",
        "solidity" => "Solidity",
        "nix" => "Nix",
        "yaml" => "YAML",
        "twig" => "Twig",
        "xml" => "XML",
        "properties" => "Java properties",
        "cfml" => "CFML",
        "cfscript" => "CFScript",
        "cfquery" => "CFQuery (SQL)",
        "cobol" => "COBOL",
        "vbnet" => "Visual Basic .NET",
        "erlang" => "Erlang",
        "terraform" => "Terraform",
        "arkts" => "ArkTS",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(path: &str, src: Option<&str>) -> &'static str {
        detect_language(path, src, None, None).as_str()
    }

    #[test]
    fn extension_mapping() {
        assert_eq!(d("src/a.ts", None), "typescript");
        assert_eq!(d("src/A.TSX", None), "tsx");
        assert_eq!(d("x.R", None), "r");
        assert_eq!(d("Makefile", None), "unknown");
        assert_eq!(d("conf/routes", None), "yaml");
        assert_eq!(d("rel/my_app.app.src", None), "erlang");
        assert_eq!(d("a/b.metal", None), "cpp");
    }

    #[test]
    fn header_sniffing() {
        assert_eq!(d("a.h", Some("int f(void);\n")), "c");
        assert_eq!(d("a.h", Some("namespace x { }\n")), "cpp");
        assert_eq!(d("a.h", Some("struct D : public B {\n};\n")), "cpp");
        assert_eq!(d("a.h", Some("/* struct foo: x, y */\nint a;\n")), "c");
        assert_eq!(d("a.h", Some("@interface Foo : NSObject\n@end\n")), "objc");
        assert_eq!(d("a.h", None), "c");
    }

    #[test]
    fn flow_pragma_makes_js_tsx() {
        assert_eq!(d("a.js", Some("// @flow\nconst a = 1;\n")), "tsx");
        assert_eq!(d("a.js", Some("/* @flow */\n")), "tsx");
        assert_eq!(d("a.js", Some("// @noflow @flow\n")), "javascript");
        assert_eq!(d("a.js", Some("const a = 1; // @flow\n")), "javascript");
        assert_eq!(d("a.js", Some("#!/usr/bin/env node\n// @flow\n")), "tsx");
    }

    #[test]
    fn inc_is_php_unless_pascal() {
        assert_eq!(d("a.inc", Some("<?php echo 1;")), "php");
        assert_eq!(d("a.inc", Some("{$IFDEF X}\nprocedure Foo;\n")), "pascal");
        assert_eq!(d("a.inc", Some("unit Foo;\n")), "pascal");
        assert_eq!(d("a.inc", Some("just text\n")), "php");
        assert_eq!(d("a.inc", Some("begin\n  x;\nend;\n")), "pascal");
    }

    #[test]
    fn source_file_detection() {
        assert!(is_source_file("a/b.go", None, None));
        assert!(!is_source_file("a/b.txt", None, None));
        assert!(!is_source_file("Makefile", None, None));
        assert!(is_source_file("conf/routes", None, None));
        let mut o = HashMap::new();
        o.insert(".foo".to_string(), Language::parse("python").unwrap());
        assert!(is_source_file("a.foo", Some(&o), None));
    }
}
