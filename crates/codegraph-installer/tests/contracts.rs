use codegraph_installer::*;
use std::{collections::HashMap, fs};

#[test]
fn audit_refresh_preserves_permission_opt_out_and_narrow_permissions() {
    for allow in [serde_json::json!([]), serde_json::json!(["mcp__codegraphV__codegraphV_explore", "Read(*)"])] {
        let dir = tempfile::tempdir().unwrap();
        let env = env(&dir);
        let path = env.home.join(".claude/settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = serde_json::json!({"permissions": {"allow": allow}}).to_string();
        fs::write(&path, &original).unwrap();
        env.apply("claude", Location::Global, InstallOptions { preserve_permissions: true, ..InstallOptions::default() }, false).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        env.apply("claude", Location::Global, InstallOptions::default(), false).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("mcp__codegraphV__*"));
        env.apply("claude", Location::Global, InstallOptions { auto_allow: false, ..InstallOptions::default() }, false).unwrap();
        assert!(!fs::read_to_string(&path).unwrap_or_default().contains("mcp__codegraphV__"));
    }
}

#[test]
fn audit_hook_mentions_are_not_owned_commands() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(&dir);
    let path = env.home.join(".claude/settings.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let commands = ["echo codegraphV prompt-hook", "# codegraphV prompt-hook", "wrapper codegraphV prompt-hook", "codegraphV prompt-hook-extra", "codegraphV prompt-hook && echo done"];
    let hooks: Vec<_> = commands.iter().map(|c| serde_json::json!({"type":"command", "command": c})).collect();
    let original = serde_json::json!({"hooks":{"Stop":[{"matcher":"*", "hooks":hooks}]}});
    fs::write(&path, original.to_string()).unwrap();
    env.apply("claude", Location::Global, InstallOptions::default(), true).unwrap();
    let after: serde_json::Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(after, original);
}

#[test]
fn audit_claude_does_not_introduce_an_instruction_shadow() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(&dir);
    let path = env.project.join("AGENTS.md");
    let original = "# Existing project guidance\n";
    fs::write(&path, original).unwrap();
    env.apply("claude", Location::Local, InstallOptions::default(), false).unwrap();
    assert!(!env.project.join(".claude/CLAUDE.md").exists());
    assert!(fs::read_to_string(&path).unwrap().starts_with(original));
    env.apply("claude", Location::Local, InstallOptions::default(), true).unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), original);
}

#[test]
fn audit_malformed_hooks_fail_before_any_write() {
    for malformed in [r#"{"hooks":[]}"#, r#"{"hooks":{"UserPromptSubmit":{}}}"#, r#"{"hooks":{"Stop":[{"hooks":false}]}}"#, r#"{"permissions":{"allow":"custom"}}"#] {
        let dir = tempfile::tempdir().unwrap();
        let env = env(&dir);
        let path = env.project.join(".claude/settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, malformed).unwrap();
        assert!(env.apply("claude", Location::Local, InstallOptions { prompt_hook: Some(true), ..InstallOptions::default() }, false).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), malformed);
        assert!(!env.config_path("claude", Location::Local).unwrap().exists());
        assert!(!env.instructions_path("claude", Location::Local).unwrap().exists());
    }
}

