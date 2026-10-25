//! Chained calls whose receiver is itself a call, which the extractors encode as
//! `<innerCallee>().<method>`: PHP `Cls::for($x)->method()`, Rust `Foo::new().bar()`
//! (`matchScopedCallChain`) and `Foo.getInstance().bar()` (`matchDottedCallChain`).
//!
//! The receiver's type is what the inner call returns; the outer method is then resolved and
//! VALIDATED on it (`resolve_method_on_type` requires the method to exist on the type or a
//! supertype it conforms to), so a wrong inference yields no edge rather than a wrong one.

use super::exact::{match_by_exact_name, match_fuzzy};
use super::method::resolve_method_on_type;
use super::dart_chain::match_dart_chain_link;
use crate::ctx::Ctx;
use crate::types::{ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::{Language, NodeKind};

/// Return type captured at extraction for `Class::method` (or a free function), read off the
/// indexed node's `returnType`. Language-filtered. None when not indexed or no return type was
/// recorded (a `void`/primitive return).
pub fn lookup_callee_return_type(callee: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let parts: Vec<&str> = callee.split("::").filter(|p| !p.is_empty()).collect();
    let (method, cls) = if callee.contains("::") {
        (parts.last().copied().unwrap_or(callee), parts[..parts.len().saturating_sub(1)].join("::"))
    } else {
        (callee, String::new())
    };
    let nodes = ctx.get_nodes_by_name(method);
    let mut candidates = nodes.iter().filter(|n| {
        matches!(n.kind, NodeKind::Method | NodeKind::Function) && n.language == r.language && n.return_type.as_deref().is_some_and(|t| !t.is_empty())
    });
    if !cls.is_empty() {
        let want = format!("{cls}::{method}");
        // The call site may name the class with MORE namespace qualification than the stored node
        // (`details::registry::instance` at the call vs `registry::instance` on the node — the
        // receiver type only carries the immediate class), or LESS. Accept an exact match or
        // either being a namespace-suffix of the other; the shared `::<class>::<method>` tail
        // keeps it specific.
        return candidates
            .find(|n| n.qualified_name == want || n.qualified_name.ends_with(&format!("::{want}")) || want.ends_with(&format!("::{}", n.qualified_name)))
            .and_then(|n| n.return_type.clone());
    }
    candidates.find(|n| n.kind == NodeKind::Function).and_then(|n| n.return_type.clone())
}

/// When several classes share a simple type name, the caller file's import of that type is the
/// only signal that names WHICH one (#314).
fn imported_fqn_of(type_name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    ctx.get_import_mappings(&r.file_path, r.language).iter().find(|i| i.local_name == type_name).map(|i| i.source.clone())
}

/// `^(.+)\(\)\.(\w+)$`: the inner callee and the outer method of an encoded chain.
fn split_chain(r: &UnresolvedRef) -> Option<(String, String)> {
    let m = re!(r"^(.+)\(\)\.(\w+)$").captures(&r.reference_name)?;
    let inner = m.text(1).filter(|s| !s.is_empty())?;
    let method = m.text(2).filter(|s| !s.is_empty())?;
    Some((inner.to_string(), method.to_string()))
}

/// A `::`-scoped factory chain `Cls::factory().method`. A `self` marker (PHP `: self` /
/// `: static`, Rust `-> Self`) resolves to the factory's own type, a concrete return type to that
/// type. Shared by the `::`-receiver languages (PHP, Rust).
pub fn match_scoped_call_chain(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let (inner, method) = split_chain(r)?;
    // Only static-factory (`Cls::method`) chains.
    let cut = inner.rfind("::")?;
    let factory_class = &inner[..cut];
    let ret = lookup_callee_return_type(&inner, r, ctx)?;
    // `self` (the extractor's marker for self/static/$this) → the factory's class.
    let resolved_class = if ret == "self" { factory_class } else { ret.as_str() };
    resolve_method_on_type(resolved_class, &method, r, ctx, 0.85, ResolvedBy::InstanceMethod, None, 0)
}

/// Languages where an unprefixed capitalized call `Foo(args)` constructs the class (so a
/// `Foo(args).method()` receiver's type is `Foo`). Java/C# need `new`, so a bare `Foo()` there is
/// a method call, not construction. Scala's `Foo(args)` is a case-class / companion `apply`,
/// which conventionally returns `Foo`. Pascal/Delphi: a `TFoo(x)` is a typecast whose result is
/// a `TFoo`.
fn constructs_via_bare_call(language: Language) -> bool {
    matches!(language, Language::Kotlin | Language::Swift | Language::Scala | Language::Dart | Language::Pascal)
}

/// A dotted chain whose receiver is a static factory / fluent call, `Foo.getInstance().bar()`,
/// encoded as `Foo.getInstance().bar`. Shared by the dot-notation languages (Java, Kotlin, C#,
/// Swift, Go, Dart, Objective-C, Pascal).
pub fn match_dotted_call_chain(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let (inner, method) = split_chain(r)?;
    let last_dot = inner.rfind('.');
    let Some(last_dot) = last_dot.filter(|d| *d > 0) else {
        // Go: bare package-level factory FUNCTION `New().method()` — the receiver's type is what
        // `New` returns; resolve the method on that.
        if r.language == Language::Go {
            if let Some(ret) = lookup_callee_return_type(&inner, r, ctx) {
                return resolve_method_on_type(&ret, &method, r, ctx, 0.85, ResolvedBy::InstanceMethod, imported_fqn_of(&ret, r, ctx).as_deref(), 0);
            }
            // `inner` isn't a function with a captured return type — typically a package-level
            // VARIABLE holding a function value, whose type we can't recover. Fall back to
            // bare-name resolution of the method so we don't DROP an edge the un-re-encoded bare
            // path would have found.
            //
            // CRITICAL: resolve the TARGET via a synthetic bare-name ref, but return the match
            // tied to the ORIGINAL `ref` (referenceName `inner().method`). The batched resolver
            // relies on the post-batch cleanup keyed on the stored row to clear each resolved
            // row; propagating the synthetic ref's bare name as `original` would never match it
            // and the batch would never drain (a runaway that grew gin's graph to 5M edges).
            let mut bare = r.clone();
            bare.reference_name = method.clone();
            let found = match_by_exact_name(&bare, ctx).or_else(|| match_fuzzy(&bare, ctx));
            return found.map(|mut m| {
                m.original = r.clone();
                m
            });
        }
        // Constructor receiver `Foo(args).method()` (encoded `Foo().method`): a bare, capitalized
        // inner is a class construction, so the receiver's type is the class itself. Only in
        // languages where an unprefixed capitalized call constructs the class; a lowercase bare
        // inner is a top-level `factory().method()` whose type we can't recover — bail.
        if !constructs_via_bare_call(r.language) || !re!(r"^[A-Z]").is_match(&inner) {
            return None;
        }
        return resolve_method_on_type(&inner, &method, r, ctx, 0.85, ResolvedBy::InstanceMethod, imported_fqn_of(&inner, r, ctx).as_deref(), 0);
    };

    // Factory/fluent receiver `Receiver.factory(args).method()`: the receiver's type is what
    // `Receiver.factory` returns (its declared return type).
    let factory_class = inner[..last_dot].rsplit('.').next().unwrap_or("");
    let factory_method = &inner[last_dot + 1..];
    if factory_class.is_empty() || factory_method.is_empty() {
        return None;
    }
    let ret = lookup_callee_return_type(&format!("{factory_class}::{factory_method}"), r, ctx);
    if r.language == Language::Dart {
        let declared = ret
            .as_deref()
            .and_then(|t| resolve_method_on_type(t, &method, r, ctx, 0.85, ResolvedBy::InstanceMethod, imported_fqn_of(t, r, ctx).as_deref(), 0));
        if declared.is_some() {
            return declared;
        }
        // A generic factory returns its type parameter, and one outside the project declares
        // nothing here: the call site has the type. Read the chain there as a later link of a
        // chain is read (#750). The edge belongs to this ref, not to the bare name it was looked
        // up by.
        let mut link_ref = r.clone();
        link_ref.reference_name = method.clone();
        return match_dart_chain_link(&link_ref, ctx).map(|mut link| {
            link.original = r.clone();
            link
        });
    }
    let Some(ret) = ret else {
        // Objective-C: a class-message factory — `[X alloc]`, `[X new]`, `[X sharedFoo]` —
        // returns an instance of the RECEIVER class `X` by convention (`instancetype`).
        // resolve_method_on_type validates against X (and its supertypes), so a class whose method
        // actually lives elsewhere yields NO edge, not a wrong one.
        if r.language == Language::ObjC && re!(r"^[A-Z]").is_match(factory_class) {
            return resolve_method_on_type(factory_class, &method, r, ctx, 0.8, ResolvedBy::InstanceMethod, imported_fqn_of(factory_class, r, ctx).as_deref(), 0);
        }
        // Pascal/Delphi: the extractor only re-encodes a `TFoo`/`IFoo`-prefixed chain, so
        // `factory_class` is always a real class here. A factory whose return type wasn't
        // captured is a CONSTRUCTOR (or an unannotated function): the receiver's type is the
        // class itself.
        if r.language == Language::Pascal && re!(r"^[TI]").is_match(factory_class) {
            return resolve_method_on_type(factory_class, &method, r, ctx, 0.8, ResolvedBy::InstanceMethod, imported_fqn_of(factory_class, r, ctx).as_deref(), 0);
        }
        return None;
    };
    resolve_method_on_type(&ret, &method, r, ctx, 0.85, ResolvedBy::InstanceMethod, imported_fqn_of(&ret, r, ctx).as_deref(), 0)
}
