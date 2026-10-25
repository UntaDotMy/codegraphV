//! Scala scope rules: which same-named member a call can reach from where it is written (a local
//! binder, an import, an enclosing or inherited type, an anonymous `new T { }` base) and which
//! package-object members a file can see.

use super::lang_shape::RefShape;
use super::method::shares_receiver_word;
use super::util::*;
use crate::ctx::Ctx;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::{Language, Node, NodeKind};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

fn is_scala_type_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Class | NodeKind::Trait | NodeKind::Interface | NodeKind::Enum | NodeKind::Struct | NodeKind::Module | NodeKind::Namespace)
}

fn is_scala_member_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Method | NodeKind::Field | NodeKind::Property | NodeKind::Variable | NodeKind::Constant)
}

fn is_scala_function_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Method | NodeKind::Function)
}

/// Names a file imports, as the scope check reads them.
#[derive(Default)]
pub struct ScalaImports {
    /// `import a.B._` / `import a.B.{_, x}`: the owner `B`, in import order.
    owners: Vec<String>,
    /// `import a.B.x`: `B.x`.
    members: HashSet<String>,
    /// Owners that are values (a lowercase `val` / parameter), not objects.
    values: HashSet<String>,
}

memo_map!(ScalaSupersMemo, String, Rc<Vec<String>>);
memo_map!(ScalaImportsMemo, String, Rc<ScalaImports>);
memo_map!(ScalaImportedSupersMemo, String, Rc<HashSet<String>>);
memo_map!(ScalaPackageObjectsMemo, (), Rc<HashMap<String, Vec<String>>>);
memo_map!(ScalaObjectPackageMemo, String, Option<String>);

