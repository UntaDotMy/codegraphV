//! Pure helpers and constants behind `codegraph_explore` — port of the top-level
//! functions in `src/mcp/tools.ts` (lines 183–1500). Byte parity with the
//! TypeScript output is the contract: JS `length` is UTF-16, sorts are stable,
//! and every number is an `f64` where TypeScript does arithmetic on it.
use codegraph_core::CodeGraph;
use codegraph_graph::symbol_lookup::{is_qualified_symbol, last_qualifier_part, matches_symbol};
use codegraph_graph::type_hierarchy::locale_compare;
use codegraph_jsre::re;
use codegraph_search::query_utils::is_test_file;
use codegraph_types::jscompat::utf16_len;
use codegraph_types::paths::{validate_path_within_root, ValidateOptions};
use codegraph_types::Node;
use indexmap::{IndexMap, IndexSet};
use std::collections::{HashMap, HashSet};

/// An inclusive 1-based line range (`ExploreLineRange`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRange {
    pub start: i64,
    pub end: i64,
}

/// Erlang-native spellings (`mod:fn/3`) into the shapes the pipeline understands.
pub fn normalize_query_spelling(query: &str) -> String {
    let arity = re!(r"\b([A-Za-z_][\w@]*)\/(\d{1,3})(?=$|[\s,()[\]/])", "g");
    let step = arity.replace_all(query, |c| Some(c.text(1).unwrap_or("").to_string()));
    let colon = re!(
        r"(^|[\s,()[\]])(?!(?:kind|lang|language|path|name):)([a-z_][\w@]*):([A-Za-z_][\w@]*)(?=$|[\s,()[\]])",
        "g"
    );
    colon.replace_all(&step, |c| {
        Some(format!("{}{}.{}", c.text(1).unwrap_or(""), c.text(2).unwrap_or(""), c.text(3).unwrap_or("")))
    })
}

/// Does this query-named span point at a regular file inside the project?
pub fn path_is_project_file(project_root: &str, rel_path: &str) -> bool {
    match validate_path_within_root(project_root, rel_path, ValidateOptions::default()) {
        Some(abs) => std::fs::metadata(abs).map(|m| m.is_file()).unwrap_or(false),
        None => false,
    }
}

/// Kinds that name a thing without defining it in the file that holds them.
const NOT_A_DEFINITION: [&str; 4] = ["file", "import", "export", "parameter"];

/// Which indexed files define a symbol spelled like this query token?
pub fn files_defining_symbol(cg: &CodeGraph, symbol: &str) -> Vec<String> {
    let nodes = if is_qualified_symbol(symbol) {
        let reader = codegraph_graph::go_package::GoPackageReader::new(cg.project_root());
        let go_package_of = |file: &str| reader.package_of(file);
        match cg.get_nodes_by_name(&last_qualifier_part(symbol)) {
            Ok(nodes) => nodes.into_iter().filter(|n| matches_symbol(n, symbol, Some(&go_package_of))).collect(),
            Err(_) => return Vec::new(),
        }
    } else {
        match cg.get_nodes_by_name(symbol) {
            Ok(nodes) => nodes,
            Err(_) => return Vec::new(),
        }
    };
    nodes
        .into_iter()
        .filter(|n| !NOT_A_DEFINITION.contains(&n.kind.as_str()))
        .map(|n| n.file_path)
        .collect()
}

/// Recommended number of explore calls for a project of this size.
pub fn get_explore_budget(file_count: f64) -> u32 {
    if file_count < 500.0 {
        1
    } else if file_count < 5000.0 {
        2
    } else if file_count < 15000.0 {
        3
    } else if file_count < 25000.0 {
        4
    } else {
        5
    }
}

/// Adaptive output budget for `codegraph_explore`, scaled to project size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExploreOutputBudget {
    pub max_output_chars: f64,
    pub default_max_files: f64,
    pub max_chars_per_file: f64,
    pub gap_threshold: f64,
    pub max_symbols_in_file_header: usize,
    pub max_edges_per_relationship_kind: usize,
    pub include_relationships: bool,
    pub include_additional_files: bool,
    pub include_completeness_signal: bool,
    pub include_budget_note: bool,
}

