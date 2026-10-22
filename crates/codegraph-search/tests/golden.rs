//! Differential tests: the Rust helpers against outputs recorded from the
//! TypeScript implementation (`scripts/rust-golden/search.mjs`) over a corpus of
//! this repository's own paths and identifiers.

use codegraph_search::*;
use codegraph_types::NodeKind;
use serde_json::{json, Value};

fn golden() -> Value {
    serde_json::from_str(include_str!("golden/search.json")).expect("golden file parses")
}

fn s(v: &Value) -> &str {
    v.as_str().expect("string")
}

/// Run `check` over every row of one golden table and report all mismatches at
/// once, so a single run shows the whole shape of a divergence.
fn each(table: &str, mut check: impl FnMut(&[Value]) -> Option<String>) {
    let g = golden();
    let rows = g[table].as_array().unwrap_or_else(|| panic!("no table {table}"));
    let mut failures = Vec::new();
    for row in rows {
        if let Some(msg) = check(row.as_array().expect("row")) {
            failures.push(msg);
        }
    }
    assert!(
        failures.is_empty(),
        "{table}: {} of {} rows differ from the TypeScript oracle; first few:\n{}",
        failures.len(),
        rows.len(),
        failures.iter().take(8).cloned().collect::<Vec<_>>().join("\n")
    );
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().unwrap().iter().map(|x| s(x).to_string()).collect()
}

fn cmp<T: PartialEq + std::fmt::Debug>(input: &str, got: T, want: T) -> Option<String> {
    (got != want).then(|| format!("  input {input:?}\n    rust: {got:?}\n    ts:   {want:?}"))
}

#[test]
fn identifier_segments() {
    each("segments", |r| cmp(s(&r[0]), split_identifier_segments(s(&r[0])), strings(&r[1])));
}

#[test]
fn search_terms() {
    each("searchTerms", |r| {
        cmp(s(&r[0]), extract_search_terms(s(&r[0]), true), strings(&r[1]))
            .or_else(|| cmp(s(&r[0]), extract_search_terms(s(&r[0]), false), strings(&r[2])))
    });
}

#[test]
fn stem_variants() {
    each("stems", |r| cmp(s(&r[0]), get_stem_variants(s(&r[0])), strings(&r[1])));
}

#[test]
fn distinctive_identifiers() {
    each("distinctive", |r| cmp(s(&r[0]), is_distinctive_identifier(s(&r[0])), r[1].as_bool().unwrap()));
}

#[test]
fn segment_lookup() {
    each("lookup", |r| cmp(s(&r[0]), segment_lookup_variants(s(&r[0])), strings(&r[1])));
}

#[test]
fn prose_candidates_and_segment_search_words() {
    each("prose", |r| {
        cmp(s(&r[0]), extract_prose_candidates(s(&r[0])), strings(&r[1]))
            .or_else(|| cmp(s(&r[0]), extract_segment_search_words(s(&r[0])), strings(&r[2])))
    });
}

#[test]
fn query_parsing() {
    each("parse", |r| {
        let p = parse_query(s(&r[0]));
        let got = json!({
            "text": p.text,
            "kinds": p.kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
            "languages": p.languages.iter().map(|l| l.as_str()).collect::<Vec<_>>(),
            "pathFilters": p.path_filters,
            "nameFilters": p.name_filters,
        });
        cmp(s(&r[0]), got, r[1].clone())
    });
}

#[test]
fn name_match_bonus_scores() {
    each("nameBonus", |r| {
        let got = name_match_bonus(s(&r[0]), s(&r[1]));
        cmp(&format!("{} | {}", s(&r[0]), s(&r[1])), got as i64, r[2].as_i64().unwrap())
    });
}

#[test]
fn path_relevance_scores() {
    each("pathScore", |r| {
        let got = score_path_relevance(s(&r[0]), s(&r[1]), None, false);
        cmp(&format!("{} | {}", s(&r[0]), s(&r[1])), got as i64, r[2].as_i64().unwrap())
    });
}

#[test]
fn path_classification() {
    each("pathKinds", |r| {
        let p = s(&r[0]);
        let got = (is_test_file(p), is_test_path(p), is_generated_file(p));
        let want = (r[1].as_bool().unwrap(), r[2].as_bool().unwrap(), r[3].as_bool().unwrap());
        cmp(p, got, want)
    });
}

#[test]
fn edit_distance() {
    each("editDistance", |r| {
        cmp(
            &format!("{} ~ {}", s(&r[0]), s(&r[1])),
            bounded_edit_distance(s(&r[0]), s(&r[1]), 2) as i64,
            r[2].as_i64().unwrap(),
        )
    });
}

#[test]
fn kind_bonuses() {
    each("kindBonus", |r| {
        let kind = NodeKind::parse(s(&r[0])).unwrap();
        cmp(s(&r[0]), kind_bonus(kind) as i64, r[1].as_i64().unwrap())
    });
}
