//! Which files the index holds — port of the scan half of
//! `src/extraction/index.ts`: the built-in ignore defaults, `.gitignore` and git
//! exclude reading, embedded-repo discovery, the git-backed listing, the
//! filesystem-walk fallback, and git change detection.
//!
//! Every function mirrors its TypeScript counterpart step for step — including
//! which failures abort the scan and which are swallowed — because the file set
//! this produces is the input of the whole index.

use super::config::{load_exclude_patterns, load_extension_overrides, load_include_ignored_patterns, load_include_patterns};
use super::git::{git30, git5, gitdir_pointer, resolve_git_dir, run_git, MAX_BUFFER};
use super::ignore::{Ignore, IgnoreError};
use crate::languages::{detect_language, is_source_file};
use codegraph_types::jscompat::utf16_len;
use codegraph_types::log::{log_debug, log_warn};
use codegraph_types::paths::{home_dir, is_codegraph_data_dir, normalize_path, resolve_path};
use codegraph_types::Language;
use indexmap::{IndexMap, IndexSet};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

type Res<T> = Result<T, String>;

// --- built-in ignore defaults ---------------------------------------------------

/// Directory names that are dependency, build, cache, or tooling output across
/// the supported languages, excluded by default so the graph reflects your code
/// (issue #407). First-party-prone names (`packages`, `lib`, `app`, `bin`, `src`,
/// `deps`, `env`, `tmp`, `storage`, `Library`) are deliberately NOT listed.
const DEFAULT_IGNORE_DIRS: &[&str] = &[
    // JS / TS — dependency directories
    "node_modules", "bower_components", "jspm_packages", "web_modules", ".yarn", ".pnpm-store",
    // JS / TS — framework & bundler build / cache / deploy output
    ".next", ".nuxt", ".svelte-kit", ".turbo", ".vite", ".parcel-cache", ".angular", ".docusaurus",
    "storybook-static", ".vinxi", ".nitro", "out-tsc", ".vercel", ".netlify", ".wrangler",
    // Build output (common across ecosystems)
    "dist", "build", "out", ".output",
    // Test / coverage
    "coverage", ".nyc_output",
    // Python
    "__pycache__", "__pypackages__", ".venv", "venv", ".pixi", ".pdm-build", ".mypy_cache", ".pytest_cache",
    ".ruff_cache", ".tox", ".nox", ".hypothesis", ".ipynb_checkpoints", ".eggs",
    // Rust / JVM (Maven, Gradle, Scala)
    "target", ".gradle",
    // .NET
    "obj",
    // Vendored deps (Go, PHP/Composer, Ruby/Bundler)
    "vendor",
    // Swift / iOS
    ".build", "Pods", "Carthage", "DerivedData", ".swiftpm",
    // Dart / Flutter
    ".dart_tool", ".pub-cache",
    // Native (Android NDK, C/C++ deps)
    ".cxx", ".externalNativeBuild", "vcpkg_installed",
    // Scala tooling
    ".bloop", ".metals",
    // Lua / Luau (LuaRocks)
    "lua_modules", ".luarocks",
    // Delphi / RAD Studio IDE backups
    "__history", "__recovery",
    // Generic cache
    ".cache",
];

/// Android resource directory types: a `res/` tree holds only non-code resources (#1047).
const ANDROID_RES_TYPES: &[&str] = &["anim", "animator", "color", "drawable", "font", "layout", "menu", "mipmap", "navigation", "transition", "values", "xml"];

/// `build` is also a legal JVM package segment (#1642).
const JVM_BUILD_PACKAGE_DIRS: &[&str] = &["**/src/*/java/**/build/", "**/src/*/kotlin/**/build/", "**/src/*/scala/**/build/"];

/// Gitignore-style patterns for the matcher: the dirs above plus a few globs.
fn default_ignore_patterns() -> &'static [String] {
    static PATTERNS: OnceLock<Vec<String>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let mut p: Vec<String> = DEFAULT_IGNORE_DIRS.iter().map(|d| format!("{d}/")).collect();
        p.push("*.egg-info/".into());
        p.push("cmake-build-*/".into());
        p.push("bazel-*/".into());
        p.extend(ANDROID_RES_TYPES.iter().map(|t| format!("**/res/{t}*/")));
        p.extend(JVM_BUILD_PACKAGE_DIRS.iter().map(|d| format!("!{d}")));
        p.push("!**/src/**/build/".into());
        p
    })
}

// --- reading ignore sources -------------------------------------------------------

/// Read a `.gitignore` and return patterns safe to hand to the matcher — never
/// failing, even when the file isn't real gitignore text (issue #682): a file that
/// isn't valid UTF-8 is skipped whole, and a line that cannot compile to a regex
/// is dropped alone. Either way a warning that names the file is logged.
pub fn read_gitignore_patterns(gi_path: &Path) -> String {
    let Ok(buf) = std::fs::read(gi_path) else {
        return String::new();
    };
    let Some(content) = (!buf.contains(&0)).then(|| std::str::from_utf8(&buf).ok()).flatten() else {
        log_warn(
            "Ignoring a .gitignore that is not valid UTF-8 text — it may have been encrypted in place by endpoint-security software. Indexing continues without it.",
            &[("file", gi_path.display().to_string())],
        );
        return String::new();
    };
    // Fast path: one `.ignores()` call forces the library to compile every rule it reaches.
    let probe = |text: &str| Ignore::new().add(text).ignores(".codegraph-probe").is_ok();
    if probe(content) {
        return content.to_string();
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut dropped = 0;
    for line in content.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)) {
        if probe(line) {
            kept.push(line);
        } else {
            dropped += 1;
        }
    }
    if dropped > 0 {
        log_warn(
            &format!("Skipped {dropped} unparseable pattern(s) in a .gitignore; the rest are applied."),
            &[("file", gi_path.display().to_string())],
        );
    }
    kept.join("\n")
}

/// Expand a leading `~/` the way git does for `core.excludesFile`.
fn expand_user_path(p: &str) -> PathBuf {
    let home = home_dir().unwrap_or_default();
    if p == "~" {
        return home;
    }
    match p.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(p),
    }
}

/// Root-relative exclude patterns from git sources that are NOT the root
/// `.gitignore`: `.git/info/exclude` and `core.excludesFile` (#1728).
fn read_git_exclude_extra_patterns(root_dir: &Path) -> String {
    let mut chunks: Vec<String> = Vec::new();
    if let Some(git_dir) = resolve_git_dir(root_dir) {
        let exclude_path = git_dir.join("info").join("exclude");
        if exclude_path.exists() {
            let patterns = read_gitignore_patterns(&exclude_path);
            if !patterns.is_empty() {
                chunks.push(patterns);
            }
        }
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let root = root_dir.to_string_lossy();
    if let Ok(out) = run_git(&cwd, &["-C", &root, "config", "--get", "core.excludesFile"], Duration::from_secs(5), MAX_BUFFER) {
        let configured = codegraph_types::jscompat::js_trim(&out);
        if !configured.is_empty() {
            let abs = expand_user_path(configured);
            if abs.exists() {
                let patterns = read_gitignore_patterns(&abs);
                if !patterns.is_empty() {
                    chunks.push(patterns);
                }
            }
        }
    }
    chunks.join("\n")
}

/// Directories `git ls-files -o -i --exclude-standard --directory` reports as
/// ignored-untracked, relative to `root_dir` and trailing-slashed (#1728). Each
/// one is a path, not a pattern: see [`ScopeIgnore`].
fn list_git_ignored_directories(root_dir: &Path) -> Vec<String> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let root = root_dir.to_string_lossy();
    let args = ["-C", root.as_ref(), "ls-files", "-z", "-o", "-i", "--exclude-standard", "--directory"];
    let Ok(out) = run_git(&cwd, &args, Duration::from_secs(60), MAX_BUFFER) else {
        return Vec::new();
    };
    // `./` is git's "this whole directory" for a root that is itself ignored (#936); the root's
    // own files stay in scope.
    out.split('\0')
        .filter(|e| !is_whole_cwd_entry(e))
        .map(normalize_path)
        .map(|e| if e.ends_with('/') { e } else { format!("{e}/") })
        .collect()
}

