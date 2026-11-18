//! `codegraphV guard-hook` (hidden): Claude Code's `PreToolUse` hook that keeps an agent on
//! codegraphV in an indexed project. A content search whose pattern names an indexed symbol, or
//! a file listing over indexed files, is denied until the agent asked codegraphV this session —
//! the deny reason names the exact `codegraphV_explore` call to make. Plain-text searches, pipe
//! filters, reads and edits always pass.
//!
//! It also guards itself: an edit or shell command that would remove the guard from Claude's
//! settings, or that turns hooks off, is denied. That check needs no index — it runs anywhere.
//!
//! LOAD-BEARING: it must never stop work by malfunctioning. Every failure path — switch off,
//! bad payload, no index, engine error, timeout — prints nothing and exits 0 (allow).
//! `CODEGRAPHV_GUARD=0` in the agent's environment turns it off.
use codegraph_core::directory::{database_path, find_nearest_code_graph_root};
use codegraph_db::{OpenOptions as DbOpenOptions, Store};
use regex::Regex;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    io::{IsTerminal, Read, Write},
    path::{Path, PathBuf},
    sync::LazyLock,
};

/// Content searchers: their first operand is a pattern, the rest are paths.
const SEARCHERS: &[&str] = &["grep", "egrep", "fgrep", "rg", "ag", "ack", "ack-grep", "ugrep", "pt", "findstr", "select-string", "sls"];
/// Directory listers, which find files only when recursive.
const LISTERS: &[&str] = &["get-childitem", "gci", "ls", "dir"];
const FINDERS: &[&str] = &["find", "fd", "fdfind", "locate"];
const INTERPRETERS: &[&str] = &["python", "python3", "py", "node", "perl", "ruby", "deno", "bun"];
const SHELLS: &[&str] = &["bash", "sh", "zsh", "pwsh", "powershell", "cmd"];
/// Words that run the command after them: wrappers and shell keywords (`if grep …; then`).
const WRAPPERS: &[&str] = &[
    "sudo", "command", "exec", "time", "nice", "nohup", "env", "&", ".", "xargs", "!", "if", "then", "else", "elif", "while", "until", "do",
];
const CD: &[&str] = &["cd", "pushd", "set-location", "sl", "chdir"];
/// The codegraphV subcommands that answer a lookup.
const LOOKUPS: &[&str] = &["explore", "node", "query", "callers", "callees", "impact", "context", "files"];
/// Search options that take the next argument as their value.
const VALUE_OPTS: &[&str] = &[
    "-e", "-f", "-g", "--glob", "--iglob", "-t", "--type", "-T", "--type-not", "-m", "--max-count", "-A", "-B", "-C", "--context",
    "--after-context", "--before-context", "-j", "--threads", "-M", "--max-columns", "--type-add", "--encoding", "--sort", "--sortr",
    "--path-separator", "--include", "--exclude", "--exclude-dir", "-d", "-D", "--regexp", "--file", "--max-depth", "--maxdepth",
    "--ignore-file", "--pre", "-pattern", "-path", "-literalpath", "-include", "-exclude", "-encoding", "-context",
];

/// Inline code that walks or greps files.
static WALK_CODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)os\.walk|rglob|\bglob\(|glob\.glob|readdirsync|readdir\(|get-childitem|select-string|re\.(search|findall|finditer|match)|\bgrep\b|\brg\b|scandir").unwrap()
});
static IDENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z_][A-Za-z0-9_]{2,}").unwrap());
static ANY_IDENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").unwrap());
static CODE_SHAPED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[a-z][A-Z]|[A-Z]{2}[a-z]|\d").unwrap());

fn rg_type_exts(t: &str) -> Vec<String> {
    let exts: &[&str] = match t {
        "rust" => &["rs"],
        "py" | "python" => &["py", "pyi"],
        "js" => &["js", "jsx", "mjs", "cjs"],
        "ts" | "typescript" => &["ts", "tsx", "mts", "cts"],
        "c" => &["c", "h"],
        "cpp" => &["cpp", "cc", "cxx", "hpp", "hh", "h"],
        "cs" | "csharp" => &["cs"],
        "rb" | "ruby" => &["rb"],
        "kotlin" => &["kt", "kts"],
        "md" | "markdown" => &["md", "mdx"],
        "yaml" => &["yaml", "yml"],
        "html" => &["html", "htm"],
        "sh" => &["sh", "bash"],
        "gd" | "gdscript" => &["gd"],
        other => return vec![other.to_owned()],
    };
    exts.iter().map(|s| (*s).to_owned()).collect()
}

// ---------------------------------------------------------------- session state

/// What this session asked codegraphV: one line per lookup, `root \t token token …`, appended so
/// parallel tool calls never lose each other's writes.
struct Session {
    file: Option<PathBuf>,
    roots: HashSet<String>,
    tokens: HashSet<String>,
}

fn state_dir() -> PathBuf {
    std::env::temp_dir().join("codegraphV-guard")
}

