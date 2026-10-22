//! Identifier-segment utilities for the prompt hook's graph-derived gate
//! (`name_segment_vocab`): symbol names split into the words a human would use
//! for them in prose, and prompt prose normalized into candidate words to look
//! those segments up with.
//!
//! "OrderStateMachine" → order / state / machine — so the French prompt
//! "comment marche la state machine des commandes ?" (or any language's prose
//! naming the concept in Latin script) can be verified against the graph
//! without a keyword list ever knowing the words. The FTS index can't serve
//! this — its tokenizer keeps camelCase names as single tokens — which is why
//! segments are materialized at index time instead (see schema.sql,
//! `name_segment_vocab`).

use codegraph_types::jscompat::utf16_len;
use indexmap::IndexSet;
use std::collections::HashSet;
use std::sync::LazyLock;
use unicode_general_category::{get_general_category, GeneralCategory as Gc};
use unicode_normalization::UnicodeNormalization;

/// Bounds keep degenerate identifiers (minified names, hashes) from bloating
/// the vocab: segments outside them carry no prose signal anyway.
const MIN_SEGMENT_CHARS: usize = 2;
const MAX_SEGMENT_CHARS: usize = 32;
const MAX_SEGMENTS_PER_NAME: usize = 12;

fn is_letter(c: char) -> bool {
    matches!(
        get_general_category(c),
        Gc::UppercaseLetter
            | Gc::LowercaseLetter
            | Gc::TitlecaseLetter
            | Gc::ModifierLetter
            | Gc::OtherLetter
    )
}

fn is_number(c: char) -> bool {
    matches!(
        get_general_category(c),
        Gc::DecimalNumber | Gc::LetterNumber | Gc::OtherNumber
    )
}

fn is_mark(c: char) -> bool {
    matches!(
        get_general_category(c),
        Gc::NonspacingMark | Gc::SpacingMark | Gc::EnclosingMark
    )
}

fn is_lower(c: char) -> bool {
    get_general_category(c) == Gc::LowercaseLetter
}

fn is_upper(c: char) -> bool {
    get_general_category(c) == Gc::UppercaseLetter
}

/// `s.match(/[\p{L}\p{N}]+/gu)`.
pub fn letter_number_runs(s: &str) -> Vec<&str> {
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in s.char_indices() {
        if is_letter(c) || is_number(c) {
            start.get_or_insert(i);
        } else if let Some(st) = start.take() {
            runs.push(&s[st..i]);
        }
    }
    if let Some(st) = start {
        runs.push(&s[st..]);
    }
    runs
}

/// `/^\p{N}+$/u.test(s)`.
fn is_all_numbers(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_number)
}

/// Split a symbol or file name into lowercase word segments.
///
/// Handles camelCase / PascalCase (inner lower→Upper), acronym runs
/// ("HTMLParser" → html/parser), snake_case / kebab-case / dotted file names
/// (non-alphanumerics separate), and keeps digits glued to their word
/// ("base64Encode" → base64/encode). Digit-only fragments are dropped.
pub fn split_identifier_segments(name: &str) -> Vec<String> {
    if name.is_empty() {
        return Vec::new();
    }
    let mut out: IndexSet<String> = IndexSet::new();
    for run in letter_number_runs(name) {
        // Split before an Upper that follows lower/digit (camelCase hump), and
        // before the last Upper of an acronym run when a lowercase follows
        // ("HTMLParser" → HTML | Parser).
        let chars: Vec<(usize, char)> = run.char_indices().collect();
        let mut parts: Vec<&str> = Vec::new();
        let mut part_start = 0usize;
        for k in 1..chars.len() {
            let (pos, cur) = chars[k];
            let prev = chars[k - 1].1;
            let hump = (is_lower(prev) || is_number(prev)) && is_upper(cur);
            let acronym_end = is_upper(prev)
                && is_upper(cur)
                && chars.get(k + 1).is_some_and(|&(_, next)| is_lower(next));
            if hump || acronym_end {
                parts.push(&run[part_start..pos]);
                part_start = pos;
            }
        }
        parts.push(&run[part_start..]);

        for part in parts {
            if out.len() >= MAX_SEGMENTS_PER_NAME {
                return out.into_iter().collect();
            }
            let seg = part.to_lowercase();
            let len = utf16_len(&seg);
            if !(MIN_SEGMENT_CHARS..=MAX_SEGMENT_CHARS).contains(&len) {
                continue;
            }
            if is_all_numbers(&seg) {
                continue;
            }
            out.insert(seg);
        }
    }
    out.into_iter().collect()
}

