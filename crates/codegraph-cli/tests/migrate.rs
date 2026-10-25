//! DEV-85: `codegraphV migrate` takes a machine from the original codegraph to codegraphV.
//! Every run uses a throwaway home (and APPDATA, LOCALAPPDATA, XDG config) and a PATH without
//! npm, so the real agent configuration and global packages are never read or written.
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

struct Sandbox {
    home: tempfile::TempDir,
    empty_path: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self { home: tempfile::tempdir().unwrap(), empty_path: tempfile::tempdir().unwrap() }
    }
    fn home(&self) -> &Path {
        self.home.path()
    }
    fn cmd(&self, cwd: &Path) -> Command {
        let home = self.home();
        let mut c = Command::new(env!("CARGO_BIN_EXE_codegraphV"));
        c.current_dir(cwd)
            .stdin(Stdio::null())
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("LOCALAPPDATA", home)
            .env("APPDATA", home.join("AppData").join("Roaming"))
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("PATH", self.empty_path.path())
            .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
            .env("CODEGRAPHV_DAEMON", "0");
        for var in ["CODEGRAPHV_DIR", "CODEGRAPH_DIR", "CLAUDE_CONFIG_DIR", "CODEX_HOME", "HERMES_HOME", "COPILOT_HOME", "CODEGRAPHV_INSTALL_DIR"] {
            c.env_remove(var);
        }
        c
    }
    fn migrate(&self, cwd: &Path, args: &[&str]) -> Output {
        self.cmd(cwd).arg("migrate").args(args).output().unwrap()
    }
    /// A project with an index the original codegraph made: codegraphV indexes it, and the folder
    /// gets the original's name (both engines write the same database).
    fn original_project(&self, dir: &Path) {
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/app.ts"), "export function start() { return 1; }\n").unwrap();
        ok(self.cmd(dir).args(["init", "."]).output().unwrap());
        fs::rename(dir.join(".codegraphV"), dir.join(".codegraph")).unwrap();
    }
    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let path = rel.split('/').fold(self.home().to_path_buf(), |p, part| p.join(part));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        path
    }
    /// What the original's `codegraph install` left: Claude (server entry, permission, prompt
    /// hook, instructions block), Cursor and Codex, each next to the user's own settings.
    fn original_agents(&self, claude_projects: &[&Path]) {
        let projects: serde_json::Map<String, Value> =
            claude_projects.iter().map(|p| (p.to_string_lossy().into_owned(), serde_json::json!({}))).collect();
        let claude = serde_json::json!({
            "theme": "dark",
            "mcpServers": {"codegraph": {"type": "stdio", "command": "codegraph", "args": ["serve", "--mcp"]}, "other": {"command": "x"}},
            "projects": projects,
        });
        self.write(".claude.json", &serde_json::to_string_pretty(&claude).unwrap());
        self.write(".claude/settings.json", r#"{"permissions":{"allow":["Bash(ls)","mcp__codegraph__*"]},"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"codegraph prompt-hook"}]}],"Stop":[{"hooks":[{"type":"command","command":"stop-hook"}]}]}}"#);
        self.write(".claude/CLAUDE.md", "# Mine\n\nKeep this.\n\n<!-- CODEGRAPH_START -->\n## CodeGraph\nold block\n<!-- CODEGRAPH_END -->\n");
        self.write(".cursor/mcp.json", r#"{"mcpServers":{"codegraph":{"command":"codegraph","args":["serve","--mcp","--path","${workspaceFolder}"]},"keep":{"command":"y"}}}"#);
        self.write(".codex/config.toml", "model = \"m\"\n\n[mcp_servers.codegraph]\ncommand = \"codegraph\"\nargs = [\"serve\", \"--mcp\"]\n\n[mcp_servers.sib]\ncommand = \"s\"\n");
    }
    /// The v0.0.1 binary in its install folder (plus a copy an upgrade renamed aside) and the
    /// original's ~/.codegraph state.
    fn original_program(&self) -> Vec<PathBuf> {
        self.write(".codegraph/update-check.json", "{}");
        if cfg!(windows) {
            vec![self.write("codegraph/bin/codegraph.exe", "old"), self.write("codegraph/bin/codegraph.exe.old", "older")]
        } else {
            let bin = self.write(".local/bin/codegraph", "#!/bin/sh\necho 'codegraph 0.0.1'\n");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
            }
            vec![bin]
        }
    }
}

fn ok(out: Output) -> String {
    assert!(out.status.success(), "{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn snapshot(roots: &[&Path]) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    for root in roots {
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    files.insert(path.clone(), fs::read(&path).unwrap());
                }
            }
        }
    }
    files
}

