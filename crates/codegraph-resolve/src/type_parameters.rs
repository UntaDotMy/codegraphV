//! Declared type parameters — port of `src/resolution/type-parameters.ts`.
//!
//! A name a generic declaration introduces (`T` in `class Box<T>`) is not the namesake class
//! elsewhere in the project: a reference to it from inside the declaration is dropped. Scala
//! adds the value parameters of a def and the scope of members, C++ the `template<…>` header.

use crate::ctx::Ctx;
use crate::types::{NodeRef, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_types::jscompat::{utf16_from, utf16_slice};
use codegraph_types::{EdgeKind, Language, Node, NodeKind, ReferenceKind};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

const HEAD_LINES: usize = 40;

fn is_generic_language(l: Language) -> bool {
    matches!(
        l,
        Language::Scala
            | Language::Java
            | Language::Kotlin
            | Language::CSharp
            | Language::TypeScript
            | Language::Tsx
            | Language::Swift
            | Language::Rust
            | Language::Dart
            | Language::Go
            | Language::Cpp
            | Language::VbNet
            | Language::ArkTs
    )
}

fn is_type_ref_kind(k: ReferenceKind) -> bool {
    matches!(
        k,
        ReferenceKind::Edge(EdgeKind::References | EdgeKind::TypeOf | EdgeKind::Returns | EdgeKind::Instantiates)
    )
}

fn is_declaration_kind(k: NodeKind) -> bool {
    matches!(
        k,
        NodeKind::Class
            | NodeKind::Struct
            | NodeKind::Interface
            | NodeKind::Trait
            | NodeKind::Protocol
            | NodeKind::Enum
            | NodeKind::TypeAlias
            | NodeKind::Function
            | NodeKind::Method
            | NodeKind::Union
            | NodeKind::Module
    )
}

fn is_scala_owner_kind(k: NodeKind) -> bool {
    matches!(k, NodeKind::Class | NodeKind::Trait | NodeKind::Module | NodeKind::Interface | NodeKind::Struct | NodeKind::Enum)
}

fn is_scala_type_kind(k: NodeKind) -> bool {
    matches!(k, NodeKind::Class | NodeKind::Trait | NodeKind::Interface | NodeKind::Struct | NodeKind::Enum | NodeKind::TypeAlias)
}

type TypeParams = Rc<HashMap<String, Option<String>>>;

#[derive(Default)]
struct Memos {
    types: RefCell<HashMap<String, TypeParams>>,
    values: RefCell<HashMap<String, Rc<HashSet<String>>>>,
}

pub fn gate_type_parameter(resolved: Option<ResolvedRef>, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let mut result = resolved?;
    if r.language == Language::Scala
        && matches!(r.reference_kind, ReferenceKind::Edge(EdgeKind::Calls | EdgeKind::References))
        && re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name)
    {
        for declaration in enclosing_declarations(r, ctx) {
            if matches!(declaration.kind, NodeKind::Function | NodeKind::Method)
                && declared_value_parameters(&declaration, ctx).contains(&r.reference_name)
            {
                return None;
            }
        }
    }
    if r.language == Language::Scala && !scala_member_in_scope(&result, r, ctx) {
        let target = ctx.node_by_id(&result.target_node_id)?;
        if !is_scala_type_kind(target.kind) {
            return None;
        }
        let in_scope: Vec<NodeRef> = ctx
            .get_nodes_by_name(&r.reference_name)
            .iter()
            .filter(|n| n.id != target.id && n.language == Language::Scala && is_scala_type_kind(n.kind))
            .filter(|n| {
                let mut probe = result.clone();
                probe.target_node_id = n.id.clone();
                scala_member_in_scope(&probe, r, ctx)
            })
            .cloned()
            .collect();
        let chosen = if in_scope.len() == 1 { Some(in_scope[0].clone()) } else { in_scope.iter().find(|n| n.file_path == r.file_path).cloned() };
        result.target_node_id = chosen?.id.clone();
    }
    if r.language == Language::Cpp {
        let qualifier = re!(r"^([A-Za-z_]\w*)::").captures(&r.reference_name).and_then(|c| c.text(1).map(str::to_string));
        if let Some(qualifier) = qualifier {
            for declaration in enclosing_declarations(r, ctx) {
                let parameters = declared_type_parameters(&declaration, ctx);
                if !parameters.contains_key(&qualifier) {
                    continue;
                }
                return if is_member_of_default(&result, parameters.get(&qualifier).and_then(|v| v.as_deref()), ctx) {
                    Some(result)
                } else {
                    None
                };
            }
        }
    }
    if !is_generic_language(r.language) || !is_type_ref_kind(r.reference_kind) {
        return Some(result);
    }
    let name = &r.reference_name;
    if !re!(r"^[A-Z][A-Za-z0-9_]*$").is_match(name) {
        return Some(result);
    }
    for declaration in enclosing_declarations(r, ctx) {
        if declared_type_parameters(&declaration, ctx).contains_key(name) {
            return None;
        }
    }
    Some(result)
}

