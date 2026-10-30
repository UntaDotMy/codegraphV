//! CodeGraph utilities — the security helpers and the cross-process lock of `src/utils.ts`.
//!
//! (`Mutex`, `processInBatches`, `debounce` and the memory monitor exist for the JavaScript event
//! loop; Rust has `std::sync` and plain loops for them.)

use codegraph_types::{
    nodepath,
    paths::{home_dir, is_same_or_under},
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

/// Sensitive system directories that should never be used as project roots. Checked on all
/// platforms; non-applicable paths are harmlessly skipped.
const SENSITIVE_PATHS: &[&str] = &[
    "/", "/etc", "/usr", "/bin", "/sbin", "/var", "/tmp", "/dev", "/proc", "/sys", "/root", "/boot", "/lib", "/lib64", "/opt", "c:\\",
    "c:\\windows", "c:\\windows\\system32",
];

/// Config "languages" whose nodes are pure key/value DATA lifted from a config file (e.g. Spring
/// `application.{yml,properties}`), not source code.
pub const CONFIG_LEAF_LANGUAGES: &[&str] = &["yaml", "properties"];

/// A config-leaf node is a single key lifted out of a pure config/data file — `kind: 'constant'`
/// in a [`CONFIG_LEAF_LANGUAGES`] language. Its on-disk line is `key = <value>`, and that value is
/// routinely a secret (DB password, API key, JDBC URL with embedded creds). CodeGraph must surface
/// the KEY only and never read/return the value, or it pushes secrets into agent context unbidden.
/// (#383)
pub fn is_config_leaf_node(kind: &str, language: Option<&str>) -> bool {
    kind == "constant" && language.is_some_and(|l| CONFIG_LEAF_LANGUAGES.contains(&l))
}

pub use codegraph_types::paths::{is_missing_path_error, real_path, validate_path_within_root, ValidateOptions};

/// Validate that a path is a safe project root directory.
///
/// Rejects sensitive system directories and ensures the path is a real, existing directory. Used
/// at MCP and API entry points to prevent arbitrary directory access. Returns an error message if
/// invalid, or `None` if valid.
pub fn validate_project_path(dir_path: &str) -> Option<String> {
    if let Some(refusal) = sensitive_directory_refusal(dir_path) {
        return Some(refusal);
    }

    // Verify it's a real directory.
    let resolved = nodepath::resolve(&[dir_path]);
    match std::fs::metadata(&resolved) {
        Ok(m) if !m.is_dir() => Some(format!("Path is not a directory: {resolved}")),
        Ok(_) => None,
        Err(_) => Some(format!("Path does not exist or is not accessible: {resolved}")),
    }
}

/// The sensitive-folder half of [`validate_project_path`]: why `dir_path` is, or is inside, a
/// folder CodeGraph never uses as a project, or `None`. Unlike that function it doesn't require
/// the path to exist or be a directory, so it can vet a path a project root is searched for from.
///
/// The path is checked both as spelled and as the folder it really names, so a link to `~/.ssh`,
/// a case variant (`~/.SSH`) or a `\\?\` spelling is refused like the folder itself (#1930). It is
/// kept to one real-path lookup per call, since the viewer calls this for every file it reads:
/// the sensitive folders' own real paths are cached and re-read every couple of seconds.
pub fn sensitive_directory_refusal(dir_path: &str) -> Option<String> {
    let resolved = nodepath::resolve(&[dir_path]);
    let lexical = nodepath::resolve(&[&strip_win32_namespace_prefix(dir_path)]);
    let mut candidates = vec![lexical.clone()];
    if let Some(real) = native_real_path(&lexical).filter(|real| *real != lexical) {
        candidates.push(real);
    }
    let named = |candidate: &str| if candidate == resolved { resolved.clone() } else { format!("{resolved} (resolves to {candidate})") };

    // Block sensitive system directories.
    let system_paths = sensitive_system_paths();
    for candidate in &candidates {
        if SENSITIVE_PATHS.contains(&candidate.as_str())
            || SENSITIVE_PATHS.contains(&candidate.to_lowercase().as_str())
            || system_paths.contains(&fold_path_case(candidate))
        {
            return Some(format!("Refusing to operate on sensitive system directory: {}", named(candidate)));
        }
    }

    // Also block common sensitive home subdirectories.
    let home_trees = sensitive_home_trees();
    for candidate in &candidates {
        let folded = fold_path_case(candidate);
        if home_trees.iter().any(|tree| is_same_or_under(&folded, tree)) {
            return Some(format!("Refusing to operate on sensitive directory: {}", named(candidate)));
        }
    }

    None
}

/// Credential and config folders in the home directory. Unlike the system set, everything under
/// them is refused too.
const SENSITIVE_HOME_DIRS: [&str; 4] = [".ssh", ".gnupg", ".aws", ".config"];

/// How long the real paths of the sensitive home folders are reused. The viewer re-checks its root
/// on every file it reads, so they are not re-read per check, but a folder linked somewhere new
/// while the process runs is noticed soon.
const SENSITIVE_HOME_TTL: Duration = Duration::from_secs(2);

struct SensitiveHome {
    home: PathBuf,
    expires: Instant,
    trees: Vec<String>,
}

static SENSITIVE_HOME_CACHE: Mutex<Option<SensitiveHome>> = Mutex::new(None);
static SENSITIVE_SYSTEM_REAL_PATHS: OnceLock<HashSet<String>> = OnceLock::new();

/// A path in the form the sensitive-folder check compares: lower-cased where the file system
/// ignores case by default (NTFS, APFS), so `~/.SSH` is `~/.ssh`.
fn fold_path_case(p: &str) -> String {
    if cfg!(any(windows, target_os = "macos")) {
        p.to_lowercase()
    } else {
        p.to_string()
    }
}

/// The plain spelling of a Win32 namespace path: `\\?\C:\x` and `\\.\C:\x` are `C:\x`, and
/// `\\?\UNC\host\share` is `\\host\share`. Path resolution keeps the prefix, so a string
/// comparison would never see the folder behind it. Other namespace paths (a `\\?\Volume{…}\`
/// name, a device) are returned unchanged; the real path the caller also checks names their
/// folder.
fn strip_win32_namespace_prefix(p: &str) -> String {
    if !cfg!(windows) {
        return p.to_string();
    }
    let bytes = p.as_bytes();
    let separator = |b: u8| b == b'\\' || b == b'/';
    if bytes.len() < 4 || !separator(bytes[0]) || !separator(bytes[1]) || !matches!(bytes[2], b'?' | b'.') || !separator(bytes[3]) {
        return p.to_string();
    }
    let rest = &p[4..];
    let rest_bytes = rest.as_bytes();
    if rest_bytes.len() >= 4 && rest_bytes[..3].eq_ignore_ascii_case(b"UNC") && separator(rest_bytes[3]) {
        return format!(r"\\{}", &rest[4..]);
    }
    if rest_bytes.len() >= 3 && rest_bytes[0].is_ascii_alphabetic() && rest_bytes[1] == b':' && separator(rest_bytes[2]) {
        return rest.to_string();
    }
    p.to_string()
}

/// The folder a path really names: links (symlinks, junctions, a `subst` drive) followed, and the
/// case and long names stored on disk. `None` when it can't be resolved, most often because it
/// doesn't exist; the lexical check still runs.
fn native_real_path(p: &str) -> Option<String> {
    real_path(Path::new(p)).ok().map(|real| real.to_string_lossy().into_owned())
}

/// The system set's own real paths, folded. macOS keeps `/etc`, `/tmp` and `/var` under
/// `/private`, so a project whose real path is `/private/etc` is `/etc`. Read once: these folders
/// don't move.
fn sensitive_system_paths() -> &'static HashSet<String> {
    SENSITIVE_SYSTEM_REAL_PATHS.get_or_init(|| {
        SENSITIVE_PATHS
            .iter()
            // Only this platform's entries: on Windows `/` would resolve to the root of the
            // current drive and refuse every project at a drive root.
            .filter(|entry| if cfg!(windows) { entry.as_bytes().get(1) == Some(&b':') } else { entry.starts_with('/') })
            .filter_map(|entry| native_real_path(entry))
            .map(|real| fold_path_case(&real))
            .collect()
    })
}

