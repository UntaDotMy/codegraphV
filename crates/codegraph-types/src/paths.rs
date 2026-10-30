//! Path conventions every layer shares — the `normalizePath` helper of
//! `src/utils.ts` and the data-directory naming of `src/directory.ts`.

use crate::nodepath;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// The default per-project data directory name.
pub const DEFAULT_CODEGRAPH_DIR: &str = ".codegraphV";

/// The data directory name WSL gives a fresh project on a Windows drive, so it
/// never shares one index with codegraphV on Windows (issue #995).
pub const WSL_CODEGRAPH_DIR: &str = ".codegraphV-wsl";

/// The original (TypeScript) codegraph's per-project folder: never indexed or written by
/// codegraphV; `codegraphV migrate` moves it to `.codegraphV`.
pub const ORIGINAL_CODEGRAPH_DIR: &str = ".codegraph";

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
    let raw = std::env::var("CODEGRAPHV_DIR").unwrap_or_default();
    let raw = crate::jscompat::js_trim(&raw);
    if raw.is_empty() {
        return DEFAULT_CODEGRAPH_DIR.to_string();
    }
    let invalid = raw == "." || raw.contains("..") || raw.contains('/') || raw.contains('\\') || Path::new(raw).is_absolute();
    if invalid {
        if !WARNED_BAD_DIR_NAME.swap(true, Ordering::Relaxed) {
            // stderr only — stdout is the MCP protocol channel.
            eprintln!(
                "[codegraphV] Ignoring invalid CODEGRAPHV_DIR=\"{raw}\" — it must be a plain directory name (no path separators, no \"..\", not absolute). Using \"{DEFAULT_CODEGRAPH_DIR}\"."
            );
        }
        return DEFAULT_CODEGRAPH_DIR.to_string();
    }
    raw.to_string()
}

