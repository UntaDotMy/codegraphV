//! Search query utilities: term extraction and ranking bonuses.
//!
//! Port of `src/search/query-utils.ts`. The ranking constants here are tuned
//! against real repositories (see the issue numbers in the comments); change
//! them only with the same kind of evidence.

use codegraph_types::jscompat::{js_trim, utf16_len};
use codegraph_types::NodeKind;
use indexmap::IndexSet;
use regex::Regex;
use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

/// Normalize a name to a comparable token: lowercase, alphanumerics only.
pub fn normalize_name_token(raw: &str) -> String {
    raw.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect()
}

/// POSIX `path.basename` — index paths are stored with `/` separators.
fn basename(p: &str) -> &str {
    let trimmed = p.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or("")
}

/// POSIX `path.dirname`.
fn dirname(p: &str) -> &str {
    let trimmed = p.trim_end_matches('/');
    match trimmed.rfind('/') {
        None => ".",
        Some(0) => "/",
        Some(i) => &trimmed[..i],
    }
}

/// Tokens that name the PROJECT as a whole — its `go.mod` module,
/// `package.json` name, or repo root directory — rather than any specific
/// symbol. A user naturally puts the project name in a query as context
/// ("MyApp backend routes"), but it carries no discriminative signal: when it's
/// also a substring of a symbol or path on one stack (a `MyAppFrontend/` dir, a
/// `MyAppApp` class) it lexically inflates that stack and buries the rest
/// (#720).
///
/// Returned normalized (lowercase, alphanumerics only). Only names ≥5 chars are
/// kept — short ones (`api`, `app`, `core`, `web`) collide with real query
/// terms too often to safely down-weight.
pub fn derive_project_name_tokens(project_root: &Path) -> HashSet<String> {
    let mut tokens = HashSet::new();
    let mut add = |raw: Option<&str>| {
        if let Some(raw) = raw.filter(|r| !r.is_empty()) {
            let norm = normalize_name_token(raw);
            if norm.len() >= 5 {
                tokens.insert(norm);
            }
        }
    };

    // go.mod module last segment (the most reliable signal for Go repos).
    static GO_MODULE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^\s*module\s+(\S+)").unwrap());
    if let Ok(gomod) = std::fs::read_to_string(project_root.join("go.mod")) {
        if let Some(m) = GO_MODULE.captures(&gomod) {
            add(m
                .get(1)
                .map(|m| m.as_str().rsplit('/').next().unwrap_or("")));
        }
    }

    // package.json name (strip an `@scope/` prefix).
    if let Ok(text) = std::fs::read_to_string(project_root.join("package.json")) {
        if let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(name) = pkg.get("name").and_then(|n| n.as_str()) {
                static SCOPE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^@[^/]+/").unwrap());
                add(Some(&SCOPE.replace(name, "")));
            }
        }
    }

    // Repo root directory name — a fallback when neither manifest names the project.
    if let Ok(abs) = std::path::absolute(project_root) {
        add(abs.file_name().and_then(|n| n.to_str()));
    }

    tokens
}

/// Common stop words to filter from search queries: generic English plus
/// code-specific noise words.
pub static STOP_WORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        // English
        "the",
        "a",
        "an",
        "and",
        "or",
        "but",
        "in",
        "on",
        "at",
        "to",
        "for",
        "of",
        "with",
        "by",
        "from",
        "is",
        "it",
        "that",
        "this",
        "are",
        "was",
        "be",
        "has",
        "had",
        "have",
        "do",
        "does",
        "did",
        "will",
        "would",
        "could",
        "should",
        "may",
        "might",
        "can",
        "shall",
        "not",
        "no",
        "all",
        "each",
        "every",
        "how",
        "what",
        "where",
        "when",
        "who",
        "which",
        "why",
        "i",
        "me",
        "my",
        "we",
        "our",
        "you",
        "your",
        "he",
        "she",
        "they",
        "show",
        "give",
        "tell",
        "been",
        "done",
        "made",
        "used",
        "using",
        "work",
        "works",
        "found",
        "also",
        "into",
        "then",
        "than",
        "just",
        "more",
        "some",
        "such",
        "over",
        "only",
        "out",
        "its",
        "so",
        "up",
        "as",
        "if",
        "look",
        "need",
        "needs",
        "want",
        "happen",
        "happens",
        "affect",
        "affected",
        "break",
        "breaks",
        "failing",
        "implemented",
        "implement",
        // Code-specific noise (avoid filtering common symbol names like get/set/add/build/find/list)
        "code",
        "file",
        "files",
        "function",
        "method",
        "class",
        "type",
        "fix",
        "bug",
        "called",
    ]
    .into_iter()
    .collect()
});

