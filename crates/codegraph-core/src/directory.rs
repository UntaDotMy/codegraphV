//! Directory management — port of `src/directory.ts`.
//!
//! Manages the `.codegraph/` directory structure for CodeGraph data. (The prompt-hook helpers of
//! that module live in [`crate::prompt_hook`].)

use crate::utils::{real_path, sensitive_directory_refusal};
use crate::watch_policy::is_wsl_windows_drive;
use codegraph_types::nodepath;
use codegraph_types::paths::codegraph_dir_name;
pub use codegraph_types::paths::{is_codegraph_data_dir, DEFAULT_CODEGRAPH_DIR, WSL_CODEGRAPH_DIR};
use rusqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

/// `codeGraphDirName()`: the per-project data directory name, honoring `CODEGRAPH_DIR`.
pub fn code_graph_dir_name() -> String {
    codegraph_dir_name()
}

/// The data directory name for one project: [`code_graph_dir_name`], except for a project on a
/// Windows drive under WSL (`/mnt/c/...`) with no `CODEGRAPH_DIR` set. Windows-native CodeGraph
/// opens `.codegraph` in that same tree, and SQLite's locking doesn't hold across the 9p/DrvFs
/// bridge, so the two sharing one index fails with "disk I/O error" (issue #995). There:
///
///   1. `.codegraph-wsl/` exists → it. Once WSL has its own index it keeps it, even after Windows
///      builds a `.codegraph` beside it.
///   2. `.codegraph/codegraph.db` exists → `.codegraph`. An index built before this default is
///      kept rather than silently rebuilt somewhere else.
///   3. neither → `.codegraph-wsl`, so a fresh WSL index never shares.
///
/// Every other host keeps the plain name without a stat: the WSL check is cached per process.
pub fn code_graph_dir_name_for(project_root: &str) -> String {
    let overridden = std::env::var("CODEGRAPHV_DIR").map(|v| !codegraph_types::jscompat::js_trim(&v).is_empty()).unwrap_or(false);
    if overridden || !is_wsl_windows_drive(project_root) {
        return code_graph_dir_name();
    }
    if Path::new(project_root).join(WSL_CODEGRAPH_DIR).is_dir() {
        return WSL_CODEGRAPH_DIR.to_string();
    }
    if Path::new(project_root).join(DEFAULT_CODEGRAPH_DIR).join("codegraph.db").exists() {
        return DEFAULT_CODEGRAPH_DIR.to_string();
    }
    WSL_CODEGRAPH_DIR.to_string()
}

/// Get the `.codegraph` directory path for a project.
pub fn get_code_graph_dir(project_root: &str) -> String {
    nodepath::join(&[project_root, &code_graph_dir_name_for(project_root)])
}

/// The project's database, in its data directory (`getDatabasePath`).
pub fn database_path(project_root: &Path) -> PathBuf {
    Path::new(&get_code_graph_dir(&project_root.to_string_lossy())).join(codegraph_db::DATABASE_FILENAME)
}

/// What `codegraph.db` holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchemaProbe {
    Schema,
    NoSchema,
    NotSqlite,
    Unknown,
}

/// A SQLite file header is 100 bytes; anything shorter cannot hold a schema.
const SQLITE_HEADER_SIZE: u64 = 100;

#[derive(Clone, Copy)]
struct ProbeEntry {
    mtime_ms: u128,
    size: u64,
    ok: bool,
}

static SCHEMA_PROBE_CACHE: Mutex<Option<HashMap<String, ProbeEntry>>> = Mutex::new(None);

fn mtime_ms(meta: &std::fs::Metadata) -> u128 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0)
}

