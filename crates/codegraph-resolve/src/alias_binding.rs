//! Alias bindings: a name bound to nothing but another symbol — port of
//! `src/resolution/alias-binding.ts`.
//!
//! ```text
//! export const alias = realImpl;      // p1 — cross-file through an import
//! const local = realImpl;             // p6 — same file
//! export const api = { run: impl };   // p5 — property holding a function ref
//! export { realImpl as alias };       // p2 — a local export clause
//! ```
//!
//! A call through an alias IS a call to the aliased function, so these hops are only ever
//! applied to `calls` refs.

use crate::ctx::Ctx;
use crate::name_matcher::resolve_object_literal_binding;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::{EdgeKind, Language, NodeKind};

/// Kinds that can be a pure alias for another symbol.
fn is_alias_binding_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Constant | NodeKind::Variable | NodeKind::Property)
}

/// Kinds an alias may usefully forward a CALL to.
fn is_callable_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Function | NodeKind::Method | NodeKind::Class | NodeKind::Component)
}

/// The symbol name an alias binding forwards to, or `None` when the initializer is anything
/// else (a call, a literal, an expression — all genuine definitions).
///
/// `member_name` targets a property of an object-literal initializer (`= { run: impl }` for
/// `api.run()`), including ES shorthand (`= { impl }`).
pub fn alias_target_name(signature: Option<&str>, member_name: Option<&str>) -> Option<String> {
    let signature = signature.filter(|s| !s.is_empty())?;
    let initializer = codegraph_types::jscompat::js_trim(signature);

    if let Some(member) = member_name.filter(|m| !m.is_empty()) {
        let key = escape_regex(member);
        let explicit = Re::new(&format!(r"[{{,]\s*{key}\s*:\s*([A-Za-z_$][\w$]*)\s*[,}}]"), "");
        if let Some(c) = explicit.captures(initializer) {
            return c.text(1).map(String::from);
        }
        // `{ impl }` — shorthand binds the property to the same-named symbol.
        let shorthand = Re::new(&format!(r"[{{,]\s*({key})\s*[,}}]"), "");
        if let Some(c) = shorthand.captures(initializer) {
            return c.text(1).map(String::from);
        }
        return None;
    }

    re!(r"^=\s*([A-Za-z_$][\w$]*)\s*(?:as\s+[\w.<>[\]]+\s*)?;?$")
        .captures(initializer)
        .and_then(|c| c.text(1).map(String::from))
}

/// The callable an alias binding forwards to.
///
/// Prefers a declaration in the alias's own file; cross-file is accepted only when the name is
/// unique in the project, so an ambiguous name yields no hop rather than an invented edge.
pub fn resolve_alias_binding(alias_node: &NodeRef, member_name: Option<&str>, ctx: &Ctx) -> Option<NodeRef> {
    if !is_alias_binding_kind(alias_node.kind) {
        return None;
    }

    if let Some(member) = member_name {
        if matches!(
            alias_node.language,
            Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx | Language::ArkTs
        ) {
            let synthetic = UnresolvedRef::new(
                alias_node.id.clone(),
                member,
                EdgeKind::Calls,
                alias_node.start_line,
                alias_node.start_column,
                alias_node.file_path.clone(),
                alias_node.language,
            );
            let resolved = resolve_object_literal_binding(alias_node, member, &synthetic, ctx);
            return resolved.and_then(|r| ctx.node_by_id(&r.target_node_id));
        }
    }

    let target_name = alias_target_name(alias_node.signature.as_deref(), member_name)?;
    if target_name == alias_node.name {
        return None;
    }

    let candidates: Vec<NodeRef> = ctx.get_nodes_by_name(&target_name).iter().filter(|n| is_callable_kind(n.kind)).cloned().collect();
    if candidates.is_empty() {
        return None;
    }

    let same_file: Vec<&NodeRef> = candidates.iter().filter(|n| n.file_path == alias_node.file_path).collect();
    if same_file.len() == 1 {
        return Some(same_file[0].clone());
    }
    if same_file.len() > 1 {
        return None;
    }
    if candidates.len() == 1 {
        Some(candidates[0].clone())
    } else {
        None
    }
}