/// A matcher seeded with the built-in defaults, merged with the project's root
/// `.gitignore` (so a negation there, e.g. `!vendor/`, overrides a default), plus
/// git's other root-relative exclude files — so watcher / walk scope matches
/// `git ls-files --exclude-standard`. Shared by both enumeration paths.
pub fn build_default_ignore(root_dir: &Path) -> Ignore {
    let mut ig = Ignore::new();
    ig.add_patterns(default_ignore_patterns());
    let mut project_rules = Ignore::new();
    let root_gitignore = root_dir.join(".gitignore");
    if root_gitignore.exists() {
        let patterns = read_gitignore_patterns(&root_gitignore);
        ig.add(&patterns);
        project_rules.add(&patterns);
    }
    let extra = read_git_exclude_extra_patterns(root_dir);
    if !extra.is_empty() {
        ig.add(&extra);
        project_rules.add(&extra);
    }
    ig.with_src_build_exclusion(Some(project_rules))
}

/// Defaults-only matcher (no root `.gitignore`): used wherever the parent repo's
/// own rules must NOT apply — inside embedded child repos (#514).
pub fn defaults_only_ignore() -> Ignore {
    let mut ig = Ignore::new();
    ig.add_patterns(default_ignore_patterns());
    ig.with_src_build_exclusion(None)
}

fn matcher_of(patterns: &[String]) -> Option<Ignore> {
    if patterns.is_empty() {
        return None;
    }
    let mut ig = Ignore::new();
    ig.add_patterns(patterns);
    Some(ig)
}

/// The `includeIgnored` matcher; none when the project opted in nothing.
fn load_include_ignored_matcher(root_dir: &Path) -> Option<Ignore> {
    matcher_of(&load_include_ignored_patterns(root_dir))
}

/// The `exclude` matcher: paths kept OUT of the index even when git-tracked (#999).
pub fn load_exclude_matcher(root_dir: &Path) -> Option<Ignore> {
    matcher_of(&load_exclude_patterns(root_dir))
}

/// The `include` matcher: source forced INTO the index despite `.gitignore`.
pub fn load_include_matcher(root_dir: &Path) -> Option<Ignore> {
    matcher_of(&load_include_patterns(root_dir))
}

// --- `include` whitelist -------------------------------------------------------------

fn has_glob_meta(segment: &str) -> bool {
    segment.chars().any(|c| matches!(c, '*' | '?' | '[' | ']' | '{' | '}' | '!'))
}

/// The static directory prefix of each `include` pattern — the literal leading
/// path up to the first glob segment — trailing-slashed. A pattern that starts
/// with a glob yields `""`, meaning "walk the whole tree". Roots nested under a
/// broader root are collapsed.
pub fn include_static_roots(patterns: &[String]) -> Vec<String> {
    let mut roots: IndexSet<String> = IndexSet::new();
    for pattern in patterns {
        let mut p = pattern.trim_start_matches('/');
        let trailing_slash = p.ends_with('/');
        if trailing_slash {
            p = &p[..p.len() - 1];
        }
        let segs: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
        let mut lead: Vec<&str> = Vec::new();
        for s in &segs {
            if has_glob_meta(s) {
                break;
            }
            lead.push(s);
        }
        let had_wildcard = lead.len() < segs.len();
        // A wholly-literal pattern with no trailing slash names a file (or a dir we
        // can't tell apart) — walk the containing dir and let the matcher pick it.
        if !had_wildcard && !trailing_slash && !lead.is_empty() {
            lead.pop();
        }
        if lead.is_empty() {
            return vec![String::new()];
        }
        roots.insert(format!("{}/", lead.join("/")));
    }
    let all: Vec<String> = roots.into_iter().collect();
    all.iter().filter(|r| !all.iter().any(|other| other != *r && r.starts_with(other.as_str()))).cloned().collect()
}

/// Actively discover the source files an `include` whitelist forces in: a
/// filtered filesystem walk of just the opted-in subtrees. `.gitignore` is
/// deliberately NOT consulted — overriding it is the point — but built-in
/// default-ignored dirs, `.git`, and CodeGraph's data dir are never resurfaced,
/// and an explicit `exclude` always wins.
fn collect_included_files(
    root_dir: &Path,
    include: &Ignore,
    exclude: Option<&Ignore>,
    roots: &[String],
    overrides: &HashMap<String, Language>,
) -> Res<IndexSet<String>> {
    struct Walk<'a> {
        root_dir: &'a Path,
        include: &'a Ignore,
        exclude: Option<&'a Ignore>,
        overrides: &'a HashMap<String, Language>,
        defaults: Ignore,
        visited: HashSet<PathBuf>,
        out: IndexSet<String>,
    }
    impl Walk<'_> {
        fn consider(&mut self, abs: &Path, rel: &str, is_dir: bool) -> Res<()> {
            if is_dir {
                if self.defaults.ignores(&format!("{rel}/"))? {
                    return Ok(()); // never node_modules/dist/… via include
                }
                // An explicit `exclude` wins; prune the whole subtree here.
                if let Some(ex) = self.exclude {
                    if ex.ignores(&format!("{rel}/"))? {
                        return Ok(());
                    }
                }
                self.walk(abs)
            } else {
                if self.defaults.ignores(rel)? || !self.include.ignores(rel)? {
                    return Ok(());
                }
                if let Some(ex) = self.exclude {
                    if ex.ignores(rel)? {
                        return Ok(());
                    }
                }
                if !is_source_file(rel, Some(self.overrides), Some(self.root_dir)) {
                    return Ok(());
                }
                self.out.insert(rel.to_string());
                Ok(())
            }
        }

        fn walk(&mut self, abs_dir: &Path) -> Res<()> {
            let Ok(real) = std::fs::canonicalize(abs_dir) else { return Ok(()) };
            if !self.visited.insert(real) {
                return Ok(()); // symlink-cycle guard
            }
            let Ok(entries) = std::fs::read_dir(abs_dir) else { return Ok(()) };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == ".git" || is_codegraph_data_dir(&name) {
                    continue;
                }
                let abs = abs_dir.join(&name);
                let rel = normalize_path(&relative_to(self.root_dir, &abs));
                if rel.is_empty() || rel.starts_with("..") {
                    continue;
                }
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_symlink() {
                    if let Ok(st) = std::fs::canonicalize(&abs).and_then(std::fs::metadata) {
                        self.consider(&abs, &rel, st.is_dir())?;
                    }
                    continue;
                }
                self.consider(&abs, &rel, ft.is_dir())?;
            }
            Ok(())
        }
    }
    let mut w = Walk { root_dir, include, exclude, overrides, defaults: defaults_only_ignore(), visited: HashSet::new(), out: IndexSet::new() };
    for root in roots {
        let dir = if root.is_empty() { root_dir.to_path_buf() } else { root_dir.join(root) };
        w.walk(&dir)?;
    }
    Ok(w.out)
}

