//! What a name imported into a JS/TS file is bound to: namespace and out-of-repo imports,
//! re-exports, and destructured dynamic imports (`const { a } = await import('./x')`).

use super::js_scope::{blanked_code_units, js_block_end, js_body_end};
use super::object_literal::ByteUnits;
use super::util::*;
use crate::ctx::Ctx;
use crate::types::{ReExport, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::{js_trim, utf16_len};
use codegraph_types::{EdgeKind, Node, NodeKind, ReferenceKind};
use std::rc::Rc;

/// `receiver` begins at an import of a whole module (namespace or out-of-repo package).
pub fn is_imported_module_receiver(receiver: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let root = receiver.split('.').next().unwrap_or("");
    let mappings = ctx.get_import_mappings(&r.file_path, r.language);
    let Some(binding) = mappings.iter().find(|m| m.local_name == root) else { return false };
    binding.is_namespace || ctx.is_out_of_repo_import(&binding.source, &r.file_path, r.language)
}

pub fn is_vue_component_method(n: &Node) -> bool {
    if n.kind != NodeKind::Method || !n.file_path.ends_with(".vue") {
        return false;
    }
    let base = n.file_path.rsplit('/').next().unwrap_or("");
    let base = base.strip_suffix(".vue").unwrap_or(base);
    n.qualified_name == format!("{base}::{}", n.name)
}

/// `this.name(` appears on the reference's line, in the candidate's own file.
pub fn is_this_call_in_own_file(n: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if n.file_path != r.file_path {
        return false;
    }
    let line = line_at(ctx, &r.file_path, r.line).unwrap_or_default();
    Re::cached(&format!(r"\bthis\s*\??\.\s*{}\s*\(", escape_regex(&n.name)), "").is_match(&line)
}

/// The root of `receiver` is one of the file's import bindings.
pub fn is_import_binding(receiver: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let root = receiver.split('.').next().unwrap_or("");
    ctx.get_import_mappings(&r.file_path, r.language).iter().any(|m| m.local_name == root)
}

/// The module `name` is re-exported from, when exactly one `export { name } from` names it.
pub fn re_export_source(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let mut source: Option<String> = None;
    for rex in ctx.get_re_exports(&r.file_path, r.language).iter() {
        let ReExport::Named { original_name, source: s, .. } = rex else { continue };
        if original_name != name {
            continue;
        }
        if source.as_deref().is_some_and(|prev| prev != s) {
            return None;
        }
        source = Some(s.clone());
    }
    source
}

/// `name` is bound to something outside the repo (an npm package, a Node builtin).
pub fn is_out_of_repo_binding(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let mappings = ctx.get_import_mappings(&r.file_path, r.language);
    let binding = mappings.iter().find(|m| m.local_name == name);
    let source = dynamic_import_source(name, r, ctx)
        .or_else(|| binding.map(|b| b.source.clone()))
        .or_else(|| if r.reference_kind == ReferenceKind::Edge(EdgeKind::Imports) { re_export_source(name, r, ctx) } else { None });
    source.is_some_and(|s| ctx.is_out_of_repo_import(&s, &r.file_path, r.language))
}

// -- Dynamic imports -----------------------------------------------------------------------------

struct DynamicBinding {
    names: Vec<String>,
    source: String,
    at: usize,
    scope_end: usize,
}

#[derive(Default)]
pub struct DynamicImportScan {
    names: std::collections::HashSet<String>,
    line_starts: Vec<usize>,
    bindings: Vec<DynamicBinding>,
}

type DynamicScans = Bounded<Rc<DynamicImportScan>, 64>;

/// `{ a, b: c, d = 1, ...rest }` → the local names bound (`a`, `c`, `d`).
fn destructured_locals(list: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in list.split(',') {
        let binding = js_trim(&re!(r"=[\s\S]*$").replace_first(part, |_| String::new())).to_string();
        if binding.starts_with("...") {
            continue;
        }
        let local = match binding.find(':') {
            Some(colon) => js_trim(&binding[colon + 1..]).to_string(),
            None => binding.clone(),
        };
        if is_js_ident(&local) {
            out.push(local);
        }
    }
    out
}

fn dynamic_import_scan(file_path: &str, ctx: &Ctx) -> Rc<DynamicImportScan> {
    ctx.memo::<DynamicScans>().get_or(file_path, false, || {
        let source = ctx.read_file(file_path).map(|s| s.to_string()).unwrap_or_default();
        let dynamic = re!(r"\bimport\s*\(").is_match(&source);
        if !dynamic {
            return Rc::new(DynamicImportScan::default());
        }
        let declaration = re!(r#"\b(?:const|let|var)\s*\{([^{}]*)\}\s*=\s*await\s+import\s*\(\s*(['"])([^'"\n]+)\2\s*\)"#, "g");
        let callback = re!(
            r#"\bimport\s*\(\s*(['"])([^'"\n]+)\1\s*\)\s*\.\s*then\s*\(\s*(?:async\s+)?(?:function\s*[\w$]*\s*)?\(\s*\{([^{}]*)\}\s*\)\s*(=>|\{)"#,
            "g"
        );
        let declarations: Vec<_> = declaration.captures_iter(&source).collect();
        let callbacks: Vec<_> = callback.captures_iter(&source).collect();
        if declarations.is_empty() && callbacks.is_empty() {
            return Rc::new(DynamicImportScan::default());
        }
        let code = blanked_code_units(&source, Some("javascript"));
        let source_units: Vec<u16> = source.encode_utf16().collect();
        let units = ByteUnits::new(&source);
        let mut bindings: Vec<DynamicBinding> = Vec::new();
        for m in &declarations {
            let g = m.get(0).unwrap();
            let at = units.at(g.start());
            if code.get(at) != source_units.get(at) {
                continue;
            }
            bindings.push(DynamicBinding {
                names: destructured_locals(m.text(1).unwrap_or("")),
                source: m.text(3).unwrap_or("").to_string(),
                at,
                scope_end: js_block_end(&code, at),
            });
        }
        for m in &callbacks {
            let g = m.get(0).unwrap();
            let at = units.at(g.start());
            if code.get(at) != source_units.get(at) {
                continue;
            }
            let body = at + utf16_len(g.as_str());
            let scope_end = if m.text(4) == Some("{") { js_block_end(&code, body) } else { js_body_end(&code, body) };
            bindings.push(DynamicBinding {
                names: destructured_locals(m.text(3).unwrap_or("")),
                source: m.text(2).unwrap_or("").to_string(),
                at,
                scope_end,
            });
        }
        if bindings.is_empty() {
            return Rc::new(DynamicImportScan::default());
        }
        let mut line_starts = vec![0usize];
        for (i, &u) in source_units.iter().enumerate() {
            if u == 10 {
                line_starts.push(i + 1);
            }
        }
        let names = bindings.iter().flat_map(|b| b.names.iter().cloned()).collect();
        Rc::new(DynamicImportScan { names, line_starts, bindings })
    })
}

/// The module a destructured dynamic import binds `name` from, where `r` sits inside its scope.
pub fn dynamic_import_source(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    if !is_js_family(r.language) {
        return None;
    }
    let scan = dynamic_import_scan(&r.file_path, ctx);
    if !scan.names.contains(name) {
        return None;
    }
    let at = scan.line_starts.get((r.line as usize).wrapping_sub(1)).map_or(usize::MAX, |&s| s.saturating_add(r.column as usize));
    let mut found: Option<&DynamicBinding> = None;
    for b in &scan.bindings {
        if b.at < at && at <= b.scope_end && b.names.iter().any(|n| n == name) && found.map_or(true, |f| b.at > f.at) {
            found = Some(b);
        }
    }
    found.map(|b| b.source.clone())
}

/// The reference's name is bound by an import to something other than `candidate` (a namespace, an asset, a symbol in another file).
pub fn import_binds_other(candidate: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if !is_js_ident(&r.reference_name) {
        return false;
    }
    let mappings = ctx.get_import_mappings(&r.file_path, r.language);
    let Some(binding) = mappings.iter().find(|m| m.local_name == r.reference_name) else { return false };
    let asset = re!(
        r"\.(?:css|scss|sass|less|styl|stylus|pcss|graphql|gql|png|jpe?g|gif|svg|webp|avif|ico|bmp|tiff?|json5?|ya?ml|toml|md|mdx|txt|html?|xml|csv|woff2?|ttf|otf|eot|mp3|mp4|webm|ogg|wav|flac|pdf|wasm)$",
        "i"
    );
    if binding.is_namespace || asset.is_match(&re!(r"[?#].*$").replace_first(&binding.source, |_| String::new())) {
        return true;
    }
    let bound: Option<ResolvedRef> = crate::import_resolver::resolve_import(r, ctx);
    let node = bound.and_then(|b| ctx.node_by_id(&b.target_node_id));
    let Some(node) = node else { return false };
    if matches!(node.kind, NodeKind::Function | NodeKind::Method) || node.file_path == candidate.file_path {
        return false;
    }
    let lines = lines_of(ctx, &node.file_path);
    let from = node.start_line as usize - 1;
    let to = (node.end_line as usize).min(node.start_line as usize + 15);
    let body = lines.get(from..to.max(from).min(lines.len())).map(|s| s.join("\n")).unwrap_or_default();
    !re!(r"\bimport\s*\(").is_match(&body)
}