/// Generate stem variants of a search term by removing common English
/// suffixes. Used for FTS query expansion so "caching" also finds "cache",
/// "eviction" finds "evict", etc. Stems are used as PREFIX matches in FTS, so
/// they don't need to be perfect English words.
pub fn get_stem_variants(term: &str) -> Vec<String> {
    let mut variants: IndexSet<String> = IndexSet::new();
    let t = term.to_lowercase();
    let len = utf16_len(&t);
    let drop = |n: usize| t[..t.len() - n].to_string();
    let doubled_final = |base: &str| {
        let mut cs = base.chars().rev();
        match (cs.next(), cs.next()) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    };
    let drop_last_char = |base: &str| {
        let mut s = base.to_string();
        s.pop();
        s
    };

    // -ing: caching→cach/cache, handling→handl/handle, running→run
    if t.ends_with("ing") && len > 5 {
        let base = drop(3);
        variants.insert(base.clone());
        variants.insert(format!("{base}e"));
        if utf16_len(&base) >= 2 && doubled_final(&base) {
            variants.insert(drop_last_char(&base));
        }
    }

    // -tion/-sion: eviction→evict, expression→express
    if (t.ends_with("tion") || t.ends_with("sion")) && len > 5 {
        variants.insert(drop(3));
    }

    // -ment: management→manage
    if t.ends_with("ment") && len > 6 {
        variants.insert(drop(4));
    }

    if t.ends_with("ies") && len > 4 {
        // -ies: entries→entry
        variants.insert(format!("{}y", drop(3)));
    } else if t.ends_with("es") && len > 4 {
        // -es: processes→process, classes→class
        variants.insert(drop(2));
    } else if t.ends_with('s') && !t.ends_with("ss") && len > 4 {
        // -s: errors→error (skip -ss endings like "class")
        variants.insert(drop(1));
    }

    // -ed: handled→handle, propagated→propagate, carried→carry
    if t.ends_with("ed") && !t.ends_with("eed") && len > 4 {
        variants.insert(drop(1));
        variants.insert(drop(2));
        if t.ends_with("ied") && len > 5 {
            variants.insert(format!("{}y", drop(3)));
        }
    }

    // -er: builder→build/builde, handler→handl/handle, getter→get
    if t.ends_with("er") && len > 4 {
        let base = drop(2);
        variants.insert(base.clone());
        variants.insert(format!("{base}e"));
        if utf16_len(&base) >= 2 && doubled_final(&base) {
            variants.insert(drop_last_char(&base));
        }
    }

    variants
        .into_iter()
        .filter(|v| utf16_len(v) >= 3 && *v != t)
        .collect()
}

static COMPOUND: LazyLock<Regex> = LazyLock::new(|| {
    // ASCII word boundaries, as in the JavaScript original.
    Regex::new(
        r"(?-u:\b)([a-zA-Z][a-zA-Z0-9]*(?:[A-Z][a-z]+)+|[A-Z][a-z]+(?:[A-Z][a-z]*)+)(?-u:\b)",
    )
    .unwrap()
});
static SNAKE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?-u:\b)([a-zA-Z][a-zA-Z0-9]*(?:_[a-zA-Z0-9]+)+)(?-u:\b)").unwrap()
});
static LOWER_UPPER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([a-z])([A-Z])").unwrap());
static ACRONYM_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([A-Z]+)([A-Z][a-z])").unwrap());
static UNDERSCORE_DOT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[_.]+").unwrap());
static NON_ALNUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-zA-Z0-9]+").unwrap());