impl Session {
    fn load(id: &str) -> Self {
        let safe: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
        let file = (!safe.is_empty()).then(|| state_dir().join(format!("{safe}.log")));
        let mut session = Session { file, roots: HashSet::new(), tokens: HashSet::new() };
        if let Some(text) = session.file.as_ref().and_then(|f| std::fs::read_to_string(f).ok()) {
            for line in text.lines() {
                let (root, toks) = line.split_once('\t').unwrap_or((line, ""));
                if !root.is_empty() {
                    session.roots.insert(root.to_owned());
                }
                session.tokens.extend(toks.split(' ').filter(|t| !t.is_empty()).map(str::to_owned));
            }
        }
        session
    }

    fn record(&mut self, root: Option<&str>, texts: &[String]) {
        let tokens: Vec<String> =
            texts.iter().flat_map(|t| ANY_IDENT.find_iter(t).map(|m| m.as_str().to_lowercase()).collect::<Vec<_>>()).collect();
        let root = root.unwrap_or_default();
        if !root.is_empty() {
            self.roots.insert(root.to_owned());
        }
        self.tokens.extend(tokens.iter().cloned());
        let Some(file) = &self.file else { return };
        let _ = std::fs::create_dir_all(state_dir());
        prune_old_sessions();
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(file) {
            let _ = writeln!(f, "{root}\t{}", tokens.join(" "));
        }
    }
}

/// Session logs untouched for a week are dropped.
fn prune_old_sessions() {
    let Ok(entries) = std::fs::read_dir(state_dir()) else { return };
    let week = std::time::Duration::from_secs(7 * 86_400);
    for entry in entries.flatten() {
        if entry.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|age| age > week)) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

// ---------------------------------------------------------------- the index

struct Guard {
    session: Session,
    stores: HashMap<String, Option<(Store, Vec<String>)>>,
}

/// Why a call is denied.
type Deny = String;

fn norm_path(p: &str, cwd: &Path) -> PathBuf {
    let mut p = p.trim().trim_matches(|c| c == '\'' || c == '"').to_owned();
    if let Some(home) = codegraph_types::paths::home_dir() {
        let home = home.to_string_lossy().into_owned();
        if p == "~" || p.starts_with("~/") || p.starts_with("~\\") {
            p = format!("{home}{}", &p[1..]);
        }
        p = p.replace("${HOME}", &home).replace("$HOME", &home);
    }
    // Git Bash spells C:\Users as /c/Users.
    if cfg!(windows) {
        let b = p.as_bytes();
        if b.len() >= 2 && b[0] == b'/' && b[1].is_ascii_alphabetic() && (b.len() == 2 || b[2] == b'/') {
            p = format!("{}:/{}", (b[1] as char).to_ascii_uppercase(), p.get(3..).unwrap_or(""));
        }
    }
    let path = Path::new(&p);
    let joined = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) };
    lexical_normalize(&joined)
}

/// `a/./b/../c` → `a/c` without touching the filesystem.
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in p.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn find_root(path: &Path) -> Option<String> {
    // A path with a wildcard (`src/*.ts`) starts its walk at the folder that holds it.
    let mut start = path.to_path_buf();
    while start.file_name().is_some_and(|n| n.to_string_lossy().contains(['*', '?', '['])) {
        start.pop();
    }
    if !start.is_dir() {
        start.pop();
    }
    find_nearest_code_graph_root(&start.to_string_lossy())
}

fn glob_matcher(globs: &[String]) -> Option<globset::GlobSet> {
    let mut builder = globset::GlobSetBuilder::new();
    let mut any = false;
    for g in globs.iter().filter(|g| !g.is_empty() && !g.starts_with('!')) {
        let g = g.replace('\\', "/");
        // A bare name pattern (`*.ts`) matches at any depth, as the searchers apply it.
        for pattern in [g.clone(), format!("**/{}", g.trim_start_matches("**/"))] {
            if let Ok(glob) = globset::GlobBuilder::new(&pattern).literal_separator(true).build() {
                builder.add(glob);
                any = true;
            }
        }
    }
    any.then(|| builder.build().ok()).flatten()
}

impl Guard {
    /// The read-only index at `root` and its file list; None when it cannot be read.
    fn index(&mut self, root: &str) -> Option<&(Store, Vec<String>)> {
        self.stores
            .entry(root.to_owned())
            .or_insert_with(|| {
                let store = Store::open(database_path(Path::new(root)), DbOpenOptions { read_only: true, ..Default::default() }).ok()?;
                // Mid-rebuild the index is half-built and holding it would block the file the
                // rebuild replaces: allow, as the prompt hook stays out.
                if store.get_metadata("index_state").ok().flatten().as_deref() == Some("indexing") {
                    return None;
                }
                let files = store.get_all_file_paths().ok()?;
                Some((store, files))
            })
            .as_ref()
    }

    /// Indexed files under `scope` (a file, folder, or path with a wildcard name) that the
    /// filters let through.
    fn indexed_matches(&mut self, root: &str, scope: &Path, globs: &[String], exts: &[String]) -> Vec<String> {
        let mut scope = scope.to_path_buf();
        let mut globs = globs.to_vec();
        if let Some(name) = scope.file_name().map(|n| n.to_string_lossy().into_owned()) {
            if name.contains(['*', '?', '[']) {
                globs.push(name);
                scope.pop();
            }
        }
        let Ok(rel) = scope.strip_prefix(root) else { return Vec::new() };
        let rel = rel.to_string_lossy().replace('\\', "/");
        let Some((_, files)) = self.index(root) else { return Vec::new() };
        let matcher = glob_matcher(&globs);
        files
            .iter()
            .filter(|f| rel.is_empty() || **f == rel || f.strip_prefix(&rel).is_some_and(|r| r.starts_with('/')))
            .filter(|f| exts.is_empty() || f.rsplit_once('.').is_some_and(|(_, e)| exts.iter().any(|x| x.eq_ignore_ascii_case(e))))
            .filter(|f| matcher.as_ref().is_none_or(|m| m.is_match(f.as_str())))
            .cloned()
            .collect()
    }

