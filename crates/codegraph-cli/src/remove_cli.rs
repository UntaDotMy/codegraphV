//! Removing the CLI on `codegraph uninstall` (src/upgrade/remove-binary.ts, for the native
//! binary): the running `codegraph` when it is an installed copy, plus the copies the installer
//! scripts put in their install locations, and the user-PATH entry install-rust.ps1 added.
//!
//! Safety: a cargo-built binary (a source checkout) is reported, never deleted; only files named
//! like the binary in an install location are candidates; a shared folder (~/.local/bin, a custom
//! CODEGRAPH_INSTALL_DIR) is never removed, only the dedicated %LOCALAPPDATA%\codegraph one when
//! it is left empty. Windows cannot delete a running exe but can rename it: the running copy is
//! moved aside and deleted by a helper once this process has exited.
use crate::upgrade::source_checkout;
use std::path::{Path, PathBuf};

pub fn exe_name() -> &'static str {
    if cfg!(windows) {
        "codegraph.exe"
    } else {
        "codegraph"
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// Installed binaries to delete.
    pub binaries: Vec<PathBuf>,
    /// The running binary is a cargo build in this checkout: reported, not deleted.
    pub source_root: Option<PathBuf>,
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Every installed copy: the running one (unless cargo-built), then CODEGRAPH_INSTALL_DIR and the
/// installers' default location.
pub fn plan(exe: &Path, env: &dyn Fn(&str) -> Option<String>, home: &Path) -> Plan {
    let mut plan = Plan::default();
    let mut candidates: Vec<PathBuf> = Vec::new();
    match source_checkout(exe) {
        Some(root) => plan.source_root = Some(root),
        None if exe.file_name().is_some_and(|n| n == exe_name()) => candidates.push(exe.to_path_buf()),
        None => {}
    }
    if let Some(dir) = env("CODEGRAPH_INSTALL_DIR").filter(|d| !d.trim().is_empty()) {
        candidates.push(Path::new(&dir).join(exe_name()));
    }
    candidates.push(default_install_dir(env, home).join(exe_name()));
    for candidate in candidates {
        if candidate.is_file()
            && source_checkout(&candidate).is_none()
            && !plan.binaries.iter().any(|b| same_file(b, &candidate))
        {
            plan.binaries.push(candidate);
        }
    }
    plan
}

/// Where install-rust.ps1 / install-rust.sh put the binary by default.
pub fn default_install_dir(env: &dyn Fn(&str) -> Option<String>, home: &Path) -> PathBuf {
    if cfg!(windows) {
        let local = env("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| home.join("AppData").join("Local"));
        local.join("codegraph").join("bin")
    } else {
        home.join(".local").join("bin")
    }
}

/// A `;`-separated PATH value without `dir` (case-insensitive, trailing separators ignored), or
/// None when `dir` is not on it.
pub fn path_without(value: &str, dir: &str) -> Option<String> {
    let norm = |s: &str| s.trim().trim_end_matches(['\\', '/']).to_lowercase();
    let want = norm(dir);
    let parts: Vec<&str> = value.split(';').collect();
    let kept: Vec<&str> = parts.iter().copied().filter(|p| norm(p) != want).collect();
    (kept.len() != parts.len()).then(|| kept.join(";"))
}

#[cfg(windows)]
fn powershell(script: &str) -> Option<String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim_end_matches(['\r', '\n']).to_string())
}

fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Drop `dir` from the user PATH install-rust.ps1 added it to. Windows only; the shell
/// installer never edits rc files, so there is nothing to undo elsewhere.
fn remove_from_user_path(dir: &Path) -> bool {
    #[cfg(windows)]
    {
        let Some(current) = powershell("[Environment]::GetEnvironmentVariable('Path','User')") else { return false };
        if let Some(next) = path_without(&current, &dir.to_string_lossy()) {
            return powershell(&format!("[Environment]::SetEnvironmentVariable('Path',{},'User')", ps_quote(&next))).is_some();
        }
        false
    }
    #[cfg(not(windows))]
    {
        let _ = dir;
        false
    }
}

