use codegraph_core::CodeGraph;
use codegraph_ui_server::{api, server, trails};
use serde_json::json;
fn fixture() -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("app.ts"),
        "export function finish(){return 42;}\nexport function start(){return finish();}\n",
    )
    .unwrap();
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}
#[test]
fn symbol_file_and_flow_views_share_the_native_graph() {
    let (_dir, cg) = fixture();
    let finish = cg.get_nodes_by_name("finish").unwrap().remove(0);
    let node = api::node(&cg, &finish.id).unwrap();
    assert_eq!(node["node"]["file"], "app.ts");
    assert!(node["incoming"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["node"]["name"] == "start"));
    let file = api::file(&cg, "app.ts").unwrap();
    assert_eq!(file["outline"]["total"], 2);
    let flow = server::get(
        &cg,
        "/api/flow",
        &vec![("symbols".into(), "start finish".into())],
        false,
    )
    .unwrap();
    assert!(!flow["flows"].as_array().unwrap().is_empty());
    for path in [
        "/api/stats",
        "/api/search",
        "/api/routes",
        "/api/entrypoints",
        "/api/map",
        "/api/deadcode",
        "/api/filecode/app.ts",
    ] {
        assert!(server::get(&cg, path, &vec![], false).is_ok(), "{path}");
    }
    cg.close();
}
#[test]
fn source_flags_drift_and_refuses_paths_outside_the_index() {
    let (dir, cg) = fixture();
    let q = vec![
        ("file".into(), "app.ts".into()),
        ("from".into(), "2".into()),
        ("to".into(), "2".into()),
    ];
    let source = api::source(&cg, &q).unwrap();
    assert_eq!(source["drift"], false);
    assert_eq!(source["lines"].as_array().unwrap().len(), 1);
    assert!(source["lines"][0].as_str().unwrap().contains("start"));
    std::fs::write(dir.path().join("app.ts"), "new source\n").unwrap();
    let changed = api::source(&cg, &q).unwrap();
    assert_eq!(changed["drift"], true);
    assert!(changed.get("lines").is_none());
    let current = api::source(
        &cg,
        &vec![
            ("file".into(), "app.ts".into()),
            ("ondrift".into(), "current".into()),
        ],
    )
    .unwrap();
    assert_eq!(current["showing"], "current");
    assert_eq!(current["lines"][0], "new source");
    assert_eq!(
        api::resolve_file(&cg, "../outside.ts").unwrap_err().status,
        403
    );
    cg.close();
}
#[test]
fn saved_trails_survive_line_shifts_and_writes_are_scoped() {
    let (dir, cg) = fixture();
    let finish = cg.get_nodes_by_name("finish").unwrap().remove(0);
    let saved = trails::save(
        &cg,
        &json!({"name":"Finish flow","hops":[{"id":finish.id}]}),
        false,
    )
    .unwrap();
    let id = saved["saved"].as_str().unwrap().to_owned();
    assert!(saved["trails"][0]["intact"].as_bool().unwrap());
    assert!(trails::save(&cg, &json!({"name":"blocked"}), true).is_err());
    cg.close();
    std::fs::write(
        dir.path().join("app.ts"),
        "\n\nexport function finish(){return 42;}\nexport function start(){return finish();}\n",
    )
    .unwrap();
    let mut cg = CodeGraph::open(dir.path().to_str().unwrap(), Default::default()).unwrap();
    cg.sync(&Default::default(), None).unwrap();
    let listed = trails::list(&cg, false).unwrap();
    assert_eq!(listed["trails"][0]["hops"][0]["status"], "moved");
    assert_eq!(listed["trails"][0]["resolved"], 1);
    assert!(trails::remove(&cg, "../outside", false).is_err());
    let removed = trails::remove(&cg, &id, false).unwrap();
    assert!(removed["trails"].as_array().unwrap().is_empty());
    cg.close();
}
/// A directory link: a junction on Windows (no privilege needed), a symlink elsewhere.
fn link_dir(target: &std::path::Path, link: &std::path::Path) {
    #[cfg(windows)]
    assert!(std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap()
        .status
        .success());
    #[cfg(not(windows))]
    std::os::unix::fs::symlink(target, link).unwrap();
}
#[test]
fn a_trail_save_never_follows_a_linked_ui_folder_out_of_the_project() {
    let (dir, cg) = fixture();
    let outside = tempfile::tempdir().unwrap();
    let ui = dir.path().join(".codegraphV").join("ui");
    let _ = std::fs::remove_dir_all(&ui);
    link_dir(outside.path(), &ui);
    let finish = cg.get_nodes_by_name("finish").unwrap().remove(0);
    let saved = trails::save(&cg, &json!({"name":"Escape","hops":[{"id":finish.id}]}), false);
    let wrote_outside = outside.path().join("trails").join("escape.json").exists();
    cg.close();
    let _ = std::fs::remove_dir(&ui);
    assert!(saved.is_err());
    assert!(!wrote_outside);
}
#[test]
fn an_unversioned_trail_reads_as_version_one_and_an_unknown_version_is_skipped() {
    let (dir, cg) = fixture();
    let finish = cg.get_nodes_by_name("finish").unwrap().remove(0);
    let trails_dir = dir.path().join(".codegraphV").join("ui").join("trails");
    std::fs::create_dir_all(&trails_dir).unwrap();
    let hop = json!({"dir":"start","id":finish.id,"name":"finish","qualifiedName":finish.qualified_name,"kind":"function","file":"app.ts","line":1});
    std::fs::write(trails_dir.join("old.json"), json!({"id":"old","name":"Old","hops":[hop.clone()]}).to_string()).unwrap();
    std::fs::write(trails_dir.join("next.json"), json!({"version":2,"id":"next","name":"Next","hops":[hop]}).to_string()).unwrap();
    let listed = trails::list(&cg, false).unwrap();
    cg.close();
    let ids: Vec<&str> = listed["trails"].as_array().unwrap().iter().filter_map(|t| t["id"].as_str()).collect();
    assert_eq!(ids, vec!["old"]);
    assert_eq!(listed["skipped"], 1);
}
