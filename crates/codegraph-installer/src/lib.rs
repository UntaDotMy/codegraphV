//! Agent configuration without a JavaScript runtime. All paths and environment
//! overrides are injected so callers and tests never need to mutate a real home.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
mod dsh;
mod edit;
pub mod upgrade;
mod markers;
mod transaction;
use transaction::read;
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
    "bob",
    "droid",
    "windsurf",
    "qwen",
    "dsh",
    "grok",
    "kimi",
    "qoder",
    "pi",
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
    /// The folder an agent's `*_HOME`-style variable names, or `fallback` when it is unset or
    /// blank. A leading `~` is the home and a relative path is taken from the project, so the
    /// installer writes where the agent reads (upstream `configDirFromEnv`).
    fn home_var(&self, key: &str, fallback: PathBuf) -> PathBuf {
        let Some(value) = self.vars.get(key).map(|s| s.trim()).filter(|s| !s.is_empty()) else {
            return fallback;
        };
        if value == "~" {
            return self.home.clone();
        }
        if let Some(rest) = value.strip_prefix("~/").or_else(|| value.strip_prefix("~\\")) {
            return self.home.join(rest);
        }
        self.project.join(value)
    }
    fn agent_home(&self, loc: Location, key: &str, global: &str, local: &str) -> PathBuf {
        if loc == Location::Local {
            self.project.join(local)
        } else {
            self.home_var(key, self.home.join(global))
        }
    }
    /// The configuration folder of an agent added in upstream 61e16218: its presence is how
    /// `auto` detects the agent, and its files live in it.
    fn agent_dir(&self, id: &str, loc: Location) -> Option<PathBuf> {
        Some(match id {
            "bob" => self.base(loc, ".bob"),
            "droid" => self.base(loc, ".factory"),
            "windsurf" => self.home.join(".codeium/windsurf"),
            "qwen" => self.agent_home(loc, "QWEN_HOME", ".qwen", ".qwen"),
            "grok" => self.agent_home(loc, "GROK_HOME", ".grok", ".grok"),
            "kimi" => self.agent_home(loc, "KIMI_CODE_HOME", ".kimi-code", ".kimi-code"),
            "qoder" => self.agent_home(loc, "QODER_CONFIG_DIR", ".qoder", ".qoder"),
            "pi" => self.agent_home(loc, "PI_CODING_AGENT_DIR", ".pi/agent", ".pi"),
            "dsh" => self.home_var("DSH_HOME", self.home.join(".dsh")),
            _ => return None,
        })
    }
    /// The dsh profiles' own patch files that exist.
    fn dsh_profile_patches(&self) -> Vec<PathBuf> {
        let Some(home) = self.agent_dir("dsh", Location::Global) else { return Vec::new() };
        let Ok(entries) = std::fs::read_dir(home.join("profiles")) else { return Vec::new() };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path().join(dsh::PATCH_FILE))
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        files
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
                "hermes" | "antigravity" | "copilot-cli" | "copilot-jetbrains" | "windsurf" | "dsh"
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
            "bob" => {
                let dir = self.agent_dir(id, loc).unwrap();
                if loc == Location::Local {
                    dir.join("mcp.json")
                } else {
                    // Bob's earlier settings file stays in use until the current one exists.
                    let current = dir.join("settings/mcp.json");
                    let earlier = dir.join("settings/mcp_settings.json");
                    if !current.exists() && earlier.exists() { earlier } else { current }
                }
            }
            "droid" | "kimi" | "pi" => self.agent_dir(id, loc).unwrap().join("mcp.json"),
            "windsurf" => self.agent_dir(id, loc).unwrap().join("mcp_config.json"),
            "qwen" => self.agent_dir(id, loc).unwrap().join("settings.json"),
            "grok" => self.agent_dir(id, loc).unwrap().join("config.toml"),
            "qoder" => self.agent_dir(id, loc).unwrap().join(if loc == Location::Local { "settings.local.json" } else { "settings.json" }),
            "dsh" => self.agent_dir(id, loc).unwrap().join(dsh::PATCH_FILE),
            _ => {
                return Err(
                    format!("Unknown target: {id}. Available: {}", TARGETS.join(", ")).into(),
                )
            }
        })
    }
    pub fn instructions_path(&self, id: &str, loc: Location) -> Option<PathBuf> {
        match id {
            "claude" if loc == Location::Local
                && !self.project.join("CLAUDE.md").exists()
                && !self.project.join(".claude/CLAUDE.md").exists() => Some(self.project.join("AGENTS.md")),
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
            "droid" | "kimi" | "pi" => Some(if loc == Location::Local {
                self.project.join("AGENTS.md")
            } else {
                self.agent_dir(id, loc)?.join("AGENTS.md")
            }),
            "qwen" => Some(if loc == Location::Local {
                self.project.join("QWEN.md")
            } else {
                self.agent_dir(id, loc)?.join("QWEN.md")
            }),
            "grok" => Some(self.agent_dir(id, loc)?.join("rules/codegraphV.md")),
            "dsh" => Some(self.agent_dir(id, loc)?.join("AGENTS.md")),
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
        // Agents whose documented stdio entry is a bare `{ command, args }`.
        if matches!(id, "antigravity" | "bob" | "windsurf" | "qwen" | "kimi" | "qoder" | "pi") {
            entry.as_object_mut().unwrap().remove("type");
        }
        if id == "pi" {
            entry["exposure"] = json!("direct");
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
        if id == "codex" || id == "grok" {
            return edit::toml("", CODEGRAPHV.server, Some(&entry));
        }
        if id == "dsh" {
            return Ok(dsh::render_entry(CODEGRAPHV.server, &self.executable, &mcp_args(&entry)).join("\n") + "\n");
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
        if let Some(dir) = self.agent_dir(id, loc) {
            return self.supports(id, loc) && dir.exists();
        }
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
        if id == "dsh" {
            return std::iter::once(path)
                .chain(self.dsh_profile_patches())
                .any(|p| std::fs::read_to_string(p).is_ok_and(|t| dsh::has_server(&t, brand.server)));
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            return false;
        };
        if id == "codex" || id == "grok" {
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
        Ok(self.apply_many(&[(id.to_owned(), options)], loc, remove)?.remove(0).1)
    }
    /// Plan and commit every target as one transaction: a write failure restores all of them.
    pub fn apply_many(
        &self,
        requests: &[(String, InstallOptions)],
        loc: Location,
        remove: bool,
    ) -> Result<Vec<(String, WriteResult)>> {
        self.apply_many_as(&CODEGRAPHV, requests, loc, remove)
    }
    /// Remove everything `brand` wrote for `id` (its server entry, permission, prompt hook and
    /// instructions block), leaving every other product's entries alone.
    pub fn remove_as(&self, brand: &Brand, id: &str, loc: Location, dry_run: bool) -> Result<WriteResult> {
        Ok(self.remove_many_as(brand, &[id.to_owned()], loc, dry_run)?.remove(0).1)
    }
    pub fn remove_many_as(&self, brand: &Brand, ids: &[String], loc: Location, dry_run: bool) -> Result<Vec<(String, WriteResult)>> {
        let requests: Vec<_> = ids.iter().map(|id| (id.clone(), InstallOptions { dry_run, ..InstallOptions::default() })).collect();
        self.apply_many_as(brand, &requests, loc, true)
    }
    fn apply_many_as(
        &self,
        brand: &Brand,
        requests: &[(String, InstallOptions)],
        loc: Location,
        remove: bool,
    ) -> Result<Vec<(String, WriteResult)>> {
        let dry_run = requests.iter().any(|(_, options)| options.dry_run);
        // Recovery writes, so a dry run must not trigger it.
        let recovery = if dry_run { Vec::new() } else { transaction::recover(&self.home)? };
        let mut merged: Vec<transaction::PlannedFile> = Vec::new();
        let mut index: HashMap<PathBuf, usize> = HashMap::new();
        let mut results: Vec<(String, WriteResult)> = Vec::new();
        let batch: Vec<String> = requests.iter().map(|(id, _)| id.clone()).collect();
        for (id, options) in requests {
            let (mut notes, files) = self.plan_as(brand, id, loc, *options, remove, &batch)?;
            if id == "codex" && loc == Location::Local && !remove {
                notes.push("Codex applies project configuration only after the project is trusted.".into());
            }
            let mut result = WriteResult { files: Vec::new(), notes };
            for file in files {
                if let Some(&at) = index.get(&file.path) {
                    if merged[at].after != file.after {
                        return Err(format!(
                            "Two integrations would write different content to {}; nothing was written. Configure them separately.",
                            file.path.display()
                        ).into());
                    }
                    // A later target observes the file as the earlier one leaves it.
                    result.files.push(FileChange { path: file.path, action: "unchanged".into() });
                } else {
                    index.insert(file.path.clone(), merged.len());
                    let action = action_for(&file, remove);
                    result.files.push(FileChange { path: file.path.clone(), action: action.into() });
                    merged.push(file);
                }
            }
            results.push((id.clone(), result));
        }
        let mut notes = transaction::commit(&self.home, &merged, dry_run)?;
        if let Some((_, first)) = results.first_mut() {
            first.notes.extend(recovery);
            first.notes.append(&mut notes);
        }
        Ok(results)
    }
    /// Compute one target's complete write set without touching the disk.
    fn plan_as(
        &self,
        brand: &Brand,
        id: &str,
        loc: Location,
        options: InstallOptions,
        remove: bool,
        batch: &[String],
    ) -> Result<(Vec<String>, Vec<transaction::PlannedFile>)> {
        // Only codegraphV is ever installed; another brand is only removed.
        let remove = remove || brand.server != CODEGRAPHV.server;
        if !self.supports(id, loc) {
            return Ok((
                vec![format!("{id} supports global configuration only; use --location global.")],
                vec![],
            ));
        }
        let file = self.config_path(id, loc)?;
        let existed = file.exists();
        let before = read(&file)?;
        let entry = self.entry(id, loc);
        let value = (!remove).then_some(&entry);
        let mut notes = if remove { Vec::new() } else { self.install_notes(id, loc, &file) };
        let mut extra = Vec::new();
        let after = match id {
            "codex" | "grok" => Some(edit::toml(&before, brand.server, value)?),
            "hermes" => Some(edit::yaml(&before, brand.server, value)?),
            "dsh" if remove => {
                for profile in self.dsh_profile_patches() {
                    let text = read(&profile)?;
                    let (after, note) = dsh::remove(&text, &profile, brand.server, false);
                    notes.extend(note);
                    if after.as_deref() != Some(text.as_str()) {
                        extra.push(planned_file(profile, true, text, after));
                    }
                }
                let (after, note) = dsh::remove(&before, &file, brand.server, true);
                notes.extend(note);
                after
            }
            "dsh" => {
                let elsewhere = if dsh::has_server(&before, brand.server) {
                    None
                } else {
                    self.dsh_profile_patches().into_iter().find(|p| read(p).is_ok_and(|t| dsh::has_server(&t, brand.server)))
                };
                match elsewhere {
                    Some(profile) => {
                        notes.insert(0, format!(
                            "Left the CodeGraphV entry in {} as you wrote it: a second one in {} would fail to load. Move it there to use CodeGraphV in every profile.",
                            profile.display(), file.display()
                        ));
                        Some(before.clone())
                    }
                    None => Some(dsh::install(&before, &file, brand.server, &self.executable, &mcp_args(&entry))?),
                }
            }
            _ => {
                // opencode: drop a pre-#1698 `mcp.<server>` entry either way.
                let legacy = if id == "opencode" {
                    edit::json(&before, &["mcp", brand.server], None)?
                } else if id == "qwen" {
                    // Qwen Code's allow rule names the server (upstream 61e16218).
                    let rule = format!("mcp__{}", brand.server);
                    qwen_allow_rule(&before, &rule, if remove { Some(false) } else { options.auto_allow.then_some(true) })?
                } else {
                    before.clone()
                };
                match value {
                    Some(value) => {
                        let keys = config_keys(id, brand.server);
                        let data = jsonc_parser::parse_to_serde_value(if legacy.trim().is_empty() { "{}" } else { &legacy }, &Default::default())?.unwrap_or(json!({}));
                        let mut existing = &data;
                        for key in &keys { existing = &existing[*key]; }
                        if !existing.is_null() && !existing.is_object() {
                            return Err("Managed MCP entry must be an object; nothing was changed.".into());
                        }
                        let mut merged = legacy.clone();
                        for (key, val) in value.as_object().unwrap() {
                            // Refresh launch wiring, retaining per-server user preferences.
                            if matches!(key.as_str(), "command" | "args") || existing.get(key).is_none() {
                                let mut path = keys.clone();
                                path.push(key);
                                merged = edit::json(&merged, &path, Some(val))?;
                            }
                        }
                        Some(merged)
                    },
                    None => remove_json(&before, &legacy, &config_keys(id, brand.server))?,
                }
            }
        };
        // `None` deletes the file: uninstall left nothing in it.
        let mut planned = vec![planned_file(file, existed, before, after)];
        planned.append(&mut extra);
        // Installation writes one primary path. Removal also looks where an earlier install
        // wrote before the user created a file that shadows it (A34/A35).
        let instruction_paths: Vec<PathBuf> = if remove {
            self.instruction_files(id, loc)
        } else {
            self.instructions_path(id, loc).into_iter().collect()
        };
        for file in instruction_paths {
            let existed = file.exists();
            let before = read(&file)?;
            if remove {
                // Only files that name this product's markers can be edited, so an unrelated
                // malformed example elsewhere never blocks an uninstall.
                if !before.contains(brand.start) && !before.contains(brand.end) {
                    continue;
                }
                let shared = TARGETS.iter().any(|other| *other != id
                    && !batch.iter().any(|removed| removed == other)
                    && self.supports(other, loc)
                    && self.instructions_path(other, loc).as_ref() == Some(&file)
                    && self.configured_as(brand, other, loc));
                if shared {
                    continue;
                }
                markers::validate(&before, brand.start, brand.end)?;
                let Some(after) = markers::strip_preserving(&before, brand.start, brand.end) else {
                    continue;
                };
                planned.push(planned_file(file, existed, before, after));
            } else {
                markers::validate(&before, brand.start, brand.end)?;
                let existing = existed.then_some(before.as_str());
                let after = markers::upsert_preserving(existing, INSTRUCTIONS, brand.start, brand.end);
                planned.push(transaction::PlannedFile { path: file, before: existed.then_some(before), after: Some(after) });
            }
        }
        if id == "claude" {
            let settings = self
                .profile(loc, "CLAUDE_CONFIG_DIR", ".claude")
                .join("settings.json");
            let existed = settings.exists();
            let before = read(&settings)?;
            let parse = |text: &str| -> Result<Value> {
                Ok(jsonc_parser::parse_to_serde_value(if text.trim().is_empty() { "{}" } else { text }, &Default::default())?
                    .unwrap_or(json!({})))
            };
            // Permissions, then the prompt hook, applied in turn to the same file.
            let mut after: Option<String> = Some(before.clone());
            validate_claude_settings(&parse(&before)?)?;
            if (remove || !options.preserve_permissions) && (!before.is_empty() || options.auto_allow) {
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
                planned.push(planned_file(settings, existed, before, after));
            }
            if loc == Location::Local {
                let file = self.project.join(".claude.json");
                let existed = file.exists();
                let before = read(&file)?;
                if !before.is_empty() {
                    let after = remove_json(&before, &before, &["mcpServers", brand.server])?;
                    planned.push(planned_file(file, existed, before, after));
                }
            }
        }
        if id == "antigravity" && remove {
            for suffix in [
                ".gemini/config/mcp_config.json",
                ".gemini/antigravity/mcp_config.json",
            ] {
                let path = self.home.join(suffix);
                if !planned.iter().any(|p| p.path == path) {
                    let existed = path.exists();
                    let before = read(&path)?;
                    if !before.is_empty() {
                        let after = remove_json(&before, &before, &["mcpServers", brand.server])?;
                        planned.push(planned_file(path, existed, before, after));
                    }
                }
            }
        }
        Ok((notes, planned))
    }
    /// What a person should know after installing one of upstream 61e16218's agents.
    fn install_notes(&self, id: &str, loc: Location, file: &Path) -> Vec<String> {
        let local = loc == Location::Local;
        let shown = file.display();
        let (always, trusted): (&str, Option<String>) = match id {
            "droid" => ("", local.then(|| format!("Droid shares {shown} with your team: it is meant to be committed."))),
            "windsurf" => ("Restart Windsurf, or refresh its MCP list, to load CodeGraphV.", None),
            "qwen" => ("Restart any running Qwen Code session to pick up the MCP server.", None),
            "dsh" => ("Start a new dsh session to pick up the MCP server, after a restart if dsh-hmr is off.", None),
            "grok" => ("Start a new Grok session, or press r in /mcps, to pick up the MCP server.", local.then(|| format!("Grok applies {shown} only in a trusted folder: run /hooks-trust there, or start grok with --trust."))),
            "kimi" => ("Start a new Kimi Code session: a server added mid-session only joins later ones.", local.then(|| format!("Kimi Code runs {shown} only in a trusted folder: trust this folder when Kimi Code asks."))),
            "qoder" => ("This sets up the Qoder CLI. In the Qoder IDE, add the server under Settings > MCP.", local.then(|| format!("Qoder loads {shown} only in a trusted folder."))),
            "pi" => ("Run /reload in Pi, or restart it, to pick up the MCP server.", local.then(|| format!("Pi reads {shown} only after you trust this project."))),
            _ => ("", None),
        };
        (!always.is_empty()).then(|| always.to_owned()).into_iter().chain(trusted).collect()
    }
    /// Every path this target may have written its instructions block to. Claude uses
    /// AGENTS.md only while no CLAUDE.md exists; removal must still find a block the user has
    /// since shadowed by creating one (A34/A35).
    fn instruction_files(&self, id: &str, loc: Location) -> Vec<PathBuf> {
        let primary = self.instructions_path(id, loc);
        if id == "claude" && loc == Location::Local {
            let mut files = vec![self.project.join("AGENTS.md")];
            files.extend(primary);
            files.sort();
            files.dedup();
            files
        } else {
            primary.into_iter().collect()
        }
    }
}
fn action_for(file: &transaction::PlannedFile, remove: bool) -> &'static str {
    if file.after == file.before {
        if file.before.is_some() { "unchanged" } else { "not-found" }
    } else if remove {
        "removed"
    } else if file.before.is_some() {
        "updated"
    } else {
        "created"
    }
}
/// A missing file whose planned result is empty stays missing: removal of an entry that was
/// never there must not create an empty config file.
fn planned_file(path: PathBuf, existed: bool, before: String, after: Option<String>) -> transaction::PlannedFile {
    let after = if !existed && after.as_deref().is_some_and(str::is_empty) { None } else { after };
    transaction::PlannedFile { path, before: existed.then_some(before), after }
}
/// The launch arguments of an MCP entry.
fn mcp_args(entry: &Value) -> Vec<String> {
    entry["args"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(str::to_owned)).collect()
}
/// Qwen Code's `permissions.allow` with `rule` added (`Some(true)`), removed (`Some(false)`, with
/// an emptied `allow` and a then-empty `permissions` dropped) or left alone (`None`). A shape
/// other than an object holding an array is the user's and is never changed.
fn qwen_allow_rule(text: &str, rule: &str, add: Option<bool>) -> Result<String> {
    let Some(add) = add else { return Ok(text.to_owned()) };
    let data = jsonc_parser::parse_to_serde_value(if text.trim().is_empty() { "{}" } else { text }, &Default::default())?
        .unwrap_or(json!({}));
    let permissions = data.get("permissions");
    let allow = permissions.and_then(|p| p.get("allow"));
    if add {
        let next = match (permissions, allow) {
            (None, _) => json!([rule]),
            (Some(p), _) if !p.is_object() => return Ok(text.to_owned()),
            (Some(_), None) => json!([rule]),
            (Some(_), Some(Value::Array(rules))) if !rules.iter().any(|r| r == rule) => {
                let mut rules = rules.clone();
                rules.push(json!(rule));
                Value::Array(rules)
            }
            _ => return Ok(text.to_owned()),
        };
        return edit::json(text, &["permissions", "allow"], Some(&next));
    }
    let Some(Value::Array(rules)) = allow else { return Ok(text.to_owned()) };
    if !rules.iter().any(|r| r == rule) {
        return Ok(text.to_owned());
    }
    if rules.len() > 1 {
        let rest: Vec<Value> = rules.iter().filter(|r| *r != rule).cloned().collect();
        return edit::json(text, &["permissions", "allow"], Some(&Value::Array(rest)));
    }
    let only_allow = permissions.and_then(Value::as_object).is_some_and(|p| p.len() == 1);
    let path: &[&str] = if only_allow { &["permissions"] } else { &["permissions", "allow"] };
    edit::json(text, path, None)
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
    /// Refresh leaves the user's current allow list exactly as configured.
    pub preserve_permissions: bool,
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
            preserve_permissions: false,
            prompt_hook: None,
            dry_run: false,
        }
    }
}