/// The sensitive home folders, folded: as spelled under the home directory, under the home's real
/// path (a linked home), and each folder's own real path (a `~/.ssh` that is itself a link into a
/// dotfiles checkout).
fn sensitive_home_trees() -> Vec<String> {
    let Some(home) = home_dir() else { return Vec::new() };
    let now = Instant::now();
    let mut cache = SENSITIVE_HOME_CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(cached) = cache.as_ref().filter(|cached| cached.home == home && now < cached.expires) {
        return cached.trees.clone();
    }
    let home_text = home.to_string_lossy().into_owned();
    let real_home = native_real_path(&home_text);
    let mut trees: Vec<String> = Vec::new();
    let mut add = |tree: String| {
        let tree = fold_path_case(&tree);
        if !trees.contains(&tree) {
            trees.push(tree);
        }
    };
    for dir in SENSITIVE_HOME_DIRS {
        let spelled = nodepath::join(&[&home_text, dir]);
        if let Some(real_home) = &real_home {
            add(nodepath::join(&[real_home, dir]));
        }
        if let Some(real) = native_real_path(&spelled) {
            add(real);
        }
        add(spelled);
    }
    *cache = Some(SensitiveHome { home, expires: now + SENSITIVE_HOME_TTL, trees: trees.clone() });
    trees
}

