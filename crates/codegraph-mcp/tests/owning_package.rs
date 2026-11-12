//! The package that owns a file (upstream 17d5426d, #1871): the nearest package.json or
//! composer.json above it that names a package, read from disk when asked.
use codegraph_core::CodeGraph;
use codegraph_mcp::owning_package::{
    group_files_by_package, package_group_title, package_listing_title, OwningPackageLookup,
};
use codegraph_mcp::tools::ToolHandler;
use serde_json::json;
use std::path::Path;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn manifest(root: &Path, rel: &str, value: serde_json::Value) {
    write(root, rel, &value.to_string());
}

fn owner_name(root: &Path, file: &str) -> Option<String> {
    OwningPackageLookup::new(&root.to_string_lossy()).owner_of(file).map(|o| o.name.clone())
}

#[test]
fn takes_the_nearest_manifest_however_deep_the_file_is() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    manifest(root, "package.json", json!({"name": "workspace-root"}));
    manifest(root, "apps/api/package.json", json!({"name": "@acme/api"}));
    assert_eq!(owner_name(root, "apps/api/src/deep/er/a.ts").as_deref(), Some("@acme/api"));
    assert_eq!(owner_name(root, "apps/api/b.ts").as_deref(), Some("@acme/api"));
    assert_eq!(owner_name(root, "apps/web/c.ts").as_deref(), Some("workspace-root"));
    assert_eq!(owner_name(root, "d.ts").as_deref(), Some("workspace-root"));
}

#[test]
fn walks_past_a_manifest_that_names_nothing_and_says_so_when_nothing_does() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    manifest(root, "apps/api/package.json", json!({"name": "@acme/api"}));
    manifest(root, "apps/api/dist/esm/package.json", json!({"type": "module"}));
    manifest(root, "package.json", json!({"private": true}));
    assert_eq!(owner_name(root, "apps/api/dist/esm/index.js").as_deref(), Some("@acme/api"));
    assert_eq!(owner_name(root, "scripts/a.ts"), None);
}

#[test]
fn package_json_settles_a_directory_and_composer_answers_when_it_names_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    manifest(root, "lib/composer.json", json!({"name": "acme/lib"}));
    manifest(root, "lib/package.json", json!({"name": "@acme/lib"}));
    let owner = OwningPackageLookup::new(&root.to_string_lossy()).owner_of("lib/a.ts").unwrap();
    assert_eq!((owner.name.as_str(), owner.dir.as_str(), owner.manifest), ("@acme/lib", "lib", "package.json"));

    manifest(root, "lib/package.json", json!({"private": true}));
    let owner = OwningPackageLookup::new(&root.to_string_lossy()).owner_of("lib/a.php").unwrap();
    assert_eq!((owner.name.as_str(), owner.manifest), ("acme/lib", "composer.json"));
}

#[test]
fn a_bad_manifest_names_nothing_and_never_fails_the_listing() {
    for content in ["{ \"name\": ", "", "{ \"name\": \"   \" }", "{ \"name\": { \"first\": \"x\" } }", "[\"name\"]", "null"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        manifest(root, "package.json", json!({"name": "workspace-root"}));
        write(root, "lib/package.json", content);
        assert_eq!(owner_name(root, "lib/a.ts").as_deref(), Some("workspace-root"), "{content:?}");
    }
}

#[test]
fn reads_a_byte_order_mark_and_trims_the_name() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "lib/composer.json", "\u{feff}{ \"name\": \"acme/lib\" }");
    manifest(root, "web/package.json", json!({"name": "  @acme/web "}));
    assert_eq!(owner_name(root, "lib/a.php").as_deref(), Some("acme/lib"));
    assert_eq!(owner_name(root, "web/a.ts").as_deref(), Some("@acme/web"));
}

#[test]
fn skips_a_manifest_over_the_source_size_limit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    manifest(root, "package.json", json!({"name": "workspace-root"}));
    let pad = "x".repeat(codegraph_types::limits::max_source_file_bytes() as usize);
    write(root, "lib/package.json", &format!("{{\"name\":\"huge\",\"pad\":\"{pad}\"}}"));
    assert_eq!(owner_name(root, "lib/a.ts").as_deref(), Some("workspace-root"));
}

#[test]
fn does_not_read_a_manifest_reached_through_a_link_out_of_the_project() {
    let outside = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    manifest(outside.path(), "package.json", json!({"name": "from-outside"}));
    manifest(root, "package.json", json!({"name": "workspace-root"}));
    let link = root.join("linked");
    #[cfg(windows)]
    assert!(std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(&link).arg(outside.path()).output().unwrap().status.success());
    #[cfg(not(windows))]
    std::os::unix::fs::symlink(outside.path(), &link).unwrap();
    assert_eq!(owner_name(root, "linked/a.ts").as_deref(), Some("workspace-root"));
}

#[test]
fn shares_one_answer_across_a_package_and_rereads_for_the_next_listing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    manifest(root, "lib/package.json", json!({"name": "@acme/lib"}));
    let mut lookup = OwningPackageLookup::new(&root.to_string_lossy());
    let a = lookup.owner_of("lib/a.ts").unwrap();
    let b = lookup.owner_of("lib/src/b.ts").unwrap();
    assert!(std::rc::Rc::ptr_eq(&a, &b));
    manifest(root, "lib/package.json", json!({"name": "@acme/renamed"}));
    assert_eq!(owner_name(root, "lib/a.ts").as_deref(), Some("@acme/renamed"));
}

