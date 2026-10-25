//! Go packages: which directory a name belongs to, what a reference is written through, and which
//! of two build-constrained twins a file sees.
//!
//! A package is a directory. A name is exported by its first letter. A file may dot-import
//! another package, and a `_test` package shares a directory with the one it tests but is a
//! package of its own.

use super::methods::go_written_type;
use super::scope::{go_local_decl, GO_TYPE_KEYWORDS};
use crate::ctx::Ctx;
use crate::name_matcher::exact::prefer_call_site_file;
use crate::name_matcher::util::*;
use crate::types::{ImportMapping, NodeRef, ResolvedRef, Tri, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_types::jscompat::js_trim;
use codegraph_types::nodepath::posix;
use codegraph_types::{Language, Node, NodeKind, ReferenceKind};
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::LazyLock;

/// Standard-library package names (and kubernetes' aliases of a few of its own util packages). A
/// dotted Go reference through one is taken for a call into that package, unless the name is a
/// parameter or local where the call is written.
pub static GO_STDLIB_PACKAGES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "fmt", "os", "io", "net", "http", "log", "math", "sort", "sync", "time", "path", "bytes", "strings", "strconv", "errors", "context", "json",
        "xml", "csv", "html", "template", "regexp", "reflect", "runtime", "testing", "flag", "bufio", "crypto", "encoding", "filepath", "hash",
        "mime", "rand", "signal", "sql", "syscall", "unicode", "unsafe", "atomic", "binary", "debug", "exec", "heap", "ring", "scanner", "tar",
        "zip", "gzip", "zlib", "tls", "url", "user", "pprof", "trace", "ast", "build", "parser", "printer", "token", "types", "cgo", "plugin",
        "race", "ioutil", "utilruntime", "utilwait", "utilnet",
    ]
    .into_iter()
    .collect()
});

/// Names a struct field of a builtin type can carry: none of them is a project type.
pub static GO_BUILTIN_FIELD_TYPES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "string", "bool", "byte", "rune", "error", "any", "int", "int8", "int16", "int32", "int64", "uint", "uint8", "uint16", "uint32", "uint64",
        "uintptr", "float32", "float64", "complex64", "complex128", "chan", "map", "func", "struct", "interface",
    ]
    .into_iter()
    .collect()
});

pub fn go_package_dir(file_path: &str) -> String {
    posix::dirname(&file_path.replace('\\', "/"))
}

/// The directory of the project package the file's import named `qualifier` maps to: undefined
/// without such an import, null when the import is not a project package.
pub fn go_import_package_dir(qualifier: &str, file_path: &str, ctx: &Ctx) -> Tri<String> {
    let imports = ctx.get_import_mappings(file_path, Language::Go);
    let Some(imp) = imports.iter().find(|i| i.local_name == qualifier) else { return Tri::Undefined };
    match ctx.get_go_package_dir(&imp.source, Some(file_path)) {
        Some(dir) => Tri::Value(dir),
        None => Tri::Null,
    }
}

/// The directory of the package a declared (written) type belongs to.
pub fn go_declared_type_package(raw: Option<&str>, file_path: &str, ctx: &Ctx) -> Option<String> {
    let raw = raw.filter(|r| !r.is_empty())?;
    let name = re!(r"[*&\s]", "g").replace_all_str(raw, "");
    let Some(dot) = name.find('.') else {
        return match re!(r"^[A-Za-z_]\w*").find(&name) {
            Some(bare) => go_bare_type_dir(bare.as_str(), file_path, ctx),
            None => Some(go_package_dir(file_path)),
        };
    };
    go_import_package_dir(&name[..dot], file_path, ctx).value()
}

pub fn go_package_types(type_name: &str, pkg_dir: &str, ctx: &Ctx) -> Vec<NodeRef> {
    ctx.get_nodes_by_name(type_name)
        .iter()
        .filter(|n| n.language == Language::Go && crate::name_matcher::is_go_type_kind(n.kind) && go_package_dir(&n.file_path) == pkg_dir)
        .cloned()
        .collect()
}

