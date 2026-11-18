//! Marker-delimited git hooks, ported from `src/sync/git-hooks.ts`.
use crate::{git, resolved};
use codegraph_types::{
    jscompat::{is_js_space, js_trim},
    nodepath,
};
use serde::{Deserialize, Serialize};
use std::{cell::OnceCell, fs, io, path::Path};

const BEGIN: &str = "# >>> codegraphV sync hook >>>";
const END: &str = "# <<< codegraphV sync hook <<<";
const ROOT_PREFIX: &str = "# codegraphV-root: ";

/// The markers the original `codegraph` wrote, and the root line it used. A project that ran it
/// keeps its hook, so an upgrade has to adopt that block — replacing it in place — instead of
/// leaving it beside ours, which would run both engines' sync on every commit.
const LEGACY_BEGIN: &str = "# >>> codegraph sync hook >>>";
const LEGACY_END: &str = "# <<< codegraph sync hook <<<";
const LEGACY_ROOT_PREFIX: &str = "# codegraph-root: ";

/// One engine's marker pair.
#[derive(Clone, Copy)]
struct Markers {
    begin: &'static str,
    end: &'static str,
}
const MARKERS: [Markers; 2] = [
    Markers { begin: BEGIN, end: END },
    Markers { begin: LEGACY_BEGIN, end: LEGACY_END },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GitHookName {
    PostCommit,
    PostMerge,
    PostCheckout,
}
impl GitHookName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PostCommit => "post-commit",
            Self::PostMerge => "post-merge",
            Self::PostCheckout => "post-checkout",
        }
    }
}
pub const DEFAULT_SYNC_HOOKS: &[GitHookName] = &[
    GitHookName::PostCommit,
    GitHookName::PostMerge,
    GitHookName::PostCheckout,
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHookResult {
    pub installed: Vec<GitHookName>,
    pub hooks_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsupported: Option<Vec<GitHookName>>,
}
impl GitHookResult {
    fn skipped(reason: &str, dir: Option<String>) -> Self {
        Self {
            installed: Vec::new(),
            hooks_dir: dir,
            skipped: Some(reason.into()),
            unsupported: None,
        }
    }
}

pub fn is_git_repo(root: &str) -> bool {
    git(root, &["rev-parse", "--is-inside-work-tree"]).as_deref() == Some("true")
}
fn hooks_dir(root: &str) -> Option<String> {
    let out = git(root, &["rev-parse", "--git-path", "hooks"])?;
    if out.is_empty() {
        return None;
    }
    Some(if Path::new(&out).is_absolute() {
        out
    } else {
        nodepath::resolve(&[root, &out])
    })
}
fn root_key(root: &str) -> String {
    resolved(root).replace(nodepath::SEP, "/")
}

fn marker_block(root: &str) -> String {
    let quoted = format!("'{}'", root.replace('\'', "'\\''"));
    [
        BEGIN.to_string(),
        "# Keeps the CodeGraphV index fresh while the live file watcher is off".into(),
        "# (e.g. WSL2 /mnt drives). Runs in the background so it never blocks git.".into(),
        "# Managed by codegraphV; remove with `codegraphV uninit` or delete this block.".into(),
        format!("{ROOT_PREFIX}{root}"),
        "if command -v codegraphV >/dev/null 2>&1; then".into(),
        format!("  ( codegraphV sync {quoted} >/dev/null 2>&1 & ) >/dev/null 2>&1"),
        "fi".into(),
        END.into(),
    ]
    .join("\n")
}

struct Owner<'a> {
    root: String,
    project: &'a str,
    top: OnceCell<Option<String>>,
}
impl<'a> Owner<'a> {
    fn new(project: &'a str) -> Self {
        Self {
            root: root_key(project),
            project,
            top: OnceCell::new(),
        }
    }
    fn owns(&self, block: Option<&str>) -> bool {
        if let Some(block) = block {
            return block == self.root;
        }
        self.top
            .get_or_init(|| {
                git(self.project, &["rev-parse", "--show-toplevel"])
                    .filter(|s| !s.is_empty())
                    .map(|s| root_key(&s))
            })
            .as_deref()
            == Some(&self.root)
    }
}

/// The marker pair `line` opens, if any.
fn markers_opened_by(line: &str) -> Option<Markers> {
    MARKERS.into_iter().find(|m| line == m.begin)
}

/// A block's root line, which both prefixes name. A line carries one prefix as easily across
/// prefixes (`# codegraph-root: x` also `strip_prefix`es `# codegraphV`), so the pair decides.
fn root_named_by(line: &str) -> Option<&str> {
    line.strip_prefix(ROOT_PREFIX)
        .or_else(|| line.strip_prefix(LEGACY_ROOT_PREFIX))
}

/// Where the block opened at `from` ends: the index of its closing marker, or the last line when
/// the file has none.
fn block_end(lines: &[&str], from: usize, markers: Markers) -> usize {
    let mut end = from;
    while end < lines.len() && js_trim(lines[end]) != markers.end {
        end += 1;
    }
    end
}