#[test]
fn audit_refresh_launch_wiring_preserves_server_preferences() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(&dir);
    let codex = env.config_path("codex", Location::Local).unwrap();
    fs::create_dir_all(codex.parent().unwrap()).unwrap();
    fs::write(&codex, "[mcp_servers.codegraphV]\ncommand = 'old'\nargs = []\nenabled = false\nstartup_timeout_sec = 19\n[mcp_servers.codegraphV.env]\nUSER_SETTING = 'keep'\n").unwrap();
    env.apply("codex", Location::Local, InstallOptions::default(), false).unwrap();
    let text = fs::read_to_string(codex).unwrap();
    assert!(text.contains("enabled = false") && text.contains("startup_timeout_sec = 19") && text.contains("USER_SETTING = 'keep'"));
    let cursor = env.config_path("cursor", Location::Local).unwrap();
    fs::create_dir_all(cursor.parent().unwrap()).unwrap();
    fs::write(&cursor, r#"{"mcpServers":{"codegraphV":{"command":"old","args":[],"env":{"USER_SETTING":"keep"},"disabled":true,"timeout":19}}}"#).unwrap();
    env.apply("cursor", Location::Local, InstallOptions::default(), false).unwrap();
    let data: serde_json::Value = serde_json::from_str(&fs::read_to_string(cursor).unwrap()).unwrap();
    assert_eq!(data["mcpServers"]["codegraphV"]["env"]["USER_SETTING"], "keep");
    assert_eq!(data["mcpServers"]["codegraphV"]["disabled"], true);
    assert_eq!(data["mcpServers"]["codegraphV"]["timeout"], 19);
    assert_eq!(data["mcpServers"]["codegraphV"]["command"], env.executable);
}

#[test]
fn audit_instruction_round_trips_preserve_bytes_existence_and_later_edits() {
    for id in ["claude", "codex", "opencode", "gemini"] {
        for original in ["", " \t\r\n  ", "no final newline", "\u{feff}日本語\r\n", "# Rules\n\n", "trailing spaces  \n"] {
            let dir = tempfile::tempdir().unwrap();
            let env = env(&dir);
            let path = env.instructions_path(id, Location::Local).unwrap();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, original).unwrap();
            env.apply(id, Location::Local, InstallOptions::default(), false).unwrap();
            let installed = fs::read_to_string(&path).unwrap();
            assert!(installed.starts_with(original));
            env.apply(id, Location::Local, InstallOptions::default(), false).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), installed);
            env.apply(id, Location::Local, InstallOptions::default(), true).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), original, "{id} {original:?}");
            env.apply(id, Location::Local, InstallOptions::default(), false).unwrap();
            let installed = fs::read_to_string(&path).unwrap();
            fs::write(&path, format!("new user prefix\n{installed}new user suffix\n")).unwrap();
            env.apply(id, Location::Local, InstallOptions::default(), true).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), format!("new user prefix\n{original}new user suffix\n"));
        }
    }
}