/// Tiered budget; a larger tier never gets a smaller `max_chars_per_file`.
pub fn get_explore_output_budget(file_count: f64) -> ExploreOutputBudget {
    let tier = |max_output_chars: f64,
                default_max_files: f64,
                max_chars_per_file: f64,
                gap_threshold: f64,
                symbols: usize,
                meta: bool| ExploreOutputBudget {
        max_output_chars,
        default_max_files,
        max_chars_per_file,
        gap_threshold,
        max_symbols_in_file_header: symbols,
        max_edges_per_relationship_kind: symbols,
        include_relationships: meta,
        include_additional_files: meta,
        include_completeness_signal: meta,
        include_budget_note: meta,
    };
    if file_count < 150.0 {
        let mut b = tier(13000.0, 4.0, 3800.0, 7.0, 5, false);
        b.max_edges_per_relationship_kind = 4;
        b
    } else if file_count < 500.0 {
        tier(18000.0, 5.0, 3800.0, 8.0, 6, false)
    } else if file_count < 5000.0 {
        tier(24000.0, 8.0, 6500.0, 12.0, 10, true)
    } else {
        tier(24000.0, 8.0, 7000.0, 15.0, 15, true)
    }
}

// ── Relevance scoring (CG-10 / #1500) ──────────────────────────────────────

/// How strongly a match on a symbol of this kind corroborates its file.
pub fn relevance_kind_weight(kind: &str) -> Option<f64> {
    Some(match kind {
        "function" | "method" | "class" | "struct" | "union" | "interface" | "trait" | "protocol"
        | "component" | "route" | "enum" | "type_alias" | "constructor" => 1.0,
        "namespace" | "module" => 0.8,
        "property" | "field" => 0.5,
        "enum_member" => 0.35,
        "file" => 0.5,
        "constant" => 0.35,
        "variable" => 0.3,
        "parameter" => 0.15,
        _ => return None,
    })
}
pub const DEFAULT_RELEVANCE_KIND_WEIGHT: f64 = 0.5;
pub const TYPE_MEMBER_RELEVANCE_WEIGHT: f64 = 0.5;
pub const WEAK_RELEVANCE_KINDS: [&str; 6] = ["constant", "variable", "parameter", "field", "property", "enum_member"];
pub const ISOLATED_WEAK_KIND_WEIGHT: f64 = 0.08;
pub const RELEVANCE_USAGE_EDGES: [&str; 10] = [
    "calls", "references", "extends", "implements", "overrides", "instantiates", "returns", "type_of", "decorates",
    "navigates",
];
pub const PERIPHERAL_SCORE_CAP: f64 = 5.0;
pub const GENERATED_RANK_PENALTY: f64 = 0.3;
pub const LOW_VALUE_RANK_PENALTY: f64 = 0.5;
pub const AMBIENT_DECLARATION_RANK_PENALTY: f64 = 0.5;
pub const DECLARATION_KINDS: [&str; 5] = ["interface", "type_alias", "enum", "enum_member", "namespace"];
pub const SCORE_FLOOR_ABSOLUTE: f64 = 1.0;
pub const SCORE_FLOOR_FRACTION_OF_TOP: f64 = 0.2;
pub const SCORE_FLOOR_MAX: f64 = 10.0;
pub const SCORE_FLOOR_KEEP_MIN: usize = 3;

// ── Score-proportional byte allocation (CG-12 / #1500) ─────────────────────
pub const CLIFF_FRACTION: f64 = 0.15;
pub const CLIFF_MAX: f64 = SCORE_FLOOR_MAX;
pub const MIN_CHARS: f64 = 700.0;
pub const MAX_SHARE: f64 = 0.7;
pub const FILE_OVERHEAD: f64 = 200.0;
pub const SPINE_WEIGHT_BOOST: f64 = 2.0;
pub const WHOLE_FILE_GRACE_FRACTION: f64 = 0.15;
pub const WHOLE_FILE_GRACE_MAX: f64 = 800.0;
pub const WHOLE_FILE_BUY_FRACTION: f64 = 0.6;
pub const WHOLE_FILE_BUY_OVERSHOOT_FRACTION: f64 = 0.15;

