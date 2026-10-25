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
fn snapshot(root: &std::path::Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    let mut files = std::collections::BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.insert(path.strip_prefix(root).unwrap().to_path_buf(), fs::read(&path).unwrap());
            }
        }
    }
    files
}
/// Uninstall gives back what install found: a CRLF instructions file keeps its line breaks,
/// and files install created (settings.json, an instructions file) do not linger emptied.
#[test]
fn claude_uninstall_restores_user_files_byte_for_byte() {
    let claude_json = "{\n  \"theme\": \"dark\",\n  \"mcpServers\": {\n    \"other\": {\n      \"command\": \"x\"\n    }\n  }\n}\n";
    for (config, notes) in [(Some(claude_json), Some("# My notes\r\n\r\nKeep this.\r\n")), (None, None)] {
        let dir = tempfile::tempdir().unwrap();
        let env = env(&dir);
        if let Some(text) = config {
            fs::write(env.home.join(".claude.json"), text).unwrap();
        }
        if let Some(text) = notes {
            fs::create_dir_all(env.home.join(".claude")).unwrap();
            fs::write(env.home.join(".claude").join("CLAUDE.md"), text).unwrap();
        }
        let before = snapshot(&env.home);
        env.apply("claude", Location::Global, InstallOptions::default(), false).unwrap();
        assert_ne!(snapshot(&env.home), before);
        env.apply("claude", Location::Global, InstallOptions::default(), true).unwrap();
        let after = snapshot(&env.home);
        for (path, bytes) in &after {
            assert_eq!(before.get(path), Some(bytes), "{} differs:\n{}", path.display(), String::from_utf8_lossy(bytes));
        }
        assert_eq!(after.len(), before.len(), "{:?}", after.keys().collect::<Vec<_>>());
    }
}
/// DEV-65: the Claude front-load hook (TS writePromptHookEntry / removePromptHookEntry).
#[test]
fn claude_prompt_hook_is_added_migrated_idempotent_and_removed_without_touching_siblings() {
    let read = |p: &std::path::Path| -> serde_json::Value { serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap() };
    let commands = |v: &serde_json::Value| -> Vec<String> {
        v["hooks"]["UserPromptSubmit"].as_array().into_iter().flatten()
            .flat_map(|g| g["hooks"].as_array().cloned().unwrap_or_default())
            .map(|h| h["command"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    let hook = |on: Option<bool>| InstallOptions { prompt_hook: on, ..InstallOptions::default() };
    let dir = tempfile::tempdir().unwrap();
    let first_env = env(&dir);
    let settings = first_env.home.join(".claude").join("settings.json");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    // A TS install's Windows spelling plus the user's own hooks.
    fs::write(&settings, r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"my-own"}]},{"hooks":[{"type":"command","command":"codegraph.cmd prompt-hook"}]}],"Stop":[{"hooks":[{"type":"command","command":"stop-hook"}]}]}}"#).unwrap();
    first_env.apply("claude", Location::Global, hook(Some(true)), false).unwrap();
    let v = read(&settings);
    assert_eq!(commands(&v), vec!["my-own", "codegraph prompt-hook"], "migrated in place, no duplicate");
    assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], "stop-hook");
    let first = fs::read_to_string(&settings).unwrap();
    let again = first_env.apply("claude", Location::Global, hook(Some(true)), false).unwrap();
    assert!(again.files.iter().all(|f| f.action == "unchanged"), "{again:?}");
    assert_eq!(fs::read_to_string(&settings).unwrap(), first);
    // None (a refresh) leaves the user's choice alone.
    first_env.apply("claude", Location::Global, hook(None), false).unwrap();
    assert_eq!(fs::read_to_string(&settings).unwrap(), first);
    // Opting out strips only ours.
    first_env.apply("claude", Location::Global, hook(Some(false)), false).unwrap();
    let v = read(&settings);
    assert_eq!(commands(&v), vec!["my-own"]);
    assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], "stop-hook");
    // A fresh settings.json gets one entry; uninstall removes it and the emptied keys.
    let dir2 = tempfile::tempdir().unwrap();
    let env2 = env(&dir2);
    env2.apply("claude", Location::Global, hook(Some(true)), false).unwrap();
    let settings2 = env2.home.join(".claude").join("settings.json");
    assert_eq!(commands(&read(&settings2)), vec!["codegraph prompt-hook"]);
    env2.apply("claude", Location::Global, InstallOptions::default(), true).unwrap();
    let left = fs::read_to_string(&settings2).unwrap_or_default();
    assert!(!left.contains("prompt-hook"), "{left}");
    assert!(!left.contains("UserPromptSubmit"), "{left}");
}

/// BUG-75: the npm build wrote `codegraph.cmd prompt-hook` on Windows; the Rust build has no
/// `.cmd` shim, so after the switch that hook fails on every prompt. A refresh keeps the user's
/// choice (it never turns the hook on) but rewrites an installer-written spelling.
#[test]
fn claude_refresh_rewrites_the_npm_hook_spelling_and_never_adds_one() {
    let commands = |p: &std::path::Path| -> Vec<String> {
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap();
        v["hooks"]["UserPromptSubmit"].as_array().into_iter().flatten()
            .flat_map(|g| g["hooks"].as_array().cloned().unwrap_or_default())
            .map(|h| h["command"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    let refresh = InstallOptions { prompt_hook: None, ..InstallOptions::default() };
    let dir = tempfile::tempdir().unwrap();
    let first_env = env(&dir);
    let settings = first_env.home.join(".claude").join("settings.json");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    fs::write(&settings, r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"npx @colbymchenry/codegraph prompt-hook"}]},{"hooks":[{"type":"command","command":"codegraph.cmd prompt-hook"}]}],"Stop":[{"hooks":[{"type":"command","command":"stop-hook"}]}]}}"#).unwrap();
    first_env.apply("claude", Location::Global, refresh, false).unwrap();
    assert_eq!(commands(&settings), vec!["npx @colbymchenry/codegraph prompt-hook", "codegraph prompt-hook"]);
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], "stop-hook");
    let first = fs::read_to_string(&settings).unwrap();
    first_env.apply("claude", Location::Global, refresh, false).unwrap();
    assert_eq!(fs::read_to_string(&settings).unwrap(), first, "idempotent");

    // No hook before the refresh: none after it.
    let dir2 = tempfile::tempdir().unwrap();
    let env2 = env(&dir2);
    let settings2 = env2.home.join(".claude").join("settings.json");
    fs::create_dir_all(settings2.parent().unwrap()).unwrap();
    fs::write(&settings2, r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"stop-hook"}]}]}}"#).unwrap();
    env2.apply("claude", Location::Global, refresh, false).unwrap();
    assert!(commands(&settings2).is_empty());
}
