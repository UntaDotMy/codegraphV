//! `codegraphV migrate`: from the original codegraph (npm `@colbymchenry/codegraph`, or this
//! fork's v0.0.1 `codegraph`) to codegraphV in one step. In order: each agent's configuration
//! (the original's entries replaced by codegraphV's, after a backup), each project index (moved
//! from `.codegraph/` to `.codegraphV/`, never rebuilt), then the original's program and state.
//! Running it again finishes what an earlier run could not (files another process held open)
//! and otherwise changes nothing.
use codegraph_installer::original::{self, ORIGINAL};
use codegraph_installer::{Environment, InstallOptions, Location, TARGETS};
use codegraph_types::paths::{codegraph_dir_name, ORIGINAL_CODEGRAPH_DIR};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub struct Options {
    pub dry_run: bool,
    pub yes: bool,
    /// More project folders whose `.codegraph/` index to move.
    pub paths: Vec<PathBuf>,
}

#[derive(Debug)]
struct Agent {
    id: String,
    loc: Location,
    files: Vec<PathBuf>,
}

#[derive(Debug, PartialEq)]
enum IndexStep {
    /// `.codegraph/` becomes `.codegraphV/`.
    Move,
    /// The project already has `.codegraphV/`: that index stays, the original's goes.
    RemoveOld,
}

#[derive(Debug)]
struct Index {
    root: PathBuf,
    step: IndexStep,
}

#[derive(Debug, Default)]
struct Plan {
    agents: Vec<Agent>,
    indexes: Vec<Index>,
    npm: bool,
    /// The v0.0.1 binary and copies an upgrade left next to it.
    binaries: Vec<PathBuf>,
    /// `~/.codegraph`: the original's settings and caches.
    state_dir: Option<PathBuf>,
    warnings: Vec<String>,
}

impl Plan {
    fn is_empty(&self) -> bool {
        self.agents.is_empty() && self.indexes.is_empty() && !self.npm && self.binaries.is_empty() && self.state_dir.is_none()
    }
}

fn loc_name(loc: Location) -> &'static str {
    if loc == Location::Global {
        "global"
    } else {
        "this project"
    }
}

/// Every target whose configuration names the original, with the files removing it would change.
fn plan_agents(env: &Environment, warnings: &mut Vec<String>) -> Vec<Agent> {
    let mut agents: Vec<Agent> = Vec::new();
    for loc in [Location::Global, Location::Local] {
        for id in TARGETS {
            if !env.supports(id, loc) {
                continue;
            }
            match env.remove_as(&ORIGINAL, id, loc, true) {
                Ok(result) => {
                    let files: Vec<PathBuf> =
                        result.files.into_iter().filter(|f| f.action == "removed").map(|f| f.path).collect();
                    // The project is the home folder: its "local" files are the global ones.
                    let seen = agents.iter().any(|a| a.id == *id && files.iter().all(|f| a.files.contains(f)));
                    if !files.is_empty() && !seen {
                        agents.push(Agent { id: id.to_string(), loc, files });
                    }
                }
                Err(error) => warnings.push(format!("{id} ({}): could not read its configuration: {error}", loc_name(loc))),
            }
        }
    }
    agents
}

