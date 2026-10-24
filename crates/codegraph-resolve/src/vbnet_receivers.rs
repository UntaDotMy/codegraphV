//! VB.NET receivers, projects and receiver-less calls — port of `src/resolution/vbnet-receivers.ts`.
//!
//! The name matcher guesses a `receiver.Method()` call it cannot type by the method's name alone,
//! and a receiver-less call by the nearest same-named member. VB.NET brought none of the evidence
//! C# does to those guesses, so a call landed on whichever project method shared its name. What
//! this module reads, from the source:
//!
//! 1. A receiver's declared type — `Dim x As T`, `ByVal x As T`, `x As New T`, a type character, a
//!    `For Each` over a typed collection, a field or property of the class, of one it inherits (with
//!    the type arguments the subclass gives) or of a `Module`, or what `Dim x = obj.GetString(…)`
//!    returns. A typed receiver's call is the type's own method or one it inherits, else an
//!    extension method declared for the type, else nothing of the project's.
//! 2. Which type a name means where it is written: the namespaces around it, outward, then its
//!    file's and project's `Imports` (aliases included), then the project a file belongs to.
//! 3. A receiver-less call (or one on `Me` / `MyClass` / `MyBase`) is a member of a type around it,
//!    of what those inherit, or of a `Module`; a type nested in a class is named bare only inside it.
//!
//! VB.NET's keywords and names are matched without regard to case. Nothing here runs for another
//! language.

mod calls;
mod members;
mod project;
mod text;

pub use calls::match_vb_typed_call;
pub use project::{break_vb_tie, is_vb_type_qualified_by, prefer_vb_project, same_vb_project};

use crate::ctx::{js_lower, Ctx};
use crate::name_matcher::util::{before_col, line_at, memo_map, MemoMap};
use crate::types::{AlsoTarget, NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use calls::{call_on_value, read_through};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::{EdgeKind, Language, Node, NodeKind, ReferenceKind};
use members::*;
use project::*;
use std::collections::HashSet;
use std::rc::Rc;
use text::*;

/// What a read through a type names: a value it holds or gives (a field, a property, a constant,
/// an `Enum` case, an event), a method — which VB.NET runs when it is named without parentheses —
/// or a type nested in it.
const VB_READ_KINDS: [NodeKind; 11] = [
    NodeKind::Field,
    NodeKind::Property,
    NodeKind::Constant,
    NodeKind::Variable,
    NodeKind::EnumMember,
    NodeKind::Method,
    NodeKind::Class,
    NodeKind::Struct,
    NodeKind::Interface,
    NodeKind::Enum,
    NodeKind::TypeAlias,
];

/// What a read through a value names: a field, property, constant or event of its type, or a method it runs.
const VB_INSTANCE_READ_KINDS: [NodeKind; 5] = [NodeKind::Field, NodeKind::Property, NodeKind::Constant, NodeKind::Variable, NodeKind::Method];

/// Members that belong to a type, not an instance, besides those declared `Shared`: a constant, an `Enum` case, a nested type.
const VB_SHARED_READ_KINDS: [NodeKind; 7] =
    [NodeKind::Constant, NodeKind::EnumMember, NodeKind::Class, NodeKind::Struct, NodeKind::Interface, NodeKind::Enum, NodeKind::TypeAlias];

/// A member read (or a `With` block's call) as the extractor sends it: the receiver — a name,
/// `Me` / `MyClass` / `MyBase`, or `{T}` for a value of a type the code writes — then each member
/// read through it, the last the one used: `AppSession.SessionId`, `x.Normal`, `Me._h.Title`,
/// `{BoolParam}.Switch`.
fn member_path() -> &'static Re {
    re!(r"^(\{[^{}]+\}|\[?[A-Za-z_]\w*\]?)((?:\.\[?[A-Za-z_]\w*\]?)*)\.(\[?[A-Za-z_]\w*\]?)$")
}

