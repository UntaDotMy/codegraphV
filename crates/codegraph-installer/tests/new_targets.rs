//! The nine agents upstream 61e16218 added: Bob, Droid, Windsurf, Qwen Code, DeepSeek Harness
//! (dsh), Grok Build, Kimi Code, Qoder CLI and Pi. Each writes where that agent reads.
use codegraph_installer::{Environment, InstallOptions, Location, TARGETS};
use serde_json::Value;
use std::{collections::HashMap, fs, path::Path};

fn env(dir: &tempfile::TempDir) -> Environment {
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&project).unwrap();
    Environment { home, project, executable: "/opt/cg/codegraphV".into(), vars: HashMap::new() }
}

fn json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn install(env: &Environment, id: &str, loc: Location) -> codegraph_installer::WriteResult {
    env.apply(id, loc, InstallOptions::default(), false).unwrap()
}

fn uninstall(env: &Environment, id: &str, loc: Location) -> codegraph_installer::WriteResult {
    env.apply(id, loc, InstallOptions::default(), true).unwrap()
}

#[test]
fn all_nine_are_targets() {
    for id in ["bob", "droid", "windsurf", "qwen", "dsh", "grok", "kimi", "qoder", "pi"] {
        assert!(TARGETS.contains(&id), "{id}");
    }
}

#[test]
fn json_targets_write_upstreams_paths_and_entry_shapes() {
    let dir = tempfile::tempdir().unwrap();
    let e = env(&dir);
    let h = &e.home;
    let p = &e.project;
    // (id, location, config file, entry has "type", instructions file)
    let cases: Vec<(&str, Location, std::path::PathBuf, bool, Option<std::path::PathBuf>)> = vec![
        ("bob", Location::Global, h.join(".bob/settings/mcp.json"), false, None),
        ("bob", Location::Local, p.join(".bob/mcp.json"), false, None),
        ("droid", Location::Global, h.join(".factory/mcp.json"), true, Some(h.join(".factory/AGENTS.md"))),
        ("droid", Location::Local, p.join(".factory/mcp.json"), true, Some(p.join("AGENTS.md"))),
        ("windsurf", Location::Global, h.join(".codeium/windsurf/mcp_config.json"), false, None),
        ("qwen", Location::Global, h.join(".qwen/settings.json"), false, Some(h.join(".qwen/QWEN.md"))),
        ("qwen", Location::Local, p.join(".qwen/settings.json"), false, Some(p.join("QWEN.md"))),
        ("kimi", Location::Global, h.join(".kimi-code/mcp.json"), false, Some(h.join(".kimi-code/AGENTS.md"))),
        ("kimi", Location::Local, p.join(".kimi-code/mcp.json"), false, Some(p.join("AGENTS.md"))),
        ("qoder", Location::Global, h.join(".qoder/settings.json"), false, None),
        ("qoder", Location::Local, p.join(".qoder/settings.local.json"), false, None),
        ("pi", Location::Global, h.join(".pi/agent/mcp.json"), false, Some(h.join(".pi/agent/AGENTS.md"))),
        ("pi", Location::Local, p.join(".pi/mcp.json"), false, Some(p.join("AGENTS.md"))),
    ];
    for (id, loc, file, typed, instructions) in cases {
        assert_eq!(e.config_path(id, loc).unwrap(), file, "{id} {loc:?}");
        install(&e, id, loc);
        let entry = &json(&file)["mcpServers"]["codegraphV"];
        assert_eq!(entry["command"], "/opt/cg/codegraphV", "{id}");
        assert_eq!(entry["args"], serde_json::json!(["serve", "--mcp"]), "{id}");
        assert_eq!(entry.get("type").is_some(), typed, "{id}: {entry}");
        if id == "pi" {
            assert_eq!(entry["exposure"], "direct");
        }
        assert_eq!(e.instructions_path(id, loc), instructions, "{id} {loc:?}");
        if let Some(path) = e.instructions_path(id, loc) {
            assert!(fs::read_to_string(path).unwrap().contains("<!-- CODEGRAPHV_START -->"), "{id}");
        }
        assert!(e.configured(id, loc), "{id}");
        uninstall(&e, id, loc);
        assert!(!e.configured(id, loc), "{id}");
    }
}

