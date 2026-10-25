//! `codegraph upgrade [version] [--check] [--force]` (src/upgrade/index.ts `runUpgrade`, for
//! the native binary): re-runs the release installer pinned to the target tag into the folder
//! the running binary lives in, proves the new version is the one on PATH, then refreshes the
//! agent configuration earlier versions wrote.
use codegraph_installer::upgrade::{
    installer_url, is_update_available, normalize_version, reindex_advisory, resolve_latest_version,
};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// What `upgrade` does, decided from the versions and the options alone.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// --check: report, change nothing.
    Report { update_available: bool },
    /// Already on the target and nothing forces a reinstall.
    UpToDate,
    /// A build from a source checkout (cargo's target dir): the installer must not overwrite it.
    Source { root: PathBuf },
    /// Run the installer for this tag into this folder.
    Install { tag: String, dir: PathBuf },
}

pub fn plan(current: &str, target: &str, pinned: bool, check: bool, force: bool, exe: &Path) -> Plan {
    let update_available = is_update_available(current, target);
    if check {
        return Plan::Report { update_available };
    }
    if !update_available && !force && !pinned {
        return Plan::UpToDate;
    }
    if let Some(root) = source_checkout(exe) {
        return Plan::Source { root };
    }
    Plan::Install {
        tag: normalize_version(target),
        dir: exe.parent().map(Path::to_path_buf).unwrap_or_default(),
    }
}

/// The workspace a cargo-built binary came from: `<root>/target/<profile>/codegraph[.exe]`
/// (or `<root>/target/<triple>/<profile>/…`) with a Cargo.toml at `<root>`.
pub fn source_checkout(exe: &Path) -> Option<PathBuf> {
    exe.ancestors()
        .skip(1)
        .take(3)
        .find(|dir| dir.file_name().is_some_and(|n| n == "target"))
        .and_then(Path::parent)
        .filter(|root| root.join("Cargo.toml").is_file())
        .map(Path::to_path_buf)
}

fn binary_name() -> &'static str {
    if cfg!(windows) {
        "codegraphV.exe"
    } else {
        "codegraphV"
    }
}

