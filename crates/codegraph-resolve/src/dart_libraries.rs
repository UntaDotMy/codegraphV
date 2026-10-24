//! Dart libraries — port of `src/resolution/dart-libraries.ts`.
//!
//! What a Dart file sees: its own library (the file plus the `part`s it lists), the libraries it
//! imports through `show`/`hide` filters and prefixes, and what those re-export.

use crate::ctx::Ctx;
use codegraph_jsre::re;
use codegraph_types::jscompat::js_trim;
use codegraph_types::nodepath::posix;
use indexmap::{IndexMap, IndexSet};
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

#[derive(Debug, PartialEq)]
pub struct NameFilter {
    pub show: Option<BTreeSet<String>>,
    pub hide: Option<BTreeSet<String>>,
    pub key: String,
}

type Filter = Rc<NameFilter>;

fn every_name() -> Filter {
    Rc::new(NameFilter { show: None, hide: None, key: "*|".to_string() })
}

fn is_every(f: &NameFilter) -> bool {
    f.show.is_none() && f.hide.is_none()
}

fn make_filter(show: Option<BTreeSet<String>>, hide: Option<BTreeSet<String>>) -> Filter {
    let hidden = hide.filter(|h| !h.is_empty());
    if show.is_none() && hidden.is_none() {
        return every_name();
    }
    let key = format!(
        "{}|{}",
        show.as_ref().map_or("*".to_string(), |s| s.iter().cloned().collect::<Vec<_>>().join(",")),
        hidden.as_ref().map_or(String::new(), |s| s.iter().cloned().collect::<Vec<_>>().join(","))
    );
    Rc::new(NameFilter { show, hide: hidden, key })
}

fn compose_filters(a: &Filter, b: &Filter) -> Filter {
    if is_every(a) {
        return Rc::clone(b);
    }
    if is_every(b) {
        return Rc::clone(a);
    }
    let show = match (&a.show, &b.show) {
        (Some(x), Some(y)) => Some(x.intersection(y).cloned().collect::<BTreeSet<_>>()),
        (x, y) => x.clone().or_else(|| y.clone()),
    };
    let hide = match (&a.hide, &b.hide) {
        (Some(x), Some(y)) => Some(x.union(y).cloned().collect::<BTreeSet<_>>()),
        (x, y) => x.clone().or_else(|| y.clone()),
    };
    make_filter(show, hide)
}

fn lets(filter: &NameFilter, name: &str) -> bool {
    filter.show.as_ref().is_none_or(|s| s.contains(name)) && filter.hide.as_ref().is_none_or(|h| !h.contains(name))
}

