//! CFML scope rules: a bare call in a component reaches the methods of its `extends` chain.

use super::lang_shape::RefShape;
use super::line_scope::has_no_receiver_on_line;
use super::util::*;
use crate::ctx::Ctx;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::{Node, NodeKind};
use std::collections::{HashSet, VecDeque};
use std::rc::Rc;

memo_map!(CfmlChainMemo, String, Rc<HashSet<String>>);

/// Is a method of component `method.file_path` reachable from a bare call inside a `.cfc`?
pub fn is_cfml_method_in_scope(method: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if !re!(r"\.cfc$", "i").is_match(&r.file_path) || !has_no_receiver_on_line(r, ctx) {
        return true;
    }
    cfml_chain(&r.file_path, ctx).contains(&method.file_path)
}

/// A component file and the files of every component it `extends`.
fn cfml_chain(file: &str, ctx: &Ctx) -> Rc<HashSet<String>> {
    memoized::<CfmlChainMemo>(ctx, file.to_string(), || {
        let mut chain: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = VecDeque::from([file.to_string()]);
        while let Some(f) = queue.pop_front() {
            if chain.len() >= 30 {
                break;
            }
            if !chain.insert(f.clone()) {
                continue;
            }
            let text = ctx.read_file(&f).map(|s| s.to_string()).unwrap_or_default();
            // `text.slice(0, text.search(...) >>> 0 || text.length)`: a match at 0 (or none) keeps the whole text.
            let cut = re!(r"\bfunction\b|<cffunction", "i").find(&text).map(|m| m.start()).filter(|at| *at > 0).unwrap_or(text.len());
            let head = &text[..cut];
            if let Some(ext) = re!(r#"\bextends\s*=\s*["']?([\w.\/:-]+)["']?"#, "i").captures(head).and_then(|c| c.text(1).map(String::from)) {
                queue.extend(cfml_component_files(&ext, &f, ctx));
            }
        }
        Rc::new(chain)
    })
}

/// The `.cfc` files a dotted component name (`a.b.Base`) can mean from a given file.
fn cfml_component_files(dotted: &str, from: &str, ctx: &Ctx) -> Vec<String> {
    let normalized = dotted.replace('/', ".");
    let segments: Vec<&str> = normalized.split('.').filter(|s| !s.is_empty()).collect();
    let Some(last) = segments.last() else { return Vec::new() };
    let name = last.to_lowercase();
    let mut files: Vec<String> = Vec::new();
    for n in ctx.get_nodes_by_lower_name(&name).iter() {
        if n.kind == NodeKind::Class && re!(r"\.cfc$", "i").is_match(&n.file_path) && !files.contains(&n.file_path) {
            files.push(n.file_path.clone());
        }
    }
    if files.is_empty() {
        return Vec::new();
    }
    let dir_of = |f: &str| f[..f.rfind('/').map_or(0, |i| i + 1)].to_string();
    let dir = dir_of(from);
    if segments.len() == 1 {
        let local: Vec<String> = files.iter().filter(|f| dir_of(f) == dir).cloned().collect();
        return if local.is_empty() { files } else { local };
    }
    for take in (1..=segments.len()).rev() {
        let suffix = format!("/{}.cfc", segments[segments.len() - take..].join("/").to_lowercase());
        let hits: Vec<String> = files.iter().filter(|f| format!("/{}", f.to_lowercase()).ends_with(&suffix)).cloned().collect();
        if !hits.is_empty() {
            return hits;
        }
    }
    files
}

/// The CFML contribution to [`super::lang_shape::LangShape`]: a bare call.
pub struct CfmlShape;

impl CfmlShape {
    pub fn of(r: &UnresolvedRef) -> Option<CfmlShape> {
        (is_cfml(r.language) && is_calls(r) && re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name)).then_some(CfmlShape)
    }
}

impl RefShape for CfmlShape {
    fn excludes(&self, n: &NodeRef, r: &UnresolvedRef, ctx: &Ctx) -> bool {
        n.kind == NodeKind::Method && !is_cfml_method_in_scope(n, r, ctx)
    }
}
