//! A Java call resolves on its receiver's declared type, and a library type gets no guess (upstream
//! 7973df0d, PR #1942 by danusha2345).
//!
//! A receiver is a field (`mHandler`, `this.mHandler`, `Outer.this.mHandler`), a chain of up to three
//! fields after it (`this.mOwner.mRepo`), or a local; its type is read as the declaration writes it
//! (a package or an outer class stays), each hop of a chain with its own file's imports. A type the
//! project does not declare, or that an import or a written package pins outside the project, gets
//! no edge.

use super::infer::{field_signature_type, infer_java_field_receiver_type, infer_local_receiver_type, NON_TYPE_RECEIVER_TOKENS};
use super::java::java_file_scope;
use super::lang_shape::is_method_owner_kind;
use super::members::{declared_supertypes, type_parameter_bound};
use super::method::resolve_method_on_type;
use super::same_language_family;
use super::util::{memo_map, memoized};
use crate::ctx::Ctx;
use crate::types::{NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::{Language, NodeKind};
use std::collections::HashSet;
use std::rc::Rc;

/// A Java type as written (`Foo`, `java.util.List<Foo>`, `Map.Entry<K, V>`,
/// `play.api.mvc.BodyParser<B>`, `Foo[]`), type arguments and array markers dropped. The class chain
/// is joined with `::` (`Map::Entry`), so a nested type is never taken for a same-named type nested
/// elsewhere, and a package written in source stays in front of it, dotted
/// (`play.api.mvc.BodyParser`), for [`java_type_ref`] to pin the declaration with. `None` for a
/// primitive.
pub fn java_type_name(raw: &str) -> Option<String> {
    let mut ty = raw.to_string();
    loop {
        let next = re!(r"<[^<>]*>", "g").replace_all_str(&ty, "");
        if next == ty {
            break;
        }
        ty = next;
    }
    let ty = re!(r"\s+", "g").replace_all_str(&re!(r"\.\.\.$").replace_first(&re!(r"\[\s*\]", "g").replace_all_str(&ty, ""), |_| String::new()), "");
    let segments: Vec<&str> = ty.split('.').filter(|s| !s.is_empty()).collect();
    let mut first = segments.len();
    while first > 0 && re!(r"^[A-Z][\w$]*$").is_match(segments[first - 1]) {
        first -= 1;
    }
    let chain = segments[first..].join("::");
    if chain.is_empty() || NON_TYPE_RECEIVER_TOKENS.contains(&chain.as_str()) {
        return None;
    }
    let pkg = segments[..first].join(".");
    Some(if pkg.is_empty() { chain } else { format!("{pkg}.{chain}") })
}

/// Java: the declared type of the field a receiver names — `mHandler`, `this.mHandler`, or
/// `Outer.this.mHandler`. The field is looked up in the class enclosing the call and then in each
/// class around it, tightest first, because an inner or anonymous class reads its outer classes'
/// fields (`mHandler.post(...)` inside a `new Runnable() {…}`); `Outer.this.f` names the class to
/// read. A `static final` field is indexed as a `constant`.
pub fn java_enclosing_field_type(receiver_name: &str, r: &UnresolvedRef, in_file: &[NodeRef]) -> Option<String> {
    let m = re!(r"^(?:(?:(\w+)\.)?this\.)?(\w+)$").captures(receiver_name)?;
    let outer = m.text(1).filter(|s| !s.is_empty());
    let field_name = m.text(2)?;
    let mut enclosing: Vec<&NodeRef> = in_file
        .iter()
        .filter(|n| {
            matches!(n.kind, NodeKind::Class | NodeKind::Interface | NodeKind::Enum)
                && n.language == Language::Java
                && n.start_line <= r.line
                && n.end_line.max(n.start_line) >= r.line
        })
        .collect();
    enclosing.sort_by(|a, b| b.start_line.cmp(&a.start_line));
    for cls in enclosing {
        if outer.is_some_and(|o| cls.name != o) {
            continue;
        }
        let qualified = format!("{}::{field_name}", cls.qualified_name);
        let field = in_file.iter().find(|n| matches!(n.kind, NodeKind::Field | NodeKind::Constant) && n.name == field_name && n.qualified_name == qualified);
        if let Some(field) = field {
            return field_signature_type(field);
        }
        if outer.is_some() {
            return None;
        }
    }
    None
}

fn is_jvm_declared_type_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Class | NodeKind::Interface | NodeKind::Enum | NodeKind::Struct | NodeKind::Trait)
}

