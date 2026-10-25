//! A TS/JS call on a constructor receiver, `new Runner(args).run()`, which the extractor records
//! as `new Runner().run`: the receiver is an instance of the class written at the call, so the
//! call resolves only through that class — port of `matchNewReceiverCall` (name-matcher.ts).

use super::infer::ts_imported_type_file;
use super::csharp::is_csharp_type_visible;
use super::method::{imported_fqn_of, resolve_method_on_type};
use super::same_language_family;
use super::util::lines_of;
use crate::ctx::Ctx;
use crate::js_builtins::JS_BUILT_INS;
use crate::types::{NodeRef, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::{Language, NodeKind};

/// `new <Class>().<method>`: the class as written, and the method.
fn new_receiver_shape(r: &UnresolvedRef) -> Option<(String, String)> {
    let m = re!(r"^new ([\w$.]+)\(\)\.([\w$]+)$").captures(&r.reference_name)?;
    Some((m.text(1)?.to_string(), m.text(2)?.to_string()))
}

/// Whether `r` is written `new <Class>().<method>` (`NEW_RECEIVER_SHAPE`).
pub fn is_new_receiver_shape(r: &UnresolvedRef) -> bool {
    new_receiver_shape(r).is_some()
}

/// The class a `new C().m` ref constructs, as the name its declaration carries: `ns.Runner` →
/// `Runner`, an aliased named import `R` → `Runner`. A JS/TS built-in (`RegExp`, `Map`, `Date`…)
/// counts only when the file imports or declares a class of that name — otherwise a project class
/// that merely shares the name would take every `new Map().get()` in the repo.
pub fn new_receiver_class(r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let (written, _) = new_receiver_shape(r)?;
    let last = written.rsplit('.').next().unwrap_or("").to_string();
    let dotted = written.contains('.');
    let mappings = ctx.get_import_mappings(&r.file_path, r.language);
    let imported = if dotted { None } else { mappings.iter().find(|i| i.local_name == written) };
    if let Some(imp) = imported {
        if !imp.is_default && !imp.is_namespace && re!(r"^[A-Za-z_$][\w$]*$").is_match(&imp.exported_name) {
            return Some(imp.exported_name.clone());
        }
    }
    if imported.is_none()
        && !dotted
        && JS_BUILT_INS.contains(last.as_str())
        && !ctx.get_nodes_in_file(&r.file_path).iter().any(|n| n.kind == NodeKind::Class && n.name == last)
    {
        return None;
    }
    Some(last)
}

/// Resolve a `new C().m` call on the constructed class or one of its supertypes (validated by
/// `resolve_method_on_type`), or on a function assigned onto an ES5 constructor function's
/// prototype in its file, or not at all: a class with no project declaration — `new
/// URL(u).toString()` — has no project method to bind to.
pub fn match_new_receiver_call(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let cls = new_receiver_class(r, ctx)?;
    let (written, method) = new_receiver_shape(r)?;
    // Java's and C#'s no-argument `new Foo().bar()` reach here from the conformance pass; their
    // own rules pick among same-named classes.
    if matches!(r.language, Language::Java | Language::CSharp) {
        return match_java_csharp_new_receiver_call(&cls, &written, &method, r, ctx);
    }
    // Several classes may share the name: the one the call's file imports under the written
    // name is the class constructed.
    let preferred = ts_imported_type_file(&written, r, ctx);
    if let Some(on_class) = resolve_method_on_type(&cls, &method, r, ctx, 0.9, ResolvedBy::InstanceMethod, preferred.as_deref(), 0) {
        return Some(on_class);
    }
    // An ES5 constructor function (`function Params() {}`) keeps its methods as functions
    // assigned onto its prototype in its own file (`prototype.render = function render() {…}`).
    let ctors: Vec<NodeRef> =
        ctx.get_nodes_by_name(&cls).iter().filter(|n| n.kind == NodeKind::Function && same_language_family(n.language, r.language)).cloned().collect();
    if ctors.len() != 1 {
        return None;
    }
    let file = &ctors[0].file_path;
    let lines = lines_of(ctx, file);
    let on_proto: Vec<NodeRef> = ctx
        .get_nodes_in_file(file)
        .iter()
        .filter(|n| {
            n.name == method
                && matches!(n.kind, NodeKind::Function | NodeKind::Method)
                && (n.start_line as usize).checked_sub(1).and_then(|i| lines.get(i)).is_some_and(|l| re!(r"\bprototype\b").is_match(l))
        })
        .cloned()
        .collect();
    (on_proto.len() == 1).then(|| ResolvedRef::new(r, on_proto[0].id.clone(), 0.8, ResolvedBy::InstanceMethod))
}

/// `file.slice(0, file.lastIndexOf('/'))`: the directory part, or (with no `/`) all but the last
/// character, as JS slices to `-1`.
fn dir_of(file: &str) -> &str {
    match file.rfind('/') {
        Some(i) => &file[..i],
        None => file.char_indices().last().map_or("", |(i, _)| &file[..i]),
    }
}

/// A Java or C# `new Foo().bar()`. With one project class of the name, its method or an inherited
/// one, as for any typed call. With several, the class the language picks, or none: Java's written
/// package (`new a.b.Foo()`), the file's single-type import, else the caller's own package; C#'s
/// one type the file's namespaces and usings can see. Then only that class's own method: an
/// inherited one is looked up by name, across every class of the name.
fn match_java_csharp_new_receiver_call(cls: &str, written: &str, method: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let twins: Vec<NodeRef> = ctx
        .get_nodes_by_name(cls)
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Class | NodeKind::Struct) && same_language_family(n.language, r.language))
        .cloned()
        .collect();
    if twins.len() <= 1 {
        return resolve_method_on_type(cls, method, r, ctx, 0.9, ResolvedBy::InstanceMethod, None, 0);
    }
    let chosen: Option<NodeRef> = if r.language == Language::CSharp {
        let visible: Vec<&NodeRef> = twins.iter().filter(|n| is_csharp_type_visible(n, r, ctx)).collect();
        (visible.len() == 1).then(|| visible[0].clone())
    } else {
        let fqn = if written.contains('.') { Some(written.to_string()) } else { imported_fqn_of(cls, r, ctx) };
        match fqn {
            Some(fqn) => {
                let tail = format!("/{}", fqn.replace('.', "/"));
                twins.iter().find(|n| n.file_path.strip_suffix(".java").unwrap_or(&n.file_path).ends_with(&tail)).cloned()
            }
            None => {
                let own: Vec<&NodeRef> = twins.iter().filter(|n| dir_of(&n.file_path) == dir_of(&r.file_path)).collect();
                (own.len() == 1).then(|| own[0].clone())
            }
        }
    };
    let chosen = chosen?;
    let qn = format!("{}::{method}", chosen.qualified_name);
    let own = ctx.get_nodes_in_file(&chosen.file_path).iter().find(|n| n.kind == NodeKind::Method && n.name == method && n.qualified_name == qn).cloned()?;
    Some(ResolvedRef::new(r, own.id.clone(), 0.9, ResolvedBy::InstanceMethod))
}
