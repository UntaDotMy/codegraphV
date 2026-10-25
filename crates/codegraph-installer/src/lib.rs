//! Agent configuration without a JavaScript runtime. All paths and environment
//! overrides are injected so callers and tests never need to mutate a real home.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{self, Write},
    path::{Path, PathBuf},
};
mod edit;
pub mod upgrade;
mod markers;
pub mod original;
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
pub const TARGETS: &[&str] = &[
    "claude",
    "cursor",
    "codex",
    "opencode",
    "hermes",
    "gemini",
    "antigravity",
    "kiro",
    "copilot-vscode",
    "copilot-cli",
    "copilot-jetbrains",
];
pub const INSTRUCTIONS: &str = include_str!("assets/instructions.md");
/// The names one product writes into agent configuration, so removing it touches only its own
/// entries: codegraphV's ([`CODEGRAPHV`]) or the original codegraph's ([`original::ORIGINAL`]).
#[derive(Debug, Clone, Copy)]
pub struct Brand {
    /// The MCP server key (`mcpServers.<server>`, `[mcp_servers.<server>]`, ...).
    pub server: &'static str,
    /// Claude's tool permission; allow entries starting with `permission_prefix` are this product's.
    pub permission: &'static str,
    pub permission_prefix: &'static str,
    /// Its program name: a prompt hook running `<path>/<binary>[.exe|.cmd] prompt-hook` is its.
    pub binary: &'static str,
    /// Other prompt-hook command spellings this product has written.
    pub hook_forms: &'static [&'static str],
    /// The markers fencing its block in an instructions file.
    pub start: &'static str,
    pub end: &'static str,
}
pub const CODEGRAPHV: Brand = Brand {
    server: "codegraphV",
    permission: "mcp__codegraphV__*",
    permission_prefix: "mcp__codegraphV__",
    binary: "codegraphV",
    hook_forms: &[PROMPT_HOOK_COMMAND],
    start: "<!-- CODEGRAPHV_START -->",
    end: "<!-- CODEGRAPHV_END -->",
};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    Global,
    Local,
}
#[derive(Debug, Clone)]
pub struct Environment {
    pub home: PathBuf,
    pub project: PathBuf,
    pub executable: String,
    pub vars: HashMap<String, String>,
}
impl Environment {
    pub fn current() -> Result<Self> {
        Ok(Self {
            home: codegraph_types::paths::home_dir().ok_or("Cannot find home directory")?,
            project: std::env::current_dir()?,
            executable: std::env::current_exe()?.to_string_lossy().into_owned(),
            vars: std::env::vars().collect(),
        })
    }
    fn env(&self, key: &str, fallback: PathBuf) -> PathBuf {
        self.vars
            .get(key)
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or(fallback)
    }
    fn base(&self, loc: Location, dir: &str) -> PathBuf {
        if loc == Location::Local {
            self.project.join(dir)
        } else {
            self.home.join(dir)
        }
    }
    fn xdg(&self) -> PathBuf {
        self.env("XDG_CONFIG_HOME", self.home.join(".config"))
    }
    fn profile(&self, loc: Location, key: &str, dir: &str) -> PathBuf {
        if loc == Location::Local {
            self.project.join(dir)
        } else {
            self.env(key, self.home.join(dir))
        }
    }
    pub fn supports(&self, id: &str, loc: Location) -> bool {
        loc == Location::Global
            || !matches!(
                id,
                "hermes" | "antigravity" | "copilot-cli" | "copilot-jetbrains"
            )
    }
    pub fn config_path(&self, id: &str, loc: Location) -> Result<PathBuf> {
        Ok(match id {
            "claude" => {
                if loc == Location::Local {
                    self.project.join(".mcp.json")
                } else if self
                    .vars
                    .get("CLAUDE_CONFIG_DIR")
                    .is_some_and(|s| !s.trim().is_empty())
                {
                    self.profile(loc, "CLAUDE_CONFIG_DIR", ".claude")
                        .join(".claude.json")
                } else {
                    self.home.join(".claude.json")
                }
            }
            "cursor" => self.base(loc, ".cursor").join("mcp.json"),
            "codex" => self
                .profile(loc, "CODEX_HOME", ".codex")
                .join("config.toml"),
            "opencode" => {
                let base = if loc == Location::Local {
                    self.project.clone()
                } else {
                    self.xdg().join("opencode")
                };
                let jsonc = base.join("opencode.jsonc");
                if jsonc.exists() || !base.join("opencode.json").exists() {
                    jsonc
                } else {
                    base.join("opencode.json")
                }
            }
            "hermes" => self
                .env("HERMES_HOME", self.home.join(".hermes"))
                .join("config.yaml"),
            "gemini" => self.base(loc, ".gemini").join("settings.json"),
            "antigravity" => {
                let config = self.home.join(".gemini/config");
                if config.join(".migrated").exists() || config.join("mcp_config.json").exists() {
                    config.join("mcp_config.json")
                } else {
                    self.home.join(".gemini/antigravity/mcp_config.json")
                }
            }
            "kiro" => self.base(loc, ".kiro").join("settings/mcp.json"),
            "copilot-cli" => self
                .env("COPILOT_HOME", self.home.join(".copilot"))
                .join("mcp-config.json"),
            "copilot-vscode" => {
                if loc == Location::Local {
                    self.project.join(".vscode/mcp.json")
                } else {
                    let base = if cfg!(windows) {
                        self.env("APPDATA", self.home.join("AppData/Roaming"))
                            .join("Code/User")
                    } else if cfg!(target_os = "macos") {
                        self.home.join("Library/Application Support/Code/User")
                    } else {
                        self.xdg().join("Code/User")
                    };
                    base.join("mcp.json")
                }
            }
            "copilot-jetbrains" => {
                let base = if self
                    .vars
                    .get("XDG_CONFIG_HOME")
                    .is_some_and(|s| !s.trim().is_empty())
                {
                    self.xdg()
                } else if cfg!(windows) {
                    self.env("LOCALAPPDATA", self.home.join("AppData/Local"))
                } else {
                    self.xdg()
                };
                base.join("github-copilot/intellij/mcp.json")
            }
            _ => {
                return Err(
                    format!("Unknown target: {id}. Available: {}", TARGETS.join(", ")).into(),
                )
            }
        })
    }
    pub fn instructions_path(&self, id: &str, loc: Location) -> Option<PathBuf> {
        match id {
            "claude" => Some(
                self.profile(loc, "CLAUDE_CONFIG_DIR", ".claude")
                    .join("CLAUDE.md"),
            ),
            "codex" => Some(if loc == Location::Local {
                self.project.join("AGENTS.md")
            } else {
                self.profile(loc, "CODEX_HOME", ".codex").join("AGENTS.md")
            }),
            "opencode" => Some(if loc == Location::Local {
                self.project.join("AGENTS.md")
            } else {
                self.xdg().join("opencode/AGENTS.md")
            }),
            "gemini" => Some(if loc == Location::Local {
                self.project.join("GEMINI.md")
            } else {
                self.home.join(".gemini/GEMINI.md")
            }),
            _ => None,
        }
    }
    pub fn entry(&self, id: &str, loc: Location) -> Value {
        let mut args = vec!["serve".to_owned(), "--mcp".to_owned()];
        if id == "cursor" || loc == Location::Local && id == "copilot-vscode" {
            args.push("--path".into());
            args.push(if loc == Location::Global {
                "${workspaceFolder}".into()
            } else {
                self.project.to_string_lossy().into_owned()
            });
        }
        if id == "opencode" {
            let mut command = vec![self.executable.clone()];
            command.extend(args);
            return json!({"type":"local","command":command,"disabled":false,"codemode":false});
        }
        let mut entry = json!({"type":"stdio","command":self.executable,"args":args});
        if id == "antigravity" {
            entry.as_object_mut().unwrap().remove("type");
        }
        if id == "claude" {
            entry["alwaysLoad"] = json!(true);
        }
        if id == "copilot-cli" {
            entry["tools"] = json!(["*"]);
            entry["deferTools"] = json!("never");
        }
        entry
    }
    pub fn print_config(&self, id: &str, loc: Location) -> Result<String> {
        let entry = self.entry(id, loc);
        self.config_path(id, loc)?;
        if id == "codex" {
            return edit::toml("", CODEGRAPHV.server, Some(&entry));
        }
        if id == "hermes" {
            return edit::yaml("", CODEGRAPHV.server, Some(&entry));
        }
        let mut object = json!({});
        let mut node = &mut object;
        for key in config_keys(id, CODEGRAPHV.server) {
            node = &mut node[key];
        }
        *node = entry;
        Ok(serde_json::to_string_pretty(&object)?)
    }
    pub fn detected(&self, id: &str, loc: Location) -> bool {
        self.config_path(id, loc)
            .is_ok_and(|p| p.exists() || p.parent().is_some_and(Path::exists))
    }
    pub fn configured(&self, id: &str, loc: Location) -> bool {
        self.configured_as(&CODEGRAPHV, id, loc)
    }
    /// What `brand` has in Claude Code's settings.json at `loc`: (its tool permission, its
    /// prompt hook). An unreadable file has neither.
    pub fn claude_settings_as(&self, brand: &Brand, loc: Location) -> (bool, bool) {
        let path = self.profile(loc, "CLAUDE_CONFIG_DIR", ".claude").join("settings.json");
        let Ok(Some(data)) = std::fs::read_to_string(path)
            .map(|text| jsonc_parser::parse_to_serde_value(&text, &Default::default()).ok().flatten())
        else {
            return (false, false);
        };
        let permission = data["permissions"]["allow"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v.as_str().is_some_and(|s| s.starts_with(brand.permission_prefix))));
        let hook = has_prompt_hook(brand, data.get("hooks"));
        (permission, hook)
    }
    /// Whether `brand`'s MCP server entry is in `id`'s config file.
    pub fn configured_as(&self, brand: &Brand, id: &str, loc: Location) -> bool {
        let Ok(path) = self.config_path(id, loc) else {
            return false;
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return false;
        };
        if id == "codex" {
            return text
                .parse::<toml_edit::DocumentMut>()
                .ok()
                .is_some_and(|d| {
                    d.get("mcp_servers")
                        .and_then(|m| m.get(brand.server))
                        .is_some()
                });
        }
        if id == "hermes" {
            return edit::yaml_configured(&text, brand.server);
        }
        let Ok(Some(mut data)) = jsonc_parser::parse_to_serde_value(&text, &Default::default())
        else {
            return false;
        };
        for key in config_keys(id, brand.server) {
            data = data[key].take();
        }
        !data.is_null()
    }
    pub fn resolve_targets(&self, flag: &str, loc: Location) -> Result<Vec<String>> {
        let targets: Vec<String> = match flag {
            "all" => TARGETS.iter().map(|s| s.to_string()).collect(),
            "none" => vec![],
            "auto" => {
                let mut found: Vec<_> = TARGETS
                    .iter()
                    .filter(|id| self.supports(id, loc) && self.detected(id, loc))
                    .map(|id| id.to_string())
                    .collect();
                if found.is_empty() {
                    found.push("claude".into());
                }
                found
            }
            _ => flag
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
        };
        for id in &targets {
            if !TARGETS.contains(&id.as_str()) {
                return Err(
                    format!("Unknown target: {id}. Available: {}", TARGETS.join(", ")).into(),
                );
            }
        }
        Ok(targets)
    }
    pub fn apply(
        &self,
        id: &str,
        loc: Location,
        options: InstallOptions,
        remove: bool,
    ) -> Result<WriteResult> {
        self.apply_as(&CODEGRAPHV, id, loc, options, remove)
    }
    /// Remove everything `brand` wrote for `id` (its server entry, permission, prompt hook and
    /// instructions block), leaving every other product's entries alone.
    pub fn remove_as(&self, brand: &Brand, id: &str, loc: Location, dry_run: bool) -> Result<WriteResult> {
        self.apply_as(brand, id, loc, InstallOptions { dry_run, ..InstallOptions::default() }, true)
    }
    fn apply_as(
        &self,
        brand: &Brand,
        id: &str,
        loc: Location,
        options: InstallOptions,
        remove: bool,
    ) -> Result<WriteResult> {
        // Only codegraphV is ever installed; another brand is only removed.
        let remove = remove || brand.server != CODEGRAPHV.server;
        if !self.supports(id, loc) {
            return Ok(WriteResult {
                files: vec![],
                notes: vec![format!(
                    "{id} supports global configuration only; use --location global."
                )],
            });
        }
        let file = self.config_path(id, loc)?;
        let before = read(&file)?;
        let entry = self.entry(id, loc);
        let value = (!remove).then_some(&entry);
        let after = match id {
            "codex" => Some(edit::toml(&before, brand.server, value)?),
            "hermes" => Some(edit::yaml(&before, brand.server, value)?),
            _ => {
                // opencode: drop a pre-#1698 `mcp.<server>` entry either way.
                let legacy = if id == "opencode" {
                    edit::json(&before, &["mcp", brand.server], None)?
                } else {
                    before.clone()
                };
                match value {
                    Some(value) => Some(edit::json(&legacy, &config_keys(id, brand.server), Some(value))?),
                    None => remove_json(&before, &legacy, &config_keys(id, brand.server))?,
                }
            }
        };
        // `None` deletes the file: uninstall left nothing in it.
        let mut planned: Vec<(PathBuf, String, Option<String>)> = vec![(file, before, after)];
        if let Some(file) = self.instructions_path(id, loc) {
            let before = read(&file)?;
            let after = if remove {
                match markers::strip(&before, brand.start, brand.end) {
                    Some(rest) if markers::is_blank(&rest) => None,
                    Some(rest) => Some(rest),
                    None => Some(before.clone()),
                }
            } else {
                let existing = file.exists().then_some(before.as_str());
                Some(markers::upsert(existing, INSTRUCTIONS, brand.start, brand.end))
            };
            planned.push((file, before, after));
        }
        if id == "claude" {
            let settings = self
                .profile(loc, "CLAUDE_CONFIG_DIR", ".claude")
                .join("settings.json");
            let before = read(&settings)?;
            let parse = |text: &str| -> Result<Value> {
                Ok(jsonc_parser::parse_to_serde_value(if text.trim().is_empty() { "{}" } else { text }, &Default::default())?
                    .unwrap_or(json!({})))
            };
            // Permissions, then the prompt hook, applied in turn to the same file.
            let mut after: Option<String> = Some(before.clone());
            if !before.is_empty() || !remove && options.auto_allow {
                let data = parse(&before)?;
                let mut allow = data["permissions"]["allow"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let prior = allow.clone();
                allow.retain(|v| !v.as_str().is_some_and(|s| s.starts_with(brand.permission_prefix)));
                if !remove && options.auto_allow {
                    allow.push(json!(brand.permission));
                }
                if allow != prior {
                    after = if allow.is_empty() {
                        remove_json(&before, &before, &["permissions", "allow"])?
                    } else {
                        Some(edit::json(&before, &["permissions", "allow"], Some(&json!(allow)))?)
                    };
                }
            }
            let hook_choice = if remove { Some(false) } else { options.prompt_hook };
            if let Some(text) = after.clone() {
                let data = parse(&text)?;
                let current = data.get("hooks");
                // None (a refresh) keeps whatever is there.
                let command = prompt_hook_command(&self.executable);
                let next = match hook_choice {
                    Some(on) => prompt_hook_hooks(brand, current, on, &command),
                    // A refresh keeps the choice, but brings an existing hook to today's spelling.
                    None if has_prompt_hook(brand, current) => prompt_hook_hooks(brand, current, true, &command),
                    None => current.cloned(),
                };
                if next.as_ref() != current {
                    after = match next {
                        Some(hooks) => Some(edit::json(&text, &["hooks"], Some(&hooks))?),
                        None => remove_json(&before, &text, &["hooks"])?,
                    };
                }
            }
            if after.as_ref() != Some(&before) {
                planned.push((settings, before, after));
            }
            if loc == Location::Local {
                let file = self.project.join(".claude.json");
                let before = read(&file)?;
                if !before.is_empty() {
                    let after = remove_json(&before, &before, &["mcpServers", brand.server])?;
                    planned.push((file, before, after));
                }
            }
        }
        if id == "antigravity" && remove {
            for suffix in [
                ".gemini/config/mcp_config.json",
                ".gemini/antigravity/mcp_config.json",
            ] {
                let path = self.home.join(suffix);
                if !planned.iter().any(|(p, _, _)| p == &path) {
                    let before = read(&path)?;
                    if !before.is_empty() {
                        let after = remove_json(&before, &before, &["mcpServers", brand.server])?;
                        planned.push((path, before, after));
                    }
                }
            }
        }
        let mut result = WriteResult::default();
        for (path, before, after) in planned {
            let exists = path.exists();
            let action = if after.as_ref() == Some(&before) || after.is_none() && !exists {
                if exists {
                    "unchanged"
                } else {
                    "not-found"
                }
            } else {
                match &after {
                    _ if options.dry_run => {}
                    Some(text) => write_atomic(&path, text)?,
                    // A link is emptied through, never replaced by deleting it.
                    None if path.is_symlink() => write_atomic(&path, "")?,
                    None => std::fs::remove_file(&path)?,
                }
                if remove {
                    "removed"
                } else if exists {
                    "updated"
                } else {
                    "created"
                }
            };
            result.files.push(FileChange {
                path,
                action: action.into(),
            });
        }
        if id == "codex" && loc == Location::Local && !remove {
            result.notes.push(
                "Codex applies project configuration only after the project is trusted.".into(),
            );
        }
        Ok(result)
    }
}
fn config_keys<'a>(id: &str, server: &'a str) -> Vec<&'a str> {
    match id {
        "opencode" => vec!["mcp", "servers", server],
        "copilot-vscode" | "copilot-jetbrains" => vec!["servers", server],
        _ => vec!["mcpServers", server],
    }
}
#[derive(Debug, Clone, Copy)]
pub struct InstallOptions {
    pub auto_allow: bool,
    /// Claude's front-load `UserPromptSubmit` hook: Some(true) writes it, Some(false) strips it
    /// (an opt-out round-trips), None leaves whatever is there (a refresh).
    pub prompt_hook: Option<bool>,
    /// Report what would change (the same actions) and write nothing.
    pub dry_run: bool,
}
impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            auto_allow: true,
            prompt_hook: None,
            dry_run: false,
        }
    }
}