// -- Package clause ------------------------------------------------------------------------------

memo_map!(PackageClauseMemo, String, Option<String>);

/// The package name a file declares (`package foo`), read past a leading block comment.
pub fn go_package_clause(file_path: &str, ctx: &Ctx) -> Option<String> {
    memoized::<PackageClauseMemo>(ctx, file_path.to_string(), || {
        let lines = lines_of(ctx, file_path);
        let mut comment = false;
        for line in lines.iter().take(500) {
            if comment {
                if line.contains("*/") {
                    comment = false;
                }
                continue;
            }
            if re!(r"^\s*\/\*").is_match(line) && !line.contains("*/") {
                comment = true;
                continue;
            }
            if let Some(m) = re!(r"^\s*package\s+([A-Za-z_]\w*)").captures(line) {
                return m.text(1).map(String::from);
            }
        }
        None
    })
}

// -- Dot imports ---------------------------------------------------------------------------------

pub struct GoDotImports {
    /// The project package directories the file dot-imports.
    pub dirs: Vec<String>,
    /// The file dot-imports a package from outside the project.
    pub outside: bool,
    types: RefCell<Option<HashSet<String>>>,
}

memo_map!(DotImportsMemo, String, Rc<GoDotImports>);

pub fn go_dot_imports(file_path: &str, ctx: &Ctx) -> Rc<GoDotImports> {
    memoized::<DotImportsMemo>(ctx, file_path.to_string(), || {
        let mut found = GoDotImports { dirs: Vec::new(), outside: false, types: RefCell::new(None) };
        for line in lines_of(ctx, file_path).iter() {
            // Imports come first.
            if re!(r"^(?:func|type|var|const)\b").is_match(line) {
                break;
            }
            let Some(dot) = re!(r#"^\s*(?:import\s+)?\.\s+"([^"]+)""#).captures(line) else { continue };
            let source = dot.text(1).unwrap_or("");
            match ctx.get_go_package_dir(source, Some(file_path)) {
                Some(dir) if !dir.is_empty() => found.dirs.push(dir),
                _ => {
                    if !source.starts_with('.') && !source.contains("/internal/") {
                        found.outside = true;
                    }
                }
            }
        }
        Rc::new(found)
    })
}

/// The directory a bare type name is declared in: the file's own package, or a package it
/// dot-imports. None when the name can only be an outside package's.
pub fn go_bare_type_dir(name: &str, file_path: &str, ctx: &Ctx) -> Option<String> {
    let own = go_package_dir(file_path);
    let dots = go_dot_imports(file_path, ctx);
    if (dots.dirs.is_empty() && !dots.outside) || !go_package_types(name, &own, ctx).is_empty() {
        return Some(own);
    }
    if let Some(dir) = dots.dirs.iter().find(|d| !go_package_types(name, d, ctx).is_empty()) {
        return Some(dir.clone());
    }
    if !dots.outside || !re!(r"^[A-Z]\w*$").is_match(name) {
        return Some(own);
    }
    if dots.types.borrow().is_none() {
        let mut types = HashSet::new();
        for line in lines_of(ctx, file_path).iter() {
            let stripped = re!(r"\/\/.*$").replace_first(line, |_| String::new());
            for m in re!(r"(?:^|[^\w.])type\s+([A-Za-z_]\w*)", "g").captures_iter(&stripped) {
                if let Some(t) = m.text(1) {
                    types.insert(t.to_string());
                }
            }
        }
        *dots.types.borrow_mut() = Some(types);
    }
    if dots.types.borrow().as_ref().is_some_and(|t| t.contains(name)) {
        Some(own)
    } else {
        None
    }
}

/// A supertype named bare in a file that dot-imports its package: the target is that package's.
pub fn go_dot_imported_supertype(result: ResolvedRef, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let name = r.reference_name.as_str();
    if !re!(r"^[A-Za-z_]\w*$").is_match(name) {
        return Some(result);
    }
    let dir = go_bare_type_dir(name, &r.file_path, ctx);
    if dir.as_deref() == Some(go_package_dir(&r.file_path).as_str()) || go_ref_qualification(r, ctx).written.is_some() {
        return Some(result);
    }
    let dir = dir?;
    if ctx.node_by_id(&result.target_node_id).is_some_and(|t| go_package_dir(&t.file_path) == dir) {
        return Some(result);
    }
    let types = prefer_call_site_file(go_package_types(name, &dir, ctx), &r.file_path);
    let mut moved = result;
    moved.target_node_id = types.first()?.id.clone();
    Some(moved)
}

// -- What a reference is written through ---------------------------------------------------------

/// The package a Go reference is written through, read from its line (the index keeps only the
/// name): `io` in `io.Closer`, and the file's import of it. Empty for a name written bare.
#[derive(Clone, Default)]
pub struct GoQualification {
    /// The package name written before the reference's name, as spelled.
    pub written: Option<String>,
    /// The file's import that name is.
    pub imported: Option<ImportMapping>,
    /// For a call through a parameter or local that takes an import's name, the import the type
    /// its declaration writes is written through — `clock` in a parameter `clock clock.PassiveClock`,
    /// or its value comes from: `reflect` in `value := reflect.New(t)`.
    pub type_package: Option<ImportMapping>,
}

memo_map!(QualifierMemo, String, GoQualification);

pub fn go_ref_qualification(r: &UnresolvedRef, ctx: &Ctx) -> GoQualification {
    if r.reference_kind == ReferenceKind::Edge(codegraph_types::EdgeKind::Imports) {
        return GoQualification::default();
    }
    let name = r.reference_name.rsplit('.').next().unwrap_or("").to_string();
    if !re!(r"^[A-Za-z_]\w*$").is_match(&name) {
        return GoQualification::default();
    }
    let key = format!("{}\0{}\0{}\0{}", r.file_path, r.line, r.column, r.reference_name);
    memoized::<QualifierMemo>(ctx, key, || {
        let dot = r.reference_name.find('.');
        let mut written: Option<String> = None;
        if let Some(dot) = dot {
            // `pkg.Name` written whole: the head is the qualifier.
            let head = &r.reference_name[..dot];
            if re!(r"^[A-Za-z_]\w*$").is_match(head) {
                written = Some(head.to_string());
            }
        } else {
            // A bare name: read the line for the package written before it.
            let line = line_at(ctx, &r.file_path, r.line).unwrap_or_default();
            if starts_with_at(&line, &name, r.column) {
                written = re!(r"(?:^|[^\w.]|\.{3})([A-Za-z_]\w*)\.$").captures(before_col(&line, r.column)).and_then(|c| c.text(1).map(String::from));
            } else if !Re::cached(&format!(r"(?<![\w.]){name}\b"), "").is_match(&line) {
                written = Re::cached(&format!(r"(?:^|[^\w.]|\.{{3}})([A-Za-z_]\w*)\.{name}\b"), "").captures(&line).and_then(|c| c.text(1).map(String::from));
            }
        }
        let imports = ctx.get_import_mappings(&r.file_path, Language::Go);
        let mut imported = written.as_ref().and_then(|w| imports.iter().find(|m| &m.local_name == w).cloned());
        let mut type_package = None;
        // A parameter or local that takes an import's name is not the package.
        let local = if imported.is_some() && dot.is_some() && is_calls(r) { written.as_deref().and_then(|w| go_local_decl(w, r, ctx)) } else { None };
        if let Some(local) = local {
            let qualifier = re!(r"^[*&\s]*([A-Za-z_]\w*)\.[A-Za-z_]\w*$")
                .captures(local.ty.as_deref().unwrap_or(""))
                .and_then(|c| c.text(1).map(String::from))
                .or_else(|| local.from.clone());
            type_package = qualifier.and_then(|q| imports.iter().find(|m| m.local_name == q).cloned());
            written = None;
            imported = None;
        }
        GoQualification { written, imported, type_package }
    })
}

/// The import that qualifies a Go reference (`pkg.Name`).
pub fn go_ref_qualifier(r: &UnresolvedRef, ctx: &Ctx) -> Option<ImportMapping> {
    go_ref_qualification(r, ctx).imported
}

/// A name written through a package this file does not import.
pub fn is_go_unknown_qualified(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let q = go_ref_qualification(r, ctx);
    q.written.is_some() && q.imported.is_none()
}

/// Where a type-position reference resolved, re-aimed at the package the reference is written
/// through (or declared in): a bare `Foo` in a dot-importing file is the dot-imported package's,
/// a `pkg.Foo` is `pkg`'s, never a same-named symbol of another package.
pub fn go_type_position_target(result: ResolvedRef, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let Some(target) = ctx.node_by_id(&result.target_node_id) else { return Some(result) };
    if target.language != Language::Go {
        return Some(result);
    }
    let is_type = super::super::is_go_type_kind(target.kind);
    let dot = r.reference_name.rfind('.');
    let name = &r.reference_name[dot.map_or(0, |d| d + 1)..];
    let mut pkg_dir: Option<String> = None;
    let mut bare = false;
    if let Some(dot) = dot {
        pkg_dir = go_import_package_dir(&r.reference_name[..dot], &r.file_path, ctx).value();
    } else {
        let q = go_ref_qualification(r, ctx);
        bare = q.written.is_none();
        if bare {
            pkg_dir = Some(go_bare_type_dir(name, &r.file_path, ctx)?);
        } else if let Some(imported) = q.imported {
            pkg_dir = ctx.get_go_package_dir(&imported.source, Some(&r.file_path));
        }
    }
    if is_type && (!bare || Some(go_package_dir(&target.file_path)) == pkg_dir) {
        return Some(result);
    }
    let types = match &pkg_dir {
        Some(dir) => go_package_types(name, dir, ctx),
        None => Vec::new(),
    };
    if !types.is_empty() {
        let mut moved = result;
        moved.target_node_id = prefer_call_site_file(types, &r.file_path)[0].id.clone();
        return Some(moved);
    }
    if is_type {
        Some(result)
    } else {
        None
    }
}

/// An import from outside the project (and not a relative or `internal` one).
fn is_go_outside_import(imp: &ImportMapping, file_path: &str, ctx: &Ctx) -> bool {
    !(imp.source.starts_with('.') || imp.source.contains("/internal/") || ctx.get_go_package_dir(&imp.source, Some(file_path)).is_some())
}

/// Written through a package from outside the project: nothing in the project is that symbol.
pub fn is_go_external_qualified(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let q = go_ref_qualification(r, ctx);
    q.imported.or(q.type_package).is_some_and(|pkg| is_go_outside_import(&pkg, &r.file_path, ctx))
}

/// A written type qualified by a package from outside the project (`*pkg.Type`).
fn is_go_outside_type(written: &str, file_path: &str, ctx: &Ctx) -> bool {
    re!(r"^[*&\s]*([A-Za-z_]\w*)\s*\.").captures(written).and_then(|c| c.text(1).map(String::from)).is_some_and(|q| is_go_outside_package(&q, file_path, ctx))
}

/// The package named `name` in this file is one from outside the project.
pub fn is_go_outside_package(name: &str, file_path: &str, ctx: &Ctx) -> bool {
    ctx.get_import_mappings(file_path, Language::Go).iter().find(|m| m.local_name == name).is_some_and(|imp| is_go_outside_import(imp, file_path, ctx))
}

/// A declared type none of whose methods are the project's: outside, or declared nowhere.
pub fn go_has_no_project_methods(named: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if is_go_outside_type(named, &r.file_path, ctx) {
        return true;
    }
    if let Some(dot) = named.find('.') {
        if go_import_package_dir(&named[..dot], &r.file_path, ctx).value().is_none() {
            return false;
        }
    }
    matches!(go_written_type(named, r, ctx), Tri::Null)
}

/// `*pkg.Type[T]` → `pkg.Type`, `Type` → `Type`; none for a type keyword or a literal.
pub fn go_named_type(written: &str) -> Option<String> {
    let m = re!(r"^\*?\s*([A-Za-z_]\w*)(?:\s*\.\s*([A-Za-z_]\w*))?\s*(?:\[.*\])?$").captures(js_trim(written))?;
    let first = m.text(1)?;
    if GO_TYPE_KEYWORDS.contains(first) {
        return None;
    }
    Some(match m.text(2).filter(|s| !s.is_empty()) {
        Some(second) => format!("{first}.{second}"),
        None => first.to_string(),
    })
}

/// Whether `candidate` lives in the project package a Go reference is written through, when it
/// is one: `store.Manager` names the `Manager` of the directory the `store` import maps to,
/// never a same-named symbol of another package (#2322).
pub fn is_in_go_qualifier_package(candidate: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    // A package never holds a method: a call or method value landing on one is made through a
    // variable that shadows the import.
    if candidate.kind == NodeKind::Method && (is_calls(r) || r.reference_kind == ReferenceKind::FunctionRef) {
        return true;
    }
    let Some(imported) = go_ref_qualifier(r, ctx) else { return true };
    match ctx.get_go_package_dir(&imported.source, Some(&r.file_path)) {
        None => true,
        Some(dir) => go_package_dir(&candidate.file_path) == dir,
    }
}

/// Whether Go lets `r`'s file name the Go declaration `candidate`: an exported name from
/// anywhere, an unexported one only inside its own package, which is a directory. By the name's
/// first letter, not the extractor's flag: the flag is unset for every Go method.
///
/// A package's external tests (`package a_test` beside `package a`) share its directory but are a
/// package of their own: a name one of them declares at the top level reaches the other only
/// through an import, written `a.Real`, or bare after `import . "…/a"`, and then only an exported
/// one (#2581).
pub fn is_go_name_visible(candidate: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let dir = go_package_dir(&candidate.file_path);
    if dir != go_package_dir(&r.file_path) {
        return candidate.name.starts_with(|c: char| c.is_ascii_uppercase());
    }
    if candidate.file_path == r.file_path || is_type_member_kind(candidate.kind) || r.reference_name.contains('.') {
        return true;
    }
    let (own, theirs) = (go_package_clause(&r.file_path, ctx), go_package_clause(&candidate.file_path, ctx));
    let (Some(own), Some(theirs)) = (own, theirs) else { return true };
    if own == theirs {
        return true;
    }
    candidate.name.starts_with(|c: char| c.is_ascii_uppercase()) && go_dot_imports(&r.file_path, ctx).dirs.contains(&dir)
}

// -- Build constraints ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GoBuildTarget {
    pub goos: &'static str,
    pub goarch: &'static str,
}