/// One local export clause binding: `export { realImpl as alias }` exports `alias`, bound to `realImpl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalExportAlias {
    pub exported_name: String,
    pub local_name: String,
}

/// Local export clauses: `export { realImpl as alias }` / `export { realImpl }` with no `from`
/// source, plus the CommonJS forms. Type-only specifiers are skipped: they carry no runtime call.
pub fn extract_local_export_aliases(content: &str) -> Vec<LocalExportAlias> {
    let mut out = Vec::new();
    // `export { … }` NOT followed by `from` — the `from` form is a re-export.
    let clause_re = re!(r"export\s*\{([^}]*)\}\s*(?!\s*from)[;\n]", "g");
    for clause in clause_re.captures_iter(content) {
        for raw in clause.text(1).unwrap_or("").split(',') {
            let specifier = codegraph_types::jscompat::js_trim(raw);
            if specifier.is_empty() || re!(r"^type\s").is_match(specifier) {
                continue;
            }
            if let Some(renamed) = re!(r"^([A-Za-z_$][\w$]*)\s+as\s+([A-Za-z_$][\w$]*)$").captures(specifier) {
                out.push(LocalExportAlias {
                    local_name: renamed.text(1).unwrap().to_string(),
                    exported_name: renamed.text(2).unwrap().to_string(),
                });
                continue;
            }
            if re!(r"^[A-Za-z_$][\w$]*$").is_match(specifier) {
                out.push(LocalExportAlias { local_name: specifier.to_string(), exported_name: specifier.to_string() });
            }
        }
    }
    // CommonJS: `exports.setCharset = function setCharset(…)`, `module.exports.x = impl`, and a
    // named class expression, `exports.Router = class Router {…}` (#2499).
    let member_re = re!(
        r"^[ \t]*(?:module\.)?exports\.([A-Za-z_$][\w$]*)\s*=\s*(?:(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)\s*\(|class\s+(?!extends\b)([A-Za-z_$][\w$]*)|([A-Za-z_$][\w$]*)\s*;?[ \t]*$)",
        "gm"
    );
    for member in member_re.captures_iter(content) {
        let local = member.text(2).or_else(|| member.text(3)).or_else(|| member.text(4)).unwrap_or("");
        out.push(LocalExportAlias { exported_name: member.text(1).unwrap().to_string(), local_name: local.to_string() });
    }
    // `module.exports = { parse, format: formatDate }`.
    let object_re = re!(r"^[ \t]*module\.exports\s*=\s*\{([^{}]*)\}", "gm");
    for object in object_re.captures_iter(content) {
        for raw in object.text(1).unwrap_or("").split(',') {
            if let Some(entry) = re!(r"^\s*([A-Za-z_$][\w$]*)\s*(?::\s*([A-Za-z_$][\w$]*)\s*)?$").captures(raw) {
                let exported = entry.text(1).unwrap().to_string();
                let local = entry.text(2).map(String::from).unwrap_or_else(|| exported.clone());
                out.push(LocalExportAlias { exported_name: exported, local_name: local });
            }
        }
    }
    out
}

/// The binding a module's default export is as a whole: `export default Page;`,
/// `export { Page as default }`. `None` for a declaration or an expression.
pub fn default_export_binding(content: &str) -> Option<String> {
    re!(r"(?:^|;)\s*export\s+default\s+([A-Za-z_$][\w$]*)\s*(?:;|$)", "m")
        .captures(content)
        .and_then(|c| c.text(1).map(String::from))
        .or_else(|| extract_local_export_aliases(content).into_iter().find(|a| a.exported_name == "default").map(|a| a.local_name))
}
