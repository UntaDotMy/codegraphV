//! C# scope rules: which same-named member a receiver-less call reaches (the enclosing types,
//! their bases, `using static`), which namespaces a file can name types from (its own, the
//! `using`s, the project's implicit and global usings), and which nested types are in reach.

use super::cpp_text::split_cpp_top_level;
use super::line_scope::{has_no_receiver_on_line, posix_dirname};
use super::lang_shape::RefShape;
use super::util::*;
use crate::ctx::Ctx;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_extract::strip_comments::strip_comments_for_regex;
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::rc::Rc;

fn is_csharp_type_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Class | NodeKind::Interface | NodeKind::Enum | NodeKind::Struct)
}

fn is_csharp_member_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Method | NodeKind::Property | NodeKind::Field | NodeKind::EnumMember | NodeKind::Constant)
}

const CSHARP_IMPLICIT_USINGS: &[&str] = &["System", "System.Collections.Generic", "System.IO", "System.Linq", "System.Net.Http", "System.Threading", "System.Threading.Tasks"];

/// The usings in effect for a file: its namespaces, `using X;` namespaces and `using A = B;` aliases.
pub struct CsharpNamespaceScope {
    pub namespaces: Vec<String>,
    pub usings: HashSet<String>,
    pub aliases: HashMap<String, String>,
}

#[derive(Clone)]
pub struct ProjectUsings {
    usings: HashSet<String>,
    project: bool,
}

memo_map!(CsharpSupersMemo, String, Rc<Vec<String>>);
memo_map!(CsharpStaticUsingsMemo, String, Rc<HashSet<String>>);
memo_map!(CsharpProjectStaticUsingsMemo, String, Rc<HashSet<String>>);
memo_map!(CsharpNamespaceScopeMemo, String, Rc<CsharpNamespaceScope>);
memo_map!(CsharpProjectUsingsMemo, String, Rc<ProjectUsings>);
memo_map!(CsharpGlobalUsingsMemo, String, Rc<HashSet<String>>);
memo_map!(CsharpAncestorsMemo, String, Rc<RefCell<HashSet<String>>>);
memo_map!(CsharpAliasesMemo, String, Rc<HashMap<String, String>>);

/// `using Alias = Some.Type;` → `Type`, when `name` is such an alias in the reference's file.
pub fn csharp_using_alias(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    if !re!(r"^[A-Za-z_]\w*$").is_match(name) {
        return None;
    }
    let aliases = memoized::<CsharpAliasesMemo>(ctx, r.file_path.clone(), || {
        let source = ctx.read_file(&r.file_path).map(|s| s.to_string()).unwrap_or_default();
        let mut aliases = HashMap::new();
        for m in re!(r"^\s*(?:global\s+)?using\s+([A-Za-z_]\w*)\s*=\s*(?:global::)?([\w.]+)\s*(?:<[^;>]*>)?\s*;", "gm").captures_iter(&source) {
            aliases.insert(m.text(1).unwrap_or("").to_string(), m.text(2).unwrap_or("").rsplit('.').next().unwrap_or("").to_string());
        }
        Rc::new(aliases)
    });
    aliases.get(name).cloned()
}

