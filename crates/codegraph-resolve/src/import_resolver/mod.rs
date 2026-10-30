//! Import Resolver — port of `src/resolution/import-resolver.ts`.
//!
//! Resolves import paths to actual files and symbols.

pub mod cpp_include;
pub mod exports;
pub mod find_exported;
pub mod mappings;
pub mod python;
pub mod via_import;

pub use find_exported::*;
pub use cpp_include::{clear_cpp_include_dir_cache, load_cpp_include_dirs};
pub use mappings::{extract_import_mappings, extract_re_exports};
pub use python::{python_package_importers, python_reopen_scope, python_root_fingerprint};
pub use via_import::*;

use crate::ctx::Ctx;
use crate::path_aliases::{alias_and_base_url_candidates, apply_aliases};
use crate::types::UnresolvedRef;
use crate::workspace_packages::{resolve_workspace_import, workspace_import_targets, WorkspaceImportTargets};
use codegraph_jsre::re;
use codegraph_types::{nodepath, EdgeKind, Language, NodeKind};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Extension resolution order by language.
pub fn extensions_for(language: Language) -> &'static [&'static str] {
    use Language::*;
    match language {
        // `/index.jsx` last, as tsc probes a directory: a route table resolves the module its
        // `lazy(() => import('views/x'))` loads as TypeScript, whatever the file is written in.
        TypeScript => &[".ts", ".tsx", ".d.ts", ".js", ".jsx", "/index.ts", "/index.tsx", "/index.js", "/index.jsx"],
        // ArkTS imports both `.ets` components and plain `.ts` logic modules. `/Index.ets` (capital
        // I) is ohpm's module-entry convention.
        ArkTs => &[".ets", ".ts", ".d.ts", ".js", "/Index.ets", "/index.ets", "/index.ts", "/index.js"],
        // A JavaScript file imports TypeScript modules too, as the bundler and tsc's `allowJs`
        // read it. JavaScript comes first, so a `helpers.ts` beside `helpers.js` doesn't take its
        // importers. Never `.d.ts`: a declaration file is not the module a JavaScript file runs.
        JavaScript => &[".js", ".jsx", ".mjs", ".cjs", ".xsjs", ".xsjslib", ".ts", ".tsx", "/index.js", "/index.jsx", "/index.ts", "/index.tsx"],
        Tsx => &[".tsx", ".ts", ".d.ts", ".js", ".jsx", "/index.tsx", "/index.ts", "/index.js", "/index.jsx"],
        Jsx => &[".jsx", ".js", ".tsx", ".ts", "/index.jsx", "/index.js", "/index.tsx", "/index.ts"],
        // SFC consumers import plain TS/JS, sibling components, and barrels (#629).
        Svelte => &[".ts", ".js", ".svelte", ".tsx", ".jsx", "/index.ts", "/index.js", "/index.svelte"],
        Vue => &[".ts", ".js", ".vue", ".tsx", ".jsx", "/index.ts", "/index.js", "/index.vue"],
        Astro => &[".ts", ".js", ".astro", ".tsx", ".jsx", "/index.ts", "/index.js", "/index.astro"],
        Python => &[".py", "/__init__.py"],
        Go => &[".go"],
        Rust => &[".rs", "/mod.rs"],
        Java => &[".java"],
        C => &[".h", ".c"],
        Cpp => &[".h", ".hpp", ".hxx", ".cpp", ".cc", ".cxx"],
        CSharp => &[".cs"],
        Php => &[".php"],
        Ruby => &[".rb"],
        ObjC => &[".h", ".m", ".mm"],
        Nix => &[".nix", "/default.nix"],
        _ => &[],
    }
}

/// Languages whose imports are ES-module specifiers, extracted by `extractJSImports` and
/// therefore classified by the same bare-specifier / alias / workspace rules.
pub fn is_esm_import_language(language: Language) -> bool {
    matches!(
        language,
        Language::TypeScript
            | Language::Tsx
            | Language::JavaScript
            | Language::Jsx
            | Language::ArkTs
            | Language::Svelte
            | Language::Vue
            | Language::Astro
    )
}