/// What `codegraph.db` holds, asked of SQLite itself through a read-only connection: the codegraph
/// schema, a database without it, not a database at all (SQLITE_NOTADB), or `Unknown` — locked,
/// busy, a WAL db in a directory we cannot create `-shm` in (read-only checkout, mount, another
/// user's tree), disk I/O. Callers treat `Unknown` as initialized: the pre-existing behaviour for
/// a database we cannot inspect.
///
/// The file is never read through a descriptor of our own, not even for its 16-byte header.
/// Closing ANY descriptor on a database file drops every POSIX lock this process holds on it,
/// including those of a connection it already has open (sqlite.org/howtocorrupt.html §2.2.1) —
/// and the MCP server resolves projects through `is_initialized` on every call while it holds the
/// index as its writer. SQLite's own connections share one lock table per file, so a second
/// connection opened and closed here leaves the first one's locks alone.
fn probe_schema(db_path: &Path) -> SchemaProbe {
    let classify = |e: &rusqlite::Error| match e {
        rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::NotADatabase => SchemaProbe::NotSqlite,
        _ => SchemaProbe::Unknown,
    };
    codegraph_db::configure_sqlite();
    let conn = match Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX) {
        Ok(c) => c,
        Err(e) => return classify(&e),
    };
    // Never hold the handle: Windows file locking would block the owner (dropped at return).
    let found = conn.query_row("SELECT 1 AS ok FROM sqlite_master WHERE type = 'table' AND name = 'nodes'", [], |r| r.get::<_, i64>(0));
    match found {
        Ok(_) => SchemaProbe::Schema,
        Err(rusqlite::Error::QueryReturnedNoRows) => SchemaProbe::NoSchema,
        Err(e) => classify(&e),
    }
}

fn has_code_graph_schema(db_path: &Path, meta: &std::fs::Metadata) -> bool {
    if !meta.is_file() || meta.len() < SQLITE_HEADER_SIZE {
        return false;
    }
    let key = db_path.to_string_lossy().into_owned();
    let (mtime, size) = (mtime_ms(meta), meta.len());
    if let Some(cached) = SCHEMA_PROBE_CACHE.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&key)) {
        if cached.mtime_ms == mtime && cached.size == size {
            return cached.ok;
        }
    }
    let probe = probe_schema(db_path);
    let ok = matches!(probe, SchemaProbe::Schema | SchemaProbe::Unknown);
    SCHEMA_PROBE_CACHE.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(key, ProbeEntry { mtime_ms: mtime, size, ok });
    ok
}

fn db_path_of(project_root: &str) -> PathBuf {
    Path::new(&get_code_graph_dir(project_root)).join("codegraph.db")
}

/// Check if a project has been initialized with CodeGraph.
///
/// Requires `.codegraph/codegraph.db` to exist AND to carry the codegraph schema. A file that
/// merely exists — empty, or a SQLite database with no tables, as an interrupted `init` or a stray
/// `touch` leaves behind — used to count as initialized, so one such file in an ANCESTOR directory
/// (worst case: `$HOME`) captured the upward resolution of every project beneath it and made their
/// real indexes unreachable (#1895).
///
/// The probe is cheap and gated so hot callers (the prompt hook, MCP root resolution on every
/// call) pay one `stat`: file size, then a read-only SQLite open with a single `sqlite_master`
/// lookup — memoized per path + mtime + size so an unchanged db is never reopened.
pub fn is_initialized(project_root: &str) -> bool {
    let codegraph_dir = get_code_graph_dir(project_root);
    if !Path::new(&codegraph_dir).is_dir() {
        return false;
    }
    // Must have codegraph.db, not just .codegraph folder.
    let db_path = Path::new(&codegraph_dir).join("codegraph.db");
    match std::fs::metadata(&db_path) {
        Ok(meta) => has_code_graph_schema(&db_path, &meta),
        Err(_) => false,
    }
}

/// `codegraph.db` exists at `project_root` but does not carry the schema, and `init` can add it in
/// place: an empty file, or a SQLite database without the codegraph tables (#1895). A file that is
/// not SQLite at all is NOT this case — see [`has_foreign_db_file`].
pub fn has_schemaless_db(project_root: &str) -> bool {
    let db_path = db_path_of(project_root);
    let Ok(meta) = std::fs::metadata(&db_path) else { return false };
    if !meta.is_file() || is_initialized(project_root) {
        return false;
    }
    meta.len() == 0 || probe_schema(&db_path) == SchemaProbe::NoSchema
}

/// `codegraph.db` exists at `project_root` and is not a SQLite database (no header magic): SQLite
/// refuses to open it, so `init` cannot rebuild it in place. The caller must say so rather than
/// promise a repair; nothing here deletes the file.
pub fn has_foreign_db_file(project_root: &str) -> bool {
    let db_path = db_path_of(project_root);
    let Ok(meta) = std::fs::metadata(&db_path) else { return false };
    meta.is_file() && meta.len() > 0 && probe_schema(&db_path) == SchemaProbe::NotSqlite
}