#[test]
fn audit_ambiguous_markers_refuse_the_whole_target_before_writing() {
    for text in [
        "<!-- CODEGRAPHV_START -->\nuser text\n",
        "<!-- CODEGRAPHV_END -->\n<!-- CODEGRAPHV_START -->\n",
        "<!-- CODEGRAPHV_START -->\n<!-- CODEGRAPHV_START -->\n<!-- CODEGRAPHV_END -->\n",
        "<!-- CODEGRAPHV_START -->\nx\n<!-- CODEGRAPHV_END -->\n<!-- CODEGRAPHV_START -->\ny\n<!-- CODEGRAPHV_END -->\n",
        "```markdown\nunclosed example\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let env = env(&dir);
        let path = env.project.join("AGENTS.md");
        fs::write(&path, text).unwrap();
        // Installation must inspect the whole document before writing.
        assert!(env.apply("codex", Location::Local, InstallOptions::default(), false).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        assert!(!env.config_path("codex", Location::Local).unwrap().exists());
        // Removal only validates a file that names an owned marker; an unrelated malformed
        // example is left untouched rather than blocking an uninstall.
        let mentions = text.contains("CODEGRAPHV_START") || text.contains("CODEGRAPHV_END");
        let result = env.apply("codex", Location::Local, InstallOptions::default(), true);
        assert_eq!(result.is_err(), mentions, "{text:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        assert!(!env.config_path("codex", Location::Local).unwrap().exists());
    }
}

#[test]
fn audit_exec_hook_paths_are_literal_and_extra_arguments_are_not_owned() {
    for executable in [r"C:\space $cash`tick's\日本\codegraphV.exe", "/space $cash`tick's/日本/codegraphV"] {
        let dir = tempfile::tempdir().unwrap();
        let mut env = env(&dir);
        env.executable = executable.into();
        env.apply("claude", Location::Local, InstallOptions { prompt_hook: Some(true), ..InstallOptions::default() }, false).unwrap();
        let path = env.project.join(".claude/settings.json");
        let mut data: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let hook = &mut data["hooks"]["UserPromptSubmit"][0]["hooks"][0];
        assert_eq!(hook["command"], executable);
        assert_eq!(hook["args"], serde_json::json!(["prompt-hook"]));
        hook["args"] = serde_json::json!(["prompt-hook", "user-extension"]);
        fs::write(&path, data.to_string()).unwrap();
        env.apply("claude", Location::Local, InstallOptions::default(), true).unwrap();
        let after: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after["hooks"], data["hooks"]);
    }
}

#[test]
fn audit_shared_guidance_remains_until_last_consumer_in_both_orders() {
    for order in [["codex", "opencode"], ["opencode", "codex"]] {
        let dir = tempfile::tempdir().unwrap();
        let env = env(&dir);
        let path = env.project.join("AGENTS.md");
        let original = "# User rules\n\nKeep me.\n";
        fs::write(&path, original).unwrap();
        for id in order { env.apply(id, Location::Local, InstallOptions::default(), false).unwrap(); }
        env.apply(order[0], Location::Local, InstallOptions::default(), true).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains(CODEGRAPHV.start));
        env.apply(order[0], Location::Local, InstallOptions::default(), true).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains(CODEGRAPHV.start));
        env.apply(order[1], Location::Local, InstallOptions::default(), true).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }
}
#[test]
fn audit_targets_sharing_an_instructions_file_plan_it_once() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(&dir);
    let requests = vec![
        ("claude".to_owned(), InstallOptions::default()),
        ("codex".to_owned(), InstallOptions::default()),
    ];
    let results = env.apply_many(&requests, Location::Local, false).unwrap();
    let path = env.project.join("AGENTS.md");
    let text = fs::read_to_string(&path).unwrap();
    assert_eq!(text.matches(CODEGRAPHV.start).count(), 1, "{text}");
    let agents: Vec<_> = results.iter().flat_map(|(_, r)| &r.files).filter(|f| f.path.ends_with("AGENTS.md")).collect();
    assert_eq!(agents.len(), 2, "both targets report the shared file");
    assert_eq!(agents.iter().filter(|f| f.action == "created").count(), 1, "it is written once");
    let results = env.remove_many_as(&CODEGRAPHV, &["claude".into(), "codex".into()], Location::Local, false).unwrap();
    assert!(results.iter().flat_map(|(_, r)| &r.files).any(|f| f.path.ends_with("AGENTS.md") && f.action == "removed"));
    assert!(!path.exists());
}

fn env(dir: &tempfile::TempDir) -> Environment {
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&project).unwrap();
    Environment {
        home,
        project,
        executable: "C:\\codegraphV native\\codegraphV.exe".into(),
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
            let before=match *id{"dsh"=>"# keep comment\n- insert:\n    - id: sibling\n      name: sibling\n", "codex"|"grok"=>"# keep comment\nmodel = \"example\"\n[mcp_servers.sibling]\ncommand = \"sibling\"\n", "hermes"=>"# keep comment\nmodel: example\nmcp_servers:\n  sibling:\n    command: sibling\n",_=>"{\n  // keep comment\n  \"sibling\": {\"value\": true},\n}\n"};
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
    assert_eq!(text.matches("<!-- CODEGRAPHV_START -->").count(), 1);
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
        let mut after = snapshot(&env.home);
        // The empty OS-lock file is permanent so concurrent installers always lock the
        // same filesystem object. It contains no configuration or instruction content.
        assert_eq!(after.remove(std::path::Path::new(".codegraphV/install.lock")), Some(Vec::new()));
        for (path, bytes) in &after {
            assert_eq!(before.get(path), Some(bytes), "{} differs:\n{}", path.display(), String::from_utf8_lossy(bytes));
        }
        assert_eq!(after.len(), before.len(), "{:?}", after.keys().collect::<Vec<_>>());
    }
}
fn hook_commands(p: &std::path::Path) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap();
    v["hooks"]["UserPromptSubmit"].as_array().into_iter().flatten()
        .flat_map(|g| g["hooks"].as_array().cloned().unwrap_or_default())
        .map(|h| h["command"].as_str().unwrap_or_default().to_string())
        .collect()
}
fn stop_hook(p: &std::path::Path) -> serde_json::Value {
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap();
    v["hooks"]["Stop"][0]["hooks"][0]["command"].clone()
}
/// The original codegraph's hooks (both spellings it wrote) next to the user's own.
const ORIGINAL_HOOKS: &str = r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"my-own"}]},{"hooks":[{"type":"command","command":"codegraph.cmd prompt-hook"}]},{"hooks":[{"type":"command","command":"npx @colbymchenry/codegraph prompt-hook"}]}],"Stop":[{"hooks":[{"type":"command","command":"stop-hook"}]}]}}"#;

