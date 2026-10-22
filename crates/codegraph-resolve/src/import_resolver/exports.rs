//! Per-file export index and default-export reading (first part of `import-resolver.ts`).

use crate::alias_binding::extract_local_export_aliases;
use crate::ctx::Ctx;
use crate::synth_utils::make_line_at;
use crate::types::NodeRef;
use codegraph_extract::strip_comments::strip_comments_for_regex;
use codegraph_jsre::re;
use codegraph_types::jscompat::utf16_prefix;
use codegraph_types::NodeKind;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

pub fn is_default_binding_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Function | NodeKind::Class | NodeKind::Component | NodeKind::Constant | NodeKind::Variable)
}

fn is_default_declaration_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Function | NodeKind::Class | NodeKind::Component)
}

/// `.js`/`.ts`-family file.
pub fn is_js_family_path(file_path: &str) -> bool {
    re!(r"\.(?:[cm]?[jt]sx?)$").is_match(file_path)
}

pub fn is_python_module_file(file_path: &str) -> bool {
    re!(r"\.py$").is_match(file_path)
}

/// A def at module level — what `from mod import name` can bind.
fn is_python_module_level_def(n: &NodeRef) -> bool {
    matches!(n.kind, NodeKind::Function | NodeKind::Class | NodeKind::Variable | NodeKind::Constant) && !n.qualified_name.contains("::")
}

/// What a file's own `export default` statement exports: the node it declares or names,
/// `Anonymous` for a function or class with no name (no node stands for it), `Unstated` when the
/// file says nothing a node answers.
#[derive(Clone)]
pub enum StatedDefault {
    Node(NodeRef),
    Anonymous,
    Unstated,
}

/// Per-file index of exported symbols. First-wins insertion preserves exactly the array-order
/// semantics of the `.find` calls it replaces.
pub struct FileExportIndex {
    by_name: HashMap<String, NodeRef>,
    pub default_component: Option<NodeRef>,
    pub default_fn_class: Option<NodeRef>,
    stated_default: RefCell<Option<StatedDefault>>,
    clause_aliases: RefCell<Option<HashMap<String, NodeRef>>>,
}

#[derive(Default)]
pub struct FileExportIndexes(RefCell<HashMap<String, Rc<FileExportIndex>>>);

pub fn get_file_export_index(file_path: &str, ctx: &Ctx) -> Rc<FileExportIndex> {
    let memo = ctx.memo::<FileExportIndexes>();
    if let Some(i) = memo.0.borrow().get(file_path) {
        return Rc::clone(i);
    }
    let mut idx = FileExportIndex {
        by_name: HashMap::new(),
        default_component: None,
        default_fn_class: None,
        stated_default: RefCell::new(None),
        clause_aliases: RefCell::new(None),
    };
    // Python has no export keyword: every module-level def is importable, and the extractor
    // never flags one isExported.
    let exported: Vec<NodeRef> = if is_python_module_file(file_path) {
        ctx.get_nodes_in_file(file_path).iter().filter(|n| n.is_exported == Some(true) || is_python_module_level_def(n)).cloned().collect()
    } else {
        ctx.get_exported_nodes_in_file(file_path)
    };
    for n in exported {
        idx.by_name.entry(n.name.clone()).or_insert_with(|| Rc::clone(&n));
        if idx.default_component.is_none() && n.kind == NodeKind::Component {
            idx.default_component = Some(Rc::clone(&n));
        }
        if idx.default_fn_class.is_none() && matches!(n.kind, NodeKind::Function | NodeKind::Class) {
            idx.default_fn_class = Some(Rc::clone(&n));
        }
    }
    let idx = Rc::new(idx);
    memo.0.borrow_mut().insert(file_path.to_string(), Rc::clone(&idx));
    idx
}

/// The file's nodes named `name`, in `getNodesInFile` order.
pub fn nodes_in_file_named(file_path: &str, name: &str, ctx: &Ctx) -> Vec<NodeRef> {
    ctx.get_nodes_in_file_named(file_path, name)
}

enum DefaultValue {
    Anonymous,
    Named { name: String, name_at: usize, declared: bool },
}

