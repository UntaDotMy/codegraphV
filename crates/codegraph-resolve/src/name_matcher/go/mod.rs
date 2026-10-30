//! Go name resolution: local scopes, package qualification, method lookup across a package, and
//! the type of a call chain's receiver read from source text. Port of the Go parts of
//! `name-matcher.ts`.

pub mod chain;
pub mod methods;
pub mod qualify;
pub mod scope;

pub use chain::{go_asserted_type, go_chain_links, go_valued_has_no_project_methods, match_go_chained_call, match_go_indexed_call, GoChainKind, GoChainLink};
pub use methods::{
    go_alias_target, go_asserted_local_type, go_embedded_types, go_local_value, go_unread_file_declares, go_written_type,
    is_go_asserted_local, is_go_project_local_call, match_go_asserted_call, match_go_field_chain_call, match_go_package_value_call,
    match_go_stdlib_local_call, resolve_go_method_in_package, GoLocalValue, GoProjectType, GoTypeRef,
};
pub use qualify::{
    go_bare_type_dir, go_declared_type_package, go_dot_imported_supertype, go_has_no_project_methods, go_import_package_dir,
    go_named_type, go_package_clause, go_package_dir, go_package_types, go_ref_qualification, go_ref_qualifier,
    go_type_position_target, is_go_external_qualified, is_go_name_visible, is_go_outside_package, is_go_unknown_qualified,
    is_in_go_qualifier_package, prefer_go_build_twin, GoQualification, GO_BUILTIN_FIELD_TYPES, GO_STDLIB_PACKAGES,
};
pub use scope::{
    blank_go_code, go_local_decl, go_local_declaration_at, go_ref_offset, go_scope_index, is_go_locally_bound, GoDecl, GoDeclKind,
    GoScopeIndex, GO_TYPE_KEYWORDS,
};

use crate::ctx::Ctx;
use crate::types::UnresolvedRef;

/// The type a Go local's declaration writes (`var x T`, `x := T{}`), when it writes one.
pub fn go_local_decl_type(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    go_local_decl(name, r, ctx).and_then(|d| d.ty.clone())
}

/// The call a Go local's declaration is initialised from (`x := pkg.New()`), as written.
pub fn go_local_decl_from(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    go_local_decl(name, r, ctx).and_then(|d| d.from.clone())
}
