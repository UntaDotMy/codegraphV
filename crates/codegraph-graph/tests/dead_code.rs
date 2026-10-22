//! Port of the derivation-level assertions in `__tests__/dead-code.test.ts`
//! (CG-59). The HTTP endpoint and the viewer are not ported yet.
use codegraph_db::Store;
use codegraph_graph::dead_code::{
    build_dead_code_report, is_header_file, is_implicit_entry_name, is_test_scope, is_vendored_path,
    mention_count, DeadCodeQuery, SourceReader,
};
use codegraph_types::{Edge, EdgeKind, Language, Node, NodeKind};

fn node(id: &str, kind: NodeKind, name: &str, file: &str, start: u32, end: u32) -> Node {
    Node::new(
        id,
        kind,
        name,
        format!("{file}::{name}"),
        file,
        Language::TypeScript,
        start,
        end,
        0,
        0,
        0,
    )
}

fn exported(mut n: Node) -> Node {
    n.is_exported = Some(true);
    n
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    root: String,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let files = [
        (
            "src/util.ts",
            "export function used(value: string): string { return value.trim(); }\nfunction neverCalledAnywhere(value: string): string { return value.toUpperCase(); }\nexport function publicEntryPoint(): string { return 'hello'; }\n",
        ),
        (
            "src/component.ts",
            "export function mount(): void {}\nfunction onSubmit() { return 1; }\nconst handler = onSubmit;\n",
        ),
        (
            "src/big.ts",
            "export function bigAnchor(): void {}\nfunction bigDead(): number {\n  return 1;\n}\n\nfunction tinyDead(): number { return 2; }\n",
        ),
        (
            "src/decorated.ts",
            "export class Service { onEvent(): void {} }\nclass Injectable {}\n",
        ),
        (
            "src/widget.test.ts",
            "export function testAnchor(): void {}\nfunction helperNothingCalls(): void {}\n",
        ),
        ("src/island.ts", "function strandedHelper(): void {}\n"),
        (
            "src/app.ts",
            "export function caller(): void { used('x'); mount(); bigAnchor(); testAnchor(); }\n",
        ),
    ];
    for (rel, body) in files {
        let full = root.join(rel);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, body).unwrap();
    }

    let store = Store::initialize(root.join("codegraph.db")).unwrap();
    let nodes = vec![
        node("n_used", NodeKind::Function, "used", "src/util.ts", 1, 1),
        node(
            "n_never",
            NodeKind::Function,
            "neverCalledAnywhere",
            "src/util.ts",
            2,
            2,
        ),
        exported(node(
            "n_public",
            NodeKind::Function,
            "publicEntryPoint",
            "src/util.ts",
            3,
            3,
        )),
        node("n_mount", NodeKind::Function, "mount", "src/component.ts", 1, 1),
        node("n_onsubmit", NodeKind::Function, "onSubmit", "src/component.ts", 2, 2),
        node("n_biganchor", NodeKind::Function, "bigAnchor", "src/big.ts", 1, 1),
        node("n_bigdead", NodeKind::Function, "bigDead", "src/big.ts", 2, 4),
        node("n_tinydead", NodeKind::Function, "tinyDead", "src/big.ts", 6, 6),
        node("n_service", NodeKind::Class, "Service", "src/decorated.ts", 1, 1),
        node("n_onevent", NodeKind::Method, "onEvent", "src/decorated.ts", 1, 1),
        node(
            "n_injectable",
            NodeKind::Class,
            "Injectable",
            "src/decorated.ts",
            2,
            2,
        ),
        node(
            "n_testanchor",
            NodeKind::Function,
            "testAnchor",
            "src/widget.test.ts",
            1,
            1,
        ),
        node(
            "n_helpernothing",
            NodeKind::Function,
            "helperNothingCalls",
            "src/widget.test.ts",
            2,
            2,
        ),
        node(
            "n_stranded",
            NodeKind::Function,
            "strandedHelper",
            "src/island.ts",
            1,
            1,
        ),
        node("n_caller", NodeKind::Function, "caller", "src/app.ts", 1, 1),
    ];
    store.insert_nodes(&nodes).unwrap();
    store
        .insert_edges(&[
            Edge::new("n_service", "n_onevent", EdgeKind::Contains),
            Edge::new("n_onevent", "n_injectable", EdgeKind::Decorates),
            Edge::new("n_caller", "n_used", EdgeKind::Calls),
            Edge::new("n_caller", "n_mount", EdgeKind::Calls),
            Edge::new("n_caller", "n_biganchor", EdgeKind::Calls),
            Edge::new("n_caller", "n_testanchor", EdgeKind::Calls),
            Edge::new("n_caller", "n_service", EdgeKind::References),
        ])
        .unwrap();
    Fixture {
        root: root.to_string_lossy().to_string(),
        _dir: dir,
        store,
    }
}

