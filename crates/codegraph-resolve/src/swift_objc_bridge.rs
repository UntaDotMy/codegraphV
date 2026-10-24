//! Swift ↔ Objective-C bridging rules. Port of `src/resolution/swift-objc-bridge.ts`.
//!
//! Apple's auto-bridging mechanism exposes Swift declarations to the ObjC runtime under a
//! deterministic selector name. This module is **pure name math** — given a Swift declaration's
//! base name + parameter external labels (or the raw signature text), produce the bridged ObjC
//! selector(s); given an ObjC selector, produce the candidate Swift base names. No graph/DB
//! access here.
//!
//! ```text
//!   Swift declaration                             ObjC selector
//!   func play()                                    play
//!   func play(_ song: String)                      play:
//!   func play(song: String)                        playWithSong:
//!   func play(_ song: String, by artist: String)   play:by:
//!   func play(song: String, by artist: String)     playWithSong:by:
//!   init(name: String)                             initWithName:
//!   init(name: String, age: Int)                   initWithName:age:
//!   var name: String  (getter / setter)            name  /  setName:
//!   @objc(custom:) func f(_ x: Int)                custom:        (literal override)
//! ```

use codegraph_jsre::re;

/// Capitalize the first character of a string.
fn cap_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Lowercase the first character.
fn lower_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// The auto-bridged ObjC selector for a Swift method declaration. `external_labels` are the
/// parameter EXTERNAL labels in declaration order (`None` for a `_`); `explicit_objc_name` is the
/// `@objc(customSel:)` literal, which short-circuits the rule.
pub fn objc_selector_for_swift_method(base_name: &str, external_labels: &[Option<&str>], explicit_objc_name: Option<&str>) -> Option<String> {
    if base_name.is_empty() {
        return None;
    }
    if let Some(explicit) = explicit_objc_name.filter(|e| !e.is_empty()) {
        return Some(explicit.to_string());
    }

    let Some((first, rest)) = external_labels.split_first() else {
        return Some(base_name.to_string());
    };
    // Single param: "_" → "base:" ; "label" → "baseWithLabel:"
    // Multi-param mirrors the same first-keyword formation, then appends each subsequent label as
    // its own keyword. A `null` later label is invalid ObjC (no way to express unlabeled middle
    // params) — keep as `:` to be safe.
    let first_keyword = match first {
        None | Some("_") | Some("") => format!("{base_name}:"),
        Some(label) => format!("{base_name}With{}:", cap_first(label)),
    };
    let rest_keywords: String = rest.iter().map(|l| format!("{}:", l.unwrap_or(""))).collect();
    Some(first_keyword + &rest_keywords)
}

/// The bridged ObjC selector for a Swift `init(...)` declaration. Apple always uses `initWith`
/// regardless of whether the first label is `_`; for `_` the internal (second identifier) name
/// is used.
pub fn objc_selector_for_swift_init(external_labels: &[Option<&str>], internal_names: &[&str], explicit_objc_name: Option<&str>) -> Option<String> {
    if let Some(explicit) = explicit_objc_name.filter(|e| !e.is_empty()) {
        return Some(explicit.to_string());
    }

    let Some((first_ext, rest_ext)) = external_labels.split_first() else {
        return Some("init".to_string());
    };
    // Use the internal name when external is "_"; ObjC needs *some* keyword, and Swift's
    // auto-bridger uses the parameter's local name in this case.
    let first_label = match first_ext {
        None | Some("_") | Some("") => internal_names.first().copied(),
        Some(ext) => Some(*ext),
    };
    let first_label = first_label.filter(|l| !l.is_empty())?;

    let first_keyword = format!("initWith{}:", cap_first(first_label));
    let rest_keywords: String = rest_ext
        .iter()
        .enumerate()
        .map(|(idx, label)| {
            let internal = internal_names.get(idx + 1).copied();
            let name = match label {
                Some(l) if !l.is_empty() && *l != "_" => *l,
                _ => internal.unwrap_or(""),
            };
            format!("{name}:")
        })
        .collect();
    Some(first_keyword + &rest_keywords)
}

/// The bridged ObjC getter + setter for a Swift `@objc` property.
pub fn objc_accessors_for_swift_property(swift_name: &str, explicit_objc_name: Option<&str>) -> Option<(String, String)> {
    if swift_name.is_empty() {
        return None;
    }
    // The override syntax `@objc(customGetterName)` re-points the GETTER only; the setter still
    // follows the `setX:` rule but is keyed off the override.
    let getter = explicit_objc_name.unwrap_or(swift_name);
    Some((getter.to_string(), format!("set{}:", cap_first(getter))))
}

