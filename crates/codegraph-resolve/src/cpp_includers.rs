//! Who includes a C/C++ file — port of `src/resolution/cpp-includers.ts`.
//!
//! The include graph read from the `import` nodes of C and C++ files, each placed by the same
//! import resolution the include's own edge gets.

use crate::ctx::Ctx;
use crate::import_resolver::resolve_import;
use crate::types::UnresolvedRef;
use codegraph_types::{EdgeKind, Language, NodeKind};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

#[derive(Clone)]
struct Include {
    id: String,
    spec: String,
    file: String,
    language: Language,
    line: u32,
    column: u32,
}

#[derive(Default)]
struct Memo {
    by_name: Option<Rc<HashMap<String, Vec<Include>>>>,
    by_stem: Option<Rc<HashMap<String, Vec<Include>>>>,
    targets: HashMap<String, Option<String>>,
    includers: HashMap<String, Rc<HashSet<String>>>,
}

#[derive(Default)]
struct MemoCell(RefCell<Memo>);

fn is_c_family(language: Language) -> bool {
    matches!(language, Language::C | Language::Cpp)
}

fn file_name(file_path: &str) -> &str {
    let cut = [file_path.rfind('/'), file_path.rfind('\\')].into_iter().flatten().max();
    match cut {
        Some(c) => &file_path[c + 1..],
        None => file_path,
    }
}

fn stem_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    }
}

fn dir_of(file_path: &str) -> &str {
    match file_path.rfind('/') {
        Some(i) => &file_path[..=i],
        None => "",
    }
}

fn included_file(include: &Include, ctx: &Ctx, memo: &MemoCell) -> Option<String> {
    if let Some(hit) = memo.0.borrow().targets.get(&include.id) {
        return hit.clone();
    }
    let r = UnresolvedRef::new(
        include.id.clone(),
        include.spec.clone(),
        EdgeKind::Imports,
        include.line,
        include.column,
        include.file.clone(),
        include.language,
    );
    let target = resolve_import(&r, ctx).and_then(|resolved| ctx.node_by_id(&resolved.target_node_id));
    let file = target.filter(|t| t.kind == NodeKind::File).map(|t| t.file_path.clone());
    memo.0.borrow_mut().targets.insert(include.id.clone(), file.clone());
    file
}

fn includes_by_name(ctx: &Ctx, memo: &MemoCell) -> Rc<HashMap<String, Vec<Include>>> {
    if let Some(hit) = memo.0.borrow().by_name.as_ref() {
        return Rc::clone(hit);
    }
    let mut by_name: HashMap<String, Vec<Include>> = HashMap::new();
    for n in ctx.get_cpp_include_nodes() {
        if !is_c_family(n.language) {
            continue;
        }
        let name = file_name(&n.name).to_string();
        by_name.entry(name).or_default().push(Include {
            id: n.id,
            spec: n.name,
            file: n.file_path,
            language: n.language,
            line: n.start_line,
            column: n.start_column,
        });
    }
    let by_name = Rc::new(by_name);
    memo.0.borrow_mut().by_name = Some(Rc::clone(&by_name));
    by_name
}

fn includes_by_stem(ctx: &Ctx, memo: &MemoCell) -> Rc<HashMap<String, Vec<Include>>> {
    if let Some(hit) = memo.0.borrow().by_stem.as_ref() {
        return Rc::clone(hit);
    }
    let mut by_stem: HashMap<String, Vec<Include>> = HashMap::new();
    for (name, includes) in includes_by_name(ctx, memo).iter() {
        by_stem.entry(stem_of(name).to_string()).or_default().extend(includes.iter().cloned());
    }
    let by_stem = Rc::new(by_stem);
    memo.0.borrow_mut().by_stem = Some(Rc::clone(&by_stem));
    by_stem
}

/// Headers in the same directory as `file_path`, named like it (`a.cpp` → `a.h`), that it includes.
pub fn cpp_included_headers_beside(file_path: &str, ctx: &Ctx) -> Vec<String> {
    let memo = ctx.memo::<MemoCell>();
    let dir = dir_of(file_path);
    let mut found: Vec<String> = Vec::new();
    let by_stem = includes_by_stem(ctx, &memo);
    let Some(includes) = by_stem.get(stem_of(file_name(file_path))) else { return found };
    for include in includes {
        let Some(target) = included_file(include, ctx, &memo) else { continue };
        if target == file_path || found.contains(&target) || dir_of(&target) != dir {
            continue;
        }
        if cpp_includers(&target, ctx).contains(file_path) {
            found.push(target);
        }
    }
    found
}

/// The file an `import` node of C/C++ includes, when the index holds it.
pub fn cpp_included_file(include: &codegraph_types::Node, ctx: &Ctx) -> Option<String> {
    if include.kind != NodeKind::Import || !is_c_family(include.language) {
        return None;
    }
    let memo = ctx.memo::<MemoCell>();
    let inc = Include {
        id: include.id.clone(),
        spec: include.name.clone(),
        file: include.file_path.clone(),
        language: include.language,
        line: include.start_line,
        column: include.start_column,
    };
    included_file(&inc, ctx, &memo)
}

/// Every file that includes `file_path`, directly or through other headers.
pub fn cpp_includers(file_path: &str, ctx: &Ctx) -> Rc<HashSet<String>> {
    let memo = ctx.memo::<MemoCell>();
    if let Some(hit) = memo.0.borrow().includers.get(file_path) {
        return Rc::clone(hit);
    }
    let by_name = includes_by_name(ctx, &memo);
    let mut found: HashSet<String> = HashSet::new();
    let mut queue: Vec<String> = vec![file_path.to_string()];
    let mut i = 0;
    while i < queue.len() {
        let target = queue[i].clone();
        i += 1;
        let name = file_name(&target);
        let spelled: Vec<&str> = match name.rfind('.') {
            Some(dot) if dot > 0 => vec![name, &name[..dot]],
            _ => vec![name],
        };
        for spelled in spelled {
            let Some(list) = by_name.get(spelled) else { continue };
            for include in list {
                if include.file == file_path || found.contains(&include.file) {
                    continue;
                }
                if included_file(include, ctx, &memo).as_deref() != Some(target.as_str()) {
                    continue;
                }
                found.insert(include.file.clone());
                queue.push(include.file.clone());
            }
        }
    }
    let found = Rc::new(found);
    memo.0.borrow_mut().includers.insert(file_path.to_string(), Rc::clone(&found));
    found
}