const GO_KNOWN_OS: [&str; 18] = [
    "aix", "android", "darwin", "dragonfly", "freebsd", "hurd", "illumos", "ios", "js", "linux", "nacl", "netbsd", "openbsd", "plan9", "solaris",
    "wasip1", "windows", "zos",
];
const GO_KNOWN_ARCH: [&str; 24] = [
    "386", "amd64", "amd64p32", "arm", "armbe", "arm64", "arm64be", "loong64", "mips", "mipsle", "mips64", "mips64le", "mips64p32", "mips64p32le", "ppc",
    "ppc64", "ppc64le", "riscv", "riscv64", "s390", "s390x", "sparc", "sparc64", "wasm",
];
const GO_UNIX_OS: [&str; 12] = ["aix", "android", "darwin", "dragonfly", "freebsd", "hurd", "illumos", "ios", "linux", "netbsd", "openbsd", "solaris"];

/// The platforms a build constraint is tried against: the common ones first, then every known
/// OS on its usual architecture, then every architecture on Linux.
static GO_BUILD_TARGETS: LazyLock<Vec<GoBuildTarget>> = LazyLock::new(|| {
    let usual = |os: &str| match os {
        "js" | "wasip1" => "wasm",
        "ios" | "android" => "arm64",
        "aix" => "ppc64",
        "zos" => "s390x",
        "hurd" => "386",
        _ => "amd64",
    };
    let mut targets: Vec<GoBuildTarget> = vec![
        GoBuildTarget { goos: "linux", goarch: "amd64" },
        GoBuildTarget { goos: "darwin", goarch: "arm64" },
        GoBuildTarget { goos: "windows", goarch: "amd64" },
        GoBuildTarget { goos: "linux", goarch: "arm64" },
        GoBuildTarget { goos: "darwin", goarch: "amd64" },
        GoBuildTarget { goos: "freebsd", goarch: "amd64" },
    ];
    let mut add = |goos: &'static str, goarch: &'static str| {
        if !targets.iter().any(|t| t.goos == goos && t.goarch == goarch) {
            targets.push(GoBuildTarget { goos, goarch });
        }
    };
    for goos in GO_KNOWN_OS {
        add(goos, usual(goos));
    }
    for goarch in GO_KNOWN_ARCH {
        add("linux", goarch);
    }
    targets
});

