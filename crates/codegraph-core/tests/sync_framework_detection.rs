//! A long-lived graph keeps framework detection current across syncs (upstream 31ec9082, #2516,
//! #2519).
//!
//! The MCP server's watcher holds one `CodeGraph` for the whole session. Its framework detection
//! used to be cached from the first time it ran, so a framework that arrived mid-session, like
//! Express in a new `package.json`, was missing from every file synced after it until
//! `codegraphV index`. A sync now detects again when what detection read changed, without walking
//! the project on a watcher's scoped sync, and says when files indexed earlier were read without
//! the change.
use codegraph_core::{CodeGraph, FrameworkChanges, IndexOptions, OpenOptions};
use codegraph_db::Store;
use codegraph_resolve::ctx::Ctx;
use codegraph_resolve::frameworks::package_deps::declared_dependencies;
use codegraph_types::NodeKind;
use std::fs;
use std::path::Path;
use std::rc::Rc;

const EXPRESS: &str = r#"{"dependencies":{"express":"^4.18.0"}}"#;
const ROUTES: &str = "const app = express();\napp.get('/hello', function hello(req, res) {});\n";

fn write(root: &Path, relative: &str, body: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn routes(cg: &CodeGraph, relative: &str) -> Vec<String> {
    cg.get_nodes_by_kind(NodeKind::Route).unwrap().into_iter().filter(|n| n.file_path == relative).map(|n| n.name).collect()
}

fn scoped(paths: &[&str]) -> IndexOptions {
    IndexOptions { paths: Some(paths.iter().map(|p| p.to_string()).collect()), ..IndexOptions::default() }
}

fn has(cg: &CodeGraph, framework: &str) -> bool {
    cg.get_detected_frameworks().iter().any(|f| f == framework)
}

fn root_of(dir: &tempfile::TempDir) -> String {
    dir.path().to_str().unwrap().to_string()
}

#[test]
fn a_file_added_with_a_new_framework_is_read_with_it_by_a_full_sync() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/app.ts", "export function boot() { return 1; }\n");
    let mut cg = CodeGraph::init_and_index(&root_of(&dir), None).unwrap();
    // An ordinary edit first, so this graph has detected (no frameworks) and cached it.
    write(dir.path(), "src/app.ts", "export function boot() { return 22; }\n");
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert!(!has(&cg, "express"));

    write(dir.path(), "package.json", EXPRESS);
    write(dir.path(), "src/routes.js", ROUTES);
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert!(has(&cg, "express"), "{:?}", cg.get_detected_frameworks());
    assert_eq!(routes(&cg, "src/routes.js"), ["GET /hello"]);
}

#[test]
fn a_file_added_with_a_new_framework_is_read_with_it_by_a_scoped_sync() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/app.ts", "export function boot() { return 1; }\n");
    let mut cg = CodeGraph::init_and_index(&root_of(&dir), None).unwrap();
    write(dir.path(), "src/app.ts", "export function boot() { return 22; }\n");
    cg.sync(&scoped(&["src/app.ts"]), None).unwrap();
    assert!(!has(&cg, "express"));

    write(dir.path(), "package.json", EXPRESS);
    write(dir.path(), "src/routes.js", ROUTES);
    cg.sync(&scoped(&["src/routes.js"]), None).unwrap();
    assert!(has(&cg, "express"), "{:?}", cg.get_detected_frameworks());
    assert_eq!(routes(&cg, "src/routes.js"), ["GET /hello"]);
}

#[test]
fn detects_again_when_a_file_is_added_with_no_manifest_change() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/app.ts", "export function boot() { return 1; }\n");
    let mut cg = CodeGraph::init_and_index(&root_of(&dir), None).unwrap();
    assert!(!has(&cg, "svelte"));
    // Svelte is detected from a .svelte file alone.
    write(dir.path(), "src/Widget.svelte", "<script>let count = 0;</script>\n<button>{count}</button>\n");
    cg.sync(&scoped(&["src/Widget.svelte"]), None).unwrap();
    assert!(has(&cg, "svelte"), "{:?}", cg.get_detected_frameworks());
}

#[test]
fn detects_again_when_a_source_file_detection_read_is_edited() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/app.ts", "export function boot() { return 1; }\n");
    let mut cg = CodeGraph::init_and_index(&root_of(&dir), None).unwrap();
    assert!(!has(&cg, "react-native-bridge"));
    // The bridge is detected from a TurboModule spec in the first files' source.
    write(dir.path(), "src/app.ts", "export const Native = TurboModuleRegistry.getEnforcing<Spec>('Native');\n");
    cg.sync(&scoped(&["src/app.ts"]), None).unwrap();
    assert!(has(&cg, "react-native-bridge"), "{:?}", cg.get_detected_frameworks());
}

#[test]
fn an_edit_detection_did_not_read_leaves_it_alone() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "package.json", EXPRESS);
    write(dir.path(), "src/routes.js", ROUTES);
    write(dir.path(), "src/util.js", "function util() { return 1; }\n");
    let mut cg = CodeGraph::init_and_index(&root_of(&dir), None).unwrap();
    cg.sync(&scoped(&["src/util.js"]), None).unwrap();
    let (runs, _) = cg.framework_detection_stats();
    write(dir.path(), "src/util.js", "function util() { return 22; }\n");
    let result = cg.sync(&scoped(&["src/util.js"]), None).unwrap();
    assert_eq!(result.files_modified, 1);
    assert_eq!(cg.framework_detection_stats().0, runs, "an edit nothing read must not detect again");
    assert!(has(&cg, "express"));
}