fn same_dir_key(path: &Path) -> String {
    let text = codegraph_types::paths::resolve_path(path).to_string_lossy().trim_end_matches(['\\', '/']).to_string();
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

/// The current folder, `--path` folders, and every project Claude Code has opened.
fn candidate_roots(env: &Environment, extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = vec![env.project.clone()];
    roots.extend(extra.iter().cloned());
    if let Ok(config) = env.config_path("claude", Location::Global) {
        if let Ok(Value::Object(data)) = std::fs::read_to_string(config).map(|t| serde_json::from_str(&t).unwrap_or(Value::Null)) {
            if let Some(projects) = data.get("projects").and_then(Value::as_object) {
                roots.extend(projects.keys().map(PathBuf::from));
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    roots.retain(|r| seen.insert(same_dir_key(r)));
    roots
}

fn plan_index(root: &Path) -> Option<Index> {
    if !root.join(ORIGINAL_CODEGRAPH_DIR).join("codegraph.db").is_file() {
        return None;
    }
    let step = if root.join(codegraph_dir_name()).exists() { IndexStep::RemoveOld } else { IndexStep::Move };
    Some(Index { root: root.to_path_buf(), step })
}

fn npm() -> Command {
    // npm is a .cmd script on Windows, which only cmd runs.
    let mut command = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", "npm"]);
        c
    } else {
        Command::new("npm")
    };
    command.stdin(Stdio::null());
    command
}

/// Does `npm ls -g <package> --json` output list the original's package?
pub fn npm_lists_original(json: &str) -> bool {
    serde_json::from_str::<Value>(json).is_ok_and(|v| v["dependencies"][original::NPM_PACKAGE].is_object())
}

fn npm_has_original() -> bool {
    npm()
        .args(["ls", "-g", original::NPM_PACKAGE, "--depth=0", "--json"])
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|out| npm_lists_original(&String::from_utf8_lossy(&out.stdout)))
}

/// The v0.0.1 binary install-rust.ps1 / .sh put in its default location, plus the copies an
/// upgrade or an earlier migrate renamed aside (`codegraph.exe.old…`).
fn original_binaries(env: &Environment) -> Vec<PathBuf> {
    if cfg!(windows) {
        let local = env
            .vars
            .get("LOCALAPPDATA")
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| env.home.join("AppData").join("Local"));
        let dir = local.join(original::WINDOWS_INSTALL_DIR).join("bin");
        let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("codegraph.exe")))
            .collect();
        found.sort();
        found
    } else {
        // ~/.local/bin is shared: only a file that says it is the Rust codegraph.
        let file = env.home.join(".local").join("bin").join(original::BINARY);
        let is_file = std::fs::symlink_metadata(&file).is_ok_and(|m| m.is_file());
        let is_ours = is_file
            && Command::new(&file)
                .arg("--version")
                .stdin(Stdio::null())
                .output()
                .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).starts_with("codegraph 0.0."));
        if is_ours {
            vec![file]
        } else {
            Vec::new()
        }
    }
}

fn make_plan(env: &Environment, options: &Options) -> Plan {
    let mut plan = Plan::default();
    plan.agents = plan_agents(env, &mut plan.warnings);
    plan.indexes = candidate_roots(env, &options.paths).iter().filter_map(|r| plan_index(r)).collect();
    plan.npm = npm_has_original();
    plan.binaries = original_binaries(env);
    let state = env.home.join(original::STATE_DIR);
    // A home folder the original indexed holds an index there, which the index step moves.
    if state.is_dir() && !state.join("codegraph.db").exists() {
        plan.state_dir = Some(state);
    }
    plan
}

fn print_plan(plan: &Plan) {
    if !plan.agents.is_empty() {
        println!("Agents (the original's entries are replaced by codegraphV's):");
        for a in &plan.agents {
            let files: Vec<String> = a.files.iter().map(|f| f.display().to_string()).collect();
            println!("  {} ({}): {}", a.id, loc_name(a.loc), files.join(", "));
        }
    }
    if !plan.indexes.is_empty() {
        println!("Project indexes (kept as they are; nothing is re-indexed):");
        for i in &plan.indexes {
            match i.step {
                IndexStep::Move => println!("  {}: {ORIGINAL_CODEGRAPH_DIR}/ -> {}/", i.root.display(), codegraph_dir_name()),
                IndexStep::RemoveOld => println!(
                    "  {}: remove the original's {ORIGINAL_CODEGRAPH_DIR}/ only after checking that {}/ is a healthy, complete index",
                    i.root.display(),
                    codegraph_dir_name()
                ),
            }
        }
    }
    if plan.npm || !plan.binaries.is_empty() || plan.state_dir.is_some() {
        println!("The original program:");
        if plan.npm {
            println!("  the npm package {} (npm rm -g)", original::NPM_PACKAGE);
        }
        for b in &plan.binaries {
            println!("  {}", b.display());
        }
        if let Some(dir) = &plan.state_dir {
            println!("  {} (its settings and caches)", dir.display());
        }
    }
}