fn is_member_of_default(result: &ResolvedRef, fallback: Option<&str>, ctx: &Ctx) -> bool {
    let ty = fallback.and_then(|f| re!(r"([A-Za-z_]\w*)\s*(?:<[\s\S]*>)?\s*$").captures(f).and_then(|c| c.text(1).map(str::to_string)));
    let Some(ty) = ty.filter(|t| !t.is_empty()) else { return false };
    let qn = ctx.node_by_id(&result.target_node_id).map(|n| n.qualified_name.clone()).unwrap_or_default();
    let segments: Vec<&str> = if qn.is_empty() { Vec::new() } else { qn.split("::").collect() };
    segments.len() >= 2 && segments[segments.len() - 2] == ty
}

fn scala_member_in_scope(result: &ResolvedRef, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if !re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name) || !r.is_kind(EdgeKind::References) {
        return true;
    }
    let line: Option<String> = ctx
        .get_file_lines(&r.file_path)
        .and_then(|l| l.get((r.line as usize).wrapping_sub(1)).cloned())
        .or_else(|| {
            ctx.read_file(&r.file_path)
                .and_then(|t| re!(r"\r?\n").split(&t).get((r.line as usize).wrapping_sub(1)).map(|s| s.to_string()))
        });
    let Some(line) = line else { return true };
    if !utf16_from(&line, r.column as usize).starts_with(r.reference_name.as_str())
        || re!(r"\.\s*$").is_match(utf16_slice(&line, 0, r.column as usize))
    {
        return true;
    }
    let Some(target) = ctx.node_by_id(&result.target_node_id) else { return true };
    if target.language != Language::Scala {
        return true;
    }
    let Some(cut) = target.qualified_name.rfind("::") else { return true };
    let owner_qn = &target.qualified_name[..cut];
    let containers: Vec<NodeRef> = ctx
        .get_nodes_by_qualified_name(owner_qn)
        .iter()
        .filter(|o| o.file_path == target.file_path && o.start_line <= target.start_line && o.end_line >= target.end_line)
        .cloned()
        .collect();
    let inside = |o: &Node| r.file_path == o.file_path && r.line >= o.start_line && r.line <= o.end_line;
    if let Some(def) = containers.iter().find(|o| matches!(o.kind, NodeKind::Function | NodeKind::Method)) {
        return inside(def);
    }
    let Some(owner) = containers.iter().find(|o| is_scala_owner_kind(o.kind)) else { return true };
    if inside(owner) {
        return true;
    }
    for around in enclosing_declarations(r, ctx) {
        if !is_scala_owner_kind(around.kind) {
            continue;
        }
        if around.name == owner.name {
            return true;
        }
        if ctx.get_supertypes(&around.name, Language::Scala).contains(&owner.name) {
            return true;
        }
    }
    let text = ctx.read_file(&r.file_path).map(|t| t.to_string()).unwrap_or_default();
    let ident = re!(r"^[A-Za-z_]\w*$");
    if !(ident.is_match(&owner.name) && ident.is_match(&target.name)) {
        return true;
    }
    let (o, t) = (codegraph_jsre::escape_regex(&owner.name), codegraph_jsre::escape_regex(&target.name));
    let pat = format!(r"\bimport\s+[\w.]*\b{o}\.(?:_|\*|given\b|\{{[^}}]*\b(?:_|\*|{t})\b[^}}]*\}}|{t}\b)");
    Re::cached(&pat, "").is_match(&text)
}

