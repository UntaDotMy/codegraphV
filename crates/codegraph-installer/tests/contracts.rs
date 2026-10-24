use codegraph_installer::*;
use std::{collections::HashMap, fs};
fn env(dir: &tempfile::TempDir) -> Environment {
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&project).unwrap();
    Environment {
        home,
        project,
        executable: "C:\\codegraph native\\codegraph.exe".into(),
        vars: HashMap::new(),
    }
}
#[test]
fn every_target_installs_idempotently_and_uninstalls_without_removing_siblings() {
    for id in TARGETS {
        for loc in [Location::Global, Location::Local] {
            let dir = tempfile::tempdir().unwrap();
            let env = env(&dir);
            if !env.supports(id, loc) {
                continue;
            }
            let config = env.config_path(id, loc).unwrap();
            fs::create_dir_all(config.parent().unwrap()).unwrap();
            let before=match *id{"codex"=>"# keep comment\nmodel = \"example\"\n[mcp_servers.sibling]\ncommand = \"sibling\"\n", "hermes"=>"# keep comment\nmodel: example\nmcp_servers:\n  sibling:\n    command: sibling\n",_=>"{\n  // keep comment\n  \"sibling\": {\"value\": true},\n}\n"};
            fs::write(&config, before).unwrap();
            env.apply(id, loc, InstallOptions::default(), false)
                .unwrap();
            assert!(env.configured(id, loc), "{id}");
            let first = fs::read_to_string(&config).unwrap();
            assert!(first.contains("keep comment"), "{id}");
            assert!(first.contains("sibling"), "{id}");
            let second = env
                .apply(id, loc, InstallOptions::default(), false)
                .unwrap();
            assert!(
                second
                    .files
                    .iter()
                    .all(|f| f.action == "unchanged" || f.action == "not-found"),
                "{id}: {second:?}"
            );
            assert_eq!(fs::read_to_string(&config).unwrap(), first, "{id}");
            env.apply(id, loc, InstallOptions::default(), true).unwrap();
            assert!(!env.configured(id, loc), "{id}");
            let after = fs::read_to_string(config).unwrap();
            assert!(after.contains("keep comment"), "{id}");
            assert!(after.contains("sibling"), "{id}");
        }
    }
}
#[test]
fn profile_overrides_and_cursor_path_are_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let mut env = env(&dir);
    env.vars.insert(
        "CODEX_HOME".into(),
        dir.path()
            .join("codex-profile")
            .to_string_lossy()
            .into_owned(),
    );
    assert!(env
        .config_path("codex", Location::Global)
        .unwrap()
        .ends_with("codex-profile/config.toml"));
    assert!(env
        .config_path("codex", Location::Local)
        .unwrap()
        .starts_with(&env.project));
    assert_eq!(
        env.entry("cursor", Location::Global)["args"][3],
        "${workspaceFolder}"
    );
    assert_eq!(
        env.entry("cursor", Location::Local)["args"][3],
        env.project.to_string_lossy().as_ref()
    );
}
#[test]
fn malformed_config_is_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(&dir);
    let path = env.config_path("cursor", Location::Global).unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "{ malformed").unwrap();
    assert!(env
        .apply("cursor", Location::Global, InstallOptions::default(), false)
        .is_err());
    assert_eq!(fs::read_to_string(path).unwrap(), "{ malformed");
}
#[test]
fn shared_instructions_preserve_user_text() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(&dir);
    let file = env.project.join("AGENTS.md");
    fs::write(&file, "# Personal instructions\n\nKeep this.\n").unwrap();
    env.apply("codex", Location::Local, InstallOptions::default(), false)
        .unwrap();
    env.apply(
        "opencode",
        Location::Local,
        InstallOptions::default(),
        false,
    )
    .unwrap();
    let text = fs::read_to_string(&file).unwrap();
    assert_eq!(text.matches("<!-- CODEGRAPH_START -->").count(), 1);
    env.apply("codex", Location::Local, InstallOptions::default(), true)
        .unwrap();
    assert!(fs::read_to_string(file).unwrap().contains("Keep this."));
}
