//! Whether a candidate can be NAMED from a reference in another file at all, given what its
//! language says about the definition's visibility (`isVisibleAcrossFiles`).
//!
//! A definition the language makes file-local is not a candidate for a cross-file name match,
//! however well the names agree. Applied by the resolver to the target the whole name-matching
//! pipeline settled on, so a rejection ends the reference unresolved: declining inside
//! `matchByExactName` instead let the ref fall through to `matchFuzzy`, which then committed to
//! a same-language namesake the ranking had passed over. `matchFuzzy` checks its own survivor
//! as well, since nothing runs after it.

use super::lua::lua_alias_target;
use super::go::*;
use super::java::*;
use super::php::*;
use super::pending::*;
use super::reach::{is_cross_file_reachable, is_lua_local};
use super::rust::is_rust_visible_across_files;
use super::objc::is_objc_file_local_elsewhere;
use super::csharp::{is_csharp_nested_type_in_scope, is_csharp_type_visible};
use super::scala::is_scala_package_object_member_visible;
use super::util::*;
use crate::cpp_includers::cpp_includers;
use crate::ctx::Ctx;
use crate::dart_libraries::in_same_dart_library;
use crate::types::UnresolvedRef;
use codegraph_jsre::re;
use codegraph_search::generated::is_minified_content;
use codegraph_search::query_utils::is_test_path;
use codegraph_types::{Language, Node, NodeKind};
use std::rc::Rc;

memo_map!(MinifiedMemo, String, bool);
memo_map!(SfcRangesMemo, String, Rc<Vec<SfcScript>>);

/// A test suite — not just a file that is a test — is not linked into the program: typeorm's
/// `Record<K, V>` is not a test entity `Record`, tokio's `Output` not a `runtime/tests` type.
pub fn is_test_suite_path(file_path: &str) -> bool {
    if !is_test_path(file_path) {
        return false;
    }
    let lower = crate::ctx::js_lower(file_path);
    let name = &lower[lower.rfind('/').map_or(0, |i| i + 1)..];
    let original = &file_path[file_path.rfind('/').map_or(0, |i| i + 1)..];
    if name.starts_with("test_")
        || re!(r"[._-](?:test|tests|unittest|unittests)\.[a-z0-9]+$|[._](?:spec|specs)\.[a-z0-9]+$").is_match(name)
        || re!(r"(?:Test|Tests|TestCase)\.(?:java|kt|kts|swift|cs|scala|groovy|m|mm|vb|fs)$").is_match(original)
        || name == "conftest.py"
    {
        return true;
    }
    re!(r"(?:^|\/)(?:tests?|__tests__|specs?|e2e|unittests|[\w.]+[-_]unittests?)\/").is_match(&lower)
        || re!(r"(?:^|\/)[A-Za-z0-9]*(?:Test|Tests|Spec)\/").is_match(file_path)
}

/// A vendored minified bundle's names are mangled: healthchecks' 369 `$(…)` (jQuery, a global)
/// went to a one-letter helper inside bootstrap-native.min.js.
pub fn is_minified_script(file_path: &str, ctx: &Ctx) -> bool {
    if !re!(r"\.(?:m?js|cjs)$", "i").is_match(file_path) {
        return false;
    }
    memoized::<MinifiedMemo>(ctx, file_path.to_string(), || {
        re!(r"[.-]min\.m?js$", "i").is_match(file_path) || is_minified_content(file_path, ctx.read_file(file_path).as_deref().unwrap_or(""))
    })
}

/// One `<script>` block of a single-file component: its 1-based line range, and whether the
/// component hands its bindings out.
pub struct SfcScript {
    start: u32,
    end: u32,
    exported: bool,
}

