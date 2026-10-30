//! The viewer re-checks its root on every file it reads (upstream #2929): a root that is, or has
//! become, a link to a credential folder serves nothing.
use codegraph_core::CodeGraph;
use codegraph_ui_server::api;
use std::{fs, path::Path};

fn link_dir(target: &Path, link: &Path) {
    #[cfg(windows)]
    {
        let made = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link).arg(target).output().unwrap().status.success();
        assert!(made, "mklink /J failed");
    }
    #[cfg(not(windows))]
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[test]
fn a_root_that_is_a_link_to_a_sensitive_folder_serves_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let ssh = home.join(".ssh");
    fs::create_dir_all(&ssh).unwrap();
    fs::write(ssh.join("id_ed25519.ts"), "export const key = 'FAKE-KEY';\n").unwrap();
    CodeGraph::init_and_index(&ssh.to_string_lossy(), None).unwrap().close();
    let ordinary = dir.path().join("project");
    fs::create_dir_all(&ordinary).unwrap();
    fs::write(ordinary.join("app.ts"), "export const fine = 1;\n").unwrap();
    CodeGraph::init_and_index(&ordinary.to_string_lossy(), None).unwrap().close();
    let root = dir.path().join("ssh-link");
    link_dir(&ssh, &root);

    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let saved = std::env::var_os(var);
    std::env::set_var(var, &home);
    let through_link = CodeGraph::open(&root.to_string_lossy(), Default::default()).unwrap();
    let refused = api::resolve_file(&through_link, "id_ed25519.ts");
    let control = CodeGraph::open(&ordinary.to_string_lossy(), Default::default()).unwrap();
    let served = api::resolve_file(&control, "app.ts");
    match saved {
        Some(value) => std::env::set_var(var, value),
        None => std::env::remove_var(var),
    }
    through_link.close();
    control.close();
    let _ = fs::remove_dir(&root);

    let error = refused.err().expect("a file under a link to ~/.ssh was served");
    assert_eq!(error.status, 403);
    assert!(error.message.contains("sensitive"), "{}", error.message);
    assert!(served.is_ok(), "an ordinary project still serves its files");
}