/// One candidate file's allocation inputs, in final rank order.
#[derive(Debug, Clone)]
pub struct AllocationCandidate {
    pub path: String,
    pub score: f64,
    pub worth: f64,
    pub spine: bool,
    pub pinned: bool,
    pub named: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ExploreAllocation {
    /// path → chars of source it may render (admitted files only, rank order).
    pub allowances: IndexMap<String, f64>,
    pub cliffed: Vec<String>,
    pub cliff_at: f64,
    pub pool: f64,
}

/// `Math.max(...values)` — `-Infinity` for an empty list, NaN-propagating.
pub fn js_max(values: impl IntoIterator<Item = f64>) -> f64 {
    let mut out = f64::NEG_INFINITY;
    for v in values {
        if v.is_nan() {
            return f64::NAN;
        }
        if v > out {
            out = v;
        }
    }
    out
}

/// `Math.min(a, b)` with JavaScript's NaN propagation.
pub fn js_min2(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a < b {
        a
    } else {
        b
    }
}

/// `Math.max(a, b)` with JavaScript's NaN propagation.
pub fn js_max2(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a > b {
        a
    } else {
        b
    }
}

/// `Math.round`: halves round toward +Infinity.
pub fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// Split `budget.max_output_chars` across ranked candidates in proportion to relevance.
pub fn allocate_explore_budget(
    candidates: &[AllocationCandidate],
    budget: &ExploreOutputBudget,
    max_files: usize,
) -> ExploreAllocation {
    if candidates.is_empty() {
        return ExploreAllocation::default();
    }
    let weight_of = |c: &AllocationCandidate| {
        let w = js_max2(0.0, c.score)
            * js_max2(0.0, js_min2(1.0, c.worth))
            * if c.spine { SPINE_WEIGHT_BOOST } else { 1.0 };
        if w.is_finite() {
            w
        } else {
            0.0
        }
    };
    let mut raw: HashMap<&str, f64> = HashMap::new();
    for c in candidates {
        raw.insert(&c.path, weight_of(c));
    }
    // A Map built from entries keeps the LAST value per duplicate key, in first-key order.
    let top_raw = js_max(dedup_values(candidates, |c| raw[c.path.as_str()]));
    let mut weights: HashMap<&str, f64> = HashMap::new();
    for c in candidates {
        let r = raw[c.path.as_str()];
        weights.insert(&c.path, if c.pinned { js_max2(js_max2(r, top_raw), 1.0) } else { r });
    }
    let top_weight = js_max(dedup_values(candidates, |c| weights[c.path.as_str()]));
    if !(top_weight > 0.0) {
        return ExploreAllocation::default();
    }
    let weight = |c: &AllocationCandidate| weights.get(c.path.as_str()).copied().unwrap_or(0.0);

    let cliff_at = js_min2(top_weight * CLIFF_FRACTION, CLIFF_MAX);
    let mut cliffed: Vec<String> = Vec::new();
    let mut admitted: Vec<&AllocationCandidate> = Vec::new();
    for c in candidates {
        if !c.spine && !c.pinned && weight(c) < cliff_at {
            cliffed.push(c.path.clone());
        } else {
            admitted.push(c);
        }
    }
    if admitted.is_empty() {
        admitted = vec![&candidates[0]];
        // `splice(indexOf(...), 1)`: indexOf is never -1 here, the first candidate was cliffed.
        if let Some(i) = cliffed.iter().position(|p| *p == candidates[0].path) {
            cliffed.remove(i);
        }
    }
    for c in admitted.iter().skip(max_files) {
        cliffed.push(c.path.clone());
    }
    admitted.truncate(max_files);

    let affordable = js_max2(1.0, (budget.max_output_chars / (MIN_CHARS + FILE_OVERHEAD)).floor()) as usize;
    if admitted.len() > affordable {
        let mut by_weight = admitted.clone();
        by_weight.sort_by(|a, b| js_cmp(weight(b) - weight(a)));
        let mut keep: IndexSet<&str> = by_weight.iter().take(affordable).map(|c| c.path.as_str()).collect();
        for c in &admitted {
            if c.spine || c.pinned {
                keep.insert(&c.path);
            }
        }
        for c in &admitted {
            if !keep.contains(c.path.as_str()) {
                cliffed.push(c.path.clone());
            }
        }
        admitted.retain(|c| keep.contains(c.path.as_str()));
    }

    let mut allowances = IndexMap::new();
    let pool = js_max2(0.0, budget.max_output_chars - FILE_OVERHEAD * admitted.len() as f64);
    let total: f64 = admitted.iter().fold(0.0, |s, c| s + weight(c));
    if total <= 0.0 || admitted.is_empty() {
        return ExploreAllocation { allowances, cliffed, cliff_at, pool };
    }
    let ceiling = js_round(budget.max_output_chars * MAX_SHARE);
    let floors = js_min2(pool, MIN_CHARS * admitted.len() as f64);
    let remainder = js_max2(0.0, pool - floors);
    for c in &admitted {
        let share = (floors / admitted.len() as f64).floor() + ((remainder * weight(c)) / total).floor();
        let named = c.named || c.pinned;
        allowances.insert(c.path.clone(), if named { share } else { js_min2(share, ceiling) });
    }
    ExploreAllocation { allowances, cliffed, cliff_at, pool }
}

/// The values of a JS `Map` built from `candidates.map(c => [c.path, f(c)])`: one per
/// distinct path, holding the last value written.
fn dedup_values(candidates: &[AllocationCandidate], f: impl Fn(&AllocationCandidate) -> f64) -> Vec<f64> {
    let mut map: IndexMap<&str, f64> = IndexMap::new();
    for c in candidates {
        map.insert(&c.path, f(c));
    }
    map.into_values().collect()
}

/// A comparator result from a JS numeric difference (NaN compares equal, as V8's sort treats it).
pub fn js_cmp(diff: f64) -> std::cmp::Ordering {
    if diff < 0.0 {
        std::cmp::Ordering::Less
    } else if diff > 0.0 {
        std::cmp::Ordering::Greater
    } else {
        std::cmp::Ordering::Equal
    }
}

/// `CODEGRAPH_EXPLORE_LINENUMS` (default on).
pub fn explore_line_numbers_enabled() -> bool {
    std::env::var("CODEGRAPHV_EXPLORE_LINENUMS").map_or(true, |v| v != "0")
}

/// `CODEGRAPH_ADAPTIVE_EXPLORE` (default on).
pub fn adaptive_explore_enabled() -> bool {
    std::env::var("CODEGRAPHV_ADAPTIVE_EXPLORE").map_or(true, |v| v != "0" && v != "false")
}

/// Prefix each line with its 1-based number and a tab (`cat -n`).
pub fn number_source_lines(slice: &str, first_line_number: i64) -> String {
    slice
        .split('\n')
        .enumerate()
        .map(|(i, line)| format!("{}\t{}", first_line_number + i as i64, line))
        .collect::<Vec<_>>()
        .join("\n")
}

pub const FILE_SECTION_PREFIX: &str = "**`";
pub const SUMMARY_SENTINEL: &str = "[[codegraph-explore-summary]]";

pub fn file_section_header(file_path: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        format!("{FILE_SECTION_PREFIX}{file_path}`**")
    } else {
        format!("{FILE_SECTION_PREFIX}{file_path}`** — {suffix}")
    }
}