pub fn is_nix_path_import_ref(r: &UnresolvedRef) -> bool {
    r.language == Language::Nix
        && r.is_kind(EdgeKind::Imports)
        && (r.reference_name.starts_with("./") || r.reference_name.starts_with("../"))
        && !re!(r#"[\s{}()[\];"'<>$]"#).is_match(&r.reference_name)
}

/// Is this reference a PHP include/require PATH (vs a namespace `use` symbol)? A slash or dot
/// marks a path-shaped include; such references resolve to files only.
pub fn is_php_include_path_ref(r: &UnresolvedRef) -> bool {
    r.language == Language::Php && r.is_kind(EdgeKind::Imports) && (r.reference_name.contains('/') || r.reference_name.contains('.'))
}

/// Is this a COBOL COPY / EXEC SQL INCLUDE copybook reference?
pub fn is_cobol_copybook_ref(r: &UnresolvedRef) -> bool {
    r.language == Language::Cobol && r.is_kind(EdgeKind::Imports)
}

/// Is this a Dart `import`, `export` or `part`? Its name is the URI.
pub fn is_dart_import_ref(r: &UnresolvedRef) -> bool {
    r.language == Language::Dart && r.is_kind(EdgeKind::Imports)
}

/// Is this a Lua / Luau `require`?
pub fn is_lua_require_ref(r: &UnresolvedRef) -> bool {
    matches!(r.language, Language::Lua | Language::Luau) && r.is_kind(EdgeKind::Imports)
}

// Per-context memos for the hottest pure lookups on the resolution path: import-specifier → file
// resolution and exported-symbol lookup. Both are pure given a stable file set + node table,
// which is exactly the window between `Ctx::clear_caches` calls.
#[derive(Default)]
struct ImportPathMemo(RefCell<HashMap<String, Option<String>>>);
#[derive(Default)]
struct CobolCopybookIndex(RefCell<Option<Rc<HashMap<String, Vec<String>>>>>);
#[derive(Default)]
struct LuaBasenameIndex(RefCell<Option<Rc<HashMap<String, Vec<String>>>>>);

/// Basename → file-paths index for Lua/Luau require resolution, in `get_all_files` order.
pub fn lua_basename_index(ctx: &Ctx) -> Rc<HashMap<String, Vec<String>>> {
    let memo = ctx.memo::<LuaBasenameIndex>();
    if let Some(i) = memo.0.borrow().as_ref() {
        return Rc::clone(i);
    }
    let mut index: HashMap<String, Vec<String>> = HashMap::new();
    for f in ctx.get_all_files() {
        let base = f.rsplit('/').next().unwrap_or("").to_string();
        index.entry(base).or_default().push(f);
    }
    let index = Rc::new(index);
    *memo.0.borrow_mut() = Some(Rc::clone(&index));
    index
}

/// Resolve an import path to an actual file.
pub fn resolve_import_path(import_path: &str, from_file: &str, language: Language, ctx: &Ctx) -> Option<String> {
    let memo = ctx.memo::<ImportPathMemo>();
    let key = format!("{}\0{}\0{}", language.as_str(), from_file, import_path);
    if let Some(hit) = memo.0.borrow().get(&key) {
        return hit.clone();
    }
    let resolved = resolve_import_path_uncached(import_path, from_file, language, ctx);
    memo.0.borrow_mut().insert(key, resolved.clone());
    resolved
}

fn resolve_import_path_uncached(import_path: &str, from_file: &str, language: Language, ctx: &Ctx) -> Option<String> {
    // COBOL COPY/EXEC SQL INCLUDE names a copybook member, not a path — the compiler searches a
    // library, so we match against indexed file basenames. Must run before is_external_import: a
    // bare member name would otherwise be misclassified as an external package.
    if language == Language::Cobol {
        return resolve_cobol_copybook(import_path, from_file, ctx);
    }

    // Skip external/npm packages — but pass the context so the bare-specifier heuristic can
    // consult the project's tsconfig alias map first. What still looks like a package may name a
    // file through the importing app's own config.
    if is_external_import(import_path, language, ctx, ExternalOpts { from_file: Some(from_file), ..ExternalOpts::default() }) {
        return resolve_from_own_config(import_path, from_file, language, ctx);
    }

    // Python names modules, not paths: one resolver answers relative and absolute specifiers
    // alike, so every caller gets the same answer.
    if language == Language::Python {
        return python::resolve_python_module(import_path, from_file, ctx);
    }

    let project_root = ctx.project_root();
    let from_dir = nodepath::dirname(&nodepath::join(&[project_root, from_file]));

    // Handle relative imports.
    if import_path.starts_with('.') {
        return resolve_relative_import(import_path, &from_dir, language, ctx);
    }

    // Handle absolute/aliased imports (like @/ or src/).
    if let Some(aliased) = resolve_aliased_import(import_path, project_root, language, ctx, Some(from_file)) {
        return Some(aliased);
    }

    // C/C++ include directory search: when neither relative nor aliased resolution found a
    // match, search -I directories from compile_commands.json or heuristic probing.
    if language == Language::C || language == Language::Cpp {
        return resolve_cpp_include_path(import_path, language, ctx);
    }

    None
}

/// COBOL copybook lookup: `COPY CVACT01Y` (or `EXEC SQL INCLUDE X`) names a library member
/// resolved by the compiler's copybook search path, so we match the member against indexed file
/// basenames, case-insensitively. `.cpy` wins over a same-named program; a same-directory hit
/// wins within a tier.
fn resolve_cobol_copybook(member: &str, from_file: &str, ctx: &Ctx) -> Option<String> {
    let memo = ctx.memo::<CobolCopybookIndex>();
    let index = {
        let existing = memo.0.borrow().clone();
        match existing {
            Some(i) => i,
            None => {
                let mut index: HashMap<String, Vec<String>> = HashMap::new();
                for file_node in ctx.get_nodes_by_kind(NodeKind::File).iter() {
                    let normalized = file_node.file_path.replace('\\', "/");
                    let base = normalized.rsplit('/').next().unwrap_or("");
                    let stem = match base.rfind('.') {
                        Some(dot) if dot > 0 => &base[..dot],
                        _ => base,
                    };
                    index.entry(stem.to_lowercase()).or_default().push(file_node.file_path.clone());
                }
                let index = Rc::new(index);
                *memo.0.borrow_mut() = Some(Rc::clone(&index));
                index
            }
        }
    };

    let candidates = index.get(&member.to_lowercase())?;
    if candidates.is_empty() {
        return None;
    }

    let from_normalized = from_file.replace('\\', "/");
    let from_dir = dir_part(&from_normalized);
    let mut best: Option<&String> = None;
    let mut best_score = -1i32;
    for candidate in candidates {
        let normalized = candidate.replace('\\', "/");
        let ext = match normalized.rfind('.') {
            Some(i) => normalized[i..].to_lowercase(),
            // `slice(lastIndexOf('.'))` with -1 slices from the last character.
            None => normalized.chars().last().map(|c| c.to_string()).unwrap_or_default().to_lowercase(),
        };
        let mut score = 0;
        if ext == ".cpy" {
            score += 4;
        } else if ext == ".cbl" || ext == ".cob" || ext == ".cobol" {
            score += 2;
        }
        if dir_part(&normalized) == from_dir {
            score += 1;
        }
        if score > best_score {
            best_score = score;
            best = Some(candidate);
        }
    }
    best.cloned()
}

/// `p.split('/').slice(0, -1).join('/')`.
fn dir_part(p: &str) -> String {
    let parts: Vec<&str> = p.split('/').collect();
    parts[..parts.len() - 1].join("/")
}

#[derive(Debug, Clone, Copy)]
pub struct ExternalOpts<'a> {
    pub alias_prefixes: bool,
    /// The importing file: a package another package depends on by path
    /// (`"acme-shared": "file:../common"`) is the project's for the files under the manifest that
    /// declares it (#2456).
    pub from_file: Option<&'a str>,
}

