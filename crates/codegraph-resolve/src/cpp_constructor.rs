//! Local C++ object construction — port of `src/resolution/cpp-constructor.ts`.
//!
//! `T obj(args)` is extracted as a call to `ns::T::T/<argc>`. It binds to a constructor of the
//! lexically nearest `T` whose overload set admits that many arguments, or to nothing: a guess
//! by name would land on a namesake.

use crate::cpp_type_aliases::resolve_cpp_aliased_type;
use crate::ctx::Ctx;
use crate::types::{ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use std::collections::HashMap;
use std::rc::Rc;

pub fn is_cpp_constructor_ref(r: &UnresolvedRef) -> bool {
    r.language == Language::Cpp && r.is_kind(EdgeKind::Calls) && re!(r"::[^:]+/\d+$").is_match(&r.reference_name)
}

struct ConstructorShape {
    min: usize,
    max: Option<usize>,
    key: String,
}

fn constructor_shape(signature: Option<&str>) -> Option<ConstructorShape> {
    let signature = signature?;
    let signature = signature.strip_suffix(';').unwrap_or(signature);
    if !signature.starts_with('(') || !signature.ends_with(')') {
        return None;
    }
    let text = signature[1..signature.len() - 1].trim();
    if text.is_empty() || text == "void" {
        return Some(ConstructorShape { min: 0, max: Some(0), key: String::new() });
    }
    let mut parts: Vec<&str> = Vec::new();
    let mut start = 0;
    let mut depth = 0i32;
    let mut quote = 0u8;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if quote != 0 {
            if c == b'\\' {
                i += 1;
            } else if c == quote {
                quote = 0;
            }
            i += 1;
            continue;
        }
        if c == b'"' || c == b'\'' {
            quote = c;
            i += 1;
            continue;
        }
        if b"(<[{".contains(&c) {
            depth += 1;
        }
        if b")>]}".contains(&c) {
            depth -= 1;
        }
        if c == b',' && depth == 0 {
            parts.push(&text[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    if depth != 0 || quote != 0 {
        return None;
    }
    parts.push(&text[start..]);
    if parts.iter().any(|p| re!(r"[<>]=|==|!=").is_match(p)) {
        return None;
    }
    let variadic = parts.iter().any(|p| p.contains("..."));
    let types: Vec<String> = parts
        .iter()
        .map(|p| {
            let ty = p.split('=').next().unwrap_or("").trim();
            let ty = re!(r"(.*[\s*&>])([A-Za-z_]\w*)$").replace_first(ty, |c| {
                let whole = c.text(0).unwrap_or("");
                let prefix = c.text(1).unwrap_or("");
                let name = c.text(2).unwrap_or("");
                if re!(r"^(?:void|bool|char|short|int|long|float|double|signed|unsigned|const|volatile)$").is_match(name)
                    || re!(r"^(?:const|volatile|struct|class|enum)\s*$").is_match(prefix)
                {
                    whole.to_string()
                } else {
                    prefix.to_string()
                }
            });
            re!(r"\s+", "g").replace_all_str(&ty, "")
        })
        .collect();
    Some(ConstructorShape {
        key: types.join(","),
        min: parts.iter().filter(|p| !p.contains('=') && !p.contains("...")).count(),
        max: if variadic { None } else { Some(parts.len()) },
    })
}

struct Overload {
    nodes: Vec<Rc<Node>>,
    min: usize,
    max: Option<usize>,
}

/// Resolve a local construction `ns::T::T/<argc>` to the one constructor its arguments admit.
pub fn match_cpp_constructor(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let m = re!(r"^(.*)::([^:]+)/(\d+)$").captures(&r.reference_name)?;
    let raw_type = m.text(1).unwrap_or("");
    let ctor_name = m.text(2).unwrap_or("");
    let count = m.text(3).unwrap_or("0");
    let mut ty = raw_type.strip_prefix("::").unwrap_or(raw_type).to_string();
    if ty.rsplit("::").next() != Some(ctor_name) {
        return None;
    }
    let argc: usize = count.parse().unwrap_or(usize::MAX);
    let caller = ctx.node_by_id(&r.from_node_id);
    let mut scopes: Vec<String> =
        if raw_type.starts_with("::") { Vec::new() } else { caller.as_ref().map(|c| c.qualified_name.split("::").map(String::from).collect()).unwrap_or_default() };
    if !raw_type.starts_with("::") {
        match resolve_cpp_aliased_type(&ty, r, ctx) {
            Tri::Null => return None,
            Tri::Value(aliased) => {
                ty = aliased.target.iter().chain(aliased.rest.iter()).cloned().collect::<Vec<_>>().join("::");
                scopes = if aliased.scope.is_empty() { Vec::new() } else { aliased.scope.split("::").map(String::from).collect() };
            }
            Tri::Undefined => {}
        }
    }
    let name = ty.rsplit("::").next().unwrap_or("").to_string();
    let mut candidates: Vec<String> = Vec::new();
    for i in (1..=scopes.len()).rev() {
        candidates.push(format!("{}::{ty}", scopes[..i].join("::")));
    }
    candidates.push(ty.clone());
    for qualified in candidates {
        let has_owner = ctx
            .get_nodes_by_qualified_name(&qualified)
            .iter()
            .any(|n| n.language == Language::Cpp && matches!(n.kind, NodeKind::Class | NodeKind::Struct | NodeKind::Union));
        if !has_owner {
            continue;
        }
        let want = format!("{qualified}::{name}");
        let constructors: Vec<Rc<Node>> = ctx
            .get_nodes_by_name(&name)
            .iter()
            .filter(|n| n.language == Language::Cpp && n.kind == NodeKind::Method && n.qualified_name == want)
            .cloned()
            .collect();
        if constructors.iter().any(|n| re!(r"\binitializer_list\b").is_match(n.signature.as_deref().unwrap_or(""))) {
            return None;
        }
        let mut overloads: Vec<(String, Overload)> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for node in &constructors {
            let shape = constructor_shape(node.signature.as_deref())?;
            match index.get(&shape.key) {
                Some(&at) => {
                    let prior = &mut overloads[at].1;
                    prior.nodes.push(Rc::clone(node));
                    prior.min = prior.min.min(shape.min);
                }
                None => {
                    index.insert(shape.key.clone(), overloads.len());
                    overloads.push((shape.key, Overload { nodes: vec![Rc::clone(node)], min: shape.min, max: shape.max }));
                }
            }
        }
        let admitting: Vec<&Overload> =
            overloads.iter().map(|(_, o)| o).filter(|o| o.min <= argc && o.max.is_none_or(|max| argc <= max)).collect();
        if admitting.len() != 1 {
            return None;
        }
        let definitions: Vec<&Rc<Node>> = admitting[0].nodes.iter().filter(|n| !n.signature.as_deref().is_some_and(|s| s.ends_with(';'))).collect();
        let targets: Vec<&Rc<Node>> = if definitions.is_empty() { admitting[0].nodes.iter().collect() } else { definitions };
        if targets.len() != 1 {
            return None;
        }
        return Some(ResolvedRef::new(r, targets[0].id.clone(), 0.9, ResolvedBy::QualifiedName));
    }
    None
}
