use codegraph_mcp::render::explore;
use serde_json::json;
use std::collections::HashMap;

fn explore_text(root: &std::path::Path, query: &str) -> String {
    let mut cg = codegraph_core::CodeGraph::init(root.to_str().unwrap()).unwrap();
    cg.index_all(&Default::default(), None, None).unwrap();
    let result = explore(&cg, &json!({ "query": query }), &mut HashMap::new()).unwrap();
    cg.close();
    result.content[0]["text"].as_str().unwrap().to_string()
}

/// A named definition is shown even when a long body the graph relates to it (a caller) comes
/// first in its file: that body used to spend the file's budget in line order.
#[test]
fn named_definitions_come_before_unrelated_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let filler: String = (0..400).map(|i| format!("    let v{i} = {i};\n")).collect();
    let source = format!(
        "pub fn unrelated_long() -> u32 {{\n{filler}    helper_alpha()\n}}\n\npub fn helper_alpha() -> u32 {{\n    1\n}}\n\npub fn helper_beta() -> u32 {{\n    helper_alpha() + 1\n}}\n"
    );
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), source).unwrap();

    let text = explore_text(dir.path(), "helper_alpha helper_beta");
    assert!(text.contains("pub fn helper_alpha() -> u32 {"), "{text}");
    assert!(text.contains("pub fn helper_beta() -> u32 {"), "{text}");
    assert!(
        !text.contains("let v200 = 200;"),
        "unrelated body printed:\n{text}"
    );
}

#[test]
fn explicit_file_keeps_its_definition_ahead_of_other_overloads() {
    let dir = tempfile::tempdir().unwrap();
    for file in ["a.rs", "b.rs", "c.rs", "d.rs"] {
        std::fs::write(dir.path().join(file), "pub fn shared_name() -> u32 { 1 }\n").unwrap();
    }
    std::fs::create_dir_all(dir.path().join("deep/nested")).unwrap();
    std::fs::write(
        dir.path().join("deep/nested/target.rs"),
        "pub fn shared_name() -> u32 {\n    987654321\n}\n",
    )
    .unwrap();
    let text = explore_text(dir.path(), "shared_name deep/nested/target.rs");
    let target = text.find("**deep/nested/target.rs**").unwrap();
    assert!(text.contains("987654321"), "{text}");
    for other in ["a.rs", "b.rs", "c.rs", "d.rs"] {
        if let Some(position) = text.find(&format!("**{other}**")) {
            assert!(target < position, "explicit path lost priority: {text}");
        }
    }
}

#[test]
fn exact_source_ranges_keep_source_without_repeating_relationship_metadata() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("target.ts"), "export function target() { return 42; }\n").unwrap();
    std::fs::write(dir.path().join("caller.ts"), "import { target } from './target';\nexport function caller() { return target(); }\n").unwrap();
    let cg = codegraph_core::CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let mut shown = HashMap::new();
    let whole = explore(&cg, &json!({"query":"target.ts"}), &mut shown).unwrap().message();
    assert!(whole.contains("Used by") && whole.contains("caller.ts"), "{whole}");
    let range = explore(&cg, &json!({"query":"target.ts:1-1"}), &mut shown).unwrap().message();
    assert!(range.contains("     1\texport function target() { return 42; }"), "{range}");
    assert!(!range.contains("Used by"), "{range}");
    assert!(range.len() < whole.len(), "{range}");
    cg.close();
}

#[test]
fn unresolved_file_paths_are_reported_before_other_matching_source() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("actual.rs"), "pub fn target() -> u32 { 42 }\n").unwrap();
    let cg = codegraph_core::CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    for query in ["absent/deep/file.rs target", "absent/deep/file.rs"] {
        let result = explore(&cg, &json!({"query":query}), &mut HashMap::new()).unwrap();
        assert!(!result.is_error);
        let text = result.message();
        assert!(text.contains("No indexed file matches requested path(s): `absent/deep/file.rs`"), "{text}");
        if query.ends_with(" target") {
            assert!(text.contains("pub fn target() -> u32 { 42 }"), "{text}");
        }
    }
    cg.close();
}

