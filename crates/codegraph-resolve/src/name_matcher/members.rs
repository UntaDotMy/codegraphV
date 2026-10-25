//! Class members as seen by a receiver: the declared type of a field, the supertypes a class
//! names in its head, and the method an inherited chain reaches.

use super::infer::{normalize_inferred_type_name, TYPE_ARGS};
use super::lang_shape::is_method_owner_kind;
use super::objc::objc_supertypes_of;
use super::dart::dart_supertypes_of;
use super::scala::scala_supertypes_of;
use super::swift::swift_decl_supers;
use super::ruby::{ruby_methods_of_kind_first, RubyMethodKind};
use super::util::*;
use crate::ctx::Ctx;
use crate::types::{NodeRef, Tri, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::js_trim;
use codegraph_types::{Language, Node, NodeKind};
use std::rc::Rc;

memo_map!(MemberTypeMemo, String, Option<String>);
memo_map!(MemberShadowMemo, String, bool);
memo_map!(MemberWalkMemo, String, Option<String>);
memo_map!(MemberLinesMemo, String, Rc<Vec<(String, usize)>>);
memo_map!(DeclaredSupersMemo, String, Rc<Vec<String>>);
memo_map!(InheritedMethodMemo, String, Option<NodeRef>);

pub fn is_member_typed_language(l: Language) -> bool {
    matches!(l, Language::CSharp | Language::Java | Language::Kotlin | Language::Swift)
}

pub fn is_member_class_kind(k: NodeKind) -> bool {
    matches!(k, NodeKind::Class | NodeKind::Struct | NodeKind::Interface | NodeKind::Enum)
}

const MEMBER_TYPE_NON_TYPES: &[&str] = &[
    "return", "new", "case", "throw", "else", "in", "out", "ref", "params", "await", "yield", "is", "as", "using", "var", "val", "goto", "nameof",
    "typeof", "sizeof", "default", "when", "where", "get", "set", "init", "class", "interface", "enum", "struct", "record", "object", "namespace",
    "package", "import", "extends", "implements", "fun", "static", "final", "abstract", "sealed", "override", "virtual", "delegate", "event",
    // Java's `sealed interface Shape permits Circle, Square` and `void run() throws Failure {`.
    "permits", "throws",
];

/// The type of a field `receiver` (`this.repo`, `repo`) of the class around the reference, read from the class head or body.
pub fn infer_member_receiver_type(receiver: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let name = re!(r"^(?:this|self)\.").replace_first(receiver, |_| String::new());
    if !is_plain_ident(&name) {
        return None;
    }
    let in_file: Vec<NodeRef> = ctx.get_nodes_in_file(&r.file_path).iter().filter(|n| n.language == r.language).cloned().collect();
    let mut cls: Option<NodeRef> = None;
    let mut func: Option<NodeRef> = None;
    for n in &in_file {
        if n.start_line > r.line || n.end_line < r.line {
            continue;
        }
        if is_member_class_kind(n.kind) && cls.as_ref().map_or(true, |c| n.start_line >= c.start_line) {
            cls = Some(n.clone());
        } else if matches!(n.kind, NodeKind::Method | NodeKind::Function) && func.as_ref().map_or(true, |f| n.start_line >= f.start_line) {
            func = Some(n.clone());
        }
    }
    let cls = cls?;
    let found = member_type_through_hierarchy(&cls, &name, ctx)?;
    match func {
        Some(f) if binds_name_itself(&f, &name, ctx) => None,
        _ => Some(found),
    }
}

/// A lambda parameter, `var` local, `out` variable or `foreach` binding in `func` shadows the field.
fn binds_name_itself(func: &Node, name: &str, ctx: &Ctx) -> bool {
    memoized::<MemberShadowMemo>(ctx, format!("{}|{}", func.id, name), || {
        let lines = lines_of(ctx, &func.file_path);
        let from = (func.start_line as usize - 1).min(lines.len());
        let to = (func.end_line as usize).min(lines.len()).max(from);
        let body = lines[from..to].join("\n");
        let r = escape_regex(name);
        Re::cached(
            &format!(
                r"\b(?:var|val|let|out\s+[\w.<>?]+|foreach\s*\(\s*[\w.<>?,\s]+?)\s+{r}\b|\bfor\s*\([^;)]*\s{r}\s*:|\b{r}\s*=>|[(,]\s*{r}\s*(?:,[^()]*)?\)\s*=>|\b{r}\s*(?:,[^{{}}]*)?->"
            ),
            "",
        )
        .is_match(&body)
    })
}

struct Queued {
    ty: NodeRef,
    args: std::collections::HashMap<String, String>,
}

/// The type of member `name` found on `cls` or the nearest base declaring it, with generic arguments substituted.
fn member_type_through_hierarchy(cls: &NodeRef, name: &str, ctx: &Ctx) -> Option<String> {
    memoized::<MemberWalkMemo>(ctx, format!("{}|{}", cls.id, name), || {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut queue: std::collections::VecDeque<Queued> = std::collections::VecDeque::new();
        queue.push_back(Queued { ty: cls.clone(), args: Default::default() });
        while let Some(Queued { ty, args }) = queue.pop_front() {
            if seen.len() >= 8 {
                break;
            }
            if seen.contains(&ty.id) {
                continue;
            }
            seen.insert(ty.id.clone());
            if let Some(found) = class_member_type(&ty, name, ctx) {
                if ty.id == cls.id {
                    return Some(found);
                }
                if let Some(given) = args.get(&found) {
                    return Some(given.clone());
                }
                return match type_parameter_bound_in(&found, &[ty.clone()], ctx) {
                    Tri::Undefined => Some(found),
                    Tri::Null => Some("object".to_string()),
                    Tri::Value(b) => Some(b),
                };
            }
            for sup in class_head_supertypes(&ty, ctx) {
                let given: Vec<String> = head_type_arguments(&ty, &sup, ctx).into_iter().map(|a| args.get(&a).cloned().unwrap_or(a)).collect();
                for decl in ctx.get_nodes_by_name(&sup).iter() {
                    if decl.language != ty.language || !is_member_class_kind(decl.kind) {
                        continue;
                    }
                    let params = declared_type_parameters(decl, ctx);
                    let map = params
                        .into_iter()
                        .enumerate()
                        .filter_map(|(i, p)| {
                            let a = given.get(i).cloned().unwrap_or_default();
                            if a.is_empty() { None } else { Some((p, a)) }
                        })
                        .collect();
                    queue.push_back(Queued { ty: decl.clone(), args: map });
                }
            }
        }
        None
    })
}

/// The top-level arguments of the first `<…>` in `text`.
fn angle_arguments(text: &str) -> Vec<String> {
    let Some(open) = text.find('<') else { return Vec::new() };
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in text[open + 1..].chars() {
        if ch == '<' {
            depth += 1;
        } else if ch == '>' {
            let was = depth;
            depth -= 1;
            if was == 0 {
                break;
            }
        }
        if ch == ',' && depth == 0 {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(ch);
        }
    }
    out.push(cur);
    out.into_iter().map(|a| js_trim(&a).to_string()).collect()
}

pub fn declared_type_parameters(decl: &Node, ctx: &Ctx) -> Vec<String> {
    let lines = lines_of(ctx, &decl.file_path);
    let from = (decl.start_line as usize - 1).min(lines.len());
    let to = (decl.start_line as usize + 3).min(lines.len()).max(from);
    let head = lines[from..to].join(" ");
    let Some(at) = Re::cached(&format!(r"\b{}\s*<", escape_regex(&decl.name)), "").find(&head) else { return Vec::new() };
    angle_arguments(&head[at.start()..])
        .into_iter()
        .map(|p| {
            let p = re!(r"^(?:in|out|reified)\s+").replace_first(&p, |_| String::new());
            re!(r"([A-Za-z_]\w*)\s*(?:extends\b.*|:.*)?$").captures(&p).and_then(|c| c.text(1).map(|s| s.to_string())).unwrap_or_default()
        })
        .collect()
}

fn head_type_arguments(cls: &Node, sup: &str, ctx: &Ctx) -> Vec<String> {
    let lines = lines_of(ctx, &cls.file_path);
    let from = (cls.start_line as usize - 1).min(lines.len());
    let to = (cls.start_line as usize + 8).min(lines.len()).max(from);
    let joined = lines[from..to].join(" ");
    let head = joined.split('{').next().unwrap_or("");
    let Some(at) = Re::cached(&format!(r"(?:[:,]|\bextends|\bimplements)\s*(?:[\w.]+\.)?{}\s*<", escape_regex(sup)), "").find(head) else {
        return Vec::new();
    };
    angle_arguments(&head[at.start()..])
        .into_iter()
        .map(|a| {
            let stripped = re!(r"<[\s\S]*$").replace_first(&a, |_| String::new());
            js_trim(stripped.rsplit('.').next().unwrap_or("")).to_string()
        })
        .collect()
}

/// The supertypes a class names in its head, as the language writes them.
pub fn declared_supertypes(cls: &Node, ctx: &Ctx) -> Rc<Vec<String>> {
    memoized::<DeclaredSupersMemo>(ctx, cls.id.clone(), || Rc::new(read_declared_supertypes(cls, ctx)))
}

fn read_declared_supertypes(cls: &Node, ctx: &Ctx) -> Vec<String> {
    match cls.language {
        Language::Java | Language::CSharp | Language::Kotlin => return class_head_supertypes(cls, ctx),
        Language::Dart => return dart_supertypes_of(&cls.name, ctx),
        Language::Swift => return swift_decl_supers(&cls.name, ctx),
        Language::ObjC => return objc_supertypes_of(&cls.name, ctx).to_vec(),
        Language::Scala => return scala_supertypes_of(&cls.name, ctx).to_vec(),
        _ => {}
    }
    let lines = lines_of(ctx, &cls.file_path);
    let from = (cls.start_line as usize - 1).min(lines.len());
    let to = (cls.start_line as usize + 2).min(lines.len()).max(from);
    let head = lines[from..to].join(" ");
    let names = |text: Option<String>| -> Vec<String> {
        match text {
            Some(t) if !t.is_empty() => re!(r"([A-Za-z_][\w.:\\]*)", "g")
                .captures_iter(&t)
                .filter_map(|m| m.text(1).and_then(|s| re!(r"::|\.|\\").split(s).into_iter().last().map(|x| x.to_string())))
                .filter(|w| !re!(r"^(?:metaclass|object)$").is_match(w))
                .collect(),
            _ => Vec::new(),
        }
    };
    let cap = |pattern: &Re, text: &str| pattern.captures(text).and_then(|c| c.text(1).map(|s| s.to_string()));
    match cls.language {
        Language::Pascal => names(cap(re!(r"=\s*class\s*(?:sealed|abstract)?\s*\(([^)]*)\)", "i"), &head)),
        Language::Python => {
            let inner = cap(re!(r"\bclass\s+\w+\s*\(([^)]*)\)"), &head)
                .map(|s| re!(r"\w+\s*=\s*[\w.]+", "g").replace_all_str(&s, ""))
                .map(|s| re!(r"([A-Za-z_][\w.]*)\.\w+\s*\(", "g").replace_all(&s, |m| Some(format!("{} (", m.text(1).unwrap_or("")))));
            names(inner)
        }
        Language::Ruby => names(cap(re!(r"\bclass\s+[\w:]+\s*<\s*([\w:]+)"), &head)),
        Language::Php => names(cap(re!(r"\bextends\s+([\w.\\]+)"), &head)),
        Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx => {
            names(cap(re!(r"\bextends\s+([\w.\\]+)"), &ts_class_head(&lines, cls)))
        }
        _ => Vec::new(),
    }
}

/// A TS/JS class's (or interface's) head, from its `class` keyword to its body, with type
/// parameter lists and type arguments left out.
pub fn ts_class_head(lines: &[String], cls: &Node) -> String {
    let from = (cls.start_line as usize - 1).min(lines.len());
    let to = (cls.start_line as usize + 7).min(lines.len()).max(from);
    let text = lines[from..to].join(" ");
    let named = Re::cached(&format!(r"\b(?:class|interface)\s+{}\b", escape_regex(&cls.name)), "").find(&text).map(|m| m.start());
    let start = named.or_else(|| re!(r"\bclass\b").find(&text).map(|m| m.start())).unwrap_or(0);
    let bytes: Vec<char> = text[start..].chars().collect();
    let mut head = String::new();
    let mut angles = 0i32;
    for (i, &c) in bytes.iter().enumerate() {
        if c == '<' {
            angles += 1;
        } else if c == '>' && angles > 0 && !(i > 0 && bytes[i - 1] == '=') {
            angles -= 1;
        } else if angles == 0 {
            if c == '{' {
                break;
            }
            head.push(c);
        }
    }
    head
}

/// The supertypes in a Java/C#/Kotlin class head's `extends`/`implements`/`:` clause.
pub fn class_head_supertypes(cls: &Node, ctx: &Ctx) -> Vec<String> {
    let lines = lines_of(ctx, &cls.file_path);
    let from = (cls.start_line as usize - 1).min(lines.len());
    let to = (cls.start_line as usize + 8).min(lines.len()).max(from);
    let joined = lines[from..to].join(" ");
    let cleaned = re!(r"//[^\n]*|/\*.*?\*/", "g").replace_all_str(&joined, " ");
    let mut depth = 0i32;
    let mut head = String::new();
    for ch in cleaned.chars() {
        if ch == '{' && depth == 0 {
            break;
        }
        if ch == '<' || ch == '(' {
            depth += 1;
        } else if ch == '>' || ch == ')' {
            depth = (depth - 1).max(0);
        } else if depth == 0 {
            head.push(ch);
        }
    }
    let clause = re!(r"\b(?:extends|implements)\b([\s\S]*)$")
        .captures(&head)
        .and_then(|c| c.text(1).map(|s| s.to_string()))
        .or_else(|| re!(r"\b(?:class|interface|struct|record|object)\s+\w+[^:]*:([\s\S]*)$").captures(&head).and_then(|c| c.text(1).map(|s| s.to_string())))
        .unwrap_or_default();
    let clause = re!(r"\bwhere\b[\s\S]*$").replace_first(&clause, |_| String::new());
    re!(r"([A-Z]\w*)\s*(?=,|$|\bimplements\b)", "g").captures_iter(&clause).filter_map(|m| m.text(1).map(|s| s.to_string())).collect()
}

/// The method `name` on the nearest ancestor (within five levels) of any of `classes`.
/// `kind` picks between a Ruby class's `def self.m` and `def m` (`ruby_methods_of_kind_first`).
pub fn inherited_class_method(classes: &[NodeRef], name: &str, ctx: &Ctx, kind: Option<RubyMethodKind>) -> Option<NodeRef> {
    if classes.is_empty() {
        return None;
    }
    let key = format!("{}|{}|{}", classes.iter().map(|c| c.id.as_str()).collect::<Vec<_>>().join(","), name, RubyMethodKind::key(kind));
    memoized::<InheritedMethodMemo>(ctx, key, || find_inherited_class_method(classes, name, ctx, kind))
}

fn find_inherited_class_method(classes: &[NodeRef], name: &str, ctx: &Ctx, kind: Option<RubyMethodKind>) -> Option<NodeRef> {
    let mut seen: std::collections::HashSet<String> = classes.iter().map(|c| c.id.clone()).collect();
    let mut frontier: Vec<NodeRef> = classes.to_vec();
    let mut depth = 0;
    while depth < 5 && !frontier.is_empty() {
        let mut next: Vec<NodeRef> = Vec::new();
        for cls in &frontier {
            for sup in declared_supertypes(cls, ctx).iter() {
                for decl in ctx.get_nodes_by_name(sup).iter() {
                    if decl.language != cls.language || !is_method_owner_kind(decl) || seen.contains(&decl.id) {
                        continue;
                    }
                    seen.insert(decl.id.clone());
                    let in_decl_file = ctx.get_nodes_in_file(&decl.file_path);
                    let methods: Vec<NodeRef> = in_decl_file
                        .iter()
                        .filter(|n| {
                            n.kind == NodeKind::Method && n.name == name && {
                                let qn = n.qualified_name.as_str();
                                let owner = &qn[..qn.rfind("::").unwrap_or(0)];
                                re!(r"::|\.").split(owner).into_iter().last() == Some(decl.name.as_str())
                            }
                        })
                        .cloned()
                        .collect();
                    if let Some(m) = ruby_methods_of_kind_first(methods, kind).into_iter().next() {
                        return Some(m);
                    }
                    next.push(decl.clone());
                }
            }
        }
        frontier = next;
        depth += 1;
    }
    None
}

/// The declared type of member `name` of `cls` (`Logger log;`, `val log: Logger`).
pub fn class_member_type(cls: &Node, name: &str, ctx: &Ctx) -> Option<String> {
    memoized::<MemberTypeMemo>(ctx, format!("{}|{}", cls.id, name), || {
        let r = escape_regex(name);
        let pattern = if matches!(cls.language, Language::Kotlin | Language::Swift) {
            Re::cached(&format!(r"\b(?:val|var|let)\s+{r}\s*:\s*([A-Z][\w.]*)"), "")
        } else {
            Re::cached(&format!(r"(?:^|[\s(,])([A-Za-z_][\w.]*)\s*{TYPE_ARGS}\??\s+{r}\s*(?:[=;,)]|\{{)"), "")
        };
        for (text, depth) in class_member_lines(cls, ctx).iter() {
            if !text.contains(name) {
                continue;
            }
            let Some(m) = pattern.captures(text) else { continue };
            let g = m.get(0).unwrap();
            let prefix = &text[..g.start()];
            let in_parens = *depth == 1 && prefix.matches('(').count() > prefix.matches(')').count();
            let t = m.text(1).unwrap_or("");
            if !in_parens && !MEMBER_TYPE_NON_TYPES.contains(&t) {
                return normalize_inferred_type_name(t);
            }
        }
        None
    })
}

/// The class's own lines (strings and comments blanked) down to member depth.
pub fn class_member_lines(cls: &Node, ctx: &Ctx) -> Rc<Vec<(String, usize)>> {
    memoized::<MemberLinesMemo>(ctx, cls.id.clone(), || {
        let lines = lines_of(ctx, &cls.file_path);
        let mut out: Vec<(String, usize)> = Vec::new();
        let mut depth = 0usize;
        let mut in_comment = false;
        for line in cls.start_line..=cls.end_line {
            let mut raw = lines.get(line as usize - 1).cloned().unwrap_or_default();
            if in_comment {
                match raw.find("*/") {
                    None => continue,
                    Some(close) => {
                        raw = raw[close + 2..].to_string();
                        in_comment = false;
                    }
                }
            }
            let mut text = re!(r#""(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'"#, "g").replace_all(&raw, |_| Some("\"\"".to_string()));
            text = re!(r"/\*.*?\*/", "g").replace_all_str(&text, " ");
            text = re!(r"//.*$").replace_first(&text, |_| String::new());
            if let Some(open) = text.find("/*") {
                text.truncate(open);
                in_comment = true;
            }
            if depth <= 1 && !js_trim(&text).is_empty() {
                out.push((text.clone(), depth));
            }
            for ch in text.chars() {
                if ch == '{' {
                    depth += 1;
                } else if ch == '}' {
                    depth = depth.saturating_sub(1);
                }
            }
        }
        Rc::new(out)
    })
}

/// The bound of generic parameter `type_name` where `r` sits (`T extends Foo`): a value when bounded, null when declared but unbounded, undefined when not a type parameter.
pub fn type_parameter_bound(type_name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Tri<String> {
    let around: Vec<NodeRef> = ctx
        .get_nodes_in_file(&r.file_path)
        .iter()
        .filter(|n| n.start_line <= r.line && n.end_line >= r.line && (is_member_class_kind(n.kind) || matches!(n.kind, NodeKind::Method | NodeKind::Function)))
        .cloned()
        .collect();
    type_parameter_bound_in(type_name, &around, ctx)
}

pub fn type_parameter_bound_in(type_name: &str, decls: &[NodeRef], ctx: &Ctx) -> Tri<String> {
    if !re!(r"^[A-Z]\w*$").is_match(type_name) {
        return Tri::Undefined;
    }
    let t = type_name;
    let mut declared = false;
    for n in decls {
        let lines = lines_of(ctx, &n.file_path);
        let from = (n.start_line as usize - 1).min(lines.len());
        let to = (n.start_line as usize + 5).min(lines.len()).max(from);
        let joined = lines[from..to].join(" ");
        let head = joined.split('{').next().unwrap_or("");
        let bound = Re::cached(&format!(r"[<,]\s*(?:in\s+|out\s+|reified\s+)?{t}\s*(?:extends|:)\s*([A-Z][\w.]*)"), "")
            .captures(head)
            .and_then(|c| c.text(1).map(|s| s.to_string()))
            .or_else(|| Re::cached(&format!(r"\bwhere\s+{t}\s*:\s*([A-Z][\w.]*)"), "").captures(head).and_then(|c| c.text(1).map(|s| s.to_string())));
        if let Some(b) = bound {
            return Tri::Value(b.rsplit('.').next().unwrap_or("").to_string());
        }
        if Re::cached(&format!(r"[<,]\s*(?:in\s+|out\s+|reified\s+)?{t}\s*[,>]"), "").is_match(head) {
            declared = true;
        }
    }
    if declared {
        Tri::Null
    } else {
        Tri::Undefined
    }
}

/// The method `method_name` declared on `class_node` itself (by qualified name, then by owner name).
/// `kind` picks between a Ruby class's `def self.m` and `def m`, which share one qualified name.
pub fn find_own_method(nodes_in_file: &[NodeRef], class_node: &Node, method_name: &str, kind: Option<RubyMethodKind>) -> Option<NodeRef> {
    let methods =
        ruby_methods_of_kind_first(nodes_in_file.iter().filter(|n| n.kind == NodeKind::Method && n.name == method_name).cloned().collect(), kind);
    let methods: Vec<&NodeRef> = methods.iter().collect();
    let exact = format!("{}::{}", class_node.qualified_name, method_name);
    let by_name = format!("{}::{}", class_node.name, method_name);
    methods
        .iter()
        .find(|n| n.qualified_name == exact)
        .or_else(|| methods.iter().find(|n| n.qualified_name == by_name || n.qualified_name.ends_with(&format!("::{by_name}"))))
        .map(|n| (*n).clone())
}

/// A class's method by name, own first, then inherited.
pub fn class_strategy_method(class_node: &NodeRef, method_name: &str, ctx: &Ctx, kind: Option<RubyMethodKind>) -> Option<NodeRef> {
    find_own_method(&ctx.get_nodes_in_file(&class_node.file_path), class_node, method_name, kind)
        .or_else(|| inherited_class_method(std::slice::from_ref(class_node), method_name, ctx, kind))
}
