//! Resolution ties that decide a link break by UTF-16 code unit, not by locale collation (upstream
//! 7ff2841e). Collation puts `alpha/` before `Beta/`; code-unit order puts `Beta/` first, and it
//! does so on every host, so two machines index one repository the same way.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

fn index(files: &[(&str, &str)]) -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    for (relative, content) in files {
        let path = dir.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

#[test]
fn an_equal_proximity_field_call_tie_goes_to_the_code_unit_first_path() {
    // Two apps declare the same service; the caller is equally near both.
    let service = "export class UserService {\n  load(): string { return \"x\"; }\n}\n";
    let (_dir, cg) = index(&[
        ("src/apps/Beta/service.ts", service),
        ("src/apps/alpha/service.ts", service),
        (
            "src/shared/hub.ts",
            "export class Hub {\n  constructor(private readonly users: UserService) {}\n  run(): string { return this.users.load(); }\n}\n",
        ),
    ]);
    let run = cg.get_nodes_by_name("run").unwrap().into_iter().find(|n| n.file_path == "src/shared/hub.ts").expect("Hub.run");
    let callees: Vec<String> = cg.get_callees(&run.id, 1).unwrap().into_iter().map(|c| c.node.file_path).collect();
    assert_eq!(callees, ["src/apps/Beta/service.ts"]);
}

#[test]
fn two_copies_of_a_swift_bridge_class_resolve_to_the_code_unit_first_file() {
    let shim = "#import <React/RCTBridgeModule.h>\n#import <React/RCTViewManager.h>\n\n@interface RCT_EXTERN_MODULE(CaptureView, RCTViewManager)\n\nRCT_EXTERN_METHOD(syncSettings:(NSDictionary *)settings)\n\n@end\n";
    let swift = "import Foundation\n\n@objc(CaptureView)\nclass CaptureView: NSObject {\n  @objc func syncSettings(_ settings: NSDictionary) {}\n}\n";
    let (_dir, cg) = index(&[
        ("package.json", r#"{"name":"app","dependencies":{"react-native":"0.76"}}"#),
        ("ios/CaptureView.m", shim),
        ("ios/alpha/CaptureView.swift", swift),
        ("ios/Beta/CaptureView.swift", swift),
        (
            "src/a.tsx",
            "import { NativeModules } from 'react-native';\nconst captureView = NativeModules.CaptureView;\nexport function save() {\n  captureView.syncSettings({});\n}\n",
        ),
    ]);
    let save = cg.get_nodes_by_name("save").unwrap().into_iter().find(|n| n.kind == NodeKind::Function).expect("save");
    let targets: Vec<String> = cg
        .get_outgoing_edges(&save.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Calls)
        .filter_map(|e| cg.get_node(&e.target).unwrap())
        .map(|n| n.file_path)
        .collect();
    assert_eq!(targets, ["ios/Beta/CaptureView.swift"], "the bridge call should reach the Beta copy");
}

#[test]
fn an_alias_two_headers_declare_differently_takes_the_code_unit_first_file() {
    // rocksdb's `using DBWithTTLImplBase = …;` in each arm of an `#if`: the base is the first
    // declaration, by file in code-unit order.
    let (_dir, cg) = index(&[
        ("B/config_b.h", "#pragma once\nnamespace n {\nclass FastStore { public: void flush() {} };\n#ifdef USE_FAST\ntypedef FastStore Store;\n#endif\n}\n"),
        ("a/config_a.h", "#pragma once\nnamespace n {\nclass SafeStore { public: void flush() {} };\n#ifndef USE_FAST\ntypedef SafeStore Store;\n#endif\n}\n"),
        ("main.cpp", "#include \"B/config_b.h\"\n#include \"a/config_a.h\"\nnamespace n {\nclass Derived : public Store {};\n}\n"),
    ]);
    let derived = cg.get_nodes_by_name("Derived").unwrap().into_iter().find(|n| n.qualified_name.ends_with("n::Derived")).expect("Derived");
    let bases: Vec<String> = cg
        .get_outgoing_edges(&derived.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Extends)
        .filter_map(|e| cg.get_node(&e.target).unwrap())
        .map(|n| n.file_path)
        .collect();
    assert_eq!(bases, ["B/config_b.h"]);
}
