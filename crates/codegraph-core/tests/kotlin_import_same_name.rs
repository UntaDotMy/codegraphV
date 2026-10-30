//! A Kotlin call on an imported type reaches that package's class when two share a name (upstream
//! 86c85cff, #314). The class lives in a file NOT named after it (`Converters.kt`, the idiomatic
//! Kotlin layout), so the file-path suffix heuristic (`com/example/b/FooConverter.kt`) cannot
//! match; only the package-qualified name (`com.example.b::FooConverter`) disambiguates.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

fn write(root: &std::path::Path, relative: &str, text: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

#[test]
fn an_import_disambiguates_same_name_classes_in_non_eponymous_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "module-a/src/main/kotlin/com/example/a/Converters.kt", "package com.example.a\nclass FooConverter { fun convert(x: String): String { return \"a:\" + x } }\n");
    write(root, "module-b/src/main/kotlin/com/example/b/Converters.kt", "package com.example.b\nclass FooConverter { fun convert(x: String): String { return \"b:\" + x } }\n");
    // The caller imports the module-b converter; module-a is lexically first in the candidate
    // list, so a correct result proves the import wins.
    write(
        root,
        "web/src/main/kotlin/com/example/web/Handler.kt",
        "package com.example.web\n\nimport com.example.b.FooConverter\n\nclass Handler {\n  private val fooConverter: FooConverter = FooConverter()\n  fun use(): String { return fooConverter.convert(\"input\") }\n}\n",
    );
    let cg = CodeGraph::init_and_index(root.to_str().unwrap(), None).unwrap();
    let use_method = cg.get_nodes_by_kind(NodeKind::Method).unwrap().into_iter().find(|n| n.qualified_name.ends_with("Handler::use")).expect("Handler.use is indexed");
    let calls: Vec<_> = cg.get_outgoing_edges(&use_method.id).unwrap().into_iter().filter(|e| e.kind == EdgeKind::Calls).collect();
    assert!(!calls.is_empty(), "Handler.use should call something");
    let target = cg.get_node(&calls[0].target).unwrap().expect("the call target exists");
    assert_eq!(target.name, "convert");
    assert_eq!(target.qualified_name, "com.example.b::FooConverter::convert");
}