/// Safely parse JSON with a fallback value. Prevents crashes from corrupted database metadata.
pub fn safe_json_parse<T: serde::de::DeserializeOwned>(value: &str, fallback: T) -> T {
    serde_json::from_str(value).unwrap_or(fallback)
}

/// Clamp a numeric value to a range. Used to enforce sane limits on MCP tool inputs.
pub fn clamp<T: PartialOrd>(value: T, min: T, max: T) -> T {
    if value < min {
        min
    } else if value > max {
        max
    } else {
        value
    }
}

/// Normalize a file path to use forward slashes.
pub fn normalize_path(file_path: &str) -> String {
    codegraph_types::paths::normalize_path(file_path)
}

/// Escape `text` to match itself literally inside a regular expression.
pub fn escape_reg_exp(text: &str) -> String {
    codegraph_jsre::escape_regex(text)
}

/// Is a process with this PID still running?
pub fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: signal 0 only probes for existence.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        // SAFETY: plain handle open / query / close.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut code: u32 = 0;
            let ok = GetExitCodeProcess(handle, &mut code);
            CloseHandle(handle);
            ok != 0 && code == STILL_ACTIVE as u32
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// Cross-process file lock using a lock file with PID tracking.
///
/// Prevents multiple processes (e.g. git hooks, CLI, MCP server) from writing to the same
/// database simultaneously.
pub struct FileLock {
    lock_path: PathBuf,
    held: bool,
}

/// The lock is held by another live process.
pub const LOCKED_BY_ANOTHER: &str = "locked by another";

impl FileLock {
    pub fn new(lock_path: impl Into<PathBuf>) -> FileLock {
        FileLock { lock_path: lock_path.into(), held: false }
    }

    /// Acquire the lock. Errs if the lock is held by another live process.
    pub fn acquire(&mut self) -> Result<(), String> {
        // Check for existing lock.
        if self.lock_path.exists() {
            match std::fs::read_to_string(&self.lock_path) {
                Ok(content) => {
                    // A long sync can hold the lock for hours. Its age says nothing about whether
                    // the writer is still alive; only a dead PID is stale.
                    if let Some(pid) = parse_int_prefix(content.trim()) {
                        if is_process_alive(pid) {
                            return Err(format!(
                                "CodeGraphV database is locked by another process (PID {pid}). If this is stale, run 'codegraphV unlock' or delete {}",
                                self.lock_path.display()
                            ));
                        }
                    }
                    // Stale lock (dead process or malformed PID) - remove it.
                    let _ = std::fs::remove_file(&self.lock_path);
                }
                Err(_) => {
                    // Other errors reading lock file - try to remove it.
                    let _ = std::fs::remove_file(&self.lock_path);
                }
            }
        }

        // Write our PID to the lock file using exclusive create flag.
        use std::io::Write;
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&self.lock_path) {
            Ok(mut f) => {
                let _ = write!(f, "{}", std::process::id());
                self.held = true;
                Ok(())
            }
            // Race condition: another process grabbed the lock between our check and write.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(format!(
                "CodeGraphV database is locked by another process. If this is stale, run 'codegraphV unlock' or delete {}",
                self.lock_path.display()
            )),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Whether another live process holds the lock right now: the lock file names a live PID that
    /// is not this process's. A dead PID's leftover file is not a holder.
    pub fn held_by_another(&self) -> bool {
        std::fs::read_to_string(&self.lock_path)
            .ok()
            .and_then(|content| parse_int_prefix(content.trim()))
            .is_some_and(|pid| pid != std::process::id() && is_process_alive(pid))
    }

    /// Release the lock (only if we still own it).
    pub fn release(&mut self) {
        if !self.held {
            return;
        }
        if let Ok(content) = std::fs::read_to_string(&self.lock_path) {
            if parse_int_prefix(content.trim()) == Some(std::process::id()) {
                let _ = std::fs::remove_file(&self.lock_path);
            }
        }
        self.held = false;
    }

    /// Execute a function while holding the lock.
    pub fn with_lock<T>(&mut self, f: impl FnOnce() -> T) -> Result<T, String> {
        self.acquire()?;
        let out = f();
        self.release();
        Ok(out)
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        self.release();
    }
}