#[test]
fn orders_packages_by_name_then_directory_with_the_unowned_group_last() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    manifest(root, "z/package.json", json!({"name": "alpha"}));
    manifest(root, "a/package.json", json!({"name": "alpha"}));
    manifest(root, "m/package.json", json!({"name": "beta"}));
    let files = ["orphan.ts", "m/b.ts", "z/b.ts", "z/a.ts", "a/a.ts"];
    let groups = group_files_by_package(&root.to_string_lossy(), &files, |f| f);
    let owners: Vec<_> = groups.iter().map(|g| g.owner.as_ref().map(|o| format!("{}@{}", o.name, o.dir))).collect();
    assert_eq!(owners, vec![Some("alpha@a".into()), Some("alpha@z".into()), Some("beta@m".into()), None]);
    assert_eq!(groups[1].files, vec![&"z/a.ts", &"z/b.ts"]);
    assert_eq!(groups[3].files, vec![&"orphan.ts"]);
}

#[test]
fn words_a_listing_the_same_way_for_every_surface() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    manifest(root, "package.json", json!({"name": "workspace-root"}));
    manifest(root, "apps/api/package.json", json!({"name": "@acme/api"}));
    let files = ["apps/api/a.ts", "apps/api/b.ts", "c.ts"];
    let groups = group_files_by_package(&root.to_string_lossy(), &files, |f| f);
    assert_eq!(package_listing_title(&groups), "Files by Package (3 files, 2 packages)");
    let titles: Vec<_> = groups.iter().map(package_group_title).collect();
    assert_eq!(titles, vec!["@acme/api in apps/api (2 files)", "workspace-root (1 file)"]);
    let unowned = group_files_by_package(&root.join("nowhere").to_string_lossy(), &["x.ts"], |f| f);
    assert_eq!(package_group_title(&unowned[0]), "no owning package (1 file)");
    assert_eq!(package_listing_title(&unowned), "Files by Package (1 file, 0 packages)");
}

fn indexed_monorepo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    manifest(root, "package.json", json!({"private": true}));
    manifest(root, "apps/api/package.json", json!({"name": "@acme/api"}));
    manifest(root, "apps/web/package.json", json!({"name": "@acme/web"}));
    write(root, "apps/api/src/handler.ts", "export function handler() { return 1; }\n");
    write(root, "apps/web/src/view.ts", "export function view() { return 2; }\n");
    write(root, "scripts/tool.ts", "export function tool() { return 3; }\n");
    CodeGraph::init_and_index(&root.to_string_lossy(), None).unwrap().close();
    dir
}

fn listed(root: &Path, args: serde_json::Value) -> String {
    let mut handler = ToolHandler::new(Some(root.to_string_lossy().into_owned()), false);
    handler.catch_up = false;
    let mut call = json!({"format": "package"});
    for (k, v) in args.as_object().unwrap() {
        call[k] = v.clone();
    }
    let result = handler.call("codegraphV_files", &call);
    assert!(!result.is_error);
    result.content[0]["text"].as_str().unwrap().to_string()
}

#[test]
fn codegraphv_files_sections_by_package_with_the_unowned_last() {
    let dir = indexed_monorepo();
    let text = listed(dir.path(), json!({"includeMetadata": false}));
    let expected = [
        "**Files by Package (3 files, 2 packages)**",
        "",
        "**@acme/api in apps/api (1 file)**",
        "- apps/api/src/handler.ts",
        "",
        "**@acme/web in apps/web (1 file)**",
        "- apps/web/src/view.ts",
        "",
        "**no owning package (1 file)**",
        "- scripts/tool.ts",
        "",
    ]
    .join("\n");
    assert!(text.ends_with(&expected), "{text}");
    assert!(listed(dir.path(), json!({})).contains("- apps/api/src/handler.ts (typescript, "));
}

#[test]
fn codegraphv_files_finds_the_manifest_above_a_path_filter_and_follows_a_rename() {
    let dir = indexed_monorepo();
    let text = listed(dir.path(), json!({"path": "apps/api/src", "includeMetadata": false}));
    assert!(text.contains("**@acme/api in apps/api (1 file)**"), "{text}");
    assert!(!text.contains("@acme/web") && !text.contains("no owning package"), "{text}");
    manifest(dir.path(), "apps/api/package.json", json!({"name": "@acme/renamed"}));
    assert!(listed(dir.path(), json!({"includeMetadata": false})).contains("**@acme/renamed in apps/api (1 file)**"));
    let flat = listed(dir.path(), json!({"format": "flat", "includeMetadata": false}));
    assert!(flat.contains("apps/api/src/handler.ts") && !flat.contains("Files by Package"), "{flat}");
}

#[test]
fn the_tool_schema_offers_the_format() {
    // The default tool list exposes only explore; the bundled definitions hold every schema.
    let tools: Vec<serde_json::Value> = serde_json::from_str(include_str!("../src/assets/tools.json")).unwrap();
    let files = tools.iter().find(|t| t["name"] == "codegraphV_files").unwrap();
    let formats = files["inputSchema"]["properties"]["format"]["enum"].as_array().unwrap();
    assert!(formats.iter().any(|f| f == "package"), "{formats:?}");
}