/// The hook command earlier builds wrote: found through PATH, which the agent may not have
/// (BUG-95). Still recognized, and replaced by [`prompt_hook_command`].
pub const PROMPT_HOOK_COMMAND: &str = "codegraphV prompt-hook";

/// The hook command `install` writes: the binary's absolute path, quoted, with forward slashes
/// so Git Bash (Claude Code's hook shell on Windows) and sh read it alike. Like the MCP entry,
/// it then runs whatever PATH the agent inherited.
pub fn prompt_hook_command(executable: &str) -> String {
    format!("\"{}\" prompt-hook", executable.replace('\\', "/"))
}

fn is_prompt_hook_command(brand: &Brand, command: &Value) -> bool {
    let Some(command) = command.as_str() else { return false };
    if brand.hook_forms.iter().any(|f| command.contains(f)) {
        return true;
    }
    // `"<dir>/<binary>[.exe]" prompt-hook` or `<dir>/<binary> prompt-hook`.
    let command = command.trim();
    let (program, rest) = match command.strip_prefix('"') {
        Some(quoted) => match quoted.split_once('"') {
            Some(split) => split,
            None => return false,
        },
        None => command.split_once(char::is_whitespace).unwrap_or((command, "")),
    };
    if rest.trim() != "prompt-hook" {
        return false;
    }
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let lower = name.to_ascii_lowercase();
    let stem = if lower.ends_with(".exe") || lower.ends_with(".cmd") { &name[..name.len() - 4] } else { name };
    stem.eq_ignore_ascii_case(brand.binary)
}