fn enclosing_declarations(r: &UnresolvedRef, ctx: &Ctx) -> Vec<NodeRef> {
    ctx.get_nodes_in_file(&r.file_path)
        .iter()
        .filter(|n| is_declaration_kind(n.kind) && n.start_line <= r.line && n.end_line >= r.line)
        .cloned()
        .collect()
}

fn head_of(node: &Node, ctx: &Ctx) -> Option<(Rc<Vec<String>>, Vec<char>)> {
    let lines = ctx.get_file_lines(&node.file_path).or_else(|| {
        ctx.read_file(&node.file_path).map(|t| Rc::new(re!(r"\r?\n").split(&t).into_iter().map(|s| s.to_string()).collect::<Vec<_>>()))
    })?;
    let from = (node.start_line as usize).saturating_sub(1);
    let to = (node.end_line as usize).min(from + HEAD_LINES);
    let head: String = lines.iter().skip(from).take(to.saturating_sub(from)).cloned().collect::<Vec<_>>().join("\n");
    Some((lines, head.chars().collect()))
}

fn declared_type_parameters(node: &Node, ctx: &Ctx) -> TypeParams {
    let memo = ctx.memo::<Memos>();
    if let Some(hit) = memo.types.borrow().get(&node.id) {
        return Rc::clone(hit);
    }
    let mut names: HashMap<String, Option<String>> = HashMap::new();
    if let Some((lines, head)) = head_of(node, ctx) {
        let name: Vec<char> = node.name.chars().collect();
        let at = name_index(&head, &name);
        if let Some(at) = at.filter(|_| node.language != Language::Cpp) {
            let mut open = at + name.len();
            while open < head.len() && matches!(head[open], ' ' | '\t') {
                open += 1;
            }
            if open < head.len() && (head[open] == '<' || head[open] == '[') {
                collect(&head, open, &mut names, false);
            }
            let before = &head[..at];
            for i in 0..before.len() {
                if before[i] == '<' && (i == 0 || is_js_ws(before[i - 1]) || matches!(before[i - 1], '(' | ',')) {
                    collect(before, i, &mut names, false);
                }
            }
        }
        if node.language == Language::Go && node.kind == NodeKind::Method {
            let text: String = head.iter().collect();
            if let Some(m) = re!(r"^\s*func\s*\(\s*(?:[A-Za-z_]\w*\s+)?\*?\s*[A-Za-z_]\w*\s*\[").find(&text) {
                collect(&head, m.as_str().chars().count() - 1, &mut names, false);
            }
        }
        if node.language == Language::Cpp {
            let from = (node.start_line as usize).saturating_sub(3);
            let to = node.start_line as usize;
            let above: Vec<char> = lines.iter().skip(from).take(to.saturating_sub(from)).cloned().collect::<Vec<_>>().join("\n").chars().collect();
            let above_s: String = above.iter().collect();
            for m in re!(r"\btemplate\s*<", "g").captures_iter(&above_s) {
                let end_char = above_s[..m.get(0).unwrap().end()].chars().count();
                collect(&above, end_char - 1, &mut names, true);
            }
        }
    }
    let names = Rc::new(names);
    memo.types.borrow_mut().insert(node.id.clone(), Rc::clone(&names));
    names
}

fn declared_value_parameters(node: &Node, ctx: &Ctx) -> Rc<HashSet<String>> {
    let memo = ctx.memo::<Memos>();
    if let Some(hit) = memo.values.borrow().get(&node.id) {
        return Rc::clone(hit);
    }
    let mut names: HashSet<String> = HashSet::new();
    if let Some((_, head)) = head_of(node, ctx) {
        let name: Vec<char> = node.name.chars().collect();
        if let Some(at) = name_index(&head, &name) {
            let mut i = at + name.len();
            let skip_space = |i: &mut usize| {
                while *i < head.len() && is_js_ws(head[*i]) {
                    *i += 1;
                }
            };
            skip_space(&mut i);
            if i < head.len() && head[i] == '[' {
                i = match matching_bracket(&head, i) {
                    Some(close) => close + 1,
                    None => head.len(),
                };
            }
            skip_space(&mut i);
            while i < head.len() && head[i] == '(' {
                let Some(close) = matching_bracket(&head, i) else { break };
                let inner: Vec<char> = head[i + 1..close].to_vec();
                for item in split_top_level(&inner, false) {
                    let item: String = item.iter().collect();
                    let bare = re!(r"^\s*(?:@[\w.]+(?:\([^()]*\))?\s*)+").replace_first(&item, |_| String::new());
                    let param = re!(r"^\s*(?:(?:implicit|using|val|var|override|private|protected|final|inline|erased)\s+)*([A-Za-z_]\w*)\s*:")
                        .captures(&bare)
                        .and_then(|c| c.text(1).map(str::to_string));
                    if let Some(p) = param {
                        names.insert(p);
                    }
                }
                i = close + 1;
                skip_space(&mut i);
            }
        }
    }
    let names = Rc::new(names);
    memo.values.borrow_mut().insert(node.id.clone(), Rc::clone(&names));
    names
}