fn go_build_tag_holds(tag: &str, t: &GoBuildTarget) -> bool {
    if tag == t.goos || tag == t.goarch {
        return true;
    }
    match tag {
        "unix" => GO_UNIX_OS.contains(&t.goos),
        "linux" => t.goos == "android",
        "darwin" => t.goos == "ios",
        "solaris" => t.goos == "illumos",
        "gc" => true,
        "cgo" => t.goos != "js" && t.goos != "wasip1",
        _ => re!(r"^go1\.\d+$").is_match(tag),
    }
}

/// A file's build constraint: its file-name suffix and its `//go:build` / `// +build` header.
#[derive(Debug)]
pub enum GoBuildConstraint {
    Tag(String),
    Not(Box<GoBuildConstraint>),
    And(Box<GoBuildConstraint>, Box<GoBuildConstraint>),
    Or(Box<GoBuildConstraint>, Box<GoBuildConstraint>),
    /// `// +build a,!b c`: any option (space-separated) whose every term (comma-separated) holds.
    PlusBuild(Vec<Vec<(bool, String)>>),
    /// A `_linux_amd64.go` suffix.
    Both(String, String),
    All(Vec<GoBuildConstraint>),
}

impl GoBuildConstraint {
    fn holds(&self, t: &GoBuildTarget) -> bool {
        match self {
            GoBuildConstraint::Tag(tag) => go_build_tag_holds(tag, t),
            GoBuildConstraint::Not(inner) => !inner.holds(t),
            GoBuildConstraint::And(a, b) => a.holds(t) && b.holds(t),
            GoBuildConstraint::Or(a, b) => a.holds(t) || b.holds(t),
            GoBuildConstraint::PlusBuild(options) => options.iter().any(|terms| terms.iter().all(|(neg, tag)| go_build_tag_holds(tag, t) != *neg)),
            GoBuildConstraint::Both(a, b) => go_build_tag_holds(a, t) && go_build_tag_holds(b, t),
            GoBuildConstraint::All(parts) => parts.iter().all(|c| c.holds(t)),
        }
    }
}