/// A Svelte component's instance script, or a Vue SFC's `<script setup>`, is private to the
/// component: shadcn-svelte's 838 `<Item.Root>` (a namespace import) went to a `type Item` one
/// example component declares for itself.
pub fn is_sfc_private(n: &Node, ctx: &Ctx) -> bool {
    let svelte = n.file_path.ends_with(".svelte");
    if (!svelte && !n.file_path.ends_with(".vue")) || matches!(n.kind, NodeKind::Component | NodeKind::File) {
        return false;
    }
    let ranges = memoized::<SfcRangesMemo>(ctx, n.file_path.clone(), || {
        let lines = lines_of(ctx, &n.file_path);
        let mut found = Vec::new();
        let mut open: Option<(u32, bool)> = None;
        for (i, text) in lines.iter().enumerate() {
            if open.is_none() {
                if let Some(tag) = re!(r"<script\b([^>]*)>", "i").captures(text) {
                    let attrs = tag.text(1).unwrap_or("");
                    let exported = if svelte {
                        re!(r#"\bmodule\b|context\s*=\s*["']module["']"#).is_match(attrs)
                    } else {
                        !re!(r"\bsetup\b").is_match(attrs)
                    };
                    open = Some((i as u32 + 1, exported));
                }
            }
            if let Some((start, exported)) = open {
                if re!(r"<\/script\s*>", "i").is_match(text) {
                    found.push(SfcScript { start, end: i as u32 + 1, exported });
                    open = None;
                }
            }
        }
        Rc::new(found)
    });
    let block = ranges.iter().find(|r| n.start_line >= r.start && n.start_line <= r.end);
    if block.is_some_and(|b| b.exported) {
        return false;
    }
    // Vue hoists the types `<script setup>` exports: mealie imports CrudTable.vue's `TableConfig`.
    if !svelte && block.is_some() {
        let line = line_at(ctx, &n.file_path, n.start_line).unwrap_or_default();
        if re!(r"^\s*export\s+(?:declare\s+)?(?:interface|type|enum)\b").is_match(&line) {
            return false;
        }
    }
    true
}

/// Languages whose `private` is class- or file-local: an Android `editor.apply()` resolved onto an
/// unrelated class's `private fun apply`.
fn private_is_file_local(language: Language) -> bool {
    matches!(
        language,
        Language::Kotlin | Language::Java | Language::CSharp | Language::Swift | Language::Scala | Language::Dart | Language::Php
    )
}

/// Candidate-language visibility of `n` from where `r` is written — the PHP class, Java type, Go
/// name, Dart extension, Scala package-object member and C# type rules of `matchByExactName`'s filter.
pub fn is_visible_by_language(n: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if !is_php_class_visible(n, r, ctx) || !is_java_type_visible(n, r, ctx) {
        return false;
    }
    if n.language == Language::Go && !is_go_name_visible(n, r, ctx) {
        return false;
    }
    if dart_extension_named(n, ctx) == Some(false) {
        return false;
    }
    if r.language == Language::Scala && n.language == Language::Scala && n.file_path != r.file_path && !is_scala_package_object_member_visible(n, r, ctx) {
        return false;
    }
    !(r.language == Language::CSharp
        && n.language == Language::CSharp
        && matches!(n.kind, NodeKind::Class | NodeKind::Interface | NodeKind::Enum | NodeKind::Struct)
        && re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name)
        && (!is_csharp_nested_type_in_scope(n, r, ctx) || (n.file_path != r.file_path && !is_csharp_type_visible(n, r, ctx))))
}

