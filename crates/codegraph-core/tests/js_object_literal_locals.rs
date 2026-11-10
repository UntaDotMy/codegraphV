//! A function inside an object literal's method is a local of that block, not a member
//! (upstream 9c5cf97e, #3161).
use codegraph_core::CodeGraph;
use codegraph_types::EdgeKind;
use std::fs;
use std::path::Path;

fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (relative, content) in files {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

fn index(files: &[(&str, &str)]) -> (tempfile::TempDir, CodeGraph) {
    let root = tempfile::tempdir().unwrap();
    write_files(root.path(), files);
    let cg = CodeGraph::init_and_index(root.path().to_str().unwrap(), None).unwrap();
    (root, cg)
}

fn edges_of(cg: &CodeGraph) -> Vec<String> {
    let nodes = cg.store().get_all_nodes().unwrap();
    let mut out = Vec::new();
    for node in &nodes {
        for edge in cg.get_outgoing_edges(&node.id).unwrap() {
            let kind = match edge.kind {
                EdgeKind::Calls => "calls",
                EdgeKind::References => "references",
                _ => continue,
            };
            let Some(target) = cg.get_node(&edge.target).unwrap() else { continue };
            out.push(format!(
                "{kind} {}@{} -> {}@{}#{}",
                node.qualified_name,
                node.file_path,
                target.qualified_name,
                target.file_path,
                edge.line.unwrap_or(0)
            ));
        }
    }
    out.sort();
    out.dedup();
    out
}

fn has(edges: &[String], needle: &str) -> bool {
    edges.iter().any(|e| e.contains(needle))
}

fn bodies() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "satisfies",
            "const rule = {\n  create(context) {\n    function report() {}\n    function visit() { report(); }\n    return { Program: visit };\n  },\n} satisfies Rule;\n",
        ),
        (
            "as const",
            "const rule = {\n  create(context) {\n    function report() {}\n    function visit() { report(); }\n    return { Program: visit };\n  },\n} as const;\n",
        ),
        (
            "a nested object",
            "const rule = {\n  rules: {\n    create(context) {\n      function report() {}\n      function visit() { report(); }\n      return { Program: visit };\n    },\n  },\n};\n",
        ),
        (
            "a computed key",
            "const key = \"create\";\nconst rule = {\n  plain() {},\n  [key](context) {\n    function report() {}\n    function visit() { report(); }\n    return { Program: visit };\n  },\n};\n",
        ),
        (
            "a callback a member passes on",
            "declare function wrap(f: unknown): unknown;\nconst rule = {\n  create: wrap(function (context) {\n    function report() {}\n    function visit() { report(); }\n    return { Program: visit };\n  }),\n};\n",
        ),
        (
            "a property read off the literal",
            "const key = \"create\";\nconst rule = {\n  create() {\n    function report() {}\n    function visit() { report(); }\n    return { Program: visit };\n  },\n}[key];\n",
        ),
    ]
}

#[test]
fn a_neighbour_in_the_method_reaches_the_function() {
    for (shape, body) in bodies() {
        let source = format!("{body}export default rule;\n");
        let (_root, cg) = index(&[("rule.ts", &source)]);
        let got = edges_of(&cg);
        assert!(has(&got, "calls rule::visit@rule.ts -> rule::report@rule.ts"), "{shape}: {got:?}");
        assert!(has(&got, "references rule@rule.ts -> rule::visit@rule.ts"), "{shape}: {got:?}");
        cg.close();
    }
}

#[test]
fn a_vue_component_reaches_the_function() {
    let source = [
        "<template><p>Don't {{ msg }}</p></template>",
        "<script setup lang=\"ts\">",
        "const rule = {",
        "  create() {",
        "    function report() {}",
        "    function visit() { report(); }",
        "    return visit;",
        "  },",
        "} satisfies Rule;",
        "</script>",
        "",
    ]
    .join("\n");
    let (_root, cg) = index(&[("Panel.vue", &source)]);
    let got = edges_of(&cg);
    assert!(has(&got, "calls rule::visit@Panel.vue -> rule::report@Panel.vue"), "{got:?}");
    cg.close();
}

#[test]
fn the_function_stays_in_the_block_that_declares_it() {
    let (_root, cg) = index(&[
        (
            "rule.ts",
            "const rule = {\n  create() {\n    function report() {}\n    return report();\n  },\n  check() { return report(); },\n} satisfies Rule;\nexport function outside() { return report(); }\nexport default rule;\n",
        ),
        ("other.ts", "export function elsewhere() { return report(); }\n"),
    ]);
    let got = edges_of(&cg);
    let lines: Vec<String> = got
        .iter()
        .filter(|e| e.starts_with("calls ") && e.contains(" -> rule::report@rule.ts#"))
        .map(|e| e.rsplit('#').next().unwrap().to_string())
        .collect();
    assert_eq!(lines, vec!["4".to_string()], "{got:?}");
    assert!(!has(&got, "calls outside@rule.ts -> rule::report@rule.ts"), "{got:?}");
    assert!(!has(&got, "calls elsewhere@other.ts -> rule::report@rule.ts"), "{got:?}");
    cg.close();
}

#[test]
fn a_member_is_reached_only_through_its_object() {
    let (_root, cg) = index(&[(
        "app.ts",
        "const App = {\n  init() { return 1; },\n  run() {\n    function inner() { return 2; }\n    return inner();\n  },\n};\nexport function bare() { return init(); }\nexport function through() { return App.init(); }\nexport default App;\n",
    )]);
    let got = edges_of(&cg);
    assert!(!has(&got, "calls bare@app.ts -> "), "{got:?}");
    assert!(has(&got, "calls through@app.ts -> App::init@app.ts"), "{got:?}");
    assert!(has(&got, "calls App::run@app.ts -> App::run::inner@app.ts"), "{got:?}");
    cg.close();
}