/// Reverse: from an ObjC selector, the candidate Swift base names the resolver should try when
/// looking for the bridged Swift declaration (`playWithSong:` → `play`, `playWithSong`;
/// `initWithName:` → `initWithName`, `init`; `setName:` → `setName`, `name`).
pub fn swift_base_names_for_objc_selector(selector: &str) -> Vec<String> {
    if selector.is_empty() {
        return Vec::new();
    }

    // Strip trailing colons and split into keywords.
    let stripped = selector.trim_end_matches(':');
    let keywords: Vec<&str> = stripped.split(':').collect();
    let first_keyword = keywords[0];
    if first_keyword.is_empty() {
        return Vec::new();
    }

    let mut candidates: Vec<String> = Vec::new();
    let mut add = |c: &str| {
        if !candidates.iter().any(|x| x == c) {
            candidates.push(c.to_string());
        }
    };

    // Always a candidate: the raw first keyword. Covers `play:` → `play`, `play:by:` → `play`,
    // `playWithSong:` → `playWithSong` (a literal Swift name), `tableView:...:` → `tableView`.
    add(first_keyword);

    // `initWith<X>:` and `initWith<X>:<more>:` always reduce to `init`.
    if first_keyword.starts_with("initWith") {
        add("init");
    }

    // Preposition-prefix patterns: `<base>(With|For|By|In|On|At|From|To|Of|As)<Cap>:` covers both
    // Swift's @objc EXPORT rule (always "With") and Cocoa's IMPORTED selectors which use other
    // prepositions natively (e.g. `objectForKey:`, `stringWithFormat:`, `compareTo:`,
    // `imageNamed:inBundle:`). Strip to recover the Swift base name a caller would use (e.g.
    // `object`, `string`, `compare`, `image`).
    if let Some(m) = re!(r"^([a-z][a-zA-Z0-9]*?)(?:With|For|By|In|On|At|From|To|Of|As)[A-Z]").captures(first_keyword) {
        if let Some(base) = m.text(1).filter(|b| !b.is_empty()) {
            add(base);
        }
    }

    // `setX:` could be a property setter — the Swift property is `x` (lowercase). Only fires for
    // the obvious shape: `set` + capital letter + ':' (one param).
    if keywords.len() == 1 && re!(r"^set[A-Z]").is_match(first_keyword) && selector.ends_with(':') {
        let prop_name = lower_first(&first_keyword[3..]);
        if !prop_name.is_empty() {
            add(&prop_name);
        }
    }

    candidates
}

/// Whether a Swift method uses the `@objc(custom:)` override form, returning the literal
/// selector when present. `None` when the declaration is plain `@objc` or has no `@objc`
/// attribute at all.
pub fn detect_explicit_objc_name(source_slice: &str) -> Option<String> {
    // `@objc(customName:)` or `@objc(custom:name:)` — the parens contents are the literal ObjC
    // selector. Whitespace permitted.
    re!(r"@objc\s*\(\s*([^)\s]+)\s*\)").captures(source_slice).and_then(|m| m.text(1).filter(|s| !s.is_empty()).map(String::from))
}

/// Whether a Swift declaration is `@objc`-exposed by scanning the source slice that precedes
/// it. `@nonobjc` returns false even if `@objc` also appears (per Swift's rule that `@nonobjc`
/// opts out of class-level `@objcMembers`).
pub fn is_objc_exposed(source_slice: &str) -> bool {
    if re!(r"@nonobjc\b").is_match(source_slice) {
        return false;
    }
    re!(r"@objc\b").is_match(source_slice)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_bridge_matches_the_cheat_sheet() {
        assert_eq!(objc_selector_for_swift_method("play", &[], None).as_deref(), Some("play"));
        assert_eq!(objc_selector_for_swift_method("play", &[None], None).as_deref(), Some("play:"));
        assert_eq!(objc_selector_for_swift_method("play", &[Some("song")], None).as_deref(), Some("playWithSong:"));
        assert_eq!(objc_selector_for_swift_method("play", &[None, Some("by")], None).as_deref(), Some("play:by:"));
        assert_eq!(objc_selector_for_swift_method("play", &[Some("song"), Some("by")], None).as_deref(), Some("playWithSong:by:"));
        assert_eq!(objc_selector_for_swift_init(&[Some("name"), Some("age")], &["name", "age"], None).as_deref(), Some("initWithName:age:"));
        assert_eq!(objc_selector_for_swift_init(&[None], &["name"], None).as_deref(), Some("initWithName:"));
        assert_eq!(objc_accessors_for_swift_property("name", None), Some(("name".to_string(), "setName:".to_string())));
    }

    #[test]
    fn reverse_bridge_lists_candidates() {
        assert_eq!(swift_base_names_for_objc_selector("play"), vec!["play"]);
        assert_eq!(swift_base_names_for_objc_selector("playWithSong:"), vec!["playWithSong", "play"]);
        assert_eq!(swift_base_names_for_objc_selector("initWithName:age:"), vec!["initWithName", "init"]);
        assert_eq!(swift_base_names_for_objc_selector("setName:"), vec!["setName", "name"]);
        assert_eq!(swift_base_names_for_objc_selector("objectForKey:"), vec!["objectForKey", "object"]);
        assert!(swift_base_names_for_objc_selector(":").is_empty());
    }

    #[test]
    fn objc_attribute_scan() {
        assert!(is_objc_exposed("  @objc func f()"));
        assert!(!is_objc_exposed("@objc @nonobjc func f()"));
        assert_eq!(detect_explicit_objc_name("@objc(custom:) func f(_ x: Int)").as_deref(), Some("custom:"));
    }
}
