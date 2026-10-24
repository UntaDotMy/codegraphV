//! PHP scope rules: which class a bare name is (its namespace's, or the one a `use` names), which
//! method `$this->` / `self::` / `parent::` reaches through the class hierarchy and its traits, and
//! whether a receiver's name points at a class that has a method.

use super::method::split_camel_case;
use super::util::*;
use crate::ctx::Ctx;
use super::lang_shape::RefShape;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_types::{Language, Node, NodeKind};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

fn is_php_type_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Class | NodeKind::Trait | NodeKind::Interface | NodeKind::Enum)
}

/// A file's namespace and its `use` imports (alias or last segment → fully qualified name).
pub struct PhpFileScope {
    pub namespace: String,
    pub uses: HashMap<String, String>,
}

memo_map!(PhpFileScopeMemo, String, Rc<PhpFileScope>);
memo_map!(PhpSupersMemo, String, Rc<Vec<String>>);

pub fn php_file_scope(file: &str, ctx: &Ctx) -> Rc<PhpFileScope> {
    memoized::<PhpFileScopeMemo>(ctx, file.to_string(), || {
        let text = ctx.read_file(file).map(|s| s.to_string()).unwrap_or_default();
        let namespace = re!(r"^\s*namespace\s+([\w\\]+)\s*[;{]", "m").captures(&text).and_then(|c| c.text(1).map(String::from)).unwrap_or_default();
        let mut uses = HashMap::new();
        // Only the imports above the first class-like declaration.
        let header_end = re!(r"^\s*(?:(?:abstract|final|readonly)\s+)*(?:class|trait|interface|enum)\s", "m")
            .find(&text)
            .map(|m| m.start())
            .filter(|at| *at > 0)
            .unwrap_or(text.len());
        let header = &text[..header_end];
        for m in re!(r"^\s*use\s+(?:function\s+|const\s+)?([\w\\]+)(?:\s+as\s+(\w+))?\s*;", "gm").captures_iter(header) {
            let fqn = m.text(1).unwrap_or("").trim_start_matches('\\').to_string();
            let key = m.text(2).filter(|s| !s.is_empty()).map(String::from).unwrap_or_else(|| fqn.rsplit('\\').next().unwrap_or("").to_string());
            uses.insert(key, fqn);
        }
        Rc::new(PhpFileScope { namespace, uses })
    })
}

/// A PHP class name as the qualified name the graph stores: `Ns\Cls` → `Ns::Cls`.
fn php_type_qn(name: &str, file: &str, ctx: &Ctx) -> String {
    let fqn = if let Some(rest) = name.strip_prefix('\\') {
        rest.to_string()
    } else {
        let scope = php_file_scope(file, ctx);
        let mut parts = name.split('\\');
        let head = parts.next().unwrap_or("");
        let rest: Vec<&str> = parts.collect();
        match scope.uses.get(head) {
            Some(imported) => std::iter::once(imported.as_str()).chain(rest).collect::<Vec<_>>().join("\\"),
            None if !scope.namespace.is_empty() => format!("{}\\{name}", scope.namespace),
            None => name.to_string(),
        }
    };
    match fqn.rfind('\\') {
        Some(at) => format!("{}::{}", &fqn[..at], &fqn[at + 1..]),
        None => fqn,
    }
}

/// The qualified names of a class's parent, and of the traits its body `use`s.
fn php_supertype_qns(decl: &Node, ctx: &Ctx) -> Rc<Vec<String>> {
    memoized::<PhpSupersMemo>(ctx, decl.id.clone(), || {
        let mut names: Vec<String> = Vec::new();
        let lines = lines_of(ctx, &decl.file_path);
        let head = js_slice(&lines, decl.start_line as i64 - 1, decl.start_line as i64 + 4).join(" ");
        let extended = re!(r"\bextends\s+([^{]*?)(?:\bimplements\b|\{)").captures(&head).and_then(|c| c.text(1).map(String::from)).unwrap_or_default();
        for m in re!(r"(\\?[A-Za-z_][\w\\]*)", "g").captures_iter(&extended) {
            names.push(m.text(1).unwrap_or("").to_string());
        }
        let body_lines = js_slice(&lines, decl.start_line as i64, (decl.end_line as i64).min(decl.start_line as i64 + 80));
        let first_function = body_lines.iter().position(|l| re!(r"\bfunction\b").is_match(l));
        let body = body_lines[..first_function.unwrap_or(body_lines.len())].join("\n");
        for used in re!(r"^\s*use\s+([\w\\,\s]+?)\s*[;{]", "gm").captures_iter(&body) {
            for t in used.text(1).unwrap_or("").split(',') {
                let t = t.trim();
                if !t.is_empty() {
                    names.push(t.to_string());
                }
            }
        }
        Rc::new(names.iter().map(|n| php_type_qn(n, &decl.file_path, ctx)).collect())
    })
}

/// Every class a start set derives from (parents and traits), and whether the walk left the repo.
struct PhpAncestry {
    qns: HashSet<String>,
    leaves_repo: bool,
}