fn has_prompt_hook(brand: &Brand, hooks: Option<&Value>) -> bool {
    hooks.and_then(Value::as_object).is_some_and(|events| {
        events.values().filter_map(Value::as_array).flatten().any(|group| {
            group["hooks"].as_array().is_some_and(|hs| hs.iter().any(|h| is_prompt_hook_command(brand, &h["command"])))
        })
    })
}

/// `settings.hooks` with `brand`'s prompt hook written (`on`) or removed; None = drop `hooks`.
/// Unchanged input comes back equal, so the caller writes nothing.
/// `command` is what the hook runs; an older spelling of `brand`'s hook is rewritten in place.
fn prompt_hook_hooks(brand: &Brand, hooks: Option<&Value>, on: bool, command: &str) -> Option<Value> {
    if on {
        let mut hooks = hooks.filter(|h| h.is_object()).cloned().unwrap_or_else(|| json!({}));
        if !hooks["UserPromptSubmit"].is_array() {
            hooks["UserPromptSubmit"] = json!([]);
        }
        let groups = hooks["UserPromptSubmit"].as_array_mut().unwrap();
        let mut already = false;
        for hook in groups.iter_mut().filter_map(|g| g.get_mut("hooks").and_then(Value::as_array_mut)).flatten() {
            if is_prompt_hook_command(brand, &hook["command"]) {
                already = true;
                if hook["command"] != command {
                    hook["command"] = json!(command);
                }
            }
        }
        if !already {
            groups.push(json!({"hooks": [{"type": "command", "command": command}]}));
        }
        return Some(hooks);
    }
    let original = hooks?;
    let mut hooks = original.clone();
    let Some(events) = hooks.as_object_mut() else { return Some(hooks) };
    let mut removed = false;
    for groups in events.values_mut().filter_map(Value::as_array_mut) {
        for group in groups.iter_mut() {
            if let Some(hs) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = hs.len();
                hs.retain(|h| !is_prompt_hook_command(brand, &h["command"]));
                removed |= hs.len() != before;
            }
        }
    }
    if !removed {
        return Some(original.clone());
    }
    // Prune emptied groups, then events with no groups left, then `hooks` itself.
    for groups in events.values_mut().filter_map(Value::as_array_mut) {
        groups.retain(|g| !g["hooks"].as_array().is_some_and(|hs| hs.is_empty()));
    }
    events.retain(|_, groups| !groups.as_array().is_some_and(|g| g.is_empty()));
    (!events.is_empty()).then_some(hooks)
}
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct WriteResult {
    pub files: Vec<FileChange>,
    pub notes: Vec<String>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct FileChange {
    pub path: PathBuf,
    pub action: String,
}
fn read(path: &Path) -> io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
}
/// `text` (`original` after earlier edits) without the value at `path` and without the
/// objects that removal leaves empty; `None` when the document is then empty.
fn remove_json(original: &str, text: &str, path: &[&str]) -> Result<Option<String>> {
    let parse = |t: &str| -> Result<Value> {
        Ok(jsonc_parser::parse_to_serde_value(t, &Default::default())?.unwrap_or(Value::Null))
    };
    let mut out = edit::json(text, path, None)?;
    for depth in (1..path.len()).rev() {
        let parsed = parse(&out)?;
        let parent = path[..depth].iter().fold(&parsed, |v, k| &v[k]);
        if !parent.as_object().is_some_and(|o| o.is_empty()) {
            break;
        }
        out = edit::json(&out, &path[..depth], None)?;
    }
    let empty = !out.trim().is_empty() && parse(&out)?.as_object().is_some_and(|o| o.is_empty());
    Ok(if out != original && empty { None } else { Some(out) })
}
fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    let target = if path.is_symlink() {
        std::fs::canonicalize(path)?
    } else {
        path.to_owned()
    };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut temp = target.as_os_str().to_os_string();
    temp.push(format!(".codegraph-{}.tmp", std::process::id()));
    let temp = PathBuf::from(temp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let result = (|| {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, &target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}