/// Extract meaningful search terms from a natural language query. Splits
/// camelCase, PascalCase, snake_case, SCREAMING_SNAKE, and dot.notation into
/// individual tokens before filtering.
///
/// Preserves original compound identifiers (e.g. "scrapeLoop") alongside their
/// split parts so that FTS can match both the full symbol name and individual
/// words within it. With `stems`, also generates stem variants ("caching" →
/// "cache", "eviction" → "evict") so FTS prefix matching can find related
/// symbols. Path scoring passes `stems: false` — stems inflate path scores.
pub fn extract_search_terms(query: &str, stems: bool) -> Vec<String> {
    let mut tokens: IndexSet<String> = IndexSet::new();

    // First, extract and preserve compound identifiers before splitting
    // CamelCase: scrapeLoop, UserService, getCallGraph
    for m in COMPOUND.captures_iter(query) {
        let g = &m[1];
        if g.len() >= 3 {
            tokens.insert(g.to_lowercase());
        }
    }

    // snake_case: scrape_loop, user_service
    for m in SNAKE.captures_iter(query) {
        let g = &m[1];
        if g.len() >= 3 {
            tokens.insert(g.to_lowercase());
        }
    }

    // Split camelCase / PascalCase: "getUserName" → "get User Name"
    let camel_split = LOWER_UPPER.replace_all(query, "${1} ${2}");
    let camel_split = ACRONYM_END.replace_all(&camel_split, "${1} ${2}");
    // Replace underscores and dots with spaces (snake_case, dot.notation)
    let normalised = UNDERSCORE_DOT.replace_all(&camel_split, " ");
    // Split on any non-alphanumeric character
    for word in NON_ALNUM.split(&normalised).filter(|w| !w.is_empty()) {
        let lower = word.to_lowercase();
        if lower.len() < 3 || STOP_WORDS.contains(lower.as_str()) {
            continue;
        }
        tokens.insert(lower);
    }

    if stems {
        let mut extra: IndexSet<String> = IndexSet::new();
        for token in &tokens {
            for variant in get_stem_variants(token) {
                if !tokens.contains(&variant) && !STOP_WORDS.contains(variant.as_str()) {
                    extra.insert(variant);
                }
            }
        }
        tokens.extend(extra);
    }

    tokens.into_iter().collect()
}