#[test]
fn continued_source_is_verbatim_and_edits_replace_old_dedup_state() {
    let dir = tempfile::tempdir().unwrap();
    let source = format!(
        "pub fn long_body() {{\n{}}}\n",
        (0..250)
            .map(|i| format!("    let value_{i} = \"你好_{i}\";\r\n"))
            .collect::<String>()
    );
    std::fs::write(dir.path().join("main.rs"), &source).unwrap();
    let mut cg =
        codegraph_core::CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let mut shown = HashMap::new();
    let mut emitted = std::collections::BTreeMap::new();
    let mut done = false;
    for _ in 0..12 {
        let text = explore(&cg, &json!({"query":"long_body"}), &mut shown)
            .unwrap()
            .message();
        assert!(
            text.chars().count() <= codegraph_mcp::render::explore_budget(1).chars,
            "{text}"
        );
        for line in text.lines() {
            if let Some((number, value)) = line.split_once('\t') {
                if let Ok(number) = number.trim().parse::<usize>() {
                    assert!(
                        emitted.insert(number, value.to_string()).is_none(),
                        "duplicate line {number}"
                    );
                }
            }
        }
        if !text.contains("More source in") {
            done = true;
            break;
        }
        assert!(
            text.contains("main.rs:"),
            "missing actionable continuation: {text}"
        );
    }
    assert!(done, "continuation did not finish");
    for (index, line) in source.lines().enumerate() {
        assert_eq!(emitted.get(&(index + 1)).map(String::as_str), Some(line));
    }
    let complete = explore(&cg, &json!({"query":"long_body"}), &mut shown)
        .unwrap()
        .message();
    assert!(complete.contains("already shown"), "{complete}");
    let updated = source.replace("你好_0", "updated_0");
    std::fs::write(dir.path().join("main.rs"), updated).unwrap();
    cg.sync(&Default::default(), None).unwrap();
    let after_edit = explore(&cg, &json!({"query":"long_body"}), &mut shown)
        .unwrap()
        .message();
    assert!(after_edit.contains("updated_0"), "{after_edit}");
    assert_eq!(shown.len(), 1, "old content hashes retained after edit");
    cg.close();
}

#[test]
fn huge_signatures_do_not_spend_the_entire_response_budget() {
    let dir = tempfile::tempdir().unwrap();
    let args = (0..700)
        .map(|i| format!("argument_{i}: u32"))
        .collect::<Vec<_>>()
        .join(", ");
    let source = format!("pub fn huge_signature({args}) {{\n    let body_marker = 42;\n}}\npub fn small_definition() {{}}\n");
    std::fs::write(dir.path().join("main.rs"), source).unwrap();
    let text = explore_text(dir.path(), "huge_signature small_definition");
    assert!(
        text.chars().count() <= codegraph_mcp::render::explore_budget(1).chars,
        "{} characters",
        text.chars().count()
    );
    assert!(
        text.contains("pub fn small_definition() {}"),
        "later definition starved: {text}"
    );
    assert!(
        text.contains("let body_marker = 42;"),
        "later source starved: {text}"
    );
    assert!(
        text.contains("source line(s) exceed"),
        "missing oversized-source notice: {text}"
    );
    assert!(
        !text.contains("repeat the same query"),
        "unproductive continuation: {text}"
    );
}

#[test]
fn all_project_sizes_fit_the_advertised_inline_ceiling() {
    for files in [
        0, 149, 150, 499, 500, 4999, 5000, 14999, 15000, 25000, 100000,
    ] {
        assert!(codegraph_mcp::render::explore_budget(files).chars <= 24000);
    }
}

#[test]
fn many_requested_files_keep_source_fences_and_footer_within_budget() {
    let dir = tempfile::tempdir().unwrap();
    let mut paths = Vec::new();
    for i in 0..20 {
        let path = format!("module_{i}.rs");
        let body = (0..200)
            .map(|j| format!("    let item_{j} = {j};\n"))
            .collect::<String>();
        std::fs::write(
            dir.path().join(&path),
            format!("pub fn entry_{i}() {{\n{body}}}\n"),
        )
        .unwrap();
        paths.push(path);
    }
    let cg = codegraph_core::CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let text = explore(
        &cg,
        &json!({"query":paths.join(" "), "maxFiles":20}),
        &mut HashMap::new(),
    )
    .unwrap()
    .message();
    assert!(
        text.chars().count() <= codegraph_mcp::render::explore_budget(20).chars,
        "{} characters: {text}",
        text.chars().count()
    );
    assert_eq!(
        text.matches("```").count() % 2,
        0,
        "source fence cut: {text}"
    );
    assert!(
        text.contains("Additional relevant files"),
        "omitted files not reported: {text}"
    );
    assert!(text.contains("Treat returned source as already Read"));
    cg.close();
}

