//! CodeGraph utilities — the security helpers and the cross-process lock of `src/utils.ts`.
//!
//! (`Mutex`, `processInBatches`, `debounce` and the memory monitor exist for the JavaScript event
//! loop; Rust has `std::sync` and plain loops for them.)

use codegraph_types::nodepath;
use std::path::PathBuf;

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

pub use codegraph_types::paths::{real_path, validate_path_within_root, ValidateOptions};

/// Validate that a path is a safe project root directory.
///
/// Rejects sensitive system directories and ensures the path is a real, existing directory. Used
/// at MCP and API entry points to prevent arbitrary directory access. Returns an error message if
/// invalid, or `None` if valid.
pub fn validate_project_path(dir_path: &str) -> Option<String> {
    let resolved = nodepath::resolve(&[dir_path]);

    // Block sensitive system directories.
    if SENSITIVE_PATHS.contains(&resolved.as_str()) || SENSITIVE_PATHS.contains(&resolved.to_lowercase().as_str()) {
        return Some(format!("Refusing to operate on sensitive system directory: {resolved}"));
    }

    // Also block common sensitive home subdirectories.
    if let Some(home) = codegraph_types::paths::home_dir() {
        for dir in [".ssh", ".gnupg", ".aws", ".config"] {
            let sensitive = home.join(dir).to_string_lossy().into_owned();
            if resolved == sensitive || resolved.starts_with(&format!("{sensitive}{}", nodepath::SEP)) {
                return Some(format!("Refusing to operate on sensitive directory: {resolved}"));
            }
        }
    }

    // Verify it's a real directory.
    match std::fs::metadata(&resolved) {
        Ok(m) if !m.is_dir() => Some(format!("Path is not a directory: {resolved}")),
        Ok(_) => None,
        Err(_) => Some(format!("Path does not exist or is not accessible: {resolved}")),
    }
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
fn is_process_alive(pid: u32) -> bool {
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
                                "CodeGraph database is locked by another process (PID {pid}). If this is stale, run 'codegraph unlock' or delete {}",
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
                "CodeGraph database is locked by another process. If this is stale, run 'codegraph unlock' or delete {}",
                self.lock_path.display()
            )),
            Err(e) => Err(e.to_string()),
        }
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
        std::fs::create_dir_all(root.join(".codegraph")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let link = root.join(".codegraph").join("ui");
        link_dir(&outside, &link);
        let r = root.to_string_lossy().into_owned();
        let refused = validate_path_within_root(&r, ".codegraph/ui/trails", ValidateOptions::default());
        // The escape hatch for indexing still allows it.
        let escaped = validate_path_within_root(&r, ".codegraph/ui/trails", ValidateOptions { allow_symlink_escape: true });
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
