//! Path conventions every layer shares — the `normalizePath` helper of
//! `src/utils.ts` and the data-directory naming of `src/directory.ts`.

use crate::nodepath;
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

/// `fs.realpathSync`: the canonical path, without the `\\?\` verbatim prefix Windows adds.
pub fn real_path(p: &Path) -> std::io::Result<PathBuf> {
    let canonical = std::fs::canonicalize(p)?;
    Ok(strip_verbatim(canonical))
}

/// `\\?\C:\x` → `C:\x`, `\\?\UNC\server\share` → `\\server\share`.
fn strip_verbatim(p: PathBuf) -> PathBuf {
    if !cfg!(windows) {
        return p;
    }
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        let bytes = rest.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return PathBuf::from(rest.to_string());
        }
    }
    p
}

/// Node's non-native `fs.realpathSync`: resolve symlinks while retaining the
/// spelling of ordinary components (notably Windows 8.3 aliases). Path identity
/// consumers need this distinction; content-serving containment uses the native
/// canonicalization above. A failed lookup is returned to the caller, which may
/// choose to retain its lexical input.
pub fn real_path_preserving_spelling(path: &Path) -> std::io::Result<PathBuf> {
    fn walk(path: &Path, depth: usize) -> std::io::Result<PathBuf> {
        if depth > 40 {
            return Err(std::io::Error::other("Too many symbolic links"));
        }
        let mut result = PathBuf::new();
        for part in path.components() {
            result.push(part.as_os_str());
            if matches!(part, std::path::Component::Prefix(_)) { continue; }
            let meta = std::fs::symlink_metadata(&result)?;
            let is_link = meta.file_type().is_symlink();
            #[cfg(windows)]
            let is_link = {
                use std::os::windows::fs::MetadataExt;
                is_link || meta.file_attributes() & 0x400 != 0
            };
            if is_link {
                let target = strip_verbatim(std::fs::read_link(&result)?);
                let target = if target.is_absolute() { target } else { result.parent().unwrap_or(Path::new("")).join(target) };
                result = walk(&resolve_path(target), depth + 1)?;
            }
        }
        Ok(result)
    }
    walk(&resolve_path(path), 0)
}

/// Options of [`validate_path_within_root`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ValidateOptions {
    /// Follow in-root symlinks out of the root (indexing read path only); the content-serving
    /// sinks must never set this.
    pub allow_symlink_escape: bool,
}

/// Validate that a file path stays within the project root, resolving symlinks.
///
/// Two layers: a cheap lexical check that catches `../` traversal, then a realpath check that
/// catches symlink escapes — an in-repo symlink whose logical path is inside the root but whose
/// real target points outside it (issue #527). A symlink that stays within the root is still
/// allowed, so legitimate in-tree symlinks keep working. Both content-serving read sinks
/// (`codegraph_node` `includeCode`, `codegraph_explore` source) go through here, so this is the
/// chokepoint that keeps out-of-root file contents from leaking.
///
/// `allow_symlink_escape` waives **only** the realpath-escape rejection (the lexical `../` guard
/// still applies) for the INDEXING read path. The directory walk deliberately descends into
/// in-root symlinks whose targets live outside the root (e.g. a `game/` symlink in a Dota
/// custom-game tree, #935); discovery and the reader must agree, or every file the walk
/// enumerated fails to index.
///
/// Returns the resolved absolute path (realpath when it exists), or `None` if it escapes the root.
/// A path that does not exist yet is judged by its nearest existing ancestor, and every path under
/// a root that does not exist is `None`.
pub fn validate_path_within_root(project_root: &str, file_path: &str, options: ValidateOptions) -> Option<String> {
    // 1. Lexical containment — cheap, catches `../` traversal.
    let resolved = lexical_path_within_root(project_root, file_path)?;
    let normalized_root = nodepath::resolve(&[project_root]);

    // 2. Symlink-aware containment — resolve symlinks on both sides and re-check. Every path
    //    under a root that does not exist is refused.
    let real_root = real_path(Path::new(&normalized_root)).ok()?.to_string_lossy().into_owned();
    match real_path(Path::new(&resolved)) {
        Ok(resolved_real) => {
            let resolved_real = resolved_real.to_string_lossy().into_owned();
            if options.allow_symlink_escape {
                return Some(resolved_real);
            }
            return is_within_dir(&resolved_real, &real_root).then_some(resolved_real);
        }
        // Any resolution failure other than ENOENT (ELOOP, EACCES, …) is treated as unsafe.
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return None,
        Err(_) => {}
    }
    // A missing leaf (a file about to be written, an index entry for a since-deleted file) may
    // still sit below an EXISTING link: judge it by its nearest existing ancestor, or
    // `.codegraph/ui -> /outside` plus a missing `trails/` passes and mkdir follows the link out.
    let mut ancestor = Path::new(&resolved).parent()?.to_path_buf();
    while is_within_dir(&ancestor.to_string_lossy(), &normalized_root) {
        match real_path(&ancestor) {
            Ok(real) => {
                let inside = options.allow_symlink_escape || is_within_dir(&real.to_string_lossy(), &real_root);
                return inside.then_some(resolved);
            }
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return None,
            Err(_) => ancestor = ancestor.parent()?.to_path_buf(),
        }
    }
    None
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

    #[cfg(windows)]
    #[test]
    fn non_native_realpath_keeps_short_alias_and_case_spelling() {
        let temp = std::env::temp_dir();
        let input = resolve_path(&temp).to_string_lossy().to_lowercase();
        assert_eq!(real_path_preserving_spelling(Path::new(&input)).unwrap().to_string_lossy(), input);
        assert!(real_path(Path::new(&input)).is_ok());
    }
}