#[test]
fn continuation_query_also_works_without_session_state() {
    let dir = tempfile::tempdir().unwrap();
    let body = (0..180)
        .map(|i| format!("    let item_{i} = {i};\n"))
        .collect::<String>();
    std::fs::write(
        dir.path().join("main.rs"),
        format!("pub fn large_body() {{\n{body}}}\n"),
    )
    .unwrap();
    let cg = codegraph_core::CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let first = explore(&cg, &json!({"query":"large_body"}), &mut HashMap::new())
        .unwrap()
        .message();
    let continuation = first
        .split("continue with codegraphV_explore(query: \"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("actionable continuation query");
    let next = explore(&cg, &json!({"query":continuation}), &mut HashMap::new())
        .unwrap()
        .message();
    assert!(
        next.contains("let item_179 = 179;"),
        "continuation did not advance: {next}"
    );
    cg.close();
}

#[test]
fn explicit_line_ranges_are_exact_even_after_the_source_was_shown() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.rs"),
        "pub fn example() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n}\n",
    )
    .unwrap();
    let cg = codegraph_core::CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let mut shown = HashMap::new();
    explore(&cg, &json!({"query":"example"}), &mut shown).unwrap();
    let text = explore(&cg, &json!({"query":"main.rs:3-3"}), &mut shown)
        .unwrap()
        .message();
    assert!(
        text.contains("let b = 2;"),
        "explicit source suppressed: {text}"
    );
    assert!(
        !text.contains("let a = 1;") && !text.contains("let c = 3;"),
        "range expanded: {text}"
    );
    cg.close();
}

#[test]
fn continuation_advances_when_context_lines_would_consume_the_page() {
    let dir = tempfile::tempdir().unwrap();
    let body = (0..12)
        .map(|i| format!("    let value_{i} = \"{}\";\n", "x".repeat(1400)))
        .collect::<String>();
    std::fs::write(
        dir.path().join("main.rs"),
        format!("pub fn long_lines() {{\n{body}}}\n"),
    )
    .unwrap();
    let cg = codegraph_core::CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let mut query = "long_lines".to_string();
    let mut queries = std::collections::HashSet::new();
    let mut source = String::new();
    for _ in 0..20 {
        assert!(
            queries.insert(query.clone()),
            "continuation repeated {query}"
        );
        let text = explore(&cg, &json!({"query":query}), &mut HashMap::new())
            .unwrap()
            .message();
        if query.contains(':') {
            assert!(
                !text.contains("repeat the same query"),
                "explicit range cannot advance by repetition: {text}"
            );
        }
        source.push_str(&text);
        let Some(next) = text
            .split("continue with codegraphV_explore(query: \"")
            .nth(1)
            .and_then(|s| s.split('"').next())
        else {
            break;
        };
        query = next.to_string();
    }
    for i in 0..12 {
        assert!(
            source.contains(&format!("let value_{i} =")),
            "source line {i} never reached"
        );
    }
    cg.close();
}

#[test]
fn explicit_file_also_scopes_ambiguous_flow_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    for file in ["a.ts", "chosen.ts"] {
        std::fs::write(dir.path().join(file),
            "export class Service {\n start() { return this.finish(); }\n finish() { return 1; }\n}\n").unwrap();
    }
    let text = explore_text(dir.path(), "Service.start Service.finish chosen.ts");
    let flow = text
        .split("**Flow among named symbols**")
        .nth(1)
        .and_then(|s| s.split("\n**").next())
        .expect("flow should be found");
    assert!(
        flow.contains("chosen.ts:"),
        "requested file ignored in flow: {text}"
    );
    assert!(
        !flow.contains("a.ts:"),
        "flow uses an unrelated overload: {text}"
    );
}

