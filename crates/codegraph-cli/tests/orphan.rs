//! BUG-89: an MCP server whose agent is gone exits, even when its stdin stays open (a launcher
//! shim, or a grandchild holding the pipe). The test is the "grandparent": it holds the server's
//! stdin and stdout pipes, while a shell in between starts the server and exits at once.
use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

/// Start `codegraphV serve --mcp` through a shell that exits immediately, so the server's parent
/// dies while this test still holds both of its pipes.
fn orphaned_server(home: &Path, project: &Path, daemon: bool) -> (std::process::ChildStdin, mpsc::Receiver<Option<String>>) {
    let exe = env!("CARGO_BIN_EXE_codegraphV");
    let mut shell = if cfg!(windows) {
        let mut c = Command::new("cmd");
        // `start /b` runs it in this console with the inherited handles; cmd then exits. A raw
        // argument: cmd does not understand the `\"` escapes `arg` would add.
        #[cfg(windows)]
        std::os::windows::process::CommandExt::raw_arg(&mut c, format!("/c start \"\" /b \"{exe}\" serve --mcp"));
        c
    } else {
        let mut c = Command::new("sh");
        // A background job's stdin is /dev/null unless redirected from a saved copy. The shell
        // lives on briefly so the server starts as its child: POSIX signals parent death by
        // reparenting, so a server that starts already reparented has no change to notice (as
        // in the TypeScript watchdog, which also must not treat pid 1 itself as orphaning).
        c.arg("-c").arg(format!("exec 3<&0; '{exe}' serve --mcp <&3 & sleep 2"));
        c
    };
    shell
        .current_dir(project)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("LOCALAPPDATA", home)
        .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
        .env("CODEGRAPHV_PPID_POLL_MS", "200")
        .env("CODEGRAPHV_DAEMON", if daemon { "1" } else { "0" })
        .env_remove("CLAUDE_CONFIG_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = shell.spawn().unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    assert!(child.wait().unwrap().success(), "the launcher shell exits at once");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line.ok()).is_err() {
                return;
            }
        }
        let _ = tx.send(None); // EOF: every holder of the server's stdout has exited
    });
    (stdin, rx)
}

fn exits_after_its_parent(daemon: bool) {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("app.ts"), "export function start() { return 1; }\n").unwrap();
    let init = Command::new(env!("CARGO_BIN_EXE_codegraphV"))
        .args(["init", "."])
        .current_dir(project.path())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    assert!(init.status.success());
    let (mut stdin, lines) = orphaned_server(home.path(), project.path(), daemon);
    // It serves while orphaned: the stdin pipe is still open.
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"t","version":"1"}}}}}}"#).unwrap();
    let first = lines.recv_timeout(Duration::from_secs(20)).expect("an initialize response");
    assert!(first.unwrap_or_default().contains("codegraphV"));
    // ... and then notices its parent is gone and exits, closing its stdout.
    let start = Instant::now();
    loop {
        match lines.recv_timeout(Duration::from_secs(5).saturating_sub(start.elapsed())) {
            Ok(None) => break,
            Ok(Some(_)) => continue,
            Err(_) => panic!("the server outlived its parent by 5 s"),
        }
    }
    drop(stdin);
    if daemon {
        // The shared daemon has no clients left and leaves after its idle timeout (tested in
        // daemon.rs); stop it now so the temp project can be removed.
        let _ = Command::new(env!("CARGO_BIN_EXE_codegraphV"))
            .args(["daemon", "--stop"])
            .current_dir(project.path())
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .output();
    }
}

#[test]
fn direct_server_exits_when_its_parent_dies() {
    exits_after_its_parent(false);
}

#[test]
fn daemon_proxy_exits_when_its_parent_dies() {
    exits_after_its_parent(true);
}