/// Is the same-named member `n` one a receiver-less call can reach: through `using static`, or an
/// enclosing type or one of its bases?
pub fn is_csharp_member_in_scope(n: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if !is_csharp_member_kind(n.kind) {
        return true;
    }
    let Some(cut) = n.qualified_name.rfind("::") else { return true };
    if !has_no_receiver_on_line(r, ctx) {
        return true;
    }
    let owner = last_owner_segment(&n.qualified_name[..cut]);
    if csharp_static_usings(&r.file_path, ctx).contains(owner) {
        return true;
    }
    let around: Vec<NodeRef> = ctx.get_nodes_in_file(&r.file_path).iter().filter(|t| is_csharp_type_kind(t.kind) && t.start_line <= r.line && t.end_line >= r.line).cloned().collect();
    // Top-level statements have no enclosing type to hold the member.
    if around.is_empty() {
        return true;
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = around.iter().map(|t| t.name.clone()).collect();
    while let Some(name) = queue.pop_front() {
        if seen.len() >= 40 {
            break;
        }
        if !seen.insert(name.clone()) {
            continue;
        }
        if name == owner {
            return true;
        }
        queue.extend(csharp_supertypes_of(&name, ctx).iter().cloned());
    }
    false
}

/// The last `::` / `.` separated segment of an owner path.
fn last_owner_segment(path: &str) -> &str {
    let by_colons = path.rfind("::").map(|i| i + 2);
    let by_dot = path.rfind('.').map(|i| i + 1);
    &path[by_colons.into_iter().chain(by_dot).max().unwrap_or(0)..]
}

/// The base types a C# type's head names after its `:`.
fn csharp_supertypes_of(type_name: &str, ctx: &Ctx) -> Rc<Vec<String>> {
    memoized::<CsharpSupersMemo>(ctx, type_name.to_string(), || {
        let mut names: Vec<String> = Vec::new();
        for decl in ctx.get_nodes_by_name(type_name).iter() {
            if decl.language != Language::CSharp || !is_csharp_type_kind(decl.kind) {
                continue;
            }
            let lines = lines_of(ctx, &decl.file_path);
            let mut head = js_slice(&lines, decl.start_line as i64 - 1, decl.start_line as i64 + 8).join(" ");
            if let Some(brace) = head.find('{') {
                head.truncate(brace);
            }
            let mut depth = 0i32;
            let mut flat = String::new();
            for ch in head.chars() {
                if ch == '<' || ch == '(' {
                    depth += 1;
                } else if ch == '>' || ch == ')' {
                    depth = (depth - 1).max(0);
                } else if depth == 0 {
                    flat.push(ch);
                }
            }
            let bases = Re::cached(&format!(r"\b{}\s*:\s*(.*?)(?:\bwhere\b|$)", escape_regex(type_name)), "").captures(&flat).and_then(|c| c.text(1).map(String::from)).unwrap_or_default();
            for m in re!(r"([A-Za-z_][\w.]*)", "g").captures_iter(&bases) {
                names.push(m.text(1).unwrap_or("").rsplit('.').next().unwrap_or("").to_string());
            }
        }
        Rc::new(names)
    })
}

fn csproj_entries(ctx: &Ctx, dir: &str) -> Vec<(String, String)> {
    let base = if dir == "." || dir.is_empty() || dir == "/" { Path::new(ctx.project_root()).to_path_buf() } else { Path::new(ctx.project_root()).join(dir) };
    let Ok(read) = std::fs::read_dir(&base) else { return Vec::new() };
    let mut names: Vec<String> = read.filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).collect();
    names.sort();
    names
        .into_iter()
        .filter(|entry| re!(r"\.(?:csproj|props)$", "i").is_match(entry))
        .filter_map(|entry| std::fs::read_to_string(base.join(&entry)).ok().map(|text| (entry, text)))
        .collect()
}

/// The types a file can call statically without a qualifier: its own `using static`s plus the
/// project's `global using static` and `<Using Static="true">` ones above it.
fn csharp_static_usings(file: &str, ctx: &Ctx) -> Rc<HashSet<String>> {
    memoized::<CsharpStaticUsingsMemo>(ctx, file.to_string(), || {
        let mut owners: HashSet<String> = csharp_project_static_usings(&posix_dirname(file), ctx).iter().cloned().collect();
        let text = ctx.read_file(file).map(|s| s.to_string()).unwrap_or_default();
        for m in re!(r"^\s*(?:global\s+)?using\s+static\s+([\w.]+)\s*;", "gm").captures_iter(&text) {
            owners.insert(m.text(1).unwrap_or("").rsplit('.').next().unwrap_or("").to_string());
        }
        Rc::new(owners)
    })
}

