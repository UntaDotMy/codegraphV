//! Python receiver, module-global and local-binding analysis — the Python region of
//! `src/resolution/name-matcher.ts` (`pythonRefClass` … `pythonFieldType`).

use super::same_language_family;
use crate::ctx::Ctx;
use crate::import_resolver::resolve_import;
use crate::types::{ImportMapping, NodeRef, Tri, UnresolvedRef};
use codegraph_extract::strip_comments::strip_comments_for_regex;
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_search::query_utils::is_test_path;
use codegraph_types::jscompat::js_trim;
use codegraph_types::{Language, Node, NodeKind, ReferenceKind};
use indexmap::{IndexMap, IndexSet};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

// -- Memos ---------------------------------------------------------------------

#[derive(Default)]
struct MemberLinesMemo(RefCell<HashMap<String, Rc<Vec<String>>>>);
#[derive(Default)]
struct StatementStartsMemo(RefCell<HashMap<String, Rc<Vec<bool>>>>);
#[derive(Default)]
struct ImportKeysMemo(RefCell<HashMap<String, Rc<HashSet<String>>>>);
#[derive(Default)]
struct GlobalBindingsMemo(RefCell<HashMap<String, Rc<Vec<PythonBinding>>>>);
#[derive(Default)]
struct OwnWritesMemo(RefCell<HashMap<String, Option<Rc<Vec<PythonWrite>>>>>);
#[derive(Default)]
struct ExternalWritesMemo(RefCell<HashMap<String, Rc<ExternalWrites>>>);
#[derive(Default)]
struct MainBlockMemo(RefCell<HashMap<String, Rc<HashSet<usize>>>>);
#[derive(Default)]
struct GlobalNodeMemo(RefCell<HashMap<String, Option<NodeRef>>>);
#[derive(Default)]
struct GlobalClassesMemo(RefCell<HashMap<String, Option<Rc<Vec<NodeRef>>>>>);
#[derive(Default)]
struct ImportedFilesMemo(RefCell<HashMap<String, Rc<Vec<String>>>>);
#[derive(Default)]
struct WriteCandidatesMemo(RefCell<Option<Rc<WriteIndex>>>);