pub fn is_visible_across_files(candidate: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    // Go's `context.Context`, `http.Handler`: written through a package from outside the module,
    // so nothing in it — not even the same file's method `Stream.Context`. Through one of the
    // project's packages, only that package's symbol.
    if r.language == Language::Go
        && candidate.language == Language::Go
        && (is_go_external_qualified(r, ctx) || !is_in_go_qualifier_package(candidate, r, ctx))
    {
        return false;
    }
    if candidate.file_path == r.file_path {
        return true;
    }
    if is_minified_script(&candidate.file_path, ctx) {
        return false;
    }
    // A test suite is not linked into the program. A C or C++ file the reference's translation
    // unit includes is, whatever its name.
    if is_test_suite_path(&candidate.file_path) && !is_test_path(&r.file_path) && !is_included_cpp_test_suite(candidate, r, ctx) {
        return false;
    }
    if is_sfc_private(candidate, ctx) {
        return false;
    }
    // A bare PHP class name is its namespace's class, or the one a `use` names. A bare Java type
    // name is its package's, an import's, or a nested type in reach.
    if !is_php_class_visible(candidate, r, ctx) || !is_java_type_visible(candidate, r, ctx) {
        return false;
    }
    // A Dart `extension on Token { … }` has no name: `Token` is analyzer's type, not bloc_lint's
    // extension block. And it applies only in its own library.
    if dart_extension_named(candidate, ctx) == Some(false) {
        return false;
    }
    if is_dart_unnamed_extension_member(candidate, ctx) && !in_same_dart_library(&r.file_path, &candidate.file_path, ctx) {
        return false;
    }
    // A Scala package object's member is in scope in its package and those under it, or through
    // an import.
    if candidate.language == Language::Scala && r.language == Language::Scala && !is_scala_package_object_member_visible(candidate, r, ctx) {
        return false;
    }
    if candidate.language == Language::CSharp
        && r.language == Language::CSharp
        && matches!(candidate.kind, NodeKind::Class | NodeKind::Interface | NodeKind::Enum | NodeKind::Struct)
        && re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name)
        && (!is_csharp_type_visible(candidate, r, ctx) || !is_csharp_nested_type_in_scope(candidate, r, ctx))
    {
        return false;
    }
    let lang = candidate.language;
    if is_c_family(lang) {
        // A source file's `static` function, only for that file and one that `#include`s it.
        if candidate.kind == NodeKind::Function
            && re!(r"\.(c|cc|cpp|cxx|c\+\+|m|mm)$", "i").is_match(&candidate.file_path)
            && is_static_c_function(candidate, ctx)
            && !cpp_includers(&candidate.file_path, ctx).contains(&r.file_path)
        {
            return false;
        }
        return !is_cpp_translation_unit_local(candidate, r, ctx);
    }
    // An Objective-C source file is a translation unit too: a C type or global it defines is its
    // own. (A class is the runtime's, wherever it is written.)
    if is_objc_file_local_elsewhere(candidate, r) {
        return false;
    }
    if lang == Language::Go {
        return is_go_name_visible(candidate, r, ctx);
    }
    if lang == Language::Rust {
        return is_rust_visible_across_files(candidate, r, ctx);
    }
    if private_is_file_local(lang) {
        return candidate.visibility != Some(codegraph_types::Visibility::Private);
    }
    // An R test file runs in an environment of its own (testthat); the `helper-*.R` /
    // `setup-*.R` files are sourced for every test, so theirs are shared.
    if lang == Language::R
        && matches!(candidate.kind, NodeKind::Variable | NodeKind::Constant)
        && re!(r"(?:^|\/)tests?\/").is_match(&candidate.file_path)
        && !re!(r"(?:^|\/)(?:helper|setup)[^/]*\.[rR]$").is_match(&candidate.file_path)
    {
        return false;
    }
    // A Lua `local` belongs to its chunk. Its module can still hand it out — `return { check =
    // check_phase }` — to a file that names it through a `require` alias.
    if is_lua(lang) && is_lua_local(candidate, ctx) {
        return is_calls(r)
            && re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name)
            && lua_alias_target(r, ctx).as_value().is_some_and(|t| t.target_node_id == candidate.id);
    }
    // JS/TS/ArkTS sealed modules + markdown/JSON call-target guards (#1719). Same predicate
    // matchByExactName / matchFuzzy apply to their survivors so a rejection here cannot fall
    // through to a promoted runner-up.
    is_cross_file_reachable(candidate, r, ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_suite_is_told_from_a_file_that_merely_has_test_in_its_path() {
        assert!(is_test_suite_path("src/foo/__tests__/bar.ts"));
        assert!(is_test_suite_path("pkg/test_helpers.py"));
        assert!(is_test_suite_path("app/FooTests.swift"));
        assert!(!is_test_suite_path("pkg/conftest.py"));
        assert!(!is_test_suite_path("src/foo/bar.ts"));
    }
}