/// JVM type declarations named `name`.
fn jvm_type_decls(name: &str, ctx: &Ctx) -> Vec<NodeRef> {
    ctx.get_nodes_by_name(name).iter().filter(|n| is_jvm_declared_type_kind(n.kind) && same_language_family(n.language, Language::Java)).cloned().collect()
}

/// Whether `decl` is the type one of `fqns` (an import, or a package written in source) names: the
/// Java declaration of that qualified name or file, or a Scala / Kotlin one in that package's
/// directory (their qualified names carry no package, and their files need not be named after the
/// type).
fn declares_jvm_fqn(decl: &codegraph_types::Node, fqns: &[String]) -> bool {
    let dotted = decl.qualified_name.replace("::", ".");
    let decl_path = decl.file_path.replace('\\', "/");
    let decl_file = re!(r"\.\w+$").replace_first(&decl_path, |_| String::new());
    let decl_dir = &decl_path[..decl_path.rfind('/').map_or(0, |i| i + 1)];
    fqns.iter().any(|fqn| {
        let fqn_path = fqn.replace('.', "/");
        if dotted == *fqn || decl_file == fqn_path || decl_file.ends_with(&format!("/{fqn_path}")) {
            return true;
        }
        let pkg_path = &fqn_path[..fqn_path.rfind('/').map_or(0, |i| i + 1)];
        decl.language != Language::Java && !pkg_path.is_empty() && (decl_dir == pkg_path || decl_dir.ends_with(&format!("/{pkg_path}")))
    })
}

memo_map!(JavaFileAncestorsMemo, String, Rc<HashSet<String>>);

/// The Java classes and interfaces the classes of `file_path` extend or implement, up to five levels
/// up.
fn java_file_ancestors(file_path: &str, ctx: &Ctx) -> Rc<HashSet<String>> {
    memoized::<JavaFileAncestorsMemo>(ctx, file_path.to_string(), || {
        let classes: Vec<NodeRef> = ctx.get_nodes_in_file(file_path).iter().filter(|n| n.language == Language::Java && is_method_owner_kind(n)).cloned().collect();
        let mut seen: HashSet<String> = classes.iter().map(|c| c.id.clone()).collect();
        let mut ancestors: HashSet<String> = HashSet::new();
        let mut frontier = classes;
        for _ in 0..5 {
            if frontier.is_empty() {
                break;
            }
            let mut next: Vec<NodeRef> = Vec::new();
            for cls in &frontier {
                for sup in declared_supertypes(cls, ctx).iter() {
                    for decl in ctx.get_nodes_by_name(sup).iter() {
                        if decl.language != Language::Java || !is_method_owner_kind(decl) || seen.contains(&decl.id) {
                            continue;
                        }
                        seen.insert(decl.id.clone());
                        ancestors.insert(decl.id.clone());
                        next.push(Rc::clone(decl));
                    }
                }
            }
            frontier = next;
        }
        Rc::new(ancestors)
    })
}

/// Whether Java file `file_path` can name `decl` by its simple name without importing it — for a
/// type read from a declaration (a field's, a local's), where `is_java_type_visible` judges a name
/// written at a reference. A Java qualified name reads `package::Outer::Inner`; a nested type is
/// named that way only in its own file, in its own package (as `Outer.Inner`), or where its
/// top-level class is imported. A top-level type of another package is also reachable through a
/// wildcard import, which the import mappings do not list, so it counts as visible, and so does a
/// Scala or Kotlin declaration, whose qualified name carries no package to check. A class also names
/// the nested types of the classes it extends or implements (SystemUI's `AlertEntry` in a subclass
/// of `AlertingNotificationManager`).
fn is_java_type_nameable(decl: &codegraph_types::Node, file_path: &str, ctx: &Ctx) -> bool {
    if decl.language != Language::Java || decl.file_path == file_path {
        return true;
    }
    let segments: Vec<&str> = decl.qualified_name.split("::").collect();
    let has_package = segments.len() > 1 && re!(r"^[a-z]").is_match(segments[0]);
    let class_chain = if has_package { &segments[1..] } else { &segments[..] };
    if class_chain.len() <= 1 {
        return true;
    }
    let pkg = if has_package { segments[0] } else { "" };
    let scope = java_file_scope(file_path, ctx);
    if pkg == scope.pkg {
        return true;
    }
    let outer = if pkg.is_empty() { class_chain[0].to_string() } else { format!("{pkg}.{}", class_chain[0]) };
    let outer_dot = format!("{outer}.");
    if ctx.get_import_mappings(file_path, Language::Java).iter().any(|i| i.source == outer || i.source.starts_with(&outer_dot)) {
        return true;
    }
    // `import play.mvc.Http.*;` names Http's nested types.
    if scope.demand.contains(&segments[..segments.len() - 1].join(".")) {
        return true;
    }
    let ancestors = java_file_ancestors(file_path, ctx);
    ctx.get_nodes_by_qualified_name(&segments[..segments.len() - 1].join("::")).iter().any(|o| ancestors.contains(&o.id))
}

