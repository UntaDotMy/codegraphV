//! What type a member, a local or a call result has, and the member a type (or one it inherits)
//! declares. Port of the middle of `src/resolution/vbnet-receivers.ts`.

use super::project::*;
use super::text::*;
use crate::ctx::{js_lower, Ctx};
use crate::types::{NodeRef, Tri, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::{Node, NodeKind};

/// A member found on a type, with what the type parameters of the type declaring it stand for.
pub struct Found {
    pub node: NodeRef,
    pub args: TypeArgs,
}

/// The type a field or property declares itself with, read from its declaration.
fn member_declared_type(member: &Node, ctx: &Ctx) -> Option<VbType> {
    let code = code_slice(&member.file_path, member.start_line as i64 - 1, member.start_line as i64 + 2, ctx);
    let r = escape_regex(&member.name);
    if let Some(property) = Re::cached(&format!(r"\bProperty\s+{r}\b\s*"), "i").captures(&code) {
        let mut at = property.get(0)?.end();
        if code.as_bytes().get(at) == Some(&b'(') {
            at = close_paren(&code, at)? + 1;
        }
        let as_clause = re!(r"^\s*As\s+(New\s+)?", "i").captures(&code[at..]);
        let ty = match &as_clause {
            Some(a) => read_type(&code, at + a.get(0)?.end()),
            None => None,
        };
        return match (ty, as_clause.as_ref().and_then(|a| a.text(1))) {
            (Some(t), Some(_)) => Some(VbType { array: false, ..t }),
            (t, _) => t,
        };
    }
    if let Some(VbBinding::Type(t)) = binding_on(&code, &member.name) {
        return Some(t);
    }
    // `Private x = New Foo()`: a field's initializer is what it holds.
    let init = Re::cached(&format!(r"(?<![\w.]){r}\s*=(?!=)"), "i").captures(&code);
    match init.map(|i| value_binding(&code[i.get(0).map(|m| m.end()).unwrap_or(0)..])) {
        Some(VbBinding::Type(t)) => Some(t),
        _ => None,
    }
}

/// The type a method, function or property returns, read from its declaration; none for a `Sub` or none written.
pub fn declared_return_type(n: &Node, ctx: &Ctx) -> Option<VbType> {
    let code = code_slice(&n.file_path, n.start_line as i64 - 1, n.start_line as i64 + 6, ctx);
    let head = Re::cached(&format!(r"\b(Function|Property|Sub)\s+{}\b", escape_regex(&n.name)), "i").captures(&code)?;
    if re!(r"^Sub$", "i").is_match(head.text(1).unwrap_or("")) {
        return None;
    }
    let mut at = head.get(0)?.end();
    // `(Of T)`, then the parameter list.
    for _ in 0..2 {
        let Some(open) = re!(r"^\s*\(").captures(&code[at..]) else { break };
        at = close_paren(&code, at + open.get(0)?.end() - 1)? + 1;
    }
    let as_clause = re!(r"^\s*As\s+", "i").captures(&code[at..])?;
    read_type(&code, at + as_clause.get(0)?.end())
}

/// What a type means where it is written: a type parameter of the method (`owner`) or of a type
/// around it stands for its constraint — none when it has none, as its value's type is then
/// unknown — and any other name for itself.
pub fn resolve_type_parameter(t: Option<VbType>, owner: Option<&Node>, ctx: &Ctx) -> Option<VbType> {
    let passthrough = match &t {
        None => true,
        Some(ty) => ty.array || is_builtin(ty) || ty.qualifier.is_some() || ty.file.is_none(),
    };
    if passthrough {
        return t;
    }
    let ty = t.as_ref().unwrap();
    let file = ty.file.as_deref().unwrap_or("");
    let key = js_lower(&ty.name);
    if let Some(o) = owner.filter(|o| o.kind == NodeKind::Method) {
        if let Some(found) = type_parameters_of(o, ctx).get(&key) {
            return found.clone();
        }
    }
    for decl in types_around(file, ty.line.unwrap_or(0), ctx) {
        if let Some(found) = type_parameters_of(&decl, ctx).get(&key) {
            return found.clone();
        }
    }
    t
}

/// The type a value member — a field, a property, or a function read without parentheses — has,
/// as written at it; a type parameter of its class is what `args` gives it where it is reached from.
pub fn value_member_type(member: &Node, ctx: &Ctx, args: &TypeArgs) -> Option<VbType> {
    let declared = if member.kind == NodeKind::Method { declared_return_type(member, ctx) } else { member_declared_type(member, ctx) };
    let ty = sited(declared, &member.file_path, member.start_line);
    if let Some(given) = ty.as_ref().and_then(|t| substitute_found(t, args)) {
        return Some(given);
    }
    resolve_type_parameter(ty, Some(member), ctx)
}

fn is_value_or_method(n: &Node) -> bool {
    VB_VALUE_KINDS.contains(&n.kind) || n.kind == NodeKind::Method
}

/// The member a receiver-less `name` reads where a call is: a field, property or function of a
/// class around the call or of one it inherits (with what that class's type parameters stand for
/// there), else of a `Module`.
pub fn member_around(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<Found> {
    for around in types_around(&r.file_path, r.line, ctx) {
        for ancestor in ancestry(&around, ctx).iter() {
            if let Some(member) = members_named(&ancestor.node, name, ctx).iter().find(|n| is_value_or_method(n)) {
                return Some(Found { node: NodeRef::clone(member), args: ancestor.args.clone() });
            }
        }
    }
    let global = module_members_named(name, r, ctx).into_iter().find(|n| is_value_or_method(n))?;
    Some(Found { node: global, args: TypeArgs::new() })
}

/// The type of what a receiver-less `name` reads where a call is (see `member_around`). Null when
/// it declares no type the source shows, undefined when nothing has the name.
fn member_type(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Tri<VbType> {
    match member_around(name, r, ctx) {
        Some(member) => Tri::from(value_member_type(&member.node, ctx, &member.args)),
        None => Tri::Undefined,
    }
}

/// The project types `t` names where it is written, none when the name leaves different types (a
/// guess between them would be no better than none), or an empty list for an array or a built-in type.
pub fn owners_of(t: &VbType, ctx: &Ctx) -> Option<Vec<NodeRef>> {
    if t.array || is_builtin(t) {
        return Some(Vec::new());
    }
    let found = types_named_at(t, ctx, &VB_TYPE_KINDS);
    if found.ambiguous {
        None
    } else {
        Some(found.owners)
    }
}

/// A method — or, `with_values`, a field or property; or what `fits` — named `name` on one of
/// `owners` (the type `typed` names) or a project type they inherit, with what the type
/// parameters of the type declaring it stand for.
pub fn member_on(
    owners: &[NodeRef],
    name: &str,
    r: &UnresolvedRef,
    ctx: &Ctx,
    with_values: bool,
    typed: Option<&VbType>,
    fits: Option<&dyn Fn(&Node) -> bool>,
) -> Option<Found> {
    let default_fits = move |n: &Node| n.kind == NodeKind::Method || (with_values && VB_VALUE_KINDS.contains(&n.kind));
    let fits: &dyn Fn(&Node) -> bool = match fits {
        Some(f) => f,
        None => &default_fits,
    };
    for level in [0, 1] {
        for owner in prefer_vb_project(owners.to_vec(), r, ctx) {
            // What the owner's own parameters are, as the receiver's type writes them (`Repo(Of Foo)`).
            let given: TypeArgs = type_parameters_of(&owner, ctx)
                .keys()
                .enumerate()
                .filter_map(|(i, p)| typed.and_then(|t| t.args.as_ref()).and_then(|a| a.get(i)).map(|a| (p.clone(), a.clone())))
                .collect();
            let ancestors = ancestry(&owner, ctx);
            let slice: &[VbAncestor] = if level == 0 { &ancestors[..ancestors.len().min(1)] } else { &ancestors[ancestors.len().min(1)..] };
            for ancestor in slice {
                let matching: Vec<NodeRef> = members_named(&ancestor.node, name, ctx).iter().filter(|n| fits(n)).cloned().collect();
                if let Some(found) = prefer_vb_project(matching, r, ctx).into_iter().next() {
                    let args = if level == 0 {
                        given
                    } else {
                        ancestor.args.iter().map(|(p, t)| (p.clone(), substitute(t, &given))).collect()
                    };
                    return Some(Found { node: found, args });
                }
            }
        }
    }
    None
}

fn is_me(receiver: &str) -> bool {
    re!(r"^(?:Me|MyClass|MyBase)$", "i").is_match(receiver)
}

/// The type of the value `Member(…)` / `obj.Member(…)` gives, from the member's declaration.
fn call_result_type(receiver: Option<&str>, member_name: &str, r: &UnresolvedRef, ctx: &Ctx, depth: u32) -> Option<VbType> {
    let mut member: Option<Found> = None;
    if receiver.is_none() || receiver.is_some_and(is_me) {
        let owners: Vec<NodeRef> = types_around(&r.file_path, r.line, ctx).into_iter().take(1).collect();
        if !owners.is_empty() {
            member = member_on(&owners, member_name, r, ctx, true, None, None);
        }
        if member.is_none() && receiver.is_none() {
            member = module_members_named(member_name, r, ctx)
                .into_iter()
                .find(|n| n.kind == NodeKind::Method || VB_VALUE_KINDS.contains(&n.kind))
                .map(|node| Found { node, args: TypeArgs::new() });
        }
    } else if let Some(receiver) = receiver {
        let ty = receiver_type(receiver, r, ctx, depth + 1);
        // Not a variable: a type or module named for a shared call (`ObjectStorage.Load()`).
        let owners = match &ty {
            Tri::Undefined => owners_of(&VbType::written_at(receiver, &r.file_path, r.line), ctx),
            Tri::Null => None,
            Tri::Value(t) => owners_of(t, ctx),
        };
        if let Some(owners) = owners.filter(|o| !o.is_empty()) {
            member = member_on(&owners, member_name, r, ctx, true, ty.as_value(), None);
        }
    }
    member.and_then(|m| value_member_type(&m.node, ctx, &m.args))
}

/// What a receiver named `name` is declared as where the call is: a local or parameter of its
/// member, else a field or property of its class, of one the class inherits, or of a `Module`.
/// Null when the receiver is bound but its type isn't known; undefined when nothing binds the
/// name (a type or module).
pub fn receiver_type(name: &str, r: &UnresolvedRef, ctx: &Ctx, depth: u32) -> Tri<VbType> {
    if depth > 2 {
        return Tri::Null;
    }
    let Some((binding, line)) = local_binding(name, r, ctx) else { return member_type(name, r, ctx) };
    let owner = ctx.node_by_id(&r.from_node_id);
    match binding {
        VbBinding::Type(t) => Tri::from(resolve_type_parameter(sited(Some(t), &r.file_path, line), owner.as_deref(), ctx)),
        VbBinding::Call { receiver, member } => {
            let at = UnresolvedRef { line, ..r.clone() };
            Tri::from(call_result_type(receiver.as_deref(), &member, &at, ctx, depth))
        }
        VbBinding::Each(collection) => {
            let at = UnresolvedRef { line, ..r.clone() };
            match receiver_type(&collection, &at, ctx, depth + 1) {
                Tri::Value(c) => Tri::from(element_type(&c)),
                _ => Tri::Null,
            }
        }
        VbBinding::Unknown => Tri::Null,
    }
}