/// Is `name` (a single path segment) a data directory? codegraphV's (`.codegraphV`, the active
/// `CODEGRAPHV_DIR` override, any `.codegraphV-*` sibling) and the original codegraph's
/// (`.codegraph`, `.codegraph-*`) — indexing and watching skip all of them (issue #636).
pub fn is_codegraph_data_dir(name: &str) -> bool {
    [DEFAULT_CODEGRAPH_DIR, ORIGINAL_CODEGRAPH_DIR].iter().any(|dir| name == *dir || name.starts_with(&format!("{dir}-")))
        || name == codegraph_dir_name()
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

/// Whether `child` is `parent` or lies under it, compared exactly as given.
///
/// A root (`C:\`, `\\server\share\`, `/`) already ends with its separator, so none is added to
/// it. Adding one made `C:\\` the prefix, which no path under the root starts with, so a project
/// at a drive root indexed nothing (#415).
pub fn is_same_or_under(child: &str, parent: &str) -> bool {
    if child == parent {
        return true;
    }
    if parent.ends_with(crate::nodepath::SEP) {
        child.starts_with(parent)
    } else {
        child.starts_with(&format!("{parent}{}", crate::nodepath::SEP))
    }
}

/// Whether `child` is `parent` itself or sits underneath it. Case-insensitive on
/// Windows (NTFS is, and realpath can hand back a different case than the lexical root).
pub fn is_within_dir(child: &str, parent: &str) -> bool {
    if cfg!(windows) {
        is_same_or_under(&child.to_lowercase(), &parent.to_lowercase())
    } else {
        is_same_or_under(child, parent)
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

/// Whether an I/O error means nothing is at the path: `NotFound`, or `NotADirectory`, which POSIX
/// reports instead when a folder on the way is a file (Windows reports `NotFound` for both).
pub fn is_missing_path_error(error: &std::io::Error) -> bool {
    matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory)
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
        // Any resolution failure other than a missing path (ELOOP, EACCES, …) is treated as unsafe.
        // A path under a file (ENOTDIR on POSIX) is a missing leaf too (#2904).
        Err(e) if !is_missing_path_error(&e) => return None,
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
            Err(e) if !is_missing_path_error(&e) => return None,
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
        assert!(is_codegraph_data_dir(".codegraphV"));
        assert!(is_codegraph_data_dir(".codegraphV-wsl"));
        // The original codegraph's folders are skipped too.
        assert!(is_codegraph_data_dir(".codegraph"));
        assert!(is_codegraph_data_dir(".codegraph-wsl"));
        assert!(is_codegraph_data_dir(".codegraph-win"));
        assert!(!is_codegraph_data_dir(".codegraphx"));
        assert!(!is_codegraph_data_dir(".codegraphVx"));
        assert!(!is_codegraph_data_dir("codegraphV"));
    }

    /// A project at a drive root, a subst drive or a share root: the root already ends with its
    /// separator, so a file under it is not found under `root + separator` (#415).
    #[cfg(windows)]
    #[test]
    fn a_root_that_ends_in_a_separator_contains_its_files() {
        for root in [r"Q:\", r"\\server\share\"] {
            let file = format!("{root}src{}a.ts", crate::nodepath::SEP);
            assert!(is_within_dir(&file, root), "{file} is under {root}");
            assert!(is_within_dir(root, root), "{root} is {root}");
            assert_eq!(lexical_path_within_root(root, "src/a.ts"), Some(file), "{root}");
            // `..` cannot climb above a root, so this stays inside it (as Node's path.win32 does).
            assert_eq!(lexical_path_within_root(root, r"..\..\elsewhere"), Some(format!("{root}elsewhere")), "{root}");
        }
        // The case folding NTFS needs still holds, and a sibling that merely shares the prefix is
        // not inside.
        assert!(is_within_dir(r"Q:\SRC\a.ts", r"q:\"));
        assert!(!is_within_dir(r"C:\foobar", r"C:\foo"));
        assert!(is_within_dir(r"C:\foo\bar", r"C:\foo"));
    }

    #[cfg(not(windows))]
    #[test]
    fn a_root_that_ends_in_a_separator_contains_its_files() {
        assert!(is_within_dir("/src/a.ts", "/"));
        assert!(is_within_dir("/", "/"));
        assert_eq!(lexical_path_within_root("/", "src/a.ts"), Some("/src/a.ts".to_string()));
        assert!(!is_within_dir("/foobar", "/foo"));
        assert!(is_within_dir("/foo/bar", "/foo"));
    }

    #[test]
    fn a_file_under_the_filesystem_root_is_within_it() {
        let root = crate::nodepath::resolve(&[if cfg!(windows) { r"C:\" } else { "/" }]);
        let file = format!("{root}{}a.ts", if root.ends_with(crate::nodepath::SEP) { "" } else { "\\" });
        assert!(is_within_dir(&file, &root), "{file} under {root}");
        assert!(lexical_path_within_root(&root, "a.ts").is_some());
    }

    #[test]
    fn only_a_missing_path_counts_as_missing() {
        use std::io::{Error, ErrorKind};
        for kind in [ErrorKind::NotFound, ErrorKind::NotADirectory] {
            assert!(is_missing_path_error(&Error::from(kind)), "{kind:?}");
        }
        for kind in [ErrorKind::PermissionDenied, ErrorKind::IsADirectory, ErrorKind::Other, ErrorKind::InvalidInput, ErrorKind::TimedOut] {
            assert!(!is_missing_path_error(&Error::from(kind)), "{kind:?}");
        }
    }

    /// A path under an existing file is a missing leaf, judged by its nearest existing ancestor:
    /// inside the root it validates, so the reader can report nothing there (#2904).
    #[test]
    fn a_path_under_an_existing_file_is_judged_like_any_missing_leaf() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = real_path(&std::env::temp_dir()).unwrap().join(format!("cgv-paths-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("file"), "x").unwrap();
        let root = dir.to_string_lossy().into_owned();
        let judged = validate_path_within_root(&root, "file/child.ts", ValidateOptions::default());
        let escaped = validate_path_within_root(&root, "../file/child.ts", ValidateOptions::default());
        let _ = std::fs::remove_dir_all(&dir);
        assert!(judged.is_some(), "a path under a file inside the root was refused");
        assert!(escaped.is_none(), "a lexical escape must still be refused");
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
