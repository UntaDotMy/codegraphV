//! Name matching — port of `src/resolution/name-matcher.ts`.
//!
//! The TypeScript module is the largest in the engine (language-specific
//! visibility rules for every supported language). This file carries the
//! cross-language gate `ctx` depends on; the per-language matchers and the
//! object-literal bindings land on top of it.

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

pub fn same_language_family(a: &str, b: &str) -> bool {
    match language_family(a) {
        Some(family) => Some(family) == language_family(b),
        None => false,
    }
}

pub fn crosses_code_boundary(a: &str, b: &str) -> bool {
    match (code_family(a), code_family(b)) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reference_crosses_within_one_family() {
        assert!(same_language_family("kotlin", "java"));
        assert!(same_language_family("tsx", "javascript"));
        assert!(same_language_family("razor", "csharp"));
        assert!(same_language_family("svelte", "typescript"));
    }

    #[test]
    fn a_reference_does_not_cross_families() {
        assert!(!same_language_family("python", "go"));
        assert!(!same_language_family("swift", "kotlin"));
        assert!(!same_language_family("ruby", "ruby "));
    }

    #[test]
    fn config_languages_have_no_family() {
        assert!(!same_language_family("yaml", "yaml"));
        assert!(!same_language_family("terraform", "terraform"));
    }

    #[test]
    fn a_code_boundary_is_crossed_only_between_two_known_families() {
        assert!(crosses_code_boundary("python", "go"));
        assert!(!crosses_code_boundary("python", "python"));
        assert!(!crosses_code_boundary("yaml", "python"));
    }
}