/// Reason a directory is unsafe to use as an index ROOT, or `None` when it's fine.
///
/// Indexing your home directory or a filesystem root drags in caches, `Library`, every other
/// project, etc. — a multi-GB index, constant file-watcher churn, and (pre-1.0 on macOS) a
/// file-descriptor blowup that exhausted `kern.maxfiles` and took unrelated apps / the whole
/// machine down (#845). The classic trigger: running the installer or `codegraph init` from
/// `$HOME`, which auto-indexes the current directory. These are never intended project roots, so
/// the installer and `init`/`index` refuse them (overridable with `--force`).
///
/// The returned string is a human phrase that slots into "… looks like {reason}".
pub fn unsafe_index_root_reason(project_root: &str) -> Option<&'static str> {
    let resolve = |p: &str| -> String {
        let resolved = nodepath::resolve(&[p]);
        real_path(Path::new(&resolved)).map(|r| r.to_string_lossy().into_owned()).unwrap_or(resolved)
    };
    let resolved = resolve(project_root);

    // Filesystem root: `/` on POSIX, a drive root like `C:\` on Windows.
    if path_root(&resolved) == resolved {
        return Some("the filesystem root");
    }

    let home = codegraph_types::paths::home_dir().map(|h| resolve(&h.to_string_lossy())).unwrap_or_default();
    // Case-insensitive on macOS/Windows (case-preserving but case-insensitive FS).
    let norm = |p: &str| -> String {
        if cfg!(any(target_os = "macos", windows)) {
            p.to_lowercase()
        } else {
            p.to_string()
        }
    };
    let r = norm(&resolved);
    let h = norm(&home);

    if r == h {
        return Some("your home directory");
    }
    // An ancestor of home (e.g. `/Users`, `/home`) — even broader than home.
    if h.starts_with(&format!("{r}{}", nodepath::SEP)) {
        return Some("a parent of your home directory");
    }
    None
}

fn lower_js(s: &str) -> String {
    s.to_lowercase()
}

/// `path.parse(p).root`: `/` on POSIX, `C:\` on Windows (and `\\server\share\` for UNC).
fn path_root(p: &str) -> String {
    let path = Path::new(p);
    let mut root = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => root.push(comp.as_os_str()),
            _ => break,
        }
    }
    root.to_string_lossy().into_owned()
}

/// `dev:ino` for a path, or `None` if it can't be stat'd or the platform doesn't report a usable
/// inode. Windows st_ino is unreliable across handle reopens, so we deliberately return `None`
/// there — the deleted-but-open-inode hazard this guards (#925) is a POSIX file-semantics issue
/// that doesn't arise on Windows (an open file can't be unlinked).
pub fn stat_inode(p: &str) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(p).ok()?;
        Some(format!("{}:{}", m.dev(), m.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = p;
        None
    }
}

/// Canonicalize a project root for IDENTITY: hashing it into a rendezvous key (the daemon's named
/// pipe / tmpdir socket, a registry record). Not for display — the caller-visible root keeps its
/// own spelling.
///
/// The contract is that two spellings of ONE directory yield ONE string, because a client that
/// derives a different key silently fails to meet the daemon that is already running. NTFS is
/// case-insensitive, so `d:\work\codegraph` and `D:\work\codegraph` name one directory but two
/// strings; the native realpath asks the filesystem for the on-disk name, and the Windows
/// lowercase is belt-and-braces on top of it.
pub fn canonical_project_root(project_root: &str) -> String {
    let resolved = nodepath::resolve(&[project_root]);
    let canonical = real_path(Path::new(&resolved)).map(|r| r.to_string_lossy().into_owned()).unwrap_or(resolved);
    if cfg!(windows) {
        canonical.to_lowercase()
    } else {
        canonical
    }
}

/// Whether two resolved index roots are one index spelled two ways — a symlinked checkout, or a
/// case-variant on a case-insensitive mount (macOS, NTFS, WSL DrvFs `/mnt/c`), where `realpath`
/// keeps the caller's casing (#1057). Compares the identity of both data directories as they are
/// NOW, so an inode reused after a delete can't match: the deleted root no longer stats. Windows
/// has no usable inode, so it compares the on-disk-cased native realpaths.
pub fn is_same_index_root(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    if cfg!(windows) {
        return match (real_path(Path::new(&get_code_graph_dir(a))), real_path(Path::new(&get_code_graph_dir(b)))) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        };
    }
    let id = stat_inode(&get_code_graph_dir(a));
    id.is_some() && id == stat_inode(&get_code_graph_dir(b))
}

