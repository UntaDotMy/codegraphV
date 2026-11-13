//! PHP scope rules: which class a bare name is (its namespace's, or the one a `use` names), which
//! method `$this->` / `self::` / `parent::` reaches through the class hierarchy and its traits, and
//! whether a receiver's name points at a class that has a method.

use super::js_store::enclosing_scope_start_line;
use super::lang_shape::RefShape;
use super::members::find_own_method;
use super::method::split_camel_case;
use super::util::*;
use crate::ctx::Ctx;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
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
memo_map!(PhpAncestryMemo, String, Rc<PhpAncestry>);
memo_map!(PhpTypeMethodMemo, String, Option<PhpTypeMethod>);

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
        // `use App\Models\{User, Post as Article};`
        for m in re!(r"^\s*use\s+(?:function\s+|const\s+)?([\w\\]+)\\\{([^}]*)\}\s*;", "gm").captures_iter(header) {
            let prefix = m.text(1).unwrap_or("").trim_start_matches('\\');
            for item in m.text(2).unwrap_or("").split(',') {
                let Some(named) = re!(r"^\s*([\w\\]+)(?:\s+as\s+(\w+))?\s*$").captures(item) else { continue };
                let item_name = named.text(1).unwrap_or("");
                if item_name.is_empty() {
                    continue;
                }
                let alias = named
                    .text(2)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| item_name.rsplit('\\').next().unwrap_or("").to_string());
                uses.insert(alias, format!("{prefix}\\{item_name}"));
            }
        }
        Rc::new(PhpFileScope { namespace, uses })
    })
}