pub const POINTER_HEADER: &str = "**Not shown above — explore these names for their source**";
pub const POINTER_MAX_FILES: usize = 10;
pub const ELIDED_SYMBOL_CAP: usize = 6;
pub const ANCHOR_CALLABLE_KINDS: [&str; 4] = ["method", "function", "constructor", "component"];
pub const ANCHOR_LINE_CONTEXT: i64 = 15;
pub const MAX_BODY_FOCUS_LINES: usize = 6;

/// A symbol a trim dropped, named so the agent can follow up by name.
#[derive(Debug, Clone, PartialEq)]
pub struct ElidedSymbolRef {
    pub name: String,
    pub kind: String,
    pub start_line: i64,
}

/// Indexed symbols whose definition starts strictly between two rendered spans.
pub fn symbols_between_ranges(nodes: &[Node], from_end: i64, to_start: i64) -> Vec<ElidedSymbolRef> {
    if to_start <= from_end + 1 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for n in nodes {
        let kind = n.kind.as_str();
        if kind == "import" || kind == "export" {
            continue;
        }
        let start = n.start_line as i64;
        if start <= from_end || start >= to_start {
            continue;
        }
        if !seen.insert(n.name.clone()) {
            continue;
        }
        out.push(ElidedSymbolRef { name: n.name.clone(), kind: kind.to_string(), start_line: start });
    }
    out.sort_by_key(|s| s.start_line);
    out
}

