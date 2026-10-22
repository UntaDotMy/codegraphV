//! Tree-sitter extraction — port of `src/extraction/*`.
//!
//! The per-language walkers (`tsjs`, `python`, `java`, …) are the native kernel
//! of the TypeScript engine carried over unchanged: each parses a file with its
//! grammar and writes the file's nodes, edges and unresolved references into the
//! flat tables of `buffers`. [`decode`] turns those tables into the
//! `codegraph_types::ExtractionResult` the rest of the engine consumes.

#![deny(clippy::all)]

/// First statement of every recursive walker function (see stack.rs, #1581):
/// once the stack pointer is inside the red zone, stop descending — the
/// latched flag makes `stack::run_guarded` discard the walk and report the file
/// as too deeply nested. `Default::default()` covers every walker return type
/// in use (`()`, `bool`, `Option<_>`, `String`); a hook returning `false` just
/// sends its caller down the generic child walk, whose own guard returns at once.
macro_rules! stack_guard {
    () => {
        if $crate::stack::exhausted() {
            return ::core::default::Default::default();
        }
    };
}

mod buffers;
pub mod commonjs;
pub mod strip_comments;
mod ccpp;
pub mod cfnptr;
mod csharp;
mod dart;
mod decode;
mod docstring;
mod go;
mod ids;
mod java;
mod kotlin;
pub mod languages;
mod langs;
mod lua;
mod php;
mod preparse;
mod python;
mod rlang;
mod ruby;
mod rustlang;
mod scala;
pub mod scan;
mod stack;
mod ts;
mod swift;
mod textutil;
mod tsjs;

pub use decode::decode_tables;
pub use langs::{grammar_for, LANGUAGES as NATIVE_LANGUAGES};
pub use stack::with_error_tolerance;

use codegraph_types::{ExtractionError, ExtractionResult, Language, NodeKind, Severity};

/// Languages with a native walker.
pub fn has_native_walker(language: Language) -> bool {
    langs::LANGUAGES.contains(&language.as_str())
}

/// Extract one file with its native walker.
///
/// `source` must already have the language's offset-preserving `preParse`
/// blanking applied where one exists (C/C++ macros, C# `#if`, Metal, CUDA).
/// Errs with a `defer:` message only when error-tolerance is off and the tree
/// contains parse errors, or when the file nests deeper than the walker's stack
/// budget allows.
pub fn extract_file(file_path: &str, source: &str, language: Language) -> Result<ExtractionResult, String> {
    let lang = language.as_str();
    let started = std::time::Instant::now();
    let out = stack::run_guarded(|| match lang {
        "java" => java::extract(file_path, source),
        "python" => python::extract(file_path, source),
        "go" => go::extract(file_path, source),
        "c" | "cpp" => ccpp::extract(file_path, source, lang),
        "rust" => rustlang::extract(file_path, source),
        "csharp" => csharp::extract(file_path, source),
        "ruby" => ruby::extract(file_path, source),
        "php" => php::extract(file_path, source),
        "swift" => swift::extract(file_path, source),
        "kotlin" => kotlin::extract(file_path, source),
        "r" => rlang::extract(file_path, source),
        "lua" | "luau" => lua::extract(file_path, source, lang),
        "scala" => scala::extract(file_path, source),
        "dart" => dart::extract(file_path, source),
        "typescript" | "tsx" | "javascript" | "jsx" => tsjs::extract(file_path, source, lang),
        other => Err(format!("no native walker for language {other}")),
    })?;
    let mut result = decode::decode_tables(&out, file_path, language)?;
    result.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
    warn_if_parse_left_nothing(&mut result, file_path);
    Ok(result)
}

/// `hasError` is routine for several grammars; warn only when no symbols
/// survived. (A re-export's import of its module is none: a barrel whose other
/// lines failed to parse has lost what it passes on.)
fn warn_if_parse_left_nothing(result: &mut ExtractionResult, file_path: &str) {
    let (tree_has_error, re_export_imports) = stack::parse_notes();
    let own_symbols = result.nodes.iter().filter(|n| n.kind != NodeKind::File).count() as i64 - i64::from(re_export_imports);
    if tree_has_error && own_symbols == 0 {
        result.errors.push(ExtractionError {
            message: format!(
                "{file_path}: parse produced no symbols (tree has errors) — the file is indexed but contributes nothing to the graph"
            ),
            file_path: None,
            line: None,
            column: None,
            severity: Severity::Warning,
            code: Some("parse_error".to_string()),
        });
    }
}

/// Extract one source file the way `extractFromSource` does for the languages
/// with a native walker: the language's offset-preserving `preParse` blanking,
/// the walker, then the references read beside the tables (a CommonJS `require`).
pub fn extract_source(file_path: &str, source: &str, language: Language) -> Result<ExtractionResult, String> {
    let pre = pre_parse(file_path, source, language);
    let mut result = extract_file(file_path, &pre, language)?;
    result.unresolved_references.extend(commonjs::commonjs_require_refs(file_path, source, language));
    // C++ methods the file's own macros define, read from the original source
    // after the walker so every file mints them (ccpp/macro_methods.rs).
    if language.as_str() == "cpp" {
        ccpp::append_cpp_macro_methods(&mut result, file_path, source, &pre);
    }
    Ok(result)
}

pub use preparse::{enable_stage_timing as enable_preparse_stage_timing, pre_parse, stage_timings as preparse_stage_timings};