/// Score path relevance to a query. Higher score = more relevant path.
pub fn score_path_relevance(
    file_path: &str,
    query: &str,
    project_name_tokens: Option<&HashSet<String>>,
    is_deprioritized: bool,
) -> i32 {
    let path_lower = file_path.to_lowercase();
    let file_name = basename(file_path).to_lowercase();
    let dir_name = dirname(file_path).to_lowercase();
    let mut score = 0;

    // Score per original query WORD, not per sub-token. A single PascalCase
    // word splits into many sub-tokens (a project name "SuperBizAgent" →
    // superbizagent / super / biz / agent) that all match the SAME path
    // segment, so summing per sub-token boosted that path 4× for one concept —
    // enough to bury the rest of the query's stack (#720). A word matches a
    // path level if ANY of its sub-tokens do, and counts ONCE.
    let all_words: Vec<&str> = query
        .split(codegraph_types::jscompat::is_js_space)
        .filter(|w| !w.is_empty())
        .collect();
    if all_words.is_empty() {
        return 0;
    }

    // A query word that just names the PROJECT carries no discriminative path
    // signal — drop it so the rest of the query decides the ranking (#720).
    // Only when OTHER words remain, so a bare project-name query still scores
    // on its path.
    let words: Vec<&str> = match project_name_tokens {
        Some(t) if !t.is_empty() => all_words
            .iter()
            .copied()
            .filter(|w| !t.contains(&normalize_name_token(w)))
            .collect(),
        _ => all_words.clone(),
    };
    let scored = if words.is_empty() { &all_words } else { &words };

    for word in scored {
        // Base terms only — stem variants inflate path scores by generating
        // many near-duplicate terms that all match the same path segments.
        let subtokens = extract_search_terms(word, false);
        if subtokens.is_empty() {
            continue;
        }
        // Exact filename match (strongest)
        if subtokens.iter().any(|t| file_name.contains(t.as_str())) {
            score += 10;
        }
        // Directory match, else general path match
        if subtokens.iter().any(|t| dir_name.contains(t.as_str())) {
            score += 5;
        } else if subtokens.iter().any(|t| path_lower.contains(t.as_str())) {
            score += 3;
        }
    }

    // Deprioritize test files unless the query is explicitly about tests, and
    // apply the same -15 to a path the project declared peripheral (#982).
    //
    // Two deliberate asymmetries, both pinned by tests:
    //  - the built-in test/fixture penalty is waived for a test-y query,
    //    because the tool inferred that classification; a `deprioritize`
    //    pattern is a standing statement by the project, so it is NOT waived.
    //  - a path that is both is docked ONCE, not twice.
    let query_lower = query.to_lowercase();
    let is_test_query = query_lower.contains("test") || query_lower.contains("spec");
    let off_target = (!is_test_query && is_test_file(file_path)) || is_deprioritized;
    if off_target {
        score -= 15;
    }

    score
}

static TEST_NAME_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[._-](test|tests|unittest|unittests|spec|specs)\.[a-z0-9]+$").unwrap()
});
static TEST_CAMEL_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:Test|Tests|TestCase|Tester|Spec|Specs)\.[A-Za-z0-9]+$").unwrap()
});
static TEST_SOURCE_SET_DIR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|/)[A-Za-z0-9]*(?:Test|Tests|Spec)/").unwrap());
static TEST_SUPPORT_DIR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:^|/)(?:[A-Za-z0-9_.]+[-_]test(?:s|ing)?|testdata|testutils?|test[-_]utils?|fakes?|mocks?|__mocks__|stubs)/",
    )
    .unwrap()
});
static UNIT_TEST_TREE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|/)(?:unittests|[A-Za-z0-9_.]+[-_]unittests?)/").unwrap());

/// Check if a file path looks like a test file.
///
/// "Test" here is the wide reading: anything that is not production code,
/// including examples, samples, benchmarks and fixtures. That is the right
/// default for ranking — none of them are what a search is looking for — but it
/// is the wrong set to put under a heading that says "Tests". A caller that
/// means literally a test suite wants [`is_test_path`].
pub fn is_test_file(file_path: &str) -> bool {
    is_test_path(file_path) || matches_non_production_dir(&file_path.to_lowercase())
}