    /// Identifiers in a code-shaped `pattern` that name indexed symbols.
    fn symbol_hits(&mut self, root: &str, pattern: &str) -> Vec<String> {
        static META: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\[bBwWsSdD]|\\.|[\^$*+?()\[\]{}|<>]").unwrap());
        static LEFTOVER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"[A-Za-z_][A-Za-z0-9_]*|[\s.:,;&#'"=-]"#).unwrap());
        let text = META.replace_all(pattern, " ");
        let idents: Vec<&str> = IDENT.find_iter(&text).map(|m| m.as_str()).collect();
        if idents.is_empty() {
            return Vec::new();
        }
        // Plain words count as symbol names only in a single-token pattern (`open`,
        // `\bopen\b|close`); a phrase with spaces ("failed to open") is a text search.
        let only_idents = LEFTOVER.replace_all(&text, "").is_empty() && !pattern.trim().contains(char::is_whitespace);
        let Some((store, _)) = self.index(root) else { return Vec::new() };
        let mut hits: Vec<String> = Vec::new();
        for ident in &idents {
            if hits.iter().any(|h| h.eq_ignore_ascii_case(ident)) {
                continue;
            }
            let code_shaped = ident.contains('_') || CODE_SHAPED.is_match(ident) || (only_idents && idents.len() <= 3);
            if code_shaped && store.get_nodes_by_lower_name(ident).is_ok_and(|n| !n.is_empty()) {
                hits.push((*ident).to_owned());
            }
        }
        hits
    }

    /// A content search over `scopes`; `pattern` None means it is unknown (inline code, a file of
    /// patterns), which needs one lookup in the project first.
    fn check_search(&mut self, pattern: Option<&str>, scopes: &[PathBuf], globs: &[String], exts: &[String]) -> Result<(), Deny> {
        for scope in scopes {
            let Some(root) = find_root(scope) else { continue };
            if self.indexed_matches(&root, scope, globs, exts).is_empty() {
                continue;
            }
            let Some(pattern) = pattern else {
                if !self.session.roots.contains(&root) {
                    return Err(deny_find(&root, &["the symbols you are after".to_owned()]));
                }
                continue;
            };
            let hits: Vec<String> =
                self.symbol_hits(&root, pattern).into_iter().filter(|h| !self.session.tokens.contains(&h.to_lowercase())).collect();
            if !hits.is_empty() {
                return Err(deny_search(&root, &hits));
            }
        }
        Ok(())
    }

    fn check_find(&mut self, scopes: &[PathBuf], globs: &[String]) -> Result<(), Deny> {
        for scope in scopes {
            let Some(root) = find_root(scope) else { continue };
            if self.session.roots.contains(&root) {
                continue;
            }
            let matches = self.indexed_matches(&root, scope, globs, &[]);
            if !matches.is_empty() {
                let names: Vec<String> = matches.iter().take(3).map(|m| m.rsplit('/').next().unwrap_or(m).to_owned()).collect();
                return Err(deny_find(&root, &names));
            }
        }
        Ok(())
    }
}

fn deny_search(root: &str, hits: &[String]) -> Deny {
    let q = hits.iter().take(6).cloned().collect::<Vec<_>>().join(" ");
    let root_json = serde_json::to_string(root).unwrap_or_default();
    format!(
        "codegraphV guard: `{q}` names indexed symbols in {root}. Call codegraphV_explore(query: \"{q}\", projectPath: {root_json}) \
         first; it returns their verbatim source, callers and call paths. After that, searching for these names is allowed."
    )
}

fn deny_find(root: &str, sample: &[String]) -> Deny {
    let root_json = serde_json::to_string(root).unwrap_or_default();
    format!(
        "codegraphV guard: {root} is indexed by codegraphV. Call codegraphV_explore (projectPath: {root_json}) with the symbol or \
         file names you are looking for first (e.g. {}); listing or finding files in this project is allowed after one codegraphV \
         query this session.",
        sample.join(", ")
    )
}

// ---------------------------------------------------------------- shell parsing

/// Shell words: quotes removed, `\"` and `\\` unescaped inside double quotes.
fn tokenize(s: &str) -> Vec<String> {
    let (mut toks, mut cur, mut quote, mut quoted) = (Vec::new(), String::new(), None::<char>, false);
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some('"') if c == '\\' && chars.peek().is_some_and(|n| matches!(n, '"' | '\\' | '$' | '`')) => cur.push(chars.next().unwrap()),
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                quoted = true;
            }
            None if c.is_whitespace() => {
                if !cur.is_empty() || quoted {
                    toks.push(std::mem::take(&mut cur));
                }
                quoted = false;
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() || quoted {
        toks.push(cur);
    }
    toks
}