/// Delete the running binary after this process exits (Windows): rename it aside now, then a
/// hidden helper waits for this PID and removes the file, and the dedicated folder if empty.
#[cfg(windows)]
fn delete_running(exe: &Path, dedicated_dir: Option<&Path>) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    let aside = exe.with_file_name(format!("{}.old-{}", exe_name(), std::process::id()));
    std::fs::rename(exe, &aside)?;
    let mut script = format!(
        "Wait-Process -Id {} -ErrorAction SilentlyContinue; for ($i = 0; $i -lt 50 -and (Test-Path -LiteralPath {a}); $i++) {{ Remove-Item -LiteralPath {a} -Force -ErrorAction SilentlyContinue; Start-Sleep -Milliseconds 200 }}",
        std::process::id(),
        a = ps_quote(&aside.to_string_lossy())
    );
    if let Some(dir) = dedicated_dir {
        for d in [dir, dir.parent().unwrap_or(dir)] {
            script.push_str(&format!("; try {{ [IO.Directory]::Delete({}) }} catch {{}}", ps_quote(&d.to_string_lossy())));
        }
    }
    std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP: hidden, and a Ctrl+C in this console does not
        // reach it. Not DETACHED_PROCESS — PowerShell without a console never starts.
        .creation_flags(0x0800_0000 | 0x0000_0200)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

/// Carry out `plan`, printing one line per removal. Returns the paths left behind.
pub fn execute(plan: &Plan, exe: &Path, env: &dyn Fn(&str) -> Option<String>, home: &Path) -> Vec<PathBuf> {
    let dedicated = default_install_dir(env, home);
    let mut leftovers = Vec::new();
    for binary in &plan.binaries {
        let dir = binary.parent().unwrap_or(Path::new("")).to_path_buf();
        let is_dedicated = cfg!(windows) && same_file(&dir, &dedicated);
        let running = same_file(binary, exe);
        #[cfg(windows)]
        let removed = if running {
            delete_running(binary, is_dedicated.then_some(dir.as_path())).is_ok()
        } else {
            std::fs::remove_file(binary).is_ok()
        };
        #[cfg(not(windows))]
        let removed = {
            let _ = running;
            std::fs::remove_file(binary).is_ok()
        };
        if removed {
            println!("Removed the codegraph CLI: {}", binary.display());
            if remove_from_user_path(&dir) {
                println!("Removed {} from your user PATH.", dir.display());
            }
            if is_dedicated && !running {
                let _ = std::fs::remove_dir(&dir);
                let _ = dir.parent().map(std::fs::remove_dir);
            }
        } else {
            println!(
                "Could not remove {} (is a codegraph process still running it?). Close your agent sessions and delete it.",
                binary.display()
            );
            leftovers.push(binary.clone());
        }
    }
    if let Some(root) = &plan.source_root {
        println!(
            "This codegraph runs from a source checkout at {}; it was not removed.",
            root.display()
        );
    }
    leftovers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_path_entry_is_dropped_case_insensitively() {
        assert_eq!(
            path_without(r"C:\a;C:\Users\X\AppData\Local\codegraph\bin\;D:\b", r"c:\users\x\appdata\local\codegraph\bin").as_deref(),
            Some(r"C:\a;D:\b")
        );
        assert_eq!(path_without(r"C:\a;D:\b", r"C:\codegraph\bin"), None);
    }

    #[test]
    fn plan_takes_installed_copies_and_reports_a_source_build() {
        let root = tempfile::tempdir().unwrap();
        let install = root.path().join("install");
        std::fs::create_dir_all(&install).unwrap();
        let copy = install.join(exe_name());
        std::fs::write(&copy, "bin").unwrap();
        let home = root.path().join("home");
        let vars = |name: &str| match name {
            "CODEGRAPH_INSTALL_DIR" => Some(install.to_string_lossy().into_owned()),
            "LOCALAPPDATA" => Some(home.to_string_lossy().into_owned()),
            _ => None,
        };
        let p = plan(&copy, &vars, &home);
        assert_eq!(p.binaries, vec![copy.clone()], "deduplicated");
        assert_eq!(p.source_root, None);

        let repo = root.path().join("repo");
        std::fs::create_dir_all(repo.join("target").join("release")).unwrap();
        std::fs::write(repo.join("Cargo.toml"), "[workspace]").unwrap();
        let built = repo.join("target").join("release").join(exe_name());
        std::fs::write(&built, "bin").unwrap();
        let none = |_: &str| None;
        let p = plan(&built, &none, &home);
        assert_eq!(p.source_root.as_deref(), Some(repo.as_path()));
        assert!(p.binaries.is_empty());
    }
}
