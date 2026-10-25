//! Name matching — port of `src/resolution/name-matcher.ts`.
//!
//! The TypeScript module is the largest in the engine (language-specific
//! visibility rules for every supported language). This file carries the
//! cross-language gate `ctx` depends on; the per-language matchers and the
//! object-literal bindings land on top of it.

use codegraph_types::Language;

/// Languages that may share a reference. A reference may cross within one
/// family (a Kotlin `Foo.BAR` can name a Java `Foo`); anything not listed forms
/// its own singleton family.
fn language_family(language: &str) -> Option<&'static str> {
    Some(match language {
        "java" | "kotlin" | "scala" => "jvm",
        "swift" | "objc" => "native",
        // ArkTS is a TS superset — every HarmonyOS project mixes `.ets` UI with
        // `.ts` logic modules, so refs must cross freely between them.
        "typescript" | "tsx" | "javascript" | "jsx" | "arkts" => "web",
        "c" | "cpp" => "native",
        // Razor/Blazor markup names C# types — same family so `@model Foo` /
        // `<MyComponent/>` resolve to their `.cs` class.
        "csharp" | "razor" | "vbnet" => "dotnet",
        "svelte" | "vue" | "astro" => "web",
        "cfml" | "cfscript" => "cfml",
        _ => return None,
    })
}

/// Config/markup transitions stay open; every other code language has a family.
fn code_family(language: &str) -> Option<&'static str> {
    language_family(language).or_else(|| {
        Some(match language {
            "python" => "python",
            "go" => "go",
            "rust" => "rust",
            "php" => "php",
            "ruby" => "ruby",
            "dart" => "dart",
            "lua" | "luau" => "lua",
            "r" => "r",
            "erlang" => "erlang",
            "pascal" => "pascal",
            "solidity" => "solidity",
            "nix" => "nix",
            "cobol" => "cobol",
            _ => return None,
        })
    })
}

pub fn same_language_family(a: Language, b: Language) -> bool {
    if a == b {
        return true;
    }
    match language_family(a.as_str()) {
        Some(family) => Some(family) == language_family(b.as_str()),
        None => false,
    }
}

pub fn crosses_code_boundary(a: Language, b: Language) -> bool {
    match (code_family(a.as_str()), code_family(b.as_str())) {
        (Some(fa), Some(fb)) => fa != fb,
        _ => false,
    }
}

/// Most same-named candidates a matcher will consider before declining.
/// Overridable with `CODEGRAPH_AMBIGUOUS_NAME_CEILING`.
pub fn ambiguous_name_ceiling() -> usize {
    std::env::var("CODEGRAPH_AMBIGUOUS_NAME_CEILING")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|n| *n > 0)
        .map(|n| n as usize)
        .unwrap_or(500)
}

mod gate;
mod pending;
pub mod kotlin_call_result;
pub mod python;
pub mod util;
pub mod calls;
pub mod python_scope;
pub mod python_attr;
pub mod js_scope;
pub mod js_binding;
pub mod object_literal;
pub mod reach;
pub mod js_imports;
pub mod js_store;
pub mod lang_shape;
pub mod exact;
pub mod infer;
pub mod members;
pub mod method;
pub mod ts_fields;
pub mod cpp_text;
pub mod cpp_decl;
pub mod cpp_types;
pub mod cpp_recv;
pub mod cpp_locals;
pub mod cpp_scope;
pub mod cpp_method;
pub mod cpp_overload;
pub mod cpp_tu;
pub mod cpp_call;
pub mod lua;
pub mod function_ref;
pub mod reference;
pub mod rust;
pub mod rust_go_shape;
pub mod std_methods;
pub mod visibility;
pub mod call_chain;
pub mod go;
pub mod java;
pub mod php;
pub mod kotlin;
pub mod swift;
pub mod objc;
pub mod scala;
pub mod csharp;
pub mod ruby;
pub mod cfml;
pub mod vb_scope;
pub mod line_scope;
pub mod solidity;
pub mod dart_code;
pub mod dart;
pub mod dart_chain;

pub use gate::gate_language_match;
pub use std_methods::is_std_method_name;
pub use visibility::{is_visible_across_files, is_visible_by_language};
pub use call_chain::{lookup_callee_return_type, match_dotted_call_chain, match_scoped_call_chain};
pub use go::*;
pub use java::is_java_type_visible;
pub use php::{is_php_class_visible, php_receiver_reaches};
pub use swift::swift_decl_supers;
pub use scala::{is_scala_package_object_member_visible, scala_supertypes_of};
pub use csharp::{csharp_using_alias, is_csharp_nested_type_in_scope, is_csharp_type_visible};
pub use objc::{is_objc_file_local_elsewhere, objc_member_receiver_type, objc_receiver_reaches, objc_supertypes_of};
pub use kotlin::{is_kotlin_number_bitwise, is_kotlin_top_level_visible, kotlin_file_scope};
pub use python_attr::{is_python_from_import_module, match_python_attr_call, match_python_factory_chain, python_attr_ref_resolves};
pub use pending::*;
pub use method::{match_method_call, resolve_method_on_type};
pub use function_ref::match_function_ref;
pub use reference::match_reference;
pub use rust::{
    is_rust_associated_item, is_rust_declared_type_kind, is_rust_foreign_impl_callable, is_rust_name_in_scope, is_rust_path_call_reachable, is_rust_project_named,
    is_rust_trait_impl_method, match_rust_as_mut_chain_call, match_rust_generic_path, match_rust_pinned_call, match_rust_self_call,
    match_rust_self_field_call, match_rust_self_path, match_rust_typed_receiver_call, match_rust_typed_self_field_call,
    match_rust_written_receiver_call, rust_chained_call_by_name, rust_field_type_name,
};
pub use exact::{match_by_exact_name, match_by_file_path, match_by_qualified_name, match_fuzzy, prefer_call_site_file};
pub use calls::{is_go_bare_name, is_python_self_call};
pub use js_scope::is_import_shadowed_by_js_binding;
pub use js_store::{is_unresolved_js_member_call, match_js_store_binding_call};
pub use object_literal::{match_object_path_call, resolve_object_literal_binding, this_scope_caller};

#[cfg(test)]
mod tests {
    use super::*;

    fn l(s: &str) -> Language {
        Language::parse(s).unwrap()
    }

    #[test]
    fn a_reference_crosses_within_one_family() {
        assert!(same_language_family(l("kotlin"), l("java")));
        assert!(same_language_family(l("tsx"), l("javascript")));
        assert!(same_language_family(l("razor"), l("csharp")));
        assert!(same_language_family(l("svelte"), l("typescript")));
    }

    #[test]
    fn a_reference_does_not_cross_families() {
        assert!(!same_language_family(l("python"), l("go")));
        assert!(!same_language_family(l("swift"), l("kotlin")));
        assert!(!same_language_family(l("ruby"), l("go")));
    }

    #[test]
    fn config_languages_have_no_family_but_match_themselves() {
        assert!(same_language_family(l("yaml"), l("yaml")));
        assert!(!same_language_family(l("yaml"), l("terraform")));
    }

    #[test]
    fn a_code_boundary_is_crossed_only_between_two_known_families() {
        assert!(crosses_code_boundary(l("python"), l("go")));
        assert!(!crosses_code_boundary(l("python"), l("python")));
        assert!(!crosses_code_boundary(l("yaml"), l("python")));
    }
}