fn csharp_project_static_usings(dir: &str, ctx: &Ctx) -> Rc<HashSet<String>> {
    memoized::<CsharpProjectStaticUsingsMemo>(ctx, dir.to_string(), || {
        let mut owners: HashSet<String>;
        if dir == "." || dir.is_empty() || dir == "/" {
            owners = HashSet::new();
            for f in ctx.get_all_files() {
                if !f.ends_with(".cs") {
                    continue;
                }
                let text = ctx.read_file(&f).map(|s| s.to_string()).unwrap_or_default();
                if !text.contains("global using static") {
                    continue;
                }
                for m in re!(r"^\s*global\s+using\s+static\s+([\w.]+)\s*;", "gm").captures_iter(&text) {
                    owners.insert(m.text(1).unwrap_or("").rsplit('.').next().unwrap_or("").to_string());
                }
            }
        } else {
            owners = csharp_project_static_usings(&posix_dirname(dir), ctx).iter().cloned().collect();
        }
        for (_, text) in csproj_entries(ctx, dir) {
            for m in re!(r#"<Using\s+Include\s*=\s*"([\w.]+)"[^>]*\bStatic\s*=\s*"true""#, "gi").captures_iter(&text) {
                owners.insert(m.text(1).unwrap_or("").rsplit('.').next().unwrap_or("").to_string());
            }
        }
        Rc::new(owners)
    })
}

/// A file's namespaces, the namespaces it imports (explicitly, through `global using`s and
/// through `<Using>` / implicit usings of its project) and its `using Alias = …` aliases.
pub fn csharp_namespace_scope(file: &str, ctx: &Ctx) -> Rc<CsharpNamespaceScope> {
    memoized::<CsharpNamespaceScopeMemo>(ctx, file.to_string(), || {
        let text = strip_comments_for_regex(&ctx.read_file(file).map(|s| s.to_string()).unwrap_or_default(), "java");
        let namespaces: Vec<String> = re!(r"^\s*namespace\s+([\w.]+)", "gm").captures_iter(&text).filter_map(|m| m.text(1).map(String::from)).collect();
        let project = csharp_project_usings(&posix_dirname(file), ctx);
        let mut usings: HashSet<String> = project.usings.clone();
        if !project.project {
            usings.extend(csharp_global_usings(".", ctx).iter().cloned());
        }
        let mut aliases = HashMap::new();
        for m in re!(r"^\s*(?:global\s+)?using\s+(?!static\b)(?:([A-Za-z_]\w*)\s*=\s*)?([\w.]+)\s*;", "gm").captures_iter(&text) {
            let target = m.text(2).unwrap_or("").to_string();
            match m.text(1).filter(|a| !a.is_empty()) {
                Some(alias) => {
                    aliases.insert(alias.to_string(), target);
                }
                None => {
                    usings.insert(target);
                }
            }
        }
        Rc::new(CsharpNamespaceScope { namespaces, usings, aliases })
    })
}

/// The `<Using>` items and implicit usings of a directory's project files and of those above it.
fn csharp_project_usings(dir: &str, ctx: &Ctx) -> Rc<ProjectUsings> {
    memoized::<CsharpProjectUsingsMemo>(ctx, dir.to_string(), || {
        let root = dir == "." || dir.is_empty() || dir == "/";
        let parent = if root { None } else { Some(csharp_project_usings(&posix_dirname(dir), ctx)) };
        let mut usings: HashSet<String> = parent.as_ref().map(|p| p.usings.clone()).unwrap_or_default();
        let mut project = parent.as_ref().is_some_and(|p| p.project);
        for (entry, text) in csproj_entries(ctx, dir) {
            if re!(r"\.csproj$", "i").is_match(&entry) && !project {
                project = true;
                usings.extend(csharp_global_usings(if root { "." } else { dir }, ctx).iter().cloned());
            }
            for m in re!(r#"<Using\s+Include\s*=\s*"([\w.]+)"(?![^>]*\bStatic\s*=\s*"true")[^>]*>"#, "gi").captures_iter(&text) {
                usings.insert(m.text(1).unwrap_or("").to_string());
            }
            if re!(r"<ImplicitUsings>\s*(?:enable|true)\s*</ImplicitUsings>", "i").is_match(&text) {
                usings.extend(CSHARP_IMPLICIT_USINGS.iter().map(|s| s.to_string()));
            }
        }
        Rc::new(ProjectUsings { usings, project })
    })
}

/// The `global using X;` namespaces declared in the files under a directory.
fn csharp_global_usings(dir: &str, ctx: &Ctx) -> Rc<HashSet<String>> {
    memoized::<CsharpGlobalUsingsMemo>(ctx, dir.to_string(), || {
        let mut usings = HashSet::new();
        let prefix = if dir == "." { String::new() } else { format!("{dir}/") };
        for f in ctx.get_all_files() {
            if !f.ends_with(".cs") || !f.starts_with(&prefix) || !ctx.file_contains(&f, "global using") {
                continue;
            }
            let text = ctx.read_file(&f).map(|s| s.to_string()).unwrap_or_default();
            for m in re!(r"^\s*global\s+using\s+(?!static\b)([\w.]+)\s*;", "gm").captures_iter(&text) {
                usings.insert(m.text(1).unwrap_or("").to_string());
            }
        }
        Rc::new(usings)
    })
}

