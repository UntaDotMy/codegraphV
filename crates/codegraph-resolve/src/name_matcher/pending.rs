//! Name-matcher entry points whose TypeScript bodies are not ported yet.
//!
//! Each carries the final signature the resolver driver calls, answering "no opinion"
//! (`None` / `Tri::Undefined` / `false`, or `true` for a gate that only vetoes), so the
//! driver is complete and measurable while the matchers land one region at a time. A
//! function leaves this file when its real body is written beside the others.

use crate::ctx::Ctx;
use crate::types::{ResolvedRef, UnresolvedRef};
use std::collections::HashSet;
use std::sync::LazyLock;

/// Languages whose identifiers fold case, so name pre-filters must too.
pub static CASE_INSENSITIVE_LANGUAGES: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| ["php", "pascal", "cfml", "cfscript", "cfquery", "cobol", "vbnet"].into_iter().collect());

/// Node kinds Go types occupy.
pub const GO_TYPE_KINDS: [codegraph_types::NodeKind; 3] =
    [codegraph_types::NodeKind::Struct, codegraph_types::NodeKind::Interface, codegraph_types::NodeKind::TypeAlias];

pub fn is_go_type_kind(kind: codegraph_types::NodeKind) -> bool {
    GO_TYPE_KINDS.contains(&kind)
}

// -- Visibility predicates of `isVisibleAcrossFiles` --------------------------------------------

pub use super::cpp_tu::{is_cpp_translation_unit_local, is_included_cpp_test_suite, is_static_c_function};

// -- Dart ------------------------------------------------------------------

pub use super::dart::{
    dart_extension_member_of, dart_extension_named, dart_supertypes_of, is_dart_annotation, is_dart_constructor, is_dart_member_read,
    is_dart_unnamed_extension_member, match_dart_annotation, match_dart_member_read, match_dart_prefixed_call, match_dart_type_member_call,
};
pub use super::dart_chain::{is_dart_chain_link, match_dart_chain_link};

// -- Go --------------------------------------------------------------------

// -- Per-language reference shapes (see `lang_shape.rs`) ---------------------------------------

/// The language's view of this reference, when it has scope rules of its own.
pub fn language_shape(r: &UnresolvedRef, ctx: &Ctx) -> Option<Box<dyn super::lang_shape::RefShape>> {
    let mut shapes: Vec<Box<dyn super::lang_shape::RefShape>> = Vec::new();
    if let Some(shape) = super::rust_go_shape::RustGoShape::of(r, ctx) {
        shapes.push(Box::new(shape));
    }
    if let Some(shape) = super::kotlin::KotlinShape::of(r, ctx) {
        shapes.push(Box::new(shape));
    }
    if let Some(shape) = super::swift::SwiftShape::of(r, ctx) {
        shapes.push(Box::new(shape));
    }
    if let Some(shape) = super::objc::ObjcShape::of(r, ctx) {
        shapes.push(Box::new(shape));
    }
    if let Some(shape) = super::scala::ScalaShape::of(r) {
        shapes.push(Box::new(shape));
    }
    if let Some(shape) = super::csharp::CsharpShape::of(r) {
        shapes.push(Box::new(shape));
    }
    if let Some(shape) = super::ruby::RubyShape::of(r) {
        shapes.push(Box::new(shape));
    }
    if let Some(shape) = super::cfml::CfmlShape::of(r) {
        shapes.push(Box::new(shape));
    }
    if let Some(shape) = super::vb_scope::VbShape::of(r, ctx) {
        shapes.push(Box::new(shape));
    }
    if let Some(shape) = super::java::JavaShape::of(r) {
        shapes.push(Box::new(shape));
    }
    if let Some(shape) = super::php::PhpShape::of(r, ctx) {
        shapes.push(Box::new(shape));
    }
    if let Some(shape) = super::solidity::SolidityShape::of(r, ctx) {
        shapes.push(Box::new(shape));
    }
    if let Some(shape) = super::dart::DartShape::of(r, ctx) {
        shapes.push(Box::new(shape));
    }
    super::lang_shape::AllOf::of(shapes)
}

/// Candidate-language rules that hide a candidate regardless of the reference's language
/// (a file-local Objective-C symbol, an unnamed Dart extension, …).
pub fn is_hidden_by_language(n: &codegraph_types::Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    super::objc::is_objc_file_local_elsewhere(n, r) || super::dart::dart_extension_named(n, ctx) == Some(false)
}

// -- C / C++ -----------------------------------------------------------------------------------

pub use super::cpp_call::{
    is_cpp_arrow_through_outside_type, is_cpp_dot_call, is_cpp_template_member, match_cpp_call_chain, match_cpp_construction,
    match_cpp_declared_type_call, match_cpp_held_type_call, match_cpp_member_chain_call, match_cpp_unspelled_receiver_call,
    cpp_finds_only_namesakes, cpp_member_operator,
};
pub use super::cpp_locals::is_cpp_local_callable_call;
pub use super::cpp_method::cpp_method_of;
pub use super::cpp_overload::{
    cpp_function_overload, cpp_qualifier_arguments, cpp_qualifier_specialization_pool, cpp_specialization_pool, match_cpp_macro_namespaced,
};
pub use super::cpp_recv::{
    infer_cpp_receiver_type, is_cpp_constructor_through, is_cpp_namespace_qualifier, is_cpp_undeclared_qualifier, CppReceiverDecl,
};
pub use super::cpp_scope::match_cpp_implicit_this_call;
pub use super::cpp_tu::{cpp_visible_first, within_cpp_translation_unit};

// -- Method-call machinery whose language modules are not ported yet ---------------------------

/// `matchRustTypedReceiverCall`: no opinion, a decided answer (possibly none), or "the binding
/// was read on this line, but the inference must agree with it".
pub enum RustTyped {
    Undefined,
    Resolved(Option<ResolvedRef>),
    Unread(u32),
}

pub use crate::vbnet_receivers::{break_vb_tie, match_vb_typed_call, prefer_vb_project};