/// Copies of `files` under `~/.codegraphV/migrate-backup/<time>/`, with `index.txt` naming where
/// each came from.
fn backup(files: &[PathBuf], home: &Path) -> std::io::Result<PathBuf> {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let base = home.join(".codegraphV").join("migrate-backup");
    std::fs::create_dir_all(&base)?;
    // Exclusive creation matters even with a high-resolution clock: retries and concurrent
    // invocations must never overwrite the original configuration saved by an earlier run.
    let mut attempt = 0u64;
    let dir = loop {
        let dir = base.join(format!("{stamp}-{}-{attempt}", std::process::id()));
        match std::fs::create_dir(&dir) {
            Ok(()) => break dir,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => attempt += 1,
            Err(e) => return Err(e),
        }
    };
    let mut index = String::new();
    for (n, file) in files.iter().filter(|f| f.is_file()).enumerate() {
        let name = format!("{n}-{}", file.file_name().unwrap_or_default().to_string_lossy());
        std::fs::copy(file, dir.join(&name))?;
        index.push_str(&format!("{name}\t{}\n", file.display()));
    }
    std::fs::write(dir.join("index.txt"), index)?;
    Ok(dir)
}

const IN_USE: &str = "in use: close the agent sessions using it, then run `codegraphV migrate` again";

/// A pre-existing folder can be an interrupted migration, an unindexed database, or an alias
/// of the original. None of those is a safe reason to discard the only working index.
fn check_existing_index(dir: &Path) -> crate::Result<()> {
    if dir.is_symlink() || dir.join("codegraph.db").is_symlink() {
        return Err("the destination is a link, not an independent index".into());
    }
    let store = codegraph_db::Store::open(dir.join("codegraph.db"), codegraph_db::OpenOptions { read_only: true })?;
    let check: String = store.conn().query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
    if check != "ok" {
        return Err(format!("database integrity check failed: {check}").into());
    }
    store.get_stats()?;
    if store.get_metadata("index_state")?.as_deref() != Some("complete") {
        return Err("the destination has no completed index".into());
    }
    Ok(())
}

/// Move one index. Windows cannot rename a folder whose database another process (the
/// original's MCP server) has open: then the database is copied consistently and the old
/// folder removed once nothing holds it.
fn migrate_index(index: &Index) -> Result<String, String> {
    let old = index.root.join(ORIGINAL_CODEGRAPH_DIR);
    let new = index.root.join(codegraph_dir_name());
    if index.step == IndexStep::RemoveOld {
        if let Err(e) = check_existing_index(&new) {
            return Err(format!(
                "kept {}: {} is not a verified replacement ({e}). Both folders were kept; move {} aside and run `codegraphV migrate` again to use the original index.",
                old.display(), new.display(), new.display()
            ));
        }
        return match std::fs::remove_dir_all(&old) {
            Ok(()) => Ok(format!("removed the original's {}", old.display())),
            Err(_) => Err(format!("could not remove {} ({IN_USE})", old.display())),
        };
    }
    if std::fs::rename(&old, &new).is_ok() {
        return Ok(format!("moved {} -> {}", old.display(), new.display()));
    }
    // Own this directory before copying or cleaning up a failed copy. A concurrent migration
    // may have created it since planning; never overwrite or remove that process's index.
    std::fs::create_dir(&new).map_err(|e| format!("could not create {}: {e}; the original index was kept", new.display()))?;
    if let Err(e) = codegraph_db::copy_database(&old.join("codegraph.db"), &new.join("codegraph.db")) {
        let _ = std::fs::remove_dir_all(&new);
        return Err(format!("could not copy {} ({e}); it was left as it was", old.display()));
    }
    let _ = std::fs::copy(old.join(".gitignore"), new.join(".gitignore"));
    match std::fs::remove_dir_all(&old) {
        Ok(()) => Ok(format!("copied {} -> {} (it was in use)", old.display(), new.display())),
        Err(_) => Ok(format!("copied {} -> {}; the old folder is {IN_USE} to remove it", old.display(), new.display())),
    }
}