/// Whether a reference is a VB.NET member read through a receiver, which `match_vb_member_read` alone resolves.
pub fn is_vb_member_read(r: &UnresolvedRef) -> bool {
    r.language == Language::VbNet && r.reference_kind == ReferenceKind::Edge(EdgeKind::References) && member_path().is_match(&r.reference_name)
}

/// Whether a reference is a VB.NET call through a receiver path, which only a `With` block sends
/// (`.Run()` in `With Me._h`, `With DirectCast(o, T)`) and `match_vb_path_call` alone resolves. A
/// call through a name (`x.Run`) is the name matcher's, which types it with `match_vb_typed_call`.
pub fn is_vb_path_call(r: &UnresolvedRef) -> bool {
    if r.language != Language::VbNet || r.reference_kind != ReferenceKind::Edge(EdgeKind::Calls) {
        return false;
    }
    match member_path().captures(&r.reference_name) {
        Some(m) => {
            let head = m.text(1).unwrap_or("");
            !m.text(2).unwrap_or("").is_empty() || head.starts_with('{') || is_me(head)
        }
        None => false,
    }
}

fn is_me(head: &str) -> bool {
    re!(r"^(?:Me|MyClass|MyBase)$", "i").is_match(head)
}

/// What a receiver path reaches: the project type whose members are read through it, and how.
struct VbReceiver {
    /// The parts of the project type the receiver names or holds a value of; none for an outside type.
    owners: Vec<NodeRef>,
    /// The type a value is declared as, with the type arguments it gives; absent through a type's name.
    typed: Option<VbType>,
    /// The receiver names a type (`AppSession`, `Outer.Mode`), whose own members are read, not a value.
    through_type: bool,
    /// The name is a value whose type has the same name (`theme As Theme`): VB.NET's "Color Color" rule.
    color_color: bool,
    /// The receiver names a namespace (`Billing`, `Acme.Billing`), whose types and Modules' members
    /// are read through it: its full segments. No owners.
    namespace: Option<Vec<String>>,
}

/// `Name` for an escaped `[Name]`.
fn unescaped(name: &str) -> String {
    if name.len() >= 2 && name.starts_with('[') && name.ends_with(']') {
        name[1..name.len() - 1].to_string()
    } else {
        name.to_string()
    }
}

/// A receiver holding a value of type `t`; none when that type isn't known, is `Object`
/// (late-bound), or names two types.
fn value_receiver(t: Option<VbType>, ctx: &Ctx) -> Option<VbReceiver> {
    let t = t?;
    if type_key(&t) == "object" {
        return None;
    }
    let owners = owners_of(&t, ctx)?;
    Some(VbReceiver { owners, typed: Some(t), through_type: false, color_color: false, namespace: None })
}

