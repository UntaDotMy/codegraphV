//! Upstream fa03fa6b: callers/callees keep one row per symbol with every call site, and a
//! recursive call appears as a row for the symbol itself. Ported from
//! `__tests__/callers-call-sites.test.ts`.
use codegraph_core::CodeGraph;
use codegraph_mcp::{render, tools::ToolHandler};
use serde_json::json;
use std::fs;

const BUSY: usize = 30;

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let report: Vec<String> = [
        "export class Logger {",
        "  log(message: string): void {}",
        "}",
        "",
        "export class Report {",
        "  constructor(private readonly logger: Logger) {}",
        "",
        "  build(): void {",
        "    this.logger.log('start');",
        "    this.logger.log('end');",
        "  }",
        "}",
        "",
        "export class TreeNode {",
        "  children: TreeNode[] = [];",
        "",
        "  walk(): void {",
        "    this.walk();",
        "  }",
        "}",
        "",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    let busy = format!(
        "import {{ Logger }} from './report';\n\nexport function busy(logger: Logger): void {{\n{}\n}}\n",
        (0..BUSY).map(|i| format!("  logger.log('{i}');")).collect::<Vec<_>>().join("\n")
    );
    let files: &[(&str, &str)] = &[
        ("src/once.ts", "import { Logger } from './report';\n\nexport function once(logger: Logger): void {\n  logger.log('only');\n}\n"),
        ("src/math.ts", "export function factorial(n: number): number {\n  return n <= 1 ? 1 : n * factorial(n - 1);\n}\n\nexport function tally(): number {\n  return factorial(3) + factorial(4);\n}\n"),
    ];
    for (path, source) in files {
        let path = dir.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    fs::write(dir.path().join("src/report.ts"), report.join("\n")).unwrap();
    fs::write(dir.path().join("src/busy.ts"), busy).unwrap();
    dir
}

fn named(cg: &CodeGraph, file: &str, name: &str) -> codegraph_types::Node {
    cg.get_nodes_by_file(file).unwrap().into_iter().find(|n| n.name == name).unwrap()
}

#[test]
fn callers_keep_one_row_per_symbol_with_every_site() {
    let dir = project();
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let log = named(&cg, "src/report.ts", "log");
    let (rows, cut) = cg.get_call_sites(&log.id, true, 10_000).unwrap();
    assert!(!cut);
    let build: Vec<_> = rows.iter().filter(|r| r.node.name == "build").collect();
    assert_eq!(build.len(), 1);
    assert_eq!(build[0].edges.iter().filter_map(|e| e.line).collect::<Vec<_>>(), [9, 10]);
    assert!(!build[0].recursive);
    let once = rows.iter().find(|r| r.node.name == "once").unwrap();
    assert_eq!(once.edges.iter().filter_map(|e| e.line).collect::<Vec<_>>(), [4]);
    let busy = rows.iter().find(|r| r.node.name == "busy").unwrap();
    assert_eq!(busy.edges.len(), BUSY);

    // The legacy walk keeps its first-edge-only, never-self shape.
    let legacy = cg.get_callers(&log.id, 1).unwrap();
    assert!(legacy.iter().all(|r| r.node.id != log.id));
    let build_legacy: Vec<_> = legacy.iter().filter(|r| r.node.name == "build").collect();
    assert_eq!(build_legacy.len(), 1);
    assert_eq!(build_legacy[0].edge.line, Some(9));
    cg.close();
}

#[test]
fn a_recursive_symbol_is_its_own_caller_and_callee() {
    let dir = project();
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    for (file, name, caller_line, callee_line) in [("src/report.ts", "walk", 18, 17), ("src/math.ts", "factorial", 2, 1)] {
        let node = named(&cg, file, name);
        for (incoming, line) in [(true, caller_line), (false, callee_line)] {
            let (rows, _) = cg.get_call_sites(&node.id, incoming, 10_000).unwrap();
            let self_rows: Vec<_> = rows.iter().filter(|r| r.node.id == node.id).collect();
            assert_eq!(self_rows.len(), 1, "{name} incoming={incoming}");
            assert!(self_rows[0].recursive);
            // Callers are shown at the call site; callees at the declaration the call leads to.
            let shown = if incoming { self_rows[0].edges.iter().filter_map(|e| e.line).collect::<Vec<_>>() } else { vec![self_rows[0].node.start_line] };
            assert_eq!(shown, [line], "{name} incoming={incoming}");
        }
        // The old walk still excludes the symbol itself.
        for refs in [cg.get_callers(&node.id, 1).unwrap(), cg.get_callees(&node.id, 1).unwrap()] {
            assert!(refs.iter().all(|r| r.node.id != node.id), "{name}");
        }
    }
    cg.close();
}

#[test]
fn the_text_surfaces_show_sites_recursion_and_a_cap() {
    let dir = project();
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let callers = render::relations(&cg, &json!({"symbol":"Logger.log"}), true).unwrap();
    assert!(callers.contains("- build (method) - src/report.ts:9, 10"), "{callers}");
    assert!(callers.contains("- once (function) - src/once.ts:4"), "{callers}");
    assert!(callers.contains(&format!("- busy (function) - src/busy.ts:4, 5, 6, 7, 8, 9, 10, 11, 12, 13, +{} more", BUSY - 10)), "{callers}");
    let factorial = render::relations(&cg, &json!({"symbol":"factorial"}), true).unwrap();
    assert!(factorial.contains("- factorial (function) - src/math.ts:2 — calls itself"), "{factorial}");

    let mut handler = ToolHandler::new(Some(dir.path().to_string_lossy().into_owned()), false);
    handler.catch_up = false;
    let trail = handler.call("codegraphV_node", &json!({"symbol":"factorial"})).message();
    assert!(trail.contains("factorial (src/math.ts:2, calls itself)"), "{trail}");
    assert!(trail.contains("tally (src/math.ts:6)"), "{trail}");
    let log_trail = handler.call("codegraphV_node", &json!({"symbol":"Logger.log"})).message();
    assert!(log_trail.contains("build (src/report.ts:9, 10)"), "{log_trail}");
    assert!(log_trail.contains(&format!("busy (src/busy.ts:4, 5, 6, +{} more)", BUSY - 3)), "{log_trail}");
    cg.close();
}