struct WriteIndex {
    files: Vec<String>,
    spelled: HashMap<String, Vec<usize>>,
    dict: Vec<(usize, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PythonBinding {
    Global,
    Assign { ty: Option<String>, value: String, line: usize },
    Import { key: String },
    Other,
}

#[derive(Debug, Clone, PartialEq)]
struct PythonWrite {
    ty: String,
    file: String,
}

#[derive(Debug, Default)]
struct ExternalWrites {
    writes: Vec<PythonWrite>,
    unknown: bool,
    writers: HashSet<String>,
}

fn with_file(r: &UnresolvedRef, file_path: &str) -> UnresolvedRef {
    let mut c = r.clone();
    c.file_path = file_path.to_string();
    c
}

fn js_char_at(chars: &[char], i: isize) -> Option<char> {
    if i < 0 {
        None
    } else {
        chars.get(i as usize).copied()
    }
}

/// `str.slice(start, end)` over a char vector.
fn char_slice(chars: &[char], start: usize, end: usize) -> String {
    let end = end.min(chars.len());
    if start >= end {
        String::new()
    } else {
        chars[start..end].iter().collect()
    }
}

// -- Classes and members --------------------------------------------------------

/// The class a (possibly module-qualified) name written in `r`'s file denotes.
pub fn python_ref_class(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    let imports = ctx.get_import_mappings(&r.file_path, Language::Python);
    let mut name = name.to_string();
    let module = imports.iter().find(|i| {
        i.is_namespace
            && name.starts_with(&format!("{}.", i.source))
            && re!(r"^\w+$").is_match(&name[i.source.len() + 1..])
    });
    if let Some(module) = module {
        if imports.iter().filter(|i| i.local_name == module.local_name).count() != 1 {
            return None;
        }
        name = format!("{}.{}", module.local_name, &name[module.source.len() + 1..]);
    }
    let head = name.split('.').next().unwrap_or("");
    if imports.iter().any(|i| i.local_name == head) {
        let mut probe = r.clone();
        probe.reference_name = name.clone();
        probe.reference_kind = ReferenceKind::Edge(codegraph_types::EdgeKind::References);
        let hit = resolve_import(&probe, ctx);
        let node = hit.and_then(|h| ctx.node_by_id(&h.target_node_id))?;
        if node.kind == NodeKind::Class
            && ctx
                .get_nodes_by_qualified_name(&node.qualified_name)
                .iter()
                .filter(|n| n.kind == NodeKind::Class && n.file_path == node.file_path)
                .count()
                == 1
        {
            return Some(node);
        }
        return None;
    }
    let classes: Vec<NodeRef> = ctx
        .get_nodes_by_name(&name)
        .iter()
        .filter(|n| n.kind == NodeKind::Class && n.file_path == r.file_path)
        .cloned()
        .collect();
    if classes.len() == 1 {
        Some(classes[0].clone())
    } else {
        None
    }
}

/// The base classes a class statement lists, as far as they resolve.
pub fn python_bases(cls: &Node, r: &UnresolvedRef, ctx: &Ctx) -> Vec<NodeRef> {
    let idx = (cls.start_line as usize).wrapping_sub(1);
    let line: String = ctx
        .get_file_lines(&cls.file_path)
        .and_then(|l| l.get(idx).cloned())
        .or_else(|| ctx.read_file(&cls.file_path).and_then(|c| c.split('\n').nth(idx).map(|s| s.to_string())))
        .unwrap_or_default();
    let Some(bases) = re!(r"^\s*class\s+\w+\s*\(([^)]*)\)").captures(&line).and_then(|c| c.text(1).map(|s| s.to_string())) else {
        return Vec::new();
    };
    let at = with_file(r, &cls.file_path);
    bases.split(',').filter_map(|name| python_ref_class(js_trim(name), &at, ctx)).collect()
}

pub fn python_derives_from(cls: &Node, base: &Node, r: &UnresolvedRef, ctx: &Ctx, seen: &mut HashSet<String>) -> bool {
    if seen.contains(&cls.id) || seen.len() >= 16 {
        return false;
    }
    seen.insert(cls.id.clone());
    python_bases(cls, r, ctx).iter().any(|p| p.id == base.id || python_derives_from(p, base, r, ctx, seen))
}

/// The declarations of `member` a class has or inherits.
pub fn python_members(cls: &NodeRef, member: &str, r: &UnresolvedRef, ctx: &Ctx, seen: &mut HashSet<String>) -> Vec<NodeRef> {
    if seen.contains(&cls.id) || seen.len() >= 16 {
        return Vec::new();
    }
    seen.insert(cls.id.clone());
    let lines = python_member_lines(&cls.file_path, ctx);
    let from = (cls.start_line as usize).saturating_sub(1).min(lines.len());
    let to = (cls.end_line as usize).min(lines.len());
    let body = if from < to { lines[from..to].join("\n") } else { String::new() };
    if Re::cached(&format!(r"^\s*(?:(?:self|cls)\.)?{}\s*(?:=|:)", escape_regex(member)), "m").is_match(&body) {
        return vec![cls.clone()];
    }
    let own: Vec<NodeRef> = ctx
        .get_nodes_by_qualified_name(&format!("{}::{member}", cls.qualified_name))
        .iter()
        .filter(|n| n.file_path == cls.file_path)
        .cloned()
        .collect();
    if !own.is_empty() {
        return own;
    }
    let mut unique: IndexMap<String, NodeRef> = IndexMap::new();
    for p in python_bases(cls, r, ctx) {
        for n in python_members(&p, member, r, ctx, seen) {
            unique.insert(n.id.clone(), n);
        }
    }
    unique.into_values().collect()
}

pub fn is_python_property(node: &Node, ctx: &Ctx) -> bool {
    if node.language != Language::Python || node.kind != NodeKind::Method {
        return false;
    }
    let lines: Vec<String> = match ctx.get_file_lines(&node.file_path) {
        Some(l) => l.as_ref().clone(),
        None => ctx.read_file(&node.file_path).map(|c| c.split('\n').map(|s| s.to_string()).collect()).unwrap_or_default(),
    };
    let mut i = node.start_line as isize - 2;
    while i >= 0 {
        let Some(line) = lines.get(i as usize) else { break };
        if !js_trim(line).starts_with('@') {
            break;
        }
        if re!(r"^\s*@(?:property|(?:functools\.)?cached_property)\s*$").is_match(line) {
            return true;
        }
        i -= 1;
    }
    false
}

/// The file's lines with comments blanked (Python), memoized.
pub fn python_member_lines(file_path: &str, ctx: &Ctx) -> Rc<Vec<String>> {
    let memo = ctx.memo::<MemberLinesMemo>();
    if let Some(l) = memo.0.borrow().get(file_path) {
        return l.clone();
    }
    let source = ctx.read_file(file_path).map(|s| s.to_string()).unwrap_or_default();
    let lines: Rc<Vec<String>> = Rc::new(strip_comments_for_regex(&source, "python").split('\n').map(|s| s.to_string()).collect());
    memo.0.borrow_mut().insert(file_path.to_string(), lines.clone());
    lines
}

/// The type a local name is bound to by annotation or construction, searching up from the reference.
pub fn python_local_type(receiver: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    if !re!(r"^\w+$").is_match(receiver) {
        return None;
    }
    let caller = ctx.node_by_id(&r.from_node_id);
    let lines = python_member_lines(&r.file_path, ctx);
    let declaration = Re::cached(&format!(r#"^\s*{receiver}\s*(?::\s*["']?([\w.]+)["']?)?\s*=\s*(.*)$"#), "");
    let annotation = Re::cached(&format!(r#"^\s*{receiver}\s*:\s*["']?([\w.]+)"#), "");
    let floor = caller.as_ref().map_or(1, |c| c.start_line) as isize - 1;
    let mut i = r.line as isize - 1;
    while i >= floor {
        let line = lines.get(i as usize).map(|s| s.as_str()).unwrap_or("");
        if let Some(assigned) = declaration.captures(line) {
            if let Some(t) = assigned.text(1) {
                return Some(t.to_string());
            }
            let rhs = assigned.text(2).unwrap_or("");
            return Some(
                re!(r"^([A-Z][\w.]*)\s*\(")
                    .captures(rhs)
                    .and_then(|c| c.text(1).map(|s| s.to_string()))
                    .unwrap_or_else(|| "<unknown>".to_string()),
            );
        }
        if let Some(declared) = annotation.captures(line).and_then(|c| c.text(1)) {
            return Some(declared.to_string());
        }
        i -= 1;
    }
    caller
        .and_then(|c| c.signature.clone())
        .and_then(|sig| {
            Re::cached(&format!(r#"\b{receiver}\s*:\s*["']?([\w.]+)"#), "").captures(&sig).and_then(|c| c.text(1).map(|s| s.to_string()))
        })
}

pub fn is_python_module_global(node: &Node, ctx: &Ctx) -> bool {
    node.language == Language::Python
        && matches!(node.kind, NodeKind::Variable | NodeKind::Constant)
        && !ctx.get_nodes_in_file(&node.file_path).iter().any(|n| {
            matches!(n.kind, NodeKind::Class | NodeKind::Function | NodeKind::Method)
                && n.start_line <= node.start_line
                && n.end_line >= node.start_line
        })
}

// -- Statement structure ----------------------------------------------------------

/// Which lines begin a statement (not inside brackets or a continuation).
pub fn python_statement_starts(file_path: &str, ctx: &Ctx) -> Rc<Vec<bool>> {
    let memo = ctx.memo::<StatementStartsMemo>();
    if let Some(s) = memo.0.borrow().get(file_path) {
        return s.clone();
    }
    let mut starts: Vec<bool> = Vec::new();
    let mut depth: i64 = 0;
    let mut continued = false;
    for line in python_member_lines(file_path, ctx).iter() {
        starts.push(depth == 0 && !continued);
        let chars: Vec<char> = line.chars().collect();
        let mut quote: Option<char> = None;
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if let Some(q) = quote {
                if c == '\\' {
                    i += 1;
                } else if c == q {
                    quote = None;
                }
                i += 1;
                continue;
            }
            if c == '"' || c == '\'' {
                quote = Some(c);
            } else if matches!(c, '(' | '[' | '{') {
                depth += 1;
            } else if matches!(c, ')' | ']' | '}') {
                depth = (depth - 1).max(0);
            }
            i += 1;
        }
        continued = re!(r"\\\s*$").is_match(line);
    }
    let starts = Rc::new(starts);
    memo.0.borrow_mut().insert(file_path.to_string(), starts.clone());
    starts
}

/// Zero-based line indexes in `scope` (or the module) not inside a nested function or class.
pub fn python_own_lines(scope: Option<&Node>, file_path: &str, ctx: &Ctx) -> Vec<usize> {
    let count = python_member_lines(file_path, ctx).len() as i64;
    let from = scope.map_or(1, |s| s.start_line as i64);
    let to = scope.map_or(count, |s| (s.end_line as i64).min(count));
    let size = (to - from + 1).max(0) as usize;
    let mut nested = vec![false; size];
    for n in ctx.get_nodes_in_file(file_path).iter() {
        if !matches!(n.kind, NodeKind::Function | NodeKind::Method | NodeKind::Class) || Some(&n.id) == scope.map(|s| &s.id) {
            continue;
        }
        let (ns, ne) = (n.start_line as i64, n.end_line as i64);
        if ns < from || ne > to || scope.is_some_and(|s| ns <= s.start_line as i64) {
            continue;
        }
        let a = (ns - from).max(0) as usize;
        let b = ((ne - from + 1).max(0) as usize).min(size);
        for slot in nested.iter_mut().take(b).skip(a) {
            *slot = true;
        }
    }
    let mut own = Vec::new();
    let mut l = from;
    while l <= to {
        if !nested[(l - from) as usize] {
            own.push((l - 1) as usize);
        }
        l += 1;
    }
    own
}

pub struct PythonAssignment {
    pub targets: Vec<String>,
    pub value: String,
    pub augmented: bool,
}

/// Split `a = b = value` (and augmented forms) at the top-level `=` signs.
pub fn python_assignment(line: &str) -> Option<PythonAssignment> {
    let chars: Vec<char> = line.chars().collect();
    let mut targets: Vec<String> = Vec::new();
    let mut depth: i64 = 0;
    let mut quote: Option<char> = None;
    let mut start = 0usize;
    let mut augmented = false;
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            if c == '\\' {
                i += 1;
            } else if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if c == '"' || c == '\'' {
            quote = Some(c);
            i += 1;
            continue;
        }
        if matches!(c, '(' | '[' | '{') {
            depth += 1;
            i += 1;
            continue;
        }
        if matches!(c, ')' | ']' | '}') {
            depth -= 1;
            i += 1;
            continue;
        }
        if c != '=' || depth != 0 {
            i += 1;
            continue;
        }
        let prev = js_char_at(&chars, i as isize - 1).unwrap_or('\0');
        let has_prev = i > 0;
        if chars.get(i + 1) == Some(&'=') {
            i += 2;
            continue;
        }
        if has_prev && (prev == '!' || prev == ':') {
            i += 1;
            continue;
        }
        if has_prev && (prev == '<' || prev == '>') && js_char_at(&chars, i as isize - 2) != Some(prev) {
            i += 1;
            continue;
        }
        let before = char_slice(&chars, start, i);
        let op = re!(r"(?:\/\/|\*\*|>>|<<|[-+*/%&|^@])$").find(&before).map(|m| m.as_str().chars().count());
        if op.is_some() {
            augmented = true;
        }
        let keep = before.chars().count() - op.unwrap_or(0);
        targets.push(before.chars().take(keep).collect());
        start = i + 1;
        i += 1;
    }
    if targets.is_empty() {
        None
    } else {
        Some(PythonAssignment { targets, value: char_slice(&chars, start, chars.len()), augmented })
    }
}

/// How a statement binds `name`: `import`, assignment, `global`, or some other way.
pub fn python_line_bindings(lines: &[String], index: usize, statements: &[bool], name: &str) -> Vec<PythonBinding> {
    let mut line = lines[index].clone();
    if statements[index] {
        let mut j = index + 1;
        while j < lines.len() && !statements[j] {
            line.push('\n');
            line.push_str(&lines[j]);
            j += 1;
        }
    }
    let mut out: Vec<PythonBinding> = Vec::new();

    let imported = if statements[index] { re!(r"^\s*(?:from\s+([\w.]+)\s+)?import\s+([\s\S]*)$").captures(&line) } else { None };
    if let Some(imported) = imported {
        let from = imported.text(1);
        let names = imported.text(2).unwrap_or("");
        if from.is_some() && js_trim(names) == "*" {
            return vec![PythonBinding::Other];
        }
        if !names.contains(name) {
            return out;
        }
        let cleaned = re!(r"[()\\]").replace_all_str(names, " ");
        for part in cleaned.split(',') {
            let Some(m) = re!(r"^([\w.]+)(?:\s+as\s+(\w+))?$").captures(js_trim(part)) else { continue };
            let m1 = m.text(1).unwrap_or("");
            let m2 = m.text(2);
            let local: String = match m2 {
                Some(a) => a.to_string(),
                None => {
                    if from.is_some() {
                        m1.to_string()
                    } else {
                        m1.split('.').next().unwrap_or("").to_string()
                    }
                }
            };
            if local != name {
                continue;
            }
            let key = match from {
                Some(f) => format!("{f}:{m1}"),
                None => format!("{}:*", if m2.is_some() { m1 } else { local.as_str() }),
            };
            out.push(PythonBinding::Import { key });
        }
        return out;
    }
    if !line.contains(name) {
        return out;
    }
    let esc = escape_regex(name);
    let word = Re::cached(&format!(r"(?<![\w.]){esc}\b(?!\s*[.\[])"), "");
    if !word.is_match(&line) {
        return out;
    }
    if let Some(declared) = re!(r"^\s*(global|nonlocal)\s+([\w\s,]+)$").captures(&line) {
        let names = declared.text(2).unwrap_or("");
        return if names.split(',').any(|s| js_trim(s) == name) {
            vec![if declared.text(1) == Some("global") { PythonBinding::Global } else { PythonBinding::Other }]
        } else {
            Vec::new()
        };
    }
    if statements[index] {
        if re!(r"^\s*case\b").is_match(&line) {
            return vec![PythonBinding::Other];
        }
        if let Some(assignment) = python_assignment(&line) {
            let target = if assignment.targets.len() == 1 && !assignment.augmented { js_trim(&assignment.targets[0]).to_string() } else { String::new() };
            let annotated = Re::cached(&format!(r#"^{esc}\s*:\s*["']?([\w.]+)["']?$"#), "").captures(&target).and_then(|c| c.text(1).map(|s| s.to_string()));
            if target == name || annotated.is_some() {
                out.push(PythonBinding::Assign { ty: annotated, value: js_trim(&assignment.value).to_string(), line: index });
            } else if assignment.targets.iter().any(|t| word.is_match(t)) {
                out.push(PythonBinding::Other);
            }
        } else {
            let annotated = Re::cached(&format!(r#"^\s*{esc}\s*:\s*["']?([\w.]+)["']?\s*$"#), "")
                .captures(&line)
                .and_then(|c| c.text(1).map(|s| s.to_string()));
            if let Some(a) = annotated {
                out.push(PythonBinding::Assign { ty: Some(a), value: String::new(), line: index });
            } else if re!(r"^\s*del\b").is_match(&line) {
                out.push(PythonBinding::Other);
            }
        }
    }
    if Re::cached(&format!(r"\b{esc}[ \t]*:=|\bas[ \t]+{esc}\b"), "").is_match(&line) {
        out.push(PythonBinding::Other);
    }
    for loop_ in re!(r"\bfor\s+([^:]+?)\s+in\b", "g").captures_iter(&line) {
        if word.is_match(loop_.text(1).unwrap_or("")) {
            out.push(PythonBinding::Other);
        }
    }
    for lambda in re!(r"\blambda\b([^:]*):", "g").captures_iter(&line) {
        if word.is_match(lambda.text(1).unwrap_or("")) {
            out.push(PythonBinding::Other);
        }
    }
    out
}

/// Is `name` bound by the function(s) enclosing the reference (parameter or local)?
pub fn python_binds_locally(name: &str, r: &UnresolvedRef, ctx: &Ctx, imports_bind: bool) -> bool {
    let lines = python_member_lines(&r.file_path, ctx);
    let statements = python_statement_starts(&r.file_path, ctx);
    let param = Re::cached(&format!(r"[(,]\s*\*{{0,2}}{}\s*[:=,)]", escape_regex(name)), "");
    let mut scopes: Vec<NodeRef> = ctx
        .get_nodes_in_file(&r.file_path)
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method) && n.start_line <= r.line && n.end_line >= r.line)
        .cloned()
        .collect();
    scopes.sort_by(|a, b| b.start_line.cmp(&a.start_line));
    for scope in &scopes {
        let own = python_own_lines(Some(scope), &r.file_path, ctx);
        let bindings: Vec<PythonBinding> = own.iter().flat_map(|&i| python_line_bindings(&lines, i, &statements, name)).collect();
        if bindings.iter().any(|b| matches!(b, PythonBinding::Global)) {
            return false;
        }
        let def = own.iter().copied().find(|&i| re!(r"^\s*(?:async\s+)?def\b").is_match(&lines[i]));
        let mut header = scope.signature.clone().unwrap_or_default();
        let mut j = def.unwrap_or(lines.len());
        while j < lines.len() && (Some(j) == def || !statements[j]) {
            header.push_str(&lines[j]);
            j += 1;
        }
        if param.is_match(&header) {
            return true;
        }
        if bindings.iter().any(|b| !matches!(b, PythonBinding::Global) && (!matches!(b, PythonBinding::Import { .. }) || imports_bind)) {
            return true;
        }
    }
    false
}

/// The module a function-local `import x` / `from m import x` binds `name` to, when it is
/// one dotted module.
pub fn python_function_module_alias(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    if r.language != Language::Python || !re!(r"^\w+$").is_match(name) {
        return None;
    }
    let lines = python_member_lines(&r.file_path, ctx);
    let statements = python_statement_starts(&r.file_path, ctx);
    let mut scopes: Vec<NodeRef> = ctx
        .get_nodes_in_file(&r.file_path)
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method) && n.start_line <= r.line && n.end_line >= r.line)
        .cloned()
        .collect();
    scopes.sort_by(|a, b| b.start_line.cmp(&a.start_line));
    for scope in &scopes {
        let bindings: Vec<PythonBinding> =
            python_own_lines(Some(scope), &r.file_path, ctx).iter().flat_map(|&i| python_line_bindings(&lines, i, &statements, name)).collect();
        if bindings.is_empty() {
            continue;
        }
        let modules: IndexSet<String> = bindings
            .iter()
            .map(|b| match b {
                PythonBinding::Import { key } if key.ends_with(":*") => key[..key.len() - 2].to_string(),
                _ => String::new(),
            })
            .collect();
        let module = modules.iter().next().cloned().unwrap_or_default();
        return if modules.len() == 1 && !module.is_empty() && module.contains('.') && !python_binds_locally(name, r, ctx, false) {
            Some(module)
        } else {
            None
        };
    }
    None
}

// -- Module-level scans --------------------------------------------------------------

fn scan_python_import_keys(name: &str, file_path: &str, ctx: &Ctx) -> HashSet<String> {
    let lines = python_member_lines(file_path, ctx);
    let statements = python_statement_starts(file_path, ctx);
    let mut keys = HashSet::new();
    for i in 0..lines.len() {
        if !statements[i] || !re!(r"\bimport\b").is_match(&lines[i]) {
            continue;
        }
        for b in python_line_bindings(&lines, i, &statements, name) {
            if let PythonBinding::Import { key } = b {
                keys.insert(key);
            }
        }
    }
    keys
}

/// The distinct module-qualified keys `name` is imported under in the file.
pub fn python_import_keys(name: &str, file_path: &str, ctx: &Ctx) -> Rc<HashSet<String>> {
    let memo = ctx.memo::<ImportKeysMemo>();
    let key = format!("{file_path}\0{name}");
    if let Some(k) = memo.0.borrow().get(&key) {
        return k.clone();
    }
    let keys = Rc::new(scan_python_import_keys(name, file_path, ctx));
    memo.0.borrow_mut().insert(key, keys.clone());
    keys
}

fn scan_python_global_bindings(name: &str, file_path: &str, ctx: &Ctx) -> Vec<PythonBinding> {
    let lines = python_member_lines(file_path, ctx);
    let statements = python_statement_starts(file_path, ctx);
    let scopes: Vec<NodeRef> = ctx
        .get_nodes_in_file(file_path)
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Class | NodeKind::Function | NodeKind::Method))
        .cloned()
        .collect();
    let mut regions: Vec<Vec<usize>> = vec![python_own_lines(None, file_path, ctx)];
    for i in 0..lines.len() {
        if !re!(r"^\s*global\b").is_match(&lines[i]) || python_line_bindings(&lines, i, &statements, name).is_empty() {
            continue;
        }
        let mut here: Vec<&NodeRef> = scopes.iter().filter(|n| n.start_line as usize <= i + 1 && n.end_line as usize >= i + 1).collect();
        here.sort_by(|a, b| b.start_line.cmp(&a.start_line));
        if let Some(scope) = here.first() {
            if scope.kind != NodeKind::Class {
                regions.push(python_own_lines(Some(scope), file_path, ctx));
            }
        }
    }
    let script = python_main_block_lines(file_path, ctx);
    regions
        .iter()
        .flat_map(|region| region.iter().filter(|i| !script.contains(i)).flat_map(|&i| python_line_bindings(&lines, i, &statements, name)).collect::<Vec<_>>())
        .filter(|b| !matches!(b, PythonBinding::Global))
        .collect()
}

pub fn python_global_bindings(name: &str, file_path: &str, ctx: &Ctx) -> Rc<Vec<PythonBinding>> {
    let memo = ctx.memo::<GlobalBindingsMemo>();
    let key = format!("{file_path}\0{name}");
    if let Some(b) = memo.0.borrow().get(&key) {
        return b.clone();
    }
    let bindings = Rc::new(scan_python_global_bindings(name, file_path, ctx));
    memo.0.borrow_mut().insert(key, bindings.clone());
    bindings
}

fn python_main_block_lines(file_path: &str, ctx: &Ctx) -> Rc<HashSet<usize>> {
    let memo = ctx.memo::<MainBlockMemo>();
    if let Some(m) = memo.0.borrow().get(file_path) {
        return m.clone();
    }
    let lines = python_member_lines(file_path, ctx);
    let mut inside: HashSet<usize> = HashSet::new();
    let guard_re = re!(r#"^if\s+(?:__name__\s*==\s*(['"])__main__\1|(['"])__main__\2\s*==\s*__name__)\s*:"#);
    for i in 0..lines.len() {
        let Some(guard) = guard_re.find(&lines[i]) else { continue };
        if !js_trim(&lines[i][guard.end()..]).is_empty() {
            inside.insert(i);
        }
        let mut j = i + 1;
        while j < lines.len() && !re!(r"^\S").is_match(&lines[j]) {
            inside.insert(j);
            j += 1;
        }
    }
    let inside = Rc::new(inside);
    memo.0.borrow_mut().insert(file_path.to_string(), inside.clone());
    inside
}

/// Replace the contents of string literals with spaces (keeping the quotes).
pub fn blank_python_strings(text: &str) -> String {
    re!(r#"(['"])(?:\\.|(?!\1)[^\\\n])*\1"#, "g").replace_all(text, |caps| {
        let whole = caps.text(0)?;
        let n = whole.chars().count();
        let q = whole.chars().next()?;
        Some(format!("{q}{}{q}", " ".repeat(n.saturating_sub(2))))
    })
}

fn is_python_test_file(file_path: &str) -> bool {
    is_test_path(file_path) || re!(r"(?:^|/)conftest\.py$").is_match(&file_path.replace('\\', "/"))
}

/// `Foo(...)` / `pkg.Foo(...)` — the class a constructor call names — when `text` is exactly that call.
pub fn python_constructor_call(text: &str) -> Option<String> {
    let mut text = text.to_string();
    // Peel redundant outer parentheses.
    loop {
        let wrapped = js_trim(&text).to_string();
        if !wrapped.starts_with('(') {
            break;
        }
        let chars: Vec<char> = wrapped.chars().collect();
        let mut depth = 0i64;
        let mut close: Option<usize> = None;
        let mut i = 0;
        while i < chars.len() && close.is_none() {
            if chars[i] == '(' {
                depth += 1;
            } else if chars[i] == ')' {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                }
            }
            i += 1;
        }
        if close != Some(chars.len() - 1) {
            break;
        }
        text = js_trim(&char_slice(&chars, 1, chars.len() - 1)).to_string();
    }
    let callee = re!(r"^((?:[A-Za-z_]\w*\.)*[A-Z]\w*)\s*\(").captures(&text)?;
    let callee_len = callee.text(0)?.chars().count();
    let callee_name = callee.text(1)?.to_string();
    let chars: Vec<char> = text.chars().collect();
    let mut depth = 0i64;
    let mut quote: Option<char> = None;
    let mut i = callee_len - 1;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            if c == '\\' {
                i += 1;
            } else if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if c == '"' || c == '\'' {
            quote = Some(c);
        } else if matches!(c, '(' | '[' | '{') {
            depth += 1;
        } else if matches!(c, ')' | ']' | '}') {
            depth -= 1;
            if depth == 0 {
                return if re!(r"^[\s\\]*$").is_match(&char_slice(&chars, i + 1, chars.len())) { Some(callee_name) } else { None };
            }
        }
        i += 1;
    }
    None
}

// -- Module-global writes ----------------------------------------------------------------

fn python_dynamic_global_write(name: &str, file_path: &str, ctx: &Ctx) -> bool {
    let lines = python_member_lines(file_path, ctx);
    let statements = python_statement_starts(file_path, ctx);
    let script = python_main_block_lines(file_path, ctx);
    let module_lines: HashSet<usize> = python_own_lines(None, file_path, ctx).into_iter().collect();
    for i in 0..lines.len() {
        if !statements[i] || script.contains(&i) {
            continue;
        }
        let mut statement = lines[i].clone();
        let mut j = i + 1;
        while j < lines.len() && !statements[j] {
            statement.push('\n');
            statement.push_str(&lines[j]);
            j += 1;
        }
        let code = blank_python_strings(&statement);
        let namespace = if module_lines.contains(&i) {
            re!(r"\bglobals\(\s*\)|\bvars\(\s*\)|\bsys\.modules\s*\[\s*__name__\s*\]", "g")
        } else {
            re!(r"\bglobals\(\s*\)|\bsys\.modules\s*\[\s*__name__\s*\]", "g")
        };
        let uses = namespace.captures_iter(&code).count();
        if uses == 0 {
            continue;
        }
        let targets = python_assignment(&statement).map(|a| a.targets).unwrap_or_default();
        let mut safe = 0;
        for m in re!(r#"\bglobals\(\s*\)\s*(?:\[\s*(['"])(\w+)\1\s*\]|\.\s*get\s*\()"#, "g").captures_iter(&statement) {
            let key = m.text(2);
            let whole = m.text(0).unwrap_or("");
            let written = key.is_some() && targets.iter().any(|t| t.contains(whole));
            if written && key == Some(name) {
                return true;
            }
            safe += 1;
        }
        if uses > safe {
            return true;
        }
    }
    false
}

fn python_own_global_writes(global: &Node, ctx: &Ctx) -> Option<Rc<Vec<PythonWrite>>> {
    let memo = ctx.memo::<OwnWritesMemo>();
    if let Some(w) = memo.0.borrow().get(&global.id) {
        return w.clone();
    }
    let computed = (|| {
        let file = &global.file_path;
        if python_dynamic_global_write(&global.name, file, ctx) {
            return None;
        }
        let mut writes: Vec<PythonWrite> = Vec::new();
        for b in python_global_bindings(&global.name, file, ctx).iter() {
            let PythonBinding::Assign { ty, value, .. } = b else { return None };
            let constructor = if !value.is_empty() && value != "None" { python_constructor_call(value) } else { None };
            if let Some(ty) = ty {
                if let Some(c) = &constructor {
                    if c.rsplit('.').next() != ty.rsplit('.').next() {
                        return None;
                    }
                }
                writes.push(PythonWrite { ty: ty.clone(), file: file.clone() });
                continue;
            }
            if value == "None" {
                continue;
            }
            let c = constructor?;
            writes.push(PythonWrite { ty: c, file: file.clone() });
        }
        Some(Rc::new(writes))
    })();
    memo.0.borrow_mut().insert(global.id.clone(), computed.clone());
    computed
}

fn python_module_files(dotted: &str, from_file: &str, ctx: &Ctx) -> Vec<String> {
    let dots = dotted.chars().take_while(|&c| c == '.').count();
    let parts: Vec<&str> = dotted[dots..].split('.').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return Vec::new();
    }
    let mut dir = String::new();
    if dots > 0 {
        dir = codegraph_types::nodepath::posix::dirname(&from_file.replace('\\', "/"));
        for _ in 1..dots {
            dir = codegraph_types::nodepath::posix::dirname(&dir);
        }
        if dir == "." {
            dir.clear();
        }
    }
    let rel: String = std::iter::once(dir.as_str()).chain(parts.iter().copied()).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("/");
    let matches = |file: &str, want: &str| if dots > 0 { file == want } else { file == want || file.ends_with(&format!("/{want}")) };
    let last = parts[parts.len() - 1];
    let mut out = Vec::new();
    for name in [format!("{last}.py"), format!("{last}.pyi"), "__init__.py".to_string()] {
        for n in ctx.get_nodes_by_name(&name).iter() {
            if n.kind == NodeKind::File
                && (matches(&n.file_path, &format!("{rel}.py")) || matches(&n.file_path, &format!("{rel}.pyi")) || matches(&n.file_path, &format!("{rel}/__init__.py")))
            {
                out.push(n.file_path.clone());
            }
        }
    }
    out
}

fn python_imported_files(m: &ImportMapping, file_path: &str, ctx: &Ctx) -> Rc<Vec<String>> {
    let memo = ctx.memo::<ImportedFilesMemo>();
    let key = format!("{file_path}\0{}\0{}\0{}\0{}", m.local_name, m.source, m.exported_name, m.is_namespace);
    if let Some(f) = memo.0.borrow().get(&key) {
        return f.clone();
    }
    let dotted = if m.is_namespace {
        m.source.clone()
    } else if re!(r"^\.+$").is_match(&m.source) {
        format!("{}{}", m.source, m.exported_name)
    } else {
        format!("{}.{}", m.source, m.exported_name)
    };
    let files = Rc::new(python_module_files(&dotted, file_path, ctx));
    memo.0.borrow_mut().insert(key, files.clone());
    files
}

/// How `file_path` spells the module `module_file`: aliases that name it uniquely, and ones that are ambiguous.
fn python_module_aliases(file_path: &str, module_file: &str, ctx: &Ctx) -> (Vec<String>, Vec<String>) {
    let mut aliases: IndexSet<String> = IndexSet::new();
    let mut ambiguous: IndexSet<String> = IndexSet::new();
    for m in ctx.get_import_mappings(file_path, Language::Python).iter() {
        let files = python_imported_files(m, file_path, ctx);
        if !files.iter().any(|f| f == module_file) {
            continue;
        }
        let explicit = Re::cached(
            &format!(r"\bimport\s[^\n]*(?<![\w.]){}\s+as\s+{}\b", escape_regex(&m.source), escape_regex(&m.local_name)),
            "",
        );
        let plain_dotted = m.is_namespace
            && m.source.contains('.')
            && Some(m.local_name.as_str()) == m.source.split('.').next_back()
            && !python_member_lines(file_path, ctx).iter().any(|line| explicit.is_match(&blank_python_strings(line)));
        let name = if plain_dotted { m.source.clone() } else { m.local_name.clone() };
        if files.len() == 1 {
            aliases.insert(name);
        } else {
            ambiguous.insert(name);
        }
    }
    (aliases.into_iter().collect(), ambiguous.into_iter().collect())
}

fn python_write_candidates(name: &str, ctx: &Ctx) -> Vec<String> {
    let memo = ctx.memo::<WriteCandidatesMemo>();
    let index = {
        let existing = memo.0.borrow().clone();
        match existing {
            Some(i) => i,
            None => {
                let files: Vec<String> = ctx.get_all_files().into_iter().filter(|f| re!(r"\.pyi?$").is_match(f)).collect();
                let mut spelled: HashMap<String, Vec<usize>> = HashMap::new();
                let mut dict: Vec<(usize, String)> = Vec::new();
                for (at, file) in files.iter().enumerate() {
                    let source = ctx.read_file(file).map(|s| s.to_string()).unwrap_or_default();
                    let mut names: IndexSet<String> = IndexSet::new();
                    for m in re!(r#"[.'"](\w+)"#, "g").captures_iter(&source) {
                        if let Some(n) = m.text(1) {
                            names.insert(n.to_string());
                        }
                    }
                    for n in &names {
                        spelled.entry(n.clone()).or_default().push(at);
                    }
                    if names.contains("__dict__") {
                        let words: IndexSet<&str> = re!(r"\w+", "g").captures_iter(&source).filter_map(|c| c.text(0)).collect();
                        dict.push((at, words.into_iter().collect::<Vec<_>>().join("\n")));
                    }
                }
                let built = Rc::new(WriteIndex { files, spelled, dict });
                *memo.0.borrow_mut() = Some(built.clone());
                built
            }
        }
    };
    if !re!(r"^\w+$").is_match(name) {
        return index.files.iter().filter(|f| ctx.read_file(f).is_some_and(|c| c.contains(name))).cloned().collect();
    }
    let mut hits: HashSet<usize> = index.spelled.get(name).map(|v| v.iter().copied().collect()).unwrap_or_default();
    for (at, words) in &index.dict {
        if words.contains(name) {
            hits.insert(*at);
        }
    }
    let mut sorted: Vec<usize> = hits.into_iter().collect();
    sorted.sort_unstable();
    sorted.into_iter().map(|at| index.files[at].clone()).collect()
}

fn python_external_writes(global: &Node, ctx: &Ctx) -> Rc<ExternalWrites> {
    let memo = ctx.memo::<ExternalWritesMemo>();
    if let Some(w) = memo.0.borrow().get(&global.id) {
        return w.clone();
    }
    let mut out = ExternalWrites::default();
    let name = global.name.as_str();
    'files: for file in python_write_candidates(name, ctx) {
        if file == global.file_path {
            continue;
        }
        let test = is_python_test_file(&file);
        let (aliases, ambiguous) = python_module_aliases(&file, &global.file_path, ctx);
        if !test && aliases.is_empty() && ambiguous.is_empty() {
            continue;
        }
        let spell = |names: &[String]| format!("(?:{})", names.iter().map(|n| escape_regex(n)).collect::<Vec<_>>().join("|"));
        let all: Vec<String> = aliases.iter().chain(ambiguous.iter()).cloned().collect();
        let receiver = if test { r"[\w.]+".to_string() } else { spell(&all) };
        let esc_name = escape_regex(name);
        let target = Re::cached(&format!(r"(?<![\w.]){receiver}\.{esc_name}\b(?!\s*[.\[(])"), "");
        let exact_receiver = if test {
            receiver.clone()
        } else {
            spell(&if aliases.is_empty() { vec!["\\0".to_string()] } else { aliases.clone() })
        };
        let exact = Re::cached(&format!(r"^{exact_receiver}\.{esc_name}(?:\s*:[^=]*)?$"), "");
        let dynamic = Re::cached(
            &format!(
                r#"\b(?:setattr|patch\.object)\s*\(\s*{receiver}\s*,\s*['"]{esc_name}['"]|(?<![\w.]){receiver}\.__dict__\s*(?:\[|\.\s*update\s*\()"#
            ),
            "",
        );
        let lines = python_member_lines(&file, ctx);
        let statements = python_statement_starts(&file, ctx);
        let script = python_main_block_lines(&file, ctx);
        for i in 0..lines.len() {
            if !statements[i] || script.contains(&i) {
                continue;
            }
            let mut statement = lines[i].clone();
            let mut j = i + 1;
            while j < lines.len() && !statements[j] {
                statement.push('\n');
                statement.push_str(&lines[j]);
                j += 1;
            }
            if !statement.contains(name) {
                continue;
            }
            let assignment = python_assignment(&statement);
            let assigned = assignment.as_ref().is_some_and(|a| a.targets.iter().any(|t| target.is_match(js_trim(t))));
            if !assigned && !dynamic.is_match(&statement) {
                continue;
            }
            if test {
                out.writers.insert(file.clone());
                continue;
            }
            let single = assignment
                .as_ref()
                .is_some_and(|a| a.targets.len() == 1 && !a.augmented && exact.is_match(js_trim(&a.targets[0])));
            let value: String = if single { js_trim(&assignment.as_ref().unwrap().value).to_string() } else { String::new() };
            if value == "None" {
                continue;
            }
            let constructor = if value.is_empty() { None } else { python_constructor_call(&value) };
            match constructor {
                None => {
                    out.unknown = true;
                    break 'files;
                }
                Some(c) => out.writes.push(PythonWrite { ty: c, file: file.clone() }),
            }
        }
    }
    let out = Rc::new(out);
    memo.0.borrow_mut().insert(global.id.clone(), out.clone());
    out
}