/// Find the nearest parent directory containing `.codegraph/`.
///
/// Walks up from the given path to find a CodeGraph-initialized project, similar to how git finds
/// `.git/` directories. Returns the project root containing `.codegraph/`, or `None` if not found.
pub fn find_nearest_code_graph_root(start_path: &str) -> Option<String> {
    let mut current = nodepath::resolve(&[start_path]);
    let root = path_root(&current);

    while current != root {
        if is_initialized(&current) {
            return Some(current);
        }
        let parent = nodepath::dirname(&current);
        if parent == current {
            break; // Reached filesystem root
        }
        current = parent;
    }

    // Check root as well.
    if is_initialized(&current) {
        return Some(current);
    }
    None
}

/// Heavy/irrelevant directory names the sub-project scan never descends into.
const SUBPROJECT_SCAN_SKIP: &[&str] = &[
    "node_modules", ".git", ".svn", ".hg", "dist", "build", "out", "target", "vendor", "bin", "obj", ".next", ".nuxt", ".svelte-kit", ".cache",
    "coverage", ".venv", "venv", "__pycache__", ".turbo", ".idea", ".vscode", "tmp", "temp",
];

/// Manifests that mark a directory as a project/workspace root. The down-scan is gated on one of
/// these so a non-project cwd (e.g. `$HOME`) is a cheap no-op instead of a deep filesystem crawl.
const WORKSPACE_ROOT_MANIFESTS: &[&str] = &[
    "package.json", "pnpm-workspace.yaml", "lerna.json", "nx.json", "turbo.json", "go.work", "go.mod", "Cargo.toml", "pom.xml", "build.gradle",
    "build.gradle.kts", "settings.gradle", "pyproject.toml", "composer.json", "Gemfile", "rush.json", "WORKSPACE", "WORKSPACE.bazel",
];

pub fn looks_like_project_root(dir: &str) -> bool {
    WORKSPACE_ROOT_MANIFESTS.iter().any(|m| Path::new(dir).join(m).exists())
}

/// Options of [`find_indexed_subproject_roots`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SubprojectScanOptions {
    pub max_depth: Option<usize>,
    pub max: Option<usize>,
}

/// Indexed sub-project roots beneath `root` (bounded breadth-first scan). For the monorepo case
/// behind #964: the index lives in a CHILD (`packages/x/.codegraph/`), not at the workspace root
/// the agent's cwd points at. Descent stops at the first indexed directory on a branch (a
/// project's own sub-dirs aren't separate projects) and is bounded by depth + count so it never
/// turns into a full-tree crawl on a large repo.
pub fn find_indexed_subproject_roots(root: &str, opts: SubprojectScanOptions) -> Vec<String> {
    // A stray workspace manifest must not enable scanning home or broader roots (#1454).
    if unsafe_index_root_reason(root).is_some() {
        return Vec::new();
    }
    let max_depth = opts.max_depth.unwrap_or(4);
    let max = opts.max.unwrap_or(64);
    let mut out: Vec<String> = Vec::new();
    fn walk(dir: &str, depth: usize, max_depth: usize, max: usize, out: &mut Vec<String>) {
        if out.len() >= max || depth > max_depth {
            return;
        }
        let Ok(read) = std::fs::read_dir(dir) else { return };
        // Directory order as Node reports it: sorted by name.
        let mut entries: Vec<(String, bool)> =
            read.flatten().map(|e| (e.file_name().to_string_lossy().into_owned(), e.file_type().map(|t| t.is_dir()).unwrap_or(false))).collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, is_dir) in entries {
            if out.len() >= max {
                return;
            }
            if !is_dir {
                continue;
            }
            if name.starts_with('.') || SUBPROJECT_SCAN_SKIP.contains(&name.as_str()) {
                continue;
            }
            let child = nodepath::join(&[dir, &name]);
            if is_initialized(&child) {
                out.push(child);
                continue; // don't descend into an indexed project
            }
            walk(&child, depth + 1, max_depth, max, out);
        }
    }
    walk(root, 1, max_depth, max, &mut out);
    out
}