impl Default for ExternalOpts<'_> {
    fn default() -> Self {
        ExternalOpts { alias_prefixes: true, from_file: None }
    }
}

/// C and C++ standard library header names (without delimiters).
fn is_c_cpp_stdlib_header(name: &str) -> bool {
    matches!(
        name,
        // C standard library headers
        "assert.h" | "complex.h" | "ctype.h" | "errno.h" | "fenv.h" | "float.h"
        | "inttypes.h" | "iso646.h" | "limits.h" | "locale.h" | "math.h" | "setjmp.h"
        | "signal.h" | "stdalign.h" | "stdarg.h" | "stdatomic.h" | "stdbool.h"
        | "stddef.h" | "stdint.h" | "stdio.h" | "stdlib.h" | "stdnoreturn.h"
        | "string.h" | "tgmath.h" | "threads.h" | "time.h" | "uchar.h" | "wchar.h"
        | "wctype.h"
        // C++ C-library wrappers (cname form)
        | "cassert" | "ccomplex" | "cctype" | "cerrno" | "cfenv" | "cfloat"
        | "cinttypes" | "ciso646" | "climits" | "clocale" | "cmath" | "csetjmp"
        | "csignal" | "cstdalign" | "cstdarg" | "cstdbool" | "cstddef" | "cstdint"
        | "cstdio" | "cstdlib" | "cstring" | "ctgmath" | "ctime" | "cuchar"
        | "cwchar" | "cwctype"
        // C++ STL headers
        | "algorithm" | "any" | "array" | "atomic" | "barrier" | "bit" | "bitset"
        | "charconv" | "chrono" | "codecvt" | "compare" | "complex" | "concepts"
        | "condition_variable" | "coroutine" | "deque" | "exception" | "execution"
        | "expected" | "filesystem" | "format" | "forward_list" | "fstream"
        | "functional" | "future" | "generator" | "initializer_list" | "iomanip"
        | "ios" | "iosfwd" | "iostream" | "istream" | "iterator" | "latch"
        | "limits" | "list" | "locale" | "map" | "mdspan" | "memory" | "memory_resource"
        | "mutex" | "new" | "numbers" | "numeric" | "optional" | "ostream" | "print"
        | "queue" | "random" | "ranges" | "ratio" | "regex" | "scoped_allocator"
        | "semaphore" | "set" | "shared_mutex" | "source_location" | "span"
        | "spanstream" | "sstream" | "stack" | "stacktrace" | "stdexcept"
        | "stdfloat" | "stop_token" | "streambuf" | "string" | "string_view"
        | "strstream" | "syncstream" | "system_error" | "thread" | "tuple"
        | "type_traits" | "typeindex" | "typeinfo" | "unordered_map"
        | "unordered_set" | "utility" | "valarray" | "variant" | "vector"
        | "version"
    )
}