/// Split on `seps` outside quotes and `$( )`/`( )`/`{ }` nesting.
fn split_top(s: &str, seps: &[&str]) -> Vec<String> {
    let (mut parts, mut cur, mut quote, mut depth) = (Vec::new(), String::new(), None::<char>, 0usize);
    let mut i = 0;
    while i < s.len() {
        let c = s[i..].chars().next().unwrap();
        let w = c.len_utf8();
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
            cur.push(c);
        } else if c == '(' || c == '{' {
            depth += 1;
            cur.push(c);
        } else if c == ')' || c == '}' {
            depth = depth.saturating_sub(1);
            cur.push(c);
        } else if let Some(sep) = seps.iter().find(|sep| depth == 0 && s[i..].starts_with(**sep)) {
            parts.push(std::mem::take(&mut cur));
            i += sep.len();
            continue;
        } else {
            cur.push(c);
        }
        i += w;
    }
    parts.push(cur);
    parts
}

/// The commands inside `$( … )` and backticks (outside single quotes), which run too.
fn substitutions(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let (mut out, mut i, mut single) = (Vec::new(), 0, false);
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            single = !single;
        } else if !single && (c == '$' || c == '<' || c == '>') && chars.get(i + 1) == Some(&'(') {
            let (mut depth, start) = (1, i + 2);
            let mut j = start;
            while j < chars.len() && depth > 0 {
                match chars[j] {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                j += 1;
            }
            out.push(chars[start..j.saturating_sub(1).max(start)].iter().collect());
            i = j;
            continue;
        } else if !single && c == '`' {
            if let Some(end) = chars[i + 1..].iter().position(|&x| x == '`') {
                out.push(chars[i + 1..i + 1 + end].iter().collect());
                i += end + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Without redirections (`2>/dev/null`, `> out`, `2>&1`, `<in`), which are never paths.
fn drop_redirects(toks: Vec<String>) -> Vec<String> {
    static BARE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d*(>>?|<)|&>>?)$").unwrap());
    static ATTACHED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d*(>>?|<)|&>>?)").unwrap());
    let (mut out, mut skip) = (Vec::new(), false);
    for t in toks {
        if std::mem::take(&mut skip) {
            continue;
        }
        if BARE.is_match(&t) {
            skip = true;
        } else if !ATTACHED.is_match(&t) {
            out.push(t);
        }
    }
    out
}

fn cmd_name(tok: &str) -> String {
    let base = tok.rsplit(['/', '\\']).next().unwrap_or(tok).to_lowercase();
    for ext in [".exe", ".cmd", ".bat", ".ps1"] {
        if let Some(stem) = base.strip_suffix(ext) {
            return stem.to_owned();
        }
    }
    base
}

/// Without leading `VAR=value` assignments and wrappers (`sudo`, `env`, `xargs -0` …).
fn strip_wrappers(mut toks: Vec<String>) -> Vec<String> {
    static ASSIGN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*=").unwrap());
    loop {
        let Some(first) = toks.first() else { return toks };
        let name = cmd_name(first);
        if ASSIGN.is_match(first) {
            toks.remove(0);
        } else if WRAPPERS.contains(&name.as_str()) {
            toks.remove(0);
            if name == "xargs" || name == "env" {
                while toks.first().is_some_and(|t| t.starts_with('-')) {
                    toks.remove(0);
                }
            }
        } else {
            return toks;
        }
    }
}

struct Search {
    pattern: Option<String>,
    paths: Vec<String>,
    globs: Vec<String>,
    exts: Vec<String>,
}

/// A search command's argv (after its name).
fn parse_search(name: &str, args: &[String]) -> Search {
    let mut s = Search { pattern: None, paths: Vec::new(), globs: Vec::new(), exts: Vec::new() };
    if name == "findstr" {
        for a in args {
            if let Some(opt) = a.strip_prefix('/') {
                if opt.len() > 2 && opt[..2].eq_ignore_ascii_case("c:") {
                    s.pattern = Some(opt[2..].to_owned());
                }
            } else if s.pattern.is_none() {
                s.pattern = Some(a.clone());
            } else {
                s.paths.push(a.clone());
            }
        }
        return s;
    }
    let ps = name == "select-string" || name == "sls";
    let (mut i, mut rest_paths, mut from_opt) = (0, false, false);
    while i < args.len() {
        let a = &args[i];
        if rest_paths {
            s.paths.push(a.clone());
        } else if a == "--" {
            rest_paths = true;
        } else if a.starts_with('-') && a.len() > 1 {
            let (key, val) = a.split_once('=').map(|(k, v)| (k.to_owned(), Some(v.to_owned()))).unwrap_or((a.clone(), None));
            let key = if ps { key.to_lowercase() } else { key };
            let val = match val {
                Some(v) => v,
                None if VALUE_OPTS.contains(&key.as_str()) && i + 1 < args.len() => {
                    i += 1;
                    args[i].clone()
                }
                None => String::new(),
            };
            match key.as_str() {
                "-e" | "--regexp" | "-pattern" => {
                    s.pattern = Some(match s.pattern.take() {
                        Some(p) => format!("{p}|{val}"),
                        None => val,
                    });
                    from_opt = true;
                }
                "-f" | "--file" => {
                    s.pattern = None;
                    from_opt = true;
                }
                "-g" | "--glob" | "--iglob" | "--include" | "-include" => s.globs.push(val),
                "-t" | "--type" => s.exts.extend(rg_type_exts(&val.to_lowercase())),
                "-path" | "-literalpath" => s.paths.push(val),
                _ => {}
            }
        } else if s.pattern.is_none() && !from_opt {
            s.pattern = Some(a.clone());
        } else {
            s.paths.push(a.clone());
        }
        i += 1;
    }
    s
}

/// A finder's argv: Some((paths, name globs)) when it lists files.
fn parse_finder(name: &str, args: &[String]) -> Option<(Vec<String>, Vec<String>)> {
    let (mut paths, mut globs) = (Vec::new(), Vec::new());
    match name {
        "find" => {
            let mut i = 0;
            while i < args.len() {
                let a = args[i].as_str();
                if matches!(a, "-name" | "-iname" | "-path" | "-ipath" | "-wholename" | "-regex" | "-iregex") && i + 1 < args.len() {
                    globs.push(if a.contains("regex") { "*".to_owned() } else { args[i + 1].clone() });
                    i += 2;
                    continue;
                }
                if a.starts_with("-exec") {
                    break;
                }
                if matches!(a, "-type" | "-maxdepth" | "-mindepth" | "-newer" | "-size" | "-mtime") {
                    i += 2;
                    continue;
                }
                if !a.starts_with(['-', '(', '!']) {
                    paths.push(a.to_owned());
                }
                i += 1;
            }
        }
        "fd" | "fdfind" => {
            let pos: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
            if let Some((first, rest)) = pos.split_first() {
                globs.push(if first.contains(['*', '?', '[']) { (*first).clone() } else { format!("*{first}*") });
                paths.extend(rest.iter().map(|p| (*p).clone()));
            }
        }
        "locate" => globs.extend(args.iter().filter(|a| !a.starts_with('-')).map(|a| format!("*{a}*"))),
        _ if LISTERS.contains(&name) => {
            let recursive = args.iter().any(|a| {
                let l = a.to_lowercase();
                matches!(l.as_str(), "-recurse" | "-r" | "-s" | "/s" | "-recurse:$true" | "-depth")
                    || (name == "ls" && a.starts_with('-') && !a.starts_with("--") && a.contains('R'))
            });
            if !recursive {
                return None;
            }
            let mut i = 0;
            while i < args.len() {
                let (a, l) = (&args[i], args[i].to_lowercase());
                if matches!(l.as_str(), "-filter" | "-include") && i + 1 < args.len() {
                    globs.extend(args[i + 1].split(',').map(|g| g.trim().to_owned()));
                    i += 2;
                    continue;
                }
                if matches!(l.as_str(), "-path" | "-literalpath") && i + 1 < args.len() {
                    paths.push(args[i + 1].clone());
                    i += 2;
                    continue;
                }
                if matches!(l.as_str(), "-exclude" | "-depth" | "-attributes") && i + 1 < args.len() {
                    i += 2;
                    continue;
                }
                if !a.starts_with(['-', '/']) {
                    if a.contains(['*', '?']) && !a.contains(['/', '\\']) {
                        globs.push(a.clone());
                    } else {
                        paths.push(a.clone());
                    }
                }
                i += 1;
            }
        }
        _ => return None,
    }
    if paths.is_empty() {
        paths.push(".".to_owned());
    }
    Some((paths, globs))
}

fn inline_code(args: &[String]) -> Option<String> {
    let at = args.iter().position(|a| {
        matches!(a.to_lowercase().as_str(), "-c" | "-e" | "--eval" | "-command" | "/c" | "/k" | "-p" | "--print" | "-encodedcommand")
    })?;
    (at + 1 < args.len()).then(|| args[at + 1..].join(" "))
}

/// One parsed pipeline stage: its command name and arguments.
fn stage(seg: &str) -> Option<(String, Vec<String>)> {
    let mut toks = strip_wrappers(drop_redirects(tokenize(seg.trim())));
    if toks.is_empty() {
        return None;
    }
    let name = cmd_name(&toks.remove(0));
    if name == "git" && toks.first().is_some_and(|s| s.eq_ignore_ascii_case("grep")) {
        toks.remove(0);
        return Some(("grep".into(), toks));
    }
    if name == "git" && toks.first().is_some_and(|s| s.eq_ignore_ascii_case("ls-files")) {
        toks.remove(0);
        return Some(("git-ls-files".into(), toks));
    }
    Some((name, toks))
}

/// Every stage of every pipeline in `command`, with the folder it runs in and whether it reads
/// a pipe; substitutions and `bash -c` bodies included.
fn stages(command: &str, cwd: &Path, depth: usize, out: &mut Vec<(PathBuf, usize, String, Vec<String>)>) {
    if depth > 3 {
        return;
    }
    for inner in substitutions(command) {
        stages(&inner, cwd, depth + 1, out);
    }
    let mut eff = cwd.to_path_buf();
    for chain in split_top(command, &["&&", "||", ";", "\r\n", "\n"]) {
        // `( … )` and `{ …; }` run their commands too.
        let t = chain.trim();
        if (t.starts_with('(') && t.ends_with(')')) || (t.starts_with('{') && t.ends_with('}')) {
            stages(&t[1..t.len() - 1], &eff, depth + 1, out);
            continue;
        }
        let segs: Vec<String> = split_top(&chain, &["|"]).into_iter().filter(|s| !s.trim().is_empty()).collect();
        for (pos, seg) in segs.iter().enumerate() {
            let Some((name, args)) = stage(seg) else { continue };
            if CD.contains(&name.as_str()) && pos == 0 {
                if let Some(dir) = args.last() {
                    eff = norm_path(dir, &eff);
                }
                continue;
            }
            if SHELLS.contains(&name.as_str()) {
                if let Some(code) = inline_code(&args) {
                    stages(&code, &eff, depth + 1, out);
                }
                continue;
            }
            out.push((eff.clone(), pos, name, args));
        }
    }
}

impl Guard {
    /// Lookups made by `command`: a stage whose command word is codegraphV.
    fn record_shell_lookups(&mut self, command: &str, cwd: &Path) {
        let mut all = Vec::new();
        stages(command, cwd, 0, &mut all);
        for (eff, _, name, args) in all {
            if name != "codegraphv" || !args.first().is_some_and(|s| LOOKUPS.contains(&s.to_lowercase().as_str())) {
                continue;
            }
            let path = args.iter().position(|a| a == "--path" || a == "-p").and_then(|i| args.get(i + 1)).cloned().or_else(|| {
                args.iter().find_map(|a| a.strip_prefix("--path=").map(str::to_owned))
            });
            let root = find_root(&path.map(|p| norm_path(&p, &eff)).unwrap_or(eff));
            self.session.record(root.as_deref(), &args[1..]);
        }
    }

    fn check_shell(&mut self, command: &str, cwd: &Path) -> Result<(), Deny> {
        let mut all = Vec::new();
        stages(command, cwd, 0, &mut all);
        let mut first_name = String::new();
        for (eff, pos, name, args) in all {
            if pos == 0 {
                first_name = name.clone();
            }
            // A path held in a variable (`grep x $f` in a loop) is unknown: count it as the folder
            // the command runs in.
            let paths = |ps: &[String]| -> Vec<PathBuf> {
                ps.iter().map(|p| if p.contains('$') { eff.clone() } else { norm_path(p, &eff) }).collect()
            };
            let n = name.as_str();
            if SEARCHERS.contains(&n) {
                let s = parse_search(n, &args);
                let listed = matches!(first_name.as_str(), "find" | "fd" | "fdfind" | "git-ls-files" | "locate") || LISTERS.contains(&first_name.as_str());
                if pos == 0 {
                    let scopes = if s.paths.is_empty() { vec![eff.clone()] } else { paths(&s.paths) };
                    self.check_search(s.pattern.as_deref(), &scopes, &s.globs, &s.exts)?;
                } else if !s.paths.is_empty() {
                    // A searcher after a pipe that names its own files reads them, not the pipe.
                    self.check_search(s.pattern.as_deref(), &paths(&s.paths), &s.globs, &s.exts)?;
                } else if listed {
                    // `find … | xargs grep sym`, `gci -r | sls sym`: searching the listed files.
                    self.check_search(s.pattern.as_deref(), &[eff.clone()], &[], &[])?;
                }
                continue;
            }
            if n == "git-ls-files" && pos == 0 {
                let ps: Vec<String> = args.iter().filter(|a| !a.starts_with('-')).cloned().collect();
                self.check_find(&if ps.is_empty() { vec![eff.clone()] } else { paths(&ps) }, &[])?;
                continue;
            }
            if (FINDERS.contains(&n) || LISTERS.contains(&n)) && pos == 0 {
                if n == "find" {
                    if let Some(j) = args.iter().position(|a| a == "-exec" || a == "-execdir") {
                        if args.get(j + 1).is_some_and(|c| SEARCHERS.contains(&cmd_name(c).as_str())) {
                            let rest: Vec<String> =
                                args[j + 2..].iter().filter(|a| !matches!(a.as_str(), "{}" | ";" | "\\;" | "+")).cloned().collect();
                            let s = parse_search(&cmd_name(&args[j + 1]), &rest);
                            let (ps, globs) = parse_finder("find", &args[..j]).unwrap_or_default();
                            self.check_search(s.pattern.as_deref(), &paths(&ps), &globs, &[])?;
                        }
                    }
                }
                if let Some((ps, globs)) = parse_finder(n, &args) {
                    self.check_find(&paths(&ps), &globs)?;
                }
                continue;
            }
            if INTERPRETERS.contains(&n) && inline_code(&args).is_some_and(|code| WALK_CODE.is_match(&code)) {
                self.check_search(None, &[eff.clone()], &[], &[])?;
            }
        }
        Ok(())
    }

    fn decide(&mut self, event: &Value) -> Result<(), Deny> {
        let tool = event["tool_name"].as_str().unwrap_or_default();
        let input = &event["tool_input"];
        let cwd = event["cwd"].as_str().filter(|c| !c.is_empty()).map(PathBuf::from).or_else(|| std::env::current_dir().ok()).unwrap_or_default();
        let text = |key: &str| input[key].as_str().unwrap_or_default().to_owned();
        // Guarding the guard comes first and needs no index: it applies everywhere.
        match tool {
            "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
                let file = input["file_path"].as_str().or(input["notebook_path"].as_str()).unwrap_or_default();
                if file.is_empty() {
                    return Ok(());
                }
                let path = norm_path(file, &cwd);
                if path.file_name().is_some_and(|n| n == "settings.json" || n == "settings.local.json")
                    && path.parent().and_then(Path::file_name).is_some_and(|n| n == ".claude")
                {
                    // The settings case is decided by the edit's effect on the guard entry, not by
                    // the words in it: a write that keeps the guard is a legitimate settings change.
                    return check_settings_edit(input, &path);
                }
                return tamper_reason(&strings_in(input).join("\n")).map_or(Ok(()), Err);
            }
            "Bash" | "PowerShell" => {
                if let Some(reason) = tamper_reason(&text("command")) {
                    return Err(reason);
                }
            }
            _ => {}
        }
        if tool.starts_with("mcp__codegraphV__") {
            let root = match input["projectPath"].as_str() {
                Some(p) => find_root(&norm_path(p, &cwd)),
                None => find_root(&cwd),
            };
            self.session.record(root.as_deref(), &strings_in(input));
            return Ok(());
        }
        match tool {
            "Grep" => {
                let path = norm_path(input["path"].as_str().unwrap_or("."), &cwd);
                let globs: Vec<String> = input["glob"].as_str().map(|g| vec![g.to_owned()]).unwrap_or_default();
                let exts = input["type"].as_str().map(|t| rg_type_exts(&t.to_lowercase())).unwrap_or_default();
                self.check_search(Some(&text("pattern")), &[path], &globs, &exts)
            }
            "Glob" => {
                let path = norm_path(input["path"].as_str().unwrap_or("."), &cwd);
                let pattern = input["pattern"].as_str().unwrap_or("*").to_owned();
                self.check_find(&[path], &[pattern])
            }
            "Bash" | "PowerShell" => {
                let command = text("command");
                self.record_shell_lookups(&command, &cwd);
                self.check_shell(&command, &cwd)
            }
            _ => Ok(()),
        }
    }
}