/// Symbols whose start line no emitted range covers.
pub fn symbols_not_in_ranges(candidates: &[ElidedSymbolRef], ranges: &[LineRange]) -> Vec<ElidedSymbolRef> {
    let mut out: Vec<ElidedSymbolRef> = if ranges.is_empty() {
        candidates.to_vec()
    } else {
        let mut seen = std::collections::HashSet::new();
        let mut kept = Vec::new();
        for n in candidates {
            if seen.contains(&n.name) {
                continue;
            }
            if ranges.iter().any(|r| n.start_line >= r.start && n.start_line <= r.end) {
                continue;
            }
            seen.insert(n.name.clone());
            kept.push(n.clone());
        }
        kept
    };
    out.sort_by_key(|s| s.start_line);
    out
}

pub const BARE_GAP_MARKER: &str = "\n\n... (gap) ...\n\n";

/// Gap marker between two slices of one file, naming the symbols in the hole.
pub fn format_gap_marker(file_path: &str, elided: &[ElidedSymbolRef]) -> String {
    if elided.is_empty() {
        return BARE_GAP_MARKER.to_string();
    }
    let shown = &elided[..elided.len().min(ELIDED_SYMBOL_CAP)];
    let more = elided.len() - shown.len();
    let mut names = shown
        .iter()
        .map(|s| format!("{} ({}:{})", s.name, file_path, s.start_line))
        .collect::<Vec<_>>()
        .join(", ");
    if more > 0 {
        names.push_str(&format!(", +{more} more"));
    }
    format!("\n\n... (gap: {names}) ...\n\n")
}

/// The note a body cut short carries where it is cut.
pub fn elided_body_note(file_path: &str, name: &str, from: i64, to: i64) -> String {
    format!("\n… lines {from}-{to} of `{name}` elided — codegraphV_explore `{file_path}:{from}-{to}` returns them")
}

/// One rendered slice of a file.
#[derive(Debug, Clone)]
pub struct RenderedPart {
    pub range: LineRange,
    pub text: String,
}

/// Join rendered parts with gap markers naming what the trim skipped, while `spare_chars` lasts.
pub fn join_parts_with_named_gaps(file_path: &str, parts: &[RenderedPart], nodes: &[Node], spare_chars: f64) -> String {
    let Some(first) = parts.first() else { return String::new() };
    let mut out = first.text.clone();
    let mut spare = spare_chars;
    let bare = utf16_len(BARE_GAP_MARKER) as f64;
    for pair in parts.windows(2) {
        let named = format_gap_marker(file_path, &symbols_between_ranges(nodes, pair[0].range.end, pair[1].range.start));
        let extra = utf16_len(&named) as f64 - bare;
        if extra <= spare {
            out.push_str(&named);
            spare -= extra;
        } else {
            out.push_str(BARE_GAP_MARKER);
        }
        out.push_str(&pair[1].text);
    }
    out
}

/// Prefer symbols the trim dropped when filling the per-file header's named slots.
pub fn bias_header_symbols(symbols: &[String], elided: &[ElidedSymbolRef], cap: usize) -> (Vec<String>, usize) {
    let elided_labels: Vec<String> = elided.iter().map(|s| format!("{}({})", s.name, s.kind)).collect();
    let elided_set: HashSet<&str> = elided_labels.iter().map(String::as_str).collect();
    let elided_names: HashSet<&str> = elided.iter().map(|s| s.name.as_str()).collect();
    let mut counts: IndexMap<String, f64> = IndexMap::new();
    for s in symbols {
        *counts.entry(s.clone()).or_insert(0.0) += 1.0;
    }
    for label in &elided_labels {
        counts.entry(label.clone()).or_insert(1.0);
    }
    let mut elided_rank: HashMap<String, f64> = HashMap::new();
    for (i, s) in elided.iter().enumerate() {
        elided_rank.insert(format!("{}({})", s.name, s.kind), i as f64);
        elided_rank.entry(s.name.clone()).or_insert(i as f64);
    }
    let score = |label: &str| -> (f64, f64, f64) {
        let name = re!(r"\(.*\)$").replace_first(label, |_| String::new());
        let preferred = if elided_set.contains(label) || elided_names.contains(name.as_str()) { 1.0 } else { 0.0 };
        let rank = elided_rank.get(label).or_else(|| elided_rank.get(&name)).copied().unwrap_or(9999.0);
        (preferred, counts.get(label).copied().unwrap_or(0.0), -rank)
    };
    let mut sorted: Vec<String> = counts.keys().cloned().collect();
    sorted.sort_by(|a, b| {
        let (pa, ca, ra) = score(a);
        let (pb, cb, rb) = score(b);
        let d = first_nonzero(&[pb - pa, cb - ca, rb - ra]);
        if d != 0.0 {
            js_cmp(d)
        } else {
            locale_compare(a, b)
        }
    });
    let shown: Vec<String> = sorted.iter().take(cap).cloned().collect();
    let omitted = sorted.len() - shown.len();
    (shown, omitted)
}