/// Check if an import is external (npm package, etc.).
///
/// `ctx` is consulted for project-defined path aliases (tsconfig/jsconfig `paths`). Without that
/// check, custom prefixes like `@components/*` would fail the bare-specifier heuristic and be
/// classified as external before alias resolution can run.
///
/// `alias_prefixes: false` skips that check, for a caller that has already asked
/// `resolve_import_path` whether an alias maps the specifier to a file.
pub fn is_external_import(import_path: &str, language: Language, ctx: &Ctx, options: ExternalOpts) -> bool {
    // Relative imports are not external.
    if import_path.starts_with('.') {
        return false;
    }

    // Workspace-member imports (`@scope/ui`, `@scope/ui/widgets`) are LOCAL to a monorepo even
    // though they look like bare npm specifiers (#629).
    if let Some(workspaces) = ctx.get_workspace_packages() {
        if resolve_workspace_import(import_path, &workspaces, options.from_file).is_some() {
            return false;
        }
    }

    // Common external patterns.
    if is_esm_import_language(language) {
        // Node built-ins.
        if ["fs", "path", "os", "crypto", "http", "https", "url", "util", "events", "stream", "child_process", "buffer"].contains(&import_path) {
            return true;
        }
        // Project-defined alias prefix? Treat as local. A catch-all `"*"` has no prefix and
        // matches `react` too: what it names is left to resolve_from_own_config.
        let aliases = if options.alias_prefixes { ctx.get_project_aliases() } else { None };
        if let Some(aliases) = aliases {
            for pat in &aliases.patterns {
                if !pat.prefix.is_empty() && import_path.starts_with(&pat.prefix) {
                    return false;
                }
            }
        }
        // Scoped packages or bare specifiers that don't start with aliases.
        if !import_path.starts_with("@/") && !import_path.starts_with("~/") && !import_path.starts_with("src/") {
            // Likely an npm package.
            return true;
        }
    }

    if language == Language::Python {
        // Standard library modules.
        let std_libs = ["os", "sys", "json", "re", "math", "datetime", "collections", "typing", "pathlib", "logging"];
        if std_libs.contains(&import_path.split('.').next().unwrap_or("")) {
            return true;
        }
    }

    if language == Language::Go {
        // In-module imports look like `<module-path>/sub/pkg` — local to this project (#388,
        // #2322).
        if ctx.get_go_package_dir(import_path, None).is_some() {
            return false;
        }
        // `internal/` packages stay local even when go.mod is missing.
        if import_path.contains("/internal/") {
            return false;
        }
        // Anything else is the Go standard library or a third-party module.
        return true;
    }

    if language == Language::C || language == Language::Cpp {
        // C/C++ standard library headers — both C-style (<stdio.h>) and C++-style (<cstdio>,
        // <vector>) forms.
        if is_c_cpp_stdlib_header(import_path) {
            return true;
        }
        // C++ headers without .h extension (e.g. "vector", "string").
        let without_ext = import_path.strip_suffix(".h").unwrap_or(import_path);
        if is_c_cpp_stdlib_header(without_ext) {
            return true;
        }
    }

    false
}