struct BuildExprParser {
    tokens: Vec<String>,
    i: usize,
}

impl BuildExprParser {
    fn or(&mut self) -> Option<GoBuildConstraint> {
        let mut left = self.and();
        while left.is_some() && self.tokens.get(self.i).map(String::as_str) == Some("||") {
            self.i += 1;
            let b = self.and();
            left = match (left, b) {
                (Some(a), Some(b)) => Some(GoBuildConstraint::Or(Box::new(a), Box::new(b))),
                _ => None,
            };
        }
        left
    }

    fn and(&mut self) -> Option<GoBuildConstraint> {
        let mut left = self.not();
        while left.is_some() && self.tokens.get(self.i).map(String::as_str) == Some("&&") {
            self.i += 1;
            let b = self.not();
            left = match (left, b) {
                (Some(a), Some(b)) => Some(GoBuildConstraint::And(Box::new(a), Box::new(b))),
                _ => None,
            };
        }
        left
    }

    fn not(&mut self) -> Option<GoBuildConstraint> {
        let tok = self.tokens.get(self.i).cloned();
        self.i += 1;
        match tok.as_deref() {
            Some("!") => self.not().map(|inner| GoBuildConstraint::Not(Box::new(inner))),
            Some("(") => {
                let inner = self.or();
                let close = self.tokens.get(self.i).map(String::as_str) == Some(")");
                self.i += 1;
                if close {
                    inner
                } else {
                    None
                }
            }
            Some(tag) if re!(r"^[A-Za-z0-9_.]+$").is_match(tag) => Some(GoBuildConstraint::Tag(tag.to_string())),
            _ => None,
        }
    }
}