/// What a receiver path's first link is where a member is read through it: `Me` / `MyClass` the
/// type around the read, `MyBase` the class that type inherits, `{T}` a value of `T`. A name holds
/// a value of its declared type when it is a local, a parameter, or a member of a type around the
/// read or of a Module; any other name means the class, module, structure, interface or enum it
/// names there, else a namespace (`Billing`, the root namespace `Acme`). A value whose type has
/// its name (`Public Property Settings As Settings`, `theme As Theme`) may mean either, by
/// VB.NET's "Color Color" rule: the member read through it decides (`match_vb_member_read`).
fn receiver_at(head: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<VbReceiver> {
    if head.starts_with('{') {
        let owner = ctx.node_by_id(&r.from_node_id);
        let written = sited(read_type(&head[1..head.len() - 1], 0), &r.file_path, r.line);
        return value_receiver(resolve_type_parameter(written, owner.as_deref(), ctx), ctx);
    }
    if is_me(head) {
        let own = types_around(&r.file_path, r.line, ctx).into_iter().next()?;
        if re!(r"^MyBase$", "i").is_match(head) {
            return value_receiver(supertypes_of(&own, ctx).iter().find(|s| !s.implemented).cloned(), ctx);
        }
        let typed = VbType::new(own.name.clone(), false);
        return Some(VbReceiver { owners: vec![own], typed: Some(typed), through_type: false, color_color: false, namespace: None });
    }
    let name = unescaped(head);
    let bound = receiver_type(&name, r, ctx, 0);
    let written: Option<VbType> = match &bound {
        Tri::Undefined => Some(VbType::written_at(name.clone(), &r.file_path, r.line)),
        Tri::Value(b) if !b.array && js_lower(&b.name) == js_lower(&name) => Some(b.clone()),
        _ => None,
    };
    let Some(written) = written else { return value_receiver(bound.value(), ctx) };
    let found = types_named_at(&written, ctx, &VB_READ_TYPE_KINDS);
    if found.ambiguous {
        return None;
    }
    if !found.owners.is_empty() {
        return Some(VbReceiver {
            owners: found.owners,
            typed: None,
            through_type: true,
            color_color: matches!(bound, Tri::Value(_)),
            namespace: None,
        });
    }
    let namespace = if matches!(bound, Tri::Undefined) { namespace_named(&name, r, ctx) } else { None };
    namespace.map(|ns| VbReceiver { owners: Vec::new(), typed: None, through_type: false, color_color: false, namespace: Some(ns) })
}

/// What `name`, read through `at`, is as a receiver in turn: a value of the type a field,
/// property or function declares, or a type nested in a type named for it (`Outer.Mode`); through
/// a namespace, a type declared in it, a namespace nested in it, or the value one of its Modules'
/// members holds. None where the member, or its type, is not known here.
fn member_at(at: &VbReceiver, name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<VbReceiver> {
    if let Some(ns) = &at.namespace {
        let types = types_in_namespace(ns, name, r, ctx)?;
        if !types.is_empty() {
            return Some(VbReceiver { owners: types, typed: None, through_type: true, color_color: false, namespace: None });
        }
        let mut nested = ns.clone();
        nested.push(js_lower(name));
        if known_namespaces(ctx).contains(&nested.join(".")) {
            return Some(VbReceiver { owners: Vec::new(), typed: None, through_type: false, color_color: false, namespace: Some(nested) });
        }
        let member = module_member_in(ns, name, r, ctx)?;
        return value_receiver(value_member_type(&member, ctx, &TypeArgs::new()), ctx);
    }
    if at.owners.is_empty() {
        return None;
    }
    let through_type = at.through_type;
    let fits = move |n: &Node| VB_INSTANCE_READ_KINDS.contains(&n.kind) || (through_type && VB_READ_TYPE_KINDS.contains(&n.kind));
    let found = member_on(&at.owners, name, r, ctx, true, at.typed.as_ref(), Some(&fits))
        .or_else(|| member_on(&at.owners, &format!("[{name}]"), r, ctx, true, at.typed.as_ref(), Some(&fits)))?;
    if VB_READ_TYPE_KINDS.contains(&found.node.kind) {
        return Some(VbReceiver { owners: vec![found.node], typed: None, through_type: true, color_color: false, namespace: None });
    }
    value_receiver(value_member_type(&found.node, ctx, &found.args), ctx)
}

/// What the receiver path `head.links…` reaches (see `receiver_at` and `member_at`), or none where a link has no type here.
fn path_receiver(head: &str, links: &[String], r: &UnresolvedRef, ctx: &Ctx) -> Option<VbReceiver> {
    let mut at = receiver_at(head, r, ctx);
    for link in links {
        let current = at.as_ref()?;
        at = member_at(current, &unescaped(link), r, ctx);
    }
    at
}

/// Whether a method is named at a reference without being run: `AddressOf Type.Method`, `NameOf(Type.Method)`.
fn names_without_running(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let line = line_at(ctx, &r.file_path, r.line).unwrap_or_default();
    re!(r"\b(?:AddressOf\s+|NameOf\s*\(\s*)$", "i").is_match(before_col(&line, r.column))
}

memo_map!(VbNoArguments, String, bool);

/// Whether VB.NET can call a method with no arguments: it declares no parameter, or only
/// `Optional` ones and a `ParamArray`. Read from its declaration; true when that can't be read.
fn takes_no_arguments(n: &Node, ctx: &Ctx) -> bool {
    let memo = ctx.memo::<VbNoArguments>();
    if let Some(hit) = memo.lookup(&n.id) {
        return hit;
    }
    let code = code_slice(&n.file_path, n.start_line as i64 - 1, n.start_line as i64 + 6, ctx);
    let pattern = format!(r"\b(?:Sub|Function)\s+\[?{}\]?(?![\w$%&!#@])", escape_regex(&unescaped(&n.name)));
    let head = Re::cached(&pattern, "i").captures(&code);
    let mut none = true;
    if let Some(head) = head {
        let mut at = head.get(0).map(|m| m.end()).unwrap_or(0);
        // `Function Parse(Of T)(text As String)`: the type parameters come first.
        if re!(r"^\s*\(\s*Of\b", "i").is_match(&code[at..]) {
            at = code[at..].find('(').and_then(|p| close_paren(&code, at + p)).map(|c| c + 1).unwrap_or(0);
        }
        let open = if at > 0 { re!(r"^\s*\(").captures(&code[at..]) } else { None };
        let params = open.and_then(|o| split_args(&code, at + o.get(0).map(|m| m.end()).unwrap_or(1) - 1));
        none = match params {
            None => true,
            Some(params) => params.iter().all(|p| {
                let stripped = re!(r"<[^<>]*>", "g").replace_all_str(p, "");
                let param = re!(r"^[\s_]+").replace_first(&stripped, |_| String::new());
                param.is_empty() || re!(r"^(?:(?:ByVal|ByRef)\s+)*(?:Optional|ParamArray)\b", "i").is_match(&param)
            }),
        };
    }
    memo.store(n.id.clone(), none);
    none
}

/// The overload of `member` (a method) that VB.NET runs where a reference names it without
/// parentheses: one that takes no arguments — `Me.Load` is `Load(Optional force = False)`, not
/// `Load(path)` — and never the member the reference is in: a constructor's `Me.New` is another
/// constructor. `member` itself when its overloads don't tell; none when the only one is the
/// member the reference is in.
fn runnable_overload(member: &NodeRef, r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    let owner = member
        .qualified_name
        .rfind("::")
        .filter(|cut| *cut > 0)
        .and_then(|cut| ctx.get_nodes_by_qualified_name(&member.qualified_name[..cut]).iter().find(|n| n.language == Language::VbNet && VB_TYPE_KINDS.contains(&n.kind)).cloned());
    let project = project_of(&member.file_path, ctx);
    let overloads: Vec<NodeRef> = match &owner {
        Some(o) => members_named(o, &member.name, ctx).iter().filter(|n| n.kind == NodeKind::Method && project_of(&n.file_path, ctx) == project).cloned().collect(),
        None => Vec::new(),
    };
    let others: Vec<NodeRef> = overloads.into_iter().filter(|n| n.id != r.from_node_id).collect();
    if others.is_empty() {
        return if member.id == r.from_node_id { None } else { Some(Rc::clone(member)) };
    }
    let runnable: Vec<NodeRef> = others.iter().filter(|n| takes_no_arguments(n, ctx)).cloned().collect();
    let pool = if runnable.is_empty() { others } else { runnable };
    pool.iter().find(|n| n.id == member.id).or_else(|| pool.first()).cloned()
}

memo_map!(VbNamesakes, String, bool);

/// Whether a member is declared as a type of its own name that isn't one of the project's
/// (`Responser As Responser`), or names two of them: a read through its name may be one through
/// that type, which nothing here can tell.
fn is_outside_namesake(member: &Node, args: &TypeArgs, ctx: &Ctx) -> bool {
    let memo = ctx.memo::<VbNamesakes>();
    // What a type parameter stands for depends on where the member is reached from.
    if args.is_empty() {
        if let Some(hit) = memo.lookup(&member.id) {
            return hit;
        }
    }
    let declared = value_member_type(member, ctx, args);
    let mut namesake = false;
    if let Some(d) = &declared {
        if !d.array && js_lower(&d.name) == js_lower(&unescaped(&member.name)) {
            let types = types_named_at(d, ctx, &VB_READ_TYPE_KINDS);
            namesake = types.ambiguous || types.owners.is_empty();
        }
    }
    if args.is_empty() {
        memo.store(member.id.clone(), namesake);
    }
    namesake
}

/// The field, property or Module variable a bare name before the dot reads as well, where VB.NET
/// binds the name to one: `Button1` in `Button1.Text` is the form's own control, `Title` a field
/// its class inherits, `Current` a variable of one of its project's Modules — whatever the member
/// read through it. None for a parameter or local of that name, which hides the member; for `Me`,
/// `MyBase` and a `{T}` value; for a `With` block's `.Text`, whose `With` names the value; and for
/// a member declared as a type of its own name from outside the project (`Responser As
/// Responser`): VB.NET reads that type's Shared members and nested types through the type, not the
/// member, and nothing here shows which these are.
fn head_member_of(head: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    if head.starts_with('{') || is_me(head) {
        return None;
    }
    let name = unescaped(head);
    // The name itself starts the access where the reference is: `Button1` or `[Button1]`, then the dot.
    let line = line_at(ctx, &r.file_path, r.line).unwrap_or_default();
    let rest = crate::name_matcher::util::after_col(&line, r.column);
    let bracketed = rest.starts_with('[');
    let mut rest = if bracketed { &rest[1..] } else { rest };
    let width = codegraph_types::jscompat::utf16_len(&name);
    if js_lower(codegraph_types::jscompat::utf16_prefix(rest, width)) != js_lower(&name) {
        return None;
    }
    rest = codegraph_types::jscompat::utf16_from(rest, width);
    if bracketed {
        rest = rest.strip_prefix(']')?;
    }
    let rest = rest.trim_start_matches([' ', '\t']);
    if !rest.starts_with('.') {
        return None;
    }
    if local_binding(&name, r, ctx).is_some() {
        return None;
    }
    let found = member_around(&name, r, ctx)?;
    let member = &found.node;
    if !VB_VALUE_KINDS.contains(&member.kind) || member.id == r.from_node_id {
        return None;
    }
    // Another project's Module is one this code can't name without a reference to it.
    let cut = member.qualified_name.rfind("::");
    let foreign = project_of(&member.file_path, ctx) != project_of(&r.file_path, ctx);
    if foreign && cut.is_some_and(|c| c > 0 && is_module(&member.qualified_name[..c], ctx)) {
        return None;
    }
    if is_outside_namesake(member, &found.args, ctx) {
        None
    } else {
        Some(Rc::clone(member))
    }
}

/// Whether a member belongs to its type rather than to an instance: `Shared`, an `Enum` case, a
/// nested type, or a `Const` — Shared without saying so, which a VB.NET field's node doesn't
/// record, so its declaration is read.
fn is_shared_member(n: &Node, ctx: &Ctx) -> bool {
    if n.is_static == Some(true) || VB_SHARED_READ_KINDS.contains(&n.kind) {
        return true;
    }
    if n.kind != NodeKind::Field {
        return false;
    }
    let code = code_slice(&n.file_path, n.start_line as i64 - 1, n.start_line as i64 + 2, ctx);
    Re::cached(&format!(r"\bConst\s+(?:[^=]*,\s*)?{}(?![\w$%&!#@])", escape_regex(&n.name)), "i").is_match(&code)
}

/// Resolve a VB.NET member read or write to the member the receiver's type declares or inherits.
/// Through a type's name (#2305) — `AppSession.SessionId`, `AppSession.CurrentUser = "demo"`,
/// `Logger.Level`, `Mode.Fast` — the read links the type as well, and a `Shared` member before a
/// same-named instance one. Through a value — `x.Normal = 3` on a parameter `x As Holder`,
/// `Me._h.Title`, a `With` block's `.Value`, an initializer's `.Switch` — it links the member of
/// the type the value is declared as, whatever that type's name; never a member of the type the
/// receiver's own name would mean. A value named like its type (`theme As Theme`) reads an
/// instance member as a value does and a Shared one as the type does, by VB.NET's "Color Color"
/// rule. A method named either way is called, as without parentheses VB.NET runs it — the overload
/// that takes no arguments — unless `AddressOf` or `NameOf` only names it. Through a namespace —
/// `Billing.Invoice`, `Acme.Billing.Invoice.MaxLines`, a Module's `Assets.Logo` — it links the
/// type or the Module's member the namespace declares. A bare name before the dot that is a field,
/// property or Module variable (`Button1` of `Button1.Text`) is read too, whatever the member read
/// through it. None when a link of the path is not typed here (an outside type: `Color.Red`,
/// `str.Length`), is late-bound (`Object`), or names two of the project's types.
pub fn match_vb_member_read(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let m = member_path().captures(&r.reference_name)?;
    let head = m.text(1).unwrap_or("");
    let links: Vec<String> = match m.text(2) {
        Some(l) if !l.is_empty() => l[1..].split('.').map(String::from).collect(),
        _ => Vec::new(),
    };
    let at = path_receiver(head, &links, r, ctx);
    // `MySettings.Default` reads the property declared `[Default]`, and back.
    let name = unescaped(m.text(3).unwrap_or(""));
    let calls = |member: &Node| member.kind == NodeKind::Method && !names_without_running(r, ctx);
    // A method VB.NET runs here is the overload it can call without arguments.
    let run = |member: Option<NodeRef>| -> Option<NodeRef> {
        let member = member?;
        if calls(&member) {
            runnable_overload(&member, r, ctx)
        } else {
            Some(member)
        }
    };
    let through_value = |member: &Node| -> ResolvedRef {
        let mut resolved = ResolvedRef::new(r, member.id.clone(), 0.9, ResolvedBy::InstanceMethod);
        if calls(member) {
            resolved.edge_kind = Some(EdgeKind::Calls);
        }
        resolved
    };
    // The member a bare name before the dot reads, beside what is read through it. A method run
    // through the name (`Button1.PerformClick`) is linked alone, as a call written with
    // parentheses is.
    let with_head = |read: Option<ResolvedRef>| -> Option<ResolvedRef> {
        let head_node = if links.is_empty() { head_member_of(head, r, ctx) } else { None };
        let Some(head_node) = head_node else { return read };
        match read {
            None => Some(ResolvedRef::new(r, head_node.id.clone(), 0.9, ResolvedBy::InstanceMethod)),
            Some(mut read) => {
                if read.edge_kind != Some(EdgeKind::Calls) {
                    read.also_targets.get_or_insert_with(Vec::new).push(AlsoTarget { target_node_id: head_node.id.clone(), metadata: None });
                }
                Some(read)
            }
        }
    };
    let Some(at) = at else { return with_head(None) };
    if let Some(ns) = &at.namespace {
        // A type, or a Module's member, read through its namespace.
        let types = types_in_namespace(ns, &name, r, ctx);
        let target = match types {
            Some(t) => match t.into_iter().next() {
                Some(first) => Some(first),
                None => run(module_member_in(ns, &name, r, ctx)),
            },
            None => None,
        };
        let target = target.filter(|t| t.id != r.from_node_id)?;
        let mut resolved = ResolvedRef::new(r, target.id.clone(), if target.kind == NodeKind::Method { 0.85 } else { 0.9 }, ResolvedBy::QualifiedName);
        if calls(&target) {
            resolved.edge_kind = Some(EdgeKind::Calls);
        }
        return Some(resolved);
    }
    if at.owners.is_empty() {
        return with_head(None);
    }
    if !at.through_type {
        // The member the name means: the nearest the type declares or inherits. The read's own
        // member (`Return Me.Count` in `Count`) runs itself.
        let fits = |n: &Node| VB_INSTANCE_READ_KINDS.contains(&n.kind);
        let found = member_on(&at.owners, &name, r, ctx, true, at.typed.as_ref(), Some(&fits))
            .or_else(|| member_on(&at.owners, &format!("[{name}]"), r, ctx, true, at.typed.as_ref(), Some(&fits)));
        let member = run(found.map(|f| f.node));
        return with_head(member.filter(|m| m.id != r.from_node_id).map(|m| through_value(&m)));
    }
    // Not the member the read is written in: staxrip's `Overrides ReadOnly Property Package`
    // returns the class's `Shared ReadOnly Property Package`.
    let from_id = r.from_node_id.clone();
    let fits = move |n: &Node| VB_READ_KINDS.contains(&n.kind) && n.id != from_id;
    let found = member_on(&at.owners, &name, r, ctx, true, None, Some(&fits))
        .or_else(|| member_on(&at.owners, &format!("[{name}]"), r, ctx, true, None, Some(&fits)))?;
    let mut member: NodeRef = found.node;
    // Of a `Shared` member and an instance one of the same name, a type's name reads the `Shared` one.
    if member.is_static != Some(true) {
        let original = Rc::clone(&member);
        member = ctx
            .get_nodes_by_qualified_name(&original.qualified_name)
            .iter()
            .find(|n| n.is_static == Some(true) && n.file_path == original.file_path && fits(n))
            .cloned()
            .unwrap_or(original);
    }
    member = run(Some(Rc::clone(&member))).unwrap_or(member);
    // "Color Color": a value named like its type (`theme As Theme`) reads an instance member
    // through itself, and only a Shared one through the type.
    if at.color_color && !is_shared_member(&member, ctx) {
        return with_head(Some(through_value(&member)));
    }
    if member.kind == NodeKind::Method {
        let mut resolved = ResolvedRef::new(r, member.id.clone(), 0.85, ResolvedBy::QualifiedName);
        if calls(&member) {
            resolved.edge_kind = Some(EdgeKind::Calls);
        }
        return Some(resolved);
    }
    // A type named through another (`Outer.Mode.Fast`) is linked by the read that names it
    // (`Outer.Mode`), so only a type named itself is linked here.
    if links.is_empty() {
        read_through(&member, &at.owners, r, ctx, None, Some(0.9))
    } else {
        Some(ResolvedRef::new(r, member.id.clone(), 0.9, ResolvedBy::QualifiedName))
    }
}

/// Resolve a VB.NET call through a receiver path, which a `With` block sends for its `.Run()`
/// (`With Me._h`, `With user.Settings`, `With DirectCast(o, T)`), as `match_vb_typed_call`
/// resolves one through a typed name: the method the path's type has or inherits, a read of a
/// field or property it indexes, or an extension method for the type. None — never a guess by the
/// method's name — when a link of the path is not typed here.
pub fn match_vb_path_call(r: &UnresolvedRef, ctx: &Ctx, is_std: &dyn Fn(&str) -> bool) -> Option<ResolvedRef> {
    let m = member_path().captures(&r.reference_name)?;
    let links: Vec<String> = match m.text(2) {
        Some(l) if !l.is_empty() => l[1..].split('.').map(String::from).collect(),
        _ => Vec::new(),
    };
    let at = path_receiver(m.text(1).unwrap_or(""), &links, r, ctx)?;
    match &at.typed {
        Some(typed) if !at.through_type => call_on_value(typed, &at.owners, &unescaped(m.text(3).unwrap_or("")), r, ctx, is_std),
        _ => None,
    }
}

memo_map!(VbTypeImports, String, Rc<HashSet<String>>);

/// The last names of a file's `Imports` (a type imported this way lends its shared members; an alias lends none).
fn imported_names(file: &str, ctx: &Ctx) -> Rc<HashSet<String>> {
    let memo = ctx.memo::<VbTypeImports>();
    if let Some(hit) = memo.lookup(&file.to_string()) {
        return hit;
    }
    let lines = code_lines(file, ctx);
    let end = lines.iter().position(|l| {
        re!(r"^\s*(?:Namespace|Module|Class|Structure|Interface|Enum|Delegate|Public|Friend|Private|Protected|Partial|NotInheritable|MustInherit)\b", "i").is_match(l)
    });
    let header = &lines[..end.unwrap_or(lines.len())];
    let mut names: HashSet<String> = HashSet::new();
    for line in header {
        if let Some(m) = re!(r"^\s*Imports\s+([\w.]+)\s*$", "i").captures(line) {
            names.insert(js_lower(m.text(1).unwrap_or("").rsplit('.').next().unwrap_or("")));
        }
    }
    let names = Rc::new(names);
    memo.store(file.to_string(), Rc::clone(&names));
    names
}

memo_map!(VbScopes, String, Option<Rc<HashSet<String>>>);

/// The lowercased names of the types around a call and of the project types they inherit; none outside any type.
fn scope_owners(r: &UnresolvedRef, ctx: &Ctx) -> Option<Rc<HashSet<String>>> {
    let memo = ctx.memo::<VbScopes>();
    let key = format!("{}:{}", r.file_path, r.line);
    if let Some(hit) = memo.lookup(&key) {
        return hit;
    }
    let around = types_around(&r.file_path, r.line, ctx);
    let owners = if around.is_empty() {
        None
    } else {
        Some(Rc::new(around.iter().flat_map(|t| hierarchy(t, ctx)).map(|t| js_lower(&t.name)).collect::<HashSet<String>>()))
    };
    memo.store(key, owners.clone());
    owners
}

/// Whether an unqualified VB.NET type name can mean `n`: a type nested in a class, structure or
/// interface is named bare only inside it or a type deriving from it — designer code's `New
/// Point(4, 285)` is not staxrip's nested `ButtonEx.SymbolDrawer.Point`. A `Module`'s types belong
/// to its namespace; a file can import the outer type, or alias the nested one (`Imports
/// UserMediaD = SCrawler.DownloadObjects.TDownloader.UserMediaD`). Not judged when no type is
/// around the name.
pub fn is_vb_nested_type_in_scope(n: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if n.language != Language::VbNet || !VB_TYPE_LIKE_KINDS.contains(&n.kind) {
        return true;
    }
    let Some(parent) = enclosing_type(n, ctx) else { return true };
    let Some(owners) = scope_owners(r, ctx) else { return true };
    let parent_name = js_lower(&parent.name);
    if owners.contains(&parent_name) {
        return true;
    }
    if let Some(alias) = import_aliases(&r.file_path, ctx).get(&js_lower(&r.reference_name)) {
        if js_lower(&alias.name) == js_lower(&n.name) && alias.qualifier.as_ref().is_none_or(|q| ends_with(&container_of(n, ctx), q)) {
            return true;
        }
    }
    imported_names(&r.file_path, ctx).contains(&parent_name)
}

/// Whether a receiver-less VB.NET call (or one through `Me` / `MyClass` / `MyBase`) can mean
/// member `n`: one of a type around the call or of a type those inherit, of a `Module`, or of a
/// type the file imports. Not judged when no type is around the call, or `n` is not a member of a type.
pub fn is_vb_member_in_scope(n: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if n.language != Language::VbNet || !VB_MEMBER_KINDS.contains(&n.kind) {
        return true;
    }
    let Some(cut) = n.qualified_name.rfind("::") else { return true };
    let Some(owners) = scope_owners(r, ctx) else { return true };
    let owner_qn = &n.qualified_name[..cut];
    let owner = js_lower(owner_qn.replace("::", ".").rsplit('.').next().unwrap_or(""));
    owners.contains(&owner) || is_module(owner_qn, ctx) || imported_names(&r.file_path, ctx).contains(&owner)
}