/// Resolve a relative import.
fn resolve_relative_import(import_path: &str, from_dir: &str, language: Language, ctx: &Ctx) -> Option<String> {
    let project_root = ctx.project_root();
    let extensions = extensions_for(language);

    // Try the path as-is first.
    let base_path = nodepath::resolve(&[from_dir, import_path]);
    let relative_path = nodepath::relative(project_root, &base_path).replace('\\', "/");

    // Try each extension. `require('..')` up to the project root is its `index.js`.
    for ext in extensions {
        if relative_path.is_empty() && !ext.starts_with('/') {
            continue;
        }
        let candidate = if relative_path.is_empty() { ext[1..].to_string() } else { format!("{relative_path}{ext}") };
        if ctx.file_exists(&candidate) {
            return Some(candidate);
        }
    }

    // Try without extension (might already have one).
    if ctx.file_exists(&relative_path) {
        return Some(relative_path);
    }

    find_source_for_emitted_specifier(&relative_path, language, ctx)
}

/// TypeScript under `moduleResolution: node16 | nodenext | bundler` writes the EMITTED extension
/// in the specifier (`import x from './util.js'` for `util.ts`, `.mjs` for `.mts`, `.cjs` for
/// `.cts`), and the source file with that exact name never exists in the repo.
fn find_source_for_emitted_specifier(relative_path: &str, language: Language, ctx: &Ctx) -> Option<String> {
    find_source_for_emitted_specifier_in(relative_path, language, &|file| ctx.file_exists(file))
}

/// `find_source_for_emitted_specifier` with its own idea of which files exist.
fn find_source_for_emitted_specifier_in(relative_path: &str, language: Language, exists: &dyn Fn(&str) -> bool) -> Option<String> {
    if !matches!(
        language,
        Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx | Language::Vue | Language::Svelte | Language::Astro | Language::ArkTs
    ) {
        return None;
    }
    let table: [(&str, &[&str]); 4] = [
        (".js", &[".ts", ".tsx", ".d.ts"]),
        (".jsx", &[".tsx"]),
        (".mjs", &[".mts", ".d.mts"]),
        (".cjs", &[".cts", ".d.cts"]),
    ];
    for (emitted, sources) in table {
        let Some(stem) = relative_path.strip_suffix(emitted) else { continue };
        for ext in sources {
            let candidate = format!("{stem}{ext}");
            if exists(&candidate) {
                return Some(candidate);
            }
        }
        return None;
    }
    None
}