#[derive(Debug, Clone)]
pub struct DartNamespaceDirective {
    pub uris: Vec<String>,
    pub prefix: Option<String>,
    pub filter: Filter,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PartOf {
    Uri(String),
    Name(String),
}

#[derive(Debug, Clone, Default)]
pub struct DartDirectives {
    pub library_name: Option<String>,
    pub part_of: Option<PartOf>,
    pub parts: Vec<String>,
    pub imports: Vec<DartNamespaceDirective>,
    pub exports: Vec<DartNamespaceDirective>,
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'$'
}

fn is_ident_part(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

/// A sticky `/[A-Za-z_$][\w$]*/y` at `i`.
fn identifier(src: &[u8], i: usize) -> Option<&str> {
    if i >= src.len() || !is_ident_start(src[i]) {
        return None;
    }
    let mut j = i + 1;
    while j < src.len() && is_ident_part(src[j]) {
        j += 1;
    }
    std::str::from_utf8(&src[i..j]).ok()
}

fn at(src: &[u8], i: usize) -> u8 {
    src.get(i).copied().unwrap_or(0)
}

fn skip_trivia(src: &[u8], mut i: usize) -> usize {
    let n = src.len();
    while i < n {
        let c = src[i];
        if c == 32 || (9..=13).contains(&c) {
            i += 1;
        } else if src[i..].starts_with(&[0xEF, 0xBB, 0xBF]) {
            i += 3;
        } else if c == b'/' && at(src, i + 1) == b'/' {
            i = match src[i + 2.min(n - i)..].iter().position(|&b| b == b'\n') {
                Some(p) => i + 2 + p + 1,
                None => n,
            };
        } else if c == b'/' && at(src, i + 1) == b'*' {
            let mut depth = 1;
            i += 2;
            while i < n && depth > 0 {
                if src[i] == b'/' && at(src, i + 1) == b'*' {
                    depth += 1;
                    i += 2;
                } else if src[i] == b'*' && at(src, i + 1) == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else {
            break;
        }
    }
    i
}

/// A string literal at `i`: its value and the index after it.
fn read_string(src: &[u8], mut i: usize) -> Option<(String, usize)> {
    let mut raw = false;
    if at(src, i) == b'r' && (at(src, i + 1) == b'\'' || at(src, i + 1) == b'"') {
        raw = true;
        i += 1;
    }
    let quote = at(src, i);
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    let triple = [quote; 3];
    let close: &[u8] = if src[i..].starts_with(&triple) { &triple } else { &triple[..1] };
    let mut j = i + close.len();
    let mut value: Vec<u8> = Vec::new();
    while j < src.len() {
        if src[j..].starts_with(close) {
            return Some((String::from_utf8_lossy(&value).into_owned(), j + close.len()));
        }
        let c = src[j];
        if c == b'\n' && close.len() == 1 {
            return None;
        }
        if c == b'\\' && !raw {
            if j + 1 < src.len() {
                value.push(src[j + 1]);
            }
            j += 2;
        } else if c == b'$' && !raw && at(src, j + 1) == b'{' {
            let mut depth = 1;
            j += 2;
            while j < src.len() && depth > 0 {
                if let Some((_, end)) = read_string(src, j) {
                    j = end;
                    continue;
                }
                if src[j] == b'{' {
                    depth += 1;
                } else if src[j] == b'}' {
                    depth -= 1;
                }
                j += 1;
            }
        } else {
            value.push(c);
            j += 1;
        }
    }
    None
}

/// The index after the balanced `open`…`close` group starting at `i`, or `None`.
fn skip_group(src: &[u8], mut i: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth = 0i32;
    while i < src.len() {
        i = skip_trivia(src, i);
        if let Some((_, end)) = read_string(src, i) {
            i = end;
            continue;
        }
        let c = at(src, i);
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return Some(i + 1);
            }
        }
        i += 1;
    }
    None
}

fn skip_annotation(src: &[u8], mut i: usize) -> usize {
    i += 1;
    loop {
        i = skip_trivia(src, i);
        let Some(name) = identifier(src, i) else { return i };
        i += name.len();
        let next = skip_trivia(src, i);
        if at(src, next) != b'.' {
            break;
        }
        i = next + 1;
    }
    i = skip_trivia(src, i);
    if at(src, i) == b'<' {
        let Some(end) = skip_group(src, i, b'<', b'>') else { return src.len() };
        i = skip_trivia(src, end);
    }
    if at(src, i) == b'(' {
        return skip_group(src, i, b'(', b')').unwrap_or(src.len());
    }
    i
}

#[derive(Debug, Clone)]
enum Token {
    Str(String),
    Word(String),
    Condition,
    Punct(u8),
}

fn read_directive_tokens(src: &[u8], mut i: usize) -> Option<(Vec<Token>, usize)> {
    let mut tokens: Vec<Token> = Vec::new();
    loop {
        i = skip_trivia(src, i);
        if i >= src.len() || tokens.len() > 2000 {
            return None;
        }
        let c = src[i];
        if c == b';' {
            return Some((tokens, i + 1));
        }
        if c == b'{' || c == b'}' || (c == b'=' && at(src, i + 1) == b'>') {
            return None;
        }
        if let Some((value, end)) = read_string(src, i) {
            tokens.push(Token::Str(value));
            i = end;
            continue;
        }
        if c == b'(' {
            i = skip_group(src, i, b'(', b')')?;
            tokens.push(Token::Condition);
            continue;
        }
        if let Some(word) = identifier(src, i) {
            i += word.len();
            tokens.push(Token::Word(word.to_string()));
            continue;
        }
        tokens.push(Token::Punct(c));
        i += 1;
    }
}

fn dotted_name(tokens: &[Token]) -> Option<String> {
    let mut name = String::new();
    for t in tokens {
        match t {
            Token::Word(w) => name.push_str(w),
            Token::Punct(b'.') => name.push('.'),
            _ => return None,
        }
    }
    Some(name)
}

fn add_directive(out: &mut DartDirectives, keyword: &str, tokens: &[Token]) -> bool {
    let first = tokens.first();
    if keyword == "library" {
        let Some(name) = dotted_name(tokens) else { return false };
        out.library_name = Some(name);
        return true;
    }
    if keyword == "part" {
        if let Some(Token::Word(w)) = first {
            if w == "of" {
                match tokens.get(1) {
                    Some(Token::Str(uri)) => out.part_of = Some(PartOf::Uri(uri.clone())),
                    _ => match dotted_name(&tokens[1.min(tokens.len())..]).filter(|n| !n.is_empty()) {
                        Some(name) => out.part_of = Some(PartOf::Name(name)),
                        None => return false,
                    },
                }
                return true;
            }
        }
        let Some(Token::Str(uri)) = first else { return false };
        out.parts.push(uri.clone());
        return true;
    }
    let Some(Token::Str(first_uri)) = first else { return false };
    let mut uris = vec![first_uri.clone()];
    let mut prefix: Option<String> = None;
    let mut filter = every_name();
    let mut k = 1usize;
    while let Some(Token::Str(more)) = tokens.get(k) {
        uris[0].push_str(more);
        k += 1;
    }
    while k < tokens.len() {
        let t = &tokens[k];
        let next = tokens.get(k + 1);
        let uri = tokens.get(k + 2);
        match (t, next, uri) {
            (Token::Word(w), Some(Token::Condition), Some(Token::Str(u))) if w == "if" => {
                uris.push(u.clone());
                k += 3;
            }
            (Token::Word(w), Some(Token::Word(p)), _) if w == "as" => {
                prefix = Some(p.clone());
                k += 2;
            }
            (Token::Word(w), _, _) if w == "show" || w == "hide" => {
                let mut names = BTreeSet::new();
                k += 1;
                while let Some(Token::Word(name)) = tokens.get(k) {
                    names.insert(name.clone());
                    match tokens.get(k + 1) {
                        Some(Token::Punct(b',')) => k += 2,
                        _ => {
                            k += 1;
                            break;
                        }
                    }
                }
                let part = if w == "show" { make_filter(Some(names), None) } else { make_filter(None, Some(names)) };
                filter = compose_filters(&filter, &part);
            }
            _ => k += 1,
        }
    }
    let directive = DartNamespaceDirective { uris, prefix, filter };
    if keyword == "import" {
        out.imports.push(directive);
    } else {
        out.exports.push(directive);
    }
    true
}

/// The directives at the head of a Dart source file.
pub fn read_dart_directives(source: &str) -> DartDirectives {
    let src = source.as_bytes();
    let mut out = DartDirectives::default();
    let mut i = if src.starts_with(&[0xEF, 0xBB, 0xBF]) { 3 } else { 0 };
    if src[i..].starts_with(b"#!") {
        i = match src[i..].iter().position(|&b| b == b'\n') {
            Some(p) => i + p + 1,
            None => src.len(),
        };
    }
    loop {
        i = skip_trivia(src, i);
        if i >= src.len() {
            break;
        }
        if src[i] == b'@' {
            i = skip_annotation(src, i);
            continue;
        }
        let Some(keyword) = identifier(src, i) else { break };
        if !matches!(keyword, "library" | "import" | "export" | "part") {
            break;
        }
        let Some((tokens, end)) = read_directive_tokens(src, i + keyword.len()) else { break };
        if !add_directive(&mut out, keyword, &tokens) {
            break;
        }
        i = end;
    }
    out
}

#[derive(Debug)]
struct PubPackage {
    name: String,
    root: String,
}

struct FileScope {
    own: IndexSet<String>,
    importers: Vec<String>,
    imported: RefCell<Option<HashMap<String, Vec<(Filter, String)>>>>,
    visible: RefCell<HashMap<String, Rc<Vec<Filter>>>>,
}

type Namespace = Rc<IndexMap<String, Vec<Filter>>>;

#[derive(Default)]
struct Memo {
    directives: RefCell<HashMap<String, Option<Rc<DartDirectives>>>>,
    package_of_dir: RefCell<HashMap<String, Option<Rc<PubPackage>>>>,
    packages: RefCell<Option<HashMap<String, Vec<Rc<PubPackage>>>>>,
    path_dependencies: RefCell<HashMap<String, Rc<Vec<String>>>>,
    library_names: RefCell<Option<HashMap<String, Vec<String>>>>,
    library_of: RefCell<HashMap<String, Option<String>>>,
    parent_of: RefCell<HashMap<String, String>>,
    members: RefCell<HashMap<String, Rc<IndexSet<String>>>>,
    exports: RefCell<HashMap<String, Namespace>>,
    scopes: RefCell<HashMap<String, Option<Rc<FileScope>>>>,
    prefixes: RefCell<HashMap<String, Rc<IndexSet<String>>>>,
}

fn parent_dir(file: &str) -> String {
    match file.rfind('/') {
        Some(cut) => file[..cut].to_string(),
        None => String::new(),
    }
}

fn directives_of(file: &str, ctx: &Ctx, memo: &Memo) -> Option<Rc<DartDirectives>> {
    if let Some(hit) = memo.directives.borrow().get(file) {
        return hit.clone();
    }
    let directives = ctx.read_file(file).map(|s| Rc::new(read_dart_directives(&s)));
    memo.directives.borrow_mut().insert(file.to_string(), directives.clone());
    directives
}

fn package_of_dir(dir: &str, ctx: &Ctx, memo: &Memo) -> Option<Rc<PubPackage>> {
    let mut walked: Vec<String> = Vec::new();
    let mut d = dir.to_string();
    let found: Option<Rc<PubPackage>>;
    loop {
        if let Some(hit) = memo.package_of_dir.borrow().get(&d) {
            found = hit.clone();
            break;
        }
        walked.push(d.clone());
        let pubspec = if d.is_empty() { "pubspec.yaml".to_string() } else { format!("{d}/pubspec.yaml") };
        let name = if ctx.file_exists(&pubspec) {
            ctx.read_file(&pubspec)
                .and_then(|t| re!(r#"^name[ \t]*:[ \t]*['"]?([A-Za-z_]\w*)"#, "m").captures(&t).and_then(|c| c.text(1).map(str::to_string)))
        } else {
            None
        };
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            found = Some(Rc::new(PubPackage { name, root: d.clone() }));
            break;
        }
        if d.is_empty() {
            found = None;
            break;
        }
        d = parent_dir(&d);
    }
    for d in walked {
        memo.package_of_dir.borrow_mut().insert(d, found.clone());
    }
    found
}

fn packages_named(name: &str, ctx: &Ctx, memo: &Memo) -> Vec<Rc<PubPackage>> {
    if memo.packages.borrow().is_none() {
        let mut by_name: HashMap<String, Vec<Rc<PubPackage>>> = HashMap::new();
        let mut seen: Vec<Rc<PubPackage>> = Vec::new();
        for file in ctx.get_all_files() {
            if !file.ends_with(".dart") {
                continue;
            }
            let Some(pkg) = package_of_dir(&parent_dir(&file), ctx, memo) else { continue };
            if seen.iter().any(|s| Rc::ptr_eq(s, &pkg)) {
                continue;
            }
            seen.push(Rc::clone(&pkg));
            by_name.entry(pkg.name.clone()).or_default().push(pkg);
        }
        *memo.packages.borrow_mut() = Some(by_name);
    }
    memo.packages.borrow().as_ref().and_then(|m| m.get(name).cloned()).unwrap_or_default()
}

fn yaml_path_dependencies(text: &str, name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let lines = re!(r"\r?\n").split(text);
    for i in 0..lines.len() {
        let Some(key) = re!(r#"^([ \t]+)(['"]?)([A-Za-z_]\w*)\2[ \t]*:(?:[ \t]+(.*))?$"#).captures(lines[i]) else { continue };
        if key.text(3) != Some(name) {
            continue;
        }
        let value = js_trim(&re!(r"(?:^|[ \t])#.*$").replace_first(key.text(4).unwrap_or(""), |_| String::new())).to_string();
        if !value.is_empty() {
            let flow = if re!(r"^\{[^{}]*\}$").is_match(&value) {
                re!(r#"[{,][ \t]*path[ \t]*:[ \t]*(?:"([^"]*)"|'([^']*)'|([^,}\s]+))"#).captures(&value)
            } else {
                None
            };
            if let Some(f) = flow {
                out.push(f.text(1).or(f.text(2)).or(f.text(3)).unwrap_or("").to_string());
            }
            continue;
        }
        let mut child_indent: i64 = -1;
        for line in &lines[i + 1..] {
            if re!(r"^[ \t]*(?:#.*)?$").is_match(line) {
                continue;
            }
            let indent = re!(r"^[ \t]*").find(line).map_or(0, |m| m.as_str().chars().count()) as i64;
            if indent <= key.text(1).unwrap_or("").chars().count() as i64 {
                break;
            }
            if child_indent < 0 {
                child_indent = indent;
            }
            let dep = if indent == child_indent {
                re!(r#"^[ \t]*path[ \t]*:[ \t]*(?:"([^"]*)"|'([^']*)'|([^\s#]+))"#).captures(line)
            } else {
                None
            };
            if let Some(d) = dep {
                out.push(d.text(1).or(d.text(2)).or(d.text(3)).unwrap_or("").to_string());
            }
        }
    }
    out
}

fn path_dependencies(pkg: &PubPackage, name: &str, ctx: &Ctx, memo: &Memo) -> Rc<Vec<String>> {
    let key = format!("{}\0{name}", pkg.root);
    if let Some(hit) = memo.path_dependencies.borrow().get(&key) {
        return Rc::clone(hit);
    }
    let mut dirs: Vec<String> = Vec::new();
    for file in ["pubspec_overrides.yaml", "pubspec.yaml"] {
        let pubspec = if pkg.root.is_empty() { file.to_string() } else { format!("{}/{file}", pkg.root) };
        let text = if ctx.file_exists(&pubspec) { ctx.read_file(&pubspec) } else { None };
        for written in text.map(|t| yaml_path_dependencies(&t, name)).unwrap_or_default() {
            let dep = written.replace('\\', "/");
            if dep.starts_with('/') || re!(r"^[A-Za-z]:").is_match(&dep) {
                continue;
            }
            let joined = posix::normalize(&posix::join(&[if pkg.root.is_empty() { "." } else { &pkg.root }, &dep]));
            let dir = re!(r"/+$").replace_first(&joined, |_| String::new());
            if dir == ".." || dir.starts_with("../") {
                continue;
            }
            let root = if dir == "." { String::new() } else { dir };
            if !dirs.contains(&root) {
                dirs.push(root);
            }
        }
    }
    let dirs = Rc::new(dirs);
    memo.path_dependencies.borrow_mut().insert(key, Rc::clone(&dirs));
    dirs
}

fn resolve_dart_uri(from: &str, uri: &str, ctx: &Ctx, memo: &Memo) -> Vec<String> {
    if let Some(after) = uri.strip_prefix("package:") {
        let Some(slash) = after.find('/') else { return Vec::new() };
        let name = &after[..slash];
        let rest = &after[slash + 1..];
        let mut chosen = packages_named(name, ctx, memo);
        if chosen.len() > 1 {
            let own = package_of_dir(&parent_dir(from), ctx, memo);
            let enclosing: Vec<Rc<PubPackage>> =
                chosen.iter().filter(|p| p.root.is_empty() || from.starts_with(&format!("{}/", p.root))).cloned().collect();
            if let Some(o) = own.as_ref().filter(|o| o.name == name) {
                chosen = vec![Rc::clone(o)];
            } else if !enclosing.is_empty() {
                let mut best = Rc::clone(&enclosing[0]);
                for p in &enclosing[1..] {
                    if p.root.chars().count() > best.root.chars().count() {
                        best = Rc::clone(p);
                    }
                }
                chosen = vec![best];
            } else if let Some(o) = own {
                let dirs = path_dependencies(&o, name, ctx, memo);
                let declared: Vec<Rc<PubPackage>> = chosen.iter().filter(|p| dirs.contains(&p.root)).cloned().collect();
                if declared.len() == 1 {
                    chosen = declared;
                }
            }
        }
        return chosen
            .iter()
            .map(|p| posix::normalize(&if p.root.is_empty() { format!("lib/{rest}") } else { format!("{}/lib/{rest}", p.root) }))
            .collect();
    }
    if re!(r"^[A-Za-z][\w+.-]*:").is_match(uri) {
        return Vec::new();
    }
    let dir = parent_dir(from);
    let joined = posix::normalize(&posix::join(&[if dir.is_empty() { "." } else { &dir }, uri]));
    if joined == ".." || joined.starts_with("../") || joined.starts_with('/') {
        Vec::new()
    } else {
        vec![joined]
    }
}

/// The file a directive's `uri` names from `from`, when it names exactly one.
pub fn dart_directive_file(from: &str, uri: &str, ctx: &Ctx) -> Option<String> {
    let memo = ctx.memo::<Memo>();
    let files = resolve_dart_uri(from, uri, ctx, &memo);
    if files.len() == 1 {
        Some(files.into_iter().next().unwrap())
    } else {
        None
    }
}

fn library_files_named(name: &str, ctx: &Ctx, memo: &Memo) -> Vec<String> {
    if memo.library_names.borrow().is_none() {
        let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
        for file in ctx.get_all_files() {
            if !file.ends_with(".dart") || !ctx.file_contains(&file, "library") {
                continue;
            }
            let Some(declared) = directives_of(&file, ctx, memo).and_then(|d| d.library_name.clone()).filter(|n| !n.is_empty()) else {
                continue;
            };
            by_name.entry(declared).or_default().push(file);
        }
        *memo.library_names.borrow_mut() = Some(by_name);
    }
    memo.library_names.borrow().as_ref().and_then(|m| m.get(name).cloned()).unwrap_or_default()
}

fn part_parent(file: &str, part_of: &PartOf, ctx: &Ctx, memo: &Memo) -> Option<String> {
    let candidates: Vec<String> = match part_of {
        PartOf::Uri(uri) => resolve_dart_uri(file, uri, ctx, memo).into_iter().filter(|f| directives_of(f, ctx, memo).is_some()).collect(),
        PartOf::Name(name) => library_files_named(name, ctx, memo),
    };
    if candidates.len() <= 1 {
        return candidates.into_iter().next();
    }
    let listing: Vec<&String> = candidates
        .iter()
        .filter(|c| {
            directives_of(c, ctx, memo)
                .map(|d| d.parts.clone())
                .unwrap_or_default()
                .iter()
                .any(|uri| resolve_dart_uri(c, uri, ctx, memo).iter().any(|f| f == file))
        })
        .collect();
    if listing.len() == 1 {
        Some(listing[0].clone())
    } else {
        None
    }
}

fn library_of(file: &str, ctx: &Ctx, memo: &Memo) -> Option<String> {
    if let Some(hit) = memo.library_of.borrow().get(file) {
        return hit.clone();
    }
    memo.library_of.borrow_mut().insert(file.to_string(), None);
    let directives = directives_of(file, ctx, memo);
    let mut library: Option<String> = None;
    if let Some(d) = &directives {
        match &d.part_of {
            None => library = Some(file.to_string()),
            Some(part_of) => {
                if let Some(parent) = part_parent(file, part_of, ctx, memo) {
                    memo.parent_of.borrow_mut().insert(file.to_string(), parent.clone());
                    library = library_of(&parent, ctx, memo);
                }
            }
        }
    }
    memo.library_of.borrow_mut().insert(file.to_string(), library.clone());
    library
}

fn members_of(library: &str, ctx: &Ctx, memo: &Memo) -> Rc<IndexSet<String>> {
    if let Some(hit) = memo.members.borrow().get(library) {
        return Rc::clone(hit);
    }
    let mut files: IndexSet<String> = IndexSet::new();
    files.insert(library.to_string());
    let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    queue.push_back(library.to_string());
    while let Some(file) = queue.pop_front() {
        if files.len() >= 1000 {
            break;
        }
        for uri in directives_of(&file, ctx, memo).map(|d| d.parts.clone()).unwrap_or_default() {
            for part in resolve_dart_uri(&file, &uri, ctx, memo) {
                if files.contains(&part) {
                    continue;
                }
                files.insert(part.clone());
                queue.push_back(part);
            }
        }
    }
    let files = Rc::new(files);
    memo.members.borrow_mut().insert(library.to_string(), Rc::clone(&files));
    files
}

fn add_filter(namespace: &mut IndexMap<String, Vec<Filter>>, file: &str, filter: Filter) {
    match namespace.get_mut(file) {
        None => {
            namespace.insert(file.to_string(), vec![filter]);
        }
        Some(filters) => {
            if is_every(&filters[0]) {
                return;
            }
            if is_every(&filter) {
                filters.clear();
                filters.push(filter);
            } else if !filters.iter().any(|f| f.key == filter.key) {
                filters.push(filter);
            }
        }
    }
}

fn exports_of(library: &str, ctx: &Ctx, memo: &Memo, stack: &mut HashMap<String, usize>) -> (Namespace, f64) {
    if let Some(hit) = memo.exports.borrow().get(library) {
        return (Rc::clone(hit), f64::INFINITY);
    }
    let depth = stack.len();
    stack.insert(library.to_string(), depth);
    let mut namespace: IndexMap<String, Vec<Filter>> = IndexMap::new();
    let mut low = f64::INFINITY;
    let members = members_of(library, ctx, memo);
    for file in members.iter() {
        add_filter(&mut namespace, file, every_name());
    }
    for file in members.iter() {
        for directive in directives_of(file, ctx, memo).map(|d| d.exports.clone()).unwrap_or_default() {
            for uri in &directive.uris {
                for target in resolve_dart_uri(file, uri, ctx, memo) {
                    let Some(exported) = library_of(&target, ctx, memo) else { continue };
                    if let Some(&on_stack) = stack.get(&exported) {
                        low = low.min(on_stack as f64);
                        continue;
                    }
                    let (inner_ns, inner_low) = exports_of(&exported, ctx, memo, stack);
                    low = low.min(inner_low);
                    for (f, filters) in inner_ns.iter() {
                        for filter in filters {
                            add_filter(&mut namespace, f, compose_filters(&directive.filter, filter));
                        }
                    }
                }
            }
        }
    }
    stack.remove(library);
    let namespace = Rc::new(namespace);
    if low >= depth as f64 {
        memo.exports.borrow_mut().insert(library.to_string(), Rc::clone(&namespace));
    }
    (namespace, low)
}

fn scope_of(file: &str, ctx: &Ctx, memo: &Memo) -> Option<Rc<FileScope>> {
    if let Some(hit) = memo.scopes.borrow().get(file) {
        return hit.clone();
    }
    let library = if package_of_dir(&parent_dir(file), ctx, memo).is_none() { None } else { library_of(file, ctx, memo) };
    let mut scope: Option<Rc<FileScope>> = None;
    if let Some(library) = library {
        let mut own: IndexSet<String> = (*members_of(&library, ctx, memo)).clone();
        own.insert(file.to_string());
        let mut importers: Vec<String> = Vec::new();
        let mut f = Some(file.to_string());
        while let Some(cur) = f {
            if importers.len() >= 32 {
                break;
            }
            importers.push(cur.clone());
            if cur == library {
                break;
            }
            f = memo.parent_of.borrow().get(&cur).cloned();
        }
        scope = Some(Rc::new(FileScope { own, importers, imported: RefCell::new(None), visible: RefCell::new(HashMap::new()) }));
    }
    memo.scopes.borrow_mut().insert(file.to_string(), scope.clone());
    scope
}

/// The filters a prefix lets through for names declared in `decl_file`; `None` when the prefix
/// is not one the file imports under.
fn imported_filters(scope: &FileScope, prefix: &str, decl_file: &str, ctx: &Ctx, memo: &Memo) -> Option<Rc<Vec<Filter>>> {
    if scope.imported.borrow().is_none() {
        let mut imported: HashMap<String, Vec<(Filter, String)>> = HashMap::new();
        for importer in &scope.importers {
            for directive in directives_of(importer, ctx, memo).map(|d| d.imports.clone()).unwrap_or_default() {
                let key = directive.prefix.clone().unwrap_or_default();
                let libraries = imported.entry(key).or_default();
                for uri in &directive.uris {
                    for target in resolve_dart_uri(importer, uri, ctx, memo) {
                        if let Some(library) = library_of(&target, ctx, memo) {
                            libraries.push((Rc::clone(&directive.filter), library));
                        }
                    }
                }
            }
        }
        *scope.imported.borrow_mut() = Some(imported);
    }
    let libraries: Option<Vec<(Filter, String)>> = scope.imported.borrow().as_ref().unwrap().get(prefix).cloned();
    let Some(libraries) = libraries else {
        return if prefix.is_empty() { Some(Rc::new(Vec::new())) } else { None };
    };
    let key = format!("{prefix}\0{decl_file}");
    if let Some(hit) = scope.visible.borrow().get(&key) {
        return Some(Rc::clone(hit));
    }
    let mut through: IndexMap<String, Vec<Filter>> = IndexMap::new();
    for (filter, library) in &libraries {
        let (ns, _) = exports_of(library, ctx, memo, &mut HashMap::new());
        for exported in ns.get(decl_file).cloned().unwrap_or_default() {
            add_filter(&mut through, decl_file, compose_filters(filter, &exported));
        }
    }
    let filters = Rc::new(through.get(decl_file).cloned().unwrap_or_default());
    scope.visible.borrow_mut().insert(key, Rc::clone(&filters));
    Some(filters)
}

/// Does `from_file` see `name` declared in `decl_file` without a prefix?
pub fn dart_library_sees(from_file: &str, decl_file: &str, name: &str, ctx: &Ctx) -> bool {
    if from_file == decl_file {
        return true;
    }
    let memo = ctx.memo::<Memo>();
    let Some(scope) = scope_of(from_file, ctx, &memo) else { return true };
    if scope.own.contains(decl_file) {
        return true;
    }
    if name.starts_with('_') {
        return false;
    }
    imported_filters(&scope, "", decl_file, ctx, &memo).is_some_and(|fs| fs.iter().any(|f| lets(f, name)))
}

/// Does `from_file` see `name` declared in `decl_file` through the import prefix `prefix`?
pub fn dart_prefix_sees(from_file: &str, prefix: &str, decl_file: &str, name: &str, ctx: &Ctx) -> bool {
    let memo = ctx.memo::<Memo>();
    let Some(scope) = scope_of(from_file, ctx, &memo) else { return true };
    match imported_filters(&scope, prefix, decl_file, ctx, &memo) {
        None => true,
        Some(filters) => !name.starts_with('_') && filters.iter().any(|f| lets(f, name)),
    }
}

/// The import prefixes (`import 'x' as p`) in effect in `from_file`.
pub fn dart_import_prefixes(from_file: &str, ctx: &Ctx) -> Rc<IndexSet<String>> {
    let memo = ctx.memo::<Memo>();
    if let Some(hit) = memo.prefixes.borrow().get(from_file) {
        return Rc::clone(hit);
    }
    let mut prefixes: IndexSet<String> = IndexSet::new();
    library_of(from_file, ctx, &memo);
    let mut file = Some(from_file.to_string());
    let mut hops = 0;
    while let Some(cur) = file {
        if hops >= 32 {
            break;
        }
        for directive in directives_of(&cur, ctx, &memo).map(|d| d.imports.clone()).unwrap_or_default() {
            if let Some(p) = directive.prefix {
                prefixes.insert(p);
            }
        }
        file = memo.parent_of.borrow().get(&cur).cloned();
        hops += 1;
    }
    let prefixes = Rc::new(prefixes);
    memo.prefixes.borrow_mut().insert(from_file.to_string(), Rc::clone(&prefixes));
    prefixes
}

/// Are the two files in one library (the same file, or a library and its parts)?
pub fn in_same_dart_library(from_file: &str, decl_file: &str, ctx: &Ctx) -> bool {
    if from_file == decl_file {
        return true;
    }
    let memo = ctx.memo::<Memo>();
    scope_of(from_file, ctx, &memo).is_some_and(|s| s.own.contains(decl_file))
}