/// The value of the `export default` statement whose value starts at byte `at`, in
/// comment-stripped code: the name a declaration or a binding gives it, `Anonymous` for a
/// function, class or arrow function without one, or `None` for any other expression.
fn read_default_export(code: &str, at: usize) -> Option<DefaultValue> {
    let rest = utf16_prefix(&code[at..], 2048);
    if let Some(decl) = re!(
        r"^(?:async\s+)?function\b\s*(?:\*\s*)?([A-Za-z_$][\w$]*)?|^(?:abstract\s+)?class\b(?:\s+(?!extends\b|implements\b)([A-Za-z_$][\w$]*))?"
    )
    .captures(rest)
    {
        let name = decl.text(1).or_else(|| decl.text(2)).filter(|n| !n.is_empty());
        let whole = decl.get(0).unwrap().as_str();
        return Some(match name {
            Some(name) => DefaultValue::Named { name: name.to_string(), name_at: at + whole.len() - name.len(), declared: true },
            None => DefaultValue::Anonymous,
        });
    }
    if let Some(arrow) = re!(r"^(?:async\s+)?(?:[A-Za-z_$][\w$]*\s*=>|\()").find(rest) {
        let a = arrow.as_str();
        if !a.ends_with('(') {
            return Some(DefaultValue::Anonymous);
        }
        // A parenthesized list is an arrow function's parameters when `=>` or a return type
        // (`(…): JSX.Element =>`) follows it; otherwise it is an expression (`(Foo)`).
        let mut depth = 0i32;
        let bytes = rest.as_bytes();
        let mut i = a.len() - 1;
        while i < bytes.len() {
            if bytes[i] == b'(' {
                depth += 1;
            } else if bytes[i] == b')' {
                depth -= 1;
                if depth == 0 {
                    return if re!(r"^\s*(?:=>|:)").is_match(&rest[i + 1..]) { Some(DefaultValue::Anonymous) } else { None };
                }
            }
            i += 1;
        }
        return None;
    }
    re!(r"^([A-Za-z_$][\w$]*)\s*;?[ \t]*(?:\r?\n|$)")
        .captures(rest)
        .map(|b| DefaultValue::Named { name: b.text(1).unwrap().to_string(), name_at: at, declared: false })
}

/// What `file_path`'s own `export default` statement exports. The statement says it outright,
/// where the export index can only guess.
pub fn stated_default_export(file_path: &str, idx: &FileExportIndex, ctx: &Ctx) -> StatedDefault {
    if let Some(s) = idx.stated_default.borrow().as_ref() {
        return s.clone();
    }
    let mut stated: Option<StatedDefault> = None;
    let source = if is_js_family_path(file_path) { ctx.read_file(file_path) } else { None };
    if let Some(source) = source.filter(|s| !s.is_empty() && re!(r"\bexport\s+default\b").is_match(s)) {
        let code = strip_comments_for_regex(&source, "typescript");
        let line_at = make_line_at(&code, 1);
        for m in re!(r"^[ \t]*export\s+default\s+", "gm").captures_iter(&code) {
            let whole = m.get(0).unwrap();
            let value = read_default_export(&code, whole.end());
            match value {
                Some(DefaultValue::Anonymous) => {
                    stated.get_or_insert(StatedDefault::Anonymous);
                    continue;
                }
                _ => {
                    stated.get_or_insert(StatedDefault::Unstated);
                }
            }
            let Some(DefaultValue::Named { name, name_at, declared }) = value else { continue };
            let named = nodes_in_file_named(file_path, &name, ctx);
            // A declaration starts on the statement's line, or on its name's when the statement
            // breaks before it (`export default\nfunction X`).
            let from = line_at.at(whole.start());
            let to = line_at.at(name_at);
            let node = if declared {
                named.iter().find(|n| is_default_declaration_kind(n.kind) && n.start_line >= from && n.start_line <= to).cloned()
            } else {
                let mut v: Vec<NodeRef> = named.iter().filter(|n| is_default_binding_kind(n.kind)).cloned().collect();
                v.sort_by(|a, b| a.start_line.cmp(&b.start_line).then(a.start_column.cmp(&b.start_column)));
                v.into_iter().next()
            };
            if let Some(node) = node {
                stated = Some(StatedDefault::Node(node));
                break;
            }
        }
    }
    let result = stated.unwrap_or(StatedDefault::Unstated);
    *idx.stated_default.borrow_mut() = Some(result.clone());
    result
}

