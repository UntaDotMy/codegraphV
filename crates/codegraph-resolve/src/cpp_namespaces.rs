//! Namespaces C++ headers open and close through macros — port of
//! `src/resolution/cpp-namespaces.ts`.
//!
//! `#define NS_OPEN namespace a { namespace b {` / `#define NS_CLOSE } }` make a line of the
//! form `NS_OPEN` open `a::b` until the matching closer. The frames map a line range of a file
//! to the namespace path the macros put it in.

use crate::ctx::{split_lines, Ctx};
use codegraph_extract::strip_comments::strip_comments_for_regex;
use codegraph_jsre::{re, Re};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// A line range `[start, end]` (1-based, inclusive) opened by a namespace macro.
#[derive(Debug, Clone, PartialEq)]
pub struct NamespaceFrame {
    pub start: usize,
    pub end: usize,
    pub path: Vec<String>,
}

#[derive(Default)]
pub struct NamespaceMacros {
    pub openers: IndexMap<String, Vec<String>>,
    pub opener_fns: HashSet<String>,
    pub closers: HashMap<String, usize>,
    pub aliases: IndexMap<String, String>,
}

#[derive(Default)]
struct MacrosMemo(RefCell<Option<Rc<NamespaceMacros>>>);
#[derive(Default)]
struct FramesMemo(RefCell<HashMap<String, Rc<Vec<NamespaceFrame>>>>);
#[derive(Default)]
struct AliasesMemo(RefCell<Option<Rc<IndexMap<String, String>>>>);

fn is_closer_body(body: &str) -> bool {
    re!(r"^(?:[A-Za-z_]\w*\s+)*\}(?:\s*\})*\s*;?$").is_match(body)
}

/// `namespace x = a::b;` aliases written in any C/C++ file (the first wins).
pub fn cpp_namespace_aliases(ctx: &Ctx) -> Rc<IndexMap<String, String>> {
    let memo = ctx.memo::<AliasesMemo>();
    if let Some(hit) = memo.0.borrow().as_ref() {
        return Rc::clone(hit);
    }
    let mut aliases: IndexMap<String, String> = IndexMap::new();
    for file in ctx.get_all_files() {
        if !re!(r"\.(?:h|hh|hpp|hxx|inl|c|cc|cpp|cxx)$", "i").is_match(&file) {
            continue;
        }
        let Some(source) = ctx.read_file(&file) else { continue };
        if source.is_empty() || !source.contains("namespace") {
            continue;
        }
        for m in re!(r"^[ \t]*namespace[ \t]+([A-Za-z_]\w*)[ \t]*=[ \t]*(?:::)?([A-Za-z_][\w:]*)[ \t]*;", "gm").captures_iter(&source) {
            let name = m.text(1).unwrap_or("");
            if !aliases.contains_key(name) {
                aliases.insert(name.to_string(), m.text(2).unwrap_or("").to_string());
            }
        }
    }
    let aliases = Rc::new(aliases);
    *memo.0.borrow_mut() = Some(Rc::clone(&aliases));
    aliases
}

