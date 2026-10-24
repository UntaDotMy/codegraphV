//! What every synthesis pass shares: the knobs, a `Metadata` builder, the node/edge reads a pass
//! makes, and the heuristic-edge shape. Port of the head of `src/resolution/callback-synthesizer.ts`.

use crate::ctx::{q, Ctx};
use crate::types::NodeRef;
use codegraph_types::{Edge, EdgeKind, Language, Node, NodeKind, Provenance};
use std::rc::Rc;

pub const MAX_CALLBACKS_PER_CHANNEL: usize = 40;
/// Skip events with more handlers/dispatchers than this (too generic without type info).
pub const EVENT_FANOUT_CAP: usize = 6;

/// `JS_FAMILY` — the languages the JS-targeting passes gate on.
pub const JS_FAMILY: [Language; 4] = [Language::TypeScript, Language::JavaScript, Language::Tsx, Language::Jsx];
/// The languages that can write JSX: not TypeScript's `.ts` files.
pub const JSX_FAMILY: [Language; 3] = [Language::Tsx, Language::Jsx, Language::JavaScript];

/// Build a `Metadata` map with the keys in the order written (JS object-literal order).
#[macro_export]
macro_rules! meta {
    ($($k:literal => $v:expr),* $(,)?) => {{
        #[allow(unused_mut)]
        let mut m = codegraph_types::Metadata::new();
        $( m.insert($k.to_string(), serde_json::Value::from($v)); )*
        m
    }};
}

/// A heuristic `calls`-shaped edge with the metadata a pass attaches.
pub fn heuristic_edge(
    source: &str,
    target: &str,
    kind: EdgeKind,
    line: Option<u32>,
    metadata: codegraph_types::Metadata,
) -> Edge {
    let mut e = Edge::new(source, target, kind);
    e.line = line;
    e.provenance = Some(Provenance::Heuristic);
    e.metadata = Some(metadata);
    e
}

/// A heuristic `calls` edge.
pub fn calls_edge(source: &str, target: &str, line: Option<u32>, metadata: codegraph_types::Metadata) -> Edge {
    heuristic_edge(source, target, EdgeKind::Calls, line, metadata)
}

/// `content.split('\n').slice(startLine - 1, endLine).join('\n')`, null when a bound is missing.
pub fn slice_lines(content: &str, start_line: u32, end_line: u32) -> Option<String> {
    if start_line == 0 || end_line == 0 {
        return None;
    }
    let skip = (start_line - 1) as usize;
    let take = (end_line as usize).saturating_sub(skip);
    let mut out = String::new();
    for (i, line) in content.split('\n').skip(skip).take(take).enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line);
    }
    Some(out)
}

/// `const content = ctx.readFile(n.filePath); content && sliceLines(content, n.startLine, n.endLine)`,
/// reading an empty slice as absent (every caller tests `!src`).
pub fn node_source(ctx: &Ctx, n: &Node) -> Option<String> {
    let content = ctx.read_file(&n.file_path)?;
    if content.is_empty() {
        return None;
    }
    slice_lines(&content, n.start_line, n.end_line).filter(|s| !s.is_empty())
}

/// `queries.getNodeById`.
pub fn node(ctx: &Ctx, id: &str) -> Option<NodeRef> {
    ctx.node_by_id(id)
}

/// The nodes a node CONTAINS, in edge order: `getOutgoingEdges(id, ['contains'])` → `getNodeById`.
pub fn contained_nodes(ctx: &Ctx, id: &str) -> Vec<NodeRef> {
    q("getOutgoingEdges", ctx.store().get_outgoing_edges(id, &[EdgeKind::Contains], None))
        .iter()
        .filter_map(|e| ctx.node_by_id(&e.target))
        .collect()
}

pub fn outgoing(ctx: &Ctx, id: &str, kinds: &[EdgeKind]) -> Vec<Edge> {
    q("getOutgoingEdges", ctx.store().get_outgoing_edges(id, kinds, None))
}

pub fn incoming(ctx: &Ctx, id: &str, kinds: &[EdgeKind]) -> Vec<Edge> {
    q("getIncomingEdges", ctx.store().get_incoming_edges(id, kinds))
}

/// `queries.iterateNodesByKind(kind)`, each node shared.
pub fn each_node(ctx: &Ctx, kind: NodeKind, mut visit: impl FnMut(NodeRef)) {
    let r = ctx.store().for_each_node_of_kind(kind, |n| {
        visit(Rc::new(n));
        true
    });
    q("iterateNodesByKind", r.map(|_| ()));
}

/// `queries.iterateNodesByKindIn(kind, languages)`.
pub fn each_node_in(ctx: &Ctx, kinds: &[NodeKind], languages: &[Language], mut visit: impl FnMut(NodeRef)) {
    let r = ctx.store().for_each_node_of_kind_in(kinds, languages, |n| {
        visit(Rc::new(n));
        true
    });
    q("iterateNodesByKindIn", r.map(|_| ()));
}

/// `methodAndFunctionNodes`: every method, then every function — streamed.
pub fn each_method_and_function(ctx: &Ctx, mut visit: impl FnMut(NodeRef)) {
    each_node(ctx, NodeKind::Method, &mut visit);
    each_node(ctx, NodeKind::Function, &mut visit);
}

/// 1-based line of byte offset `idx` in `text`: `text.slice(0, idx).split('\n').length`.
pub fn line_of(text: &str, idx: usize) -> u32 {
    text.as_bytes()[..idx].iter().filter(|b| **b == b'\n').count() as u32 + 1
}

/// JS `/^\d+$/.test(s)`.
pub fn is_all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// A memo that drops its oldest entry past `max`, so a whole-repo walk stays bounded.
pub struct Memo<V> {
    map: std::collections::HashMap<String, V>,
    order: std::collections::VecDeque<String>,
    max: usize,
}

impl<V: Clone> Memo<V> {
    pub fn new(max: usize) -> Self {
        Memo { map: std::collections::HashMap::new(), order: std::collections::VecDeque::new(), max }
    }
    pub fn get(&self, key: &str) -> Option<V> {
        self.map.get(key).cloned()
    }
    pub fn put(&mut self, key: String, value: V) -> V {
        if self.map.len() >= self.max {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            }
        }
        self.order.push_back(key.clone());
        self.map.insert(key, value.clone());
        value
    }
}