/// The hook `install` writes for env()'s executable: its absolute path, so it runs whatever PATH
/// the agent inherited (BUG-95).
const OURS: &str = r"C:\codegraphV native\codegraphV.exe";

/// DEV-65: the Claude front-load hook (TS writePromptHookEntry / removePromptHookEntry). DEV-83:
/// codegraphV's hook is its own; the original codegraph's hooks are another product's and stay.
#[test]
fn claude_prompt_hook_is_added_idempotent_and_removed_without_touching_siblings() {
    let hook = |on: Option<bool>| InstallOptions { prompt_hook: on, ..InstallOptions::default() };
    let dir = tempfile::tempdir().unwrap();
    let first_env = env(&dir);
    let settings = first_env.home.join(".claude").join("settings.json");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    fs::write(&settings, ORIGINAL_HOOKS).unwrap();
    let theirs = ["my-own", "codegraph.cmd prompt-hook", "npx @colbymchenry/codegraph prompt-hook"];
    first_env.apply("claude", Location::Global, hook(Some(true)), false).unwrap();
    let mut with_ours: Vec<&str> = theirs.to_vec();
    with_ours.push(OURS);
    assert_eq!(hook_commands(&settings), with_ours, "added once, nothing else touched");
    assert_eq!(stop_hook(&settings), "stop-hook");
    let first = fs::read_to_string(&settings).unwrap();
    let again = first_env.apply("claude", Location::Global, hook(Some(true)), false).unwrap();
    assert!(again.files.iter().all(|f| f.action == "unchanged"), "{again:?}");
    assert_eq!(fs::read_to_string(&settings).unwrap(), first);
    // None (a refresh) leaves the user's choice alone.
    first_env.apply("claude", Location::Global, hook(None), false).unwrap();
    assert_eq!(fs::read_to_string(&settings).unwrap(), first);
    // Opting out strips only ours.
    first_env.apply("claude", Location::Global, hook(Some(false)), false).unwrap();
    assert_eq!(hook_commands(&settings), theirs);
    assert_eq!(stop_hook(&settings), "stop-hook");
    // A fresh settings.json gets one entry; uninstall removes it and the emptied keys.
    let dir2 = tempfile::tempdir().unwrap();
    let env2 = env(&dir2);
    env2.apply("claude", Location::Global, hook(Some(true)), false).unwrap();
    let settings2 = env2.home.join(".claude").join("settings.json");
    assert_eq!(hook_commands(&settings2), vec![OURS]);
    env2.apply("claude", Location::Global, InstallOptions::default(), true).unwrap();
    let left = fs::read_to_string(&settings2).unwrap_or_default();
    assert!(!left.contains("prompt-hook"), "{left}");
    assert!(!left.contains("UserPromptSubmit"), "{left}");
}