/// Resolve an aliased/absolute import. Tries, in order: project-defined `compilerOptions.paths`,
/// workspace packages, the legacy hard-coded fallback list (`@/`, `~/`, `src/`, ...), and the
/// direct path.
fn resolve_aliased_import(import_path: &str, project_root: &str, language: Language, ctx: &Ctx, from_file: Option<&str>) -> Option<String> {
    let try_with_ext = |base_path: &str| try_with_extensions(base_path, language, ctx);

    // 1. tsconfig/jsconfig paths: the config nearest the importing file (an app of a monorepo
    //    keeps its own `@/*`), then the project root's.
    let nearest = from_file.and_then(|f| ctx.get_nearest_aliases(f));
    let root_map = ctx.get_project_aliases();
    let mut maps: Vec<Rc<crate::path_aliases::AliasMap>> = Vec::new();
    match (&nearest, &root_map) {
        (Some(n), Some(r)) if !Rc::ptr_eq(n, r) => {
            maps.push(Rc::clone(n));
            maps.push(Rc::clone(r));
        }
        (Some(n), None) => {
            maps.push(Rc::clone(n));
        }
        (Some(_), Some(r)) | (None, Some(r)) => maps.push(Rc::clone(r)),
        (None, None) => {}
    }
    // `nearest !== rootMap` compares object identity; the nearest map for a root-level config is
    // the same object only when it was loaded once, so compare content as well.
    if maps.len() == 2 && *maps[0] == *maps[1] {
        maps.pop();
    }
    for alias_map in &maps {
        for c in apply_aliases(import_path, alias_map, project_root) {
            if let Some(hit) = try_with_ext(&c) {
                return Some(hit);
            }
        }
    }

    // 1.5 Workspace packages and path dependencies: the entry the package's manifest declares
    //     for the import (#2502), else the member's directory (`@scope/ui/widgets` →
    //     `packages/ui/widgets`), where the extension/index permutations find its barrel (#629).
    //     A path into the package's build output the index lacks (`@acme/utils/lib/path`) is read
    //     as the source it is built from, before a file only a local build left on disk.
    let targets = ctx.get_workspace_packages().and_then(|ws| workspace_import_targets(import_path, &ws, from_file));
    if let Some(targets) = targets {
        if let Some(declared) = declared_workspace_entry(&targets, language, ctx) {
            return Some(declared);
        }
        let direct = try_with_ext(&targets.directory);
        if let Some(d) = &direct {
            if ctx.is_indexed_file(d) {
                return direct;
            }
        }
        if let Some(built) = built_source_of(&targets.directory, &targets.package_dir, language, ctx) {
            return Some(built);
        }
        if direct.is_some() {
            return direct;
        }
    }

    // 2. Hard-coded fallback list. Kept for projects that use these conventional aliases without
    //    declaring them in tsconfig.
    let fallback_aliases: [(&str, &str); 6] =
        [("@/", "src/"), ("~/", "src/"), ("@src/", "src/"), ("src/", "src/"), ("@app/", "app/"), ("app/", "app/")];
    for (alias, replacement) in fallback_aliases {
        if import_path.starts_with(alias) {
            // `String.prototype.replace(string, string)`: the first occurrence.
            let replaced = import_path.replacen(alias, replacement, 1);
            if let Some(hit) = try_with_ext(&replaced) {
                return Some(hit);
            }
        }
    }

    // 3. Direct path.
    try_with_ext(import_path)
}

/// The file `base_path` names: with the language's extensions or index files, as is, or as an
/// emitted `.js`.
fn try_with_extensions(base_path: &str, language: Language, ctx: &Ctx) -> Option<String> {
    for ext in extensions_for(language) {
        let candidate = indexed_spelling(&format!("{base_path}{ext}"), language);
        if ctx.file_exists(&candidate) {
            return Some(candidate);
        }
    }
    let as_is = indexed_spelling(base_path, language);
    if ctx.file_exists(&as_is) {
        return Some(as_is);
    }
    find_source_for_emitted_specifier(&as_is, language, ctx)
}