/// Normalize a prose word for segment lookup: lowercase + strip diacritics
/// (NFD, drop combining marks), so "références" matches the segment
/// "references" and "résolution" matches "resolution". Identifier segments are
/// overwhelmingly ASCII, so this is what buys Latin-script languages their
/// cross-lingual reach on loanwords.
pub fn normalize_prose_word(word: &str) -> String {
    let stripped: String = word.nfd().filter(|c| !is_mark(*c)).collect();
    stripped.to_lowercase()
}

/// Candidate cap: a prompt's first words carry its subject; scanning an essay
/// buys nothing and the vocab lookup cost scales with this.
const MAX_PROSE_CANDIDATES: usize = 16;
/// "the"/"des"/"une"/"fix" out; "auth"/"flow"/"path" in.
const MIN_PROSE_CHARS: usize = 4;
/// An unsegmented-script sentence is one giant run — skip it.
const MAX_PROSE_CHARS: usize = 24;

/// English prompt words that are never evidence a symbol was NAMED, however
/// rare their segment happens to be in a given repo: function words, filler,
/// hyper-common dev verbs, and words ABOUT code rather than OF it ("rename
/// this file", "there's an issue"). Measured FPs that motivated this: "fix
/// THIS typo" matched `resolveDeferredThisMemberRefs` (repo-rare segment!),
/// "WRITE a haiku" matched `writeConfig`.
///
/// English-only ON PURPOSE — identifiers are written in English, so only
/// English prose words can accidentally collide with segments. Domain nouns
/// ("state", "checkout", "order") stay OUT — they are exactly the signal; the
/// rarity/co-occurrence rules judge them per-repo.
static ENGLISH_PROSE_STOPWORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "about", "above", "actually", "after", "again", "against", "almost", "along", "also",
        "always", "another", "anything", "around", "away", "back", "because", "been", "before",
        "behind", "being", "below", "best", "better", "between", "both", "cannot", "come", "could",
        "does", "doing", "done", "down", "each", "either", "else", "even", "ever", "every",
        "everything", "fine", "first", "from", "getting", "give", "goes", "going", "gone", "good",
        "great", "have", "having", "help", "here", "inside", "instead", "into", "just", "keep",
        "know", "last", "least", "less", "like", "likely", "little", "look", "looking", "made",
        "make", "making", "many", "maybe", "mind", "more", "most", "much", "must", "need", "needs",
        "never", "next", "nice", "none", "nothing", "okay", "only", "onto", "other", "otherwise",
        "over", "please", "pretty", "probably", "quite", "rather", "really", "right", "same",
        "seem", "seems", "should", "show", "since", "some", "someone", "something", "somewhere",
        "soon", "still", "such", "sure", "take", "than", "thank", "thanks", "that", "their",
        "them", "then", "there", "these", "they", "thing", "things", "think", "this", "those",
        "though", "tried", "tries", "trying", "under", "until", "upon", "very", "want", "wants",
        "well", "went", "were", "what", "when", "which", "while", "will", "wish", "with", "within",
        "without", "would", "wrong", "your", "yours",
        // words ABOUT code, not OF it — present in a huge share of prompts while
        // almost never naming the symbol the user means
        "change", "changes", "check", "class", "classes", "code", "detail", "details",
        "directory", "error", "errors", "example", "examples", "file", "files", "folder",
        "function", "functions", "issue", "issues", "line", "lines", "method", "methods", "name",
        "names", "problem", "problems", "project", "question", "questions", "rename", "test",
        "tests", "type", "types", "update", "value", "values", "warning", "warnings", "work",
        "working", "write", "writing",
    ]
    .into_iter()
    .collect()
});