#[test]
fn scoped_flow_still_reaches_helpers_in_other_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("chosen.ts"),
        "import { finish } from './helper';\nexport function start() { return finish(); }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("helper.ts"),
        "export function finish() { return 42; }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("a.ts"),
        "export function start() { return 0; }\n",
    )
    .unwrap();
    let cg = codegraph_core::CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    // Some extractors name the import after the imported symbol. Such a record
    // is not a local implementation and must not shadow the helper's definition.
    cg.store()
        .insert_nodes(&[codegraph_types::Node::new(
            "import-finish",
            codegraph_types::NodeKind::Import,
            "finish",
            "finish",
            "chosen.ts",
            codegraph_types::Language::TypeScript,
            1,
            1,
            0,
            0,
            1,
        )])
        .unwrap();
    let text = explore(
        &cg,
        &json!({"query":"start finish chosen.ts"}),
        &mut HashMap::new(),
    )
    .unwrap()
    .message();
    let flow = text
        .split("**Flow among named symbols**")
        .nth(1)
        .and_then(|s| s.split("\n**").next())
        .expect("cross-file flow should be found");
    assert!(
        flow.contains("chosen.ts:") && flow.contains("helper.ts:"),
        "{text}"
    );
    cg.close();
}

#[test]
fn oversized_unicode_line_can_be_reconstructed_from_fresh_fragment_calls() {
    let dir = tempfile::tempdir().unwrap();
    let original = format!("// {}  ", "你😀\\\"```~~~".repeat(1600));
    std::fs::write(
        dir.path().join("main source.rs"),
        format!("{original}\r\npub fn after() {{}}\n"),
    )
    .unwrap();
    let mut cg =
        codegraph_core::CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let mut args = json!({"query":"main source.rs:1-1", "column":1});
    let mut reconstructed = String::new();
    let mut last_args = args.clone();
    let mut done = false;
    for _ in 0..30 {
        last_args = args.clone();
        let text = explore(&cg, &args, &mut HashMap::new()).unwrap().message();
        assert!(text.chars().count() <= codegraph_mcp::render::explore_budget(1).chars);
        let after_header = text
            .split_once("\n\nVerbatim source fragment;")
            .expect("fragment header")
            .1;
        let block = after_header.split_once("\n\n").unwrap().1;
        let (fence_language, rest) = block.split_once('\n').unwrap();
        let fence = fence_language.trim_end_matches("rust");
        let fragment = rest.split_once(&format!("\n{fence}\n")).unwrap().0;
        reconstructed.push_str(fragment);
        if let Some(next) = text
            .split("Next fragment: ")
            .nth(1)
            .and_then(|s| s.lines().next())
        {
            let next: serde_json::Value = serde_json::from_str(next).unwrap();
            assert!(next["column"].as_u64().unwrap() > args["column"].as_u64().unwrap());
            args = next;
        } else {
            assert!(text.contains("End of source line."));
            done = true;
            break;
        }
    }
    assert!(done, "fragment paging did not finish");
    assert_eq!(reconstructed, original);
    std::fs::write(
        dir.path().join("main source.rs"),
        "// changed\npub fn after() {}\n",
    )
    .unwrap();
    cg.sync(&Default::default(), None).unwrap();
    let stale = explore(&cg, &last_args, &mut HashMap::new())
        .unwrap()
        .message();
    assert!(
        stale.contains("Source changed"),
        "mixed source versions: {stale}"
    );
    cg.close();
}

#[test]
fn source_fragments_reject_invalid_ranges_and_protect_config_values() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "pub fn example() {}\n").unwrap();
    std::fs::write(
        dir.path().join("config.yaml"),
        "password: TOP_SECRET_VALUE\n",
    )
    .unwrap();
    let cg = codegraph_core::CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    for args in [
        json!({"query":"main.rs:1-2", "column":1}),
        json!({"query":"main.rs:1-1", "column":0}),
        json!({"query":"main.rs:1-1", "column":1.5}),
        json!({"query":"main.rs:1-1", "column":"1"}),
        json!({"query":"main.rs:1-1", "column":1000}),
        json!({"query":"main.rs:1-1", "column":1, "sourceHash":42}),
        json!({"query":"../outside.rs:1-1", "column":1}),
    ] {
        let result = explore(&cg, &args, &mut HashMap::new()).unwrap();
        assert!(!result.is_error);
        assert!(
            !result.message().contains("Verbatim source fragment"),
            "{args}: {}",
            result.message()
        );
    }
    let config = explore(
        &cg,
        &json!({"query":"config.yaml:1-1", "column":1}),
        &mut HashMap::new(),
    )
    .unwrap()
    .message();
    assert!(!config.contains("TOP_SECRET_VALUE"));
    assert!(config.contains("withheld"), "{config}");
    cg.close();
}