/// Result of [`resolve_server_root`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerRootResolution {
    /// The project root to serve as the default, or `None` when none resolved.
    pub root: Option<String>,
    /// True when `root` was adopted from the down-scan rather than the up-walk.
    pub via_sub_scan: bool,
    /// Indexed sub-projects the down-scan saw when it ran but could NOT adopt (zero or several
    /// candidates). Empty when the up-walk resolved or the scan was skipped. Callers surface
    /// these so "no default project" errors can say what IS reachable (#1607).
    pub candidates: Vec<String>,
    /// Why the root that was found is not served: it, or the path the search started from, is a
    /// sensitive folder (`~/.ssh`, `~/.config`, `/etc`, …), refused as a tool call's `projectPath`
    /// is. `root` is then `None`.
    pub refusal: Option<String>,
}

/// Whether `base` is a plausible workspace root for the sub-project down-scan. Mirrors
/// `plan_frontload`'s manifest gate, widened to accept a bare `.git` entry — the #1606 shape is a
/// workspace container holding only agent config and a `.git`, with every build manifest living
/// in the indexed children. The user's home directory and the filesystem root are never eligible:
/// a stray manifest there must not turn server startup into a scan that could adopt an unrelated
/// project (#1454 documents that failure mode for the prompt-hook).
fn eligible_for_subproject_scan(base: &str) -> bool {
    if base == path_root(base) {
        return false;
    }
    if let Some(home) = codegraph_types::paths::home_dir() {
        let home = home.to_string_lossy().into_owned();
        if base == home || base == nodepath::resolve(&[&home]) {
            return false;
        }
    }
    if looks_like_project_root(base) {
        return true;
    }
    Path::new(base).join(".git").exists()
}

/// Resolve the project root an MCP server should serve as its DEFAULT project (#1606). Up-walk
/// first ([`find_nearest_code_graph_root`] — the common case, and cheap). When nothing is indexed
/// at or above `search_from`, run the bounded sub-project down-scan `plan_frontload` already
/// uses, behind the workspace gate above: EXACTLY ONE indexed sub-project is unambiguous and is
/// adopted as the root; zero or several yield no root, with the candidates carried so the caller
/// can name them instead of failing silently (#1607).
///
/// `subproject_scan: false` skips the down-scan entirely (the per-tool-call retry path throttles
/// it; the up-walk always runs).
///
/// The default project gets the sensitive-folder refusal a tool call's `projectPath` gets: on the
/// path searched from when it exists, and on the root the search lands on. A refused root resolves
/// to `None`, with the reason in `refusal`: the server has no default project, so nothing there
/// is opened, watched or synced. Sub-projects below a refused path aren't listed either; a tool
/// call would refuse them.
pub fn resolve_server_root(search_from: &str, subproject_scan: bool) -> ServerRootResolution {
    let found = find_server_root(search_from, subproject_scan);
    if found.root.is_none() && found.candidates.is_empty() {
        return found;
    }
    let start_refusal = Path::new(search_from).exists().then(|| sensitive_directory_refusal(search_from)).flatten();
    let Some(root) = found.root.as_deref() else {
        return if start_refusal.is_some() { ServerRootResolution { candidates: Vec::new(), ..found } } else { found };
    };
    match start_refusal.or_else(|| sensitive_directory_refusal(root)) {
        Some(refusal) => ServerRootResolution { refusal: Some(refusal), ..Default::default() },
        None => found,
    }
}

/// [`resolve_server_root`] before the sensitive-folder refusal.
fn find_server_root(search_from: &str, subproject_scan: bool) -> ServerRootResolution {
    if let Some(up) = find_nearest_code_graph_root(search_from) {
        return ServerRootResolution { root: Some(up), ..Default::default() };
    }
    if !subproject_scan {
        return ServerRootResolution::default();
    }
    let base = nodepath::resolve(&[search_from]);
    if !eligible_for_subproject_scan(&base) {
        return ServerRootResolution::default();
    }
    let subs = find_indexed_subproject_roots(&base, SubprojectScanOptions::default());
    if subs.len() == 1 {
        return ServerRootResolution { root: Some(subs[0].clone()), via_sub_scan: true, candidates: subs, refusal: None };
    }
    ServerRootResolution { candidates: subs, ..Default::default() }
}

