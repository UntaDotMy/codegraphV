//! Upstream 9f379af7 (1657): codegraph_node keeps a late language and a late top-level folder
//! visible when a name has more definitions than its caps hold. Ported from
//! `__tests__/node-overload-spread.test.ts`.
use codegraph_core::CodeGraph;
use codegraph_mcp::render;
use serde_json::json;
use std::fs;

fn write(root: &std::path::Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn node_text(cg: &CodeGraph, args: serde_json::Value) -> String {
    render::node(cg, &args).unwrap().message()
}

#[test]
fn a_late_language_and_folder_survive_the_caps() {
    let dir = tempfile::tempdir().unwrap();
    for f in 0..40 {
        let classes: String = (0..10)
            .map(|c| format!("export class C{f}_{c} {{\n  num(v: number): string {{\n    return v.toFixed({c});\n  }}\n}}\n"))
            .collect();
        write(dir.path(), &format!("aaa_client/m{f:02}.ts"), &classes);
    }
    write(dir.path(), "bbb_lib/helper.ts", "export function num(v: number): number {\n  return Math.round(v);\n}\n");
    write(dir.path(), "zzz_server/format.py", "def num(value):\n    return round(value, 2)\n");
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();

    let text = node_text(&cg, json!({"symbol":"num","includeCode":true}));
    assert!(text.contains("**402 definitions named \"num\"**"), "{text}");
    assert!(text.contains("Returning 17 in full; 385 more listed below"), "{text}");
    assert!(text.contains("**Location:** zzz_server/format.py:1"), "{text}");
    assert!(text.contains("def num(value):"), "{text}");
    let ts_bodies: Vec<&str> = text
        .lines()
        .filter_map(|line| line.strip_prefix("**Location:** "))
        .filter(|loc| loc.starts_with("aaa_client/"))
        .collect();
    let mut expected: Vec<String> = (0..10).map(|c| format!("aaa_client/m00.ts:{}", 2 + 5 * c)).collect();
    expected.extend((0..6).map(|c| format!("aaa_client/m01.ts:{}", 2 + 5 * c)));
    assert_eq!(ts_bodies, expected);

    let listed_start = text.find("**Other definitions**").expect("a list of the rest");
    let listed: Vec<&str> = text[listed_start..]
        .lines()
        .filter_map(|line| line.strip_prefix("- `num` ("))
        .filter_map(|rest| rest.split_once(") — ").map(|(_, loc)| loc))
        .collect();
    assert_eq!(listed.len(), 20);
    assert!(listed.contains(&"bbb_lib/helper.ts:1"), "{listed:?}");
    assert!(text.contains("- … +365 more: 365 typescript in aaa_client/"), "{text}");
    assert!(!text.lines().any(|line| line.starts_with("- … +") && line.ends_with(" more")), "the bare overflow line is gone:\n{text}");
    assert!(text.chars().count() <= 15_000);
    cg.close();
}

#[test]
fn a_set_that_fits_every_cap_reads_as_before() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a/x.ts", "export function num(v: number): string {\n  return String(v);\n}\n");
    write(dir.path(), "b/y.py", "def num(value):\n    return str(value)\n");
    write(dir.path(), "c/z.ts", "export function num(v: number): number {\n  return v;\n}\n");
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let text = node_text(&cg, json!({"symbol":"num","includeCode":true}));
    assert!(text.contains("Returning 3 in full — pick the one you need"), "{text}");
    let locations: Vec<&str> = text.lines().filter_map(|line| line.strip_prefix("**Location:** ")).collect();
    assert_eq!(locations, ["a/x.ts:1", "b/y.py:1", "c/z.ts:1"]);
    assert!(!text.contains("**Other definitions**"), "{text}");
    cg.close();
}