/// Where `text` goes on past the C# attribute lists that open at `from`.
fn csharp_past_attributes(text: &str, from: usize) -> usize {
    let bytes = text.as_bytes();
    let mut at = from;
    loop {
        loop {
            while at < bytes.len() && bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            if at + 1 < bytes.len() && bytes[at] == b'/' && bytes[at + 1] == b'/' {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
                continue;
            }
            if at + 1 < bytes.len() && bytes[at] == b'/' && bytes[at + 1] == b'*' {
                at += 2;
                while at + 1 < bytes.len() && !(bytes[at] == b'*' && bytes[at + 1] == b'/') {
                    at += 1;
                }
                at = (at + 2).min(bytes.len());
                continue;
            }
            if at < bytes.len() && bytes[at] == b'#' {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
                continue;
            }
            break;
        }
        if at >= bytes.len() || bytes[at] != b'[' {
            return at;
        }
        let mut depth = 0i32;
        while at < bytes.len() {
            let c = bytes[at];
            if c == b'"' || c == b'\'' {
                let quote = c;
                at += 1;
                while at < bytes.len() {
                    if bytes[at] == b'\\' && quote == b'"' {
                        at += 2;
                        continue;
                    }
                    if bytes[at] == quote {
                        if quote == b'"' && bytes.get(at + 1) == Some(&b'"') {
                            at += 2;
                            continue;
                        }
                        at += 1;
                        break;
                    }
                    at += 1;
                }
                continue;
            }
            if c == b'[' {
                depth += 1;
            } else if c == b']' {
                depth -= 1;
                if depth == 0 {
                    at += 1;
                    break;
                }
            }
            at += 1;
        }
        if depth > 0 {
            return at;
        }
    }
}

/// A C# declaration's source from past its attributes: its node starts at them, so its first
/// line may be `[MethodImpl(…)]`, and more of them may follow.
fn csharp_declaration_source(node: &Node, ctx: &Ctx) -> String {
    let lines = lines_of(ctx, &node.file_path);
    let source = js_slice(&lines, node.start_line as i64 - 1, node.end_line as i64).join("\n");
    let at = csharp_past_attributes(&source, 0);
    source[at..].to_string()
}

memo_map!(CsharpTypeArityMemo, String, usize);

/// How many type parameters a C# type declares: `class Cache<K, V>` 2, `class List` 0.
fn csharp_declared_arity(t: &Node, ctx: &Ctx) -> usize {
    memoized::<CsharpTypeArityMemo>(ctx, t.id.clone(), || {
        if t.kind == NodeKind::Component {
            return lines_of(ctx, &t.file_path).iter().filter(|l| re!(r"^\s*@typeparam\b").is_match(l)).count();
        }
        // The first `class Name` is the declaration; a later one is a comment or a string.
        let pattern = format!(
            r"\b(?:class|struct|interface|record|enum)\s+(?:(?:class|struct)\s+)?{}\b(?:\s*<([^<>]*)>)?",
            escape_regex(&t.name)
        );
        let source = csharp_declaration_source(t, ctx);
        let params = Re::cached(&pattern, "").captures(&source).and_then(|c| c.text(1));
        params.map_or(0, |p| p.split(',').count())
    })
}

