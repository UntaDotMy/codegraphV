//! A default project in a sensitive folder (upstream #3012): the session has no default project,
//! nothing there is opened, and a call that names the folder is still refused.
use codegraph_core::CodeGraph;
use codegraph_mcp::{
    session::Session,
    tools::{INSTRUCTIONS, NO_INDEX_INSTRUCTIONS},
};
use serde_json::{json, Value};
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
    /// `~/.config`, holding an index of a file whose text must never be served.
    config: PathBuf,
    project: PathBuf,
}

fn index_in(dir: &Path) {
    fs::write(dir.join("secret.ts"), "export function leakedSecretToken() { return 'FAKE-TOKEN'; }\n").unwrap();
    CodeGraph::init_and_index(&dir.to_string_lossy(), None).unwrap().close();
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let config = home.join(".config");
    let project = home.join("work").join("project");
    fs::create_dir_all(&config).unwrap();
    fs::create_dir_all(&project).unwrap();
    index_in(&config);
    fs::write(project.join("main.ts"), "export function ordinaryEntry() { return 1; }\n").unwrap();
    CodeGraph::init_and_index(&project.to_string_lossy(), None).unwrap().close();
    Fixture { _dir: dir, home, config, project }
}

fn request(session: &mut Session, id: u32, method: &str, params: Value) -> Value {
    session.process(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string()).into_iter().next().unwrap()
}

fn text(response: &Value) -> String {
    response["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string()
}

fn explore(session: &mut Session, id: u32, query: &str, project_path: Option<&Path>) -> Value {
    let mut arguments = json!({"query": query});
    if let Some(path) = project_path {
        arguments["projectPath"] = json!(path.to_string_lossy());
    }
    request(session, id, "tools/call", json!({"name":"codegraphV_explore","arguments":arguments}))
}

/// What the session says once initialized: whether it counts as having a default project, and
/// how its tools read.
fn handshake(session: &mut Session, params: Value) -> (String, bool) {
    let initialized = request(session, 1, "initialize", params);
    let instructions = initialized["result"]["instructions"].as_str().unwrap().to_string();
    let tools = request(session, 2, "tools/list", json!({}));
    let project_path_required = tools["result"]["tools"][0]["inputSchema"]["required"].as_array().unwrap().iter().any(|name| name == "projectPath");
    (instructions, project_path_required)
}

fn assert_no_default_project(session: &mut Session, f: &Fixture, how: &str) {
    let (_, project_path_required) = handshake(session, json!({}));
    assert!(project_path_required, "{how}: the tools ask for projectPath");
    let guidance = explore(session, 3, "leakedSecretToken", None);
    assert_ne!(guidance["result"]["isError"], true, "{how}: a missing default is guidance, not an error");
    let guidance = text(&guidance);
    assert!(guidance.contains("sensitive"), "{how}: the reason is said: {guidance}");
    assert!(guidance.contains("projectPath"), "{how}: {guidance}");
    assert!(!guidance.contains("FAKE-TOKEN") && !guidance.contains("leakedSecretToken()"), "{how}: nothing was read: {guidance}");
    let named = explore(session, 4, "leakedSecretToken", Some(&f.config));
    assert_eq!(named["result"]["isError"], true, "{how}: naming the folder is still refused");
    assert!(text(&named).contains("Refusing"), "{how}: {}", text(&named));
    assert_eq!(session.handler.open_project_count(), 0, "{how}: nothing was opened");
}

#[test]
fn a_default_path_in_a_sensitive_folder_gives_no_default_project() {
    let f = fixture();
    with_home(&f.home, || {
        let mut session = Session::new(Some(f.config.to_string_lossy().into_owned()), false);
        let (instructions, _) = handshake(&mut session, json!({}));
        assert!(instructions.starts_with(NO_INDEX_INSTRUCTIONS), "the no-default-project instructions are sent");
        assert!(!instructions.starts_with(INSTRUCTIONS));
        assert_no_default_project(&mut session, &f, "--path");
    });
}

#[test]
fn a_root_uri_in_a_sensitive_folder_gives_no_default_project() {
    let f = fixture();
    with_home(&f.home, || {
        let mut session = Session::new(None, false);
        let uri = url::Url::from_directory_path(&f.config).unwrap();
        let (instructions, project_path_required) = handshake(&mut session, json!({"rootUri": uri.as_str()}));
        assert!(instructions.starts_with(NO_INDEX_INSTRUCTIONS));
        assert!(project_path_required);
        let guidance = explore(&mut session, 3, "leakedSecretToken", None);
        assert_ne!(guidance["result"]["isError"], true);
        assert!(!text(&guidance).contains("FAKE-TOKEN"));
        assert_eq!(session.handler.open_project_count(), 0);
    });
}

#[test]
fn a_roots_list_answer_in_a_sensitive_folder_gives_no_default_project() {
    let f = fixture();
    with_home(&f.home, || {
        let mut session = Session::new(None, false);
        let messages = session.process(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{"roots":{}}}}).to_string());
        assert_eq!(messages[1]["method"], "roots/list");
        let uri = url::Url::from_directory_path(&f.config).unwrap();
        session.process(&json!({"jsonrpc":"2.0","id":"codegraph-roots","result":{"roots":[{"uri":uri.as_str()}]}}).to_string());
        assert_no_default_project(&mut session, &f, "roots/list");
    });
}

#[test]
fn a_link_to_a_sensitive_folder_as_the_default_gives_no_default_project() {
    let f = fixture();
    let link = f.home.join("work").join("config-link");
    link_dir(&f.config, &link);
    with_home(&f.home, || {
        let mut session = Session::new(Some(link.to_string_lossy().into_owned()), false);
        assert_no_default_project(&mut session, &f, "link");
    });
    unlink(&link);
}

#[test]
fn an_ordinary_default_project_still_answers() {
    let f = fixture();
    with_home(&f.home, || {
        let mut session = Session::new(Some(f.project.to_string_lossy().into_owned()), false);
        let (instructions, project_path_required) = handshake(&mut session, json!({}));
        assert!(instructions.starts_with(INSTRUCTIONS));
        assert!(!project_path_required);
        let answer = explore(&mut session, 3, "ordinaryEntry", None);
        assert_ne!(answer["result"]["isError"], true);
        assert!(text(&answer).contains("ordinaryEntry"), "{}", text(&answer));
        assert_eq!(session.handler.open_project_count(), 1);
    });
}