/// A Java receiver type the project declares.
pub struct JavaTypeRef {
    /// The class chain `resolve_method_on_type` matches (`Http::Cookie`).
    pub name: String,
    /// The declaration an import or a written package pins, when one does.
    pub decl: Option<NodeRef>,
}

/// What a Java type written in `file_path` ([`java_type_name`]'s form) is: a project type, or `None`
/// for a library one (`Parcel`, `Context`, `List`), which none of the project's methods belongs to.
/// It is a library type when the project declares no JVM type of its outer name; when an import, or
/// a package written in source, names a type the project does not declare (`import
/// java.util.Iterator` beside a project `UByteArray.Iterator`); when every project type of that name
/// is one the file cannot name that way (a nested type of another package); or when a project class
/// declares no nested type of the written name (`MyLayout.LayoutParams`, inherited from a library
/// class). A short all-caps name (`T`, `VH`) is taken for a type parameter, whose bound may be a
/// project type, and left alone.
pub fn java_type_ref(written: &str, file_path: &str, ctx: &Ctx) -> Option<JavaTypeRef> {
    let head = written.find("::").map_or(written, |i| &written[..i]);
    let last_dot = head.rfind('.');
    let pkg = last_dot.map_or("", |d| &head[..d]);
    let chain: Vec<&str> = last_dot.map_or(written, |d| &written[d + 1..]).split("::").collect();
    let outer = chain[0];
    if pkg.is_empty() && chain.len() == 1 && re!(r"^[A-Z][A-Z0-9]{0,2}$").is_match(outer) {
        return Some(JavaTypeRef { name: outer.to_string(), decl: None });
    }
    let decls = jvm_type_decls(outer, ctx);
    if decls.is_empty() {
        return None;
    }
    let pins: Vec<String> = if pkg.is_empty() {
        ctx.get_import_mappings(file_path, Language::Java).iter().filter(|i| i.local_name == outer).map(|i| i.source.clone()).collect()
    } else {
        vec![format!("{pkg}.{outer}")]
    };
    if pins.is_empty() {
        if !decls.iter().any(|d| is_java_type_nameable(d, file_path, ctx)) {
            return None;
        }
        if chain.len() == 1 {
            return Some(JavaTypeRef { name: outer.to_string(), decl: None });
        }
        let name = chain.join("::");
        let suffix = format!("::{name}");
        let nested = jvm_type_decls(chain[chain.len() - 1], ctx).iter().any(|n| n.qualified_name == name || n.qualified_name.ends_with(&suffix));
        return nested.then_some(JavaTypeRef { name, decl: None });
    }
    let mut decl: Option<NodeRef> = decls.iter().find(|d| declares_jvm_fqn(d, &pins)).cloned();
    for segment in &chain[1..] {
        let Some(owner) = decl.take() else { break };
        let qualified = format!("{}::{segment}", owner.qualified_name);
        decl = jvm_type_decls(segment, ctx).into_iter().find(|n| n.qualified_name == qualified);
    }
    let decl = decl?;
    let segments: Vec<&str> = decl.qualified_name.split("::").collect();
    let has_package = decl.language == Language::Java && segments.len() > 1 && re!(r"^[a-z]").is_match(segments[0]);
    let name = (if has_package { &segments[1..] } else { &segments[..] }).join("::");
    Some(JavaTypeRef { name, decl: Some(decl) })
}