/// Check if a file path names a TEST — a suite that exercises other code.
///
/// The narrow half of [`is_test_file`]: the filename and directory conventions
/// every ecosystem uses for its test suites, and nothing else. An example, a
/// benchmark or a fixture is not a test, and a list headed "Tests" that
/// contains them is telling the reader something untrue.
pub fn is_test_path(file_path: &str) -> bool {
    let lower = file_path.to_lowercase();
    // Original case — needed for camelCase boundaries.
    let file_name = basename(file_path);
    let lower_name = file_name.to_lowercase();

    // --- Filename patterns ---
    if lower_name.starts_with("test_")                                      // python: test_foo.py
        || lower_name.starts_with("test.")
        // separator-delimited: foo_test.go, foo.test.ts, foo-spec.rb, and
        // Google's C++ foo_unittest.cc. A bare `unittest.go` is no test.
        || TEST_NAME_SUFFIX.is_match(&lower_name)
        // CamelCase suffix (Java/Kotlin/Swift/C#/Scala): FooTest.kt,
        // BarTests.swift, BazSpec.scala. Capital-led so "latest.kt" is NOT matched.
        || TEST_CAMEL_SUFFIX.is_match(file_name)
    {
        return true;
    }

    // --- Directory patterns ---
    [
        "/tests/", "/test/", "/__tests__/", "/spec/", "/specs/", "/testlib/", "/testing/", "/e2e/",
    ]
    .iter()
    .any(|d| lower.contains(d))
        || ["test/", "tests/", "spec/", "specs/", "e2e/"].iter().any(|d| lower.starts_with(d))
        // CamelCase test source-set dirs (Kotlin Multiplatform / Gradle /
        // Xcode): jvmTest/, commonTest/, androidTest/, integrationTest/.
        || TEST_SOURCE_SET_DIR.is_match(file_path)
        // Test-support modules and doubles by directory name: Gradle's
        // `core/data-test/`, Go's `testdata/`, `testutil(s)/`, `fakes/`, `mocks/`.
        || TEST_SUPPORT_DIR.is_match(&lower)
        // Unit-test trees: LLVM's and Breakpad's `unittests/`. Not a bare
        // `unittest/`: CPython's `Lib/unittest/` is the framework itself.
        || UNIT_TEST_TREE.is_match(&lower)
}

/// Check if a path is in a non-production directory (integration, sample,
/// example, etc.). Handles absolute paths (/foo/integration/bar) and relative
/// ones (integration/bar).
fn matches_non_production_dir(lower_path: &str) -> bool {
    const DIRS: [&str; 11] = [
        "integration",
        "sample",
        "samples",
        "example",
        "examples",
        "fixture",
        "fixtures",
        "benchmark",
        "benchmarks",
        "demo",
        "demos",
    ];
    // Only the project layout above a `src/` counts, never the package path
    // below it: `core/data/src/main/kotlin/com/google/samples/apps/…` is a
    // Google sample by name and production code by layout.
    let scope = match lower_path.find("/src/") {
        Some(src) => &lower_path[..=src],
        None if lower_path.starts_with("src/") => "",
        None => lower_path,
    };
    DIRS.iter()
        .any(|dir| scope.contains(&format!("/{dir}/")) || scope.starts_with(&format!("{dir}/")))
}

/// Bonus when a node's name matches the search query. Exact matches get the
/// largest boost; prefix matches get smaller boosts. Multi-word queries also
/// check individual term matches against the name.
pub fn name_match_bonus(node_name: &str, query: &str) -> i32 {
    NameMatchQuery::new(query).score_name(node_name)
}

/// Query-invariant name-scoring state. Native search prepares it once for an
/// entire candidate page instead of repeating tokenization for every symbol.
pub struct NameMatchQuery {
    raw_terms: Vec<String>,
    query_tokens: Vec<String>,
    query_lower: String,
}

impl NameMatchQuery {
    pub fn new(query: &str) -> Self {
        let split_camel = LOWER_UPPER.replace_all(query, "${1} ${2}");
        let raw_terms = split_camel
            .split(|c: char| {
                codegraph_types::jscompat::is_js_space(c) || matches!(c, '_' | '.' | '-')
            })
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase)
            .filter(|t| utf16_len(t) >= 2)
            .collect();
        let query_tokens = query
            .split(codegraph_types::jscompat::is_js_space)
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase)
            .filter(|t| utf16_len(t) >= 2)
            .collect();
        let query_lower = query
            .chars()
            .filter(|c| !codegraph_types::jscompat::is_js_space(*c))
            .collect::<String>()
            .to_lowercase();
        Self {
            raw_terms,
            query_tokens,
            query_lower,
        }
    }

    pub fn score_name(&self, node_name: &str) -> i32 {
        let name_lower = node_name.to_lowercase();
        if name_lower == self.query_lower {
            return 80;
        }
        if self.query_tokens.len() > 1 && self.query_tokens.contains(&name_lower) {
            return 60;
        }
        if name_lower.starts_with(&self.query_lower) {
            let ratio = utf16_len(&self.query_lower) as f64 / utf16_len(&name_lower) as f64;
            return (10.0 + 30.0 * ratio).round() as i32;
        }
        if self.raw_terms.len() > 1
            && self
                .raw_terms
                .iter()
                .all(|t| name_lower.contains(t.as_str()))
        {
            return 15;
        }
        if name_lower.contains(&self.query_lower) {
            return 10;
        }
        0
    }
}

