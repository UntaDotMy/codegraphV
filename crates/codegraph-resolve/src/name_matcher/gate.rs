//! The cross-language gate — `hasBridgeEvidence` and `gateLanguageMatch` of
//! `src/resolution/name-matcher.ts`.

use super::{code_family, crosses_code_boundary};
use crate::ctx::Ctx;
use crate::types::{ResolvedRef, UnresolvedRef};
use codegraph_extract::strip_comments::strip_comments_for_regex;
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};

/// Is there evidence the two languages are joined by a native bridge (cgo, `extern "C"`,
/// an Expo module), so a cross-boundary match is real?
fn has_bridge_evidence(candidate: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if !r.is_kind(EdgeKind::Calls) {
        return false;
    }
    // An Expo module's native `Function("name")` is the JS-callable surface of a `requireNativeModule`.
    if code_family(r.language.as_str()) == Some("web")
        && candidate.id.starts_with("expo-module:")
        && candidate.is_exported == Some(true)
        && matches!(candidate.language, Language::Swift | Language::Kotlin)
    {
        return true;
    }
    if candidate.kind != NodeKind::Function {
        return false;
    }
    let name = escape_regex(&candidate.name);
    if code_family(r.language.as_str()) == Some("native") {
        let Some(source) = ctx.read_file(&candidate.file_path) else { return false };
        if candidate.language == Language::Go {
            let lines: Vec<&str> = source.split('\n').collect();
            let from = (candidate.start_line as i64 - 2).max(0) as usize;
            let to = (candidate.end_line as usize).min(lines.len());
            let declaration = if from < to { lines[from..to].join("\n") } else { String::new() };
            return re!(r#"\bimport\s+(?:\(\s*)?"C""#).is_match(&strip_comments_for_regex(&source, "go"))
                && Re::cached(&format!(r"^//export {name}\r?\nfunc {name}\s*\("), "m").is_match(&declaration);
        }
        if candidate.language == Language::Rust {
            let lines: Vec<&str> = source.split('\n').collect();
            let from = (candidate.start_line as usize).saturating_sub(1).min(lines.len());
            let to = (candidate.end_line as usize).min(lines.len());
            let declaration = if from < to { lines[from..to].join("\n") } else { String::new() };
            return Re::cached(&format!(r#"\bpub\s+extern\s+"C"\s+fn\s+{name}\b"#), "")
                .is_match(&strip_comments_for_regex(&declaration, "rust"));
        }
    }
    if matches!(candidate.language, Language::C | Language::Cpp) {
        let Some(source) = ctx.read_file(&r.file_path) else { return false };
        if r.language == Language::Go {
            return re!(r#"\bimport\s+(?:\(\s*)?"C""#).is_match(&strip_comments_for_regex(&source, "go"))
                && r.reference_name == format!("C.{}", candidate.name);
        }
        if r.language == Language::Rust {
            return Re::cached(&format!(r#"extern\s+"C"\s*\{{[^}}]*\bfn\s+{name}\s*\("#), "")
                .is_match(&strip_comments_for_regex(&source, "rust"));
        }
    }
    false
}

/// Drop a match that crosses a code-language boundary without bridge evidence.
pub fn gate_language_match(result: Option<ResolvedRef>, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let result = result?;
    if code_family(r.language.as_str()).is_none() {
        return Some(result);
    }
    let Some(target) = ctx.node_by_id(&result.target_node_id) else {
        // The context could not name the target node (TS falls back to a name search, which
        // cannot find a node `getNodeById` cannot).
        return Some(result);
    };
    if !crosses_code_boundary(r.language, target.language) {
        return Some(result);
    }
    if has_bridge_evidence(&target, r, ctx) {
        Some(result)
    } else {
        None
    }
}