/// How many type arguments the C# type named at `r` is written with: 2 for
/// `Cache<string, List<int>>`, 0 for none, `None` when the list doesn't close on its line.
fn csharp_written_arity(r: &UnresolvedRef, ctx: &Ctx) -> Option<usize> {
    let name = r.reference_name.rsplit('.').next().unwrap_or("");
    let lines = lines_of(ctx, &r.file_path);
    let line = lines.get(r.line as usize - 1).cloned().unwrap_or_default();
    let chars: Vec<char> = line.chars().collect();
    let name_chars: Vec<char> = name.chars().collect();
    let start = (r.column as usize).min(chars.len());
    if name_chars.is_empty() {
        return Some(0);
    }
    let at = (start..=chars.len().saturating_sub(name_chars.len()))
        .find(|&i| chars[i..i + name_chars.len()] == name_chars[..]);
    let Some(at) = at else { return Some(0) };
    let mut i = at + name_chars.len();
    while chars.get(i) == Some(&' ') {
        i += 1;
    }
    if chars.get(i) != Some(&'<') {
        return Some(0);
    }
    let mut depth = 0i32;
    let mut args = 1usize;
    while i < chars.len() {
        match chars[i] {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(args);
                }
            }
            ',' if depth == 1 => args += 1,
            _ => {}
        }
        i += 1;
    }
    None
}

/// Whether a C# type can be what `r` names, by type arguments: a name written with them means a
/// type declaring as many. `new List<CatalogBrand>()` is the BCL's `List<T>`, not the Blazor page
/// `List` around it, whose namespace would win for a bare `List`.
pub fn csharp_type_arity_fits(t: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if !(r.is_kind(EdgeKind::References) || r.is_kind(EdgeKind::Instantiates)) {
        return true;
    }
    if !(is_csharp_type_kind(t.kind) || (t.kind == NodeKind::Component && t.language == Language::Razor)) {
        return true;
    }
    let written = csharp_written_arity(r, ctx);
    match written {
        Some(n) if n > 0 => n == csharp_declared_arity(t, ctx),
        _ => true,
    }
}

/// A first parameter past its attributes, written `this`: `this T x`, `ref this int x`.
const CSHARP_THIS_PARAMETER: &str = r"^(?:(?:ref|in|scoped|readonly)\s+)*this\s";

memo_map!(CsharpExtensionMethodsMemo, String, bool);

/// Whether a C# method is an extension method: its first parameter is written `this T x`. The
/// list is found from the method's name, since attributes may stand above the name, the return
/// type may be a tuple, and the list may wrap.
pub fn is_csharp_extension_method(method: &Node, ctx: &Ctx) -> bool {
    if method.language != Language::CSharp {
        return false;
    }
    memoized::<CsharpExtensionMethodsMemo>(ctx, method.id.clone(), || {
        let source = csharp_declaration_source(method, ctx);
        let pattern = format!(r"\b{}\s*(?:<[^()]*>\s*)?\(", escape_regex(&method.name));
        let Some(list) = Re::cached(&pattern, "").captures(&source).and_then(|c| c.get(0)) else { return false };
        let at = csharp_past_attributes(&source, list.start() + list.as_str().len());
        Re::cached(CSHARP_THIS_PARAMETER, "").is_match(&source[at..])
    })
}

/// Whether a C# extension method calls itself on an element a lambda of its body receives:
/// serilog's `sequence.Elements.Select(e => e.LiteralValue())` inside
/// `LiteralValue(this LogEventPropertyValue @this)` walks a value's children.
pub fn is_csharp_extension_element_call(method: &Node, receiver: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if !is_csharp_extension_method(method, ctx) {
        return false;
    }
    let lines = lines_of(ctx, &r.file_path);
    let start = (method.start_line as usize).saturating_sub(1).min(lines.len());
    let end = (r.line as usize).saturating_sub(1).min(lines.len());
    let mut parts: Vec<String> = lines[start.min(end)..end].to_vec();
    if let Some(line) = lines.get(end) {
        parts.push(before_col(line, r.column).to_string());
    }
    let before = parts.join("\n");
    let pattern = format!(r"(?:^|[^\w.])\(?{}\)?\s*=>", escape_regex(receiver));
    Re::cached(&pattern, "").is_match(&before)
}

/// A C# type is nameable from a file in its namespace (or below it), or one that `using`s the
/// namespace; an alias names exactly its target.
pub fn is_csharp_type_visible(n: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let Some(cut) = n.qualified_name.find("::") else { return true };
    let ns = &n.qualified_name[..cut];
    let scope = csharp_namespace_scope(&r.file_path, ctx);
    if let Some(aliased) = scope.aliases.get(&r.reference_name) {
        return *aliased == format!("{ns}.{}", n.qualified_name[cut + 2..].replace("::", "."));
    }
    if scope.namespaces.iter().any(|own| own == ns || own.starts_with(&format!("{ns}."))) {
        return true;
    }
    scope.usings.contains(ns)
}