/// A refresh (`install --refresh`, run by `upgrade`) never adds a hook and never rewrites the
/// original codegraph's: replacing those is `migrate`'s job, on request.
#[test]
fn claude_refresh_leaves_the_originals_hooks_and_never_adds_one() {
    let refresh = InstallOptions { prompt_hook: None, ..InstallOptions::default() };
    let dir = tempfile::tempdir().unwrap();
    let first_env = env(&dir);
    let settings = first_env.home.join(".claude").join("settings.json");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    fs::write(&settings, ORIGINAL_HOOKS).unwrap();
    first_env.apply("claude", Location::Global, refresh, false).unwrap();
    assert_eq!(hook_commands(&settings), vec!["my-own", "codegraph.cmd prompt-hook", "npx @colbymchenry/codegraph prompt-hook"]);
    assert_eq!(stop_hook(&settings), "stop-hook");

    // No hook before the refresh: none after it.
    let dir2 = tempfile::tempdir().unwrap();
    let env2 = env(&dir2);
    let settings2 = env2.home.join(".claude").join("settings.json");
    fs::create_dir_all(settings2.parent().unwrap()).unwrap();
    fs::write(&settings2, r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"stop-hook"}]}]}}"#).unwrap();
    env2.apply("claude", Location::Global, refresh, false).unwrap();
    assert!(hook_commands(&settings2).is_empty());
}

/// BUG-95: an older spelling of codegraphV's hook (bare, or another path) is replaced in place by
/// an install and by a refresh; a refresh still adds none.
#[test]
fn claude_hook_spelling_is_updated_in_place() {
    for old in ["codegraphV prompt-hook", r#""D:/old place/codegraphV.exe" prompt-hook"#, "codegraphV.exe prompt-hook"] {
        for on in [Some(true), None] {
            let dir = tempfile::tempdir().unwrap();
            let env = env(&dir);
            let settings = env.home.join(".claude").join("settings.json");
            fs::create_dir_all(settings.parent().unwrap()).unwrap();
            let text = format!(
                r#"{{"hooks":{{"UserPromptSubmit":[{{"hooks":[{{"type":"command","command":{}}}]}},{{"hooks":[{{"type":"command","command":"my-own"}}]}}]}}}}"#,
                serde_json::to_string(old).unwrap()
            );
            fs::write(&settings, text).unwrap();
            env.apply("claude", Location::Global, InstallOptions { prompt_hook: on, ..InstallOptions::default() }, false).unwrap();
            assert_eq!(hook_commands(&settings), vec![OURS, "my-own"], "{old} {on:?}");
        }
    }
}

/// BUG-95: every spelling of the original's hook, the absolute-path one included, is the
/// original's: codegraphV's install and uninstall leave them, remove_as(ORIGINAL) takes them all
/// and nothing else.
#[test]
fn the_originals_hooks_in_every_spelling() {
    let theirs = [
        "codegraph prompt-hook",
        "codegraph.cmd prompt-hook",
        "npx @colbymchenry/codegraph prompt-hook",
        r#""C:/Users/me/AppData/Local/codegraph/bin/codegraph.exe" prompt-hook"#,
        "/home/me/.local/bin/codegraph prompt-hook",
    ];
    let groups: Vec<serde_json::Value> = ["my-own", "codegraph-helper prompt-hook"]
        .iter()
        .chain(theirs.iter())
        .map(|c| serde_json::json!({"hooks": [{"type": "command", "command": c}]}))
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let env = env(&dir);
    let settings = env.home.join(".claude").join("settings.json");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    fs::write(&settings, serde_json::json!({"hooks": {"UserPromptSubmit": groups}}).to_string()).unwrap();
    let mut all: Vec<&str> = vec!["my-own", "codegraph-helper prompt-hook"];
    all.extend(theirs);
    env.apply("claude", Location::Global, InstallOptions { prompt_hook: Some(true), ..InstallOptions::default() }, false).unwrap();
    let mut with_ours = all.clone();
    with_ours.push(OURS);
    assert_eq!(hook_commands(&settings), with_ours);
    env.apply("claude", Location::Global, InstallOptions::default(), true).unwrap();
    assert_eq!(hook_commands(&settings), all);
    assert_eq!(env.claude_settings_as(&original::ORIGINAL, Location::Global), (false, true));
    env.remove_as(&original::ORIGINAL, "claude", Location::Global, false).unwrap();
    assert_eq!(hook_commands(&settings), vec!["my-own", "codegraph-helper prompt-hook"]);
}
