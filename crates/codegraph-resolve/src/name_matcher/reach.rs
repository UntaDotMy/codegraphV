//! Lexical reachability: may a reference name this candidate at all? Local declarations, sealed ES
//! modules, unexported module bindings and TypeScript global scope — the reach region of
//! `src/resolution/name-matcher.ts`.

use super::object_literal::{host_global_prefix, is_object_literal_owner, is_object_member_reachable_by_name, is_path_holder};
use super::util::*;
use crate::ambient_modules::{in_block, is_declaration_file, scan_global_scope, GlobalScopeScan};
use crate::ctx::Ctx;
use crate::types::{is_inheritance_ref, UnresolvedRef};
use codegraph_extract::strip_comments::{blank_string_contents, strip_comments_for_regex};
use codegraph_jsre::re;
use codegraph_search::query_utils::is_test_file;
use codegraph_types::{EdgeKind, Language, Node, NodeKind, ReferenceKind};
use std::rc::Rc;

memo_map!(LocalDeclMemo, String, Option<(u32, u32)>);
memo_map!(LexicalScopeMemo, String, Option<(u32, u32)>);
memo_map!(SealedMemo, String, bool);
memo_map!(GlobalScopeMemo, String, Option<Rc<GlobalScopeScan>>);
memo_map!(ExportListMemo, String, Rc<ExportList>);
memo_map!(LuaLocalMemo, String, bool);

pub struct ExportList {
    module: bool,
    names: std::collections::HashSet<String>,
}

fn no_nested_functions(l: Language) -> bool {
    matches!(l, Language::C | Language::Cpp)
}

fn is_local_type_kind(k: NodeKind) -> bool {
    matches!(k, NodeKind::Class | NodeKind::Struct | NodeKind::Enum | NodeKind::Interface | NodeKind::Trait | NodeKind::TypeAlias)
}