pub struct CommonJsDefault {
    pub source: Option<String>,
    pub name: Option<String>,
}

/// `module.exports = …` (also `exports = module.exports = …`): the name it binds
/// (`createApplication`, `function name(`, `class Name`) or the module it forwards
/// (`require('./lib/express')`), or `None`.
pub fn common_js_default_export(file_path: &str, ctx: &Ctx) -> Option<CommonJsDefault> {
    if !is_js_family_path(file_path) {
        return None;
    }
    if !ctx.file_contains(file_path, "module.exports") {
        return None;
    }
    let source = ctx.read_file(file_path)?;
    if source.is_empty() || !source.contains("module.exports") {
        return None;
    }
    let m = re!(
        r#"^[ \t]*(?:exports\s*=\s*)?module\.exports\s*=\s*(?:exports\s*=\s*)?(?:require\(\s*['"]([^'"]+)['"]\s*\)\s*;?[ \t]*$|(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)\s*\(|class\s+([A-Za-z_$][\w$]*)|([A-Za-z_$][\w$]*)\s*;?[ \t]*$)"#,
        "m"
    )
    .captures(&source)?;
    if let Some(s) = m.text(1).filter(|s| !s.is_empty()) {
        return Some(CommonJsDefault { source: Some(s.to_string()), name: None });
    }
    let name = m.text(2).or_else(|| m.text(3)).or_else(|| m.text(4)).map(String::from);
    Some(CommonJsDefault { source: None, name })
}

/// The declaration a module default-exports: what its own `export default` statement declares or
/// names, and nothing for an anonymous default. Only a default the statement leaves unnamed, or a
/// module with no statement, falls back: to the component a single-file component file is, else
/// to the first exported function or class.
pub fn esm_default_export(file_path: &str, idx: &FileExportIndex, ctx: &Ctx) -> Option<NodeRef> {
    match stated_default_export(file_path, idx, ctx) {
        StatedDefault::Anonymous => None,
        StatedDefault::Node(n) => Some(n),
        StatedDefault::Unstated => idx.default_component.clone().or_else(|| idx.default_fn_class.clone()),
    }
}

/// Whether the module has an ESM default export at all: it is a single-file component, or a
/// JS-family file with an `export default` statement.
pub fn has_esm_default_export(file_path: &str, idx: &FileExportIndex, ctx: &Ctx) -> bool {
    if idx.default_component.is_some() {
        return true;
    }
    if !is_js_family_path(file_path) {
        return false;
    }
    match ctx.read_file(file_path) {
        Some(source) => !source.is_empty() && source.contains("default") && re!(r"^[ \t]*export\s+default\b", "m").is_match(&source),
        None => false,
    }
}

/// What this file exports as `name`: an exported declaration, else a local export clause's binding.
pub fn exported_by_name(file_path: &str, idx: &FileExportIndex, name: &str, ctx: &Ctx) -> Option<NodeRef> {
    if let Some(d) = idx.by_name.get(name) {
        return Some(Rc::clone(d));
    }
    if idx.clause_aliases.borrow().is_none() {
        // Bind names introduced by a local export clause to their declarations, so an importer
        // asking for the renamed name gets the real symbol. The declaration is the file's first
        // node of that name, exported or not.
        let mut aliases: HashMap<String, NodeRef> = HashMap::new();
        if let Some(content) = ctx.read_file(file_path) {
            if content.contains("export") {
                for a in extract_local_export_aliases(&content) {
                    if idx.by_name.contains_key(&a.exported_name) || aliases.contains_key(&a.exported_name) {
                        continue;
                    }
                    if let Some(decl) = nodes_in_file_named(file_path, &a.local_name, ctx).into_iter().next() {
                        aliases.insert(a.exported_name, decl);
                    }
                }
            }
        }
        *idx.clause_aliases.borrow_mut() = Some(aliases);
    }
    idx.clause_aliases.borrow().as_ref().and_then(|m| m.get(name).cloned())
}