/// What the front-load hook should do for a prompt issued from a directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrontloadPlan {
    /// Open + explore this project and inject its source as context. `None` when there's no
    /// single project to front-load (none indexed, or several indexed sub-projects with no clear
    /// match — see `nudge_projects`).
    pub explore_root: Option<String>,
    /// Indexed sub-projects to surface in a "pass `projectPath`" nudge: the rest of a monorepo's
    /// indexed projects alongside `explore_root`, or — when no one project clearly matches — the
    /// full list (with `explore_root` `None`).
    pub nudge_projects: Vec<String>,
    /// True when the plan came from scanning DOWN into sub-projects (cwd itself is not under any
    /// index) — the monorepo case, where a follow-up `codegraph_explore` needs an explicit
    /// `projectPath`.
    pub via_sub_scan: bool,
}

/// Decide what the front-load hook injects for a `prompt` issued from `cwd`, shaped by where the
/// `.codegraph/` index(es) actually are:
///   1. **cwd (or an ancestor) is indexed** → front-load that project. The normal single-project /
///      nested-file case.
///   2. **cwd isn't indexed but looks like a workspace root** → the indexes live in sub-projects
///      (the monorepo case behind #964). One indexed sub-project → front-load it; several →
///      front-load the one the prompt names (by relative path like `packages/api`, or package
///      directory name) and nudge about the rest; several with no match → nudge the full list so
///      the agent passes `projectPath`, rather than guessing wrong.
///   3. **nothing indexed reachable** → do nothing (the agent's own tools apply).
pub fn plan_frontload(cwd: &str, prompt: &str) -> FrontloadPlan {
    // 1. up-walk — nearest indexed ancestor (incl. cwd). Cheap; covers the common single-project
    //    case without a down-scan.
    let mut dir = nodepath::resolve(&[cwd]);
    for _ in 0..6 {
        if is_initialized(&dir) {
            return FrontloadPlan { explore_root: Some(dir), nudge_projects: Vec::new(), via_sub_scan: false };
        }
        let parent = nodepath::dirname(&dir);
        if parent == dir {
            break;
        }
        dir = parent;
    }

    // 2. down-scan — only from something that looks like a workspace root, so a non-project cwd
    //    (e.g. $HOME) is a cheap no-op, not a deep crawl.
    let base = nodepath::resolve(&[cwd]);
    if !looks_like_project_root(&base) {
        return FrontloadPlan::default();
    }
    let subs = find_indexed_subproject_roots(&base, SubprojectScanOptions::default());
    if subs.is_empty() {
        return FrontloadPlan::default();
    }
    if subs.len() == 1 {
        return FrontloadPlan { explore_root: Some(subs[0].clone()), nudge_projects: Vec::new(), via_sub_scan: true };
    }

    // Several indexed sub-projects — pick the one the prompt points at, if any.
    let p = lower_js(prompt);
    let mut best: Option<(String, u32, usize)> = None;
    for s in &subs {
        let rel = nodepath::relative(&base, s);
        let rel_lc = lower_js(&rel.replace(nodepath::SEP, "/"));
        let name = lower_js(&nodepath::basename(s));
        let mut score = 0;
        if !rel_lc.is_empty() && p.contains(&rel_lc) {
            score = 10; // "packages/api"
        } else if codegraph_types::jscompat::utf16_len(&name) >= 3
            && codegraph_jsre::Re::new(&format!(r"\b{}\b", codegraph_jsre::escape_regex(&name)), "").is_match(&p)
        {
            score = 5; // "api"
        }
        let rel_len = codegraph_types::jscompat::utf16_len(&rel);
        let better = match &best {
            None => true,
            Some((_, best_score, best_len)) => score > *best_score || (score == *best_score && rel_len < *best_len),
        };
        if score > 0 && better {
            best = Some((s.clone(), score, rel_len));
        }
    }
    if let Some((root, _, _)) = best {
        let nudge = subs.iter().filter(|s| **s != root).cloned().collect();
        return FrontloadPlan { explore_root: Some(root), nudge_projects: nudge, via_sub_scan: true };
    }
    // No clear match — nudge the full list rather than front-load a guess.
    FrontloadPlan { explore_root: None, nudge_projects: subs, via_sub_scan: true }
}

/// Contents of `.codegraph/.gitignore`. A single wildcard ignore keeps every transient file in the
/// index dir — the database, `daemon.pid`, the socket, logs, cache, and anything future versions
/// add — out of git, without having to enumerate each name (issues #788, #492, #484). Older
/// versions wrote an explicit allowlist that never listed `daemon.pid` or the socket, so those
/// runtime files were silently committed.
const GITIGNORE_CONTENT: &str = "# CodeGraphV data files — local to each machine, not for committing.\n# Ignore everything in .codegraphV/ except this file itself, so transient\n# files (the database, daemon.pid, sockets, logs) never show up in git.\n*\n!.gitignore\n";