fn remove_binaries(binaries: &[PathBuf]) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(dir) = binaries.first().and_then(|b| b.parent()).map(Path::to_path_buf) else { return lines };
    if cfg!(windows) {
        // A running exe (an agent's MCP server) cannot be deleted, only renamed aside.
        let mut left = Vec::new();
        for b in binaries {
            if std::fs::remove_file(b).is_err() {
                let aside = b.with_file_name(format!("codegraph.exe.old-migrate-{}", std::process::id()));
                left.push(if std::fs::rename(b, &aside).is_ok() { aside } else { b.clone() });
            }
        }
        if crate::remove_cli::remove_from_user_path(&dir) {
            lines.push(format!("removed {} from your user PATH", dir.display()));
        }
        let dirs: Vec<PathBuf> = std::iter::once(dir.clone()).chain(dir.parent().map(Path::to_path_buf)).collect();
        if left.is_empty() {
            for d in &dirs {
                let _ = std::fs::remove_dir(d);
            }
            lines.push(format!("removed {}", dir.display()));
        } else {
            lines.push(format!("{} is {IN_USE} to remove it", dir.display()));
        }
    } else {
        for b in binaries {
            match std::fs::remove_file(b) {
                Ok(()) => lines.push(format!("removed {}", b.display())),
                Err(e) => lines.push(format!("could not remove {}: {e}", b.display())),
            }
        }
    }
    lines
}