fn python_global_classes(global: &Node, r: &UnresolvedRef, ctx: &Ctx) -> Option<Rc<Vec<NodeRef>>> {
    let memo = ctx.memo::<GlobalClassesMemo>();
    if let Some(c) = memo.0.borrow().get(&global.id) {
        return c.clone();
    }
    let own = python_own_global_writes(global, ctx);
    let external = own.as_ref().map(|_| python_external_writes(global, ctx));
    let writes: Option<Vec<PythonWrite>> = match (&external, &own) {
        (Some(ext), Some(own)) if !ext.unknown => Some(ext.writes.iter().chain(own.iter()).cloned().collect()),
        _ => None,
    };
    let mut classes: Option<Vec<NodeRef>> = writes.as_ref().map(|_| Vec::new());
    let mut seen: HashSet<String> = HashSet::new();
    for write in writes.unwrap_or_default() {
        match python_ref_class(&write.ty, &with_file(r, &write.file), ctx) {
            None => {
                classes = None;
                break;
            }
            Some(cls) => {
                if seen.insert(cls.id.clone()) {
                    classes.as_mut().unwrap().push(cls);
                }
            }
        }
    }
    let result = classes.map(Rc::new);
    memo.0.borrow_mut().insert(global.id.clone(), result.clone());
    result
}

