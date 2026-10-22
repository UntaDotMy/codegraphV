//! Pure search and ranking helpers — no database, no I/O beyond reading a
//! project's manifest for its name.
//!
//! Ports of `src/search/identifier-segments.ts`, `query-parser.ts`,
//! `query-utils.ts` and `src/extraction/generated-detection.ts`.

pub mod generated;
pub mod identifier_segments;
pub mod query_parser;
pub mod query_paths;
pub mod query_utils;

pub use generated::{detect_generated_file, has_generated_header, is_generated_file, is_minified_content};
pub use identifier_segments::{
    extract_prose_candidates, extract_segment_search_words, normalize_prose_word,
    segment_lookup_variants, split_identifier_segments,
};
pub use query_parser::{bounded_edit_distance, parse_query, ParsedQuery};
pub use query_paths::{extract_query_paths, query_might_contain_paths, QueryPathExtraction, QueryPathOptions};
pub use query_utils::{
    derive_project_name_tokens, extract_search_terms, get_stem_variants, is_distinctive_identifier,
    is_test_file, is_test_path, kind_bonus, name_match_bonus, normalize_name_token,
    score_path_relevance, STOP_WORDS,
};