fn hook_commands(settings: &Value) -> Vec<String> {
    settings["hooks"]["UserPromptSubmit"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|g| g["hooks"].as_array().cloned().unwrap_or_default())
        .map(|h| h["command"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// The prompt hook codegraphV writes: this binary's absolute path (BUG-95).
fn our_hook() -> String {
    format!("\"{}\" prompt-hook", env!("CARGO_BIN_EXE_codegraphV").replace('\\', "/"))
}

/// BUG-95: migrate sets up codegraphV's prompt hook for Claude even when the original had none.
#[test]
fn adds_the_prompt_hook_the_original_lacked() {
    let sb = Sandbox::new();
    let cwd = tempfile::tempdir().unwrap();
    sb.write(".claude.json", r#"{"mcpServers":{"codegraph":{"type":"stdio","command":"codegraph","args":["serve","--mcp"]}}}"#);
    sb.write(".claude/settings.json", r#"{"permissions":{"allow":["mcp__codegraph__*"]}}"#);
    ok(sb.migrate(cwd.path(), &["--yes"]));
    let settings = read_json(&sb.home().join(".claude/settings.json"));
    assert_eq!(hook_commands(&settings), vec![our_hook()], "{settings}");
    assert_eq!(settings["permissions"]["allow"], serde_json::json!(["mcp__codegraphV__*"]));
}

#[test]
fn rewires_agents() {
    let sb = Sandbox::new();
    let cwd = tempfile::tempdir().unwrap();
    sb.original_agents(&[]);
    let out = ok(sb.migrate(cwd.path(), &["--yes"]));

    let claude = read_json(&sb.home().join(".claude.json"));
    assert!(claude["mcpServers"]["codegraph"].is_null(), "{claude}");
    assert_eq!(claude["mcpServers"]["codegraphV"]["args"][1], "--mcp", "{claude}");
    assert_eq!(claude["mcpServers"]["other"]["command"], "x");
    assert_eq!(claude["theme"], "dark");
    let settings = read_json(&sb.home().join(".claude/settings.json"));
    assert_eq!(settings["permissions"]["allow"], serde_json::json!(["Bash(ls)", "mcp__codegraphV__*"]));
    assert_eq!(hook_commands(&settings), vec![our_hook()], "the original had the hook on");
    assert_eq!(settings["hooks"]["Stop"][0]["hooks"][0]["command"], "stop-hook");
    let notes = fs::read_to_string(sb.home().join(".claude/CLAUDE.md")).unwrap();
    assert!(notes.contains("Keep this.") && notes.contains("<!-- CODEGRAPHV_START -->"), "{notes}");
    assert!(!notes.contains("<!-- CODEGRAPH_START -->") && !notes.contains("old block"), "{notes}");
    let cursor = read_json(&sb.home().join(".cursor/mcp.json"));
    assert!(cursor["mcpServers"]["codegraph"].is_null() && cursor["mcpServers"]["codegraphV"].is_object(), "{cursor}");
    assert_eq!(cursor["mcpServers"]["keep"]["command"], "y");
    let codex = fs::read_to_string(sb.home().join(".codex/config.toml")).unwrap();
    assert!(codex.contains("[mcp_servers.codegraphV]") && !codex.contains("[mcp_servers.codegraph]"), "{codex}");
    assert!(codex.contains("[mcp_servers.sib]") && codex.contains("model = \"m\""), "{codex}");

    // Every changed file was backed up first, as it was.
    let backups: Vec<PathBuf> = fs::read_dir(sb.home().join(".codegraphV/migrate-backup")).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(backups.len(), 1, "{out}");
    let index = fs::read_to_string(backups[0].join("index.txt")).unwrap();
    assert_eq!(index.lines().count(), 5, "{index}");
    let saved = index.lines().find(|l| l.ends_with("settings.json")).unwrap().split('\t').next().unwrap();
    assert!(fs::read_to_string(backups[0].join(saved)).unwrap().contains("mcp__codegraph__*"));

    // A second run finds nothing and changes nothing.
    let before = snapshot(&[sb.home()]);
    let again = ok(sb.migrate(cwd.path(), &["--yes"]));
    assert!(again.contains("Nothing to migrate"), "{again}");
    assert_eq!(snapshot(&[sb.home()]), before);
}

#[test]
fn moves_indexes() {
    let sb = Sandbox::new();
    let projects = tempfile::tempdir().unwrap();
    let [cwd, known, flagged, both] = ["cwd", "known", "flagged", "both"].map(|n| projects.path().join(n));
    for p in [&cwd, &known, &flagged, &both] {
        sb.original_project(p);
    }
    // `both` already has codegraphV's own index next to the original's.
    ok(sb.cmd(&both).args(["init", "."]).output().unwrap());
    let kept = fs::read(both.join(".codegraphV/codegraph.db")).unwrap();
    let moved = fs::read(known.join(".codegraph/codegraph.db")).unwrap();
    sb.write(".claude.json", &serde_json::json!({"projects": {known.to_string_lossy(): {}}}).to_string());

    let out = ok(sb.migrate(&cwd, &["--yes", "--path", &flagged.to_string_lossy(), "--path", &both.to_string_lossy()]));
    for p in [&cwd, &known, &flagged, &both] {
        assert!(!p.join(".codegraph").exists(), "{}: {out}", p.display());
        let files = ok(sb.cmd(p).args(["files", "--json"]).output().unwrap());
        assert!(files.contains("src/app.ts"), "{}: {files}", p.display());
    }
    assert_eq!(fs::read(known.join(".codegraphV/codegraph.db")).unwrap(), moved, "moved, not rebuilt");
    assert_eq!(fs::read(both.join(".codegraphV/codegraph.db")).unwrap(), kept, "codegraphV's own index is kept");
    let again = ok(sb.migrate(&cwd, &["--yes", "--path", &flagged.to_string_lossy()]));
    assert!(again.contains("Nothing to migrate"), "{again}");
}

#[test]
fn removes_the_original() {
    let sb = Sandbox::new();
    let cwd = tempfile::tempdir().unwrap();
    let binaries = sb.original_program();

    // No --yes and no terminal to ask on: refused, nothing changed.
    let before = snapshot(&[sb.home()]);
    let refused = sb.migrate(cwd.path(), &[]);
    assert!(!refused.status.success(), "{}", String::from_utf8_lossy(&refused.stdout));
    assert_eq!(snapshot(&[sb.home()]), before);

    let out = ok(sb.migrate(cwd.path(), &["--yes"]));
    for b in &binaries {
        assert!(!b.exists(), "{}: {out}", b.display());
    }
    if cfg!(windows) {
        assert!(!sb.home().join("codegraph").exists(), "the v0.0.1 install folder goes too: {out}");
    }
    assert!(!sb.home().join(".codegraph").exists(), "{out}");
    let again = ok(sb.migrate(cwd.path(), &["--yes"]));
    assert!(again.contains("Nothing to migrate"), "{again}");
}

#[test]
fn dry_run() {
    let sb = Sandbox::new();
    let projects = tempfile::tempdir().unwrap();
    let cwd = projects.path().join("cwd");
    sb.original_project(&cwd);
    sb.original_agents(&[]);
    let binaries = sb.original_program();

    let before = snapshot(&[sb.home(), projects.path()]);
    let out = ok(sb.migrate(&cwd, &["--dry-run"]));
    assert_eq!(snapshot(&[sb.home(), projects.path()]), before, "{out}");
    for expected in ["claude (global)", "cursor (global)", "codex (global)", ".codegraph/ -> .codegraphV/", "(its settings and caches)"] {
        assert!(out.contains(expected), "{expected}: {out}");
    }
    for b in &binaries {
        assert!(out.contains(&b.display().to_string()), "{}: {out}", b.display());
    }
}

/// BUG-89: an agent session of the original still has the index open. Windows cannot move the
/// folder then, so migrate copies the database and reports the old folder as in use; once the
/// session is gone, running migrate again removes it.
#[cfg(windows)]
#[test]
fn locked_index() {
    use std::os::windows::fs::OpenOptionsExt;
    let sb = Sandbox::new();
    let projects = tempfile::tempdir().unwrap();
    let cwd = projects.path().join("cwd");
    sb.original_project(&cwd);
    // Open the way SQLite does: other readers and writers allowed, deleting and renaming not.
    let held = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0x1 | 0x2)
        .open(cwd.join(".codegraph").join("codegraph.db"))
        .unwrap();
    let out = ok(sb.migrate(&cwd, &["--yes"]));
    assert!(out.contains("in use"), "{out}");
    let files = ok(sb.cmd(&cwd).args(["files", "--json"]).output().unwrap());
    assert!(files.contains("src/app.ts"), "the copy is a working index: {files}");
    assert!(cwd.join(".codegraph").exists());
    drop(held);
    let again = ok(sb.migrate(&cwd, &["--yes"]));
    assert!(!cwd.join(".codegraph").exists(), "{again}");
    assert!(ok(sb.cmd(&cwd).args(["files", "--json"]).output().unwrap()).contains("src/app.ts"));
}
