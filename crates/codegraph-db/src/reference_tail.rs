//! The keys sync's failed-ref retry (#1240) matches on: the tail a failed ref is
//! parked under, and the names a newly added file can be imported as. Shared by
//! the query layer, which writes and reads them, and the migrations that
//! rewrote the tails parked before path imports, path references and module
//! references had ones of their own.
//!
//! Port of `src/db/reference-tail.ts`.

use regex::Regex;
use std::sync::LazyLock;

/// A file name with an extension: 'price.liquid', 'icon.logo.liquid',
/// 'about.tsx' — not 'x.component#Name'.
static FILE_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\w$@+~.-]+\.[A-Za-z][A-Za-z0-9]*$").unwrap());

/// A route's reference to the module it lazily loads, capturing the module's
/// path: React Router's `lazy-import:<path>`, Vue Router's and Angular's
/// `import:<path>#<export>` — a path may hold a `#` itself, as vben's `#/views/…`
/// alias does — each also behind `layout:`, for a layout route's module.
static MODULE_REFERENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:layout:)?(?:lazy-import:(.+)|import:(.+)#[^#]+)$").unwrap());

static ARITY_SUFFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/\d{1,3}$").unwrap());

/// What a module reference's tail starts with. No symbol's name does, so a
/// lookup by symbol names never finds one.
const MODULE_KEY: &str = "module:";

/// Last segment of a (possibly dotted/qualified) reference name — the part a
/// new symbol's plain node name could match: 'util.greet' → 'greet',
/// 'mod::fn' → 'fn', 'greet' → 'greet'. Written to `unresolved_refs.name_tail`
/// when a ref is marked failed, so the #1240 retry lookup can match dotted refs
/// against newly-added node names.
///
/// An import written as a path names a FILE, so its tail is the path's last
/// segment cut at its first dot — `package:app/b.dart` → 'b', `./foo` → 'foo',
/// `inc/db.php` → 'db'.
///
/// A route's reference to the module it lazily loads names a file the same way
/// (`lazy-import:./pages/Team`, `import:./home/home.component#HomeComponent`);
/// it is parked under that path's stem behind 'module:' — 'module:Team',
/// 'module:home' — the key [`module_reference_keys`] gives a file that could be
/// the module.
///
/// Any other reference written as a path to a file names it by the file's own
/// name: Liquid's `{% render 'price' %}` is `snippets/price.liquid`, parked as
/// 'price.liquid'. A call's slashes are in its arguments, a comment or a
/// division, so only a `references` ref is read as a path.
pub fn reference_name_tail(reference_name: &str, reference_kind: Option<&str>) -> String {
    if reference_kind == Some("imports") && reference_name.trim_end_matches('/').contains('/') {
        let stem = path_stem(reference_name);
        if !stem.is_empty() {
            return stem;
        }
    }
    // Vue Router's route names the component it renders as a call.
    if matches!(reference_kind, Some("references") | Some("calls")) {
        if let Some(c) = MODULE_REFERENCE.captures(reference_name) {
            let module = c.get(1).or_else(|| c.get(2)).map(|m| m.as_str()).unwrap_or("");
            let tail = module_tail(module);
            if !tail.is_empty() {
                return tail;
            }
        }
    }
    if reference_kind == Some("references") {
        let file_name = match reference_name.rfind('/') {
            Some(i) => &reference_name[i + 1..],
            None => reference_name,
        };
        if file_name != reference_name && FILE_NAME.is_match(file_name) {
            return file_name.to_string();
        }
    }
    // Erlang refs carry a written arity (`f/1`, `mod::fn/2` — #1610); the tail a
    // new symbol's plain name could match is the arity-less function name.
    let stripped = ARITY_SUFFIX.replace(reference_name, "");
    let base: &str = if stripped.is_empty() { reference_name } else { &stripped };
    match base.rfind(['.', ':']) {
        Some(idx) => base[idx + 1..].to_string(),
        None => base.to_string(),
    }
}

/// Whether a tail or lookup key waits for a module (`module:Team`) rather than
/// for a name.
pub fn is_module_key(key: &str) -> bool {
    key.starts_with(MODULE_KEY)
}

/// SQL for the tail a reference parked under its module (`module:*`) has by its
/// own name: what follows the name's last dot — `store.get` → 'get' — or the
/// whole name. idx_unresolved_failed_module_member indexes the expression, and
/// SQLite uses an expression index only for a query that spells the expression
/// exactly so: schema.sql and the v16 migration do.
pub const MEMBER_TAIL_SQL: &str =
    "substr(reference_name, length(rtrim(reference_name, replace(reference_name, '.', ''))) + 1)";

/// The rows parked under a module: the partial WHERE of the indexes that find them.
pub const MODULE_PARKED_SQL: &str = "status = 'failed' AND name_tail GLOB 'module:*'";

/// The tail of a reference that waits for a module rather than for a name: the
/// module path's stem behind 'module:' — `./pages/Team` → 'module:Team',
/// `@/views/error/404.vue` → 'module:404' — or '' when the path's last segment
/// is only dots.
pub fn module_tail(module_path: &str) -> String {
    let stem = path_stem(module_path);
    if stem.is_empty() {
        String::new()
    } else {
        format!("{MODULE_KEY}{stem}")
    }
}

/// A path's last segment up to its first dot — `./pages/Team` → 'Team',
/// `a/b.dart` → 'b' — or '' when that is only dots.
fn path_stem(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    let last = match trimmed.rfind('/') {
        Some(i) => &trimmed[i + 1..],
        None => trimmed,
    };
    let stem = file_name_stem(last);
    // `..` and `.` name a folder only through the referring file's location.
    if stem.chars().all(|c| c == '.') {
        String::new()
    } else {
        stem.to_string()
    }
}

/// A file or folder name up to its first dot: 'b.dart' → 'b', 'types.d.ts' →
/// 'types', '.eslintrc.js' → '.eslintrc'.
fn file_name_stem(name: &str) -> &str {
    // indexOf('.', 1): a leading dot is part of the name.
    let mut chars = name.char_indices();
    chars.next();
    match chars.find(|&(_, c)| c == '.') {
        Some((i, _)) if i > 0 => &name[..i],
        _ => name,
    }
}

/// What an import of the file at `file_path` can be written as, for the failed
/// imports a sync that adds the file retries: its name (`b.h`, a bare include)
/// and stem (`b`, the tail of `a/b.h`, `./b`, `pkg.b`), and the same two for
/// its folder, which an import names for its index file (`./ui` → `ui/index.ts`,
/// `pkg` → `pkg/__init__.py`) or as a Go package.
pub fn import_path_keys(file_path: &str) -> Vec<String> {
    let parts: Vec<&str> = file_path.split('/').collect();
    let mut keys: Vec<String> = Vec::new();
    let mut push = |k: String| {
        if !keys.contains(&k) {
            keys.push(k);
        }
    };
    for name in &parts[parts.len().saturating_sub(2)..] {
        push((*name).to_string());
        push(file_name_stem(name).to_string());
    }
    keys
}

/// The keys a module reference the file at `file_path` could satisfy is parked
/// under: its [`import_path_keys`] behind 'module:' — `src/pages/Team.tsx` is
/// the module of `lazy-import:./pages/Team`, `pages/Team/index.tsx` of the same
/// path through its folder. A sync looks them up for the files it adds AND the
/// ones it changes: a route renders the component its module exports, so an
/// edit that gives the module one is what the route waited for.
pub fn module_reference_keys(file_path: &str) -> Vec<String> {
    let mut keys = import_path_keys(file_path);
    // A Python package's `__init__.py` is a submodule of the package above it,
    // as a module file beside it is through its folder:
    // `app/pkg/sub/__init__.py` is what `from app.pkg import sub as s`, parked
    // as 'module:pkg', imports.
    let parts: Vec<&str> = file_path.split('/').collect();
    let above = if parts.len() >= 3 && parts[parts.len() - 1] == "__init__.py" {
        parts[parts.len() - 3]
    } else {
        ""
    };
    if !above.is_empty() && !keys.iter().any(|k| k == above) {
        keys.push(above.to_string());
    }
    keys.into_iter().map(|k| format!("{MODULE_KEY}{k}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tails_of_plain_and_qualified_names() {
        assert_eq!(reference_name_tail("util.greet", None), "greet");
        assert_eq!(reference_name_tail("mod::fn", None), "fn");
        assert_eq!(reference_name_tail("greet", None), "greet");
        // Erlang arity is dropped before the tail is taken.
        assert_eq!(reference_name_tail("mod::fn/2", Some("calls")), "fn");
    }

    #[test]
    fn path_imports_park_under_the_file_stem() {
        assert_eq!(reference_name_tail("package:app/b.dart", Some("imports")), "b");
        assert_eq!(reference_name_tail("./foo", Some("imports")), "foo");
        assert_eq!(reference_name_tail("inc/db.php", Some("imports")), "db");
        // Expected values below were recorded from the TypeScript implementation.
        assert_eq!(reference_name_tail("../", Some("imports")), "/");
        assert_eq!(reference_name_tail("./", Some("imports")), "/");
        assert_eq!(reference_name_tail("a/.eslintrc.js", Some("imports")), ".eslintrc");
        assert_eq!(reference_name_tail("x/y/", Some("imports")), "y");
    }

    #[test]
    fn module_references_park_behind_module_key() {
        assert_eq!(reference_name_tail("lazy-import:./pages/Team", Some("references")), "module:Team");
        assert_eq!(
            reference_name_tail("import:./home/home.component#HomeComponent", Some("calls")),
            "module:home"
        );
        assert_eq!(reference_name_tail("layout:lazy-import:@/views/error/404.vue", Some("references")), "module:404");
        assert!(is_module_key("module:Team"));
        assert!(!is_module_key("Team"));
    }

    #[test]
    fn path_references_park_under_the_file_name() {
        assert_eq!(reference_name_tail("snippets/price.liquid", Some("references")), "price.liquid");
        assert_eq!(reference_name_tail("snippets/price.liquid", Some("calls")), "liquid");
    }

    #[test]
    fn file_keys() {
        assert_eq!(import_path_keys("src/ui/b.h"), ["ui", "b.h", "b"]);
        assert_eq!(module_reference_keys("src/pages/Team.tsx"), ["module:pages", "module:Team.tsx", "module:Team"]);
        assert_eq!(
            module_reference_keys("app/pkg/sub/__init__.py"),
            ["module:sub", "module:__init__.py", "module:__init__", "module:pkg"]
        );
        assert_eq!(import_path_keys("b.h"), ["b.h", "b"]);
    }
}