/// The declaration of `member` that every class written to the module global provides.
pub fn python_global_members(global: &Node, member: &str, r: &UnresolvedRef, ctx: &Ctx) -> Vec<NodeRef> {
    if python_own_global_writes(global, ctx).is_none() {
        return Vec::new();
    }
    if python_external_writes(global, ctx).writers.contains(&r.file_path) {
        return Vec::new();
    }
    let Some(classes) = python_global_classes(global, r, ctx) else { return Vec::new() };
    let mut by_id: IndexMap<String, NodeRef> = IndexMap::new();
    for cls in classes.iter() {
        for n in python_members(cls, member, r, ctx, &mut HashSet::new()) {
            by_id.insert(n.id.clone(), n);
        }
    }
    let targets: Vec<NodeRef> = by_id.into_values().collect();
    if targets.len() <= 1 {
        return targets;
    }
    let owner = |n: &Node| -> Option<NodeRef> {
        ctx.get_nodes_in_file(&n.file_path)
            .iter()
            .find(|c| c.kind == NodeKind::Class && n.qualified_name == format!("{}::{member}", c.qualified_name))
            .cloned()
    };
    let mut declarations: IndexMap<String, (NodeRef, NodeRef)> = IndexMap::new();
    let mut queue: std::collections::VecDeque<NodeRef> = targets.iter().cloned().collect();
    while !queue.is_empty() && declarations.len() < 32 {
        let decl = queue.pop_front().unwrap();
        let Some(cls) = owner(&decl) else { continue };
        if declarations.contains_key(&decl.id) {
            continue;
        }
        declarations.insert(decl.id.clone(), (decl.clone(), cls.clone()));
        for base in python_bases(&cls, r, ctx) {
            queue.extend(python_members(&base, member, r, ctx, &mut HashSet::new()));
        }
    }
    let inherits = |cls: &Node, base: &Node| cls.id == base.id || python_derives_from(cls, base, r, ctx, &mut HashSet::new());
    let shared: Vec<&(NodeRef, NodeRef)> = declarations.values().filter(|d| classes.iter().all(|c| inherits(c, &d.1))).collect();
    let nearest: Vec<&&(NodeRef, NodeRef)> = shared.iter().filter(|d| shared.iter().all(|o| inherits(&d.1, &o.1))).collect();
    if nearest.len() == 1 {
        vec![nearest[0].0.clone()]
    } else {
        targets
    }
}