/// Candidate words from a prompt for segment-vocabulary lookup, in order of
/// appearance: Unicode letter/digit runs, normalized via
/// [`normalize_prose_word`], length-bounded, digit-only dropped, English
/// stopwords dropped, deduped, capped. Everything that survives is judged
/// per-repo by the rarity and co-occurrence rules in
/// `CodeGraph.getSegmentMatches` — there is no domain-word list.
pub fn extract_prose_candidates(prompt: &str) -> Vec<String> {
    if prompt.is_empty() {
        return Vec::new();
    }
    let mut seen: IndexSet<String> = IndexSet::new();
    for run in letter_number_runs(prompt) {
        if seen.len() >= MAX_PROSE_CANDIDATES {
            break;
        }
        if utf16_len(run) > MAX_PROSE_CHARS {
            continue;
        }
        let w = normalize_prose_word(run);
        let len = utf16_len(&w);
        if !(MIN_PROSE_CHARS..=MAX_PROSE_CHARS).contains(&len) {
            continue;
        }
        if is_all_numbers(&w) {
            continue;
        }
        if ENGLISH_PROSE_STOPWORDS.contains(w.as_str()) {
            continue;
        }
        seen.insert(w);
    }
    seen.into_iter().collect()
}

/// Words to look up in the segment vocabulary for a SEARCH query (as opposed
/// to a prompt-hook gate): the query's prose candidates PLUS the segments of
/// its identifier-shaped tokens. An agent's query names concepts both ways —
/// "auto-scroll to bottom" (prose) and "atBottom tracking" (camel) — and the
/// camel token must still reach the segment "bottom" even though the whole
/// token matches no name. Same stopword/length rules as the hook path.
pub fn extract_segment_search_words(query: &str) -> Vec<String> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut words: IndexSet<String> = extract_prose_candidates(query).into_iter().collect();
    let mut segments: Vec<String> = Vec::new();
    for run in letter_number_runs(query) {
        // Only camel-humped tokens contribute segments — a plain word's
        // "segments" are itself (already covered above), and snake_case
        // arrives as separate runs because `_` is not a letter.
        let cs: Vec<char> = run.chars().collect();
        let humped = cs
            .windows(2)
            .any(|w| (is_lower(w[0]) || is_number(w[0])) && is_upper(w[1]));
        if humped {
            segments.extend(split_identifier_segments(run));
        }
    }
    if !segments.is_empty() {
        for w in extract_prose_candidates(&segments.join(" ")) {
            words.insert(w);
        }
    }
    words.into_iter().collect()
}