/// The file a package's declared entry for an import lands on (#2502): the first declared
/// target, in order, that the index holds, or that is build output the index lacks, read as the
/// source it is built from. `None` when none holds.
fn declared_workspace_entry(targets: &WorkspaceImportTargets, language: Language, ctx: &Ctx) -> Option<String> {
    targets
        .declared
        .iter()
        .find_map(|target| indexed_target(target, language, ctx).or_else(|| built_source_of(target, &targets.package_dir, language, ctx)))
}

/// `target`, build output the index lacks, as the indexed source it is built from.
fn built_source_of(target: &str, package_dir: &str, language: Language, ctx: &Ctx) -> Option<String> {
    built_sources_of(target, package_dir).iter().find_map(|source| indexed_target(source, language, ctx))
}

/// `target` as a file the index holds: as written, with the language's extensions or index
/// files, or as the source of an emitted `.js`. Only the index counts: build output a developer
/// has built locally is on disk, but nothing links to it.
fn indexed_target(target: &str, language: Language, ctx: &Ctx) -> Option<String> {
    let indexed = |file: &str| ctx.is_indexed_file(file);
    let as_is = indexed_spelling(target, language);
    if indexed(&as_is) {
        return Some(as_is);
    }
    for ext in extensions_for(language) {
        let candidate = indexed_spelling(&format!("{target}{ext}"), language);
        if indexed(&candidate) {
            return Some(candidate);
        }
    }
    find_source_for_emitted_specifier_in(&as_is, language, &indexed)
}

/// The source files a package's build output may be built from, extension-less, most literal
/// first: the build folder mirrored under `src/`, then without a format subfolder. Empty for a
/// target outside a build folder.
fn built_sources_of(target: &str, package_dir: &str) -> Vec<String> {
    let prefix = if package_dir.is_empty() { String::new() } else { format!("{package_dir}/") };
    let Some(in_package) = target.strip_prefix(prefix.as_str()) else { return Vec::new() };
    let Some(root) = re!(r"^(?:dist|build|lib|out|output|es|esm|cjs|umd)/").find(in_package) else { return Vec::new() };
    let mirrored = &in_package[root.end()..];
    let mut rests = vec![mirrored];
    if let Some(format) = re!(r"^(?:esm|cjs|es|mjs|module|modules|commonjs|types|typings|umd|node|browser|src)/").find(mirrored) {
        rests.push(&mirrored[format.end()..]);
    }
    let mut seen = std::collections::HashSet::new();
    rests
        .into_iter()
        .map(|rest| re!(r"(?:\.d)?\.(?:[cm]?[jt]s|[jt]sx)$").replace_first(rest, |_| String::new()))
        .filter(|stem| !stem.is_empty() && !stem.starts_with("src/"))
        .filter(|stem| seen.insert(stem.clone()))
        .map(|stem| format!("{prefix}src/{stem}"))
        .collect()
}

/// A path as the index keys its files (indexedSpelling, #2577). The alias, workspace and
/// direct-path branches join a specifier as written, so `@/widgets/Chart/` made
/// `src/widgets/Chart//index.tsx`; an ES module specifier is read lexically, so `.` and empty
/// segments drop out and `..` folds. A trailing slash still names a folder. Other languages keep
/// the path as found (a C include's `a/../b.h` follows the folder on disk).
fn indexed_spelling(file_path: &str, language: Language) -> String {
    if is_esm_import_language(language) && re!(r"//|(?:^|/)\.\.?(?:/|$)").is_match(file_path) {
        nodepath::posix::normalize(file_path)
    } else {
        file_path.to_string()
    }
}