/// `parseInt(s, 10)` for a non-negative PID: the leading digits.
fn parse_int_prefix(s: &str) -> Option<u32> {
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps() {
        assert_eq!(clamp(5, 1, 3), 3);
        assert_eq!(clamp(0, 1, 3), 1);
        assert_eq!(clamp(2, 1, 3), 2);
    }

    #[test]
    fn refuses_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().into_owned();
        assert!(validate_path_within_root(&root, "../outside", ValidateOptions::default()).is_none());
        assert!(validate_path_within_root(&root, "missing.txt", ValidateOptions::default()).is_some());
    }

    /// A directory link `link` → `target`: a junction on Windows (no privilege needed), a
    /// symlink elsewhere.
    fn link_dir(target: &std::path::Path, link: &std::path::Path) {
        #[cfg(windows)]
        {
            let ok = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "mklink /J failed");
        }
        #[cfg(not(windows))]
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    fn remove_link(link: &std::path::Path) {
        let _ = std::fs::remove_dir(link).or_else(|_| std::fs::remove_file(link));
    }

    #[test]
    fn missing_leaf_below_an_outward_link_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(root.join(".codegraphV")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let link = root.join(".codegraphV").join("ui");
        link_dir(&outside, &link);
        let r = root.to_string_lossy().into_owned();
        let refused = validate_path_within_root(&r, ".codegraphV/ui/trails", ValidateOptions::default());
        // The escape hatch for indexing still allows it.
        let escaped = validate_path_within_root(&r, ".codegraphV/ui/trails", ValidateOptions { allow_symlink_escape: true });
        remove_link(&link);
        assert!(refused.is_none());
        assert!(escaped.is_some());
    }

    #[test]
    fn missing_leaf_below_an_in_root_link_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(root.join("real")).unwrap();
        let link = root.join("alias");
        link_dir(&root.join("real"), &link);
        let r = root.to_string_lossy().into_owned();
        let allowed = validate_path_within_root(&r, "alias/missing/leaf.json", ValidateOptions::default());
        remove_link(&link);
        assert!(allowed.is_some());
    }

    #[test]
    fn every_path_under_a_missing_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("gone").to_string_lossy().into_owned();
        assert!(validate_path_within_root(&root, "a.txt", ValidateOptions::default()).is_none());
    }

    #[test]
    fn lock_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codegraph.lock");
        let mut a = FileLock::new(&path);
        a.acquire().unwrap();
        let mut b = FileLock::new(&path);
        assert!(b.acquire().unwrap_err().contains(LOCKED_BY_ANOTHER));
        a.release();
        b.acquire().unwrap();
    }

    #[test]
    fn sensitive_dirs_refused() {
        // An absolute sensitive path: on Windows "/" resolves against the cwd's drive, which
        // is D: on CI runners.
        let root = if cfg!(windows) { r"C:\Windows" } else { "/" };
        assert!(validate_project_path(root).is_some());
        assert!(validate_project_path(".").is_none());
    }
}
