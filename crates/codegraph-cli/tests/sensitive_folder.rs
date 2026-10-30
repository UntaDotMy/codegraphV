//! The sensitive-folder guard at the CLI's entry points (upstream #2929, #3012): `ui` turns a
//! link to a credential folder away before anything opens, and a daemon started by hand for such a
//! folder leaves without taking its locks.
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn link_dir(target: &Path, link: &Path) {
    #[cfg(windows)]
    {
        let made = Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link).arg(target).output().unwrap().status.success();
        assert!(made, "mklink /J failed");
    }
    #[cfg(not(windows))]
    std::os::unix::fs::symlink(target, link).unwrap();
}

fn unlink(link: &Path) {
    let _ = fs::remove_dir(link).or_else(|_| fs::remove_file(link));
}

/// A home folder whose `.config` holds an index.
fn home_with_indexed_config() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let config = home.join(".config");
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("secret.ts"), "export function leakedSecretToken() { return 1; }\n").unwrap();
    codegraph_core::CodeGraph::init_and_index(&config.to_string_lossy(), None).unwrap().close();
    (dir, home, config)
}

fn codegraph(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codegraphV"));
    command.env("HOME", home).env("USERPROFILE", home).env("CODEGRAPHV_NO_UPDATE_CHECK", "1").env("CODEGRAPHV_UI", "1");
    command
}

#[test]
fn ui_refuses_a_link_to_a_sensitive_home_folder() {
    let (dir, home, config) = home_with_indexed_config();
    let unindexed = dir.path().join("home").join("work");
    fs::create_dir_all(&unindexed).unwrap();
    let nested = home.join(".config").join("tool");
    fs::create_dir_all(&nested).unwrap();
    // A link to an unindexed folder inside ~/.config, and one to the indexed ~/.config itself.
    let to_nested = unindexed.join("tool-link");
    let to_config = unindexed.join("config-link");
    link_dir(&nested, &to_nested);
    link_dir(&config, &to_config);
    let mut outcomes = Vec::new();
    for target in [&to_nested, &to_config, &config] {
        // Without the guard `ui` starts serving and never exits, so it gets a deadline.
        let mut child = codegraph(&home)
            .args(["ui", "--no-open"])
            .arg(target)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let left = exits_within(&mut child, Duration::from_secs(20));
        outcomes.push((left, child.wait_with_output().unwrap()));
    }
    unlink(&to_nested);
    unlink(&to_config);
    for (left, output) in outcomes {
        let said = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        assert!(left, "the viewer started for a sensitive folder: {said}");
        assert!(!output.status.success(), "{said}");
        assert!(said.contains("Refusing to operate on sensitive"), "{said}");
    }
}

/// Wait for `child` to exit on its own; kill it if it is still running after `limit`.
fn exits_within(child: &mut Child, limit: Duration) -> bool {
    let started = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            return true;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_hand_started_daemon_for_a_sensitive_folder_exits_without_taking_its_locks() {
    let (_dir, home, config) = home_with_indexed_config();
    let mut child = codegraph(&home)
        .args(["__daemon", "--path"])
        .arg(&config)
        .args(["--idle-timeout", "600"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let left = exits_within(&mut child, Duration::from_secs(20));
    assert!(left, "the daemon kept serving a sensitive folder");
    assert!(child.wait().unwrap().success());
    assert!(!config.join(".codegraphV").join("native-daemon.json").exists(), "no endpoint was published in the folder");
}