/// `a || b || c` over comparator differences: the first one that is not zero (NaN counts as zero).
pub fn first_nonzero(diffs: &[f64]) -> f64 {
    diffs.iter().copied().find(|d| *d != 0.0 && !d.is_nan()).unwrap_or(0.0)
}

/// One pointer line: the file plus enough symbol names to make it nameable.
pub fn pointer_line_for(file_path: &str, nodes: &[Node]) -> String {
    const POINTER_SYMBOLS: usize = 6;
    let named: Vec<&Node> = nodes.iter().filter(|n| !matches!(n.kind.as_str(), "import" | "export")).collect();
    let pool: Vec<&Node> = if named.is_empty() { nodes.iter().collect() } else { named };
    let shown = &pool[..pool.len().min(POINTER_SYMBOLS)];
    let more = pool.len() - shown.len();
    let mut symbols = shown.iter().map(|n| format!("{}:{}", n.name, n.start_line)).collect::<Vec<_>>().join(", ");
    if more > 0 {
        symbols.push_str(&format!(", +{more} more"));
    }
    format!("- {file_path}: {symbols}")
}

/// A note in its two wordings: `complete` and `trimmed` (never longer than `complete`).
#[derive(Debug, Clone, Copy)]
pub struct NotePair {
    pub complete: &'static str,
    pub trimmed: &'static str,
}

impl NotePair {
    pub fn pick(&self, trimmed: bool) -> &'static str {
        if trimmed {
            self.trimmed
        } else {
            self.complete
        }
    }
}

pub const NOTE_LOST: NotePair = NotePair {
    complete: "> (Trailing pointer list omitted for size. The source above is complete and verbatim — treat it as already Read. For anything this call did not cover, run another codegraphV_explore with the specific names rather than reading those files.)",
    trimmed: "> (Trailing pointer list omitted for size. The source above is verbatim — treat it as already Read. For symbols its gap markers name, and anything else not covered, run another codegraphV_explore with those names rather than reading.)",
};
pub const NOTE_CUT: NotePair = NotePair {
    complete: "\n\n> (Trailing notes omitted for size. The source above is complete and verbatim — treat it as already Read. For anything this call did not cover, run another codegraphV_explore with the specific names rather than reading those files.)",
    trimmed: "\n\n> (Trailing notes omitted for size. The source above is verbatim — treat it as already Read. For symbols its gap markers name, and anything else not covered, run another codegraphV_explore with those names rather than reading.)",
};
pub const NOTE_TRUNCATED: NotePair = NotePair {
    complete: "\n\n... (output truncated to budget; the source above is complete and verbatim — treat it as already Read. For any area not covered, run another codegraphV_explore with the specific names — do NOT Read these files.)",
    trimmed: "\n\n... (output truncated to budget; the source above is verbatim — treat it as already Read. For names its gap markers list, or any area not covered, run another codegraphV_explore — do NOT Read these files.)",
};

/// One symbol a file section set out to deliver.
#[derive(Debug, Clone, PartialEq)]
pub struct WantedSpan {
    pub name: String,
    pub kind: String,
    pub start: i64,
    pub end: i64,
    pub importance: f64,
    pub spine: bool,
    pub qualified_name: Option<String>,
}

