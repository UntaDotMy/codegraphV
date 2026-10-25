//! A VB.NET call through a typed receiver: the type's own method, an inherited one, an indexed
//! field or property, or an extension method. Port of `matchVbTypedCall` and its helpers.

use super::members::*;
use super::project::*;
use super::text::*;
use crate::ctx::{js_lower, Ctx};
use crate::name_matcher::is_std_method_name;
use crate::name_matcher::util::{memo_map, MemoMap};
use crate::types::{AlsoTarget, NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::{EdgeKind, Node};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::LazyLock;

/// An extension method and the type it extends.
pub struct VbExtension {
    pub node: NodeRef,
    /// The type it extends; none when that is one of its own type parameters (it extends anything).
    pub param: Option<VbType>,
}

memo_map!(VbExtensions, String, Option<Rc<VbExtension>>);

/// A method's extension-method declaration (`<Extension> Function F(s As String, …)`), or none when it isn't one.
fn extension_of(n: &NodeRef, ctx: &Ctx) -> Option<Rc<VbExtension>> {
    if n.kind != codegraph_types::NodeKind::Method || n.language != codegraph_types::Language::VbNet {
        return None;
    }
    let memo = ctx.memo::<VbExtensions>();
    if let Some(hit) = memo.lookup(&n.id) {
        return hit;
    }
    let extension = read_extension(n, ctx).map(Rc::new);
    memo.store(n.id.clone(), extension.clone());
    extension
}

fn read_extension(n: &NodeRef, ctx: &Ctx) -> Option<VbExtension> {
    let code = code_slice(&n.file_path, (n.start_line as i64 - 2).max(0), n.start_line as i64 + 6, ctx);
    let decl = Re::cached(&format!(r"\b(?:Function|Sub)\s+{}\b", escape_regex(&n.name)), "i").captures(&code)?;
    let whole = decl.get(0)?;
    let attributes = re!(r"((?:<[^<>]*>\s*)+)(?:(?:Public|Friend|Private|Protected|Shared|Overloads|Async|Iterator)\s+)*$", "i")
        .captures(&code[..whole.start()])
        .and_then(|c| c.text(1).map(String::from))?;
    if !re!(r"\bExtension(?:Attribute)?\b", "i").is_match(&attributes) {
        return None;
    }
    let mut at = whole.end();
    let mut type_params: HashSet<String> = HashSet::new();
    if re!(r"^\s*\(\s*Of\b", "i").is_match(&code[at..]) {
        let open = at + code[at..].find('(')?;
        for p in split_args(&code, open).unwrap_or_default() {
            if let Some(tp) = re!(r"^(?:Of\s+)?(?:In\s+|Out\s+)?([A-Za-z_]\w*)", "i").captures(&p) {
                type_params.insert(js_lower(tp.text(1).unwrap_or("")));
            }
        }
        at = close_paren(&code, open)? + 1;
    }
    let open = re!(r"^\s*\(").captures(&code[at..]);
    let first: Option<String> = open
        .and_then(|o| split_args(&code, at + o.get(0)?.end() - 1))
        .and_then(|a| a.into_iter().next())
        .map(|f| re!(r"<[^<>]*>", "g").replace_all_str(&f, ""))
        .filter(|f| !f.is_empty());
    let first = first?;
    let as_clause = re!(r"\bAs\s+", "i").captures(&first)?;
    let ty = read_type(&first, as_clause.get(0)?.end())?;
    let param = if type_params.contains(&js_lower(&ty.name)) && !ty.array { None } else { Some(ty) };
    Some(VbExtension { node: Rc::clone(n), param })
}

/// The instance methods of .NET's everyday types: a call one of them answers is the type's own,
/// whatever extension of that name the project declares (`list.Sort()` on a `List(Of T)`), and a
/// name one of them lacks is no instance method there (`list.Join(", ")` is an extension's).
static BCL_INSTANCE_METHODS: LazyLock<HashMap<&'static str, HashSet<&'static str>>> = LazyLock::new(|| {
    // `Object`'s instance methods, which every type has.
    let object = ["tostring", "equals", "gethashcode", "gettype"];
    let table: [(&str, &[&str]); 6] = [
        (
            "string",
            &[
                "clone", "compareto", "contains", "copyto", "endswith", "getenumerator", "gettypecode", "indexof", "indexofany", "insert",
                "isnormalized", "lastindexof", "lastindexofany", "normalize", "padleft", "padright", "remove", "replace", "split", "startswith",
                "substring", "tochararray", "tolower", "tolowerinvariant", "toupper", "toupperinvariant", "trim", "trimend", "trimstart",
            ],
        ),
        (
            "stringbuilder",
            &["append", "appendformat", "appendjoin", "appendline", "clear", "copyto", "ensurecapacity", "getchunks", "insert", "remove", "replace"],
        ),
        (
            "list",
            &[
                "add", "addrange", "asreadonly", "binarysearch", "clear", "contains", "convertall", "copyto", "exists", "find", "findall",
                "findindex", "findlast", "findlastindex", "foreach", "getenumerator", "getrange", "indexof", "insert", "insertrange", "lastindexof",
                "remove", "removeall", "removeat", "removerange", "reverse", "sort", "toarray", "trimexcess", "trueforall",
            ],
        ),
        (
            "dictionary",
            &["add", "clear", "containskey", "containsvalue", "ensurecapacity", "getenumerator", "remove", "trimexcess", "tryadd", "trygetvalue"],
        ),
        (
            "hashset",
            &[
                "add", "clear", "contains", "copyto", "exceptwith", "getenumerator", "intersectwith", "ispropersubsetof", "ispropersupersetof",
                "issubsetof", "issupersetof", "overlaps", "remove", "removewhere", "setequals", "symmetricexceptwith", "trimexcess", "trygetvalue",
                "unionwith",
            ],
        ),
        (
            "[]",
            &["clone", "copyto", "getenumerator", "getlength", "getlonglength", "getlowerbound", "getupperbound", "getvalue", "initialize", "setvalue"],
        ),
    ];
    table.iter().map(|(k, ms)| (*k, ms.iter().chain(object.iter()).copied().collect())).collect()
});

/// The instance methods of a .NET type the table above knows; none for any other.
fn bcl_instance_methods(t: &VbType) -> Option<&'static HashSet<&'static str>> {
    let key = if t.array { "[]".to_string() } else { type_key(t) };
    BCL_INSTANCE_METHODS.get(key.as_str())
}

/// Whether a value of type `t` might reach an extension declared for `param` through a
/// conversion nothing here can check: `Object`, an array's collection interfaces, an interface a
/// built-in type implements, or any type an outside base type may lead to.
fn may_extend(t: &VbType, open: bool, param: &VbType) -> bool {
    if type_key(param) == "object" {
        return true;
    }
    if param.array != t.array {
        return t.array && re!(r"^(?:IEnumerable|IList|ICollection|IReadOnlyList|IReadOnlyCollection|Array)$", "i").is_match(&param.name);
    }
    if is_builtin(param) {
        return false;
    }
    if is_builtin(t) {
        return re!(r"^I[A-Z]").is_match(&param.name);
    }
    open
}

fn instance_method(r: &UnresolvedRef, target: &Node, confidence: f64) -> ResolvedRef {
    ResolvedRef::new(r, target.id.clone(), confidence, ResolvedBy::InstanceMethod)
}

/// The extension method a call on a value of type `t` reaches: one declared for `t` or for a type
/// it inherits — else, when exactly one other extension of that name could apply and the name is
/// no instance method of `t`'s (one of .NET's own names, for a type nothing here describes), that one.
fn extension_for(t: &VbType, owners: &[NodeRef], method: &str, r: &UnresolvedRef, ctx: &Ctx, is_std: &dyn Fn(&str) -> bool) -> Option<ResolvedRef> {
    let instance_methods = if owners.is_empty() { bcl_instance_methods(t) } else { None };
    if instance_methods.is_some_and(|m| m.contains(js_lower(method).as_str())) {
        return None;
    }
    let extensions: Vec<Rc<VbExtension>> = ctx.get_nodes_by_lower_name(&js_lower(method)).iter().filter_map(|n| extension_of(n, ctx)).collect();
    if extensions.is_empty() {
        return None;
    }
    // Every name the type goes by — its own, its base types', the interfaces they implement — and
    // whether one of them is an outside type, whose own ancestry nothing here shows.
    let mut names: HashSet<String> = HashSet::new();
    names.insert(type_key(&VbType::new(t.name.clone(), false)));
    let mut open = owners.is_empty() && !is_builtin(t) && !t.array;
    for owner in owners {
        for ty in hierarchy(owner, ctx) {
            names.insert(js_lower(&ty.name));
            for sup in supertypes_of(&ty, ctx).iter() {
                names.insert(type_key(sup));
                let found = types_named_at(sup, ctx, &VB_TYPE_KINDS).owners;
                if found.is_empty() {
                    open = true;
                } else if sup.implemented {
                    for i in &found {
                        for h in hierarchy(i, ctx) {
                            names.insert(js_lower(&h.name));
                        }
                    }
                }
            }
        }
    }
    let exact: Vec<NodeRef> = extensions
        .iter()
        .filter(|e| match &e.param {
            Some(p) => type_key(p) == type_key(t) || (!t.array && !p.array && names.contains(&type_key(p))),
            None => false,
        })
        .map(|e| Rc::clone(&e.node))
        .collect();
    if !exact.is_empty() {
        let target = prefer_vb_project(exact, r, ctx).into_iter().next()?;
        return Some(instance_method(r, &target, 0.85));
    }
    let loose: Vec<&Rc<VbExtension>> = extensions.iter().filter(|e| e.param.as_ref().is_none_or(|p| may_extend(t, open, p))).collect();
    if loose.len() == 1 && (instance_methods.is_some() || !is_std(method)) {
        return Some(instance_method(r, &loose[0].node, 0.7));
    }
    None
}

/// A read of `member` through one of `owners` (the parts of the type its receiver names): the
/// member, and the type as a second target — unless the read is written inside that type, which
/// doesn't depend on itself.
pub fn read_through(member: &Node, owners: &[NodeRef], r: &UnresolvedRef, ctx: &Ctx, edge_kind: Option<EdgeKind>, confidence: Option<f64>) -> Option<ResolvedRef> {
    if member.id == r.from_node_id {
        return None;
    }
    let owner = owners.iter().find(|o| o.file_path == member.file_path).or_else(|| owners.first())?;
    let from = ctx.node_by_id(&r.from_node_id).map(|n| js_lower(&n.qualified_name));
    let own = js_lower(&owner.qualified_name);
    let inside = owner.id == r.from_node_id || from.as_ref().is_some_and(|f| *f == own || f.starts_with(&format!("{own}::")));
    let mut resolved = ResolvedRef::new(r, member.id.clone(), confidence.unwrap_or(0.85), ResolvedBy::QualifiedName);
    if edge_kind.is_some() {
        resolved.edge_kind = edge_kind;
    }
    if !inside {
        resolved.also_targets = Some(vec![AlsoTarget { target_node_id: owner.id.clone(), metadata: None }]);
    }
    Some(resolved)
}

/// A call of `method` on a value of type `ty` (`owners`, the project type it names; none for an
/// outside type): the type's own method or one it inherits; an index into its field or property
/// (`x.Items(0)`), which VB.NET writes as a call, reads the member; else an extension method for
/// the type.
pub fn call_on_value(ty: &VbType, owners: &[NodeRef], method: &str, r: &UnresolvedRef, ctx: &Ctx, is_std: &dyn Fn(&str) -> bool) -> Option<ResolvedRef> {
    if !owners.is_empty() {
        if let Some(own) = member_on(owners, method, r, ctx, false, Some(ty), None) {
            return Some(instance_method(r, &own.node, 0.9));
        }
        if let Some(indexed) = member_on(owners, method, r, ctx, true, Some(ty), None) {
            if indexed.node.id == r.from_node_id {
                return None;
            }
            let mut resolved = instance_method(r, &indexed.node, 0.9);
            resolved.edge_kind = Some(EdgeKind::References);
            return Some(resolved);
        }
    }
    extension_for(ty, owners, method, r, ctx, is_std)
}

/// Resolve `receiver.method()` through the receiver's declared type: the type's own method or one
/// it inherits, else an extension method for it, else null — a typed receiver is never a guess. A
/// receiver naming one of the project's types or modules is a shared call on it. Undefined when
/// the type isn't known, or is `Object` (a late-bound call), for the name strategies.
pub fn match_vb_typed_call(receiver: &str, method: &str, r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    if !re!(r"^[A-Za-z_]\w*$").is_match(receiver) {
        return Tri::Undefined;
    }
    let is_std = |name: &str| is_std_method_name("vbnet", name);
    let ty = match receiver_type(receiver, r, ctx, 0) {
        Tri::Undefined => {
            // A type or module named for a shared call — `M3U8.Download(…)` in SCrawler's
            // `API.Reddit.UserData` is `API.Reddit.M3U8`'s, not another site's: its own member or
            // one it inherits.
            let named = types_named_at(&VbType::written_at(receiver, &r.file_path, r.line), ctx, &VB_TYPE_KINDS);
            if named.owners.is_empty() {
                return Tri::Undefined;
            }
            if named.ambiguous {
                return Tri::Null;
            }
            if let Some(shared) = member_on(&named.owners, method, r, ctx, false, None, None) {
                return Tri::Value(ResolvedRef::new(r, shared.node.id.clone(), 0.85, ResolvedBy::QualifiedName));
            }
            // `AppSession.Items(0)`: an index into a shared field or property, which VB.NET writes
            // as a call — a read of the member and of its type (#2305).
            return match member_on(&named.owners, method, r, ctx, true, None, None) {
                Some(indexed) => Tri::from(read_through(&indexed.node, &named.owners, r, ctx, Some(EdgeKind::References), None)),
                None => Tri::Null,
            };
        }
        Tri::Null => return Tri::Undefined,
        Tri::Value(t) => t,
    };
    if type_key(&ty) == "object" {
        return Tri::Undefined;
    }
    let Some(owners) = owners_of(&ty, ctx) else { return Tri::Null };
    // A type name that leaves two of the project's types: no guess between them (the `None` above).
    Tri::from(call_on_value(&ty, &owners, method, r, ctx, &is_std))
}
