//! A Kotlin import from outside the project owns its name (#2196): after `import
//! androidx.compose.ui.Modifier`, `Modifier.fillMaxSize()` is Compose's, never a project method that
//! happens to be called `fillMaxSize`. But Kotlin lets the project extend that type (`fun
//! Modifier.pad()`, indexed as `Modifier::pad`), and `Modifier.pad()` is that extension when the
//! call site can see it: in its own package, or imported by name or `.*`. `fun
//! Color.Companion.fromHex()` is called the same way, `Color.fromHex()` (upstream 40f58517).
use codegraph_core::CodeGraph;
use codegraph_types::EdgeKind;
use std::fs;

fn index() -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    let files: [(&str, &str); 6] = [
        (
            "src/main/kotlin/app/ui/Ext.kt",
            "package app.ui\n\nimport androidx.compose.ui.Modifier\nimport androidx.compose.ui.graphics.Color\n\nfun Modifier.pad(): Modifier = this\n\nfun Modifier.hidden(): Modifier = this\n\nfun Color.Companion.fromHex(hex: String): Color = Color.Red\n",
        ),
        ("src/main/kotlin/app/other/Box.kt", "package app.other\n\nclass Box {\n    fun fillMaxSize(): Box = this\n}\n"),
        ("src/main/kotlin/app/gen/Gen.kt", "package app.gen\n\nimport javax.lang.model.element.Modifier\n\nfun Modifier.keyword(): String = name\n"),
        (
            "src/main/kotlin/app/ui/Screen.kt",
            "package app.ui\n\nimport androidx.compose.ui.Modifier\nimport androidx.compose.ui.graphics.Color\n\nfun screen() {\n    Modifier.pad()\n    Modifier.fillMaxSize()\n    Color.fromHex(\"#fff\")\n}\n",
        ),
        (
            "src/main/kotlin/app/feature/Feed.kt",
            "package app.feature\n\nimport androidx.compose.ui.Modifier\nimport app.gen.keyword\nimport app.ui.pad\n\nfun feed() {\n    Modifier.pad()\n    Modifier.hidden()\n    Modifier.keyword()\n}\n",
        ),
        (
            "src/main/kotlin/app/settings/Settings.kt",
            "package app.settings\n\nimport androidx.compose.ui.Modifier\nimport app.ui.*\n\nfun settings() {\n    Modifier.hidden()\n}\n",
        ),
    ];
    for (relative, content) in files {
        let path = dir.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

/// `kind qualified-name` of every non-structural edge out of a file.
fn edges_from(cg: &CodeGraph, file: &str) -> Vec<String> {
    let mut edges = Vec::new();
    for node in cg.get_nodes_by_file(file).unwrap() {
        for edge in cg.get_outgoing_edges(&node.id).unwrap() {
            if matches!(edge.kind, EdgeKind::Contains | EdgeKind::Imports) {
                continue;
            }
            let target = cg.get_node(&edge.target).unwrap().expect("edge target exists");
            edges.push(format!("{} {}", edge.kind.as_str(), target.qualified_name));
        }
    }
    edges.sort();
    edges
}

#[test]
fn an_extension_is_what_a_call_on_that_type_reaches_from_the_same_package() {
    let (_dir, cg) = index();
    assert_eq!(edges_from(&cg, "src/main/kotlin/app/ui/Screen.kt"), ["calls Color::fromHex", "calls Modifier::pad"]);
}

#[test]
fn an_extension_is_reached_from_another_package_only_through_an_import_of_it() {
    let (_dir, cg) = index();
    assert_eq!(edges_from(&cg, "src/main/kotlin/app/feature/Feed.kt"), ["calls Modifier::pad"]);
    assert_eq!(edges_from(&cg, "src/main/kotlin/app/settings/Settings.kt"), ["calls Modifier::hidden"]);
}