/// The hook command earlier builds wrote: found through PATH, which the agent may not have
/// (BUG-95). Still recognized, and replaced by [`prompt_hook_command`].
pub const PROMPT_HOOK_COMMAND: &str = "codegraphV prompt-hook";

/// Executable for Claude's direct-exec hook form; paired with an explicit argument vector.
/// Requires a host supporting command hooks with `args` (current Claude hook contract).
pub fn prompt_hook_command(executable: &str) -> String {
    executable.to_owned()
}

fn is_prompt_hook(brand: &Brand, hook: &Value) -> bool {
    if hook["type"] != "command" { return false; }
    if hook.get("args").is_some() {
        if hook["args"] != json!(["prompt-hook"]) { return false; }
        let Some(program) = hook["command"].as_str() else { return false; };
        let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
        return name.eq_ignore_ascii_case(brand.binary)
            || name.eq_ignore_ascii_case(&format!("{}.exe", brand.binary));
    }
    is_prompt_hook_command(brand, &hook["command"])
}

fn validate_claude_settings(data: &Value) -> Result<()> {
    if !data.is_object() { return Err("Claude settings must be an object".into()); }
    if let Some(permissions) = data.get("permissions") {
        if !permissions.is_object() || permissions.get("allow").is_some_and(|v| !v.is_array()) {
            return Err("Malformed Claude permissions; nothing was changed".into());
        }
    }
    if let Some(hooks) = data.get("hooks") {
        let events = hooks.as_object().ok_or("Claude hooks must be an object; nothing was changed")?;
        for groups in events.values() {
            let groups = groups.as_array().ok_or("Claude hook event must be an array; nothing was changed")?;
            for group in groups {
                if !group.is_object() || !group.get("hooks").is_some_and(Value::is_array) {
                    return Err("Malformed Claude hook group; nothing was changed".into());
                }
            }
        }
    }
    Ok(())
}