/// A method called through a module-level global (`logger.info()`): the member every class
/// the global can hold provides, when that is exactly one function or method.
pub fn python_global_receiver_target(receiver: &str, member: &str, r: &UnresolvedRef, ctx: &Ctx, call: bool) -> Tri<NodeRef> {
    let memo = ctx.memo::<GlobalNodeMemo>();
    let key = format!("{}\0{receiver}", r.file_path);
    let cached = memo.0.borrow().get(&key).cloned();
    let global = match cached {
        Some(g) => g,
        None => {
            let g = ctx.get_nodes_in_file(&r.file_path).iter().find(|n| n.name == receiver && is_python_module_global(n, ctx)).cloned();
            memo.0.borrow_mut().insert(key, g.clone());
            g
        }
    };
    let Some(global) = global else { return Tri::Undefined };
    if python_binds_locally(receiver, r, ctx, true) {
        return Tri::Undefined;
    }
    let nodes = ctx.get_nodes_in_file(&r.file_path);
    let scopes: Vec<&NodeRef> =
        nodes.iter().filter(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method) && n.start_line <= r.line && n.end_line >= r.line).collect();
    let innermost = |n: &Node| -> Option<&NodeRef> {
        nodes
            .iter()
            .filter(|c| c.id != n.id && matches!(c.kind, NodeKind::Function | NodeKind::Method | NodeKind::Class) && c.start_line < n.start_line && c.end_line >= n.start_line)
            .max_by(|a, b| a.start_line.cmp(&b.start_line).then(std::cmp::Ordering::Greater))
    };
    if nodes.iter().any(|n| {
        n.name == receiver
            && matches!(n.kind, NodeKind::Class | NodeKind::Function)
            && scopes.iter().any(|f| Some(&f.id) == innermost(n).map(|i| &i.id))
    }) {
        return Tri::Undefined;
    }
    if python_own_global_writes(&global, ctx).is_none() || python_external_writes(&global, ctx).writers.contains(&r.file_path) {
        return Tri::Null;
    }
    let Some(classes) = python_global_classes(&global, r, ctx) else { return Tri::Null };
    if classes.iter().any(|cls| python_members(cls, member, r, ctx, &mut HashSet::new()).is_empty()) {
        return Tri::Null;
    }
    let pool: Vec<NodeRef> = python_global_members(&global, member, r, ctx)
        .into_iter()
        .filter(|n| same_language_family(n.language, r.language))
        .collect();
    let target = if pool.len() == 1 { Some(pool[0].clone()) } else { None };
    match target {
        Some(t) if matches!(t.kind, NodeKind::Function | NodeKind::Method) && (call || t.id != r.from_node_id) && !is_python_property(&t, ctx) => Tri::Value(t),
        _ => Tri::Null,
    }
}

