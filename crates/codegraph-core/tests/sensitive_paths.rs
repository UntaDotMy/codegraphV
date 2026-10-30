//! The sensitive-folder guard (upstream #2929, #3012): a link, a case variant or a Win32 namespace
//! spelling of a credential or system folder is refused like the folder itself, and a refused
//! default project resolves to no root at all.
use codegraph_core::{
    directory::resolve_server_root,
    utils::{sensitive_directory_refusal, validate_project_path},
    CodeGraph,
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

/// The home folder is process state: tests in this binary take turns with it.
static HOME: Mutex<()> = Mutex::new(());

fn with_home<T>(home: &Path, f: impl FnOnce() -> T) -> T {
    let _turn = HOME.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let saved = std::env::var_os(var);
    std::env::set_var(var, home);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    match saved {
        Some(value) => std::env::set_var(var, value),
        None => std::env::remove_var(var),
    }
    outcome.unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// A directory link `link` → `target`: a junction on Windows (no privilege needed), a symlink
/// elsewhere.
fn link_dir(target: &Path, link: &Path) {
    #[cfg(windows)]
    {
        let made = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link).arg(target).output().unwrap().status.success();
        assert!(made, "mklink /J failed");
    }
    #[cfg(not(windows))]
    std::os::unix::fs::symlink(target, link).unwrap();
}

fn unlink(link: &Path) {
    let _ = fs::remove_dir(link).or_else(|_| fs::remove_file(link));
}

struct Fixture {
    _dir: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
}

/// A home folder holding the credential folders and an ordinary project.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    for sensitive in [".ssh", ".config/tool"] {
        fs::create_dir_all(home.join(sensitive)).unwrap();
    }
    fs::write(home.join(".ssh").join("id_ed25519"), "FAKE-KEY\n").unwrap();
    let project = home.join("work").join("project");
    fs::create_dir_all(&project).unwrap();
    Fixture { _dir: dir, home, project }
}

fn s(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn refused(path: &Path) -> String {
    validate_project_path(&s(path)).unwrap_or_else(|| panic!("{} was accepted", path.display()))
}

#[test]
fn a_link_to_a_sensitive_home_folder_is_refused_naming_the_folder() {
    let f = fixture();
    let ssh_link = f.home.join("work").join("ssh-link");
    let nested_link = f.home.join("work").join("tool-link");
    link_dir(&f.home.join(".ssh"), &ssh_link);
    link_dir(&f.home.join(".config").join("tool"), &nested_link);
    let (ssh, nested, ordinary, home) = with_home(&f.home, || {
        (refused(&ssh_link), refused(&nested_link), validate_project_path(&s(&f.project)), validate_project_path(&s(&f.home)))
    });
    unlink(&ssh_link);
    unlink(&nested_link);
    assert!(ssh.starts_with("Refusing to operate on sensitive directory:"), "{ssh}");
    assert!(ssh.contains("resolves to"), "the folder behind the link is named: {ssh}");
    assert!(nested.contains("resolves to"), "{nested}");
    assert_eq!(ordinary, None, "an ordinary project stays accepted");
    assert_eq!(home, None, "the home folder itself stays accepted");
}

#[test]
fn a_link_to_a_system_folder_is_refused() {
    let f = fixture();
    let system = if cfg!(windows) { PathBuf::from(r"C:\Windows") } else { PathBuf::from("/etc") };
    let link = f.home.join("work").join("system-link");
    link_dir(&system, &link);
    let reason = with_home(&f.home, || refused(&link));
    unlink(&link);
    assert!(reason.starts_with("Refusing to operate on sensitive system directory:"), "{reason}");
}

#[cfg(any(windows, target_os = "macos"))]
#[test]
fn a_case_variant_of_a_sensitive_folder_is_refused_where_case_is_ignored() {
    let f = fixture();
    with_home(&f.home, || {
        refused(&f.home.join(".SSH"));
        refused(&f.home.join(".Config").join("tool"));
        assert_eq!(validate_project_path(&s(&f.home.join("WORK").join("PROJECT"))), None);
    });
}

#[cfg(windows)]
#[test]
fn win32_namespace_spellings_are_refused() {
    let f = fixture();
    with_home(&f.home, || {
        for prefix in [r"\\?\", r"\\.\"] {
            for sensitive in [f.home.join(".ssh"), f.home.join(".config").join("tool"), PathBuf::from(r"C:\Windows")] {
                let spelled = format!("{prefix}{}", sensitive.display());
                assert!(validate_project_path(&spelled).is_some(), "{spelled} was accepted");
            }
            let ordinary = format!("{prefix}{}", f.project.display());
            assert_eq!(validate_project_path(&ordinary), None, "{ordinary} is an ordinary project");
        }
    });
}

#[test]
fn sensitive_directory_refusal_vets_a_path_that_does_not_exist() {
    let f = fixture();
    with_home(&f.home, || {
        let below = f.home.join(".config").join("no-such-dir");
        assert!(sensitive_directory_refusal(&s(&below)).is_some());
        assert!(validate_project_path(&s(&below)).is_some());
        let ordinary = f.home.join("work").join("not-yet");
        assert_eq!(sensitive_directory_refusal(&s(&ordinary)), None);
    });
}

/// Index a project in `dir` (which may be a sensitive folder) so `resolve_server_root` finds it.
fn index_in(dir: &Path) {
    fs::write(dir.join("a.ts"), "export function alpha() { return 1; }\n").unwrap();
    CodeGraph::init_and_index(&s(dir), None).unwrap().close();
}

#[test]
fn a_default_project_in_a_sensitive_folder_resolves_to_no_root() {
    let f = fixture();
    let config = f.home.join(".config");
    index_in(&config);
    let link = f.home.join("work").join("config-link");
    link_dir(&config, &link);
    let (start, missing, linked, nested) = with_home(&f.home, || {
        (
            resolve_server_root(&s(&config), true),
            // A missing folder skips the start check, then walks up to the index in ~/.config.
            resolve_server_root(&s(&config.join("no-such-dir")), true),
            resolve_server_root(&s(&link), true),
            resolve_server_root(&s(&config.join("tool")), true),
        )
    });
    unlink(&link);
    for (name, resolution) in [("start", start), ("missing", missing), ("linked", linked), ("nested", nested)] {
        assert_eq!(resolution.root, None, "{name}: a sensitive folder is not served");
        assert!(resolution.refusal.is_some(), "{name}: the reason is carried");
        assert!(resolution.candidates.is_empty(), "{name}");
    }
}

#[test]
fn no_sub_project_below_a_sensitive_folder_is_listed() {
    let f = fixture();
    let workspace = f.home.join(".config").join("ws");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("package.json"), "{}").unwrap();
    for name in ["one", "two"] {
        let sub = workspace.join(name);
        fs::create_dir_all(&sub).unwrap();
        index_in(&sub);
    }
    let resolution = with_home(&f.home, || resolve_server_root(&s(&workspace), true));
    assert_eq!(resolution.root, None);
    assert!(resolution.candidates.is_empty(), "{:?}", resolution.candidates);
}

#[test]
fn an_ordinary_project_resolves_as_before() {
    let f = fixture();
    index_in(&f.project);
    let nested = f.project.join("src");
    fs::create_dir_all(&nested).unwrap();
    let (up, from_nested) = with_home(&f.home, || (resolve_server_root(&s(&f.project), true), resolve_server_root(&s(&nested), true)));
    for resolution in [up, from_nested] {
        assert_eq!(resolution.root.as_deref().map(|r| r.to_lowercase()), Some(s(&f.project).to_lowercase()));
        assert_eq!(resolution.refusal, None);
    }
}
