//! Rust name resolution: `use` trees and module layout, impl headers, written-type reading, and
//! method calls resolved from a receiver's type. Port of the Rust parts of `name-matcher.ts`.

pub mod binding;
pub mod calls;
pub mod impls;
pub mod scope;
pub mod types;
pub mod uses;

pub use binding::{is_rust_name_from_std, is_rust_scoped_ref, match_rust_use_bound_name, rust_path_target};
pub use calls::{
    is_rust_path_call_reachable, is_rust_typed_ref, is_rust_visible_across_files, match_rust_as_mut_chain_call, match_rust_generic_path, match_rust_pinned_call,
    match_rust_self_call, match_rust_self_field_call, match_rust_self_path, match_rust_typed_receiver_call,
    match_rust_typed_self_field_call, match_rust_written_receiver_call, rust_chained_call_by_name, rust_field_type_name,
};
pub use impls::{
    is_rust_declared_type_kind, is_rust_foreign_impl_callable, is_rust_project_named, is_rust_trait_impl_method, rust_trait_impl_header,
};
pub use scope::is_rust_name_in_scope;
pub use types::rust_trait_of;
pub use uses::is_rust_associated_item;