/// Kind-based bonus for search ranking: functions and classes are typically
/// more relevant than variables/imports.
pub fn kind_bonus(kind: NodeKind) -> i32 {
    match kind {
        NodeKind::Function | NodeKind::Method => 10,
        NodeKind::Class => 8,
        NodeKind::Interface => 9,
        NodeKind::TypeAlias | NodeKind::Struct | NodeKind::Union => 6,
        NodeKind::Trait | NodeKind::Protocol | NodeKind::Route => 9,
        NodeKind::Enum => 5,
        NodeKind::Component => 8,
        NodeKind::Module | NodeKind::Namespace => 4,
        NodeKind::Property | NodeKind::Field | NodeKind::Constant | NodeKind::EnumMember => 3,
        NodeKind::Variable => 2,
        NodeKind::Import | NodeKind::Export => 1,
        NodeKind::Parameter | NodeKind::File => 0,
    }
}

/// Whether a query token looks like a code identifier the user deliberately
/// typed (camelCase / PascalCase-with-internal-caps / snake_case / has a digit)
/// rather than a plain dictionary word ("flat", "object", "screen").
///
/// Used to decide whether an EXACT name match earns the "the user named this
/// symbol" exemption from single-term dampening. A common English word that
/// happens to exact-match an unrelated symbol — the query "flat object"
/// matching a constant named `FLAT` — must NOT get that exemption.
///
/// Classifies the token AS THE USER TYPED IT, not the matched symbol's name. A
/// leading-capital-only word ("Screen", "Zustand") is also treated as a plain
/// word — sentence-start capitalization and proper nouns aren't reliable
/// identifier signals.
pub fn is_distinctive_identifier(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    // snake_case / SCREAMING_SNAKE, or an embedded digit → a deliberate identifier.
    if token.chars().any(|c| c == '_' || c.is_ascii_digit()) {
        return true;
    }
    // An uppercase letter anywhere AFTER the first char → a camelCase/PascalCase
    // boundary (setLastEmail, OrgUserStore) or an acronym (REST, HTTP).
    token.chars().skip(1).any(|c| c.is_ascii_uppercase())
}

