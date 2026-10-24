//! React JSX child rendering and Vue SFC templates — port of `reactJsxChildEdges` /
//! `vueTemplateEdges` (and the helpers they share) from `src/resolution/callback-synthesizer.ts`.

use super::common::*;
use super::jsx_scope::{own_lines_bind, span_tags, Code, SpanTags};
use crate::ctx::Ctx;
use crate::frameworks::react::{declared_component, exported_component, wrapped_component_name, DeclaredComponent};
use crate::import_resolver::resolve_import_path;
use crate::jsx_code::jsx_code;
use crate::meta;
use crate::name_matcher::crosses_code_boundary;
use crate::name_matcher::js_scope::js_code_binds_name;
use crate::types::NodeRef;
use codegraph_jsre::re;
use codegraph_types::jscompat::{js_trim, utf16_from, utf16_prefix};
use codegraph_types::{Edge, Language, NodeKind};
use indexmap::{IndexMap, IndexSet};
use std::cell::{OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

const MAX_JSX_CHILDREN: usize = 30;
const JSX_CHILD_KINDS: [NodeKind; 3] = [NodeKind::Component, NodeKind::Function, NodeKind::Class];
/// The languages a JSX tag can plausibly name a component in. Preferred over a same-named symbol
/// in another language, never required.
const JSX_CHILD_LANGUAGES: [Language; 6] =
    [Language::TypeScript, Language::JavaScript, Language::Tsx, Language::Jsx, Language::Vue, Language::Svelte];

fn is_jsx_child_kind(k: NodeKind) -> bool {
    JSX_CHILD_KINDS.contains(&k)
}

/// Whether a JSX tag can render `node` (`jsxChild`): a script's component, function or class named
/// with a tag's capital, or any component.
#[allow(dead_code)]
pub fn is_jsx_child_candidate(kind: NodeKind, name: &str, language: Language) -> bool {
    is_jsx_child_kind(kind)
        && JS_FAMILY.contains(&language)
        && (kind == NodeKind::Component || name.chars().next().is_some_and(|c| c.is_ascii_uppercase()))
}

pub fn kebab_to_pascal(s: &str) -> String {
    s.split('-')
        .map(|p| {
            let mut cs = p.chars();
            match cs.next() {
                Some(c) => c.to_uppercase().collect::<String>() + cs.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// Nuxt auto-import name for a component, derived from its path UNDER `components/`:
/// `components/media/Card.vue` → `MediaCard`. `None` for a flat component.
fn nuxt_component_name(file_path: &str) -> Option<String> {
    let marker = file_path.rfind("components/")?;
    let rel = &file_path[marker + "components/".len()..];
    let rel = re!(r"\.(vue|ts|tsx|js|jsx)$", "i").replace_first(rel, |_| String::new());
    let segs: Vec<String> = rel.split('/').filter(|s| !s.is_empty()).map(kebab_to_pascal).collect();
    if segs.len() < 2 {
        return None;
    }
    let mut out: Vec<String> = Vec::new();
    for s in segs {
        match out.last() {
            Some(prev) if s.starts_with(prev.as_str()) => *out.last_mut().unwrap() = s,
            _ => out.push(s),
        }
    }
    Some(out.concat())
}

pub fn language_for_jsx_file(file: &str) -> Language {
    if file.ends_with(".tsx") {
        return Language::Tsx;
    }
    if re!(r"\.[cm]?ts$").is_match(file) {
        return Language::TypeScript;
    }
    if file.ends_with(".jsx") {
        return Language::Jsx;
    }
    if file.ends_with(".vue") {
        return Language::Vue;
    }
    if file.ends_with(".svelte") {
        return Language::Svelte;
    }
    Language::JavaScript
}

type Imports = Rc<HashMap<String, String>>;

/// `localName` → the project file it is imported from, for one file's imports.
pub fn imported_from(ctx: &Ctx, file: &str, language: Language) -> Imports {
    let mut out = HashMap::new();
    for im in ctx.get_import_mappings(file, language).iter() {
        // The mappings name the module as written; the file it is comes from the same resolution
        // the import resolver uses (aliases, extensions, index files).
        let resolved = im.resolved_path.clone().or_else(|| resolve_import_path(&im.source, file, language, ctx));
        if let Some(r) = resolved {
            out.insert(im.local_name.clone(), r);
        }
    }
    Rc::new(out)
}

/// Does `file` import `name` from a package outside the repository? What such a name renders is
/// the package's, never a project symbol that shares the name.
pub fn imported_from_package(ctx: &Ctx, name: &str, file: &str) -> bool {
    let language = language_for_jsx_file(file);
    let mappings = ctx.get_import_mappings(file, language);
    match mappings.iter().find(|m| m.local_name == name) {
        Some(binding) => ctx.is_out_of_repo_import(&binding.source, file, language),
        None => false,
    }
}

pub struct ProjectImport {
    pub module: String,
    pub exported: String,
}

/// The module of the project `file` imports `name` from, and the export the import names.
pub fn project_import(ctx: &Ctx, name: &str, file: &str) -> Option<ProjectImport> {
    let language = language_for_jsx_file(file);
    let mappings = ctx.get_import_mappings(file, language);
    let binding = mappings.iter().find(|m| m.local_name == name)?;
    if binding.is_namespace {
        return None;
    }
    let module = binding.resolved_path.clone().or_else(|| resolve_import_path(&binding.source, file, language, ctx))?;
    if module.is_empty() || !ctx.get_nodes_in_file(&module).iter().any(|n| n.kind == NodeKind::File) {
        return None;
    }
    let exported = if binding.is_default || binding.exported_name == "default" { "default".to_string() } else { binding.exported_name.clone() };
    Some(ProjectImport { module, exported })
}

/// The files a JSX file's project-imported tags are read through: each tag's module, the index or
/// barrel that hands its component on, and the module that declares it.
pub fn imported_tag_modules(ctx: &Ctx, file: &str, content: &str, memo: &mut HashMap<String, IndexSet<String>>) -> IndexSet<String> {
    let mut out: IndexSet<String> = IndexSet::new();
    let mut names: IndexSet<String> = IndexSet::new();
    for m in super::jsx_scope::jsx_tag_re().captures_iter(content) {
        names.insert(m.text(1).unwrap_or("").to_string());
    }
    for name in &names {
        let Some(imported) = project_import(ctx, name, file) else { continue };
        let key = format!("{}\0{}", imported.module, imported.exported);
        if !memo.contains_key(&key) {
            let mut reads: IndexSet<String> = IndexSet::new();
            exported_component(&imported.module, &imported.exported, ctx, &IndexSet::new(), &mut reads);
            memo.insert(key.clone(), reads);
        }
        for read in &memo[&key] {
            out.insert(read.clone());
        }
    }
    out.shift_remove(file);
    out
}

/// What the lookups of one file share: its imports, the values it declares, the modules its tags
/// import (memoized for the whole pass).
struct JsxScope<'a> {
    ctx: &'a Ctx,
    file: &'a str,
    own_values: RefCell<HashMap<String, Rc<Option<DeclaredComponent>>>>,
    exports: &'a RefCell<HashMap<String, Option<NodeRef>>>,
}

impl JsxScope<'_> {
    fn own_value_of(&self, name: &str) -> Rc<Option<DeclaredComponent>> {
        if let Some(v) = self.own_values.borrow().get(name) {
            return Rc::clone(v);
        }
        let v = Rc::new(declared_component(name, self.file, self.ctx));
        self.own_values.borrow_mut().insert(name.to_string(), Rc::clone(&v));
        v
    }

    /// Once per module and export for the whole pass: what a tag imported from the project
    /// renders.
    fn export_of(&self, module: &str, exported: &str) -> Option<NodeRef> {
        let key = format!("{module}\0{exported}");
        if let Some(n) = self.exports.borrow().get(&key) {
            return n.clone();
        }
        let node = exported_component(module, exported, self.ctx, &IndexSet::new(), &mut IndexSet::new())
            .and_then(|id| self.ctx.node_by_id(&id));
        self.exports.borrow_mut().insert(key, node.clone());
        node
    }
}

/// The component a JSX tag names, among every node that shares the name. Same file first; then a
/// name the file imports from a package (nothing); then the module of the project it imports from;
/// then the language decides a tie.
fn jsx_child(scope: &JsxScope, name: &str, imports_of: &dyn Fn() -> Imports) -> Option<NodeRef> {
    let ctx = scope.ctx;
    let file = scope.file;
    let own = scope.own_value_of(name);
    if let Some(own) = own.as_ref() {
        return Some(own.component.clone().unwrap_or_else(|| Rc::clone(&own.declaration)));
    }
    let candidates: Vec<NodeRef> = ctx.get_nodes_by_name(name).iter().filter(|n| is_jsx_child_kind(n.kind)).cloned().collect();
    if let Some(local) = candidates.iter().find(|n| n.file_path == file) {
        return Some(Rc::clone(local));
    }
    if imported_from_package(ctx, name, file) {
        return None;
    }
    if let Some(imported) = project_import(ctx, name, file) {
        if let Some(exported) = scope.export_of(&imported.module, &imported.exported) {
            return Some(exported);
        }
        if let Some(in_module) = candidates.iter().find(|n| n.file_path == imported.module) {
            return Some(Rc::clone(in_module));
        }
        if !candidates.is_empty() {
            return None;
        }
    }
    if candidates.is_empty() {
        // A name nothing declares is the file's DEFAULT import of a module's one component under
        // another name.
        let is_default = ctx
            .get_import_mappings(file, language_for_jsx_file(file))
            .iter()
            .any(|m| m.local_name == name && m.is_default);
        let from = if is_default { imports_of().get(name).cloned() } else { None };
        let components: Vec<NodeRef> = match from {
            Some(f) => ctx.get_nodes_in_file(&f).iter().filter(|n| n.kind == NodeKind::Component).cloned().collect(),
            None => Vec::new(),
        };
        return if components.len() == 1 { Some(Rc::clone(&components[0])) } else { None };
    }
    if candidates.len() == 1 {
        return Some(Rc::clone(&candidates[0]));
    }
    if let Some(from) = imports_of().get(name) {
        if let Some(imported) = candidates.iter().find(|n| &n.file_path == from) {
            return Some(Rc::clone(imported));
        }
    }
    candidates.iter().find(|n| JSX_CHILD_LANGUAGES.contains(&n.language)).or(candidates.first()).cloned()
}

/// The component a wrapper component wraps, bound the way a tag of that name in the file is, but
/// only where the file binds the name itself.
fn wrapped_child(scope: &JsxScope, name: &str, imports_of: &dyn Fn() -> Imports) -> Option<NodeRef> {
    let ctx = scope.ctx;
    let declared = scope.own_value_of(name).is_some()
        || ctx.get_nodes_in_file_named(scope.file, name).iter().any(|n| is_jsx_child_kind(n.kind));
    if !declared && project_import(ctx, name, scope.file).is_none() {
        return None;
    }
    jsx_child(scope, name, imports_of)
}

/// What a tag renders when its parent binds the name itself: a component the parent declares in
/// its own lines, else nothing.
fn declared_inside(ctx: &Ctx, name: &str, parent: &NodeRef, child: NodeRef) -> Option<NodeRef> {
    let inside = |n: &NodeRef| {
        n.id != parent.id && n.file_path == parent.file_path && n.start_line >= parent.start_line && n.start_line <= parent.end_line
    };
    if inside(&child) {
        return Some(child);
    }
    ctx.get_nodes_by_name(name).iter().find(|n| is_jsx_child_kind(n.kind) && inside(n)).cloned()
}

/// Whether `fn`'s code (`names`, its span) binds `name` by `line`: a `const`, a parameter or a
/// destructured name. Only a name it also writes outside a tag can be bound there.
fn scope_binds(names: &SpanTags, func: &NodeRef, name: &str, line: u32, file: &str, ctx: &Ctx) -> bool {
    names.bare.contains(name)
        && (js_code_binds_name(name, func, file, line, ctx) || names.destructured_by(name, line as i64 - func.start_line as i64))
}

/// The functions among `parents` whose lines hold `fn`'s, innermost first.
fn functions_around(parents: &[NodeRef], func: &NodeRef) -> Vec<NodeRef> {
    let mut around: Vec<NodeRef> = parents
        .iter()
        .filter(|n| {
            n.start_line <= func.start_line
                && n.end_line >= func.end_line
                && (n.start_line != func.start_line || n.end_line != func.end_line)
        })
        .cloned()
        .collect();
    around.sort_by(|a, b| b.start_line.cmp(&a.start_line).then(a.end_line.cmp(&b.end_line)));
    around
}

#[derive(Clone, Copy)]
enum Owns {
    /// `held[line] !== 1 && valuesOn[line] === 1`
    Value,
    /// `!held[line]`
    File,
}

/// Phase 5: React JSX child rendering — link each parent component/function/value → every
/// capitalized JSX child it renders.
pub fn react_jsx_child_edges(ctx: &Ctx) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let exports: RefCell<HashMap<String, Option<NodeRef>>> = RefCell::new(HashMap::new());
    let wrapper_call = re!(r"\b(?:styled|memo|forwardRef)\s*\(\s*[A-Z]");
    let before_re = re!(r"^\s*(?:(?:export|declare|default|const|let|var)\s+)*$");
    let after_re = re!(r"^[\s;]*$");

    for file in ctx.get_all_files() {
        let Some(content) = ctx.read_file(&file) else { continue };
        if content.is_empty() {
            continue;
        }
        let has_tags = content.contains("</") || content.contains("/>");
        let wraps = wrapper_call.is_match(&content);
        if !has_tags && !wraps {
            continue; // JSX-file gate
        }
        let in_file = ctx.get_nodes_in_file(&file);
        let scope = JsxScope { ctx, file: &file, own_values: RefCell::new(HashMap::new()), exports: &exports };

        // A wrapper component renders the component it wraps, with no tag to read.
        if wraps {
            let wrapper_imports: OnceCell<Imports> = OnceCell::new();
            for wrapper in in_file.iter() {
                if wrapper.kind != NodeKind::Component || !JS_FAMILY.contains(&wrapper.language) {
                    continue;
                }
                let Some(name) = wrapped_component_name(wrapper, ctx) else { continue };
                let lang = wrapper.language;
                let imports_of = || Rc::clone(wrapper_imports.get_or_init(|| imported_from(ctx, &file, lang)));
                let Some(child) = wrapped_child(&scope, &name, &imports_of) else { continue };
                if child.id == wrapper.id || crosses_code_boundary(wrapper.language, child.language) {
                    continue;
                }
                if !seen.insert(format!("{}>{}", wrapper.id, child.id)) {
                    continue;
                }
                edges.push(calls_edge(
                    &wrapper.id,
                    &child.id,
                    Some(wrapper.start_line),
                    meta! { "synthesizedBy" => "jsx-render", "via" => name.as_str(), "wrapped" => true },
                ));
            }
        }
        if !has_tags {
            continue;
        }
        let is_parent_kind = |k: NodeKind| matches!(k, NodeKind::Method | NodeKind::Function | NodeKind::Component);
        let fns: Vec<NodeRef> =
            in_file.iter().filter(|n| is_parent_kind(n.kind) && JSX_FAMILY.contains(&n.language)).cloned().collect();
        // JSX outside every function renders from where a plain call there is attributed (#2587).
        let code_cell: OnceCell<Option<String>> = OnceCell::new();
        let code_of = || code_cell.get_or_init(|| jsx_code(&content)).as_deref();
        let mut lines: Option<Vec<String>> = None;
        let holds = |n: &NodeRef| is_parent_kind(n.kind) || n.kind == NodeKind::Class;
        let mut file_node: Option<NodeRef> =
            in_file.iter().find(|n| n.kind == NodeKind::File && JSX_FAMILY.contains(&n.language)).cloned();
        let mut owns: HashMap<String, Owns> = HashMap::new();
        let mut values: Vec<NodeRef> = Vec::new();
        let mut held: Vec<u8> = Vec::new();
        let mut values_on: Vec<u8> = Vec::new();
        if let Some(fnode) = file_node.clone() {
            // Per line: 1 when a function or class holds it, 2 when any other node does.
            held = vec![0u8; fnode.end_line as usize + 2];
            values_on = vec![0u8; held.len()];
            for n in in_file.iter() {
                if matches!(n.kind, NodeKind::File | NodeKind::Import | NodeKind::Export | NodeKind::Route)
                    || n.start_line == 0
                    || n.end_line == 0
                {
                    continue;
                }
                let mark = if holds(n) { 1 } else { 2 };
                let mut l = n.start_line as usize;
                while l <= n.end_line as usize && l < held.len() {
                    if held[l] != 1 {
                        held[l] = mark;
                    }
                    l += 1;
                }
            }
            // A top-level value: its first line is in no function or class.
            values = in_file
                .iter()
                .filter(|n| {
                    matches!(n.kind, NodeKind::Constant | NodeKind::Variable)
                        && JSX_FAMILY.contains(&n.language)
                        && held.get(n.start_line as usize).copied() != Some(1)
                })
                .cloned()
                .collect();
            // A line a value shares with another statement is no one's.
            let raw: &Vec<String> = lines.get_or_insert_with(|| {
                code_of().unwrap_or(&content).split('\n').map(String::from).collect()
            });
            for v in &values {
                let mut l = v.start_line as usize;
                while l <= v.end_line as usize && l < values_on.len() {
                    if values_on[l] < 2 {
                        values_on[l] += 1;
                    }
                    l += 1;
                }
                let start_line_text = raw.get(v.start_line as usize - 1).map(String::as_str).unwrap_or("");
                let before = utf16_prefix(start_line_text, v.start_column as usize);
                let end_line_text = raw.get(v.end_line as usize - 1).map(String::as_str).unwrap_or("");
                let after = utf16_from(end_line_text, v.end_column as usize);
                if !before_re.is_match(before) {
                    values_on[v.start_line as usize] = 2;
                }
                if !after_re.is_match(after) {
                    values_on[v.end_line as usize] = 2;
                }
            }
            for v in &values {
                owns.insert(v.id.clone(), Owns::Value);
            }
            let lo = 1usize.min(held.len());
            let hi = (fnode.end_line as usize + 1).min(held.len());
            if held[lo..hi.max(lo)].contains(&0) {
                owns.insert(fnode.id.clone(), Owns::File);
            } else {
                file_node = None;
            }
        }
        let owns_line = |kind: Owns, line: usize| -> bool {
            match kind {
                Owns::Value => held.get(line).copied() != Some(1) && values_on.get(line).copied() == Some(1),
                Owns::File => held.get(line).copied().unwrap_or(0) == 0,
            }
        };
        let mut parents: Vec<NodeRef> = fns.clone();
        parents.extend(values.iter().cloned());
        if let Some(f) = &file_node {
            parents.push(Rc::clone(f));
        }
        if parents.is_empty() {
            continue;
        }
        // Read once per file, and only when a name actually turns out ambiguous.
        let imports_cell: OnceCell<Imports> = OnceCell::new();
        let parent_lang = parents[0].language;
        let imports_of = || Rc::clone(imports_cell.get_or_init(|| imported_from(ctx, &file, parent_lang)));

        // Each name a span holds, read when a function among the parents is, or a function around
        // one. A parent that owns its lines reads only those, the rest left blank.
        let mut tags_by_span: HashMap<String, Rc<SpanTags>> = HashMap::new();
        let mut span_of = |func: &NodeRef, lines: &mut Option<Vec<String>>| -> Rc<SpanTags> {
            let own = owns.get(&func.id).copied();
            let span = if own.is_some() { func.id.clone() } else { format!("{}:{}", func.start_line, func.end_line) };
            if let Some(names) = tags_by_span.get(&span) {
                return Rc::clone(names);
            }
            let all = lines.get_or_insert_with(|| code_of().unwrap_or(&content).split('\n').map(String::from).collect());
            let from = (func.start_line as usize).saturating_sub(1).min(all.len());
            let to = (func.end_line as usize).min(all.len()).max(from);
            let span_lines = &all[from..to];
            let joined = match own {
                Some(kind) => span_lines
                    .iter()
                    .enumerate()
                    .map(|(i, text)| if owns_line(kind, func.start_line as usize + i) { text.as_str() } else { "" })
                    .collect::<Vec<_>>()
                    .join("\n"),
                None => span_lines.join("\n"),
            };
            let names = Rc::new(span_tags(joined, func.start_line));
            tags_by_span.insert(span, Rc::clone(&names));
            names
        };
        // Whether the file destructures a name at its top, once per name.
        let top_patterns: OnceCell<Vec<(isize, isize)>> = OnceCell::new();
        let mut top_binds: HashMap<String, bool> = HashMap::new();
        let mut destructured_at_top = |name: &str| -> bool {
            if let Some(b) = top_binds.get(name) {
                return *b;
            }
            let binds = match code_of() {
                None => false,
                Some(src) => {
                    let code = Code::new(src);
                    let patterns = top_patterns.get_or_init(|| code.top_level_patterns());
                    !patterns.is_empty() && code.patterns_bind(patterns, name)
                }
            };
            top_binds.insert(name.to_string(), binds);
            binds
        };

        for parent in &parents {
            if parent.start_line == 0 || parent.end_line == 0 {
                continue;
            }
            let names = span_of(parent, &mut lines);
            // The functions around the parent, innermost first, once a tag needs them.
            let mut around: Option<Vec<NodeRef>> = None;
            let mut added = 0;
            for (name, &line) in names.tags.iter() {
                if added >= MAX_JSX_CHILDREN {
                    break;
                }
                if line == 0 {
                    continue;
                }
                let mut child = jsx_child(&scope, name, &imports_of);
                if let Some(c) = child.take() {
                    if let Some(own) = owns.get(&parent.id) {
                        let _ = own;
                        // A tag the file or a top-level value holds (#2587): a name its own lines
                        // declare, or one the file destructures at its top, is that local.
                        if (names.bare.contains(name) && own_lines_bind(name, &names.src)) || destructured_at_top(name) {
                            child = declared_inside(ctx, name, parent, c);
                        } else {
                            child = Some(c);
                        }
                    } else {
                        // A name the parent, or a function around it, binds by the parent's first
                        // tag is that local; it renders a component that function declares.
                        let mut binder: Option<NodeRef> =
                            if scope_binds(&names, parent, name, line, &file, ctx) { Some(Rc::clone(parent)) } else { None };
                        if binder.is_none() {
                            if around.is_none() {
                                around = Some(functions_around(&fns, parent));
                            }
                            for func in around.as_ref().unwrap() {
                                let around_names = span_of(func, &mut lines);
                                if scope_binds(&around_names, func, name, line, &file, ctx) {
                                    binder = Some(Rc::clone(func));
                                    break;
                                }
                            }
                        }
                        if let Some(b) = binder {
                            child = declared_inside(ctx, name, &b, c);
                        } else if destructured_at_top(name) {
                            // A name the file destructures at its top is every function's local.
                            let scope_fn = around.as_ref().and_then(|a| a.last()).cloned().unwrap_or_else(|| Rc::clone(parent));
                            child = declared_inside(ctx, name, &scope_fn, c);
                        } else {
                            child = Some(c);
                        }
                    }
                }
                let Some(child) = child else { continue };
                if child.id == parent.id || crosses_code_boundary(parent.language, child.language) {
                    continue;
                }
                if !seen.insert(format!("{}>{}", parent.id, child.id)) {
                    continue;
                }
                edges.push(calls_edge(
                    &parent.id,
                    &child.id,
                    Some(parent.start_line),
                    meta! { "synthesizedBy" => "jsx-render", "via" => name.as_str() },
                ));
                added += 1;
            }
        }
    }
    edges
}

/// Phase 6: Vue SFC templates — kebab-case / Nuxt children and `@event="handler"` bindings.
pub fn vue_template_edges(ctx: &Ctx) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let component_kinds = [NodeKind::Component, NodeKind::Function, NodeKind::Class];
    let handler_kinds = [NodeKind::Method, NodeKind::Function];
    // A composable's returned member may be a fn or an arrow assigned to a const.
    let return_kinds = [NodeKind::Method, NodeKind::Function, NodeKind::Variable, NodeKind::Constant];
    // Nuxt auto-imports nested components by a DIRECTORY-PREFIXED name; map each nested
    // component's Nuxt name → node so those template usages resolve.
    let mut nuxt_components: IndexMap<String, NodeRef> = IndexMap::new();
    each_node(ctx, NodeKind::Component, |c| {
        if let Some(nn) = nuxt_component_name(&c.file_path) {
            nuxt_components.entry(nn).or_insert(c);
        }
    });
    let template_re = re!(r"<template[^>]*>([\s\S]*)<\/template>", "i");
    let script_re = re!(r"<script[^>]*>([\s\S]*?)<\/script>", "i");
    let destructure_re = re!(r"(?:const|let|var)\s*\{([^}]+)\}\s*=\s*(\w+)\s*\(", "g");
    let kebab_re = re!(r"<([a-z][a-z0-9]*(?:-[a-z0-9]+)+)[\s/>]", "g");
    let pascal_re = re!(r"<([A-Z][A-Za-z0-9]*)[\s/>]", "g");
    let handler_re = re!(r##"(?:@|v-on:)([a-zA-Z][\w-]*)(?:\.[\w]+)*\s*=\s*"([^"]+)""##, "g");

    for file in ctx.get_all_files() {
        if !file.ends_with(".vue") {
            continue;
        }
        let Some(content) = ctx.read_file(&file) else { continue };
        let tpl = template_re.captures(&content).and_then(|m| m.text(1).map(String::from)).filter(|t| !t.is_empty());
        let Some(tpl) = tpl else { continue };
        let Some(comp) = ctx.get_nodes_in_file(&file).iter().find(|n| n.kind == NodeKind::Component).cloned() else { continue };

        // Composable-destructure map: alias → { composable, key }.
        let script = script_re.captures(&content).and_then(|m| m.text(1).map(String::from)).unwrap_or_default();
        let mut destructured: HashMap<String, (String, String)> = HashMap::new();
        for dm in destructure_re.captures_iter(&script) {
            let composable = dm.text(2).unwrap_or("");
            if !re!(r"^use[A-Z]").is_match(composable) {
                continue; // composables / hooks only
            }
            for part in dm.text(1).unwrap_or("").split(',') {
                if let Some(pm) = re!(r"^(\w+)\s*(?::\s*(\w+))?$").captures(js_trim(part)) {
                    let key = pm.text(1).unwrap_or("");
                    let alias = pm.text(2).filter(|s| !s.is_empty()).unwrap_or(key);
                    destructured.insert(alias.to_string(), (composable.to_string(), key.to_string()));
                }
            }
        }

        let mut added = 0;
        let mut add_edge = |target: Option<NodeRef>, metadata: codegraph_types::Metadata, edges: &mut Vec<Edge>| {
            let Some(target) = target else { return };
            if added >= MAX_JSX_CHILDREN || target.id == comp.id || crosses_code_boundary(comp.language, target.language) {
                return;
            }
            let by = metadata.get("synthesizedBy").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if !seen.insert(format!("{}>{}>{}", comp.id, target.id, by)) {
                return;
            }
            edges.push(calls_edge(&comp.id, &target.id, Some(comp.start_line), metadata));
            added += 1;
        };
        // Prefer a target in THIS SFC (handlers live in the same file's script). A name the SFC
        // imports from a package is the package's.
        let resolve = |name: &str, kinds: &[NodeKind], fallback: Option<NodeRef>| -> Option<NodeRef> {
            let matches: Vec<NodeRef> = ctx.get_nodes_by_name(name).iter().filter(|n| kinds.contains(&n.kind)).cloned().collect();
            let local = matches.iter().find(|n| n.file_path == file).cloned();
            if local.is_some() || imported_from_package(ctx, name, &file) {
                return local;
            }
            matches.first().cloned().or(fallback)
        };

        for m in kebab_re.captures_iter(&tpl) {
            let raw = m.text(1).unwrap_or("");
            let tag = kebab_to_pascal(raw);
            let target = resolve(&tag, &component_kinds, nuxt_components.get(&tag).cloned());
            add_edge(target, meta! { "synthesizedBy" => "jsx-render", "via" => raw }, &mut edges);
        }
        // PascalCase component tags. Try a direct name match first, then the Nuxt dir-prefixed
        // auto-import name. Built-ins match neither → no edge.
        for m in pascal_re.captures_iter(&tpl) {
            let tag = m.text(1).unwrap_or("");
            let target = resolve(tag, &component_kinds, nuxt_components.get(tag).cloned());
            add_edge(target, meta! { "synthesizedBy" => "jsx-render", "via" => tag }, &mut edges);
        }
        for m in handler_re.captures_iter(&tpl) {
            let event = m.text(1).unwrap_or("");
            let expr = js_trim(m.text(2).unwrap_or(""));
            if expr.contains("=>") || expr.starts_with('$') {
                continue; // inline arrow / $emit
            }
            // `@click="save"` names a method Vue calls with the event. A handler that is an
            // expression is the template's own code: the Vue extractor records its calls.
            let Some(name) = re!(r"^([A-Za-z_$][\w$]*)\s*(?:\(|$)").captures(expr).and_then(|c| c.text(1).map(String::from)) else {
                continue;
            };
            let direct = if name == expr { resolve(&name, &handler_kinds, None) } else { None };
            if direct.is_some() {
                add_edge(direct, meta! { "synthesizedBy" => "vue-handler", "event" => event }, &mut edges);
                continue;
            }
            // Composable-destructure handler → resolve to the composable's returned fn.
            let Some((composable_name, key)) = destructured.get(&name) else { continue };
            let composable = resolve(composable_name, &handler_kinds, None);
            // Resolve to the SPECIFIC returned member defined in the composable's file.
            let key_fn = composable.and_then(|c| {
                ctx.get_nodes_by_name(key).iter().find(|n| return_kinds.contains(&n.kind) && n.file_path == c.file_path).cloned()
            });
            if key_fn.is_some() {
                add_edge(
                    key_fn,
                    meta! { "synthesizedBy" => "vue-handler", "event" => event, "via" => composable_name.as_str() },
                    &mut edges,
                );
            }
        }
    }
    edges
}
