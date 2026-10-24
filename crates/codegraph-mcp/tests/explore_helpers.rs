//! Replays the TypeScript oracle's vectors for the pure explore helpers
//! (`scripts/rust-golden/explore-helpers.mjs` writes `fixtures/explore_helpers.jsonl`).
use codegraph_mcp::explore::helpers::*;
use codegraph_types::Node;
use serde_json::{json, Value};

fn num(v: &Value) -> f64 {
    match v {
        Value::String(s) if s == "Infinity" => f64::INFINITY,
        _ => v.as_f64().expect("number"),
    }
}

fn int(v: &Value) -> i64 {
    num(v) as i64
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect()
}

fn nodes(v: &Value) -> Vec<Node> {
    serde_json::from_value(v.clone()).expect("nodes")
}

fn range(v: &Value) -> LineRange {
    LineRange { start: int(&v["start"]), end: int(&v["end"]) }
}

fn ranges(v: &Value) -> Vec<LineRange> {
    v.as_array().unwrap().iter().map(range).collect()
}

fn elided(v: &Value) -> Vec<ElidedSymbolRef> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|e| ElidedSymbolRef {
            name: e["name"].as_str().unwrap().into(),
            kind: e["kind"].as_str().unwrap().into(),
            start_line: int(&e["startLine"]),
        })
        .collect()
}

fn elided_json(refs: &[ElidedSymbolRef]) -> Value {
    refs.iter().map(|e| json!({"name": e.name, "kind": e.kind, "startLine": e.start_line})).collect()
}

fn wanted(v: &Value) -> Vec<WantedSpan> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|w| WantedSpan {
            name: w["name"].as_str().unwrap().into(),
            kind: w["kind"].as_str().unwrap().into(),
            start: int(&w["start"]),
            end: int(&w["end"]),
            importance: num(&w["importance"]),
            spine: w["spine"].as_bool().unwrap(),
            qualified_name: w.get("qualifiedName").and_then(Value::as_str).map(String::from),
        })
        .collect()
}

fn wanted_json(spans: &[WantedSpan]) -> Value {
    spans
        .iter()
        .map(|w| {
            let mut o = json!({"name": w.name, "kind": w.kind, "start": w.start, "end": w.end,
                "importance": w.importance, "spine": w.spine});
            if let Some(q) = &w.qualified_name {
                o["qualifiedName"] = json!(q);
            }
            o
        })
        .collect()
}

fn budget_from(v: &Value) -> ExploreOutputBudget {
    ExploreOutputBudget {
        max_output_chars: num(&v["maxOutputChars"]),
        default_max_files: num(&v["defaultMaxFiles"]),
        max_chars_per_file: num(&v["maxCharsPerFile"]),
        gap_threshold: num(&v["gapThreshold"]),
        max_symbols_in_file_header: int(&v["maxSymbolsInFileHeader"]) as usize,
        max_edges_per_relationship_kind: int(&v["maxEdgesPerRelationshipKind"]) as usize,
        include_relationships: v["includeRelationships"].as_bool().unwrap(),
        include_additional_files: v["includeAdditionalFiles"].as_bool().unwrap(),
        include_completeness_signal: v["includeCompletenessSignal"].as_bool().unwrap(),
        include_budget_note: v["includeBudgetNote"].as_bool().unwrap(),
    }
}

fn budget_json(b: &ExploreOutputBudget) -> Value {
    json!({
        "maxOutputChars": b.max_output_chars, "defaultMaxFiles": b.default_max_files,
        "maxCharsPerFile": b.max_chars_per_file, "gapThreshold": b.gap_threshold,
        "maxSymbolsInFileHeader": b.max_symbols_in_file_header,
        "maxEdgesPerRelationshipKind": b.max_edges_per_relationship_kind,
        "includeRelationships": b.include_relationships, "includeAdditionalFiles": b.include_additional_files,
        "includeCompletenessSignal": b.include_completeness_signal, "includeBudgetNote": b.include_budget_note,
    })
}