#[test]
fn bob_keeps_using_an_earlier_mcp_settings_file() {
    let dir = tempfile::tempdir().unwrap();
    let e = env(&dir);
    let earlier = e.home.join(".bob/settings/mcp_settings.json");
    fs::create_dir_all(earlier.parent().unwrap()).unwrap();
    fs::write(&earlier, "{}").unwrap();
    assert_eq!(e.config_path("bob", Location::Global).unwrap(), earlier);
}

#[test]
fn home_variables_are_honored_with_a_leading_tilde() {
    let dir = tempfile::tempdir().unwrap();
    let mut e = env(&dir);
    for (var, value) in [
        ("QWEN_HOME", "~/q"),
        ("GROK_HOME", "~/g"),
        ("KIMI_CODE_HOME", "~/k"),
        ("QODER_CONFIG_DIR", "~/o"),
        ("PI_CODING_AGENT_DIR", "~/pi"),
        ("DSH_HOME", "~/d"),
    ] {
        e.vars.insert(var.into(), value.into());
    }
    let h = e.home.clone();
    assert_eq!(e.config_path("qwen", Location::Global).unwrap(), h.join("q").join("settings.json"));
    assert_eq!(e.config_path("grok", Location::Global).unwrap(), h.join("g").join("config.toml"));
    assert_eq!(e.config_path("kimi", Location::Global).unwrap(), h.join("k").join("mcp.json"));
    assert_eq!(e.config_path("qoder", Location::Global).unwrap(), h.join("o").join("settings.json"));
    assert_eq!(e.config_path("pi", Location::Global).unwrap(), h.join("pi").join("mcp.json"));
    assert_eq!(e.config_path("dsh", Location::Global).unwrap(), h.join("d").join("cordis.patch.yml"));
    // A local install ignores the global override.
    assert_eq!(e.config_path("qwen", Location::Local).unwrap(), e.project.join(".qwen/settings.json"));
}

#[test]
fn windsurf_and_dsh_are_global_only() {
    let dir = tempfile::tempdir().unwrap();
    let e = env(&dir);
    for id in ["windsurf", "dsh"] {
        assert!(!e.supports(id, Location::Local), "{id}");
        let result = install(&e, id, Location::Local);
        assert!(result.files.is_empty(), "{id}");
        assert!(result.notes.iter().any(|n| n.contains("global")), "{id}: {:?}", result.notes);
    }
}

#[test]
fn grok_writes_a_toml_table_and_a_rules_file() {
    let dir = tempfile::tempdir().unwrap();
    let e = env(&dir);
    let file = e.home.join(".grok/config.toml");
    assert_eq!(e.config_path("grok", Location::Global).unwrap(), file);
    assert_eq!(e.instructions_path("grok", Location::Local), Some(e.project.join(".grok/rules/codegraphV.md")));
    install(&e, "grok", Location::Global);
    let doc: toml_edit::DocumentMut = fs::read_to_string(&file).unwrap().parse().unwrap();
    assert_eq!(doc["mcp_servers"]["codegraphV"]["command"].as_str(), Some("/opt/cg/codegraphV"));
    assert!(e.home.join(".grok/rules/codegraphV.md").exists());
    let notes = install(&e, "grok", Location::Local).notes;
    assert!(notes.iter().any(|n| n.contains("trusted folder")), "{notes:?}");
}

#[test]
fn qwen_allows_its_server_and_removes_only_that_rule() {
    let dir = tempfile::tempdir().unwrap();
    let e = env(&dir);
    let file = e.home.join(".qwen/settings.json");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, "{\"permissions\": {\"allow\": [\"run_shell_command\"]}}\n").unwrap();
    install(&e, "qwen", Location::Global);
    assert_eq!(json(&file)["permissions"]["allow"], serde_json::json!(["run_shell_command", "mcp__codegraphV"]));
    install(&e, "qwen", Location::Global);
    assert_eq!(json(&file)["permissions"]["allow"], serde_json::json!(["run_shell_command", "mcp__codegraphV"]));
    uninstall(&e, "qwen", Location::Global);
    assert_eq!(json(&file)["permissions"]["allow"], serde_json::json!(["run_shell_command"]));

    // Opted out of auto-allow: no rule. A sole rule leaves no empty permissions behind.
    fs::write(&file, "{}\n").unwrap();
    e.apply("qwen", Location::Global, InstallOptions { auto_allow: false, ..Default::default() }, false).unwrap();
    assert!(json(&file).get("permissions").is_none());
    install(&e, "qwen", Location::Global);
    uninstall(&e, "qwen", Location::Global);
    assert!(!file.exists() || json(&file).get("permissions").is_none());
}