/// The version a binary reports (`codegraph 0.0.1` → `v0.0.1`).
fn reported_version(program: &Path) -> Option<String> {
    let out = Command::new(program).arg("--version").stdin(Stdio::null()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let version = text.split_whitespace().last()?;
    Some(normalize_version(version))
}

/// `powershell.exe -NoProfile` for codegraph's own scripts. It gets no PSModulePath, so it builds
/// its own: one inherited from PowerShell 7 makes 5.1 load 7's modules and lose built-in cmdlets
/// such as Get-FileHash and Start-Sleep.
pub fn windows_powershell() -> Command {
    let mut c = Command::new("powershell.exe");
    c.arg("-NoProfile").env_remove("PSModulePath");
    c
}

fn run_installer(tag: &str, dir: &Path) -> std::io::Result<bool> {
    let url = installer_url(cfg!(windows));
    let mut command = if cfg!(windows) {
        let mut c = windows_powershell();
        c.args(["-ExecutionPolicy", "Bypass", "-Command", &format!("irm '{url}' | iex")]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", &format!("curl -fsSL '{url}' | sh")]);
        c
    };
    command.env("CODEGRAPHV_VERSION", tag).env("CODEGRAPHV_INSTALL_DIR", dir);
    Ok(command.status()?.success())
}

/// Returns the process exit code.
pub fn run(version: Option<String>, check: bool, force: bool) -> i32 {
    let current = env!("CARGO_PKG_VERSION");
    let pinned = version.is_some();
    let target = match version.map(|v| normalize_version(&v)).map(Ok).unwrap_or_else(|| resolve_latest_version(12)) {
        Ok(tag) => tag,
        Err(error) => {
            eprintln!("Error: {error}");
            return 1;
        }
    };
    println!(
        "CodeGraphV  current {}  {} {target}",
        normalize_version(current),
        if pinned { "target" } else { "latest" }
    );
    let exe = std::env::current_exe().unwrap_or_default();
    match plan(current, &target, pinned, check, force, &exe) {
        Plan::Report { update_available: true } => {
            println!("An update is available: {} -> {target}", normalize_version(current));
            println!("Run `codegraphV upgrade` to install it.");
            0
        }
        Plan::Report { update_available: false } => {
            println!("You're on the latest version ({}).", normalize_version(current));
            0
        }
        Plan::UpToDate => {
            println!("Already up to date ({}).", normalize_version(current));
            println!("Use `--force` to reinstall, or `codegraphV upgrade <version>` to change versions.");
            0
        }
        Plan::Source { root } => {
            println!("Running from a source checkout at {}.", root.display());
            println!("Upgrade it with: git pull && cargo build --release -p codegraph-cli");
            0
        }
        Plan::Install { tag, dir } => {
            match run_installer(&tag, &dir) {
                Ok(true) => {}
                Ok(false) => {
                    eprintln!("Error: the installer failed; codegraphV was not changed.");
                    return 1;
                }
                Err(error) => {
                    eprintln!("Error: could not run the installer ({error}).");
                    return 1;
                }
            }
            let installed = dir.join(binary_name());
            if reported_version(&installed).as_deref() != Some(tag.as_str()) {
                eprintln!("Error: {} does not report {tag} after the upgrade.", installed.display());
                return 1;
            }
            // Another codegraph earlier on PATH would keep serving the old version.
            let on_path = reported_version(Path::new("codegraphV"));
            if on_path.as_deref().is_some_and(|v| v != tag) {
                println!(
                    "Warning: the `codegraphV` first on your PATH still reports {}; remove that install or put {} first on PATH.",
                    on_path.unwrap_or_default(),
                    dir.display()
                );
                println!("Then run `codegraphV install --refresh` to refresh agent configuration.");
            } else if std::env::var("CODEGRAPHV_NO_INSTALL_REFRESH").as_deref() != Ok("1") {
                println!("Refreshing agent configuration written by previous versions...");
                let refreshed = Command::new(&installed).args(["install", "--refresh"]).status();
                if !refreshed.is_ok_and(|s| s.success()) {
                    println!("Could not refresh agent configuration; run `codegraphV install --refresh`.");
                }
            }
            // The installer renamed the binary this process runs from aside; drop it once we exit.
            crate::remove_cli::delete_now_or_after_exit(&crate::remove_cli::stale_copies(&dir), &[]);
            println!("Upgraded to {tag}.");
            println!("{}", reindex_advisory());
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BUG-73: started from PowerShell 7, the inherited PSModulePath makes Windows PowerShell 5.1
    /// load 7's modules and lose Get-FileHash / Start-Sleep, so the installer script fails.
    #[cfg(windows)]
    #[test]
    fn windows_powershell_gets_its_own_module_path() {
        // The module path a real PowerShell 7 hands its children (only real 7 module folders
        // trigger it). No pwsh on the machine: nothing to reproduce.
        let Ok(ps7) = Command::new("pwsh").args(["-NoProfile", "-Command", "$env:PSModulePath"]).output() else {
            return;
        };
        let ps7 = String::from_utf8_lossy(&ps7.stdout).trim().to_string();
        assert!(ps7.to_lowercase().contains("powershell"), "{ps7}");
        // As when codegraph itself was started from PowerShell 7 (no other test here reads it).
        std::env::set_var("PSModulePath", ps7);
        let out = windows_powershell()
            .args(["-Command", "(Get-Command Get-FileHash).Source; (Get-Command Start-Sleep).Source"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            text.matches("Microsoft.PowerShell.Utility").count(),
            2,
            "{text}{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn plan_check_up_to_date_force_pin_and_source() {
        let installed = Path::new("C:/Users/x/AppData/Local/codegraphV/bin/codegraphV.exe");
        assert_eq!(plan("0.0.1", "v0.0.2", false, true, false, installed), Plan::Report { update_available: true });
        assert_eq!(plan("0.0.2", "v0.0.2", false, true, false, installed), Plan::Report { update_available: false });
        assert_eq!(plan("0.0.2", "v0.0.2", false, false, false, installed), Plan::UpToDate);
        let install = Plan::Install {
            tag: "v0.0.2".into(),
            dir: PathBuf::from("C:/Users/x/AppData/Local/codegraphV/bin"),
        };
        assert_eq!(plan("0.0.2", "v0.0.2", false, false, true, installed), install);
        assert_eq!(plan("0.0.1", "0.0.2", false, false, false, installed), install);
        // A pinned version installs even when it is older.
        assert!(matches!(plan("0.0.2", "v0.0.1", true, false, false, installed), Plan::Install { .. }));

        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("Cargo.toml"), "[workspace]").unwrap();
        let built = repo.path().join("target").join("release").join("codegraphV.exe");
        assert_eq!(
            plan("0.0.1", "v0.0.2", false, false, false, &built),
            Plan::Source { root: repo.path().to_path_buf() }
        );
        let cross = repo.path().join("target").join("x86_64-pc-windows-msvc").join("release").join("codegraphV.exe");
        assert_eq!(source_checkout(&cross).as_deref(), Some(repo.path()));
        assert_eq!(source_checkout(installed), None);
    }
}