/// Is the same-named candidate `n` one a call of that name can reach from where it is written?
pub fn is_scala_member_in_scope(n: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let Some(line) = line_at(ctx, &r.file_path, r.line).or_else(|| {
        let idx = (r.line as usize).checked_sub(1)?;
        ctx.read_file(&r.file_path).and_then(|s| s.split('\n').nth(idx).map(String::from))
    }) else {
        return true;
    };
    let name = r.reference_name.as_str();
    let Some(at) = Re::cached(&format!(r"(?<![\w$]){}\b", escape_regex(name)), "").find(&line) else { return true };
    let before = &line[..at.start()];
    if re!(r"\.\s*$").is_match(before) {
        // A member of the file's own is the one the receiver reaches.
        if n.file_path == r.file_path {
            return true;
        }
        if is_scala_type_kind(n.kind) {
            // `pkg.Holder.Type`: the type is a member of the package object / object that holds it.
            let segments: Vec<&str> = n.qualified_name.split("::").collect();
            let outer = if segments.len() >= 2 { Some(segments[segments.len() - 2]) } else { None };
            let holder = match outer {
                Some(o) => o.rsplit('.').next().unwrap_or("").to_string(),
                None => {
                    let source = ctx.read_file(&n.file_path).map(|s| s.to_string()).unwrap_or_default();
                    re!(r"^\s*package\s+([\w.]+)\s*$", "gm")
                        .captures_iter(&source)
                        .last()
                        .and_then(|c| c.text(1).map(|p| p.rsplit('.').next().unwrap_or("").to_string()))
                        .unwrap_or_default()
                }
            };
            return !holder.is_empty() && scala_receiver_name(before).rsplit('.').next() == Some(holder.as_str());
        }
        if !is_scala_member_kind(n.kind) {
            return n.kind != NodeKind::Function || is_scala_extension_method(n, ctx);
        }
        let receiver = scala_receiver_name(before);
        return !receiver.is_empty() && shares_receiver_word(&receiver, &n.qualified_name);
    }
    if let Some((start, end)) = scala_local_binder(name, r, ctx) {
        return n.file_path == r.file_path && n.start_line >= start && n.end_line <= end;
    }
    if !is_scala_member_kind(n.kind) {
        return true;
    }
    let Some(cut) = n.qualified_name.rfind("::") else { return true };
    if n.file_path == r.file_path {
        return true;
    }
    let owner = n.qualified_name[..cut].rsplit("::").next().unwrap_or("");
    let imports = scala_imports_of(&r.file_path, ctx);
    if imports.owners.iter().any(|o| o == owner) || imports.members.contains(&format!("{owner}.{name}")) {
        return true;
    }
    // A value imported from (`import conf._` where `conf` is a `val`) hides which members it holds.
    if !imports.values.is_empty() {
        return true;
    }
    if scala_imported_supertypes(&r.file_path, &imports, ctx).contains(owner) {
        return true;
    }
    let around: Vec<NodeRef> =
        ctx.get_nodes_in_file(&r.file_path).iter().filter(|t| is_scala_type_kind(t.kind) && t.start_line <= r.line && t.end_line >= r.line).cloned().collect();
    if around.is_empty() {
        return true;
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = around.iter().map(|t| t.name.clone()).chain(scala_anonymous_bases(r, ctx)).collect();
    while let Some(type_name) = queue.pop_front() {
        if seen.len() >= 60 {
            break;
        }
        if !seen.insert(type_name.clone()) {
            continue;
        }
        if type_name == owner {
            return true;
        }
        queue.extend(scala_supertypes_of(&type_name, ctx).iter().cloned());
    }
    false
}

/// The bases of every `new Base with Other { … }` block that encloses the reference.
fn scala_anonymous_bases(r: &UnresolvedRef, ctx: &Ctx) -> Vec<String> {
    let lines = lines_of(ctx, &r.file_path);
    let mut bases: Vec<String> = Vec::new();
    let mut depth = 0i32;
    let mut i = r.line as i64 - 1;
    while i >= 0 && i >= r.line as i64 - 400 {
        let Some(text) = lines.get(i as usize) else {
            i -= 1;
            continue;
        };
        for (c, ch) in text.char_indices().rev() {
            if ch == '}' {
                depth += 1;
            } else if ch == '{' {
                if depth > 0 {
                    depth -= 1;
                    continue;
                }
                let head = re!(r"\bnew\s+([\w.]+(?:\s*\[[^\]]*\])?(?:\s*\([^)]*\))?(?:\s+with\s+[\w.]+(?:\s*\[[^\]]*\])?)*)\s*$").captures(&text[..c]).and_then(|h| h.text(1).map(String::from));
                if let Some(head) = head {
                    let stripped = re!(r"\[[^\]]*\]|\([^)]*\)", "g").replace_all_str(&head, "");
                    for m in re!(r"\s+with\s+").split(&stripped) {
                        bases.push(m.trim_matches(codegraph_types::jscompat::is_js_space).rsplit('.').next().unwrap_or("").to_string());
                    }
                }
            }
        }
        i -= 1;
    }
    bases
}

/// The dotted name a call is written on, call and index arguments dropped.
fn scala_receiver_name(before: &str) -> String {
    let text = re!(r"\s*\.\s*$").replace_first(before, |_| String::new());
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<char> = Vec::new();
    let mut i = chars.len() as isize - 1;
    while i >= 0 {
        let ch = chars[i as usize];
        if ch == ')' || ch == ']' {
            let open = if ch == ')' { '(' } else { '[' };
            let mut depth = 0i32;
            while i >= 0 {
                if chars[i as usize] == ch {
                    depth += 1;
                } else if chars[i as usize] == open {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                i -= 1;
            }
            if i < 0 {
                return String::new();
            }
            i -= 1;
        } else if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '$' | '.') {
            out.push(ch);
            i -= 1;
        } else {
            break;
        }
    }
    out.reverse();
    out.into_iter().collect::<String>().trim_matches('.').to_string()
}

/// Is the function at `n` declared inside an `extension (x: T)` block (so it is called on a receiver)?
fn is_scala_extension_method(n: &Node, ctx: &Ctx) -> bool {
    let lines = lines_of(ctx, &n.file_path);
    js_slice(&lines, (n.start_line as i64 - 4).max(0), n.start_line as i64).iter().any(|l| re!(r"^\s*extension\b").is_match(l))
}

/// The function or type whose body binds `name` locally (a parameter, `val`, `def`, lambda
/// parameter or generator) before the reference: the span a same-named member must sit in to win.
fn scala_local_binder(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<(u32, u32)> {
    let nodes = ctx.get_nodes_in_file(&r.file_path);
    let innermost = |pick: fn(NodeKind) -> bool| -> Option<&NodeRef> {
        let mut inside: Vec<&NodeRef> = nodes.iter().filter(|f| pick(f.kind) && f.start_line <= r.line && f.end_line >= r.line).collect();
        inside.sort_by_key(|f| f.end_line.saturating_sub(f.start_line));
        inside.into_iter().next()
    };
    let function = innermost(is_scala_function_kind);
    let scope = function.or_else(|| innermost(is_scala_type_kind))?;
    let lines = lines_of(ctx, &r.file_path);
    let text = js_slice(&lines, scope.start_line as i64 - 1, r.line as i64).join("\n");
    let n = escape_regex(name);
    let binder = Re::cached(
        &format!(r"(?:[(,\[]\s*(?:implicit\s+|using\s+)?{n}\s*:)|(?:\b(?:val|var|def|lazy\s+val)\s+{n}\b)|(?:(?<![\w$.]){n}\s*(?:=>|<-))|(?:\(\s*{n}\s*(?:,[^)]*)?\)\s*=>)"),
        "g",
    );
    if let Some(function) = function {
        return binder.is_match(&text).then_some((function.start_line, function.end_line));
    }
    // A type body: a binder counts when it sits in a nested block that is still open at the reference.
    let bytes = text.as_bytes();
    let mut block_at: Vec<usize> = vec![0; bytes.len()];
    let mut open: Vec<usize> = Vec::new();
    let mut next = 0usize;
    for (i, &ch) in bytes.iter().enumerate() {
        if ch == b'{' {
            next += 1;
            open.push(next);
        } else if ch == b'}' {
            open.pop();
        }
        block_at[i] = if open.len() > 1 { *open.last().unwrap_or(&0) } else { 0 };
    }
    let live: HashSet<usize> = open.iter().skip(1).copied().collect();
    for m in binder.captures_iter(&text) {
        let Some(whole) = m.get(0) else { continue };
        let block = block_at.get(whole.start()).copied().unwrap_or(0);
        if block != 0 && live.contains(&block) {
            return Some((scope.start_line, scope.end_line));
        }
    }
    None
}

/// The types a Scala class / trait / object `extends` or mixes in with `with`.
pub fn scala_supertypes_of(type_name: &str, ctx: &Ctx) -> Rc<Vec<String>> {
    memoized::<ScalaSupersMemo>(ctx, type_name.to_string(), || {
        let mut names: Vec<String> = Vec::new();
        for decl in ctx.get_nodes_by_name(type_name).iter() {
            if decl.language != Language::Scala || !is_scala_type_kind(decl.kind) {
                continue;
            }
            let lines = lines_of(ctx, &decl.file_path);
            let text = js_slice(&lines, decl.start_line as i64 - 1, decl.start_line as i64 + 12).join(" ");
            let mut depth = 0i32;
            let mut flat = String::new();
            for ch in text.chars() {
                if ch == '[' || ch == '(' {
                    depth += 1;
                } else if ch == ']' || ch == ')' {
                    depth = (depth - 1).max(0);
                } else if depth == 0 {
                    if ch == '{' || ch == '=' {
                        break;
                    }
                    flat.push(ch);
                }
            }
            let clause = re!(r"\bextends\b(.*)$").captures(&flat).and_then(|c| c.text(1).map(String::from)).unwrap_or_default();
            for m in re!(r"([A-Za-z_][\w.]*)", "g").captures_iter(&clause) {
                let simple = m.text(1).unwrap_or("").rsplit('.').next().unwrap_or("");
                if simple != "with" && simple != "derives" && simple != type_name {
                    names.push(simple.to_string());
                }
            }
        }
        Rc::new(names)
    })
}

/// `package object P extends A with B` → `P` ↦ `[A, B]`, over every Scala file.
fn scala_package_objects(ctx: &Ctx) -> Rc<HashMap<String, Vec<String>>> {
    memoized::<ScalaPackageObjectsMemo>(ctx, (), || {
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        for file in ctx.get_all_files() {
            if !file.ends_with(".scala") || !ctx.file_contains(&file, "package object") {
                continue;
            }
            let source = ctx.read_file(&file).map(|s| s.to_string()).unwrap_or_default();
            for m in re!(r"\bpackage\s+object\s+([\w$]+)\s+extends\s+([^{\n]+)", "g").captures_iter(&source) {
                let bases = re!(r"\[[^\]]*\]", "g").replace_all_str(m.text(2).unwrap_or(""), "");
                let names: Vec<String> = re!(r"\bwith\b")
                    .split(&bases)
                    .into_iter()
                    .map(|t| {
                        let last = t.trim_matches(codegraph_types::jscompat::is_js_space).rsplit('.').next().unwrap_or("").to_string();
                        re!(r"\(.*$").replace_first(&last, |_| String::new()).trim_matches(codegraph_types::jscompat::is_js_space).to_string()
                    })
                    .filter(|s| !s.is_empty())
                    .collect();
                out.entry(m.text(1).unwrap_or("").to_string()).or_default().extend(names);
            }
        }
        Rc::new(out)
    })
}

/// Every supertype of the types a file imports from or declares itself inside, package objects included.
fn scala_imported_supertypes(file: &str, imports: &ScalaImports, ctx: &Ctx) -> Rc<HashSet<String>> {
    memoized::<ScalaImportedSupersMemo>(ctx, file.to_string(), || {
        let mut seen: HashSet<String> = HashSet::new();
        let source = ctx.read_file(file).map(|s| s.to_string()).unwrap_or_default();
        let packages = re!(r"^\s*package\s+([\w.]+)\s*$", "gm").captures_iter(&source).flat_map(|m| m.text(1).unwrap_or("").split('.').map(String::from).collect::<Vec<_>>());
        let mut queue: VecDeque<String> = imports.owners.iter().cloned().chain(packages).collect();
        let package_objects = scala_package_objects(ctx);
        while let Some(type_name) = queue.pop_front() {
            if seen.len() >= 120 {
                break;
            }
            let supers: Vec<String> = scala_supertypes_of(&type_name, ctx).iter().cloned().chain(package_objects.get(&type_name).cloned().unwrap_or_default()).collect();
            for sup in supers {
                if seen.insert(sup.clone()) {
                    queue.push_back(sup);
                }
            }
        }
        Rc::new(seen)
    })
}

fn scala_imports_of(file: &str, ctx: &Ctx) -> Rc<ScalaImports> {
    memoized::<ScalaImportsMemo>(ctx, file.to_string(), || {
        let mut found = ScalaImports::default();
        let source = ctx.read_file(file).map(|s| s.to_string()).unwrap_or_default();
        for m in re!(r"^\s*import\s+([\w.]+?)\.(?:(_|\*)|\{([^}]*)\}|([\w$]+))\s*$", "gm").captures_iter(&source) {
            let path = m.text(1).unwrap_or("");
            let owner = path.rsplit('.').next().unwrap_or("").to_string();
            let root = path.split('.').next().unwrap_or("");
            // `import conf._` where `conf` is a `val` / parameter of the file: a value, not an object.
            if re!(r"^[a-z]").is_match(root)
                && (root == path || Re::cached(&format!(r"\b(?:val|var|lazy\s+val)\s+{root}\b|[(,]\s*{root}\s*:"), "").is_match(&source))
            {
                found.values.insert(owner.clone());
            }
            if m.text(2).is_some() {
                if !found.owners.contains(&owner) {
                    found.owners.push(owner);
                }
            } else {
                for member in m.text(3).or(m.text(4)).unwrap_or("").split(',') {
                    let member = member.trim_matches(codegraph_types::jscompat::is_js_space);
                    let id = re!(r"\s*=>\s*").split(member).into_iter().next().unwrap_or("");
                    if id == "_" || id == "*" {
                        if !found.owners.contains(&owner) {
                            found.owners.push(owner.clone());
                        }
                    } else if !id.is_empty() {
                        found.members.insert(format!("{owner}.{id}"));
                    }
                }
            }
        }
        Rc::new(found)
    })
}

fn scala_package_clauses(text: &str) -> Vec<String> {
    re!(r"^\s*package\s+(?!object\b)([\w.]+)\s*$", "gm").captures_iter(text).flat_map(|m| m.text(1).unwrap_or("").split('.').map(String::from).collect::<Vec<_>>()).collect()
}

/// The package a file's `package object` makes members of (`a.b.pkg`), or none.
fn scala_package_object_package(file: &str, ctx: &Ctx) -> Option<String> {
    memoized::<ScalaObjectPackageMemo>(ctx, file.to_string(), || {
        let text = ctx.read_file(file).map(|s| s.to_string()).unwrap_or_default();
        let object = re!(r"^\s*package\s+object\s+([\w$]+)", "m").captures(&text).and_then(|c| c.text(1).map(String::from))?;
        let mut parts = scala_package_clauses(&text);
        parts.push(object);
        Some(parts.join("."))
    })
}

/// A package-object member is visible from its package (and below), or from a file importing the package.
pub fn is_scala_package_object_member_visible(candidate: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let Some(object_pkg) = scala_package_object_package(&candidate.file_path, ctx) else { return true };
    let text = ctx.read_file(&r.file_path).map(|s| s.to_string()).unwrap_or_default();
    let here = scala_package_clauses(&text).join(".");
    if here == object_pkg || here.starts_with(&format!("{object_pkg}.")) {
        return true;
    }
    let last = object_pkg.rsplit('.').next().unwrap_or("");
    Re::cached(&format!(r"^\s*import\s+[^\n]*\b{}\b", escape_regex(last)), "m").is_match(&text)
}

/// The Scala contribution to [`super::lang_shape::LangShape`]: a bare call.
pub struct ScalaShape;

impl ScalaShape {
    pub fn of(r: &UnresolvedRef) -> Option<ScalaShape> {
        (r.language == Language::Scala && is_calls(r) && re!(r"^[A-Za-z_$][\w$]*$").is_match(&r.reference_name)).then_some(ScalaShape)
    }
}

impl RefShape for ScalaShape {
    fn excludes(&self, n: &NodeRef, r: &UnresolvedRef, ctx: &Ctx) -> bool {
        !is_scala_member_in_scope(n, r, ctx)
    }
}
