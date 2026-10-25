//! BUG-67: `codegraph uninstall` removes the installed binary unless --keep-cli, like the
//! original. The binary is a copy placed in a throwaway install dir; agent config lives in a
//! throwaway home, so nothing on the machine is touched.
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "codegraph.exe"
    } else {
        "codegraph"
    }
}

/// A copy of the built binary in `<root>/install/`, as install-rust.* lays it out.
fn installed_copy(root: &Path) -> PathBuf {
    let dir = root.join("install");
    std::fs::create_dir_all(&dir).unwrap();
    let copy = dir.join(exe_name());
    std::fs::copy(env!("CARGO_BIN_EXE_codegraph"), &copy).unwrap();
    copy
}

fn uninstall(exe: &Path, home: &Path, extra: &[&str]) -> std::process::Output {
    Command::new(exe)
        .args(["uninstall", "--yes", "--target", "claude"])
        .args(extra)
        .env("HOME", home)
        .env("USERPROFILE", home)
        // The default install location must never be the machine's real one in a test.
        .env("LOCALAPPDATA", home)
        .env("CODEGRAPH_INSTALL_DIR", exe.parent().unwrap())
        .env("CODEGRAPH_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap()
}

fn gone_within(path: &Path, limit: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if !path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    !path.exists()
}

#[test]
fn uninstall_removes_the_installed_binary_unless_keep_cli() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let kept = installed_copy(root.path());
    let out = uninstall(&kept, &home, &["--keep-cli"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(kept.exists(), "--keep-cli must keep the binary");

    // BUG-79: copies an upgrade renamed aside are removed with it (Windows names; harmless elsewhere).
    let dir = kept.parent().unwrap();
    std::fs::write(dir.join(format!("{}.old", exe_name())), "stale").unwrap();
    std::fs::write(dir.join(format!("{}.old-123", exe_name())), "stale").unwrap();
    let out = uninstall(&kept, &home, &[]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Removed the codegraph CLI"), "{stdout}");
    // Windows can't delete a running exe: it is moved aside and deleted once the process exits.
    assert!(gone_within(&kept, Duration::from_secs(15)), "binary still at {}", kept.display());
    let leftovers: Vec<_> = std::fs::read_dir(kept.parent().unwrap()).unwrap().flatten().collect();
    let start = Instant::now();
    let mut remaining = leftovers.len();
    while remaining > 0 && start.elapsed() < Duration::from_secs(15) {
        std::thread::sleep(Duration::from_millis(200));
        remaining = std::fs::read_dir(kept.parent().unwrap()).map(|d| d.count()).unwrap_or(0);
    }
    assert_eq!(remaining, 0, "leftover files in the install dir");
}

#[test]
fn a_cargo_built_binary_is_reported_never_deleted() {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_codegraph"))
        .args(["uninstall", "--yes", "--target", "claude"])
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("LOCALAPPDATA", home.path())
        .env_remove("CODEGRAPH_INSTALL_DIR")
        .env("CODEGRAPH_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(Path::new(env!("CARGO_BIN_EXE_codegraph")).exists());
    assert!(String::from_utf8_lossy(&out.stdout).contains("source checkout"), "{}", String::from_utf8_lossy(&out.stdout));
}