fn go_parse_build_expr(text: &str) -> Option<GoBuildConstraint> {
    let tokens: Vec<String> = re!(r"&&|\|\||[()!]|[A-Za-z0-9_.]+|\S", "g").captures_iter(text).filter_map(|m| m.get(0).map(|m| m.as_str().to_string())).collect();
    let mut parser = BuildExprParser { tokens, i: 0 };
    let expr = parser.or();
    if parser.i == parser.tokens.len() {
        expr
    } else {
        None
    }
}

fn go_parse_plus_build(text: &str) -> Option<GoBuildConstraint> {
    let mut options: Vec<Vec<(bool, String)>> = Vec::new();
    for option in re!(r"\s+").split(js_trim(text)).into_iter().filter(|o| !o.is_empty()) {
        let mut terms = Vec::new();
        for term in option.split(',') {
            let neg = term.starts_with('!');
            let tag = if neg { &term[1..] } else { term };
            if !re!(r"^[A-Za-z0-9_.]+$").is_match(tag) {
                return None;
            }
            terms.push((neg, tag.to_string()));
        }
        options.push(terms);
    }
    if options.is_empty() {
        None
    } else {
        Some(GoBuildConstraint::PlusBuild(options))
    }
}

memo_map!(BuildConstraintMemo, String, Option<Rc<GoBuildConstraint>>);
memo_map!(BuildTwinsMemo, String, Rc<Vec<NodeRef>>);

