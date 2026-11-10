//! Upstream 787eafbf: a Java or Kotlin import names a declaration by its qualified name,
//! whichever JVM language declares it. Ported from `__tests__/jvm-cross-language-imports.test.ts`.
use codegraph_core::CodeGraph;
use codegraph_types::EdgeKind;
use std::fs;

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let files: &[(&str, &str)] = &[
        ("p/JUtil.java", "package p;\npublic class JUtil {\n    public static int make() { return 1; }\n    public static class Nested { public static int deep() { return 2; } }\n}\n"),
        ("other/JUtil.java", "package other;\npublic class JUtil {\n    public static int make() { return 9; }\n    public static class Nested { public static int deep() { return 9; } }\n}\n"),
        ("p/Outer.java", "package p;\npublic class Outer { public static class Inner { public Inner() {} } }\n"),
        ("p/Plain.java", "package p;\npublic class Plain { public static class Nest { void run() {} } }\n"),
        ("p/KUtil.kt", "package p\n\nobject KUtil {\n    fun kmake(): Int = 1\n}\n\nclass KCompanion {\n    companion object {\n        fun make(): Int = 1\n    }\n}\n"),
        ("other/KUtil.kt", "package other\n\nobject KUtil {\n    fun kmake(): Int = 9\n}\n\nclass KCompanion {\n    companion object {\n        fun make(): Int = 9\n    }\n}\n"),
        ("p/Utils.kt", "package p\n\nfun helper(): Int = 1\n"),
        ("other/Utils.kt", "package other\n\nfun helper(): Int = 9\n"),
        ("p/Modes.kt", "package p\n\nobject Modes\n\nfun Modes.shared() {}\n"),
        ("p/Ext.kt", "package p\n\nfun Modes.extOther() {}\nfun Modes.hidden() {}\n"),
        ("q/FromKotlin.kt", "package q\n\nimport p.JUtil\nimport p.Outer\nimport p.Plain\nimport p.Modes\nimport p.extOther\n\nclass FromKotlin {\n    fun staticCall() { JUtil.make() }\n    fun nestedStatic() { JUtil.Nested.deep() }\n    fun javaCtor() { Outer.Inner() }\n    fun javaNoCtor() { Plain.Nest() }\n    fun importedExtension() { Modes.extOther() }\n    fun unimportedExtension() { Modes.hidden() }\n    fun besideTypeNotImported() { Modes.shared() }\n}\n"),
        ("q2/Shared.kt", "package q2\n\nimport p.Modes\nimport p.shared\n\nfun useShared() { Modes.shared() }\n"),
        ("q/FromJava.java", "package q;\n\nimport p.KUtil;\nimport p.KCompanion;\nimport p.UtilsKt;\n\npublic class FromJava {\n    void objectCall() { KUtil.INSTANCE.kmake(); }\n    void companionCall() { KCompanion.Companion.make(); }\n    void topLevelCall() { UtilsKt.helper(); }\n}\n"),
    ];
    for (path, source) in files {
        let path = dir.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    dir
}

fn callees(cg: &CodeGraph, file: &str, caller: &str) -> Vec<String> {
    let nodes = cg.store().get_all_nodes().unwrap();
    let node = nodes.iter().find(|n| n.file_path == file && n.name == caller).unwrap();
    let mut out: Vec<_> = cg.store().get_outgoing_edges(&node.id, &[EdgeKind::Calls, EdgeKind::Instantiates], None).unwrap()
        .into_iter()
        .filter_map(|e| nodes.iter().find(|n| n.id == e.target).map(|n| format!("{} {}", e.kind.as_str(), n.qualified_name)))
        .collect();
    out.sort();
    out
}

#[test]
fn jvm_cross_language_imports_reach_the_named_declaration() {
    let dir = project();
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let cases: &[(&str, &str, &[&str])] = &[
        ("q/FromKotlin.kt", "staticCall", &["calls p::JUtil::make"]),
        ("q/FromKotlin.kt", "nestedStatic", &["calls p::JUtil::Nested::deep"]),
        // An explicit Java constructor is reached by the instantiation edge; an implicit one by
        // the same edge to its class (native models instantiation as `instantiates`).
        ("q/FromKotlin.kt", "javaCtor", &["instantiates p::Outer::Inner"]),
        ("q/FromKotlin.kt", "javaNoCtor", &["instantiates p::Plain::Nest"]),
        ("q/FromKotlin.kt", "importedExtension", &["calls Modes::extOther"]),
        ("q/FromKotlin.kt", "unimportedExtension", &[]),
        ("q/FromKotlin.kt", "besideTypeNotImported", &[]),
        ("q2/Shared.kt", "useShared", &["calls Modes::shared"]),
        ("q/FromJava.java", "objectCall", &["calls p::KUtil::kmake"]),
        ("q/FromJava.java", "companionCall", &["calls p::KCompanion::make"]),
        ("q/FromJava.java", "topLevelCall", &["calls p::helper"]),
    ];
    for (file, caller, expected) in cases {
        assert_eq!(callees(&cg, file, caller), *expected, "{file} {caller}");
    }
    // The Java import of a Kotlin file facade links that file, not a class by its name.
    let nodes = cg.store().get_all_nodes().unwrap();
    let ids: Vec<_> = nodes.iter().filter(|n| n.file_path == "q/FromJava.java").map(|n| n.id.clone()).collect();
    let mut imported: Vec<_> = ids.iter().flat_map(|id| cg.store().get_outgoing_edges(id, &[EdgeKind::Imports], None).unwrap())
        .filter_map(|e| nodes.iter().find(|n| n.id == e.target))
        .map(|n| if n.kind == codegraph_types::NodeKind::File { format!("file {}", n.file_path) } else { format!("{} {}", n.kind.as_str(), n.qualified_name) })
        .collect();
    imported.sort();
    assert_eq!(imported, ["class p::KCompanion", "class p::KUtil", "file p/Utils.kt"]);
    // Every import in the Kotlin caller binds: the extension import included.
    assert!(cg.store().get_unresolved_references().unwrap().iter().all(|r| r.file_path.as_deref() != Some("q/FromKotlin.kt")
        || r.reference_kind != codegraph_types::ReferenceKind::Edge(EdgeKind::Imports)));
    cg.close();
}