/// Resolve a method on a Java receiver type: the pinned declaration's own method first, then
/// `resolve_method_on_type` on the class chain, then — for a nested chain, which the supertype walk
/// there cannot look up — the supertypes of its simple name. A Scala or Kotlin declaration pinned by
/// an import is not followed further than its own methods: the class chain alone matches a
/// same-named Java type as well.
pub fn resolve_on_java_type(ty: &JavaTypeRef, method_name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    if let Some(decl) = &ty.decl {
        let own_name = format!("{}::{method_name}", decl.qualified_name);
        let own = ctx.get_nodes_by_name(method_name).iter().find(|n| n.kind == NodeKind::Method && n.qualified_name == own_name).cloned();
        if let Some(own) = own {
            return Some(ResolvedRef::new(r, own.id.clone(), 0.9, ResolvedBy::InstanceMethod));
        }
        if decl.language != Language::Java {
            return None;
        }
    }
    let fqn = ty.decl.as_ref().map(|d| d.qualified_name.replace("::", "."));
    let direct = resolve_method_on_type(&ty.name, method_name, r, ctx, 0.9, ResolvedBy::InstanceMethod, fqn.as_deref(), 0);
    if direct.is_some() || !ty.name.contains("::") {
        return direct;
    }
    let simple = ty.name.rsplit("::").next().unwrap_or("");
    for supertype in ctx.get_supertypes(simple, Language::Java).iter() {
        if let Some(via) = resolve_method_on_type(supertype, method_name, r, ctx, 0.9, ResolvedBy::InstanceMethod, None, 1) {
            return Some(via);
        }
    }
    None
}

/// What [`java_receiver_declared_type`] read for a receiver.
pub enum JavaDeclared {
    Type(JavaTypeRef),
    /// A type the project does not declare — a library class, or a field reached through one — so the
    /// caller can refuse a name-only guess.
    External,
}

/// Java: the declared type of a receiver the field lookup reaches — a field (`mRepo`, `this.mRepo`,
/// `Outer.this.mRepo`) or a chain of up to three fields after it (`mState.menu`,
/// `this.mOwner.mRepo`), whose head may also be a local. Each hop reads the field declared on the
/// previous hop's type, and reads that field's type with its own file's imports. `External` as soon
/// as a hop's type is a library one; `None` when a hop cannot be typed (an inherited field, an
/// untyped local).
pub fn java_receiver_declared_type(receiver: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<JavaDeclared> {
    let m = re!(r"^((?:\w+\.)?this\.\w+|[A-Za-z_$][\w$]*)((?:\.[a-zA-Z_$][\w$]*){0,3})$").captures(receiver)?;
    let head = m.text(1)?;
    let rest = m.text(2).unwrap_or("");
    let segments: Vec<&str> = if rest.is_empty() { Vec::new() } else { rest[1..].split('.').filter(|s| !s.is_empty()).collect() };
    // A plain receiver's local declaration was already read by the caller.
    let local = if !segments.is_empty() && !head.contains('.') { infer_local_receiver_type(head, r, ctx, None) } else { None };
    let head_type = local.or_else(|| infer_java_field_receiver_type(head, r, ctx))?;
    // A type parameter of the class or method around the call (`TState mTmpState` in
    // `QSTileImpl<TState extends State>`) is no library type: its bound decides, which the method
    // matcher reads when this answers `None`. As there, only a name no project class has:
    // `type_parameter_bound` also reads a type argument (`Comparable<ApplicationModule>`) as a
    // parameter.
    if re!(r"^[A-Z][\w$]*$").is_match(&head_type)
        && !ctx.get_nodes_by_name(&head_type).iter().any(|n| is_method_owner_kind(n))
        && !matches!(type_parameter_bound(&head_type, r, ctx), Tri::Undefined)
    {
        return None;
    }
    let mut written = head_type;
    let mut file_path = r.file_path.clone();
    for segment in segments {
        let Some(ty) = java_type_ref(&written, &file_path, ctx) else { return Some(JavaDeclared::External) };
        let owner = ty.decl.as_ref().map(|d| d.qualified_name.as_str());
        let named = ctx.get_nodes_by_name(segment);
        let field = named.iter().find(|n| {
            matches!(n.kind, NodeKind::Field | NodeKind::Constant)
                && n.language == Language::Java
                && match owner {
                    Some(o) => n.qualified_name == format!("{o}::{segment}"),
                    None => n.qualified_name.ends_with(&format!("::{}::{segment}", ty.name)) || n.qualified_name == format!("{}::{segment}", ty.name),
                }
        });
        let field = field?.clone();
        written = field_signature_type(&field)?;
        file_path = field.file_path.clone();
    }
    Some(java_type_ref(&written, &file_path, ctx).map_or(JavaDeclared::External, JavaDeclared::Type))
}