/// A bare specifier `is_external_import` takes for a package, resolved through the importing app's
/// own tsconfig / jsconfig — the one nearest the file, else the root's — as tsc and a paths-aware
/// bundler resolve it.
///
/// Only an indexed file binds: `file_exists` also answers for a folder and for `node_modules`. A
/// package — one the file's package.json or an enclosing one declares, or a Node built-in —
/// never binds to a project module, only to a declaration file the project keeps for it.
fn resolve_from_own_config(import_path: &str, from_file: &str, language: Language, ctx: &Ctx) -> Option<String> {
    let candidates = own_config_candidates(import_path, from_file, language, ctx)?;
    if ctx.is_declared_outside_package(import_path, from_file) {
        if !extensions_for(language).contains(&".d.ts") {
            return None;
        }
        for candidate in &candidates {
            let declarations = format!("{candidate}.d.ts");
            if ctx.file_exists(&declarations) && ctx.is_indexed_file(&declarations) {
                return Some(declarations);
            }
        }
        return None;
    }
    for candidate in &candidates {
        if let Some(hit) = try_with_extensions(candidate, language, ctx) {
            if ctx.is_indexed_file(&hit) {
                return Some(hit);
            }
        }
    }
    None
}

/// Where the importing app's own config reads a bare specifier from (see
/// `resolve_from_own_config`), or `None` when it reads none. A rooted one is a path: tsc maps it
/// through `paths` and never reads it from `baseUrl`.
fn own_config_candidates(import_path: &str, from_file: &str, language: Language, ctx: &Ctx) -> Option<Vec<String>> {
    if !is_esm_import_language(language)
        || import_path.is_empty()
        || import_path.starts_with("//")
        || re!(r"^[A-Za-z][\w+.-]*:").is_match(import_path)
    {
        return None;
    }
    let config = ctx.get_nearest_aliases(from_file).or_else(|| ctx.get_project_aliases())?;
    let candidates = if import_path.starts_with('/') {
        apply_aliases(import_path, &config, ctx.project_root())
    } else {
        alias_and_base_url_candidates(import_path, &config, ctx.project_root())
    };
    if candidates.is_empty() {
        None
    } else {
        Some(candidates)
    }
}

/// Whether a bare specifier names one of the project's files through the importing app's own
/// config: its `paths` or `baseUrl` map it to a file the index holds, and no package the app
/// declares goes by it.
pub fn names_own_config_module(import_path: &str, from_file: &str, language: Language, ctx: &Ctx) -> bool {
    !ctx.is_declared_outside_package(import_path, from_file) && resolve_import_path(import_path, from_file, language, ctx).is_some()
}

/// Resolve a C/C++ include path by searching include directories. Called as a fallback after
/// relative and aliased resolution fail.
fn resolve_cpp_include_path(import_path: &str, language: Language, ctx: &Ctx) -> Option<String> {
    let include_dirs = ctx.get_cpp_include_dirs();
    let extensions = extensions_for(language);

    for dir in &include_dirs {
        let normalized_dir = dir.replace('\\', "/");
        for ext in extensions {
            let candidate = format!("{normalized_dir}/{import_path}{ext}");
            if ctx.file_exists(&candidate) {
                return Some(candidate);
            }
        }
        // Try as-is (already has extension).
        let candidate = format!("{normalized_dir}/{import_path}");
        if ctx.file_exists(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Resolve a PHP include/require path to a project-relative file path. PHP resolves includes
/// relative to the including file's directory; php.ini `include_path` is not modeled.
pub fn resolve_php_include_path(include_path: &str, from_file: &str, ctx: &Ctx) -> Option<String> {
    let project_root = ctx.project_root();
    let from_dir = nodepath::dirname(&nodepath::join(&[project_root, from_file]));
    let base_path = nodepath::resolve(&[&from_dir, include_path]);
    let relative_path = nodepath::relative(project_root, &base_path).replace('\\', "/");
    if ctx.file_exists(&relative_path) {
        return Some(relative_path);
    }
    // The literal may omit the .php extension (e.g. include "config").
    for ext in extensions_for(Language::Php) {
        let candidate = format!("{relative_path}{ext}");
        if ctx.file_exists(&candidate) {
            return Some(candidate);
        }
    }
    None
}
