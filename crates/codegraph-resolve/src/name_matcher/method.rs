//! Method calls: `receiver.method()`, `Class::method()`, `obj:method()`, `obj$method` — find the
//! receiver's type, then the method on it (own, inherited, or by receiver-word similarity).

use super::exact::prefer_call_site_file;
use super::infer::{array_element_type, infer_esm_awaited_call_type, infer_java_field_receiver_type, infer_local_receiver_type, normalize_inferred_type_name, ts_imported_type_file, GoReceiverDecl};
use super::js_imports::{is_import_binding, is_imported_module_receiver, is_vue_component_method};
use super::js_scope::is_locally_bound_js_name;
use super::lang_shape::is_method_owner_kind;
use super::members::{class_strategy_method, infer_member_receiver_type, inherited_class_method, is_member_typed_language, type_parameter_bound};
use super::object_literal::{es_class_supertypes, es_inherited_method, resolve_object_path_member};
use super::go::*;
use super::php::*;
use super::kotlin::is_kotlin_number_bitwise;
use super::objc::{objc_member_receiver_type, objc_receiver_reaches};
use super::csharp::{csharp_using_alias, is_csharp_type_visible};
use super::python_attr::match_python_attr_call;
use super::pending::*;
use super::python_scope::{is_python_outside_module_receiver, python_fixture_return_type};
use super::reach::is_lexically_reachable;
use super::std_methods::is_std_method_name;
use super::rust::{
    is_rust_declared_type_kind, is_rust_project_named, is_rust_trait_impl_method, match_rust_generic_path, match_rust_pinned_call,
    match_rust_self_call, match_rust_self_field_call, match_rust_self_path, match_rust_typed_receiver_call,
    match_rust_typed_self_field_call,
};
use super::ts_fields::{match_ts_this_field_call, ts_indexed_property_type, ts_known_type_call, ts_outside_type_declaration};
use super::ruby::{ruby_methods_of_kind_first, RubyMethodKind};
use super::util::*;
use super::{ambiguous_name_ceiling, same_language_family};
use crate::ctx::Ctx;
use crate::js_builtins::{JS_ARRAY_METHODS, JS_BUILTIN_METHODS, JS_BUILT_INS, TS_PRIMITIVE_TYPES};
use crate::types::{NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_search::query_utils::is_test_path;
use codegraph_types::jscompat::utf16_len;
use codegraph_types::{Language, Node, NodeKind};
use std::cell::RefCell;
use std::collections::HashSet;

const DISPATCHED_ACTIONS: [&str; 17] = [
    "get", "post", "put", "patch", "delete", "head", "options", "index", "show", "store", "update", "destroy", "create", "edit", "list", "retrieve",
    "partial_update",
];

fn is_dispatched_owner(name: &str) -> bool {
    re!(r"(?:View|ViewSet|APIView|Controller|Endpoint|ViewMixin)$").is_match(name)
}

fn is_test_double_owner(s: &str) -> bool {
    re!(r"\b(?:fake|mock|mocked|stub|dummy|spy)\b", "i").is_match(s)
}

/// The owner class name in a method's qualified name (`a::B::m` → `B`).
fn owner_of(qualified_name: &str) -> Option<&str> {
    let cut = qualified_name.rfind("::")?;
    re!(r"::|\.").split(&qualified_name[..cut]).into_iter().last()
}

/// The last link of a receiver chain, skipping a trailing SCREAMING_CASE constant.
pub fn receiver_link(receiver: &str) -> &str {
    let links: Vec<&str> = receiver.split('.').collect();
    let last = links[links.len() - 1];
    if links.len() > 1 && re!(r"^[A-Z][A-Z0-9_]+$").is_match(last) {
        links[links.len() - 2]
    } else {
        last
    }
}

pub fn split_camel_case(s: &str) -> Vec<String> {
    let a = re!(r"([a-z])([A-Z])", "g").replace_all(s, |m| Some(format!("{} {}", m.text(1).unwrap_or(""), m.text(2).unwrap_or(""))));
    let b = re!(r"([A-Z]+)([A-Z][a-z])", "g").replace_all(&a, |m| Some(format!("{} {}", m.text(1).unwrap_or(""), m.text(2).unwrap_or(""))));
    re!(r"[\s._:/\\]+").split(&b).into_iter().filter(|w| utf16_len(w) > 1).map(|w| w.to_string()).collect()
}

fn flat_word(w: &str) -> String {
    let stripped = re!(r"[^A-Za-z0-9]", "g").replace_all_str(w, "");
    re!(r"\d+$").replace_first(&stripped, |_| String::new()).to_lowercase()
}

fn names_owner_type(receiver: &str, qualified_name: &str) -> bool {
    let Some(cut) = qualified_name.rfind("::") else { return false };
    flat_word(receiver.rsplit('.').next().unwrap_or("")) == flat_word(re!(r"::|\.").split(&qualified_name[..cut]).into_iter().last().unwrap_or(""))
}

/// Does the receiver's name share a word with the class that owns the method?
pub fn shares_receiver_word(receiver: &str, qualified_name: &str) -> bool {
    if names_owner_type(receiver, qualified_name) {
        return true;
    }
    let Some(cut) = qualified_name.rfind("::") else { return false };
    let owner: HashSet<String> = split_camel_case(&qualified_name[..cut]).iter().map(|w| flat_word(w)).filter(|w| utf16_len(w) > 2).collect();
    split_camel_case(receiver).iter().any(|w| owner.contains(&flat_word(w)))
}

/// Does the (Go/Dart-aware) receiver name its method's owner?
fn receiver_names_owner(receiver: &str, method: &Node, ctx: &Ctx) -> bool {
    if method.language == Language::Go {
        return names_owner_type(receiver, &method.qualified_name);
    }
    if method.language == Language::Dart {
        let owner = method.qualified_name.rfind("::").filter(|c| *c > 0).and_then(|c| method.qualified_name[..c].rsplit("::").next()).unwrap_or("");
        let decl = if owner.is_empty() { None } else { ctx.get_nodes_by_name(owner).iter().find(|n| n.language == Language::Dart && n.file_path == method.file_path).cloned() };
        let line = decl.and_then(|d| line_at(ctx, &d.file_path, d.start_line)).unwrap_or_default();
        if let Some(on) = re!(r"\bextension\s+\w*\s*(?:<[^>]*>)?\s*on\s+([\w<>, ?]+?)\s*\{").captures(&line).and_then(|c| c.text(1).map(|s| s.to_string())) {
            let on = re!(r"[<>, ?]+", "g").replace_all_str(&on, "");
            return shares_receiver_word(receiver, &format!("{on}::{}", method.name));
        }
    }
    shares_receiver_word(receiver, &method.qualified_name)
}

fn is_unnamed_test_double(method: &Node, receiver: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let Some(cut) = method.qualified_name.rfind("::").filter(|c| *c > 0) else { return false };
    let owner = re!(r"::|\.").split(&method.qualified_name[..cut]).into_iter().last().unwrap_or("");
    if !is_test_double_owner(&split_camel_case(owner).join(" ")) {
        return false;
    }
    if split_camel_case(receiver_link(receiver)).iter().any(|w| is_test_double_owner(w)) {
        return false;
    }
    !ctx.read_file(&r.file_path).map(|s| s.contains(owner)).unwrap_or(false)
}

fn is_lua_library_call(receiver: &str, method: &str, r: &UnresolvedRef, candidate: &Node) -> bool {
    const TABLES: [&str; 19] = [
        "string", "table", "math", "io", "os", "coroutine", "debug", "utf8", "package", "bit", "bit32", "jit", "ffi", "vim", "ngx", "assert", "spy", "stub", "mock",
    ];
    const STRING_METHODS: [&str; 12] = ["find", "match", "gmatch", "gsub", "sub", "format", "upper", "lower", "len", "rep", "byte", "reverse"];
    let root = re!(r"[.:]").split(receiver).into_iter().next().unwrap_or("");
    if TABLES.contains(&root) || root == "love" {
        return re!(r"::|\.").split(&candidate.qualified_name).into_iter().next() != Some(root);
    }
    STRING_METHODS.contains(&method) && r.reference_name.ends_with(&format!(":{method}"))
}

fn names_external_type(receiver: &str, language: Language) -> bool {
    if !re!(r"^[A-Z][A-Za-z0-9_]*$").is_match(receiver) {
        return false;
    }
    if language == Language::Pascal {
        return re!(r"^(?:[TEI][A-Z]\w*|Exception)$").is_match(receiver);
    }
    if language == Language::Rust {
        return receiver != "Self" && re!(r"[a-z]").is_match(receiver);
    }
    !matches!(language, Language::Go | Language::C | Language::Cpp)
}

fn is_untyped_receiver_language(l: Language) -> bool {
    matches!(l, Language::Ruby | Language::Cfml | Language::CfScript | Language::ObjC | Language::Php)
}

pub(crate) fn is_undeclared_type_name(type_name: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    re!(r"^[A-Z]").is_match(type_name) && !ctx.get_nodes_by_name(type_name).iter().any(|n| is_method_owner_kind(n) && same_language_family(n.language, r.language))
}

pub(crate) fn imported_fqn_of(type_name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    ctx.get_import_mappings(&r.file_path, r.language).iter().find(|i| i.local_name == type_name).map(|i| i.source.clone())
}

// -- Method on a type --------------------------------------------------------------------------

/// The method `method_name` on `type_name` (own, inherited through supertypes, or an extension).
pub fn resolve_method_on_type(
    type_name: &str,
    method_name: &str,
    r: &UnresolvedRef,
    ctx: &Ctx,
    confidence: f64,
    resolved_by: ResolvedBy,
    preferred_fqn: Option<&str>,
    depth: u32,
) -> Option<ResolvedRef> {
    let mut matches: Vec<NodeRef> = ctx.get_method_matches(type_name, method_name, r.language).iter().cloned().collect();
    if r.language == Language::Go {
        if let Some(fqn) = preferred_fqn {
            let scoped = resolve_go_method_in_package(type_name, method_name, fqn, &matches, r, ctx, confidence, resolved_by, depth);
            if !scoped.is_undefined() {
                return scoped.value();
            }
        }
    }
    if matches.is_empty() {
        if depth < 4 {
            let es = is_es_import_language(r.language);
            if depth == 0 && es {
                if let Some(inherited) = es_inherited_method(type_name, method_name, preferred_fqn.unwrap_or(&r.file_path), r, ctx) {
                    return Some(ResolvedRef::new(r, inherited.id.clone(), confidence, resolved_by));
                }
            }
            let supertypes: Vec<String> =
                if es { es_class_supertypes(type_name, r.language, ctx) } else { ctx.get_supertypes(type_name, r.language).iter().cloned().collect() };
            for supertype in supertypes {
                if let Some(via) = resolve_method_on_type(&supertype, method_name, r, ctx, confidence, resolved_by, preferred_fqn, depth + 1) {
                    return Some(via);
                }
            }
        }
        if r.language == Language::Dart && depth == 0 {
            if let Some(ext) = dart_extension_member_of(type_name, method_name, r, ctx) {
                return Some(ResolvedRef::new(r, ext.id.clone(), confidence, resolved_by));
            }
        }
        return None;
    }
    if r.language == Language::Rust && matches.len() > 1 {
        let inherent: Vec<NodeRef> = matches.iter().filter(|m| !is_rust_trait_impl_method(m, ctx)).cloned().collect();
        if !inherent.is_empty()
            && inherent.len() < matches.len()
            && ctx.get_nodes_by_name(type_name).iter().filter(|n| n.language == Language::Rust && is_rust_declared_type_kind(n.kind)).count() <= 1
        {
            matches = inherent;
        }
    }
    if matches.len() > 1 {
        if let Some(fqn) = preferred_fqn {
            if is_es_import_language(r.language) {
                if let Some(chosen) = matches.iter().find(|m| m.file_path == fqn) {
                    return Some(ResolvedRef::new(r, chosen.id.clone(), confidence, resolved_by));
                }
            } else if r.language != Language::Go {
                let ext = if r.language == Language::Kotlin { ".kt" } else { ".java" };
                let fqn_path = format!("{}{ext}", fqn.replace('.', "/"));
                let chosen = matches.iter().find(|m| {
                    let fp = m.file_path.replace('\\', "/");
                    fp.ends_with(&fqn_path) || fp.ends_with(&format!("/{fqn_path}"))
                });
                if let Some(chosen) = chosen {
                    return Some(ResolvedRef::new(r, chosen.id.clone(), confidence, resolved_by));
                }
            }
        }
    }
    if r.reference_kind == codegraph_types::ReferenceKind::FunctionRef && matches.len() != 1 {
        return None;
    }
    let visible = cpp_visible_first(matches, r, ctx);
    // A Ruby receiver is typed only by `x = Widget.new`, so it is an instance: `x.build` reaches
    // `def build`, not `def self.build`, wherever each is.
    let ordered = ruby_methods_of_kind_first(
        prefer_call_site_file(within_cpp_translation_unit(visible, r, ctx, false), &r.file_path),
        ruby_kind(r, RubyMethodKind::Instance),
    );
    Some(ResolvedRef::new(r, ordered[0].id.clone(), confidence, resolved_by))
}

/// `kind` for a Ruby reference, none for any other.
fn ruby_kind(r: &UnresolvedRef, kind: RubyMethodKind) -> Option<RubyMethodKind> {
    (r.language == Language::Ruby).then_some(kind)
}

// -- The receiver and its method ------------------------------------------------------------

#[derive(PartialEq, Clone, Copy)]
enum MatchKind {
    Dot,
    Colon,
    LuaColon,
    RDollar,
}

thread_local! {
    static PY_CHAIN_GUESSING: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

fn chain_key(r: &UnresolvedRef) -> String {
    format!("{}\0{}\0{}\0{}\0{}", r.from_node_id, r.reference_name, r.line, r.column, r.file_path)
}

pub fn match_method_call(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let name = r.reference_name.as_str();
    let dot_match = (if is_js_family(r.language) { re!(r"^([\w.]+)\.([\w$]+)$").captures(name) } else { None })
        .or_else(|| re!(r"^([\w.]+)\.(\w+:?(?:\w+:)*)$").captures(name))
        .or_else(|| if r.language == Language::Cpp { re!(r"^([\w.]+)\.(operator[^\w\s.]+)$").captures(name) } else { None });
    let colon_match = re!(r"^(\w+)::(\w+)$").captures(name);
    let lua_colon_match = if is_lua(r.language) { re!(r"^([\w.]+):(\w+)$").captures(name) } else { None };
    let r_dollar_match = if r.language == Language::R { re!(r"^([\w.]+)\$(\w+)$").captures(name) } else { None };
    if r.language == Language::Php {
        if let Some(m) = re!(r"^(this->\w+)\.(\w+)$").captures(name) {
            let receiver = m.text(1).unwrap_or("");
            let inferred = infer_local_receiver_type(receiver, r, ctx, None)?;
            let fqn = imported_fqn_of(&inferred, r, ctx);
            return resolve_method_on_type(&inferred, m.text(2).unwrap_or(""), r, ctx, 0.9, ResolvedBy::InstanceMethod, fqn.as_deref(), 0);
        }
    }
    if matches!(r.language, Language::TypeScript | Language::JavaScript | Language::Tsx | Language::Jsx) {
        if let Some(m) = re!(r"^this\.(#[\w$]+)\.(\w+)$").captures(name) {
            return match_ts_this_field_call(m.text(1).unwrap_or(""), m.text(2).unwrap_or(""), r, ctx, false);
        }
        // An element of a field — `this.listeners[type].push()`, emitted as
        // `this.listeners[].push`: the element type the field's declaration writes, or nothing.
        if let Some(m) = re!(r"^this\.(#?[\w$]+)\[\]\.([\w$]+)$").captures(name) {
            return match_ts_this_field_call(m.text(1).unwrap_or(""), m.text(2).unwrap_or(""), r, ctx, true);
        }
    }
    let (kind, caps) = if let Some(m) = dot_match {
        (MatchKind::Dot, m)
    } else if let Some(m) = colon_match {
        (MatchKind::Colon, m)
    } else if let Some(m) = lua_colon_match {
        (MatchKind::LuaColon, m)
    } else if let Some(m) = r_dollar_match {
        (MatchKind::RDollar, m)
    } else {
        if r.language == Language::Cpp {
            return match_cpp_implicit_this_call(r, ctx);
        }
        return None;
    };
    let object_or_class = caps.text(1).unwrap_or("").to_string();
    let method_name = caps.text(2).unwrap_or("").to_string();
    let dot = kind == MatchKind::Dot;
    // A JS/TS call of a member no node in the index is named (`list.push()`, `fs.writeFileSync()`)
    // is one only the object-literal reader can link (#2829): `const api = { alias: renamed }`
    // makes `api.alias()` a call of `renamed`. Every other strategy looks the member up by its own
    // name and finds nothing, so typing the receiver is wasted work. When the reader answers, the
    // whole path below runs and decides, as before.
    if is_calls(r) && is_js_family(r.language) && ctx.known_names().is_some_and(|names| !names.contains(&method_name)) {
        let literal = dot && !object_or_class.contains('.') && matches!(resolve_object_path_member(&object_or_class, &method_name, r, ctx, None), Tri::Value(_));
        if !literal {
            return None;
        }
    }
    let python_attr_receiver = r.language == Language::Python && dot && object_or_class.contains('.');
    let inferable_receiver = matches!(kind, MatchKind::Dot | MatchKind::LuaColon | MatchKind::RDollar) && !python_attr_receiver;
    let go_std = r.language == Language::Go && dot && !object_or_class.contains('.') && is_std_method_name("go", &method_name);

    if r.language == Language::Go && dot && GO_STDLIB_PACKAGES.contains(object_or_class.as_str()) {
        let v = match_go_stdlib_local_call(&object_or_class, &method_name, r, ctx);
        if !v.is_undefined() {
            return v.value();
        }
    }
    if r.language == Language::Go && dot && !object_or_class.contains('.') {
        let v = match_go_package_value_call(&object_or_class, &method_name, r, ctx);
        if !v.is_undefined() {
            return v.value();
        }
    }
    if r.language == Language::Dart && dot {
        let v = match_dart_type_member_call(&object_or_class, &method_name, r, ctx);
        if !v.is_undefined() {
            return v.value();
        }
    }
    if r.language == Language::Rust && dot && object_or_class == "self" {
        return match_rust_self_call(&method_name, r, ctx);
    }
    let mut rust_binding_line: Option<u32> = None;
    if r.language == Language::Rust && dot && re!(r"^[a-z_]\w*$").is_match(&object_or_class) && object_or_class != "self" {
        match match_rust_typed_receiver_call(&object_or_class, &method_name, r, ctx) {
            RustTyped::Undefined => {}
            RustTyped::Resolved(v) => return v,
            RustTyped::Unread(line) => rust_binding_line = Some(line),
        }
    }
    if inferable_receiver {
        if r.language == Language::VbNet && dot {
            let typed = match_vb_typed_call(&object_or_class, &method_name, r, ctx);
            if !typed.is_undefined() {
                return typed.value();
            }
        }
        let mut decl = GoReceiverDecl::default();
        let mut cpp_decl = CppReceiverDecl::default();
        let mut inferred_type: Option<String> = if r.language == Language::Cpp {
            infer_cpp_receiver_type(&object_or_class, r, ctx, 0, &mut cpp_decl)
        } else {
            infer_local_receiver_type(&object_or_class, r, ctx, Some(&mut decl))
        };
        if let (Some(line), Some(t)) = (rust_binding_line, inferred_type.as_ref()) {
            if decl.line != Some(line) {
                if is_undeclared_type_name(t.rsplit('.').next().unwrap_or(""), r, ctx) {
                    return None;
                }
                inferred_type = None;
            }
        }
        if r.language == Language::Cpp {
            let declared = match_cpp_declared_type_call(&object_or_class, &method_name, &cpp_decl, r, ctx);
            if !declared.is_undefined() {
                return declared.value();
            }
            if inferred_type.is_some() && is_cpp_arrow_through_outside_type(&object_or_class, &cpp_decl, r, ctx) {
                inferred_type = None;
            }
            if let (Some(t), Some(raw)) = (inferred_type.as_ref(), cpp_decl.raw.as_ref()) {
                if !cpp_decl.aliased && cpp_finds_only_namesakes(raw, ctx, Some(&method_name)) && !is_cpp_template_member(raw, r, ctx) {
                    if re!(r"^[A-Z]").is_match(t) && (cpp_decl.pointer || cpp_member_operator(&object_or_class, r, ctx) != "->") {
                        return None;
                    }
                    inferred_type = None;
                }
            }
        }
        if inferred_type.is_none() && r.language == Language::ObjC && dot {
            inferred_type = objc_member_receiver_type(&object_or_class, r, ctx);
        }
        if inferred_type.is_none() && r.language == Language::Python && dot {
            inferred_type = python_fixture_return_type(&object_or_class, r, ctx);
        }
        // A Kotlin local set from an unqualified call holds what that function declares it returns:
        // `val repo = repository()` (#2805), where the receiver's name led to the interface `Repo`.
        if inferred_type.is_none() && r.language == Language::Kotlin && dot && re!(r"^[a-z_]\w*$").is_match(&object_or_class) {
            inferred_type = super::kotlin_call_result::kotlin_local_call_result_type(&object_or_class, r, ctx);
        }
        if inferred_type.is_none() && r.language == Language::Python && dot && is_python_outside_module_receiver(&object_or_class, r, ctx) {
            return None;
        }
        if inferred_type.is_none() && r.language == Language::Python && dot && re!(r"^\w+$").is_match(&object_or_class) {
            if let Tri::Value(via) = super::python::python_global_receiver_target(&object_or_class, &method_name, r, ctx, true) {
                return Some(ResolvedRef::new(r, via.id.clone(), 0.9, ResolvedBy::InstanceMethod));
            }
        }
        if matches!(decl.declared, Tri::Null) {
            return None;
        }
        let go_declared = decl.declared.clone();
        if go_std && go_declared.is_undefined() && decl.asserted.is_undefined() {
            if let Some(from) = go_local_decl_from(&object_or_class, r, ctx) {
                if is_go_outside_package(&from, &r.file_path, ctx) {
                    return None;
                }
            }
        }
        if inferred_type.is_none() && is_member_typed_language(r.language) && dot {
            inferred_type = infer_member_receiver_type(&object_or_class, r, ctx);
            if inferred_type.as_ref().is_some_and(|t| re!(r"^[a-z]").is_match(t)) {
                return None;
            }
        }
        if let Some(t) = inferred_type.clone() {
            if is_member_typed_language(r.language) && !ctx.get_nodes_by_name(&t).iter().any(|n| is_method_owner_kind(n)) {
                match type_parameter_bound(&t, r, ctx) {
                    Tri::Null => return None,
                    Tri::Value(b) => inferred_type = Some(b),
                    Tri::Undefined => {}
                }
            }
        }
        let awaited = if inferred_type.is_none() && is_esm_family(r.language) { infer_esm_awaited_call_type(&object_or_class, r, ctx) } else { None };
        if let Some(a) = &awaited {
            match &a.name {
                Some(n) if !n.is_empty() && !TS_PRIMITIVE_TYPES.contains(n.as_str()) => inferred_type = Some(n.clone()),
                _ => return None,
            }
        }
        // A TS/JS receiver the calling function doesn't type is read by its binding in scope at
        // the call (#2589): a module's `let root: Root | null`, a `let popupRoot: Root` in the
        // function around the one calling, or an outer function's typed parameter.
        if inferred_type.is_none()
            && awaited.is_none()
            && dot
            && re!(r"^[A-Za-z_$][\w$]*$").is_match(&object_or_class)
            && matches!(r.language, Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx)
            && object_or_class != "this"
            && object_or_class != "super"
            && !is_import_binding(&object_or_class, r, ctx)
            && is_locally_bound_js_name(&object_or_class, &r.file_path, ctx)
        {
            if let Some(reading) = super::js_binding::js_receiver_binding_reading(&object_or_class, r, ctx) {
                inferred_type = Some(reading.ty);
                decl.raw = Some(reading.raw);
                decl.line = Some(reading.line);
            }
        }
        if let Some(t) = inferred_type.clone() {
            if is_esm_family(r.language) && t.ends_with(']') {
                let element = array_element_type(&t);
                let declared = match &element {
                    Some(e) => Some((e.clone(), true, None)),
                    None => ts_indexed_property_type(&t, r, ctx).map(|d| (d.ty, d.array, Some(d.file))),
                };
                match declared {
                    None => inferred_type = Some(re!(r"\s*\[[^\]]*\]$").replace_first(&t, |_| String::new())),
                    Some((ty, array, file)) => {
                        if array && JS_ARRAY_METHODS.contains(method_name.as_str()) {
                            return None;
                        }
                        inferred_type = Some(normalize_inferred_type_name(&ty)?);
                        if element.is_none() {
                            return resolve_method_on_type(inferred_type.as_deref().unwrap(), &method_name, r, ctx, 0.9, ResolvedBy::InstanceMethod, file.as_deref(), 0);
                        }
                    }
                }
            }
        }
        if r.language == Language::Rust && inferred_type.as_deref() == Some("Pin") && !is_rust_project_named("Pin", ctx) {
            return match_rust_pinned_call(&object_or_class, &method_name, r, ctx);
        }
        if let Some(t) = inferred_type.clone() {
            match &decl.asserted {
                Tri::Value(a) => return resolve_method_on_type(&a.name, &method_name, r, ctx, 0.9, ResolvedBy::InstanceMethod, Some(&a.dir), 0),
                Tri::Null => return None,
                Tri::Undefined => {}
            }
            if let Tri::Value(d) = &go_declared {
                if go_has_no_project_methods(d, r, ctx) {
                    return None;
                }
            }
            let mut imported_fqn: Option<String> = if matches!(r.language, Language::Java | Language::Kotlin) {
                imported_fqn_of(&t, r, ctx)
            } else if r.language == Language::Go {
                go_declared_type_package(decl.raw.as_deref(), &r.file_path, ctx)
            } else if is_esm_family(r.language) && awaited.is_none() {
                ts_imported_type_file(decl.raw.as_deref().unwrap_or(&t), r, ctx)
            } else {
                None
            };
            if is_esm_family(r.language) && awaited.is_none() && imported_fqn.is_none() {
                match ts_outside_type_declaration(decl.raw.as_deref().unwrap_or(&t), &t, r, ctx) {
                    Tri::Null => return None,
                    Tri::Value(f) => imported_fqn = Some(f),
                    Tri::Undefined => {}
                }
            }
            if is_esm_family(r.language) && awaited.is_none() {
                let known = ts_known_type_call(&t, imported_fqn.as_deref().unwrap_or(&r.file_path), &method_name, r, r, ctx, 0.9);
                if let Tri::Value(k) = known {
                    return Some(k);
                }
            }
            let mut at = r.clone();
            if let Some(a) = &awaited {
                at.file_path = a.file_path.clone();
            }
            let typed_match = resolve_method_on_type(&t, &method_name, &at, ctx, 0.9, ResolvedBy::InstanceMethod, imported_fqn.as_deref(), 0);
            if let Some(tm) = &typed_match {
                if r.language == Language::Kotlin {
                    if let Some(target) = ctx.node_by_id(&tm.target_node_id) {
                        if is_kotlin_number_bitwise(&target, r) {
                            return None;
                        }
                    }
                }
            }
            if let Some(mut tm) = typed_match {
                if let Some(a) = &awaited {
                    let target = ctx.node_by_id(&tm.target_node_id)?;
                    if target.qualified_name.starts_with(&format!("{t}::")) && target.file_path != a.file_path {
                        return None;
                    }
                    tm.original = r.clone();
                    return Some(tm);
                }
                return Some(tm);
            }
            if awaited.is_some() {
                return None;
            }
            if is_esm_family(r.language) && (JS_BUILT_INS.contains(t.as_str()) || TS_PRIMITIVE_TYPES.contains(t.as_str())) {
                return None;
            }
            if go_std && !go_declared.is_undefined() {
                return None;
            }
            if r.language == Language::Go {
                if let Some(fqn) = &imported_fqn {
                    if go_unread_file_declares(&t, &method_name, fqn, ctx) {
                        return None;
                    }
                }
            }
            if is_undeclared_type_name(t.rsplit('.').next().unwrap_or(""), r, ctx) {
                return None;
            }
        }
        if cpp_decl.aliased {
            if !cpp_decl.class_template && (cpp_decl.pointer || is_cpp_dot_call(&object_or_class, r, ctx)) {
                return None;
            }
            if is_undeclared_type_name(cpp_decl.written.as_deref().unwrap_or(""), r, ctx) {
                return None;
            }
        }
        if r.language == Language::Cpp && cpp_decl.callers && !cpp_decl.shadowed && !cpp_decl.pointer && cpp_member_operator(&object_or_class, r, ctx) == "->" {
            let held = match_cpp_held_type_call(cpp_decl.raw.as_deref().unwrap_or(""), &method_name, r, ctx, cpp_decl.declared.as_ref());
            if !held.is_undefined() {
                return held.value();
            }
        }
    }
    if r.language == Language::Go && dot && object_or_class.contains('.') {
        return match_go_field_chain_call(&object_or_class, &method_name, r, ctx);
    }
    if r.language == Language::Rust && dot && object_or_class.starts_with("self.") {
        let field = &object_or_class["self.".len()..];
        let typed = match_rust_typed_self_field_call(field, &method_name, r, ctx);
        if !typed.is_undefined() {
            return typed.value();
        }
        return match_rust_self_field_call(field, &method_name, r, ctx);
    }
    if matches!(r.language, Language::TypeScript | Language::JavaScript | Language::Tsx | Language::Jsx) && dot && object_or_class.starts_with("this.") {
        return match_ts_this_field_call(&object_or_class["this.".len()..], &method_name, r, ctx, false);
    }
    if python_attr_receiver && !PY_CHAIN_GUESSING.with(|g| g.borrow().contains(&chain_key(r))) {
        let mut read: std::collections::BTreeSet<String> = Default::default();
        let typed = match_python_attr_call(&object_or_class, &method_name, r, ctx, &mut read);
        if !typed.is_undefined() {
            return typed.value();
        }
        let key = chain_key(r);
        PY_CHAIN_GUESSING.with(|g| g.borrow_mut().insert(key.clone()));
        let guess = match_method_call(r, ctx);
        PY_CHAIN_GUESSING.with(|g| g.borrow_mut().remove(&key));
        read.remove(&r.file_path);
        return match guess {
            Some(mut g) if !read.is_empty() => {
                let type_from: Vec<serde_json::Value> = read.into_iter().map(serde_json::Value::String).collect();
                g.metadata.get_or_insert_with(Default::default).insert("typeFrom".to_string(), serde_json::Value::Array(type_from));
                Some(g)
            }
            other => other,
        };
    }
    if matches!(r.language, Language::Java | Language::Kotlin) && dot {
        if let Some(t) = infer_java_field_receiver_type(&object_or_class, r, ctx) {
            let fqn = imported_fqn_of(&t, r, ctx);
            if let Some(m) = resolve_method_on_type(&t, &method_name, r, ctx, 0.9, ResolvedBy::InstanceMethod, fqn.as_deref(), 0) {
                return Some(m);
            }
        }
    }
    if dot && !object_or_class.contains('.') && is_object_literal_language(r.language) {
        if let Tri::Value(m) = resolve_object_path_member(&object_or_class, &method_name, r, ctx, None) {
            return Some(m);
        }
    }
    if r.language == Language::Rust && kind == MatchKind::Colon && object_or_class == "Self" {
        let own = match_rust_self_path(&method_name, r, ctx);
        if !own.is_undefined() {
            return own.value();
        }
    }
    if r.language == Language::Rust && kind == MatchKind::Colon && re!(r"^[A-Z]\w*$").is_match(&object_or_class) && object_or_class != "Self" {
        let bounded = match_rust_generic_path(&object_or_class, &method_name, r, ctx);
        if !bounded.is_undefined() {
            return bounded.value();
        }
    }
    // Strategy 1: a class named exactly like the receiver.
    {
        let named: Vec<NodeRef> = ctx.get_nodes_by_name(&object_or_class).iter().filter(|n| is_method_owner_kind(n)).cloned().collect();
        let mut class_candidates = prefer_call_site_file(within_cpp_translation_unit(named, r, ctx, false), &r.file_path);
        if r.language == Language::CSharp && class_candidates.len() > 1 {
            let mut type_ref = r.clone();
            type_ref.reference_name = object_or_class.clone();
            let (visible, rest): (Vec<NodeRef>, Vec<NodeRef>) =
                class_candidates.iter().cloned().partition(|c| c.language != Language::CSharp || is_csharp_type_visible(c, &type_ref, ctx));
            class_candidates = visible.into_iter().chain(rest).collect();
        }
        if r.language == Language::VbNet {
            class_candidates = prefer_vb_project(class_candidates, r, ctx);
        }
        let production_caller = !is_test_path(&r.file_path);
        for class_node in &class_candidates {
            if class_node.language != r.language {
                continue;
            }
            if !is_lexically_reachable(class_node, r, ctx) {
                continue;
            }
            if production_caller && is_test_path(&class_node.file_path) {
                continue;
            }
            if class_node.language == Language::Cpp {
                let Some(method) = cpp_method_of(class_node, &method_name, r, ctx) else { continue };
                let own = method.qualified_name == format!("{}::{}", class_node.qualified_name, method_name);
                return Some(ResolvedRef::new(r, method.id.clone(), if own { 0.85 } else { 0.8 }, ResolvedBy::QualifiedName));
            }
            // A Ruby call on a class is a call of its class method: `Widget.build` reaches
            // `def self.build`, not `def build`, whichever comes first.
            if let Some(method) = class_strategy_method(class_node, &method_name, ctx, ruby_kind(r, RubyMethodKind::Class)) {
                return Some(ResolvedRef::new(r, method.id.clone(), 0.85, ResolvedBy::QualifiedName));
            }
        }
        let same_language: Vec<NodeRef> = class_candidates.iter().filter(|c| c.language == r.language).cloned().collect();
        if let Some(inherited) = inherited_class_method(&same_language, &method_name, ctx, ruby_kind(r, RubyMethodKind::Class)) {
            return Some(ResolvedRef::new(r, inherited.id.clone(), 0.8, ResolvedBy::QualifiedName));
        }
    }
    if r.language == Language::Cpp
        && kind == MatchKind::Colon
        && cpp_qualifier_arguments(r, ctx).is_some()
        && ctx.get_nodes_by_name(&object_or_class).iter().any(|n| n.language == Language::Cpp && is_method_owner_kind(n))
    {
        return None;
    }
    if is_calls(r) && is_js_family(r.language) && object_or_class != "this" && object_or_class != "super" && JS_BUILTIN_METHODS.contains(method_name.as_str()) {
        return None;
    }
    if is_calls(r) && is_js_family(r.language) && is_imported_module_receiver(&object_or_class, r, ctx) {
        return None;
    }
    if r.language == Language::CSharp {
        if let Some(aliased) = csharp_using_alias(&object_or_class, r, ctx) {
            return resolve_method_on_type(&aliased, &method_name, r, ctx, 0.9, ResolvedBy::InstanceMethod, None, 0);
        }
    }
    let types_only = matches!(r.language, Language::CSharp | Language::Java | Language::Rust);
    if names_external_type(&object_or_class, r.language)
        && !ctx.get_nodes_by_name(&object_or_class).iter().any(|n| {
            same_language_family(n.language, r.language)
                && (!types_only
                    || is_method_owner_kind(n)
                    || matches!(n.kind, NodeKind::Enum | NodeKind::Namespace | NodeKind::Module | NodeKind::Trait | NodeKind::TypeAlias))
        })
    {
        return None;
    }
    if r.language == Language::Rust && kind == MatchKind::Colon && re!(r"^[a-z_][\w]*(?:::[a-z_]\w*)*$").is_match(&object_or_class) {
        return None;
    }
    if r.language == Language::CSharp && re!(r"^(?:string|object|int|long|short|byte|bool|char|double|float|decimal|uint|ulong|ushort|sbyte)$").is_match(&object_or_class) {
        return None;
    }
    let through_cpp_namespace = || r.language == Language::Cpp && kind == MatchKind::Colon && is_cpp_namespace_qualifier(&object_or_class, r, ctx);
    let capitalized = {
        let mut c = object_or_class.chars();
        match c.next() {
            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            None => String::new(),
        }
    };
    if capitalized != object_or_class {
        let named: Vec<NodeRef> = ctx.get_nodes_by_name(&capitalized).iter().filter(|n| is_method_owner_kind(n)).cloned().collect();
        let fuzzy = prefer_call_site_file(within_cpp_translation_unit(named, r, ctx, false), &r.file_path);
        let production_caller = !is_test_path(&r.file_path);
        for class_node in &fuzzy {
            if class_node.language != r.language {
                continue;
            }
            if !is_lexically_reachable(class_node, r, ctx) {
                continue;
            }
            if production_caller && is_test_path(&class_node.file_path) {
                continue;
            }
            // A Ruby receiver named after a class may hold the class as well as an instance of it,
            // so neither of a class's two methods of the name is preferred: the first declared is taken.
            if let Some(method) = class_strategy_method(class_node, &method_name, ctx, None) {
                if through_cpp_namespace() {
                    return None;
                }
                return Some(ResolvedRef::new(r, method.id.clone(), 0.8, ResolvedBy::InstanceMethod).guessed_by_name());
            }
        }
    }
    if !method_name.is_empty() {
        return match_by_method_name(r, ctx, &object_or_class, &method_name, kind == MatchKind::Colon, &through_cpp_namespace);
    }
    None
}

/// The last strategy: the method's name alone, narrowed by reach and by the receiver's words.
fn match_by_method_name(
    r: &UnresolvedRef,
    ctx: &Ctx,
    object_or_class: &str,
    method_name: &str,
    colon: bool,
    through_cpp_namespace: &dyn Fn() -> bool,
) -> Option<ResolvedRef> {
    let method_candidates = ctx.get_nodes_by_name(method_name);
    if method_candidates.len() > ambiguous_name_ceiling() {
        return None;
    }
    let methods: Vec<NodeRef> = method_candidates.iter().filter(|n| n.kind == NodeKind::Method && n.name == method_name).cloned().collect();
    let same_language: Vec<NodeRef> = methods.iter().filter(|m| m.language == r.language).cloned().collect();
    let mut target: Vec<NodeRef> = if !same_language.is_empty() { same_language } else { methods };
    let mut narrowed = false;
    if is_js_family(r.language) && is_import_binding(object_or_class, r, ctx) {
        let kept: Vec<NodeRef> = target.iter().filter(|m| m.file_path != r.file_path).cloned().collect();
        narrowed = kept.len() != target.len();
        target = kept;
    }
    {
        let kept: Vec<NodeRef> = target
            .iter()
            .filter(|m| {
                !(DISPATCHED_ACTIONS.contains(&m.name.as_str()) && owner_of(&m.qualified_name).is_some_and(is_dispatched_owner)) && !is_kotlin_number_bitwise(m, r)
            })
            .cloned()
            .collect();
        narrowed |= kept.len() != target.len();
        target = kept;
    }
    {
        let kept: Vec<NodeRef> = target
            .iter()
            .filter(|m| !is_dart_unnamed_extension_member(m, ctx) || crate::dart_libraries::in_same_dart_library(&r.file_path, &m.file_path, ctx))
            .cloned()
            .collect();
        narrowed |= kept.len() != target.len();
        target = kept;
    }
    if !is_test_path(&r.file_path) {
        target.retain(|m| !is_test_path(&m.file_path));
    }
    let reachable: Vec<NodeRef> = target.iter().filter(|m| is_lexically_reachable(m, r, ctx)).cloned().collect();
    let scoped_out = reachable.len() != target.len();
    target = reachable;
    if is_js_family(r.language) {
        let kept: Vec<NodeRef> = target.iter().filter(|m| !is_vue_component_method(m) || (object_or_class == "this" && m.file_path == r.file_path)).cloned().collect();
        narrowed |= kept.len() != target.len();
        target = kept;
    }
    // A React class component's `render()` and lifecycle methods are React's to call (#2589):
    // react-dom's `root.render(<App />)` is no error boundary's `render`.
    if is_js_family(r.language) && object_or_class != "this" && object_or_class != "super" {
        let kept: Vec<NodeRef> = target.iter().filter(|m| !super::js_binding::is_react_lifecycle_method(m, ctx)).cloned().collect();
        narrowed |= kept.len() != target.len();
        target = kept;
    }
    let python_chain = r.language == Language::Python && object_or_class.contains('.');
    if target.len() == 1
        && !narrowed
        && !scoped_out
        && !python_chain
        && target[0].language == r.language
        && !is_unnamed_test_double(&target[0], object_or_class, r, ctx)
        && !(is_lua(r.language) && is_lua_library_call(object_or_class, method_name, r, &target[0]))
        && !(is_std_method_name(r.language.as_str(), method_name)
            && !re!(r"^(?:self|Self|this|base)$").is_match(object_or_class)
            && !receiver_names_owner(receiver_link(object_or_class), &target[0], ctx))
        && !(is_untyped_receiver_language(r.language)
            && !re!(r"^(?:self|self\.class|this|super|weak_?self|strong_?self)$", "i").is_match(object_or_class)
            && !shares_receiver_word(object_or_class, &target[0].qualified_name)
            && !(r.language == Language::ObjC && objc_receiver_reaches(object_or_class, &target[0], ctx))
            && !(r.language == Language::Php && php_receiver_reaches(object_or_class, &target[0], ctx)))
    {
        if through_cpp_namespace() && !is_cpp_constructor_through(object_or_class, &target[0], ctx) {
            return None;
        }
        return Some(ResolvedRef::new(r, target[0].id.clone(), 0.7, ResolvedBy::InstanceMethod).guessed_by_name());
    }
    if target.len() > 1 || (target.len() == 1 && (scoped_out || python_chain) && !narrowed) {
        let link = if python_chain { object_or_class.rsplit('.').next().unwrap_or("") } else { receiver_link(object_or_class) };
        let receiver_words = split_camel_case(link);
        let head = receiver_words.last().map(|w| w.to_lowercase());
        let whole_receiver = link.replace('_', "").to_lowercase();
        let singular = |w: &str| {
            let l = w.to_lowercase();
            l.strip_suffix('s').map(|s| s.to_string()).unwrap_or(l)
        };
        let mut best: Option<NodeRef> = None;
        let mut best_score = 0i64;
        let mut tied: Vec<NodeRef> = Vec::new();
        let std = is_std_method_name(r.language.as_str(), method_name) && !re!(r"^(?:self|Self|this|base)$").is_match(object_or_class);
        for method in prefer_call_site_file(within_cpp_translation_unit(target.clone(), r, ctx, false), &r.file_path) {
            if std && !receiver_names_owner(receiver_link(object_or_class), &method, ctx) {
                continue;
            }
            let cut = method.qualified_name.rfind("::").filter(|c| *c > 0);
            let class_words: Vec<String> = match cut {
                Some(c) => split_camel_case(re!(r"::|\.").split(&method.qualified_name[..c]).into_iter().last().unwrap_or("")),
                None => Vec::new(),
            };
            if python_chain {
                if let Some(h) = &head {
                    if !class_words.iter().any(|cw| singular(cw) == singular(h)) && whole_receiver != class_words.join("").to_lowercase() {
                        continue;
                    }
                }
            }
            let mut score = receiver_words.iter().filter(|w| class_words.iter().any(|cw| cw.to_lowercase() == w.to_lowercase())).count() as i64;
            if is_test_double_owner(&class_words.join(" "))
                && !receiver_words.iter().any(|w| is_test_double_owner(w))
                && !ctx
                    .read_file(&r.file_path)
                    .map(|s| s.contains(re!(r"::|\.").split(&method.qualified_name[..cut.unwrap_or(0)]).into_iter().last().unwrap_or("")))
                    .unwrap_or(false)
            {
                continue;
            }
            if let (Some(h), Some(last)) = (&head, class_words.last()) {
                if h == &last.to_lowercase() {
                    score += 1;
                }
            }
            if !class_words.is_empty() && whole_receiver == class_words.join("").to_lowercase() {
                score += 2;
            }
            if method.language == r.language {
                score += 1;
            }
            if score > best_score {
                best_score = score;
                best = Some(method.clone());
                tied = vec![method];
            } else if score == best_score {
                tied.push(method);
            }
        }
        if r.language == Language::VbNet && tied.len() > 1 && best_score >= 2 {
            best = break_vb_tie(&tied, r, ctx);
        }
        if let Some(best) = best {
            if best_score >= 2 && best.id != r.from_node_id {
                if r.language == Language::Cpp && colon && is_cpp_undeclared_qualifier(object_or_class, r, ctx) {
                    return None;
                }
                if through_cpp_namespace() && !is_cpp_constructor_through(object_or_class, &best, ctx) {
                    return None;
                }
                return Some(ResolvedRef::new(r, best.id.clone(), 0.65, ResolvedBy::InstanceMethod).guessed_by_name());
            }
        }
    }
    None
}