fn is_js_ws(c: char) -> bool {
    codegraph_types::jscompat::is_js_space(c)
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

fn name_index(head: &[char], name: &[char]) -> Option<usize> {
    if name.is_empty() {
        return None;
    }
    let mut at = 0usize;
    while at + name.len() <= head.len() {
        if head[at..at + name.len()] == *name {
            let before = if at > 0 { Some(head[at - 1]) } else { None };
            let after = head.get(at + name.len()).copied();
            if !before.is_some_and(is_word) && !after.is_some_and(is_word) {
                return Some(at);
            }
        }
        at += 1;
    }
    None
}

fn collect(text: &[char], open: usize, names: &mut HashMap<String, Option<String>>, last_word: bool) {
    let Some(close) = matching_bracket(text, open) else { return };
    let inner: Vec<char> = text[open + 1..close].to_vec();
    for item in split_top_level(&inner, text[open] == '<') {
        let item: String = item.iter().collect();
        if last_word {
            let at = item.find('=');
            let head_part = match at {
                Some(a) => &item[..a],
                None => &item[..],
            };
            let param = re!(r"([A-Za-z_]\w*)\s*$").captures(head_part).and_then(|c| c.text(1).map(str::to_string));
            if let Some(p) = param {
                if p != "typename" && p != "class" && p != "struct" {
                    names.insert(p, at.map(|a| codegraph_types::jscompat::js_trim(&item[a + 1..]).to_string()));
                }
            }
            continue;
        }
        let bare = re!(r"^\s*(?:@[\w.]+(?:\([^()]*\))?\s*)+").replace_first(&item, |_| String::new());
        let param = re!(r"^\s*(?:(?:in|out|reified|const|typename|class|struct)\s+|[+\-']\s*)*([A-Za-z_]\w*)")
            .captures(&bare)
            .and_then(|c| c.text(1).map(str::to_string));
        if let Some(p) = param {
            if p != "typename" && p != "class" && !names.contains_key(&p) {
                names.insert(p, None);
            }
        }
    }
}

fn matching_bracket(text: &[char], open: usize) -> Option<usize> {
    let angle = text[open] == '<';
    let closer = if angle {
        '>'
    } else if text[open] == '[' {
        ']'
    } else {
        ')'
    };
    let mut depth = 0i32;
    for i in open..text.len() {
        let c = text[i];
        if c == '{' || c == ';' {
            return None;
        }
        if is_opener(c, angle) {
            depth += 1;
        } else if is_closer(c, angle, if i > 0 { Some(text[i - 1]) } else { None }) {
            depth -= 1;
            if depth == 0 {
                return if c == closer { Some(i) } else { None };
            }
        }
    }
    None
}

fn split_top_level(inner: &[char], angle: bool) -> Vec<Vec<char>> {
    let mut out: Vec<Vec<char>> = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for i in 0..inner.len() {
        let c = inner[i];
        if is_opener(c, angle) {
            depth += 1;
        } else if is_closer(c, angle, if i > 0 { Some(inner[i - 1]) } else { None }) {
            depth -= 1;
        } else if c == ',' && depth == 0 {
            out.push(inner[start..i].to_vec());
            start = i + 1;
        }
    }
    out.push(inner[start..].to_vec());
    out
}

fn is_opener(c: char, angle: bool) -> bool {
    c == '[' || c == '(' || (angle && c == '<')
}

fn is_closer(c: char, angle: bool, previous: Option<char>) -> bool {
    if c == ']' || c == ')' {
        return true;
    }
    angle && c == '>' && previous != Some('-') && previous != Some('=')
}