pub fn has_import_statement(code: &str) -> bool {
    re!(r#"^[ \t]*import[\s{*'"]"#, "m").is_match(code)
}

pub fn has_esm_export(code: &str) -> bool {
    re!(r"^[ \t]*export[\s{*]|^[ \t]*declare\s+global\b", "m").is_match(code)
}

pub fn has_cjs_export(source: &str) -> bool {
    re!(r"\bmodule\.exports\b|\bexports\s*[.\[]").is_match(source)
}

fn is_plain_name(s: &str) -> bool {
    is_js_ident(s)
}

/// Can `r` see `candidate` by its bare name? Locals are visible only inside their function.
pub fn is_lexically_reachable(candidate: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if candidate.kind == NodeKind::Function && is_plain_name(&r.reference_name) && !is_object_member_reachable_by_name(candidate, r, ctx) {
        return false;
    }
    if is_path_holder(candidate)
        && is_object_literal_owner(candidate)
        && is_plain_name(&r.reference_name)
        && !host_global_prefix(last_qualified_segment(&candidate.qualified_name))
    {
        return false;
    }
    if matches!(candidate.kind, NodeKind::Variable | NodeKind::Constant) || (candidate.kind == NodeKind::Field && candidate.language == Language::Scala) {
        let scope = local_declaration_scope(candidate, ctx);
        return scope.map_or(true, |(s, e)| r.file_path == candidate.file_path && r.line >= s && r.line <= e);
    }
    if !matches!(candidate.kind, NodeKind::Function | NodeKind::Method) && !is_local_type_kind(candidate.kind) {
        return true;
    }
    if no_nested_functions(candidate.language) {
        return true;
    }
    let scope = lexical_scope_of(candidate, ctx);
    scope.map_or(true, |(s, e)| r.file_path == candidate.file_path && r.line >= s && r.line <= e)
}

/// The smallest enclosing multi-line function's line range, when the variable is a local.
fn local_declaration_scope(candidate: &Node, ctx: &Ctx) -> Option<(u32, u32)> {
    if no_nested_functions(candidate.language) {
        return None;
    }
    memoized::<LocalDeclMemo>(ctx, candidate.id.clone(), || {
        let mut scope: Option<(u32, u32)> = None;
        if !(is_lua(candidate.language) && !is_lua_local(candidate, ctx)) {
            for n in ctx.get_nodes_in_file(&candidate.file_path).iter() {
                if !matches!(n.kind, NodeKind::Function | NodeKind::Method) || n.id == candidate.id {
                    continue;
                }
                if n.start_line > candidate.start_line || n.end_line < candidate.start_line || n.start_line == n.end_line {
                    continue;
                }
                if n.start_line == candidate.start_line && n.start_column >= candidate.start_column {
                    continue;
                }
                if scope.map_or(true, |(s, e)| n.end_line - n.start_line < e - s) {
                    scope = Some((n.start_line, n.end_line));
                }
            }
            if scope.is_none() && candidate.kind == NodeKind::Field && candidate.language == Language::Scala {
                scope = scala_block_of(candidate, ctx);
            }
        }
        scope
    })
}

/// A Scala `val` declared inside a nested `{…}` of its class body is local to that block.
fn scala_block_of(candidate: &Node, ctx: &Ctx) -> Option<(u32, u32)> {
    let owner = match candidate.qualified_name.rfind("::") {
        Some(i) => &candidate.qualified_name[..i],
        None => "",
    };
    let cls = ctx
        .get_nodes_in_file(&candidate.file_path)
        .iter()
        .find(|n| n.qualified_name == owner && matches!(n.kind, NodeKind::Class | NodeKind::Trait | NodeKind::Struct | NodeKind::Module))
        .cloned()?;
    if cls.start_line >= candidate.start_line {
        return None;
    }
    let lines = lines_of(ctx, &candidate.file_path);
    let clean = |l: &str| -> String {
        let s = re!(r#""(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)'"#, "g").replace_all(l, |_| Some("\"\"".to_string()));
        re!(r"//.*$").replace_first(&s, |_| String::new())
    };
    let mut opens: Vec<u32> = Vec::new();
    let mut depth: i64 = 0;
    for line in cls.start_line..candidate.start_line {
        for ch in clean(lines.get(line as usize - 1).map_or("", |s| s.as_str())).chars() {
            if ch == '{' {
                depth += 1;
                opens.push(line);
            } else if ch == '}' {
                depth = (depth - 1).max(0);
                opens.pop();
            }
        }
    }
    if depth <= 1 {
        return None;
    }
    let start = *opens.last()?;
    let mut d = depth;
    for line in candidate.start_line..=cls.end_line {
        for ch in clean(lines.get(line as usize - 1).map_or("", |s| s.as_str())).chars() {
            if ch == '{' {
                d += 1;
            } else if ch == '}' {
                d -= 1;
                if d < depth {
                    return Some((start, line));
                }
            }
        }
    }
    Some((start, cls.end_line))
}

/// The line range of the function a nested function/type is declared inside.
fn lexical_scope_of(candidate: &Node, ctx: &Ctx) -> Option<(u32, u32)> {
    memoized::<LexicalScopeMemo>(ctx, candidate.id.clone(), || {
        let own = candidate.qualified_name.as_str();
        let parent_qn = own.rfind("::").map_or("", |i| &own[..i]);
        let mut qn = own;
        while let Some(i) = qn.rfind("::") {
            qn = &qn[..i];
            let at = ctx.get_nodes_by_qualified_name(qn);
            let container = at.iter().find(|p| {
                p.file_path == candidate.file_path
                    && matches!(p.kind, NodeKind::Function | NodeKind::Method)
                    && p.start_line <= candidate.start_line
                    && p.end_line >= candidate.end_line
            });
            let Some(container) = container else { continue };
            if candidate.kind == NodeKind::Method && qn == parent_qn {
                break;
            }
            return Some((container.start_line, container.end_line));
        }
        None
    })
}

/// An ES module that imports but never exports: nothing in it is visible to other files.
pub fn is_sealed_module(file_path: &str, ctx: &Ctx) -> bool {
    memoized::<SealedMemo>(ctx, file_path.to_string(), || {
        let exports_node = ctx.file_has_exported_node(file_path);
        let source = if exports_node { None } else { ctx.read_file(file_path) };
        match source {
            Some(source) if source.contains("import") && !has_cjs_export(&source) => {
                let code = blank_string_contents(&strip_comments_for_regex(&source, "typescript"), None);
                has_import_statement(&code) && !has_esm_export(&code)
            }
            _ => false,
        }
    })
}

pub fn is_ts_js_file(path: &str) -> bool {
    re!(r"\.[cm]?[jt]sx?$", "i").is_match(path)
}

/// Declarations visible to every file (scripts, `declare global`), per file.
pub fn ts_global_scope(file: &str, ctx: &Ctx) -> Option<Rc<GlobalScopeScan>> {
    memoized::<GlobalScopeMemo>(ctx, file.to_string(), || {
        let source = ctx.read_file(file)?;
        let scope = scan_global_scope(&source);
        if scope.script && has_cjs_export(&source) {
            return Some(Rc::new(GlobalScopeScan {
                script: false,
                global_words: Default::default(),
                module_blocks: scope.module_blocks,
                import_aliases: scope.import_aliases,
            }));
        }
        Some(Rc::new(scope))
    })
}

/// A type declared in another TS/JS file is visible only when that file is a global script or declares it under `declare global`.
pub fn is_ts_type_in_scope(n: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if n.file_path == r.file_path || !is_ts_js_file(&n.file_path) {
        return true;
    }
    let Some(scope) = ts_global_scope(&n.file_path, ctx) else { return true };
    if (scope.script && !is_declaration_file(&n.file_path)) || (is_test_file(&n.file_path) && !is_test_file(&r.file_path)) {
        return false;
    }
    scope.global_words.contains(&format!("{}:{}", n.start_line, n.start_column))
}

/// A bare type name in a module file (not a script, not imported, not inside an ambient module) can only mean a type declared in that file.
pub fn is_ts_bare_type_name(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let type_ref = r.reference_kind == ReferenceKind::Edge(EdgeKind::References);
    if !(type_ref || is_inheritance_ref(r))
        || !is_js_family(r.language)
        || !is_ts_js_file(&r.file_path)
        || !is_plain_name(&r.reference_name)
        || ctx.get_import_mappings(&r.file_path, r.language).iter().any(|m| m.local_name == r.reference_name)
        || (type_ref && is_written_qualified(r, ctx))
    {
        return false;
    }
    let Some(scope) = ts_global_scope(&r.file_path, ctx) else { return false };
    !scope.script && !scope.import_aliases.contains(&r.reference_name) && !scope.module_blocks.iter().any(|b| in_block(r.line, r.column, b))
}

/// `ns.Type` / `a . Type`: the name as written follows a `.`.
pub fn is_written_qualified(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let line = line_at(ctx, &r.file_path, r.line).unwrap_or_default();
    if !starts_with_at(&line, &r.reference_name, r.column) {
        return true;
    }
    let before = before_col(&line, r.column);
    before.trim_end_matches([' ', '\t']).ends_with('.')
}

/// Can `r` reach `candidate` across files? A sealed module or an unexported top-level binding is private.
pub fn is_cross_file_reachable(candidate: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if r.language.as_str() != "markdown" && candidate.language.as_str() == "markdown" {
        return false;
    }
    if is_calls(r)
        && is_esm_family(candidate.language)
        && matches!(candidate.kind, NodeKind::Constant | NodeKind::Variable)
        && re!(r#"^=\s*require\s*\(\s*(['"])[^'"]+\.json\1\s*\)\s*;?\s*$"#).is_match(candidate.signature.as_deref().unwrap_or(""))
    {
        return false;
    }
    candidate.file_path == r.file_path
        || !is_esm_family(candidate.language)
        || (!is_sealed_module(&candidate.file_path, ctx) && !is_unexported_module_binding(candidate, ctx))
}

fn is_esm_binding_kind(k: NodeKind) -> bool {
    matches!(
        k,
        NodeKind::Function | NodeKind::Variable | NodeKind::Constant | NodeKind::Class | NodeKind::Interface | NodeKind::TypeAlias | NodeKind::Enum | NodeKind::Component
    )
}

fn export_list(file_path: &str, ctx: &Ctx) -> Rc<ExportList> {
    memoized::<ExportListMemo>(ctx, file_path.to_string(), || {
        let source = ctx.read_file(file_path).map(|s| s.to_string()).unwrap_or_default();
        let code = blank_string_contents(&strip_comments_for_regex(&source, "typescript"), None);
        let module = (has_import_statement(&code) || has_esm_export(&code)) && !has_cjs_export(&source) && !re!(r"\bdeclare\s+global\b").is_match(&code);
        let mut names = std::collections::HashSet::new();
        if module {
            for m in re!(r"^[ \t]*export\s+(?:type\s+)?\{([^}]*)\}", "gm").captures_iter(&source) {
                for item in m.text(1).unwrap_or("").split(',') {
                    let trimmed = codegraph_types::jscompat::js_trim(item);
                    let stripped = re!(r"^type\s+").replace_first(trimmed, |_| String::new());
                    let local = re!(r"\s+as\s+").split(&stripped).into_iter().next().unwrap_or_default();
                    let local = codegraph_types::jscompat::js_trim(&local);
                    if !local.is_empty() {
                        names.insert(local.to_string());
                    }
                }
            }
            for m in re!(r"^[ \t]*export\s+(?:default|=)\s+([A-Za-z_$][\w$]*)\s*;?\s*$", "gm").captures_iter(&source) {
                names.insert(m.text(1).unwrap_or("").to_string());
            }
            for m in re!(r"^[ \t]*export\s+default\s+\{([^}]*)\}|\breturn\s*\{([^{}]*)\}", "gm").captures_iter(&code) {
                let body = m.text(1).or(m.text(2)).unwrap_or("");
                for item in body.split(',') {
                    let value = if item.contains(':') { item.rsplit(':').next().unwrap_or("") } else { item };
                    if let Some(id) = re!(r"^\s*([A-Za-z_$][\w$]*)\s*$").captures(value).and_then(|c| c.text(1).map(|s| s.to_string())) {
                        names.insert(id);
                    }
                }
            }
        }
        Rc::new(ExportList { module, names })
    })
}

/// A top-level binding of an ES module that is never exported.
pub fn is_unexported_module_binding(candidate: &Node, ctx: &Ctx) -> bool {
    if candidate.is_exported == Some(true) || !is_esm_binding_kind(candidate.kind) {
        return false;
    }
    if candidate.qualified_name.contains("::") || re!(r"\.d\.[cm]?ts$").is_match(&candidate.file_path) {
        return false;
    }
    let info = export_list(&candidate.file_path, ctx);
    if !info.module || info.names.contains(&candidate.name) {
        return false;
    }
    let line = line_at(ctx, &candidate.file_path, candidate.start_line).unwrap_or_default();
    if re!(r"^\s*export\b").is_match(&line) {
        return false;
    }
    re!(r"^\s*(?:declare\s+)?(?:async\s+)?(?:function\*?|const|let|var|(?:abstract\s+)?class|interface|type|enum)\s").is_match(&line)
}

/// A Lua `local x = …` is private to its chunk.
pub fn is_lua_local(candidate: &Node, ctx: &Ctx) -> bool {
    if !matches!(candidate.kind, NodeKind::Variable | NodeKind::Constant | NodeKind::Function) {
        return false;
    }
    memoized::<LuaLocalMemo>(ctx, candidate.id.clone(), || {
        let line = line_at(ctx, &candidate.file_path, candidate.start_line).unwrap_or_default();
        re!(r"^\s*local\b").is_match(&line)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_shapes_are_told_apart() {
        assert!(has_import_statement("import a from 'a';\n"));
        assert!(!has_import_statement("const x = 'import a';\n"));
        assert!(has_esm_export("export const x = 1;"));
        assert!(has_esm_export("declare global {}"));
        assert!(has_cjs_export("module.exports = {}"));
        assert!(has_cjs_export("exports.a = 1"));
        assert!(!has_cjs_export("const exportsList = 1"));
    }
}