fn go_file_constraint(file_path: &str, ctx: &Ctx) -> Option<Rc<GoBuildConstraint>> {
    memoized::<BuildConstraintMemo>(ctx, file_path.to_string(), || {
        let mut parts: Vec<GoBuildConstraint> = Vec::new();
        // `_linux.go`, `_amd64.go`, `_linux_amd64.go`, each before an optional `_test`.
        let base = posix::basename(&file_path.replace('\\', "/"));
        let base = base.split('.').next().unwrap_or("").to_string();
        let mut suffix: Vec<&str> = match base.find('_') {
            Some(u) => base[u + 1..].split('_').collect(),
            None => Vec::new(),
        };
        if suffix.last() == Some(&"test") {
            suffix.pop();
        }
        let last = suffix.last().copied();
        let prev = if suffix.len() >= 2 { Some(suffix[suffix.len() - 2]) } else { None };
        match (prev, last) {
            (Some(p), Some(l)) if GO_KNOWN_OS.contains(&p) && GO_KNOWN_ARCH.contains(&l) => parts.push(GoBuildConstraint::Both(p.to_string(), l.to_string())),
            (_, Some(l)) if GO_KNOWN_OS.contains(&l) || GO_KNOWN_ARCH.contains(&l) => parts.push(GoBuildConstraint::Tag(l.to_string())),
            _ => {}
        }
        // The header comments before the package clause.
        let mut go_build: Option<String> = None;
        let mut plus_build: Vec<String> = Vec::new();
        for line in lines_of(ctx, file_path).iter().take(200) {
            let text = js_trim(line.strip_prefix('\u{feff}').unwrap_or(line));
            if text.is_empty() {
                continue;
            }
            if !text.starts_with("//") {
                break;
            }
            if let Some(go) = re!(r"^\/\/go:build\s+(.*)$").captures(text) {
                if go_build.is_none() {
                    go_build = go.text(1).map(String::from);
                }
            }
            if let Some(plus) = re!(r"^\/\/\s*\+build\s+(.*)$").captures(text) {
                plus_build.extend(plus.text(1).map(String::from));
            }
        }
        let header: Vec<Option<GoBuildConstraint>> = match &go_build {
            Some(expr) => vec![go_parse_build_expr(expr)],
            None => plus_build.iter().map(|p| go_parse_plus_build(p)).collect(),
        };
        let mut readable = true;
        for c in header {
            match c {
                Some(c) => parts.push(c),
                None => readable = false,
            }
        }
        if !readable || parts.is_empty() {
            None
        } else {
            Some(Rc::new(GoBuildConstraint::All(parts)))
        }
    })
}

