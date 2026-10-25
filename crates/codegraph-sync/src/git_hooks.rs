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

fn block_roots(content: &str) -> Vec<Option<&str>> {
    let mut roots = Vec::new();
    let mut current: Option<Option<&str>> = None;
    for line in content.split('\n').map(js_trim) {
        if line == BEGIN {
            current = Some(None);
            continue;
        }
        if current.is_none() {
            continue;
        }
        if let Some(root) = line.strip_prefix(ROOT_PREFIX) {
            current = Some(Some(root));
            continue;
        }
        if line == END {
            roots.push(current.take().unwrap());
        }
    }
    roots
}
fn strip_blocks(content: &str, owner: &Owner<'_>) -> String {
    let lines: Vec<_> = content.split('\n').collect();
    let mut kept = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if js_trim(lines[i]) != BEGIN {
            kept.push(lines[i]);
            i += 1;
            continue;
        }
        let mut end = i;
        while end < lines.len() && js_trim(lines[end]) != END {
            end += 1;
        }
        let block = &lines[i..(end + 1).min(lines.len())];
        let root = block
            .iter()
            .find_map(|l| js_trim(l).strip_prefix(ROOT_PREFIX));
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