/// Header line that prefixes every .gitignore CodeGraph has auto-generated.
const GITIGNORE_MARKER: &str = "# CodeGraphV data files";

/// Is `content` a stale CodeGraph-generated `.gitignore` that should be regenerated in place?
/// True when it carries our header but predates the wildcard ignore (it has no bare `*` line) —
/// i.e. one of the old explicit allowlists (`*.db`, `cache/`, `.dirty`, …) that never ignored
/// `daemon.pid` or the socket (issue #788). A file WITHOUT our header is user-authored and is left
/// untouched; one that already has the wildcard is current.
fn is_stale_default_gitignore(content: &str) -> bool {
    if !codegraph_types::jscompat::js_trim_start(content).starts_with(GITIGNORE_MARKER) {
        return false;
    }
    !content.split('\n').any(|line| codegraph_types::jscompat::js_trim(line) == "*")
}

/// Write `.codegraph/.gitignore` if it's absent, or upgrade a stale CodeGraph-generated default
/// in place; a user-customized file is left alone. Best-effort — returns `false` only if a needed
/// write failed.
fn ensure_gitignore(gitignore_path: &Path) -> bool {
    let existing = std::fs::read_to_string(gitignore_path).ok(); // absent or unreadable — (re)create below
    // Current default or a user-authored file: nothing to do.
    if let Some(existing) = &existing {
        if !is_stale_default_gitignore(existing) {
            return true;
        }
    }
    std::fs::write(gitignore_path, GITIGNORE_CONTENT).is_ok()
}

/// Create the `.codegraph` directory structure. Errs only if `codegraph.db` already carries the
/// schema, not just because `.codegraph/` exists.
pub fn create_directory(project_root: &str) -> Result<(), String> {
    let codegraph_dir = get_code_graph_dir(project_root);

    // Only fail if CodeGraph is actually initialized (db with a schema). `.codegraph/` alone — or
    // a schema-less codegraph.db left by an interrupted init (#1895) — is fine: initialize() adds
    // the schema to it.
    if is_initialized(project_root) {
        return Err(format!("CodeGraphV already initialized in {project_root}"));
    }

    // Create main directory (if it doesn't exist).
    std::fs::create_dir_all(&codegraph_dir).map_err(|e| e.to_string())?;

    // Write .gitignore inside .codegraph (create if absent, upgrade a stale pre-wildcard default
    // left by an older version — issue #788).
    ensure_gitignore(&Path::new(&codegraph_dir).join(".gitignore"));
    Ok(())
}

/// Remove the `.codegraph` directory.
pub fn remove_directory(project_root: &str) -> std::io::Result<()> {
    let codegraph_dir = get_code_graph_dir(project_root);
    let path = Path::new(&codegraph_dir);
    let Ok(meta) = std::fs::symlink_metadata(path) else { return Ok(()) };

    // Verify .codegraph is a real directory, not a symlink pointing elsewhere.
    if meta.file_type().is_symlink() {
        // Only remove the symlink itself, never follow it for recursive delete.
        #[cfg(windows)]
        {
            if std::fs::remove_dir(path).is_err() {
                std::fs::remove_file(path)?;
            }
        }
        #[cfg(not(windows))]
        std::fs::remove_file(path)?;
        return Ok(());
    }
    if !meta.is_dir() {
        // Not a directory - remove the single file.
        return std::fs::remove_file(path);
    }
    // Recursively remove directory.
    std::fs::remove_dir_all(path)
}

/// Get all files in the `.codegraph` directory (relative, `/`-separated).
pub fn list_directory_contents(project_root: &str) -> Vec<String> {
    let codegraph_dir = get_code_graph_dir(project_root);
    let root = Path::new(&codegraph_dir);
    if !root.exists() {
        return Vec::new();
    }
    let mut files = Vec::new();
    fn walk(dir: &Path, prefix: &str, files: &mut Vec<String>) {
        let Ok(read) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = read.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let relative = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            let Ok(ft) = entry.file_type() else { continue };
            // Skip symlinks to prevent following links outside .codegraph.
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                walk(&entry.path(), &relative, files);
            } else {
                files.push(relative);
            }
        }
    }
    walk(root, "", &mut files);
    files
}