/// A PHP class name as the qualified name the graph stores: `Ns\Cls` → `Ns::Cls`.
pub fn php_type_qn(name: &str, file: &str, ctx: &Ctx) -> String {
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

/// The traits a class uses, then the parents it extends: the order PHP finds a method in.
/// A declaration starts at its attributes, so the head is read from the `class` line.
fn php_supertype_qns(decl: &Node, ctx: &Ctx) -> Rc<Vec<String>> {
    memoized::<PhpSupersMemo>(ctx, decl.id.clone(), || {
        let lines = lines_of(ctx, &decl.file_path);
        let named = Re::cached(&format!(r"\b(?:class|interface|trait|enum)\s+{}\b", escape_regex(&decl.name)), "i");
        let mut at = decl.start_line.saturating_sub(1) as usize;
        let last = decl.end_line.saturating_sub(1) as usize;
        while at < last && !named.is_match(lines.get(at).map(String::as_str).unwrap_or("")) {
            at += 1;
        }
        if at >= last {
            at = decl.start_line.saturating_sub(1) as usize;
        }
        let head_to = (at + 12).min(lines.len());
        let head = if at < lines.len() { lines[at..head_to].join(" ") } else { String::new() };
        let body_from = (at + 1).min(lines.len());
        let body_to = (decl.end_line as usize).min(at + 81).min(lines.len());
        let body_lines: &[String] = if body_from < body_to { &lines[body_from..body_to] } else { &[] };
        let first_function = body_lines.iter().position(|l| re!(r"\bfunction\b").is_match(l));
        let body = body_lines[..first_function.unwrap_or(body_lines.len())].join("\n");
        let mut names: Vec<String> = Vec::new();
        for used in re!(r"^\s*use\s+([\w\\,\s]+?)\s*[;{]", "gm").captures_iter(&body) {
            for t in used.text(1).unwrap_or("").split(',') {
                let t = t.trim();
                if !t.is_empty() {
                    names.push(t.to_string());
                }
            }
        }
        let extended = re!(r"\bextends\s+([^{]*?)(?:\bimplements\b|\{)").captures(&head).and_then(|c| c.text(1).map(String::from)).unwrap_or_default();
        for m in re!(r"(\\?[A-Za-z_][\w\\]*)", "g").captures_iter(&extended) {
            names.push(m.text(1).unwrap_or("").to_string());
        }
        Rc::new(names.iter().map(|n| php_type_qn(n, &decl.file_path, ctx)).collect())
    })
}

/// Every class a start set derives from (parents and traits), and whether the walk left the repo.
pub struct PhpAncestry {
    qns: HashSet<String>,
    leaves_repo: bool,
}

fn php_ancestry(start: &[String], ctx: &Ctx) -> Rc<PhpAncestry> {
    memoized::<PhpAncestryMemo>(ctx, start.join("\0"), || {
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
        Rc::new(PhpAncestry { qns, leaves_repo })
    })
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
    // `parent::x()` above a parent from outside the project is that parent's, not a trait of this one.
    via == PhpVia::SelfVia && up.leaves_repo && ctx.get_nodes_by_qualified_name(owner_qn).iter().any(|d| d.kind == NodeKind::Trait)
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

/// The method `name` a PHP type reaches in a trait it uses, then up what it extends.
fn php_ancestor_method(decl: &Node, name: &str, ctx: &Ctx, seen: &mut HashSet<String>) -> Option<NodeRef> {
    for qn in php_supertype_qns(decl, ctx).iter() {
        if !seen.insert(qn.clone()) {
            continue;
        }
        for up in ctx.get_nodes_by_qualified_name(qn).iter() {
            if up.language != Language::Php || !is_php_type_kind(up.kind) {
                continue;
            }
            let nodes = ctx.get_nodes_in_file(&up.file_path);
            if let Some(found) = find_own_method(&nodes, up, name, None).or_else(|| php_ancestor_method(up, name, ctx, seen)) {
                return Some(found);
            }
        }
    }
    None
}

/// A class's own method, or the one its traits and parents reach.
pub fn php_class_method(class_node: &Node, method_name: &str, ctx: &Ctx) -> Option<NodeRef> {
    let nodes = ctx.get_nodes_in_file(&class_node.file_path);
    if let Some(own) = find_own_method(&nodes, class_node, method_name, None) {
        return Some(own);
    }
    let mut seen = HashSet::from([class_node.qualified_name.clone()]);
    php_ancestor_method(class_node, method_name, ctx, &mut seen)
}

/// Whether a PHP file names a class by its short name: its own namespace, or a `use`.
pub fn php_file_names_class(candidate: &Node, file: &str, ctx: &Ctx) -> bool {
    let scope = php_file_scope(file, ctx);
    let fqn = candidate.qualified_name.replace("::", "\\").to_lowercase();
    let own = if scope.namespace.is_empty() { candidate.name.to_lowercase() } else { format!("{}\\{}", scope.namespace, candidate.name).to_lowercase() };
    fqn == own || scope.uses.values().any(|used| used.to_lowercase() == fqn)
}

/// The method a variable named after a class reaches when the file cannot name that class.
pub fn php_unnamed_class_method(class_node: &Node, method_name: &str, only_of_name: bool, ctx: &Ctx) -> Option<NodeRef> {
    let nodes = ctx.get_nodes_in_file(&class_node.file_path);
    let own = find_own_method(&nodes, class_node, method_name, None);
    own.or_else(|| if only_of_name { php_class_method(class_node, method_name, ctx) } else { None })
}

/// The method a Laravel facade hands a static call on to, when `getFacadeAccessor` returns `Name::class`.
pub fn php_facade_method(facade: &Node, name: &str, ctx: &Ctx) -> Option<NodeRef> {
    if !php_supertype_qns(facade, ctx).iter().any(|qn| qn.ends_with("::Facade")) {
        return None;
    }
    let nodes = ctx.get_nodes_in_file(&facade.file_path);
    let accessor = find_own_method(&nodes, facade, "getFacadeAccessor", None)?;
    let lines = lines_of(ctx, &facade.file_path);
    let from = (accessor.start_line as usize).saturating_sub(1).min(lines.len());
    let to = (accessor.end_line as usize).min(lines.len()).max(from);
    let named = re!(r"\breturn\s+(\\?[A-Za-z_][\w\\]*)::class\s*;").captures(&lines[from..to].join("\n")).and_then(|c| c.text(1).map(String::from))?;
    php_type_method(&php_type_qn(&named, &facade.file_path, ctx), name, ctx).and_then(|hit| hit.method)
}

/// The method `name` of the PHP type qualified `qn`. `None` when the project declares no such type.
#[derive(Clone)]
pub struct PhpTypeMethod {
    pub method: Option<NodeRef>,
}

pub fn php_type_method(qn: &str, name: &str, ctx: &Ctx) -> Option<PhpTypeMethod> {
    memoized::<PhpTypeMethodMemo>(ctx, format!("{qn}\0{name}"), || {
        let decls: Vec<NodeRef> = ctx.get_nodes_by_qualified_name(qn).iter().filter(|n| n.language == Language::Php && is_php_type_kind(n.kind)).cloned().collect();
        if decls.is_empty() {
            return None;
        }
        for decl in &decls {
            if let Some(method) = php_class_method(decl, name, ctx) {
                return Some(PhpTypeMethod { method: Some(method) });
            }
        }
        Some(PhpTypeMethod { method: None })
    })
}

/// What a declared PHP type does with a call: the method it reaches, a closed type with no such
/// method, a type that still may name a method past an outside parent or `__call`, or no such type.
pub enum PhpDeclaredCall {
    Found(NodeRef),
    Whole,
    Open,
    Unknown,
}

pub fn php_declared_call(ty: &str, method: &str, file: &str, ctx: &Ctx) -> PhpDeclaredCall {
    let qn = php_type_qn(ty, file, ctx);
    match php_type_method(&qn, method, ctx) {
        Some(PhpTypeMethod { method: Some(found) }) => PhpDeclaredCall::Found(found),
        Some(PhpTypeMethod { method: None }) => {
            let leaves = php_ancestry(std::slice::from_ref(&qn), ctx).leaves_repo;
            let magic = php_type_method(&qn, "__call", ctx).and_then(|hit| hit.method).is_some();
            if !leaves && !magic { PhpDeclaredCall::Whole } else { PhpDeclaredCall::Open }
        }
        None => PhpDeclaredCall::Unknown,
    }
}

/// Eloquent's static methods that hand back a model of the class they are called on.
fn eloquent_factory_call() -> &'static Re {
    re!(r"^\s*(?:(?:\$\w+\s*=\s*)*)(\\?[A-Z][\w\\]*)::(?:query\(\)\s*->\s*)?(?:create|forceCreate|firstOrCreate|firstOrNew|updateOrCreate|find|findOrFail)\s*\(")
}

/// The model a local holds when the nearest assignment above the call is an Eloquent factory.
pub fn php_model_factory_local_type(receiver: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    if !re!(r"^\w+$").is_match(receiver) {
        return None;
    }
    let lines = lines_of(ctx, &r.file_path);
    if lines.is_empty() {
        return None;
    }
    let from = enclosing_scope_start_line(r, ctx).saturating_sub(1) as usize;
    let variable = format!("${receiver}");
    let last = (r.line as usize).saturating_sub(1).min(lines.len() - 1);
    for i in (from..=last).rev() {
        let line = &lines[i];
        let Some(at) = line.find(&variable) else { continue };
        if !line[..at].trim().is_empty() {
            continue;
        }
        let Some(value) = re!(r"^\s*=(?!=)\s*(.*)$").captures(&line[at + variable.len()..]).and_then(|c| c.text(1).map(String::from)) else {
            continue;
        };
        let Some(named) = eloquent_factory_call().captures(&value).and_then(|c| c.text(1).map(String::from)) else {
            return None;
        };
        let qn = php_type_qn(&named, &r.file_path, ctx);
        let decls: Vec<NodeRef> = ctx.get_nodes_by_qualified_name(&qn).iter().filter(|n| n.language == Language::Php && n.kind == NodeKind::Class).cloned().collect();
        let model = decls.iter().any(|d| php_ancestry(&[d.qualified_name.clone()], ctx).qns.iter().any(|one| one.ends_with("::Model")));
        return if model { Some(named) } else { None };
    }
    None
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