#[test]
fn notices_a_manifest_edit_at_the_next_sync_and_says_what_the_files_indexed_before_it_lack() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/server.js", ROUTES);
    write(dir.path(), "src/util.js", "function util() { return 1; }\n");
    let mut cg = CodeGraph::init_and_index(&root_of(&dir), None).unwrap();
    assert!(routes(&cg, "src/server.js").is_empty());
    assert_eq!(cg.get_framework_changes_since_index(), None);

    // No source file changes with the manifest; the next edit anywhere syncs it.
    write(dir.path(), "package.json", EXPRESS);
    write(dir.path(), "src/util.js", "function util() { return 22; }\n");
    cg.sync(&scoped(&["src/util.js"]), None).unwrap();
    assert!(has(&cg, "express"), "{:?}", cg.get_detected_frameworks());
    // server.js was indexed without Express and is unchanged: status recommends a re-index.
    assert!(routes(&cg, "src/server.js").is_empty());
    assert_eq!(cg.get_framework_changes_since_index(), Some(FrameworkChanges { added: vec!["express".into()], removed: vec![] }));

    // Going back to what the index was built with clears it.
    write(dir.path(), "package.json", r#"{"dependencies":{}}"#);
    write(dir.path(), "src/util.js", "function util() { return 333; }\n");
    cg.sync(&scoped(&["src/util.js"]), None).unwrap();
    assert!(!has(&cg, "express"), "{:?}", cg.get_detected_frameworks());
    assert_eq!(cg.get_framework_changes_since_index(), None);

    // A full re-index reads every file with the new frameworks, and the note goes.
    write(dir.path(), "package.json", EXPRESS);
    cg.close();
    let mut cg = CodeGraph::recreate(&root_of(&dir)).unwrap();
    cg.index_all(&IndexOptions::default(), None, None).unwrap();
    assert_eq!(routes(&cg, "src/server.js"), ["GET /hello"]);
    assert_eq!(cg.get_framework_changes_since_index(), None);
}

#[test]
fn a_scoped_sync_in_a_fresh_process_detects_from_the_index_once() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "package.json", EXPRESS);
    write(dir.path(), "src/routes.js", ROUTES);
    let built = CodeGraph::init_and_index(&root_of(&dir), None).unwrap();
    built.close();

    // The watcher's process: open, then sync one edited file at a time.
    let mut cg = CodeGraph::open(&root_of(&dir), OpenOptions::default()).unwrap();
    let before = cg.framework_detection_stats();
    for n in [2, 3] {
        write(dir.path(), "src/routes.js", &format!("{ROUTES}// edit {n}\n"));
        cg.sync(&scoped(&["src/routes.js"]), None).unwrap();
    }
    let after = cg.framework_detection_stats();
    assert_eq!(after.1 - before.1, 0, "a scoped sync must not walk the project to detect");
    assert_eq!(after.0 - before.0, 1, "detected once, and not again while nothing it read changed");
    assert_eq!(routes(&cg, "src/routes.js"), ["GET /hello"]);
}

#[test]
fn sees_one_app_swapped_for_another_with_as_many_files() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "web/package.json", r#"{"dependencies":{"react":"^18.0.0"}}"#);
    write(dir.path(), "web/a.js", "export function a() { return 1; }\n");
    write(dir.path(), "web/b.js", "export function b() { return 2; }\n");
    let mut cg = CodeGraph::init_and_index(&root_of(&dir), None).unwrap();
    assert!(has(&cg, "react"));

    fs::remove_dir_all(dir.path().join("web")).unwrap();
    write(dir.path(), "server/package.json", EXPRESS);
    write(dir.path(), "server/a.js", ROUTES);
    write(dir.path(), "server/b.js", "export function b() { return 2; }\n");
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert!(has(&cg, "express"), "{:?}", cg.get_detected_frameworks());
    assert!(!has(&cg, "react"), "{:?}", cg.get_detected_frameworks());
    cg.reinitialize_resolver();
    assert!(has(&cg, "express"), "{:?}", cg.get_detected_frameworks());
    assert!(!has(&cg, "react"), "{:?}", cg.get_detected_frameworks());
}

#[test]
fn reads_the_manifests_again_once_told_to_forget_them() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "package.json", r#"{"dependencies":{"react":"^18.0.0"}}"#);
    let store = Rc::new(Store::initialize(":memory:").unwrap());
    let ctx = Ctx::with_file_list(root_of(&dir), store, vec!["a.js".to_string()]);
    let names = |ctx: &Ctx| {
        let mut v: Vec<String> = declared_dependencies(ctx).iter().cloned().collect();
        v.sort();
        v
    };
    assert_eq!(names(&ctx), ["react"]);
    write(dir.path(), "package.json", EXPRESS);
    // Same file count: the count alone keeps the old answer.
    assert_eq!(names(&ctx), ["react"]);
    ctx.clear_caches();
    assert_eq!(names(&ctx), ["express"]);
}