/// Lookup variants for a prose word: the word itself plus light plural folding
/// ("services" → service; a trailing s/es strip only), so common plurals still
/// hit their singular segment. Returned variants map back to the same word.
///
/// The strips are keyed on English plural spelling (#1145), in three classes:
/// - UNAMBIGUOUS `-es` (after x/sh/ss/zz: boxes, hashes, classes, quizzes) —
///   strip 2 only. Stripping 1 minted a bogus sibling ("classes" → classe).
/// - AMBIGUOUS endings (`-ches`/`-ses`/`-zes`/`-oes`): spelling alone can't
///   split patches(+es) from caches(+s), lenses from databases, heroes from
///   shoes — emit BOTH candidate keys and let the vocab lookup decide.
/// - Everything else ending in `-s`: strip 1 only. A trailing `-ss` is a
///   singular (class, process), not a plural: no strip.
pub fn segment_lookup_variants(word: &str) -> Vec<String> {
    let mut variants = vec![word.to_string()];
    let len = utf16_len(word);
    let can_strip2 = len >= MIN_PROSE_CHARS + 2;
    let can_strip1 = len > MIN_PROSE_CHARS;
    let strip = |n: usize| word[..word.len() - n].to_string();
    if ["xes", "shes", "sses", "zzes"].iter().any(|s| word.ends_with(s)) {
        if can_strip2 {
            variants.push(strip(2));
        }
    } else if ["ches", "ses", "zes", "oes"].iter().any(|s| word.ends_with(s)) {
        if can_strip2 {
            variants.push(strip(2));
        }
        if can_strip1 {
            variants.push(strip(1));
        }
    } else if word.ends_with('s') && !word.ends_with("ss") && can_strip1 {
        variants.push(strip(1));
    }
    variants
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn splits_camel_and_pascal_at_humps() {
        assert_eq!(split_identifier_segments("OrderStateMachine"), v(&["order", "state", "machine"]));
        assert_eq!(split_identifier_segments("userId"), v(&["user", "id"]));
    }

    #[test]
    fn acronym_runs_stay_one_segment() {
        assert_eq!(split_identifier_segments("parseHTMLDocument"), v(&["parse", "html", "document"]));
        assert_eq!(split_identifier_segments("HTMLParser"), v(&["html", "parser"]));
    }

    #[test]
    fn digits_stay_glued_to_their_word() {
        assert_eq!(split_identifier_segments("base64Encode"), v(&["base64", "encode"]));
        assert_eq!(split_identifier_segments("parseHTML5Doc"), v(&["parse", "html5", "doc"]));
    }

    #[test]
    fn splits_snake_kebab_and_dotted_names() {
        assert_eq!(split_identifier_segments("snake_case_name"), v(&["snake", "case", "name"]));
        assert_eq!(split_identifier_segments("MAX_RETRY_COUNT"), v(&["max", "retry", "count"]));
        assert_eq!(split_identifier_segments("checkout.service.ts"), v(&["checkout", "service", "ts"]));
        assert_eq!(split_identifier_segments("state-machine"), v(&["state", "machine"]));
    }

    #[test]
    fn drops_sub_minimum_and_digit_only_fragments_and_dedupes() {
        assert!(split_identifier_segments("x").is_empty());
        assert!(split_identifier_segments("42").is_empty());
        assert_eq!(split_identifier_segments("getData_getData"), v(&["get", "data"]));
    }

    #[test]
    fn prose_keeps_content_words_in_any_latin_language() {
        assert_eq!(
            extract_prose_candidates("comment marche la state machine des commandes ?"),
            v(&["comment", "marche", "state", "machine", "commandes"])
        );
    }

    #[test]
    fn prose_strips_diacritics() {
        assert_eq!(extract_prose_candidates("la résolution des références"), v(&["resolution", "references"]));
        assert_eq!(normalize_prose_word("Übersicht"), "ubersicht");
    }

    #[test]
    fn prose_splits_on_apostrophes() {
        assert_eq!(
            extract_prose_candidates("explique l'architecture du module de stock"),
            v(&["explique", "architecture", "module", "stock"])
        );
    }

    #[test]
    fn prose_caps_candidates_and_skips_unsegmented_sentences() {
        let many: Vec<String> = (0..25).map(|i| format!("distinctword{}", (b'a' + i as u8) as char)).collect();
        assert_eq!(extract_prose_candidates(&many.join(" ")).len(), 16);
        assert!(extract_prose_candidates("請解釋一下這個訂單狀態機的整體運作流程與架構設計方式").is_empty());
        assert_eq!(extract_prose_candidates("修复这个拼写错误"), v(&["修复这个拼写错误"]));
    }

    #[test]
    fn prose_drops_digit_only_and_short_words() {
        assert!(extract_prose_candidates("fix the bug in v2 at 1234").is_empty());
    }

    #[test]
    fn plural_folding() {
        assert!(segment_lookup_variants("services").contains(&"service".to_string()));
        assert_eq!(segment_lookup_variants("services"), v(&["services", "service"]));
        assert_eq!(segment_lookup_variants("machines"), v(&["machines", "machine"]));
        assert_eq!(segment_lookup_variants("classes"), v(&["classes", "class"]));
        assert_eq!(segment_lookup_variants("hashes"), v(&["hashes", "hash"]));
        assert_eq!(segment_lookup_variants("class"), v(&["class"]));
        assert_eq!(segment_lookup_variants("process"), v(&["process"]));
        assert_eq!(segment_lookup_variants("caches"), v(&["caches", "cach", "cache"]));
        assert_eq!(segment_lookup_variants("databases"), v(&["databases", "databas", "database"]));
        assert_eq!(segment_lookup_variants("bus"), v(&["bus"]));
        assert_eq!(segment_lookup_variants("boxes"), v(&["boxes"]));
    }

    #[test]
    fn segment_search_words_add_camel_token_segments() {
        let words = extract_segment_search_words("auto-scroll to bottom — atBottom tracking");
        for w in ["scroll", "bottom", "tracking"] {
            assert!(words.contains(&w.to_string()), "missing {w}: {words:?}");
        }
        assert!(extract_segment_search_words("where is atBottom set").contains(&"bottom".to_string()));
        let words = extract_segment_search_words("trace pinFeedIfNearBottom please");
        for w in ["feed", "near", "bottom"] {
            assert!(words.contains(&w.to_string()), "missing {w}: {words:?}");
        }
        let plain = extract_segment_search_words("how does checkout work");
        assert!(plain.contains(&"checkout".to_string()));
        assert!(!plain.contains(&"check".to_string()));
        assert!(extract_segment_search_words("").is_empty());
    }
}
