//! Path conventions every layer shares — the `normalizePath` helper of
//! `src/utils.ts` and the data-directory naming of `src/directory.ts`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// The default per-project data directory name.
pub const DEFAULT_CODEGRAPH_DIR: &str = ".codegraph";

/// The data directory name WSL gives a fresh project on a Windows drive, so it
/// never shares one index with CodeGraph on Windows (issue #995).
pub const WSL_CODEGRAPH_DIR: &str = ".codegraph-wsl";

/// Index paths are always `/`-separated.
pub fn normalize_path(file_path: &str) -> String {
    file_path.replace('\\', "/")
}

static WARNED_BAD_DIR_NAME: AtomicBool = AtomicBool::new(false);

/// The per-project data directory name, honoring the `CODEGRAPH_DIR` override
/// (default `.codegraph`). The override must be a single plain directory name:
/// one that is empty, `.`, contains `..` or a path separator, or is absolute is
/// ignored (with one stderr warning) rather than risk writing the index outside
/// the project or into its root. Read live, so it is testable.
pub fn codegraph_dir_name() -> String {
    let raw = std::env::var("CODEGRAPH_DIR").unwrap_or_default();
    let raw = crate::jscompat::js_trim(&raw);
    if raw.is_empty() {
        return DEFAULT_CODEGRAPH_DIR.to_string();
    }
    let invalid = raw == "." || raw.contains("..") || raw.contains('/') || raw.contains('\\') || Path::new(raw).is_absolute();
    if invalid {
        if !WARNED_BAD_DIR_NAME.swap(true, Ordering::Relaxed) {
            // stderr only — stdout is the MCP protocol channel.
            eprintln!(
                "[codegraph] Ignoring invalid CODEGRAPH_DIR=\"{raw}\" — it must be a plain directory name (no path separators, no \"..\", not absolute). Using \"{DEFAULT_CODEGRAPH_DIR}\"."
            );
        }
        return DEFAULT_CODEGRAPH_DIR.to_string();
    }
    raw.to_string()
}

/// Is `name` (a single path segment) a CodeGraph data directory? The default
/// `.codegraph`, the active `CODEGRAPH_DIR` override, and any `.codegraph-*`
/// sibling — indexing and watching skip all of them (issue #636).
pub fn is_codegraph_data_dir(name: &str) -> bool {
    name == DEFAULT_CODEGRAPH_DIR || name == codegraph_dir_name() || name.starts_with(&format!("{DEFAULT_CODEGRAPH_DIR}-"))
}

/// `path.resolve(p)`: absolute, with `.` and `..` folded away, no symlink
/// resolution. Separators follow the host.
pub fn resolve_path(p: impl AsRef<Path>) -> PathBuf {
    use std::path::Component;
    let abs = std::path::absolute(p.as_ref()).unwrap_or_else(|_| p.as_ref().to_path_buf());
    let mut out = PathBuf::new();
    for comp in abs.components() {
        match comp {
            Component::ParentDir => {
                if !matches!(out.components().next_back(), Some(Component::RootDir | Component::Prefix(_)) | None) {
                    out.pop();
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether `child` is `parent` itself or sits underneath it. Case-insensitive on
/// Windows (NTFS is, and realpath can hand back a different case than the lexical root).
pub fn is_within_dir(child: &str, parent: &str) -> bool {
    if cfg!(windows) {
        let (c, p) = (child.to_lowercase(), parent.to_lowercase());
        c == p || c.starts_with(&format!("{p}{}", crate::nodepath::SEP))
    } else {
        child == parent || child.starts_with(&format!("{parent}{}", crate::nodepath::SEP))
    }
}

/// The lexical half of `validatePathWithinRoot`: the resolved absolute path when
/// `file_path` stays inside `project_root` after `../` segments are applied, else
/// `None`. No filesystem access.
pub fn lexical_path_within_root(project_root: &str, file_path: &str) -> Option<String> {
    let resolved = crate::nodepath::resolve(&[project_root, file_path]);
    is_within_dir(&resolved, &crate::nodepath::resolve(&[project_root])).then_some(resolved)
}

/// The user's home directory (`os.homedir()`).
pub fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_names() {
        assert!(is_codegraph_data_dir(".codegraph"));
        assert!(is_codegraph_data_dir(".codegraph-wsl"));
        assert!(is_codegraph_data_dir(".codegraph-win"));
        assert!(!is_codegraph_data_dir(".codegraphx"));
        assert!(!is_codegraph_data_dir("codegraph"));
    }

    #[test]
    fn resolve_folds_dots() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(resolve_path("a/./b/../c"), cwd.join("a").join("c"));
        assert_eq!(normalize_path(r"a\b/c"), "a/b/c");
    }
}