/// Get the total size of the `.codegraph` directory in bytes.
pub fn get_directory_size(project_root: &str) -> u64 {
    let codegraph_dir = get_code_graph_dir(project_root);
    let root = Path::new(&codegraph_dir);
    if !root.exists() {
        return 0;
    }
    fn walk(dir: &Path) -> u64 {
        let Ok(read) = std::fs::read_dir(dir) else { return 0 };
        let mut total = 0;
        for entry in read.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            // Skip symlinks to prevent following links outside .codegraph.
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                total += walk(&entry.path());
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
        total
    }
    walk(root)
}

/// Ensure a subdirectory exists within `.codegraph`.
pub fn ensure_subdirectory(project_root: &str, subdir_name: &str) -> Result<String, String> {
    if subdir_name.contains("..") || subdir_name.contains(nodepath::SEP) || subdir_name.contains('/') {
        return Err(format!("Invalid subdirectory name: {subdir_name}"));
    }
    let subdir_path = nodepath::join(&[&get_code_graph_dir(project_root), subdir_name]);
    if !Path::new(&subdir_path).exists() {
        std::fs::create_dir_all(&subdir_path).map_err(|e| e.to_string())?;
    }
    Ok(subdir_path)
}

/// Result of [`validate_directory`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryValidation {
    pub valid: bool,
    pub errors: Vec<String>,
}

/// Check if the `.codegraph` directory has valid structure.
pub fn validate_directory(project_root: &str) -> DirectoryValidation {
    let mut errors = Vec::new();
    let codegraph_dir = get_code_graph_dir(project_root);
    let dir = Path::new(&codegraph_dir);

    if !dir.exists() {
        errors.push("CodeGraphV directory does not exist".to_string());
        return DirectoryValidation { valid: false, errors };
    }
    if !dir.is_dir() {
        errors.push(format!("{} exists but is not a directory", code_graph_dir_name_for(project_root)));
        return DirectoryValidation { valid: false, errors };
    }

    // Auto-repair / upgrade .gitignore (non-critical file). A missing one is recreated; a stale
    // pre-wildcard default that never ignored daemon.pid is regenerated in place (issue #788); a
    // user-authored file is left alone.
    let gitignore_path = dir.join(".gitignore");
    let existed_before = gitignore_path.exists();
    if !ensure_gitignore(&gitignore_path) && !existed_before {
        // Only a missing-and-uncreatable file is surfaced; a failed in-place upgrade of an
        // existing file is non-fatal — the index still works.
        errors.push(".gitignore missing in .codegraphV directory and could not be created".to_string());
    }

    DirectoryValidation { valid: errors.is_empty(), errors }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(dir: &tempfile::TempDir) -> String {
        dir.path().to_string_lossy().into_owned()
    }

    #[test]
    fn create_and_validate() {
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        create_directory(&r).unwrap();
        let gi = std::fs::read_to_string(Path::new(&get_code_graph_dir(&r)).join(".gitignore")).unwrap();
        assert!(gi.contains("\n*\n"));
        assert!(validate_directory(&r).valid);
        assert!(!is_initialized(&r));
        remove_directory(&r).unwrap();
        assert!(!Path::new(&get_code_graph_dir(&r)).exists());
    }

    #[test]
    fn stale_gitignore_is_upgraded_but_user_file_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".gitignore");
        std::fs::write(&p, "# CodeGraphV data files\n*.db\ncache/\n").unwrap();
        assert!(ensure_gitignore(&p));
        assert!(std::fs::read_to_string(&p).unwrap().contains("\n*\n"));
        std::fs::write(&p, "my own rules\n").unwrap();
        assert!(ensure_gitignore(&p));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "my own rules\n");
    }

    #[test]
    fn schemaless_db_is_not_initialized() {
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        create_directory(&r).unwrap();
        let db = Path::new(&get_code_graph_dir(&r)).join("codegraph.db");
        std::fs::write(&db, b"").unwrap();
        assert!(!is_initialized(&r));
        assert!(has_schemaless_db(&r));
        std::fs::write(&db, vec![b'x'; 4096]).unwrap();
        assert!(has_foreign_db_file(&r));
    }

    #[test]
    fn subdir_names_are_validated() {
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        assert!(ensure_subdirectory(&r, "../x").is_err());
        assert!(ensure_subdirectory(&r, "cache").is_ok());
    }
}