/// `mergeRanges` (explore-dedup.ts): valid ranges sorted and coalesced, adjacency included.
pub fn merge_ranges(ranges: &[LineRange]) -> Vec<LineRange> {
    let mut valid: Vec<LineRange> = ranges.iter().copied().filter(|r| r.end >= r.start && r.start >= 1).collect();
    valid.sort_by(|a, b| a.start.cmp(&b.start).then(a.end.cmp(&b.end)));
    let mut out: Vec<LineRange> = Vec::new();
    for r in valid {
        match out.last_mut() {
            Some(last) if r.start <= last.end + 1 => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

/// The wanted spans a section did NOT deliver in full; spine first, then importance, then source order.
pub fn elided_wanted_spans(wanted: &[WantedSpan], delivered: &[LineRange]) -> Vec<WantedSpan> {
    let merged = merge_ranges(delivered);
    let mut out: Vec<WantedSpan> = wanted
        .iter()
        .filter(|w| w.start > 0 && w.end >= w.start && !merged.iter().any(|r| r.start <= w.start && w.end <= r.end))
        .cloned()
        .collect();
    out.sort_by(|a, b| {
        js_cmp(first_nonzero(&[
            (b.spine as i32 - a.spine as i32) as f64,
            b.importance - a.importance,
            (a.start - b.start) as f64,
        ]))
    });
    out
}

/// A rendered file whose section is missing some of what it set out to deliver.
#[derive(Debug, Clone)]
pub struct PartialFile {
    pub file_path: String,
    pub elided: Vec<WantedSpan>,
}

const TRIMMED_FILES_NAMED: usize = 3;
const TRIMMED_SYMBOLS_NAMED: usize = 4;
const TRIMMED_NAME_SKIP_KINDS: [&str; 9] =
    ["file", "module", "namespace", "class", "struct", "union", "interface", "protocol", "trait"];

/// `Owner.member` for a method with a qualified name, the bare name otherwise.
pub fn follow_up_name(name: &str, kind: &str, qualified_name: Option<&str>) -> String {
    let Some(qn) = qualified_name.filter(|q| kind == "method" && !q.is_empty()) else { return name.to_string() };
    let segs: Vec<&str> = qn.split("::").collect();
    let owner = if segs.len() >= 2 { segs[segs.len() - 2] } else { "" };
    if re!(r"^[A-Za-z_$][\w$]*$").is_match(owner) {
        format!("{owner}.{name}")
    } else {
        name.to_string()
    }
}

/// Test/spec/icon/i18n file detector.
pub fn is_low_value(p: &str) -> bool {
    let lp = p.to_lowercase();
    is_test_file(p)
        || re!(r"(?:^|\/)__test__\/").is_match(&lp)
        || re!(r"\bicons?\b").is_match(&lp)
        || re!(r"\bi18n\b").is_match(&lp)
}

/// The shortest trailing slice of each path that no other path ends with.
pub fn shortest_unique_suffixes(paths: &[String]) -> IndexMap<String, String> {
    let all: IndexSet<&str> = paths.iter().map(String::as_str).collect();
    let mut out = IndexMap::new();
    for &p in &all {
        let segs: Vec<&str> = p.split('/').collect();
        let mut n = 1;
        while n < segs.len() {
            let suffix = segs[segs.len() - n..].join("/");
            let tail = format!("/{suffix}");
            if !all.iter().any(|&o| o != p && (o == suffix || o.ends_with(&tail))) {
                break;
            }
            n += 1;
        }
        out.insert(p.to_string(), segs[segs.len() - n..].join("/"));
    }
    out
}

/// The large tiers' completeness note, candidates from most to least specific.
pub fn explore_completeness_notes(files_included: usize, trimmed: &[PartialFile], known_paths: &[String]) -> Vec<String> {
    let files = match files_included {
        0 => "these files".to_string(),
        1 => "1 file".to_string(),
        n => format!("{n} files"),
    };
    if trimmed.is_empty() {
        return vec![format!("> **Complete source for {files} is included above — do NOT re-read them.** If your question also needs files/symbols listed under \"Not shown above\" (or any area this call didn't cover), make ANOTHER codegraphV_explore targeting those names — it returns the same source with line numbers and is cheaper and more complete than reading.")];
    }
    let mut all_paths: Vec<String> = known_paths.to_vec();
    all_paths.extend(trimmed.iter().map(|t| t.file_path.clone()));
    let label = shortest_unique_suffixes(&all_paths);
    let shown_files: Vec<String> = trimmed
        .iter()
        .take(TRIMMED_FILES_NAMED)
        .map(|t| format!("`{}`", label.get(&t.file_path).map(String::as_str).unwrap_or("undefined")))
        .collect();
    let more_files = trimmed.len() - shown_files.len();
    let mut trimmed_list = shown_files.join(", ");
    if more_files > 0 {
        trimmed_list.push_str(&format!(" +{more_files} more"));
    }
    let mut names: Vec<String> = Vec::new();
    for t in trimmed {
        for e in &t.elided {
            if names.len() >= TRIMMED_SYMBOLS_NAMED {
                break;
            }
            if !(e.spine || e.importance >= 9.0) || TRIMMED_NAME_SKIP_KINDS.contains(&e.kind.as_str()) {
                continue;
            }
            let name = follow_up_name(&e.name, &e.kind, e.qualified_name.as_deref());
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    let head = format!("> **Verbatim source for {files} is included above — treat it as already Read.**");
    let what = "gap markers and file headers name what was elided";
    let tail = "For those, or anything under \"Not shown above\", make ANOTHER codegraphV_explore with those exact names instead of reading the files — it returns their source with line numbers.";
    let with_files = format!("{head} Trimmed for size: {trimmed_list}; {what}");
    let mut candidates = Vec::new();
    if !names.is_empty() {
        let listed = names.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", ");
        candidates.push(format!("{with_files} (e.g. {listed}). {tail}"));
    }
    candidates.push(format!("{with_files}. {tail}"));
    candidates.push(format!("{head} Some sections were trimmed for size; {what}. {tail}"));
    candidates
}

/// Chars a block of lines costs once joined into the response.
pub fn room_for_lines(block: &[String]) -> f64 {
    block.iter().map(|s| utf16_len(s) as f64 + 1.0).sum()
}

/// The fitted epilogue: the note (a candidate block, or none), the pointer block, the room left.
#[derive(Debug, Clone, PartialEq)]
pub struct FittedEpilogue {
    pub note: Vec<String>,
    pub pointers: Vec<String>,
    pub room: f64,
}

/// Fit the completeness note and the pointer list into the room left (CG-26).
pub fn fit_explore_epilogue(
    room: f64,
    note_candidates: &[Vec<String>],
    note_yields: bool,
    pointer_entries: &[String],
    pointer_omitted: usize,
) -> FittedEpilogue {
    let fit_pointers = |space: f64| -> (Vec<String>, usize) {
        if pointer_entries.is_empty() {
            return (Vec::new(), 0);
        }
        let head = vec![POINTER_HEADER.to_string(), String::new()];
        let mut left = space - room_for_lines(&head);
        if left < 0.0 {
            return (Vec::new(), 0);
        }
        let mut taken = 0usize;
        for entry in pointer_entries {
            let dropped = pointer_entries.len() as i64 - taken as i64 - 1 + pointer_omitted as i64;
            let tail = if dropped > 0 { room_for_lines(&[format!("- ... and {dropped} more files")]) } else { 0.0 };
            let cost = utf16_len(entry) as f64 + 1.0;
            if cost + tail > left {
                break;
            }
            left -= cost;
            taken += 1;
        }
        if taken == 0 {
            return (Vec::new(), 0);
        }
        let mut block = head;
        block.extend(pointer_entries[..taken].iter().cloned());
        let dropped = pointer_entries.len() - taken + pointer_omitted;
        if dropped > 0 {
            block.push(format!("- ... and {dropped} more files"));
        }
        (block, taken)
    };
    let pointer_need = if pointer_entries.is_empty() {
        0.0
    } else {
        room_for_lines(&[
            POINTER_HEADER.to_string(),
            String::new(),
            pointer_entries[0].clone(),
            format!("- ... and {} more files", pointer_entries.len() - 1 + pointer_omitted),
        ])
    };
    let pointer_min = if note_yields && pointer_need <= room { pointer_need } else { 0.0 };
    let entries_beside = |b: &[String]| fit_pointers(room - room_for_lines(b)).1;
    let entries_floor = match note_candidates.last() {
        Some(least) if note_yields => entries_beside(least),
        _ => 0,
    };
    let note: Vec<String> = note_candidates
        .iter()
        .find(|b| room_for_lines(b) + pointer_min <= room && (!note_yields || entries_beside(b) >= entries_floor))
        .cloned()
        .unwrap_or_default();
    let mut room = room - room_for_lines(&note);
    let pointers = fit_pointers(room).0;
    room -= room_for_lines(&pointers);
    FittedEpilogue { note, pointers, room }
}