/// The annotated or constructed type of `self.field` within the owning class.
pub fn python_field_type(receiver: &str, owner: &Node, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let lines = python_member_lines(&r.file_path, ctx);
    let field = receiver.split('.').nth(1).unwrap_or("");
    let esc = escape_regex(field);
    let assignment = Re::cached(&format!(r#"^\s*(?:self|cls)\.{esc}\s*(?::\s*["']?([\w.]+)["']?)?\s*=\s*(.*)$"#), "");
    let annotation = Re::cached(&format!(r#"^\s*(?:(?:self|cls)\.)?{esc}\s*:\s*["']?([\w.]+)"#), "");
    let prefix = format!("{}::", owner.qualified_name);
    let methods: Vec<NodeRef> =
        ctx.get_nodes_in_file(&r.file_path).iter().filter(|n| n.kind == NodeKind::Method && n.qualified_name.starts_with(&prefix)).cloned().collect();
    let mut types: IndexSet<String> = IndexSet::new();
    let mut i = owner.start_line as usize;
    while i < owner.end_line as usize {
        let method = methods.iter().find(|n| n.start_line as usize <= i + 1 && n.end_line as usize >= i + 1);
        let skip = match method {
            Some(m) if m.name != "__init__" && m.id != r.from_node_id => true,
            Some(m) if m.id == r.from_node_id && i + 1 > r.line as usize => true,
            _ => false,
        };
        if skip {
            i += 1;
            continue;
        }
        let line = lines.get(i).map(|s| s.as_str()).unwrap_or("");
        let declared = if method.is_none() || re!(r"^\s*(?:self|cls)\.").is_match(line) {
            annotation.captures(line).and_then(|c| c.text(1).map(|s| s.to_string()))
        } else {
            None
        };
        if let Some(d) = declared {
            types.insert(d);
        }
        let Some(assigned) = assignment.captures(line) else {
            i += 1;
            continue;
        };
        if let Some(t) = assigned.text(1) {
            types.insert(t.to_string());
            i += 1;
            continue;
        }
        let rhs = assigned.text(2).unwrap_or("");
        if let Some(c) = re!(r"^([A-Z][\w.]*)\s*\(").captures(rhs).and_then(|c| c.text(1).map(|s| s.to_string())) {
            types.insert(c);
            i += 1;
            continue;
        }
        let param = js_trim(rhs);
        match method {
            Some(m) if re!(r"^\w+$").is_match(param) => {
                let signature = m.signature.clone().unwrap_or_default();
                if let Some(t) = Re::cached(&format!(r#"\b{param}\s*:\s*["']?([\w.]+)"#), "").captures(&signature).and_then(|c| c.text(1).map(|s| s.to_string())) {
                    types.insert(t);
                }
            }
            _ => {
                types.insert("<unknown>".to_string());
            }
        }
        i += 1;
    }
    match types.len() {
        0 => None,
        1 => types.into_iter().next(),
        _ => Some("<ambiguous>".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assignment_splits_chained_and_augmented_targets() {
        let a = python_assignment("x = y = Foo()").unwrap();
        assert_eq!(a.targets, vec!["x ", " y "]);
        assert_eq!(a.value, " Foo()");
        assert!(!a.augmented);
        let b = python_assignment("total += 1").unwrap();
        assert!(b.augmented);
        assert_eq!(b.targets, vec!["total "]);
        assert!(python_assignment("if a == b:").is_none());
        assert!(python_assignment("f(a=1)").is_none());
    }

    #[test]
    fn constructor_call_requires_the_whole_expression() {
        assert_eq!(python_constructor_call("Foo()"), Some("Foo".into()));
        assert_eq!(python_constructor_call("pkg.mod.Foo(1, bar=[2])"), Some("pkg.mod.Foo".into()));
        assert_eq!(python_constructor_call("(Foo())"), Some("Foo".into()));
        assert_eq!(python_constructor_call("Foo().bar"), None);
        assert_eq!(python_constructor_call("foo()"), None);
    }

    #[test]
    fn strings_are_blanked_keeping_quotes() {
        assert_eq!(blank_python_strings("a = 'xy' + \"z\""), "a = '  ' + \" \"");
    }

    #[test]
    fn line_bindings_classify_imports_and_assignments() {
        let lines: Vec<String> = ["import os.path as p", "x: int = 3", "for x in y: pass"].iter().map(|s| s.to_string()).collect();
        let statements = vec![true; 3];
        assert_eq!(python_line_bindings(&lines, 0, &statements, "p"), vec![PythonBinding::Import { key: "os.path:*".into() }]);
        assert!(matches!(python_line_bindings(&lines, 1, &statements, "x")[0], PythonBinding::Assign { .. }));
        assert_eq!(python_line_bindings(&lines, 2, &statements, "x"), vec![PythonBinding::Other]);
    }
}