/// A function declared once per platform in files that differ only by build constraint:
/// the twin the reference's own file builds with (`sys_linux.go` for a `linux` caller).
pub fn prefer_go_build_twin(result: Option<ResolvedRef>, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let result = result?;
    if r.language != Language::Go {
        return Some(result);
    }
    let Some(target) = ctx.node_by_id(&result.target_node_id) else { return Some(result) };
    if target.language != Language::Go || go_file_constraint(&target.file_path, ctx).is_none() {
        return Some(result);
    }
    let twins = memoized::<BuildTwinsMemo>(ctx, target.id.clone(), || {
        let dir = go_package_dir(&target.file_path);
        let clause = go_package_clause(&target.file_path, ctx);
        Rc::new(
            ctx.get_nodes_by_name(&target.name)
                .iter()
                .filter(|n| {
                    n.language == Language::Go
                        && n.kind == target.kind
                        && n.qualified_name == target.qualified_name
                        && go_package_dir(&n.file_path) == dir
                        && go_package_clause(&n.file_path, ctx) == clause
                })
                .cloned()
                .collect(),
        )
    });
    if twins.len() < 2 {
        return Some(result);
    }
    let own = go_file_constraint(&r.file_path, ctx);
    for t in GO_BUILD_TARGETS.iter() {
        if own.as_ref().is_some_and(|c| !c.holds(t)) {
            continue;
        }
        let built: Vec<&NodeRef> = twins.iter().filter(|n| go_file_constraint(&n.file_path, ctx).is_none_or(|c| c.holds(t))).collect();
        if built.len() == 1 {
            if built[0].id == target.id {
                return Some(result);
            }
            let mut moved = result;
            moved.target_node_id = built[0].id.clone();
            return Some(moved);
        }
        if built.len() > 1 {
            return Some(result);
        }
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holds(expr: &str, os: &'static str, arch: &'static str) -> bool {
        go_parse_build_expr(expr).expect("parses").holds(&GoBuildTarget { goos: os, goarch: arch })
    }

    #[test]
    fn build_expressions_are_evaluated_per_target() {
        assert!(holds("linux && amd64", "linux", "amd64"));
        assert!(!holds("linux && !amd64", "linux", "amd64"));
        assert!(holds("(darwin || freebsd) && cgo", "freebsd", "amd64"));
        assert!(holds("unix", "darwin", "arm64"));
        assert!(!holds("unix", "windows", "amd64"));
        assert!(go_parse_build_expr("linux &&").is_none());
    }

    #[test]
    fn plus_build_lines_are_or_of_ands() {
        let c = go_parse_plus_build("linux,amd64 darwin").unwrap();
        assert!(c.holds(&GoBuildTarget { goos: "darwin", goarch: "arm64" }));
        assert!(c.holds(&GoBuildTarget { goos: "linux", goarch: "amd64" }));
        assert!(!c.holds(&GoBuildTarget { goos: "linux", goarch: "arm64" }));
    }

    #[test]
    fn a_type_name_is_read_from_its_written_form() {
        assert_eq!(go_named_type("*pkg.Type[T]").as_deref(), Some("pkg.Type"));
        assert_eq!(go_named_type("Type").as_deref(), Some("Type"));
        assert_eq!(go_named_type("map[string]int"), None);
        assert_eq!(go_named_type("func()"), None);
    }
}
