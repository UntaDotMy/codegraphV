//! One file's extraction, end to end — port of `extractFromSource` in `src/extraction/tree-sitter.ts`
//! as the orchestrator calls it: the language's extractor, then the detected frameworks'
//! extraction hooks (routes, middleware, …).

use codegraph_extract::{extract_source, has_native_walker};
use codegraph_resolve::frameworks::{get_applicable_frameworks, FrameworkRef};
use codegraph_resolve::resolver::to_db_reference;
use codegraph_types::{ExtractionError, ExtractionResult, Language, Severity};
use std::panic::{catch_unwind, AssertUnwindSafe};

/// The message of a caught panic payload.
pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

fn error_result(file_path: &str, message: String, code: &str) -> ExtractionResult {
    ExtractionResult {
        errors: vec![ExtractionError {
            message,
            file_path: Some(file_path.to_string()),
            line: None,
            column: None,
            severity: Severity::Error,
            code: Some(code.to_string()),
        }],
        ..ExtractionResult::default()
    }
}

/// The language's own extractor. A language without a walker is the TypeScript engine's "no
/// parser for this language" case (`parser_error`), which every caller reads as "never parsed,
/// store nothing".
fn extract_language(file_path: &str, source: &str, language: Language) -> ExtractionResult {
    let handled_without_walker = matches!(
        language,
        Language::Svelte | Language::Astro | Language::Vue | Language::Liquid | Language::Razor | Language::Xml | Language::Yaml | Language::Twig | Language::Properties
    ) || (language == Language::Pascal && matches!(codegraph_types::nodepath::extname(file_path).to_lowercase().as_str(), ".dfm" | ".fmx"));
    if !handled_without_walker && !has_native_walker(language) {
        return error_result(file_path, format!("Failed to get parser for language: {}", language.as_str()), "parser_error");
    }
    let outcome = catch_unwind(AssertUnwindSafe(|| extract_source(file_path, source, language)));
    match outcome {
        Ok(Ok(result)) => result,
        Ok(Err(message)) => error_result(file_path, message, "parse_error"),
        Err(payload) => error_result(file_path, format!("Worker exited unexpectedly: {}", panic_message(payload.as_ref())), "parse_error"),
    }
}

/// Extract `source` (the content of `file_path`, project-relative) as `language`, then run the
/// extraction hook of every applicable framework named in `framework_names`.
/// `resolvers` is the registry (`all_framework_resolvers()`), built once per thread by the caller.
pub fn extract_from_source(file_path: &str, source: &str, language: Language, framework_names: &[String], resolvers: &[FrameworkRef]) -> ExtractionResult {
    let started = std::time::Instant::now();
    let mut result = extract_language(file_path, source, language);

    if !framework_names.is_empty() {
        let named: Vec<FrameworkRef> = resolvers.iter().filter(|r| framework_names.iter().any(|n| n == r.name())).cloned().collect();
        for fw in get_applicable_frameworks(&named, language) {
            match catch_unwind(AssertUnwindSafe(|| fw.extract(file_path, source))) {
                Ok(Some(extracted)) => {
                    result.nodes.extend(extracted.nodes);
                    result.unresolved_references.extend(extracted.references.iter().map(to_db_reference));
                }
                Ok(None) => {}
                Err(payload) => result.errors.push(ExtractionError {
                    message: format!("Framework extractor '{}' failed: {}", fw.name(), panic_message(payload.as_ref())),
                    file_path: Some(file_path.to_string()),
                    line: None,
                    column: None,
                    severity: Severity::Warning,
                    code: None,
                }),
            }
        }
    }
    if result.duration_ms == 0.0 {
        result.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
    }
    result
}

/// Did this extraction fail because the language's parser could not be loaded (#2335)? Such a file
/// was never parsed, so nothing about it may be stored.
pub fn has_grammar_load_failure(errors: &[ExtractionError]) -> bool {
    errors.iter().any(|e| e.code.as_deref() == Some("parser_error"))
}
