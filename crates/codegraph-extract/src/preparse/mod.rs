//! Per-language `preParse` hooks — ports of the `preParse` entries in
//! `src/extraction/languages/*.ts`.
//!
//! Each hook blanks (or, rarely, rewrites at the same length) syntax a grammar
//! mis-parses, so offsets, lines and columns survive and the walker sees a tree
//! without the damage. They run on an ASCII *shadow* of the source (see
//! `shadow.rs`) so a port can index bytes exactly where the TypeScript indexes
//! UTF-16 units.

mod cmask;
mod cobol;

/// `maskCppRawStrings(source).source`: the text with C++ raw-string literal bodies blanked (offsets kept).
pub fn mask_cpp_raw_strings_source(source: &str) -> String {
    cmask::mask_cpp_raw_strings(source).source
}

mod conly;
mod cpp;
mod cshared;
pub(crate) mod jsre;
pub(crate) mod shadow;
mod small;

use crate::languages::has_flow_pragma;
use codegraph_types::Language;
use shadow::with_shadow;

/// Whether the TypeScript preParse blanks an `export type *` in `source`: such a file's re-exports
/// keep their statements as written (`blanksExportTypeStar`, #2474).
pub fn blanks_export_type_star(source: &str) -> bool {
    small::has_export_type_star(source) && small::blank_export_type_star(source) != source
}

/// The language's `preParse` hook applied to `source`; the source itself for a
/// language with none.
pub fn pre_parse(file_path: &str, source: &str, language: Language) -> String {
    match language.as_str() {
        "csharp" => with_shadow(source, small::blank_csharp_preprocessor_directives),
        "kotlin" => with_shadow(source, small::pre_parse_kotlin),
        "swift" => with_shadow(source, small::join_swift_composition_continuations),
        // Flow syntax only in `@flow` files, which `detectLanguage` sends here from `.js`; then
        // `export type *`, in every file.
        // ArkTS takes the typescript extractor's preParse (its paths are never Flow candidates).
        "typescript" | "tsx" | "arkts" => {
            let flow = small::is_flow_candidate_path(file_path) && has_flow_pragma(source);
            if !flow && !small::has_export_type_star(source) {
                return source.to_string();
            }
            with_shadow(source, |s| {
                let s = if flow { small::blank_flow_syntax(s) } else { s.to_string() };
                small::blank_export_type_star(&s)
            })
        }
        "c" => with_shadow(source, conly::pre_parse_c),
        "objc" => with_shadow(source, cshared::pre_parse_objc),
        "cpp" => with_shadow(source, |s| cpp::pre_parse_cpp(s, Some(file_path))),
        // Not offset-preserving: a free-format file is shifted right (see cobol.rs).
        "cobol" => cobol::pre_parse_cobol(source),
        // ensureTrailingNewline (vbnet.ts): the grammar has no end-of-file token.
        "vbnet" if !source.ends_with('\n') => format!("{source}\n"),
        _ => source.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Stage timing (a profiling aid for the example binaries)
// ---------------------------------------------------------------------------

static TIMING_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static TIMINGS: std::sync::Mutex<Vec<(&'static str, f64)>> = std::sync::Mutex::new(Vec::new());

/// Start recording how long each pipeline pass takes (see `stage_timings`).
pub fn enable_stage_timing() {
    TIMING_ON.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The recorded `(pass, total milliseconds)` pairs, slowest first.
pub fn stage_timings() -> Vec<(&'static str, f64)> {
    let mut totals: Vec<(&'static str, f64)> = Vec::new();
    for (name, ms) in TIMINGS.lock().unwrap().iter() {
        match totals.iter_mut().find(|(n, _)| n == name) {
            Some(t) => t.1 += ms,
            None => totals.push((name, *ms)),
        }
    }
    totals.sort_by(|a, b| b.1.total_cmp(&a.1));
    totals
}

/// Run one pass, timing it when recording is on.
pub(crate) fn stage<T>(name: &'static str, f: impl FnOnce() -> T) -> T {
    if !TIMING_ON.load(std::sync::atomic::Ordering::Relaxed) {
        return f();
    }
    let t = std::time::Instant::now();
    let out = f();
    TIMINGS.lock().unwrap().push((name, t.elapsed().as_secs_f64() * 1000.0));
    out
}