const DSH_ROW: &str = "serverName: codegraphV";

#[test]
fn dsh_adds_its_own_insert_patch_after_the_users() {
    let dir = tempfile::tempdir().unwrap();
    let e = env(&dir);
    let file = e.home.join(".dsh/cordis.patch.yml");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    let user = "# my patches\n- insert:\n    - id: mine\n      name: something\n";
    fs::write(&file, user).unwrap();
    install(&e, "dsh", Location::Global);
    let text = fs::read_to_string(&file).unwrap();
    assert!(text.starts_with(user), "{text}");
    assert!(text.contains("\n\n- insert:\n    - id: mcp-codegraphV\n      name: '@deepseek-ai/dsh-mcp-client'\n"), "{text}");
    assert!(text.contains(DSH_ROW) && text.contains("transport: stdio"), "{text}");
    assert!(e.home.join(".dsh/AGENTS.md").exists());
    assert!(e.configured("dsh", Location::Global));
    let again = install(&e, "dsh", Location::Global);
    assert!(again.files.iter().all(|f| f.action == "unchanged"), "{again:?}");
    uninstall(&e, "dsh", Location::Global);
    assert_eq!(fs::read_to_string(&file).unwrap(), user);
}

#[test]
fn dsh_replaces_an_empty_list_and_leaves_one_when_only_comments_remain() {
    let dir = tempfile::tempdir().unwrap();
    let e = env(&dir);
    let file = e.home.join(".dsh/cordis.patch.yml");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, "# comment\n[]\n").unwrap();
    install(&e, "dsh", Location::Global);
    let text = fs::read_to_string(&file).unwrap();
    assert!(text.starts_with("# comment\n- insert:\n"), "{text}");
    uninstall(&e, "dsh", Location::Global);
    assert_eq!(fs::read_to_string(&file).unwrap(), "# comment\n[]\n");
}

#[test]
fn dsh_new_file_is_deleted_on_uninstall() {
    let dir = tempfile::tempdir().unwrap();
    let e = env(&dir);
    fs::create_dir_all(e.home.join(".dsh")).unwrap();
    install(&e, "dsh", Location::Global);
    let file = e.home.join(".dsh/cordis.patch.yml");
    assert!(fs::read_to_string(&file).unwrap().starts_with("- insert:\n"));
    uninstall(&e, "dsh", Location::Global);
    assert!(!file.exists());
}

#[test]
fn dsh_refuses_a_file_that_is_not_a_block_list() {
    let dir = tempfile::tempdir().unwrap();
    let e = env(&dir);
    let file = e.home.join(".dsh/cordis.patch.yml");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, "plugins: {}\n").unwrap();
    let err = e.apply("dsh", Location::Global, InstallOptions::default(), false).unwrap_err();
    assert!(err.to_string().contains("block-style YAML list"), "{err}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "plugins: {}\n");
}

#[test]
fn dsh_leaves_a_profile_entry_alone_on_install_and_removes_it_on_uninstall() {
    let dir = tempfile::tempdir().unwrap();
    let e = env(&dir);
    let profile = e.home.join(".dsh/profiles/work/cordis.patch.yml");
    fs::create_dir_all(profile.parent().unwrap()).unwrap();
    let entry = "- insert:\n    - id: cg\n      name: '@deepseek-ai/dsh-mcp-client'\n      config:\n        serverName: codegraphV\n        cwd: /work\n";
    fs::write(&profile, format!("- insert:\n    - id: other\n      name: x\n{}", &entry[10..])).unwrap();
    let result = install(&e, "dsh", Location::Global);
    assert!(result.notes.iter().any(|n| n.contains("Left the CodeGraphV entry")), "{:?}", result.notes);
    assert!(!e.home.join(".dsh/cordis.patch.yml").exists());
    assert!(e.configured("dsh", Location::Global));
    uninstall(&e, "dsh", Location::Global);
    assert_eq!(fs::read_to_string(&profile).unwrap(), "- insert:\n    - id: other\n      name: x\n");
}