fn block_roots(content: &str) -> Vec<Option<&str>> {
    let lines: Vec<&str> = content.split('\n').map(js_trim).collect();
    let mut roots = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(markers) = markers_opened_by(lines[i]) else {
            i += 1;
            continue;
        };
        let end = block_end(&lines, i, markers);
        let root = lines[i..end.min(lines.len())].iter().find_map(|l| root_named_by(l));
        // Only a closed block counts, as before: an unterminated one names nothing.
        if end < lines.len() {
            roots.push(root);
        }
        i = end + 1;
    }
    roots
}
fn strip_blocks(content: &str, owner: &Owner<'_>) -> String {
    let lines: Vec<_> = content.split('\n').collect();
    let mut kept = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(markers) = markers_opened_by(js_trim(lines[i])) else {
            kept.push(lines[i]);
            i += 1;
            continue;
        };
        let mut end = block_end(&lines, i, markers);
        let block = &lines[i..(end + 1).min(lines.len())];
        let root = block.iter().find_map(|l| root_named_by(js_trim(l)));
        if owner.owns(root) {
            if lines.get(end + 1).is_some_and(|l| js_trim(l).is_empty()) {
                end += 1;
            }
        } else {
            kept.extend_from_slice(block);
        }
        i = end + 1;
    }
    kept.join("\n")
}
fn shell_hook(content: &str) -> bool {
    let first = js_trim(content.split('\n').next().unwrap_or(""));
    let Some(shebang) = first.strip_prefix("#!") else {
        return true;
    };
    let words: Vec<_> = js_trim(shebang)
        .split(is_js_space)
        .filter(|s| !s.is_empty())
        .collect();
    let mut program = words
        .first()
        .copied()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("");
    if program == "env" {
        program = words
            .iter()
            .skip(1)
            .find(|s| !s.starts_with('-'))
            .copied()
            .unwrap_or("")
            .rsplit('/')
            .next()
            .unwrap_or("");
    }
    ["sh", "bash", "dash", "zsh", "ksh", "mksh", "ash"].contains(&program)
}
fn effectively_empty(content: &str) -> bool {
    content
        .split('\n')
        .map(js_trim)
        .all(|l| l.is_empty() || l.starts_with("#!"))
}
fn executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
    }
    #[cfg(not(unix))]
    let _ = path;
}

pub fn install_git_sync_hook(
    root: &str,
    hooks: Option<&[GitHookName]>,
) -> io::Result<GitHookResult> {
    let key = root_key(root);
    if key.contains(['\r', '\n']) {
        return Ok(GitHookResult::skipped(
            "the project path contains a line break",
            None,
        ));
    }
    let Some(dir) = hooks_dir(root) else {
        return Ok(GitHookResult::skipped("not a git repository", None));
    };
    if fs::create_dir_all(&dir).is_err() {
        return Ok(GitHookResult::skipped(
            "could not access the git hooks directory",
            Some(dir),
        ));
    }
    let owner = Owner::new(root);
    let block = marker_block(&key);
    let mut installed = Vec::new();
    let mut unsupported = Vec::new();
    for &hook in hooks.unwrap_or(DEFAULT_SYNC_HOOKS) {
        let file = Path::new(&dir).join(hook.as_str());
        let mut content = format!("#!/bin/sh\n{block}\n");
        if file.exists() {
            let existing = String::from_utf8_lossy(&fs::read(&file)?).into_owned();
            if !shell_hook(&existing) {
                unsupported.push(hook);
                continue;
            }
            let stripped = strip_blocks(&existing, &owner);
            let base = stripped.trim_end_matches(is_js_space);
            if !effectively_empty(base) {
                let mut lines: Vec<_> = base.split('\n').collect();
                let shebang = if lines.first().is_some_and(|l| l.starts_with("#!")) {
                    format!("{}\n", lines.remove(0))
                } else {
                    String::new()
                };
                let rest = lines.join("\n");
                let rest = codegraph_jsre::re!(r"^\s*\n").replace_first(&rest, |_| String::new());
                content = format!("{shebang}{block}\n\n{rest}\n");
            }
        }
        fs::write(&file, content)?;
        executable(&file);
        installed.push(hook);
    }
    Ok(GitHookResult {
        installed,
        hooks_dir: Some(dir),
        skipped: None,
        unsupported: if unsupported.is_empty() {
            None
        } else {
            Some(unsupported)
        },
    })
}

pub fn remove_git_sync_hook(
    root: &str,
    hooks: Option<&[GitHookName]>,
) -> io::Result<GitHookResult> {
    let Some(dir) = hooks_dir(root) else {
        return Ok(GitHookResult::skipped("not a git repository", None));
    };
    let owner = Owner::new(root);
    let mut removed = Vec::new();
    for &hook in hooks.unwrap_or(DEFAULT_SYNC_HOOKS) {
        let file = Path::new(&dir).join(hook.as_str());
        if !file.exists() {
            continue;
        }
        let original = String::from_utf8_lossy(&fs::read(&file)?).into_owned();
        if !block_roots(&original).into_iter().any(|r| owner.owns(r)) {
            continue;
        }
        let stripped = strip_blocks(&original, &owner);
        if effectively_empty(&stripped) {
            fs::remove_file(&file)?;
        } else {
            fs::write(
                &file,
                format!("{}\n", stripped.trim_end_matches(is_js_space)),
            )?;
            executable(&file);
        }
        removed.push(hook);
    }
    Ok(GitHookResult {
        installed: removed,
        hooks_dir: Some(dir),
        skipped: None,
        unsupported: None,
    })
}

pub fn is_sync_hook_installed(root: &str, hooks: Option<&[GitHookName]>) -> io::Result<bool> {
    let Some(dir) = hooks_dir(root) else {
        return Ok(false);
    };
    let owner = Owner::new(root);
    for &hook in hooks.unwrap_or(DEFAULT_SYNC_HOOKS) {
        let file = Path::new(&dir).join(hook.as_str());
        if file.exists()
            && block_roots(&String::from_utf8_lossy(&fs::read(file)?))
                .into_iter()
                .any(|r| owner.owns(r))
        {
            return Ok(true);
        }
    }
    Ok(false)
}
