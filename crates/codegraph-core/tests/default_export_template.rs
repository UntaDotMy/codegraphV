//! Upstream ed6b88e5 (2968): an `export default` line inside a template literal is text, not
//! the module's default. Ported from the template-literal cases of
//! `__tests__/default-export-declaration.test.ts`.
use codegraph_core::CodeGraph;
use codegraph_types::EdgeKind;
use std::fs;

fn index(files: &[(&str, &str)]) -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    for (rel, source) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

fn calls(cg: &CodeGraph, file: &str, function: &str) -> Vec<String> {
    let from = cg
        .get_nodes_by_file(file)
        .unwrap()
        .into_iter()
        .find(|n| n.name == function && n.kind.as_str() == "function")
        .unwrap();
    let mut out: Vec<String> = cg
        .get_outgoing_edges(&from.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Calls)
        .filter_map(|e| cg.get_node(&e.target).unwrap())
        .map(|n| format!("{}:{}", n.file_path, n.name))
        .collect();
    out.sort();
    out
}

#[test]
fn a_default_line_in_template_text_does_not_name_the_default() {
    let (_dir, cg) = index(&[
        ("src/gen/issue.ts", "export function App() { return 1; }\nexport function Main() { return 2; }\nexport const template = `\nexport default App\n`;\nexport default Main;\n"),
        ("src/gen/render.ts", "export function App() { return 1 }\nexport function Main() { return 2 }\nexport function render() {\n  return `intro ${`nested`}\nexport default App\n`\n}\nexport default Main\n"),
        ("src/gen/write.ts", "import { writeFileSync } from \"fs\"\nexport function App() { return 1 }\nexport function Main() { return 2 }\nwriteFileSync(\"out.ts\", `\nexport default App\n`)\nexport class Emitter {\n  sample = `\nexport default App\n`\n}\nexport default Main\n"),
        ("src/gen/plain.ts", "export function App() { return 1 }\nexport function Main() { return 2 }\nconst tick = /[`]/g\nconst half = (App() + Main()) / 2 + \"`\"\nexport default Main\n"),
        ("src/gen/unbalanced.ts", "export function App() { return 1 }\nexport function Main() { return 2 }\nexport function check(s: string, ok: boolean) {\n  if (ok) /`/.test(s)\n  return s\n}\nexport default Main\n"),
        ("src/gen/use.ts", "import Issue from \"./issue\"\nimport Render from \"./render\"\nimport Write from \"./write\"\nimport Plain from \"./plain\"\nimport Unbalanced from \"./unbalanced\"\n\nexport function generate() {\n  Issue()\n  Render()\n  Write()\n  Plain()\n  Unbalanced()\n}\n"),
    ]);
    let got = calls(&cg, "src/gen/use.ts", "generate");
    for file in ["issue", "render", "write"] {
        assert!(got.contains(&format!("src/gen/{file}.ts:Main")), "{file}: {got:?}");
        assert!(!got.contains(&format!("src/gen/{file}.ts:App")), "{file}: {got:?}");
    }
    assert!(got.contains(&"src/gen/plain.ts:Main".to_string()), "{got:?}");
    assert!(got.contains(&"src/gen/unbalanced.ts:Main".to_string()), "{got:?}");
    assert!(!got.contains(&"src/gen/plain.ts:App".to_string()), "{got:?}");
    assert!(!got.contains(&"src/gen/unbalanced.ts:App".to_string()), "{got:?}");
    cg.close();
}