/// Returns the process exit code.
pub fn run(options: Options) -> crate::Result<i32> {
    let env = Environment::current()?;
    let plan = make_plan(&env, &options);
    for w in &plan.warnings {
        eprintln!("Warning: {w}");
    }
    if plan.is_empty() {
        if !plan.warnings.is_empty() {
            println!("Could not inspect all agent configuration. Resolve the warnings and run `codegraphV migrate` again; the original program and settings were kept.");
            return Ok(1);
        }
        println!("Nothing to migrate: no configuration, index or install of the original codegraph was found.");
        return Ok(0);
    }
    println!("codegraphV migrate: from the original codegraph{}", if options.dry_run { " (dry run, nothing is changed)" } else { "" });
    print_plan(&plan);
    if options.dry_run {
        return Ok(0);
    }
    if !options.yes && !crate::confirm("Migrate now? Each config file is backed up first.")? {
        return Ok(0);
    }
    let mut failed = !plan.warnings.is_empty();
    let files: Vec<PathBuf> = plan.agents.iter().flat_map(|a| a.files.iter().cloned()).collect();
    if !files.is_empty() {
        let dir = backup(&files, &env.home)?;
        println!("Backed up {} config file(s) to {}", files.len(), dir.display());
    }
    for agent in &plan.agents {
        // codegraphV keeps the original's tool-permission choice, and Claude gets its prompt hook
        // whether or not the original had one, as `install --yes` does (BUG-95).
        let (permission, _) =
            if agent.id == "claude" { env.claude_settings_as(&ORIGINAL, agent.loc) } else { (true, false) };
        let install = InstallOptions { auto_allow: permission, prompt_hook: Some(true), dry_run: false };
        // Check the replacement before removing the working entry. Parsing failures must not
        // leave an agent disconnected from both programs.
        let step = env.apply(&agent.id, agent.loc, InstallOptions { dry_run: true, ..install }, false)
            .and_then(|_| env.remove_as(&ORIGINAL, &agent.id, agent.loc, false)).and_then(|_| {
            env.apply(
                &agent.id,
                agent.loc,
                install,
                false,
            )
        });
        match step {
            Ok(_) => println!("{} ({}): now runs codegraphV", agent.id, loc_name(agent.loc)),
            Err(e) => {
                failed = true;
                println!("{} ({}): failed: {e}", agent.id, loc_name(agent.loc));
            }
        }
    }
    for index in &plan.indexes {
        match migrate_index(index) {
            Ok(line) => println!("{line}"),
            Err(line) => {
                failed = true;
                println!("{line}");
            }
        }
    }
    if failed {
        println!("Migration is incomplete. The original program and settings were kept. Resolve the warnings or failures above, then run `codegraphV migrate` again.");
        return Ok(1);
    }
    if plan.npm {
        let ok = npm().args(["rm", "-g", original::NPM_PACKAGE]).status().is_ok_and(|s| s.success());
        failed |= !ok;
        println!(
            "{}",
            if ok {
                format!("removed the npm package {}", original::NPM_PACKAGE)
            } else {
                format!("could not remove the npm package; run: npm rm -g {}", original::NPM_PACKAGE)
            }
        );
    }
    for line in remove_binaries(&plan.binaries) {
        println!("{line}");
    }
    if let Some(dir) = &plan.state_dir {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => println!("removed {}", dir.display()),
            Err(e) => {
                failed = true;
                println!("could not remove {}: {e}", dir.display());
            }
        }
    }
    println!("Done. Restart your agents (Claude Code, Cursor, ...) so they start codegraphV.");
    Ok(if failed { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npm_listing_names_the_original_only_when_installed() {
        assert!(npm_lists_original(r#"{"dependencies":{"@colbymchenry/codegraph":{"version":"1.6.1"}}}"#));
        assert!(!npm_lists_original(r#"{"dependencies":{"typescript":{"version":"5.0.0"}}}"#));
        assert!(!npm_lists_original("{}"));
        assert!(!npm_lists_original(""));
    }

    #[test]
    fn an_index_moves_unless_codegraphv_already_has_one() {
        let dir = tempfile::tempdir().unwrap();
        assert!(plan_index(dir.path()).is_none(), "no original index");
        std::fs::create_dir(dir.path().join(".codegraph")).unwrap();
        assert!(plan_index(dir.path()).is_none(), "a folder without a database is not an index");
        std::fs::write(dir.path().join(".codegraph").join("codegraph.db"), "").unwrap();
        assert_eq!(plan_index(dir.path()).unwrap().step, IndexStep::Move);
        std::fs::create_dir(dir.path().join(".codegraphV")).unwrap();
        assert_eq!(plan_index(dir.path()).unwrap().step, IndexStep::RemoveOld);
    }

    #[test]
    fn backup_retries_never_overwrite_the_first_saved_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.json");
        std::fs::write(&file, "original").unwrap();
        let first = backup(&[file.clone()], dir.path()).unwrap();
        std::fs::write(&file, "partially migrated").unwrap();
        let second = backup(&[file], dir.path()).unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read_to_string(first.join("0-config.json")).unwrap(), "original");
        assert_eq!(std::fs::read_to_string(second.join("0-config.json")).unwrap(), "partially migrated");
    }

    #[test]
    fn copy_fallback_preserves_a_destination_created_since_planning() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join(".codegraph");
        let new = dir.path().join(".codegraphV");
        std::fs::create_dir(&old).unwrap();
        std::fs::write(old.join("codegraph.db"), "copy would fail").unwrap();
        let index = plan_index(dir.path()).unwrap();
        assert_eq!(index.step, IndexStep::Move);
        // Another invocation owns this directory by the time the first one starts copying.
        std::fs::create_dir(&new).unwrap();
        std::fs::write(new.join("keep.txt"), "owned by another invocation").unwrap();
        assert!(migrate_index(&index).is_err());
        assert_eq!(std::fs::read_to_string(new.join("keep.txt")).unwrap(), "owned by another invocation");
        assert!(old.join("codegraph.db").exists());
    }
}
