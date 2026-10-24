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
mod markers;
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
const START: &str = "<!-- CODEGRAPH_START -->";
const END: &str = "<!-- CODEGRAPH_END -->";
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
            return edit::toml("", Some(&entry));
        }
        if id == "hermes" {
            return edit::yaml("", Some(&entry));
        }
        let mut object = json!({});
        let mut node = &mut object;
        for key in config_keys(id) {
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
                        .and_then(|m| m.get("codegraph"))
                        .is_some()
                });
        }
        if id == "hermes" {
            return edit::yaml_configured(&text);
        }
        let Ok(Some(mut data)) = jsonc_parser::parse_to_serde_value(&text, &Default::default())
        else {
            return false;
        };
        for key in config_keys(id) {
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
            "codex" => Some(edit::toml(&before, value)?),
            "hermes" => Some(edit::yaml(&before, value)?),
            _ => {
                // opencode: drop the pre-#1698 `mcp.codegraph` entry either way.
                let legacy = if id == "opencode" {
                    edit::json(&before, &["mcp", "codegraph"], None)?
                } else {
                    before.clone()
                };
                match value {
                    Some(value) => Some(edit::json(&legacy, &config_keys(id), Some(value))?),
                    None => remove_json(&before, &legacy, &config_keys(id))?,
                }
            }
        };
        // `None` deletes the file: uninstall left nothing in it.
        let mut planned: Vec<(PathBuf, String, Option<String>)> = vec![(file, before, after)];
        if let Some(file) = self.instructions_path(id, loc) {
            let before = read(&file)?;
            let after = if remove {
                match markers::strip(&before, START, END) {
                    Some(rest) if markers::is_blank(&rest) => None,
                    Some(rest) => Some(rest),
                    None => Some(before.clone()),
                }
            } else {
                let existing = file.exists().then_some(before.as_str());
                Some(markers::upsert(existing, INSTRUCTIONS, START, END))
            };
            planned.push((file, before, after));
        }
        if id == "claude" {
            let settings = self
                .profile(loc, "CLAUDE_CONFIG_DIR", ".claude")
                .join("settings.json");
            let before = read(&settings)?;
            if !before.is_empty() || !remove && options.auto_allow {
                let mut data = jsonc_parser::parse_to_serde_value(
                    if before.is_empty() { "{}" } else { &before },
                    &Default::default(),
                )?
                .unwrap_or(json!({}));
                let mut allow = data["permissions"]["allow"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let prior = allow.clone();
                allow.retain(|v| !v.as_str().is_some_and(|s| s.starts_with("mcp__codegraph")));
                if !remove && options.auto_allow {
                    allow.push(json!("mcp__codegraph__*"));
                }
                if allow != prior {
                    let after = if allow.is_empty() {
                        remove_json(&before, &before, &["permissions", "allow"])?
                    } else {
                        Some(edit::json(&before, &["permissions", "allow"], Some(&json!(allow)))?)
                    };
                    planned.push((settings, before, after));
                }
                data.take();
            }
            if loc == Location::Local {
                let file = self.project.join(".claude.json");
                let before = read(&file)?;
                if !before.is_empty() {
                    let after = remove_json(&before, &before, &["mcpServers", "codegraph"])?;
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
                        let after = remove_json(&before, &before, &["mcpServers", "codegraph"])?;
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
fn config_keys(id: &str) -> Vec<&'static str> {
    match id {
        "opencode" => vec!["mcp", "servers", "codegraph"],
        "copilot-vscode" | "copilot-jetbrains" => vec!["servers", "codegraph"],
        _ => vec!["mcpServers", "codegraph"],
    }
}
#[derive(Debug, Clone, Copy)]
pub struct InstallOptions {
    pub auto_allow: bool,
}
impl Default for InstallOptions {
    fn default() -> Self {
        Self { auto_allow: true }
    }
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