/// `path.relative(from, to)` for a `to` that is lexically below `from`.
fn relative_to(from: &Path, to: &Path) -> String {
    to.strip_prefix(from).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The included source files (`codegraph.json` `include`) for a scan root, or an
/// empty set when nothing is force-included.
fn collect_included_files_for_root(root_dir: &Path) -> Res<IndexSet<String>> {
    let Some(include) = load_include_matcher(root_dir) else {
        return Ok(IndexSet::new());
    };
    let roots = include_static_roots(&load_include_patterns(root_dir));
    collect_included_files(root_dir, &include, load_exclude_matcher(root_dir).as_ref(), &roots, &load_extension_overrides(root_dir))
}

// --- git repository structure ------------------------------------------------------------

/// `git ls-files --directory` emits the literal `./` when the command's own cwd
/// is a wholly-ignored directory; it is not a real nested path (#936).
fn is_whole_cwd_entry(entry: &str) -> bool {
    matches!(entry, "./" | "." | "")
}

/// The gitignored DIRECTORIES of a repo (collapsed, trailing-slash form), relative
/// to `repo_dir` — invisible to every other listing, and exactly where a
/// multi-repo workspace keeps its nested project repos (#514).
fn list_ignored_dirs(repo_dir: &Path) -> Vec<String> {
    match git30(repo_dir, &["ls-files", "-z", "-o", "-i", "--exclude-standard", "--directory"]) {
        Ok(out) => out.split('\0').filter(|e| e.ends_with('/') && !is_whole_cwd_entry(e)).map(str::to_string).collect(),
        Err(_) => Vec::new(),
    }
}

/// Max directory depth searched below an ignored dir for nested `.git` roots.
const EMBEDDED_REPO_SEARCH_DEPTH: usize = 4;
/// Max directories examined per search — a huge ignored data dir must never stall a scan.
const EMBEDDED_REPO_SEARCH_ENTRIES: usize = 2000;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum GitDirKind {
    Embedded,
    Worktree,
    None,
}

/// Classify a directory's `.git` entry for embedded-repo discovery: a `.git`
/// directory is an embedded clone (#193, #514); a `.git` file pointing into some
/// repo's `.git/worktrees/<name>` (or `.git/modules/<m>/worktrees/<name>`) is a
/// worktree — a second view of a repo already indexed (#848, #945); any other
/// pointer is a submodule checkout, distinct code.
pub fn classify_git_dir(abs_dir: &Path) -> GitDirKind {
    let git_path = abs_dir.join(".git");
    let Ok(meta) = std::fs::metadata(&git_path) else { return GitDirKind::None };
    if meta.is_dir() {
        return GitDirKind::Embedded;
    }
    if !meta.is_file() {
        return GitDirKind::None;
    }
    if let Ok(bytes) = std::fs::read(&git_path) {
        if let Some(gitdir) = gitdir_pointer(&String::from_utf8_lossy(&bytes)) {
            if is_worktree_gitdir(&gitdir) {
                return GitDirKind::Worktree;
            }
        }
    }
    GitDirKind::Embedded
}

/// `/(^|[\\/])\.git[\\/](modules[\\/][^\\/]+[\\/])?worktrees[\\/]/`
fn is_worktree_gitdir(gitdir: &str) -> bool {
    let sep = |c: char| c == '/' || c == '\\';
    let mut from = 0;
    while let Some(i) = gitdir[from..].find(".git") {
        let at = from + i;
        from = at + 1;
        let before_ok = at == 0 || gitdir[..at].chars().next_back().is_some_and(sep);
        if !before_ok {
            continue;
        }
        let rest = &gitdir[at + 4..];
        let Some(rest) = rest.strip_prefix(sep) else { continue };
        let tail = |r: &str| r.strip_prefix("worktrees").and_then(|t| t.strip_prefix(sep)).is_some();
        if tail(rest) {
            return true;
        }
        if let Some(after_modules) = rest.strip_prefix("modules").and_then(|t| t.strip_prefix(sep)) {
            if let Some(end) = after_modules.find(sep) {
                if end > 0 && tail(&after_modules[end + 1..]) {
                    return true;
                }
            }
        }
    }
    false
}

/// Find git repositories nested under `abs_dir` (inclusive), shallow bounded BFS.
/// Stops descending at each repo root found; skips default-ignored dirs and
/// CodeGraph data dirs; depth- and entry-capped.
pub fn find_nested_git_repos(abs_dir: &Path, rel_prefix: &str) -> Vec<String> {
    let mut found = Vec::new();
    let defaults = defaults_only_ignore();
    let mut queue: VecDeque<(PathBuf, String, usize)> = VecDeque::new();
    queue.push_back((abs_dir.to_path_buf(), rel_prefix.to_string(), 0));
    let mut examined = 0;
    while let Some((abs, rel, depth)) = queue.pop_front() {
        examined += 1;
        if examined > EMBEDDED_REPO_SEARCH_ENTRIES {
            log_debug("Embedded-repo search entry cap hit — deeper repos (if any) not discovered", &[("under", rel_prefix.to_string())]);
            break;
        }
        match classify_git_dir(&abs) {
            GitDirKind::Worktree => continue, // a worktree duplicates an already-indexed repo (#848)
            GitDirKind::Embedded => {
                found.push(rel);
                continue; // its own git handles everything below
            }
            GitDirKind::None => {}
        }
        if depth >= EMBEDDED_REPO_SEARCH_DEPTH {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&abs) else { continue };
        for entry in entries.flatten() {
            // Dirent.isDirectory(): a symlink to a directory is not one.
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".git" || is_codegraph_data_dir(&name) {
                continue;
            }
            let child_rel = format!("{rel}{name}/");
            if defaults.ignores(&child_rel).unwrap_or(false) {
                continue;
            }
            queue.push_back((abs.join(&name), child_rel, depth + 1));
        }
    }
    found
}

// --- workspace scope ---------------------------------------------------------------------------

/// Workspace-scope ignore matcher. Ordinary paths get the root's matcher; paths
/// inside an EMBEDDED repo get that repo's own matcher (the parent's `.gitignore`
/// hides a child repo from git, not from the index, #514). A directory path that
/// is an ancestor of an embedded root is never ignored, so directory-pruning
/// callers still descend to reach it. Shared by the indexer and the watcher so
/// their scope cannot diverge (#1728).
pub struct ScopeIgnore {
    root_matcher: Ignore,
    /// Longest root first, so paths in nested embedded repos hit the innermost matcher.
    embedded: Vec<(String, Ignore)>,
    defaults: Ignore,
    exclude: Option<Ignore>,
    include: Option<Ignore>,
    include_roots: Vec<String>,
    /// The directories `git ls-files --exclude-standard` reports as ignored: root-relative,
    /// trailing-slashed paths, each matched as a literal path prefix, never as a gitignore
    /// pattern. As a pattern, `src/` matched every `src/` below the root too, hiding a whole Maven
    /// monorepo's sources (#2849), and a folder named `[draft]` read as a glob.
    git_ignored_dirs: HashSet<String>,
}

impl ScopeIgnore {
    pub fn new(
        root_matcher: Ignore,
        mut embedded: Vec<(String, Ignore)>,
        exclude: Option<Ignore>,
        include: Option<Ignore>,
        include_roots: Vec<String>,
    ) -> ScopeIgnore {
        embedded.sort_by(|a, b| utf16_len(&b.0).cmp(&utf16_len(&a.0)));
        ScopeIgnore { root_matcher, embedded, defaults: defaults_only_ignore(), exclude, include, include_roots, git_ignored_dirs: HashSet::new() }
    }

    /// The same matcher with the directories git reports as ignored (see `git_ignored_dirs`).
    pub fn with_git_ignored_dirs(mut self, dirs: impl IntoIterator<Item = String>) -> ScopeIgnore {
        self.git_ignored_dirs = dirs.into_iter().collect();
        self
    }

    /// True when `rel` is one of `git_ignored_dirs` or lies inside one.
    fn in_git_ignored_dir(&self, rel: &str) -> bool {
        !self.git_ignored_dirs.is_empty() && rel.match_indices('/').any(|(slash, _)| self.git_ignored_dirs.contains(&rel[..=slash]))
    }

    pub fn ignores(&self, rel: &str) -> Result<bool, IgnoreError> {
        // User `exclude` (#999) wins over everything, tracked files included.
        if let Some(ex) = &self.exclude {
            if ex.ignores(rel)? {
                return Ok(true);
            }
        }
        // User `include`: force first-party source in despite `.gitignore`, but never
        // resurface a built-in default-ignored dir.
        if let Some(inc) = &self.include {
            if !self.defaults.ignores(rel)? {
                if rel.ends_with('/') {
                    // A directory on (or leading to) an included subtree stays walkable.
                    if self.include_roots.iter().any(|r| r.starts_with(rel) || rel.starts_with(r.as_str())) {
                        return Ok(false);
                    }
                } else if inc.ignores(rel)? {
                    return Ok(false);
                }
            }
        }
        for (root, matcher) in &self.embedded {
            if let Some(inner) = rel.strip_prefix(root.as_str()) {
                if inner.is_empty() {
                    return Ok(false);
                }
                // Built-in defaults apply to the FULL path uniformly (#407).
                return Ok(self.defaults.ignores(rel)? || matcher.ignores(inner)?);
            }
        }
        // Never prune a directory that leads to an embedded repo.
        if rel.ends_with('/') && self.embedded.iter().any(|(root, _)| root.starts_with(rel)) {
            return Ok(false);
        }
        Ok(self.root_matcher.ignores(rel)? || self.in_git_ignored_dir(rel))
    }
}

/// Build the workspace-scope matcher. When the caller already knows the embedded
/// roots (the scanner discovers them during collection) pass them to skip
/// rediscovery; otherwise they're discovered here (the watcher path).
pub fn build_scope_ignore(root_dir: &Path, embedded_roots: Option<Vec<String>>) -> Res<ScopeIgnore> {
    let roots = match embedded_roots {
        Some(r) => r,
        None => discover_embedded_repo_roots(root_dir)?,
    };
    let include = load_include_matcher(root_dir);
    // The root matcher has defaults + root `.gitignore` + info/exclude + core.excludesFile. The
    // ignored-untracked directories git reports join them, so nested `.gitignore` effects prune
    // the watcher identically to the indexer (#1728) — as literal paths, not patterns (#2849).
    let root_matcher = build_default_ignore(root_dir);
    let embedded = roots.iter().map(|root| (root.clone(), build_default_ignore(&root_dir.join(root)))).collect();
    let include_roots = if include.is_some() { include_static_roots(&load_include_patterns(root_dir)) } else { Vec::new() };
    Ok(ScopeIgnore::new(root_matcher, embedded, load_exclude_matcher(root_dir), include, include_roots)
        .with_git_ignored_dirs(list_git_ignored_directories(root_dir)))
}

/// The `codegraph.json` `includeIgnored` pattern that opts in exactly the repo at `repo_dir`, a
/// root-relative, trailing-slashed path from [`find_unindexed_ignored_repos`]. Patterns are
/// gitignore syntax, so the path is anchored to the root (a bare `child/` also matches every
/// `child/` below it) and its `*` and `[` are escaped (#2849); a `?` stays a wildcard.
pub fn include_ignored_pattern_for(repo_dir: &str) -> String {
    let mut out = String::from("/");
    for c in repo_dir.chars() {
        if c == '*' || c == '[' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Whether an embedded repo found as a tracked gitlink (mode 160000, #1031/#1033)
/// must be SKIPPED rather than indexed: it sits in a built-in default-ignored
/// location, or the parent repo's `.gitignore` covers its path and the project
/// did not opt that path in via `includeIgnored` (#1065).
fn gitlink_embedded_repo_skipped(rel_dir: &str, prefix: &str, defaults: &Ignore, repo_ignore: &Ignore, include_ignored: Option<&Ignore>) -> Res<bool> {
    if defaults.ignores(rel_dir)? {
        return Ok(true); // default-ignored — never index, opt-in can't revive
    }
    if !repo_ignore.ignores(rel_dir)? {
        return Ok(false); // not ignored at all — index as before
    }
    // Gitignored by the repo's own rules — skip unless the project opted it in.
    Ok(match include_ignored {
        Some(inc) => !inc.ignores(&normalize_path(&format!("{prefix}{rel_dir}")))?,
        None => true,
    })
}

/// `git ls-files -z -s`, expanding submodules where git allows it. Older git
/// (< 2.36) refuses `--recurse-submodules` with `-s`; retry without it (#1549).
fn ls_files_staged(cwd: &Path) -> Res<String> {
    git30(cwd, &["ls-files", "-z", "-s", "--recurse-submodules"]).or_else(|_| git30(cwd, &["ls-files", "-z", "-s"]))
}

/// Standalone discovery of every embedded repo root under `root_dir` (relative,
/// trailing-slashed): the untracked kind (#193) always, the gitignored kind (#514)
/// only for directories opted in via `includeIgnored`. Recursive. Returns an
/// empty list for non-git roots: the filesystem walk handles nested repos there.
pub fn discover_embedded_repo_roots(root_dir: &Path) -> Res<Vec<String>> {
    if git5(root_dir, &["rev-parse", "--git-dir"]).is_err() {
        return Ok(Vec::new());
    }
    let defaults = defaults_only_ignore();
    let include_ignored = load_include_ignored_matcher(root_dir);
    let mut out = Vec::new();
    visit_embedded(root_dir, "", &defaults, include_ignored.as_ref(), &mut out)?;
    Ok(out)
}

fn visit_embedded(repo_abs: &Path, prefix: &str, defaults: &Ignore, include_ignored: Option<&Ignore>, out: &mut Vec<String>) -> Res<()> {
    let mut candidates: Vec<String> = Vec::new();
    // Untracked listing: a failure here (or a matcher error mid-listing) abandons
    // the rest of this listing but keeps what was found; ignored-side discovery
    // still runs.
    let _ = (|| -> Res<()> {
        let o = git30(repo_abs, &["ls-files", "-z", "-o", "--exclude-standard", "--directory"])?;
        for e in o.split('\0') {
            if e.ends_with('/') && !is_whole_cwd_entry(e) && !defaults.ignores(e)? {
                candidates.extend(find_nested_git_repos(&repo_abs.join(e), e));
            }
        }
        Ok(())
    })();
    // Unexpanded gitlinks (mode 160000) with a real checkout on disk (#1031, #1033).
    let _ = (|| -> Res<()> {
        let staged = ls_files_staged(repo_abs)?;
        let repo_ignore = build_default_ignore(repo_abs);
        for entry in staged.split('\0') {
            if entry.is_empty() || !entry.starts_with("160000") {
                continue;
            }
            let Some(tab) = entry.find('\t') else { continue };
            let rel = &entry[tab + 1..];
            let rel_dir = if rel.ends_with('/') { rel.to_string() } else { format!("{rel}/") };
            if gitlink_embedded_repo_skipped(&rel_dir, prefix, defaults, &repo_ignore, include_ignored)? {
                continue;
            }
            if classify_git_dir(&repo_abs.join(rel)) == GitDirKind::Embedded {
                candidates.push(rel_dir);
            }
        }
        Ok(())
    })();
    candidates.extend(find_ignored_embedded_repos(repo_abs, include_ignored, prefix)?);
    for rel in candidates {
        let full = normalize_path(&format!("{prefix}{rel}"));
        out.push(full.clone());
        visit_embedded(&repo_abs.join(&rel), &full, defaults, include_ignored, out)?;
    }
    Ok(())
}

/// Cap on how many skipped gitignored repos the CLI hint enumerates.
const UNINDEXED_IGNORED_REPO_HINT_CAP: usize = 100;

/// The INVERSE of the gitignored side of [`discover_embedded_repo_roots`]: nested
/// git repositories under a gitignored directory that the project has NOT opted
/// into via `includeIgnored` — real repos the default `init`/`index` deliberately
/// skips (#970, #976). The CLI turns that silent empty index into an actionable
/// hint. Returns `[]` for a non-git root; bounded.
pub fn find_unindexed_ignored_repos(root_dir: &Path) -> Res<Vec<String>> {
    if git5(root_dir, &["rev-parse", "--git-dir"]).is_err() {
        return Ok(Vec::new());
    }
    let defaults = defaults_only_ignore();
    let include_ignored = load_include_ignored_matcher(root_dir);
    let mut repos = Vec::new();
    for dir in list_ignored_dirs(root_dir) {
        if defaults.ignores(&dir)? {
            continue; // node_modules etc. — never project code
        }
        if let Some(inc) = &include_ignored {
            if inc.ignores(&normalize_path(&dir))? {
                continue; // already opted in — nothing to nag about
            }
        }
        for repo in find_nested_git_repos(&root_dir.join(&dir), &dir) {
            // Per-repo opt-in check: a child pattern (`repos/a/`) doesn't match the
            // parent dir above but DOES cover this repo (#1295).
            if let Some(inc) = &include_ignored {
                if inc.ignores(&normalize_path(&repo))? {
                    continue;
                }
            }
            repos.push(repo);
            if repos.len() >= UNINDEXED_IGNORED_REPO_HINT_CAP {
                return Ok(repos);
            }
        }
    }
    Ok(repos)
}

/// Embedded repos hidden by `repo_dir`'s OWN gitignore rules: for each gitignored
/// directory, search for nested `.git` roots. OPT-IN ONLY — returns `[]` unless the
/// project opted the directory in via `includeIgnored` (#970, #976, #622, #699).
/// `prefix` is the scan-root-relative path of `repo_dir`.
fn find_ignored_embedded_repos(repo_dir: &Path, include_ignored: Option<&Ignore>, prefix: &str) -> Res<Vec<String>> {
    let Some(include_ignored) = include_ignored else {
        return Ok(Vec::new());
    };
    let defaults = defaults_only_ignore();
    let mut repos = Vec::new();
    for dir in list_ignored_dirs(repo_dir) {
        if defaults.ignores(&dir)? {
            continue;
        }
        let nested = find_nested_git_repos(&repo_dir.join(&dir), &dir);
        if include_ignored.ignores(&normalize_path(&format!("{prefix}{dir}")))? {
            // The whole ignored dir is opted in — every nested repo under it counts.
            repos.extend(nested);
        } else {
            // A single gitignore rule often covers the PARENT of the opted-in repos
            // while `includeIgnored` names the child (#1295): match each root.
            for r in nested {
                if include_ignored.ignores(&normalize_path(&format!("{prefix}{r}")))? {
                    repos.push(r);
                }
            }
        }
    }
    Ok(repos)
}

// --- git-backed listing -----------------------------------------------------------------------------

/// Collect git-visible files (tracked + untracked, `.gitignore`-respected) from the
/// repository rooted at `repo_dir`, adding each to `files` with `prefix` prepended
/// so paths stay relative to the original scan root. Recurses into embedded git
/// repositories (#193) and unexpanded gitlinks (#1031, #1033); every embedded repo
/// root (however found) is recorded in `embedded_roots` so callers can exempt its
/// files from the parent's own gitignore rules.
fn collect_git_files(
    repo_dir: &Path,
    prefix: &str,
    files: &mut IndexSet<String>,
    embedded_roots: &mut IndexSet<String>,
    include_ignored: Option<&Ignore>,
) -> Res<()> {
    // Tracked files. `--stage` carries each entry's mode, so unexpanded gitlinks
    // (160000) can be spotted and recursed into below; `-z` keeps non-ASCII paths
    // verbatim (#541). With -s the path follows a TAB after `<mode> <object> <stage>`.
    let mut gitlink_rels: Vec<String> = Vec::new();
    let tracked = ls_files_staged(repo_dir)?;
    for entry in tracked.split('\0') {
        if entry.is_empty() {
            continue;
        }
        let Some(tab) = entry.find('\t') else { continue };
        let rel = &entry[tab + 1..];
        if entry.starts_with("160000") {
            gitlink_rels.push(rel.to_string()); // recursed into below, not a source file itself
            continue;
        }
        files.insert(normalize_path(&format!("{prefix}{rel}")));
    }

    // Untracked files. Embedded git repos surface as a single "subdir/" entry that
    // git refuses to descend into — recurse into those as their own repos.
    let untracked = git30(repo_dir, &["ls-files", "-z", "-o", "--exclude-standard"])?;
    for rel in untracked.split('\0') {
        if rel.is_empty() {
            continue;
        }
        if rel.ends_with('/') {
            let child_dir = repo_dir.join(rel);
            // A worktree surfaces here as an opaque untracked dir too — skip it (#848);
            // never descend into default-ignored locations.
            if classify_git_dir(&child_dir) == GitDirKind::Embedded && !defaults_only_ignore().ignores(rel)? {
                embedded_roots.insert(normalize_path(&format!("{prefix}{rel}")));
                collect_git_files(&child_dir, &format!("{prefix}{rel}"), files, embedded_roots, include_ignored)?;
            }
            continue;
        }
        files.insert(normalize_path(&format!("{prefix}{rel}")));
    }

    // Gitlinks that --recurse-submodules left unexpanded and that have a real
    // working tree on disk: distinct first-party code, indexed as an embedded repo.
    if !gitlink_rels.is_empty() {
        let defaults = defaults_only_ignore();
        let repo_ignore = build_default_ignore(repo_dir);
        for rel in &gitlink_rels {
            let rel_dir = if rel.ends_with('/') { rel.clone() } else { format!("{rel}/") };
            if gitlink_embedded_repo_skipped(&rel_dir, prefix, &defaults, &repo_ignore, include_ignored)? {
                continue;
            }
            let child_dir = repo_dir.join(rel);
            if classify_git_dir(&child_dir) != GitDirKind::Embedded {
                continue;
            }
            embedded_roots.insert(normalize_path(&format!("{prefix}{rel_dir}")));
            collect_git_files(&child_dir, &format!("{prefix}{rel_dir}"), files, embedded_roots, include_ignored)?;
        }
    }

    // Embedded repos hidden by THIS repo's ignore rules never appear in any listing
    // above; recursed into only when the project opted the directory in.
    for rel in find_ignored_embedded_repos(repo_dir, include_ignored, prefix)? {
        embedded_roots.insert(normalize_path(&format!("{prefix}{rel}")));
        collect_git_files(&repo_dir.join(&rel), &format!("{prefix}{rel}"), files, embedded_roots, include_ignored)?;
    }
    Ok(())
}

/// All files visible to git (tracked + untracked but not ignored), `.gitignore`
/// respected at every level and embedded repos descended into. `None` when git
/// cannot answer (non-git project, parent repo ignores this root, any failure) so
/// callers fall back to a filesystem walk.
pub fn get_git_visible_files(root_dir: &Path) -> Option<IndexSet<String>> {
    match git_visible_files(root_dir) {
        Ok(files) => files,
        Err(error) => {
            log_debug(
                "git-based file listing unavailable — falling back to filesystem walk",
                &[("rootDir", root_dir.display().to_string()), ("error", error)],
            );
            None
        }
    }
}

fn git_visible_files(root_dir: &Path) -> Res<Option<IndexSet<String>>> {
    // When rootDir lives inside a parent git repo that ignores it, `git ls-files`
    // returns nothing — fall back to a filesystem walk.
    let git_root = git5(root_dir, &["rev-parse", "--show-toplevel"])?;
    let git_root = codegraph_types::jscompat::js_trim(&git_root).to_string();
    if resolve_path(&git_root).to_string_lossy() != resolve_path(root_dir).to_string_lossy() {
        let abs = resolve_path(root_dir);
        // `git check-ignore -q` exits 0 if the path IS ignored, 1 if not.
        if git5(root_dir, &["check-ignore", "-q", &abs.to_string_lossy()]).is_ok() {
            log_debug(
                "project root is gitignored by a parent repo — falling back to filesystem walk",
                &[("rootDir", root_dir.display().to_string()), ("gitRoot", git_root)],
            );
            return Ok(None);
        }
    }
    let mut files = IndexSet::new();
    let mut embedded_roots = IndexSet::new();
    collect_git_files(root_dir, "", &mut files, &mut embedded_roots, load_include_ignored_matcher(root_dir).as_ref())?;
    // Built-in defaults apply uniformly — to tracked files too (#407); files inside
    // an EMBEDDED repo are matched against that repo's own rules (#514).
    let ig = build_scope_ignore(root_dir, Some(embedded_roots.into_iter().collect()))?;
    let mut visible = IndexSet::new();
    for f in files {
        if !ig.ignores(&f)? {
            visible.insert(f);
        }
    }
    // Force-include first-party source the project whitelisted in `include`; these
    // are gitignored, so `git ls-files` never listed them.
    for f in collect_included_files_for_root(root_dir)? {
        visible.insert(f);
    }
    Ok(Some(visible))
}

// --- git change detection -----------------------------------------------------------------------------

/// Result of git-based change detection.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GitChanges {
    /// M, MM, AM — files to re-hash + re-index.
    pub modified: Vec<String>,
    /// `??` — new untracked files to index.
    pub added: Vec<String>,
    /// D — files to remove from the DB.
    pub deleted: Vec<String>,
}

/// Metadata key: the commit the index was last brought up to date at (#1829).
pub const INDEXED_AT_COMMIT_KEY: &str = "indexed_at_commit";

/// HEAD's commit sha, or none in a non-git repo or one with no commits yet.
pub fn get_git_head_sha(root_dir: &Path) -> Option<String> {
    let out = git5(root_dir, &["rev-parse", "HEAD"]).ok()?;
    let sha = codegraph_types::jscompat::js_trim(&out);
    (!sha.is_empty()).then(|| sha.to_string())
}

/// NUL-delimited status/path fields for every path committed between
/// `since_commit` and HEAD. Failures propagate: `[]` would falsely mean clean.
fn git_committed_changes_since(repo_dir: &Path, since_commit: &str) -> Res<Vec<String>> {
    let out = run_git(
        repo_dir,
        &["diff", "--relative", "--name-status", "--no-renames", "-z", since_commit, "HEAD", "--", "."],
        Duration::from_secs(10),
        MAX_BUFFER,
    )?;
    Ok(out.split('\0').map(str::to_string).collect())
}

/// Can an INDEX trust the git fast path, given the commit it was built at? False
/// means "fall back to the full scan": the stamp is unknown to this repo (history
/// moved under the index), or absent while the repo HAS commits (#1829).
pub fn can_trust_git_fast_path(root_dir: &Path, since_commit: Option<&str>) -> bool {
    let Some(head) = get_git_head_sha(root_dir) else {
        return true; // no commits (or not a git repo — caller handles that)
    };
    let Some(since) = since_commit.filter(|s| !s.is_empty()) else {
        return false;
    };
    if since == head {
        return true;
    }
    git5(root_dir, &["cat-file", "-e", &format!("{since}^{{commit}}")]).is_ok()
}

/// Use `git status` to detect changed files instead of scanning every file;
/// `None` on failure so callers fall back to a full scan. Recurses into embedded
/// repos (#193, #514, #622, #699). `since_commit` adds what has been committed
/// since the index was last up to date (#1829).
pub fn get_git_changed_files(root_dir: &Path, since_commit: Option<&str>) -> Option<GitChanges> {
    let mut changes = GitChanges::default();
    // Custom extension → language overrides, so change detection sees the same
    // custom-extension files the full index does.
    let overrides = load_extension_overrides(root_dir);
    let is_source = |file_path: &str| is_source_file(file_path, Some(&overrides), Some(root_dir));
    collect_git_status(
        root_dir,
        "",
        &mut changes,
        &is_source,
        load_include_ignored_matcher(root_dir).as_ref(),
        load_exclude_matcher(root_dir).as_ref(),
        since_commit.filter(|s| !s.is_empty()),
    )
    .ok()?;
    Some(changes)
}

fn collect_git_status(
    repo_dir: &Path,
    prefix: &str,
    out: &mut GitChanges,
    is_source: &dyn Fn(&str) -> bool,
    include_ignored: Option<&Ignore>,
    exclude: Option<&Ignore>,
    since_commit: Option<&str>,
) -> Res<()> {
    // `-uall` lists individual untracked files instead of collapsing an
    // entirely-untracked directory into one `?? dir/` entry (#1213).
    let output = run_git(repo_dir, &["status", "--porcelain", "--no-renames", "-z", "-uall"], Duration::from_secs(10), MAX_BUFFER)?;

    // This repo's own ignore rules — built-in defaults (#407) plus its .gitignore:
    // change detection must exclude the SAME files the full index does (#766).
    let ig = build_default_ignore(repo_dir);

    let classify = |out: &mut GitChanges, status_code: &str, rel: &str| -> Res<()> {
        let file_path = normalize_path(&format!("{prefix}{rel}"));
        if !is_source(&file_path) {
            return Ok(());
        }
        if status_code.contains('D') {
            // Deletions stay unfiltered: a newly-excluded dir's stale rows clean themselves up (#766).
            out.deleted.push(file_path);
            return Ok(());
        }
        // Added / modified files inside an excluded dir must not enter the index (#766).
        if ig.ignores(rel)? {
            return Ok(());
        }
        // User `exclude` (#999) is project-root-relative.
        if let Some(ex) = exclude {
            if ex.ignores(&file_path)? {
                return Ok(());
            }
        }
        if status_code == "??" {
            out.added.push(file_path);
        } else {
            // M, MM, AM, A (staged), etc. — treat as modified; the caller re-decides
            // added-vs-modified from the DB.
            out.modified.push(file_path);
        }
        Ok(())
    };

    let mut untracked_dirs: Vec<String> = Vec::new();
    for line in output.split('\0') {
        if line.chars().count() < 4 {
            continue; // Minimum: "XY file"
        }
        let status_code = &line[..2.min(line.len())];
        let rel = normalize_path(line.get(3..).unwrap_or(""));
        // Untracked directory entries may hide an embedded repo — recurse below.
        if status_code == "??" && rel.ends_with('/') {
            untracked_dirs.push(rel);
            continue;
        }
        classify(out, status_code, &rel)?;
    }

    // Committed but unindexed: everything between the commit this index was last
    // brought up to date at and HEAD (#1829). The stamp belongs to the ROOT repo.
    if let Some(since) = since_commit {
        let fields = git_committed_changes_since(repo_dir, since)?;
        let mut i = 0;
        while i + 1 < fields.len() {
            let code = format!("{} ", fields[i].chars().next().map(String::from).unwrap_or_default());
            classify(out, &code, &normalize_path(&fields[i + 1]))?;
            i += 2;
        }
    }

    // Recurse embedded repos found under untracked dirs, and gitignored ones the
    // project opted in via `includeIgnored`.
    for rel in &untracked_dirs {
        for repo_rel in find_nested_git_repos(&repo_dir.join(rel), rel) {
            collect_git_status(&repo_dir.join(&repo_rel), &format!("{prefix}{repo_rel}"), out, is_source, include_ignored, exclude, None)?;
        }
    }
    for rel in find_ignored_embedded_repos(repo_dir, include_ignored, prefix)? {
        collect_git_status(&repo_dir.join(&rel), &format!("{prefix}{rel}"), out, is_source, include_ignored, exclude, None)?;
    }
    Ok(())
}

// --- scanning ---------------------------------------------------------------------------------------------

/// What a scan saw but could not index, tallied by extension — a project of
/// unsupported files is otherwise indistinguishable from an empty one (#1502).
#[derive(Debug, Default, Clone)]
pub struct ScanSkipStats {
    /// Lowercased extension (with dot) → how many files carried it.
    pub unsupported_by_extension: IndexMap<String, usize>,
}

/// `path.extname(rel)`.
fn extname(rel: &str) -> &str {
    let base = &rel[rel.rfind('/').map_or(0, |i| i + 1)..];
    match base.rfind('.') {
        None | Some(0) => "",
        Some(_) if base == ".." => "",
        Some(i) => &base[i..],
    }
}

fn tally_skip(stats: &mut Option<&mut ScanSkipStats>, rel: &str) {
    let Some(stats) = stats.as_deref_mut() else { return };
    let ext = extname(rel).to_lowercase();
    if ext.is_empty() {
        return;
    }
    *stats.unsupported_by_extension.entry(ext).or_insert(0) += 1;
}

/// Progress callback: `(files so far, latest file)`.
pub type ScanProgress<'a> = &'a mut dyn FnMut(usize, &str);

/// Recursively scan a directory for source files. In git repos, uses `git
/// ls-files` (inherently respecting `.gitignore` at all levels) and keeps files
/// with a supported source extension; non-git projects fall back to a filesystem
/// walk that parses `.gitignore` itself.
pub fn scan_directory<'a>(root_dir: &'a Path, mut on_progress: Option<ScanProgress<'a>>, mut stats: Option<&'a mut ScanSkipStats>) -> Res<Vec<String>> {
    let overrides = load_extension_overrides(root_dir);
    if let Some(git_files) = get_git_visible_files(root_dir) {
        let mut files = Vec::new();
        for file_path in git_files {
            if is_source_file(&file_path, Some(&overrides), Some(root_dir)) {
                files.push(file_path.clone());
                if let Some(cb) = on_progress.as_deref_mut() {
                    cb(files.len(), &file_path);
                }
            } else {
                tally_skip(&mut stats, &file_path);
            }
        }
        return Ok(files);
    }
    scan_directory_walk(root_dir, on_progress, stats)
}

/// A `.gitignore` matcher scoped to the directory that declared it: patterns in a
/// nested `.gitignore` are relative to that directory.
struct ScopedIgnore {
    /// Root-relative directory (`""` for the root), no trailing slash.
    dir: String,
    ig: Ignore,
}

fn is_ignored(rel_full: &str, is_dir: bool, matchers: &[Arc<ScopedIgnore>]) -> Res<bool> {
    for m in matchers {
        let mut rel = if m.dir.is_empty() {
            rel_full.to_string()
        } else {
            match rel_full.strip_prefix(&m.dir).and_then(|r| r.strip_prefix('/')) {
                Some(r) => r.to_string(),
                None => continue, // not under this matcher's dir
            }
        };
        if rel.is_empty() || rel.starts_with("..") {
            continue;
        }
        if is_dir {
            rel.push('/'); // dir-only rules (e.g. `build/`) only match with the slash
        }
        if m.ig.ignores(&rel)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Filesystem walk fallback for non-git projects.
fn scan_directory_walk<'a>(root_dir: &'a Path, on_progress: Option<ScanProgress<'a>>, stats: Option<&'a mut ScanSkipStats>) -> Res<Vec<String>> {
    struct Walk<'a> {
        root_dir: &'a Path,
        files: Vec<String>,
        visited: HashSet<PathBuf>,
        overrides: HashMap<String, Language>,
        on_progress: Option<ScanProgress<'a>>,
        stats: Option<&'a mut ScanSkipStats>,
    }

    impl Walk<'_> {
        fn push_file(&mut self, rel: &str) {
            if is_source_file(rel, Some(&self.overrides), Some(self.root_dir)) {
                self.files.push(rel.to_string());
                if let Some(cb) = self.on_progress.as_deref_mut() {
                    cb(self.files.len(), rel);
                }
            } else {
                tally_skip(&mut self.stats, rel);
            }
        }

        fn walk(&mut self, dir: &Path, dir_rel: &str, matchers: &[Arc<ScopedIgnore>]) -> Res<()> {
            let real = match std::fs::canonicalize(dir) {
                Ok(r) => r,
                Err(_) => {
                    log_debug("Skipping unresolvable directory", &[("dir", dir.display().to_string())]);
                    return Ok(());
                }
            };
            if !self.visited.insert(real.clone()) {
                log_debug("Skipping already-visited directory (symlink cycle)", &[("dir", dir.display().to_string()), ("realDir", real.display().to_string())]);
                return Ok(());
            }
            // This directory's own .gitignore applies to everything below it. The
            // root's is already merged into the seeded base matcher.
            let own = if dir_rel.is_empty() { None } else { load_scoped_ignore(dir, dir_rel) };
            let mut active: Vec<Arc<ScopedIgnore>> = matchers.to_vec();
            if let Some(own) = own {
                active.push(Arc::new(own));
            }
            let entries = match std::fs::read_dir(dir) {
                Ok(e) => e,
                Err(error) => {
                    log_debug("Skipping unreadable directory", &[("dir", dir.display().to_string()), ("error", error.to_string())]);
                    return Ok(());
                }
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                // Never descend into git internals or any CodeGraph data directory (#636).
                if name == ".git" || is_codegraph_data_dir(&name) {
                    continue;
                }
                let full = dir.join(&name);
                let rel = if dir_rel.is_empty() { name.clone() } else { format!("{dir_rel}/{name}") };
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_symlink() {
                    let target = std::fs::canonicalize(&full).and_then(std::fs::metadata);
                    match target {
                        Ok(st) if st.is_dir() => {
                            if !is_ignored(&rel, true, &active)? {
                                self.walk(&full, &rel, &active)?;
                            }
                        }
                        Ok(st) if st.is_file() => {
                            if !is_ignored(&rel, false, &active)? {
                                self.push_file(&rel);
                            }
                        }
                        Ok(_) => {}
                        Err(_) => log_debug("Skipping broken symlink", &[("path", full.display().to_string())]),
                    }
                    continue;
                }
                if ft.is_dir() {
                    if !is_ignored(&rel, true, &active)? {
                        self.walk(&full, &rel, &active)?;
                    }
                } else if ft.is_file() && !is_ignored(&rel, false, &active)? {
                    self.push_file(&rel);
                }
            }
            Ok(())
        }
    }

    let mut w = Walk {
        root_dir,
        files: Vec::new(),
        visited: HashSet::new(),
        overrides: load_extension_overrides(root_dir),
        on_progress,
        stats,
    };
    // Seed a base matcher with the built-in default ignores (merged with the root
    // .gitignore so a negation can override). Nested .gitignores still layer per-dir.
    let mut base: Vec<Arc<ScopedIgnore>> = vec![Arc::new(ScopedIgnore { dir: String::new(), ig: build_default_ignore(root_dir) })];
    // Project `exclude` patterns (#999), rooted at the project.
    if let Some(exclude) = load_exclude_matcher(root_dir) {
        base.push(Arc::new(ScopedIgnore { dir: String::new(), ig: exclude }));
    }
    w.walk(root_dir, "", &base)?;

    // Force-include first-party source whitelisted in `include` — the walk honours
    // `.gitignore`, so anything gitignored was dropped; add it back (deduped).
    let included = collect_included_files_for_root(root_dir)?;
    if !included.is_empty() {
        let mut seen: HashSet<String> = w.files.iter().cloned().collect();
        for f in included {
            if seen.insert(f.clone()) {
                w.files.push(f);
            }
        }
    }
    Ok(w.files)
}

fn load_scoped_ignore(dir: &Path, dir_rel: &str) -> Option<ScopedIgnore> {
    let gi_path = dir.join(".gitignore");
    if !gi_path.exists() {
        return None;
    }
    // Defensive: a non-UTF-8 (DLP-encrypted) or uncompilable .gitignore is
    // skipped/filtered with a warning, never thrown (issue #682).
    let patterns = read_gitignore_patterns(&gi_path);
    if patterns.is_empty() {
        return None;
    }
    let mut ig = Ignore::new();
    ig.add(&patterns);
    Some(ScopedIgnore { dir: dir_rel.to_string(), ig })
}

/// The grammars to preload for a file set. Path-only detection calls every `.h`
/// file C, but parse-time detection can reclassify it as C++ or Objective-C, so
/// both are preloaded whenever C is (#1628); a Flow-typed `.js` is read with the
/// TSX grammar; an `.inc` path-detects as PHP but may read as Pascal (#2279).
pub fn preload_languages_for_files(files: &[String], overrides: Option<&HashMap<String, Language>>, root_dir: Option<&Path>) -> Vec<Language> {
    let mut languages: Vec<Language> = Vec::new();
    for f in files {
        let l = detect_language(f, None, overrides, root_dir);
        if !languages.contains(&l) {
            languages.push(l);
        }
    }
    let has = |languages: &[Language], name: &str| languages.iter().any(|l| l.as_str() == name);
    let lang = |name: &str| Language::parse(name).expect("known language");
    if (has(&languages, "javascript") || has(&languages, "jsx")) && !has(&languages, "tsx") {
        languages.push(lang("tsx"));
    }
    if has(&languages, "c") {
        for ambiguous in ["cpp", "objc"] {
            if !has(&languages, ambiguous) {
                languages.push(lang(ambiguous));
            }
        }
    }
    if !has(&languages, "pascal") && !overrides.is_some_and(|o| o.contains_key(".inc")) && files.iter().any(|f| f.to_lowercase().ends_with(".inc")) {
        languages.push(lang("pascal"));
    }
    languages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_ignored_dirs_match_as_literal_paths() {
        let scope = ScopeIgnore::new(Ignore::new(), Vec::new(), None, None, Vec::new())
            .with_git_ignored_dirs(["src/".to_string(), "[draft]/".to_string()]);
        assert!(scope.ignores("src/").unwrap());
        assert!(scope.ignores("src/Main.java").unwrap());
        // A pattern `src/` would also hide every nested `src/` (#2849).
        assert!(!scope.ignores("module/src/Main.java").unwrap());
        assert!(scope.ignores("[draft]/note.md").unwrap());
        assert!(!scope.ignores("d/note.md").unwrap());
    }

    #[test]
    fn include_ignored_pattern_is_anchored_and_escaped() {
        assert_eq!(include_ignored_pattern_for("child/"), "/child/");
        assert_eq!(include_ignored_pattern_for("a[b]/*x/"), r"/a\[b]/\*x/");
    }

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cg-scan-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn defaults_drop_dependency_and_build_dirs() {
        let ig = defaults_only_ignore();
        assert!(ig.ignores("node_modules/").unwrap());
        assert!(ig.ignores("a/node_modules/x/y.js").unwrap());
        assert!(!ig.ignores("src/lib/a.ts").unwrap());
        assert!(ig.ignores("app/src/main/res/layout-v21/main.xml").unwrap());
        assert!(!ig.ignores("app/src/main/res/raw/schema.sql").unwrap());
        assert!(!ig.ignores("src/main/java/com/build/Foo.java").unwrap());
        assert!(ig.ignores("src/build/legacy/page.html").unwrap());
        assert!(!ig.ignores("src/build/legacy/page.tsx").unwrap());
    }

    #[test]
    fn include_roots() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(include_static_roots(&s(&["Tools/"])), vec!["Tools/"]);
        assert_eq!(include_static_roots(&s(&["Tools/**"])), vec!["Tools/"]);
        assert_eq!(include_static_roots(&s(&["src/local/file.ts"])), vec!["src/local/"]);
        assert_eq!(include_static_roots(&s(&["**/gen/*.ts"])), vec![""]);
        assert_eq!(include_static_roots(&s(&["a/b/", "a/"])), vec!["a/"]);
    }

    #[test]
    fn worktree_pointers() {
        assert!(is_worktree_gitdir("/repo/.git/worktrees/feature"));
        assert!(is_worktree_gitdir("C:\\repo\\.git\\worktrees\\feature"));
        assert!(is_worktree_gitdir("/repo/.git/modules/sub/worktrees/x"));
        assert!(!is_worktree_gitdir("/repo/.git/modules/sub"));
        assert!(!is_worktree_gitdir("/repo/.github/worktrees/x"));
    }

    #[test]
    fn extname_matches_node() {
        assert_eq!(extname("a/b.TXT"), ".TXT");
        assert_eq!(extname("a/.gitignore"), "");
        assert_eq!(extname("a/b."), ".");
        assert_eq!(extname("a/b"), "");
        assert_eq!(extname("a/b.c.d"), ".d");
        assert_eq!(extname(".."), "");
    }

    #[test]
    fn walk_honours_nested_gitignore_and_defaults() {
        let root = temp_root("walk");
        write(&root, "src/a.ts", "export const a = 1;");
        write(&root, "src/gen/b.ts", "export const b = 1;");
        write(&root, "src/gen/.gitignore", "*.ts\n!keep.ts\n");
        write(&root, "src/gen/keep.ts", "export const k = 1;");
        write(&root, "node_modules/p/i.js", "x");
        write(&root, "README.md", "# hi");
        let mut stats = ScanSkipStats::default();
        let mut files = scan_directory_walk(&root, None, Some(&mut stats)).unwrap();
        files.sort();
        assert_eq!(files, vec!["src/a.ts", "src/gen/keep.ts"]);
        assert_eq!(stats.unsupported_by_extension.get(".md"), Some(&1));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn preload_adds_the_ambiguous_grammars() {
        let files = vec!["a.h".to_string(), "b.js".to_string(), "c.inc".to_string()];
        let names: Vec<&str> = preload_languages_for_files(&files, None, None).iter().map(|l| l.as_str()).collect::<Vec<_>>().into_iter().collect();
        for want in ["c", "javascript", "php", "tsx", "cpp", "objc", "pascal"] {
            assert!(names.contains(&want), "{want} missing from {names:?}");
        }
    }
}