fn php_ancestry(start: &[String], ctx: &Ctx) -> PhpAncestry {
    let mut qns: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = start.iter().cloned().collect();
    let mut leaves_repo = false;
    while let Some(qn) = queue.pop_front() {
        if qns.len() >= 80 {
            break;
        }
        if !qns.insert(qn.clone()) {
            continue;
        }
        let decls: Vec<_> = ctx.get_nodes_by_qualified_name(&qn).iter().filter(|d| d.language == Language::Php && is_php_type_kind(d.kind)).cloned().collect();
        if decls.is_empty() {
            leaves_repo = true;
        }
        for decl in &decls {
            queue.extend(php_supertype_qns(decl, ctx).iter().cloned());
        }
    }
    PhpAncestry { qns, leaves_repo }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PhpVia {
    SelfVia,
    Parent,
}

/// `$this->m()` / `self::m()` / `static::m()` → self; `parent::m()` → parent.
pub fn php_self_receiver(r: &UnresolvedRef, ctx: &Ctx) -> Option<PhpVia> {
    if r.language != Language::Php || !is_calls(r) || !re!(r"^\w+$").is_match(&r.reference_name) {
        return None;
    }
    let line = line_at(ctx, &r.file_path, r.line).filter(|l| !l.is_empty()).or_else(|| {
        ctx.read_file(&r.file_path).and_then(|s| s.split('\n').nth((r.line as usize).wrapping_sub(1)).map(String::from)).filter(|l| !l.is_empty())
    })?;
    let name = &r.reference_name;
    if Re::cached(&format!(r"\$this\s*\??->\s*{name}\s*\(|\b(?:self|static)\s*::\s*{name}\s*\("), "").is_match(&line) {
        return Some(PhpVia::SelfVia);
    }
    if Re::cached(&format!(r"\bparent\s*::\s*{name}\s*\("), "").is_match(&line) {
        return Some(PhpVia::Parent);
    }
    None
}

/// Whether a method found by name is one `self::` / `parent::` / `$this->` can reach from the
/// class around the call: the class's own, an ancestor's, or (through `self`) a descendant's.
pub fn is_php_method_in_scope(method: &Node, r: &UnresolvedRef, via: PhpVia, ctx: &Ctx) -> bool {
    let Some(cut) = method.qualified_name.rfind("::") else { return true };
    let owner_qn = &method.qualified_name[..cut];
    let mut types: Vec<_> = ctx.get_nodes_in_file(&r.file_path).iter().filter(|n| is_php_type_kind(n.kind) && n.start_line <= r.line && n.end_line >= r.line).cloned().collect();
    types.sort_by(|a, b| b.start_line.cmp(&a.start_line));
    let Some(enclosing) = types.first() else { return true };
    if enclosing.kind == NodeKind::Trait {
        return true;
    }
    let start: Vec<String> = if via == PhpVia::Parent { php_supertype_qns(enclosing, ctx).to_vec() } else { vec![enclosing.qualified_name.clone()] };
    let up = php_ancestry(&start, ctx);
    if up.qns.contains(owner_qn) {
        return true;
    }
    if via == PhpVia::SelfVia && php_ancestry(&[owner_qn.to_string()], ctx).qns.contains(&enclosing.qualified_name) {
        return true;
    }
    up.leaves_repo && ctx.get_nodes_by_qualified_name(owner_qn).iter().any(|d| d.kind == NodeKind::Trait)
}

/// Whether a receiver's name (`$orderService`, `this.mailer`) names a class that is, or derives
/// from, the method's owner.
pub fn php_receiver_reaches(receiver: &str, method: &Node, ctx: &Ctx) -> bool {
    let Some(cut) = method.qualified_name.rfind("::") else { return false };
    let owner = &method.qualified_name[..cut];
    let last = receiver.rsplit('.').next().unwrap_or("");
    let last = last.strip_prefix('$').unwrap_or(last);
    if last.is_empty() {
        return false;
    }
    let words = split_camel_case(last);
    let capitalize = |w: &str| {
        let mut chars = w.chars();
        chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
    };
    let mut names: Vec<String> = vec![capitalize(last), capitalize(words.last().map_or(last, |w| w.as_str()))];
    names.dedup();
    for name in names {
        for decl in ctx.get_nodes_by_name(&name).iter() {
            if decl.language != Language::Php || !is_php_type_kind(decl.kind) {
                continue;
            }
            if php_ancestry(&[decl.qualified_name.clone()], ctx).qns.contains(owner) {
                return true;
            }
        }
    }
    false
}

/// A bare PHP class name is its namespace's class, or the one a `use` names.
pub fn is_php_class_visible(candidate: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if r.language != Language::Php || candidate.language != Language::Php || !is_php_type_kind(candidate.kind) {
        return true;
    }
    let name = &r.reference_name;
    if !re!(r"^[A-Za-z_]\w*$").is_match(name) || re!(r"^(?:self|static|parent)$", "i").is_match(name) {
        return true;
    }
    let fqn = candidate.qualified_name.replace("::", "\\");
    let scope = php_file_scope(&r.file_path, ctx);
    if let Some(imported) = scope.uses.get(name.as_str()) {
        return imported.to_lowercase() == fqn.to_lowercase();
    }
    let expected = if scope.namespace.is_empty() { name.clone() } else { format!("{}\\{name}", scope.namespace) };
    fqn.to_lowercase() == expected.to_lowercase()
}

/// The PHP contribution to [`super::lang_shape::LangShape`]: `$this->m()` / `self::m()` / `parent::m()`.
pub struct PhpShape(PhpVia);

impl PhpShape {
    pub fn of(r: &UnresolvedRef, ctx: &Ctx) -> Option<PhpShape> {
        php_self_receiver(r, ctx).map(PhpShape)
    }
}

impl RefShape for PhpShape {
    fn excludes(&self, n: &NodeRef, r: &UnresolvedRef, ctx: &Ctx) -> bool {
        n.kind != NodeKind::Method || !is_php_method_in_scope(n, r, self.0, ctx)
    }
}