fn report(f: &Fixture, mutate: impl FnOnce(&mut DeadCodeQuery)) -> Vec<String> {
    report_full(f, mutate).entries.into_iter().map(|e| e.node.name).collect()
}

fn report_full(
    f: &Fixture,
    mutate: impl FnOnce(&mut DeadCodeQuery),
) -> codegraph_graph::dead_code::DeadCodeReport {
    let mut query = DeadCodeQuery {
        project_root: &f.root,
        ..Default::default()
    };
    mutate(&mut query);
    build_dead_code_report(&f.store, &query).unwrap()
}

#[test]
fn counts_whole_identifier_mentions_only() {
    assert_eq!(mention_count("const load = 1; loader(); reload();", "load", usize::MAX), 1);
    assert_eq!(mention_count("a.load(); load();", "load", usize::MAX), 2);
    assert_eq!(mention_count("nothing here", "load", usize::MAX), 0);
    assert_eq!(mention_count("x x x x x", "x", 2), 2);
}

#[test]
fn matches_vendored_directories_as_whole_segments() {
    assert!(is_vendored_path("vendor/lib/a.go"));
    assert!(is_vendored_path("a/node_modules/b/c.js"));
    assert!(!is_vendored_path("src/vendored-parser.ts"));
}

#[test]
fn recognises_headers_as_declaration_surfaces() {
    assert!(is_header_file("src/tree_sitter/parser.h"));
    assert!(is_header_file("types/global.d.ts"));
    assert!(!is_header_file("src/parser.c"));
}

#[test]
fn recognises_a_test_scope_inside_a_file() {
    assert!(is_test_scope("tests::row_sizes_match"));
    assert!(is_test_scope("Fixtures.Tests.Helper"));
    assert!(!is_test_scope("Latest.value"));
}

#[test]
fn recognises_names_the_language_calls_by_itself() {
    assert!(is_implicit_entry_name("constructor"));
    assert!(is_implicit_entry_name("__enter__"));
    assert!(is_implicit_entry_name("ToString"));
    assert!(!is_implicit_entry_name("mainHandler"));
}

#[test]
fn finds_the_symbol_nothing_references() {
    let f = fixture();
    assert!(report(&f, |_| {}).contains(&"neverCalledAnywhere".to_string()));
    f.store.close().unwrap();
}

#[test]
fn leaves_nothing_on_the_list_that_anything_reaches() {
    let f = fixture();
    let names = report(&f, |_| {});
    for name in ["used", "mount", "bigAnchor", "testAnchor"] {
        assert!(!names.contains(&name.to_string()), "{name}");
    }
    f.store.close().unwrap();
}

#[test]
fn excludes_a_symbol_only_its_own_file_mentions_and_counts_it() {
    let f = fixture();
    let full = report_full(&f, |_| {});
    assert!(!full.entries.iter().any(|e| e.node.name == "onSubmit"));
    assert!(full.excluded.mentioned > 0);
    assert!(full.corroborated);
    f.store.close().unwrap();
}

#[test]
fn makes_the_claim_when_corroboration_is_switched_off() {
    let f = fixture();
    let full = report_full(&f, |q| q.read_source = SourceReader::Off);
    assert!(!full.corroborated);
    assert_eq!(full.excluded.mentioned, 0);
    assert!(full.entries.iter().any(|e| e.node.name == "onSubmit"));
    f.store.close().unwrap();
}