/// `trim()` convenience re-export for callers that only import this module.
pub fn trim(s: &str) -> &str {
    js_trim(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_compounds_and_their_parts() {
        let terms = extract_search_terms("scrapeLoop", false);
        assert_eq!(terms, ["scrapeloop", "scrape", "loop"]);
        let terms = extract_search_terms("user_service handler", false);
        assert_eq!(terms[0], "user_service");
        assert!(terms.contains(&"user".to_string()) && terms.contains(&"service".to_string()));
        assert!(terms.contains(&"handler".to_string()));
    }

    #[test]
    fn stop_words_and_short_words_are_dropped() {
        let terms = extract_search_terms("how does the cache work", false);
        assert_eq!(terms, ["cache"]);
    }

    #[test]
    fn stems_expand_the_term_list() {
        // Order verified against the TypeScript implementation (Set insertion order).
        let terms = extract_search_terms("caching eviction", true);
        assert_eq!(terms, ["caching", "eviction", "cach", "cache", "evict"]);
        assert_eq!(get_stem_variants("entries"), ["entry"]);
        assert_eq!(get_stem_variants("classes"), ["class"]);
        assert!(get_stem_variants("class").is_empty());
    }

    #[test]
    fn name_bonus_tiers() {
        assert_eq!(name_match_bonus("CacheBuilder", "CacheBuilder"), 80);
        assert_eq!(name_match_bonus("build", "CacheBuilder build"), 60);
        assert_eq!(name_match_bonus("PodGCControllerOptions", "Pod"), 14);
        assert_eq!(name_match_bonus("fooCacheBuilder", "cache builder"), 15);
        assert_eq!(name_match_bonus("xcachex", "cache"), 10);
        assert_eq!(name_match_bonus("zzz", "cache"), 0);
    }

    #[test]
    fn kind_bonus_table() {
        assert_eq!(kind_bonus(NodeKind::Function), 10);
        assert_eq!(kind_bonus(NodeKind::Interface), 9);
        assert_eq!(kind_bonus(NodeKind::File), 0);
        assert_eq!(kind_bonus(NodeKind::EnumMember), 3);
    }

    #[test]
    fn test_path_conventions() {
        for p in [
            "test_foo.py",
            "foo_test.go",
            "foo.test.ts",
            "foo-spec.rb",
            "src/FooTest.kt",
            "a/b/tests/c.rs",
            "tests/c.rs",
            "app/jvmTest/X.kt",
            "core/data-test/X.kt",
            "pkg/testdata/x.go",
            "llvm/unittests/ADT/X.cpp",
            "src/foo_unittest.cc",
        ] {
            assert!(is_test_path(p), "{p}");
        }
        for p in [
            "src/latest.kt",
            "src/manifest.kt",
            "Lib/unittest/case.py",
            "cmd/promtool/unittest.go",
            "src/app.ts",
        ] {
            assert!(!is_test_path(p), "{p}");
        }
        // The wide reading adds examples/samples/benchmarks…
        assert!(is_test_file("examples/basic/main.ts"));
        assert!(is_test_file("docs/benchmarks/run.js"));
        assert!(!is_test_path("examples/basic/main.ts"));
        // …but only above `src/`.
        assert!(!is_test_file(
            "core/data/src/main/kotlin/com/google/samples/apps/X.kt"
        ));
    }

    #[test]
    fn path_scoring() {
        assert_eq!(
            score_path_relevance("src/auth/login.ts", "login", None, false),
            10 + 3
        );
        assert_eq!(
            score_path_relevance("src/auth/login.ts", "auth", None, false),
            5
        );
        // Tests are docked unless the query asks for tests; deprioritized paths always are.
        assert_eq!(
            score_path_relevance("tests/login.test.ts", "login", None, false),
            10 + 3 - 15
        );
        // "login": file name +10, path +3; "tests": directory +5; no penalty for a test query.
        assert_eq!(
            score_path_relevance("tests/login.test.ts", "login tests", None, false),
            10 + 3 + 5
        );
        let tokens: HashSet<String> = ["superbizagent".to_string()].into_iter().collect();
        assert_eq!(
            score_path_relevance(
                "SuperBizAgentWeb/a.ts",
                "SuperBizAgent",
                Some(&tokens),
                false
            ),
            // sole word is the project name → still scored, so the path wins on it
            score_path_relevance("SuperBizAgentWeb/a.ts", "SuperBizAgent", None, false)
        );
        assert_eq!(score_path_relevance("src/a.ts", "", None, false), 0);
    }

    #[test]
    fn distinctive_identifiers() {
        assert!(is_distinctive_identifier("setLastEmail"));
        assert!(is_distinctive_identifier("snake_case"));
        assert!(is_distinctive_identifier("v2"));
        assert!(!is_distinctive_identifier("flat"));
        assert!(!is_distinctive_identifier("Screen"));
        assert!(is_distinctive_identifier("REST"));
    }
}