fn strings_in(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(items) => items.iter().flat_map(strings_in).collect(),
        Value::Object(map) => map.values().flat_map(strings_in).collect(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------- guarding the guard
//
// The rules the agent must not talk its way around. A settings edit is judged by what the edit
// does to the guard entry (see `check_settings_edit`); everything else goes by these text rules,
// which need no index, so they apply in every project.

/// What an edit or command's text may not do to the hook wiring.
fn tamper_reason(text: &str) -> Option<Deny> {
    let lower = text.to_lowercase();
    if lower.contains("disableallhooks") {
        return Some("codegraphV guard: turning every hook off is blocked; ask the user.".into());
    }
    let names_guard = lower.contains("guard-hook") || lower.contains("guard_hook");
    let settings = lower.contains(".claude") && (lower.contains("settings.json") || lower.contains("settings.local.json"));
    if names_guard && settings {
        return Some("codegraphV guard: changing the guard's own hook entry in Claude's settings is blocked; ask the user.".into());
    }
    let shell_write = lower.contains('>')
        || lower.contains("set-content")
        || lower.contains("add-content")
        || lower.contains("out-file")
        || lower.contains("writealltext")
        || lower.contains("remove-item")
        || lower.contains("set-item");
    if settings && shell_write {
        return Some("codegraphV guard: rewriting Claude's settings through a shell is blocked; ask the user to change settings.".into());
    }
    None
}

/// The PreToolUse entries in `settings` that are the guard. A text that does not parse holds no
/// guard, so a settings file that was already broken is not treated as a loss.
fn guard_entries(settings: &Value) -> Vec<Value> {
    settings["hooks"]["PreToolUse"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|group| {
            group["hooks"].as_array().is_some_and(|hooks| hooks.iter().any(codegraph_installer::is_guard_hook))
        })
        .cloned()
        .collect()
}

/// Deny an edit that would remove the guard from Claude's settings file.
///
/// Settings are edited by writing the whole file or by replacing a fragment, so the check applies
/// the edit to the current text and looks for a guard entry that existed before and is gone after.
/// Failing open on the settings case is what let the guard be removed by hand: so an unreadable
/// settings file is denied while an entry is gone, and a broken result is denied, since a file that
/// cannot be read holds no guard to keep.
fn check_settings_edit(input: &Value, file: &Path) -> Result<(), Deny> {
    let Ok(before_text) = std::fs::read_to_string(file) else { return Ok(()) };
    let parse = |text: &str| -> Value {
        jsonc_parser::parse_to_serde_value(text, &Default::default())
            .ok()
            .flatten()
            .unwrap_or(Value::Null)
    };
    let before = guard_entries(&parse(&before_text));
    if before.is_empty() {
        return Ok(());
    }
    let after_text = if input.get("content").and_then(Value::as_str).is_some() {
        input["content"].as_str().unwrap_or_default().to_owned()
    } else if let Some(edits) = input.get("edits").and_then(Value::as_array) {
        edits.iter().fold(before_text.clone(), |text, edit| replace_fragment(&text, edit))
    } else {
        replace_fragment(&before_text, input)
    };
    let after = guard_entries(&parse(&after_text));
    if before.iter().all(|entry| after.contains(entry)) {
        return Ok(());
    }
    Err("codegraphV guard: this change would remove the guard hook from Claude's settings; ask the user. \
         A settings file edited by hand keeps working — reinstall with `codegraphV install` to restore it."
        .into())
}

/// `text` with one `old_string` → `new_string` replacement, as the file tools apply an edit.
fn replace_fragment(text: &str, edit: &Value) -> String {
    let (Some(old), Some(new)) = (edit["old_string"].as_str(), edit["new_string"].as_str()) else {
        return text.to_owned();
    };
    if old.is_empty() {
        return text.to_owned();
    }
    if edit["replace_all"].as_bool() == Some(true) {
        return text.replace(old, new);
    }
    match text.find(old) {
        Some(at) => format!("{}{}{}", &text[..at], new, &text[at + old.len()..]),
        None => text.to_owned(),
    }
}

/// The deny JSON for `event`, or None to let the call through.
fn verdict(event: &Value) -> Option<String> {
    let session = Session::load(event["session_id"].as_str().unwrap_or_default());
    let mut guard = Guard { session, stores: HashMap::new() };
    let reason = guard.decide(event).err()?;
    Some(json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": reason}}).to_string())
}

/// The command: reads the hook payload and prints a deny decision, never failing.
pub fn run() {
    if std::env::var("CODEGRAPHV_GUARD").as_deref() == Ok("0") {
        return;
    }
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    // Past the deadline the call is allowed; returning ends the process and any blocked read.
    std::thread::spawn(move || {
        let _ = sender.send(payload_verdict());
    });
    if let Ok(Some(text)) = receiver.recv_timeout(std::time::Duration::from_millis(5_000)) {
        print!("{text}");
    }
}

fn payload_verdict() -> Option<String> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return None;
    }
    let mut raw = String::new();
    const MAX_INPUT: u64 = 4 * 1_048_576;
    if stdin.lock().take(MAX_INPUT + 1).read_to_string(&mut raw).is_err() || raw.len() as u64 > MAX_INPUT {
        return None;
    }
    let event: Value = serde_json::from_str(&raw).ok()?;
    if !event.is_object() {
        return None;
    }
    std::panic::set_hook(Box::new(|_| {}));
    std::panic::catch_unwind(|| verdict(&event)).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(command: &str) -> Vec<(usize, String)> {
        let mut out = Vec::new();
        stages(command, Path::new("/w"), 0, &mut out);
        out.into_iter().map(|(_, pos, name, _)| (pos, name)).collect()
    }

    #[test]
    fn stages_see_pipes_chains_substitutions_and_shell_bodies() {
        assert_eq!(names("git log | grep x"), [(0, "git".into()), (1, "grep".into())]);
        assert_eq!(names("a && b; c"), [(0, "a".into()), (0, "b".into()), (0, "c".into())]);
        assert_eq!(names("echo $(rg x src)"), [(0, "rg".into()), (0, "echo".into())]);
        assert_eq!(names("echo `grep -r x .`"), [(0, "grep".into()), (0, "echo".into())]);
        assert_eq!(names("bash -c 'rg x src'"), [(0, "rg".into())]);
        assert_eq!(names("echo '$(rg x src)'"), [(0, "echo".into())], "single quotes do not run");
        assert_eq!(names("FOO=1 sudo rg x"), [(0, "rg".into())]);
        assert_eq!(names("C:/tools/rg.exe x"), [(0, "rg".into())]);
    }

    #[test]
    fn search_argv_separates_pattern_paths_and_filters() {
        let args = |s: &str| tokenize(s);
        let s = parse_search("rg", &args("-n -g '*.rs' -t py parseToken src lib"));
        assert_eq!(s.pattern.as_deref(), Some("parseToken"));
        assert_eq!(s.paths, ["src", "lib"]);
        assert_eq!(s.globs, ["*.rs"]);
        assert_eq!(s.exts, ["py", "pyi"]);
        let s = parse_search("grep", &args("-e foo -e bar file"));
        assert_eq!(s.pattern.as_deref(), Some("foo|bar"));
        assert_eq!(s.paths, ["file"]);
        let s = parse_search("select-string", &args("-Path src/*.ts -Pattern parseToken"));
        assert_eq!(s.pattern.as_deref(), Some("parseToken"));
        assert_eq!(s.paths, ["src/*.ts"]);
        let s = parse_search("findstr", &args("/s /c:parse_token *.c"));
        assert_eq!(s.pattern.as_deref(), Some("parse_token"));
    }

    #[test]
    fn redirections_are_never_paths() {
        assert_eq!(drop_redirects(tokenize("rg x src 2>/dev/null > out 2>&1")), ["rg", "x", "src"]);
    }

    #[test]
    fn finders_and_recursive_listers_list_files() {
        let args = |s: &str| tokenize(s);
        assert_eq!(parse_finder("find", &args(". -name '*.ts' -type f")), Some((vec![".".into()], vec!["*.ts".into()])));
        assert_eq!(parse_finder("ls", &args("-la")), None);
        assert_eq!(parse_finder("ls", &args("-R src")), Some((vec!["src".into()], vec![])));
        assert_eq!(parse_finder("get-childitem", &args("-Recurse -Filter *.cs")), Some((vec![".".into()], vec!["*.cs".into()])));
    }
}
