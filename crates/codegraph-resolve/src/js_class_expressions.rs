//! A named class expression's name is bound inside the expression alone (#2499) — port of
//! `src/resolution/js-class-expressions.ts`.
//!
//! `register(class Gadget {…})` declares no `Gadget` for the code around it. The extractor gives
//! such a class a node, marked with its head as its signature (`class Gadget`), which a class
//! declaration doesn't carry. The resolver reads a reference outside the class expressions without
//! their classes and members (`Ctx::with_view`), so there it means what it meant before those
//! classes were indexed. A class a CommonJS module exports, `module.exports = class Router {…}`,
//! is the module's value, which every importer binds, so it isn't scoped.

use crate::types::UnresolvedRef;
use codegraph_jsre::re;
use codegraph_types::jscompat::utf16_prefix;
use codegraph_types::{Language, Node, NodeKind};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// Languages a class expression's class and the references to its name can be written in: TS/JS,
/// and the scripts of Vue, Svelte and Astro files.
pub fn is_class_expression_ref_language(language: Language) -> bool {
    matches!(
        language,
        Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx | Language::ArkTs | Language::Vue | Language::Svelte | Language::Astro
    )
}

/// What the resolver knows of the classes named class expressions declare.
pub struct ClassExpressionIndex {
    /// The classes, by name.
    pub by_name: HashMap<String, Vec<Rc<Node>>>,
    /// The classes, by the file that holds them.
    pub by_file: HashMap<String, Vec<Rc<Node>>>,
    /// The files that hold them.
    pub files: HashSet<String>,
    /// The ids of the classes and of their members.
    pub hidden_ids: HashSet<String>,
}

/// Whether `node` is a class a named class expression declares.
pub fn is_js_class_expression(node: &Node) -> bool {
    node.kind == NodeKind::Class && node.signature.as_deref().is_some_and(|s| !s.is_empty()) && is_class_expression_ref_language(node.language)
}

/// Whether a class expression is what its CommonJS module exports, as in
/// `module.exports = class Router {…}` or `exports.Router = class Router {…}` (`lines`: its
/// file's lines).
pub fn is_common_js_exported_class(node: &Node, lines: &[String]) -> bool {
    let Some(line) = lines.get((node.start_line as usize).wrapping_sub(1)) else { return false };
    let before = if node.start_line >= 2 { lines.get(node.start_line as usize - 2).map_or("", String::as_str) } else { "" };
    let text = format!("{before}\n{}", utf16_prefix(line, node.start_column as usize));
    re!(r"(?:^|[^\w$.])(?:module\.)?exports(?:\.[A-Za-z_$][\w$]*)?\s*=\s*$").is_match(&text)
}

/// Whether the reference is written inside `node`'s source range.
pub fn written_in(node: &Node, r: &UnresolvedRef) -> bool {
    if node.file_path != r.file_path {
        return false;
    }
    let after_start = r.line > node.start_line || (r.line == node.start_line && r.column >= node.start_column);
    let before_end = r.line < node.end_line || (r.line == node.end_line && r.column <= node.end_column);
    after_start && before_end
}

/// Build the index from every class node and the nodes of the files that hold class expressions;
/// None when there are none. A class a CommonJS module exports is left out.
pub fn build_class_expression_index(
    classes: &[Rc<Node>],
    lines_of: impl Fn(&str) -> Rc<Vec<String>>,
    nodes_of_files: impl FnOnce(&[String]) -> Vec<Node>,
) -> Option<ClassExpressionIndex> {
    let mut by_name: HashMap<String, Vec<Rc<Node>>> = HashMap::new();
    let mut by_file: HashMap<String, Vec<Rc<Node>>> = HashMap::new();
    let mut by_key: HashSet<String> = HashSet::new();
    let mut files: HashSet<String> = HashSet::new();
    let mut file_order: Vec<String> = Vec::new();
    let mut hidden_ids: HashSet<String> = HashSet::new();
    for n in classes {
        if !is_js_class_expression(n) || is_common_js_exported_class(n, &lines_of(&n.file_path)) {
            continue;
        }
        by_name.entry(n.name.clone()).or_default().push(Rc::clone(n));
        by_file.entry(n.file_path.clone()).or_default().push(Rc::clone(n));
        by_key.insert(format!("{}\0{}", n.file_path, n.qualified_name));
        if files.insert(n.file_path.clone()) {
            file_order.push(n.file_path.clone());
        }
        hidden_ids.insert(n.id.clone());
    }
    if by_name.is_empty() {
        return None;
    }
    // A member is its class's when it is declared in its file under its qualified name.
    for n in nodes_of_files(&file_order) {
        if !matches!(n.kind, NodeKind::Method | NodeKind::Property | NodeKind::Field) {
            continue;
        }
        let owned = n.qualified_name.rfind("::").filter(|&c| c > 0).is_some_and(|c| by_key.contains(&format!("{}\0{}", n.file_path, &n.qualified_name[..c])));
        if owned {
            hidden_ids.insert(n.id);
        }
    }
    Some(ClassExpressionIndex { by_name, by_file, files, hidden_ids })
}

/// Whether `r` resolves in the view without the class expressions: written outside every class
/// expression, or naming one it is not written in (classExpressionFreeContextFor).
pub fn wants_class_expression_free_view(r: &UnresolvedRef, index: &ClassExpressionIndex) -> bool {
    if !is_class_expression_ref_language(r.language) {
        return false;
    }
    let head = match r.reference_name.find('.') {
        Some(dot) if dot > 0 => &r.reference_name[..dot],
        _ => r.reference_name.as_str(),
    };
    let named = index.by_name.get(head);
    // Written in a class expression, a reference sees its class and the classes around it, as
    // JavaScript scopes them, unless it names another one, which is bound only inside that one.
    let inside = index.by_file.get(&r.file_path).is_some_and(|ns| ns.iter().any(|n| written_in(n, r)));
    !(inside && !named.is_some_and(|ns| !ns.iter().any(|n| written_in(n, r))))
}