/// A nested type is named bare only inside its owner, a type nested in it, or a subtype of it.
pub fn is_csharp_nested_type_in_scope(n: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let Some(cut) = n.qualified_name.rfind("::") else { return true };
    let owner = &n.qualified_name[..cut];
    let Some(owner_type) = ctx.get_nodes_in_file(&n.file_path).iter().find(|p| is_csharp_type_kind(p.kind) && p.qualified_name == owner).cloned() else { return true };
    let enclosing: Vec<NodeRef> = ctx.get_nodes_in_file(&r.file_path).iter().filter(|p| is_csharp_type_kind(p.kind) && p.start_line <= r.line && p.end_line >= r.line).cloned().collect();
    if enclosing.iter().any(|p| p.qualified_name == owner || p.qualified_name.starts_with(&format!("{owner}::"))) {
        return true;
    }
    enclosing.iter().any(|p| csharp_ancestor_names(&p.qualified_name, ctx).borrow().contains(&owner_type.name))
}

/// The simple names of everything the type `qn` derives from, transitively (to a depth).
fn csharp_ancestor_names(qn: &str, ctx: &Ctx) -> Rc<RefCell<HashSet<String>>> {
    let memo = ctx.memo::<CsharpAncestorsMemo>();
    if let Some(hit) = memo.lookup(&qn.to_string()) {
        return hit;
    }
    // A walk of its own for every type, memoized only when finished (upstream fab65e9d).
    let mut names: HashSet<String> = HashSet::new();
    let mut seen: HashSet<String> = HashSet::from([qn.to_string()]);
    let mut level = vec![qn.to_string()];
    while !level.is_empty() {
        let mut bases: Vec<String> = Vec::new();
        for ty in &level {
            for decl in ctx.get_nodes_by_qualified_name(ty).iter() {
                if decl.language != Language::CSharp || !is_csharp_type_kind(decl.kind) {
                    continue;
                }
                let lines = lines_of(ctx, &decl.file_path);
                let mut header = String::new();
                let mut i = (decl.start_line as usize).saturating_sub(1);
                while i < lines.len().min(decl.start_line as usize + 6) && !header.contains('{') {
                    header.push_str(&lines[i]);
                    header.push(' ');
                    i += 1;
                }
                let before_brace = header.split('{').next().unwrap_or("");
                let before_brace = re!(r"\bwhere\b[\s\S]*$").replace_first(before_brace, |_| String::new());
                let list = re!(r":\s*([^{;]*)").captures(&before_brace).and_then(|c| c.text(1).map(String::from)).unwrap_or_default();
                for base in split_cpp_top_level(&list) {
                    if let Some(name) = re!(r"([A-Za-z_]\w*)\s*(?:<.*)?$").captures(base.trim_matches(codegraph_types::jscompat::is_js_space)).and_then(|c| c.text(1).map(String::from)) {
                        if !bases.contains(&name) {
                            bases.push(name);
                        }
                    }
                }
            }
        }
        let mut next: Vec<String> = Vec::new();
        for base in bases {
            for t in ctx.get_nodes_by_name(&base).iter() {
                if t.language != Language::CSharp || !is_csharp_type_kind(t.kind) || !seen.insert(t.qualified_name.clone()) {
                    continue;
                }
                next.push(t.qualified_name.clone());
            }
            names.insert(base);
        }
        level = next;
    }
    let names = Rc::new(RefCell::new(names));
    memo.store(qn.to_string(), Rc::clone(&names));
    names
}

/// The C# contribution to [`super::lang_shape::LangShape`]: a bare call or reference.
pub struct CsharpShape;

impl CsharpShape {
    pub fn of(r: &UnresolvedRef) -> Option<CsharpShape> {
        (r.language == Language::CSharp && (is_calls(r) || is_kind(r, codegraph_types::EdgeKind::References)) && re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name)).then_some(CsharpShape)
    }
}

impl RefShape for CsharpShape {
    fn excludes(&self, n: &NodeRef, r: &UnresolvedRef, ctx: &Ctx) -> bool {
        !is_csharp_member_in_scope(n, r, ctx)
    }
}