fn run(name: &str, a: &[Value]) -> Value {
    match name {
        "normalizeQuerySpelling" => json!(normalize_query_spelling(a[0].as_str().unwrap())),
        "getExploreOutputBudget" => budget_json(&get_explore_output_budget(num(&a[0]))),
        "allocateExploreBudget" => {
            let cands: Vec<AllocationCandidate> = a[0]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| AllocationCandidate {
                    path: c["path"].as_str().unwrap().into(),
                    score: num(&c["score"]),
                    worth: num(&c["worth"]),
                    spine: c["spine"].as_bool().unwrap(),
                    pinned: c.get("pinned").and_then(Value::as_bool).unwrap_or(false),
                    named: c.get("named").and_then(Value::as_bool).unwrap_or(false),
                })
                .collect();
            let out = allocate_explore_budget(&cands, &budget_from(&a[1]), int(&a[2]) as usize);
            json!({
                "allowances": out.allowances.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(),
                "cliffed": out.cliffed, "cliffAt": out.cliff_at, "pool": out.pool,
            })
        }
        "symbolsBetweenRanges" => elided_json(&symbols_between_ranges(&nodes(&a[0]), int(&a[1]), int(&a[2]))),
        "symbolsNotInRanges" => elided_json(&symbols_not_in_ranges(&elided(&a[0]), &ranges(&a[1]))),
        "formatGapMarker" => json!(format_gap_marker(a[0].as_str().unwrap(), &elided(&a[1]))),
        "elidedBodyNote" => json!(elided_body_note(a[0].as_str().unwrap(), a[1].as_str().unwrap(), int(&a[2]), int(&a[3]))),
        "joinPartsWithNamedGaps" => {
            let parts: Vec<RenderedPart> = a[1]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| RenderedPart { range: range(&p["range"]), text: p["text"].as_str().unwrap().into() })
                .collect();
            let spare = a.get(3).map(num).unwrap_or(f64::INFINITY);
            json!(join_parts_with_named_gaps(a[0].as_str().unwrap(), &parts, &nodes(&a[2]), spare))
        }
        "biasHeaderSymbols" => {
            let (shown, omitted) = bias_header_symbols(&strs(&a[0]), &elided(&a[1]), int(&a[2]) as usize);
            json!({"shown": shown, "omitted": omitted})
        }
        "elidedWantedSpans" => wanted_json(&elided_wanted_spans(&wanted(&a[0]), &ranges(&a[1]))),
        "shortestUniqueSuffixes" => {
            shortest_unique_suffixes(&strs(&a[0])).into_iter().map(|(k, v)| json!([k, v])).collect()
        }
        "exploreCompletenessNotes" => {
            let trimmed: Vec<PartialFile> = a[1]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| PartialFile { file_path: t["filePath"].as_str().unwrap().into(), elided: wanted(&t["elided"]) })
                .collect();
            json!(explore_completeness_notes(int(&a[0]) as usize, &trimmed, &strs(&a[2])))
        }
        "fitExploreEpilogue" => {
            let o = &a[0];
            let cands: Vec<Vec<String>> = o["noteCandidates"].as_array().unwrap().iter().map(strs).collect();
            let out = fit_explore_epilogue(
                num(&o["room"]),
                &cands,
                o["noteYields"].as_bool().unwrap(),
                &strs(&o["pointerEntries"]),
                int(&o["pointerOmitted"]) as usize,
            );
            json!({"note": out.note, "pointers": out.pointers, "room": out.room})
        }
        other => panic!("no Rust binding for {other}"),
    }
}

/// JSON equality with numbers compared by value (`3800` == `3800.0`).
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        _ => a == b,
    }
}

#[test]
fn explore_helpers_match_typescript() {
    let text = include_str!("fixtures/explore_helpers.jsonl");
    let mut failures = Vec::new();
    let mut total = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        total += 1;
        let v: Value = serde_json::from_str(line).unwrap();
        let name = v["fn"].as_str().unwrap();
        let actual = run(name, v["args"].as_array().unwrap());
        if !same(&actual, &v["expected"]) {
            failures.push(format!("{name}\n  args:     {}\n  expected: {}\n  actual:   {}", v["args"], v["expected"], actual));
        }
    }
    assert!(failures.is_empty(), "{}/{total} vectors differ; first:\n{}", failures.len(), failures[..failures.len().min(4)].join("\n"));
}