#[test]
fn excludes_exported_symbols_by_default_and_includes_them_on_request() {
    let f = fixture();
    let strict = report_full(&f, |_| {});
    assert!(!strict.entries.iter().any(|e| e.node.name == "publicEntryPoint"));
    assert!(strict.excluded.exported > 0);
    assert!(!strict.include_exported);

    let wide = report_full(&f, |q| q.include_exported = true);
    assert!(wide.entries.iter().any(|e| e.node.name == "publicEntryPoint"));
    assert!(wide.include_exported);
    assert_eq!(wide.excluded.exported, 0);
    f.store.close().unwrap();
}

#[test]
fn excludes_test_files_by_default_and_includes_them_on_request() {
    let f = fixture();
    let strict = report_full(&f, |_| {});
    assert!(!strict.entries.iter().any(|e| e.node.name == "helperNothingCalls"));
    assert!(strict.excluded.tests > 0);
    let wide = report(&f, |q| q.include_tests = true);
    assert!(wide.contains(&"helperNothingCalls".to_string()));
    f.store.close().unwrap();
}

#[test]
fn says_nothing_about_a_file_nothing_in_the_index_reaches() {
    let f = fixture();
    let full = report_full(&f, |q| q.include_exported = true);
    assert!(!full.entries.iter().any(|e| e.node.name == "strandedHelper"));
    assert!(full.excluded.unreachable_file > 0);
    f.store.close().unwrap();
}

#[test]
fn excludes_a_decorated_member() {
    let f = fixture();
    let full = report_full(&f, |_| {});
    assert!(!full.entries.iter().any(|e| e.node.name == "onEvent"));
    assert!(full.excluded.decorated > 0);
    f.store.close().unwrap();
}

#[test]
fn ranks_by_size_and_reports_the_real_total_when_capped() {
    let f = fixture();
    let full = report_full(&f, |_| {});
    let sizes: Vec<usize> = full.entries.iter().map(|e| e.lines).collect();
    let mut sorted = sizes.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(sizes, sorted);
    assert_eq!(full.entries.first().map(|e| e.node.name.as_str()), Some("bigDead"));

    let capped = report_full(&f, |q| q.limit = 1);
    assert_eq!(capped.entries.len(), 1);
    assert_eq!(capped.total, full.total);
    assert_eq!(capped.entries[0].node.name, full.entries[0].node.name);
    f.store.close().unwrap();
}

#[test]
fn every_exclusion_count_is_a_number_of_candidates_and_they_add_up() {
    let f = fixture();
    let full = report_full(&f, |_| {});
    let excluded = full.excluded.tests
        + full.excluded.generated
        + full.excluded.exported
        + full.excluded.exports_unknown
        + full.excluded.declarations
        + full.excluded.decorated
        + full.excluded.overriding
        + full.excluded.hooks
        + full.excluded.implicit
        + full.excluded.vendored
        + full.excluded.test_scope
        + full.excluded.markup
        + full.excluded.unreachable_file
        + full.excluded.unresolved_name
        + full.excluded.ambiguous_name
        + full.excluded.mentioned
        + full.excluded.unreadable
        + full.excluded.nested;
    assert!(full.candidates > 0);
    assert!(excluded + full.entries.len() <= full.candidates);
    assert!(!full.bounded);
    f.store.close().unwrap();
}

#[test]
fn restricts_to_the_kinds_asked_for() {
    let f = fixture();
    let classes = report_full(&f, |q| q.kinds = Some(&[NodeKind::Class]));
    assert_eq!(classes.kinds, vec![NodeKind::Class]);
    assert!(classes.entries.iter().all(|e| e.node.kind == NodeKind::Class));

    // An empty request falls back to the default set, not an empty list.
    let fallback = report_full(&f, |q| q.kinds = Some(&[]));
    assert_eq!(fallback.kinds.len(), codegraph_graph::dead_code::dead_code_kinds().len());
    f.store.close().unwrap();
}