fn is_prompt_hook_command(brand: &Brand, command: &Value) -> bool {
    let Some(command) = command.as_str() else { return false };
    if brand.hook_forms.iter().any(|f| command.trim() == *f) {
        return true;
    }
    // `"<dir>/<binary>[.exe]" prompt-hook` or `<dir>/<binary> prompt-hook`.
    let command = command.trim();
    let (program, rest) = match command.strip_prefix('"') {
        Some(quoted) => match quoted.split_once('"') {
            Some(split) => split,
            None => return false,
        },
        None => {
            let split = command.split_once(char::is_whitespace).unwrap_or((command, ""));
            if split.0.contains([';', '&', '|', '<', '>', '$', '`', '(', ')']) { return false; }
            split
        },
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
            group["hooks"].as_array().is_some_and(|hs| hs.iter().any(|h| is_prompt_hook(brand, h)))
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
            if is_prompt_hook(brand, hook) {
                already = true;
                if hook["command"] != command {
                    hook["command"] = json!(command);
                }
                hook["args"] = json!(["prompt-hook"]);
            }
        }
        if !already {
            groups.push(json!({"hooks": [{"type": "command", "command": command, "args": ["prompt-hook"]}]}));
        }
        return Some(hooks);
    }
    let original = hooks?;
    let mut hooks = original.clone();
    let Some(events) = hooks.as_object_mut() else { return Some(hooks) };
    let mut removed = false;
    events.retain(|_, groups| {
        let Some(groups) = groups.as_array_mut() else { return true; };
        let was_empty = groups.is_empty();
        groups.retain_mut(|group| {
            let mut changed = false;
            if let Some(hs) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = hs.len();
                hs.retain(|h| !is_prompt_hook(brand, h));
                changed = hs.len() != before;
                removed |= changed;
            }
            !changed || !group["hooks"].as_array().is_some_and(|hs| hs.is_empty())
                || group.as_object().is_some_and(|g| g.len() > 1)
        });
        was_empty || !groups.is_empty()
    });
    if !removed {
        return Some(original.clone());
    }
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