fn cpp_namespace_macros(ctx: &Ctx) -> Rc<NamespaceMacros> {
    let memo = ctx.memo::<MacrosMemo>();
    if let Some(hit) = memo.0.borrow().as_ref() {
        return Rc::clone(hit);
    }
    let mut openers: IndexMap<String, Vec<String>> = IndexMap::new();
    let mut opener_fns: HashSet<String> = HashSet::new();
    let mut closers: HashMap<String, usize> = HashMap::new();
    let mut aliases: IndexMap<String, String> = IndexMap::new();
    let mut bodies: Vec<(String, String)> = Vec::new();
    for file in ctx.get_all_files() {
        if !re!(r"\.(?:h|hh|hpp|hxx|h\+\+|inl|ipp|tcc)$", "i").is_match(&file) {
            continue;
        }
        let Some(raw) = ctx.read_file(&file) else { continue };
        if raw.is_empty() || !raw.contains('#') || !raw.contains("define") {
            continue;
        }
        let folded = re!(r"\\\r?\n", "g").replace_all_str(&raw, " ");
        let source = strip_comments_for_regex(&folded, "cpp");
        for m in re!(r"^[ \t]*#[ \t]*define[ \t]+([A-Za-z_]\w*)(\(\s*([A-Za-z_]\w*)?\s*\))?[ \t]+([^\n]*)$", "gm").captures_iter(&source) {
            let name = m.text(1).unwrap_or("");
            let body = m.text(4).unwrap_or("").trim().to_string();
            if m.text(2).is_some() {
                if let Some(param) = m.text(3).filter(|p| !p.is_empty()) {
                    if Re::cached(&format!(r"^namespace\s+{param}\s*\{{[\w\s]*$"), "").is_match(&body) {
                        opener_fns.insert(name.to_string());
                        continue;
                    }
                }
                if is_closer_body(&body) {
                    closers.insert(name.to_string(), body.matches('}').count());
                }
                continue;
            }
            if re!(r"^[A-Za-z_]\w*$").is_match(&body) {
                aliases.insert(name.to_string(), body);
            } else if is_closer_body(&body) {
                closers.insert(name.to_string(), body.matches('}').count());
            } else if re!(r"^(?:inline\s+namespace\s+[^{}]*\{\s*|namespace\s+[A-Za-z_]\w*\s*\{\s*)+[\w\s]*$").is_match(&body) {
                bodies.push((name.to_string(), body));
            }
        }
    }
    for (name, body) in bodies {
        if openers.contains_key(&name) {
            continue;
        }
        let stripped = re!(r"inline\s+namespace\s+[^{}]*\{", "g").replace_all_str(&body, "");
        let path: Vec<String> = re!(r"namespace\s+([A-Za-z_]\w*)", "g")
            .captures_iter(&stripped)
            .map(|m| {
                let n = m.text(1).unwrap_or("");
                aliases.get(n).cloned().unwrap_or_else(|| n.to_string())
            })
            .collect();
        if !path.is_empty() {
            openers.insert(name, path);
        }
    }
    let macros = Rc::new(NamespaceMacros { openers, opener_fns, closers, aliases });
    *memo.0.borrow_mut() = Some(Rc::clone(&macros));
    macros
}

/// The outermost namespace name of every opener macro.
pub fn cpp_macro_namespace_heads(ctx: &Ctx) -> Vec<String> {
    cpp_namespace_macros(ctx).openers.values().map(|p| p[0].clone()).collect()
}

/// `#define NAME other` object-like aliases of a bare name.
pub fn cpp_name_macros(ctx: &Ctx) -> Rc<NamespaceMacros> {
    cpp_namespace_macros(ctx)
}

/// The line ranges of `file` that namespace macros open.
pub fn cpp_macro_namespace_frames(file: &str, ctx: &Ctx) -> Rc<Vec<NamespaceFrame>> {
    let memo = ctx.memo::<FramesMemo>();
    if let Some(hit) = memo.0.borrow().get(file) {
        return Rc::clone(hit);
    }
    let mut frames: Vec<NamespaceFrame> = Vec::new();
    let macros = cpp_namespace_macros(ctx);
    if !macros.openers.is_empty() || !macros.opener_fns.is_empty() {
        let lines: Vec<String> = match ctx.get_file_lines(file) {
            Some(l) => l.as_ref().clone(),
            None => ctx.read_file(file).map(|s| split_lines(&s)).unwrap_or_default(),
        };
        let mut open: Vec<(usize, Vec<String>)> = Vec::new();
        for (i, text) in lines.iter().enumerate() {
            let Some(m) = re!(r"^[ \t]*([A-Z_][A-Z0-9_]*)(?:\(\s*([A-Za-z_]\w*)?\s*\))?[ \t]*;?[ \t]*(?:\/\/.*|\/\*.*\*\/[ \t]*)?\r?$").captures(text) else {
                continue;
            };
            let Some(token) = m.text(1).filter(|t| !t.is_empty()) else { continue };
            let arg = m.text(2);
            let path: Option<Vec<String>> = match arg {
                Some(a) if macros.opener_fns.contains(token) => Some(vec![macros.aliases.get(a).cloned().unwrap_or_else(|| a.to_string())]),
                None => macros.openers.get(token).cloned(),
                _ => None,
            };
            if let Some(path) = path {
                open.push((i + 1, path));
            } else if macros.closers.contains_key(token) && !open.is_empty() {
                let (start, path) = open.pop().unwrap();
                frames.push(NamespaceFrame { start, end: i + 1, path });
            }
        }
        for (start, path) in open {
            frames.push(NamespaceFrame { start, end: lines.len(), path });
        }
    }
    let frames = Rc::new(frames);
    memo.0.borrow_mut().insert(file.to_string(), Rc::clone(&frames));
    frames
}
