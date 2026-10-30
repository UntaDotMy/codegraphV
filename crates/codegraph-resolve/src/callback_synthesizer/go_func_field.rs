//! Go func-field dispatch synthesis. Port of `src/resolution/go-func-field-synthesizer.ts`.
//!
//! A Go struct can hold a function in a field and call it through the field:
//!
//! ```go
//! type Discovery struct { refreshf func(ctx context.Context) ([]*Group, error) }
//! func (d *Discovery) refresh(ctx context.Context) (…) { … d.refreshf(ctx) … }
//! ```
//!
//! Static extraction sees a call to `d.refreshf`, which no method answers, so a flow stops at the
//! field. This links the function making the call to each function or method value the field is
//! given, keyed by (struct, field): a composite literal (`S{F: v}` or positional), an assignment
//! `x.F = v`, and one level of field-to-field copy (`S{F: o.G}`).
//!
//! A call or an assignment counts only where its receiver's type is known: the receiver,
//! parameter or local in scope there, as the resolver's Go scope reader finds it, writes the
//! type, or the local's value gives it. Generated files are not read.
//!
//! Offsets here are bytes into the blanked source; the line / UTF-16-column pairs the node table
//! and the scope reader speak are converted at the edge.

use super::common::*;
use crate::ctx::{q, Ctx};
use crate::meta;
use crate::name_matcher::go::scope::{blank_go_code, go_local_declaration_at};
use crate::name_matcher::util::locale_compare_numeric;
use crate::name_matcher::GO_TYPE_KINDS;
use crate::synth_utils::enclosing_fn;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::{is_js_space, js_trim, utf16_byte_index, utf16_len, utf16_prefix};
use codegraph_types::{Edge, EdgeKind, Language, NodeKind};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

const IDENT: &str = r"[A-Za-z_]\w*";

/// One field of a Go struct, in declaration order.
struct GoField {
    name: String,
    /// The type as written, comments and tag dropped: `func(ctx context.Context) error`, `*refresh.Discovery`.
    ty: String,
    /// An embedded field (`*refresh.Discovery`): named for its type, and its fields are promoted.
    embedded: bool,
}

/// A struct type, by package directory and name (build-tag twins share one).
struct GoStructType {
    key: String,
    name: String,
    dir: String,
    /// Its package's name, as the `package` clause writes it.
    pkg: String,
    /// The first declaration; its file qualifies the field types.
    node: NodeRef,
    fields: Vec<GoField>,
    func_fields: HashSet<String>,
}

/// A function put in a field, and where.
#[derive(Clone)]
struct FieldValue {
    target: NodeRef,
    /// `file:line` of the wiring.
    site: String,
    test: bool,
    /// The field it was copied from, for a value that came through another field.
    via: Option<String>,
}

/// Source with comments and literal contents blanked, plus its line starts.
struct MaskedFile {
    text: String,
    line_starts: Vec<usize>,
}

/// A type as written somewhere, with the file whose package and imports qualify it.
#[derive(Clone)]
struct TypeText {
    ty: String,
    file: String,
}

/// What a type expression names: a project struct or a project interface.
#[derive(Clone)]
enum Resolved {
    Struct(Rc<GoStructType>),
    Interface(NodeRef),
}

/// What a value put in a field is.
enum ValueOutcome {
    Fn(NodeRef),
    Copy { holder: Rc<GoStructType>, field: String },
}

/// What a name resolves to where it is used: `Undefined` when the function declares no such name.
enum Lookup {
    Undefined,
    Null,
    Found(TypeText),
}

const TYPE_KEYWORDS: [&str; 5] = ["chan", "func", "map", "struct", "interface"];
const PREDECLARED_TYPES: [&str; 26] = [
    "any", "bool", "byte", "comparable", "complex64", "complex128", "error", "float32", "float64", "int", "int8", "int16", "int32", "int64", "rune", "string",
    "uint", "uint8", "uint16", "uint32", "uint64", "uintptr", "", "", "", "",
];

fn is_predeclared(name: &str) -> bool {
    !name.is_empty() && PREDECLARED_TYPES.contains(&name)
}

// ── Text helpers over masked source ───────────────────────────────────────────

fn line_starts_of(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(text.bytes().enumerate().filter(|(_, b)| *b == b'\n').map(|(i, _)| i + 1));
    starts
}

/// 1-based line of `offset`.
fn line_at(file: &MaskedFile, offset: usize) -> u32 {
    let (mut lo, mut hi) = (0usize, file.line_starts.len() - 1);
    while lo < hi {
        let mid = (lo + hi + 1) >> 1;
        if file.line_starts[mid] <= offset {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo as u32 + 1
}

/// Offset of the bracket matching the one at `open`, in masked text.
fn matching_bracket(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0;
    for (i, c) in text.bytes().enumerate().skip(open) {
        match c {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// The top-level items between the bracket at `open` and its match at `close`, in masked text,
/// each with where it starts: `{a: x, b: y}` → `a: x`, `b: y`.
fn list_items(text: &str, open: usize, close: usize) -> Vec<(String, usize)> {
    let mut items = Vec::new();
    let mut depth = 0;
    let mut start = open + 1;
    let push = |start: usize, end: usize, items: &mut Vec<(String, usize)>| {
        let raw = &text[start.min(text.len())..end.min(text.len()).max(start.min(text.len()))];
        let t = js_trim(raw);
        if !t.is_empty() {
            items.push((t.to_string(), start + raw.len() - raw.trim_start_matches(is_js_space).len()));
        }
    };
    let b = text.as_bytes();
    let mut i = open + 1;
    while i < close.min(b.len()) {
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                push(start, i, &mut items);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    push(start, close, &mut items);
    items
}

/// Where the statement from `from` ends: a line break or `;` outside brackets, or a bracket
/// closing around it.
fn statement_end(text: &str, from: usize) -> usize {
    let mut depth = 0;
    for (i, c) in text.bytes().enumerate().skip(from) {
        match c {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                if depth == 0 {
                    return i;
                }
                depth -= 1;
            }
            b'\n' | b';' if depth == 0 => return i,
            _ => {}
        }
    }
    text.len()
}

/// Each item's type in a Go result list: `(int, error)` and `(n int, err error)` give `int`,
/// `error`; a grouped `(a, b int)` gives `int` twice. A list of bare types names nothing: a named
/// list's last item always has a type.
fn go_list_types(items: &[String]) -> Vec<String> {
    let split = |it: &str| -> Option<String> {
        let t = js_trim(it);
        let m = re!(r"^([A-Za-z_]\w*)\s+(\S[\s\S]*)$").captures(t)?;
        if TYPE_KEYWORDS.contains(&m.text(1)?) {
            return None;
        }
        Some(m.text(2)?.to_string())
    };
    if items.is_empty() || split(&items[items.len() - 1]).is_none() {
        return items.iter().map(|it| js_trim(it).to_string()).collect();
    }
    let mut types: Vec<String> = Vec::new();
    let mut ty = String::new();
    for it in items.iter().rev() {
        if let Some(t) = split(it) {
            ty = js_trim(&t).to_string();
        }
        types.push(ty.clone());
    }
    types.reverse();
    types
}

/// The result types of a signature `(params) results` as extraction stores it; `None` for
/// another shape.
fn go_result_types(signature: Option<&str>) -> Option<Vec<String>> {
    let sig_owned = signature.filter(|s| !s.is_empty()).map(blank_go_code).unwrap_or_default();
    let sig = js_trim(&sig_owned);
    if !sig.starts_with('(') {
        return None;
    }
    let close = matching_bracket(sig, 0)?;
    let rest = js_trim(&sig[close + 1..]);
    if !rest.starts_with('(') {
        return Some(if rest.is_empty() { Vec::new() } else { vec![rest.to_string()] });
    }
    let end = matching_bracket(rest, 0)?;
    Some(go_list_types(&list_items(rest, 0, end).into_iter().map(|(t, _)| t).collect::<Vec<_>>()))
}

fn pkg_dir(file_path: &str) -> String {
    let f = file_path.replace('\\', "/");
    match f.rfind('/') {
        None => ".".to_string(),
        Some(slash) => f[..slash].to_string(),
    }
}

/// Production wiring before test wiring, then by site: the order a capped channel keeps its values
/// in, and which site a value put in a field at several cites.
fn wiring_order(a: &FieldValue, b: &FieldValue) -> std::cmp::Ordering {
    (a.test as i32)
        .cmp(&(b.test as i32))
        .then_with(|| locale_compare_numeric(&a.site, &b.site))
        // The last tie-break is by code unit: collation depends on the host's locale.
        .then_with(|| a.target.id.encode_utf16().cmp(b.target.id.encode_utf16()))
}

// ── The pass ──────────────────────────────────────────────────────────────────

struct Named {
    name: String,
    dir: String,
    nodes: Vec<NodeRef>,
}

struct Literal {
    s: Rc<GoStructType>,
    owner: NodeRef,
    line: u32,
    column: u32,
}

struct Dispatch {
    func: NodeRef,
    line: u32,
    channel: String,
    field: String,
}

struct Synth<'a> {
    ctx: &'a Ctx,
    masked: RefCell<Memo<Option<Rc<MaskedFile>>>>,
    by_name: RefCell<Memo<Rc<Vec<NodeRef>>>>,
    import_dirs: RefCell<Memo<Option<String>>>,
    /// type_alias node id → the type it is declared as, as written after its name.
    specs: RefCell<HashMap<String, Option<String>>>,
    func_type: RefCell<HashMap<String, bool>>,
    structs: RefCell<IndexMap<String, Rc<GoStructType>>>,
    resolve_memo: RefCell<Memo<Option<Resolved>>>,
    static_calls: RefCell<Memo<Rc<Vec<Edge>>>>,
    fn_refs: RefCell<Memo<Rc<Vec<Edge>>>>,
    direct: RefCell<IndexMap<String, Vec<FieldValue>>>,
    copies: RefCell<IndexMap<String, Vec<(String, String)>>>,
}

impl<'a> Synth<'a> {
    fn masked_file(&self, file: &str) -> Option<Rc<MaskedFile>> {
        if let Some(hit) = self.masked.borrow().get(file) {
            return hit;
        }
        let m = self.ctx.read_file(file).map(|content| {
            let text = blank_go_code(&content);
            let line_starts = line_starts_of(&text);
            Rc::new(MaskedFile { text, line_starts })
        });
        self.masked.borrow_mut().put(file.to_string(), m)
    }

    /// The byte offset of `line` / UTF-16 `column` in masked text.
    fn offset_of(&self, f: &MaskedFile, line: u32, column: u32) -> usize {
        match (line as usize).checked_sub(1).and_then(|i| f.line_starts.get(i)).copied() {
            None => f.text.len() + column as usize,
            Some(start) => {
                let end = f.line_starts.get(line as usize).copied().unwrap_or(f.text.len());
                start + utf16_byte_index(&f.text[start..end], column as usize)
            }
        }
    }

    // -- Types, by package ------------------------------------------------------

    fn nodes_named(&self, name: &str) -> Rc<Vec<NodeRef>> {
        if let Some(hit) = self.by_name.borrow().get(name) {
            return hit;
        }
        let v = Rc::new(self.ctx.get_nodes_by_name(name).iter().filter(|n| n.language == Language::Go).cloned().collect::<Vec<_>>());
        self.by_name.borrow_mut().put(name.to_string(), v)
    }

    fn import_dir(&self, qualifier: &str, file: &str) -> Option<String> {
        let key = format!("{file}\0{qualifier}");
        if let Some(hit) = self.import_dirs.borrow().get(&key) {
            return hit;
        }
        let mappings = self.ctx.get_import_mappings(file, Language::Go);
        let dir = mappings.iter().find(|i| i.local_name == qualifier).and_then(|imp| self.ctx.get_go_package_dir(&imp.source, Some(file)));
        self.import_dirs.borrow_mut().put(key, dir)
    }

    fn type_nodes(&self, name: &str, dir: &str) -> Vec<NodeRef> {
        if is_predeclared(name) {
            return Vec::new();
        }
        self.nodes_named(name).iter().filter(|n| GO_TYPE_KINDS.contains(&n.kind) && pkg_dir(&n.file_path) == dir).cloned().collect()
    }

    fn spec_type(&self, n: &NodeRef) -> Option<String> {
        self.specs.borrow().get(&n.id).cloned().flatten()
    }

    /// Whether a `type` spec declares a function type: `type HandlerFunc func(*Context)`.
    fn is_func_type_node(&self, n: &NodeRef) -> bool {
        if let Some(yes) = self.func_type.borrow().get(&n.id) {
            return *yes;
        }
        let yes = n.kind == NodeKind::TypeAlias && re!(r"^func\s*\(").is_match(&self.spec_type(n).unwrap_or_default());
        self.func_type.borrow_mut().insert(n.id.clone(), yes);
        yes
    }

    /// The project types a name written in `file` can mean: `T` in its own package, `pkg.T` in the
    /// imported one.
    fn named_types(&self, text: &str, file: &str) -> Option<Named> {
        let m = named_type_re().captures(js_trim(text))?;
        let first = m.text(1)?;
        let second = m.text(2).filter(|s| !s.is_empty());
        if second.is_none() && is_predeclared(first) {
            return None;
        }
        let dir = if second.is_some() { self.import_dir(first, file)? } else { pkg_dir(file) };
        if dir.is_empty() {
            return None;
        }
        let name = second.unwrap_or(first).to_string();
        let nodes = self.type_nodes(&name, &dir);
        Some(Named { name, dir, nodes })
    }

    /// Whether a field's declared type can be called: `func(…)…`, or a project func type (not a
    /// pointer to one).
    fn is_func_field_type(&self, ty: &str, file: &str) -> bool {
        if re!(r"^func\s*\(").is_match(ty) {
            return true;
        }
        if ty.starts_with('*') {
            return false;
        }
        match self.named_types(ty, file) {
            Some(named) => !named.nodes.is_empty() && named.nodes.iter().all(|n| self.is_func_type_node(n)),
            None => false,
        }
    }

    /// What a type written in `file` names: a project struct or interface; a defined type or alias
    /// is followed.
    fn resolve_type(&self, text: &str, file: &str, depth: u32) -> Option<Resolved> {
        let key = format!("{file}\0{text}");
        if let Some(hit) = self.resolve_memo.borrow().get(&key) {
            return hit;
        }
        let mut out: Option<Resolved> = None;
        let named = if depth <= 3 { self.named_types(text, file) } else { None };
        if let Some(named) = named {
            let s = self.structs.borrow().get(&format!("{}\0{}", named.dir, named.name)).cloned();
            let iface = named.nodes.iter().find(|t| t.kind == NodeKind::Interface).cloned();
            if let Some(s) = s {
                out = Some(Resolved::Struct(s));
            } else if let Some(iface) = iface {
                out = Some(Resolved::Interface(iface));
            } else if let Some(first) = named.nodes.first() {
                // `type A = B` and `type A B` have B's fields.
                if let Some(under) = self.spec_type(first) {
                    out = self.resolve_type(&under, &first.file_path, depth + 1);
                }
            }
        }
        self.resolve_memo.borrow_mut().put(key, out)
    }

    /// Whether `s` declares a method named `name` (selector lookup takes it over a promoted field).
    fn has_method(&self, s: &GoStructType, name: &str) -> bool {
        let qn = format!("{}::{}", s.name, name);
        self.nodes_named(name).iter().any(|n| n.kind == NodeKind::Method && n.qualified_name == qn && pkg_dir(&n.file_path) == s.dir)
    }

    /// The struct whose func field `field` a value of `s` reaches: its own, or the one embedding
    /// promotes it from.
    fn field_owner(&self, s: &Rc<GoStructType>, field: &str) -> Option<Rc<GoStructType>> {
        if s.func_fields.contains(field) {
            return Some(s.clone());
        }
        if s.fields.iter().any(|f| f.name == field) || self.has_method(s, field) {
            return None;
        }
        let mut holders: Vec<Rc<GoStructType>> = Vec::new();
        for emb in &s.fields {
            if !emb.embedded {
                continue;
            }
            if let Some(Resolved::Struct(t)) = self.resolve_type(&emb.ty, &s.node.file_path, 0) {
                if t.fields.iter().any(|f| f.name == field) {
                    holders.push(t);
                }
            }
        }
        // Two at one depth is ambiguous to Go; one must hold a func field there.
        if holders.len() == 1 && holders[0].func_fields.contains(field) {
            Some(holders.remove(0))
        } else {
            None
        }
    }

    /// A struct's fields, read from its declaration in `f`.
    fn read_fields(&self, f: &MaskedFile, n: &NodeRef) -> Option<Vec<GoField>> {
        let from = self.offset_of(f, n.start_line, n.start_column);
        let to = f.line_starts.get(n.end_line as usize).copied().unwrap_or(f.text.len());
        let window = if from <= to { &f.text[from.min(f.text.len())..to.min(f.text.len())] } else { "" };
        let kw = re!(r"\bstruct\s*\{").captures(window)?.get(0)?;
        let open = from + kw.end() - 1;
        let close = matching_bracket(&f.text, open)?;
        let mut fields: Vec<GoField> = Vec::new();
        let take = |start: usize, end: usize, fields: &mut Vec<GoField>| {
            let mut decl = js_trim(&f.text[start.min(f.text.len())..end.min(f.text.len()).max(start.min(f.text.len()))]).to_string();
            // A tag is a string literal, blanked down to its quotes.
            if let Some(quote) = decl.chars().last().filter(|c| *c == '`' || *c == '"') {
                let len = decl.len();
                let idx = if len == 1 { 0 } else { decl[..len - 1].rfind(quote).unwrap_or(0) };
                decl = js_trim(&decl[..idx]).to_string();
            }
            if decl.is_empty() {
                return;
            }
            if let Some(embedded) = named_type_re().captures(&decl) {
                let name = embedded.text(2).filter(|s| !s.is_empty()).or(embedded.text(1)).unwrap_or("").to_string();
                fields.push(GoField { name, ty: decl.clone(), embedded: true });
                return;
            }
            let Some(named) = re!(r"^([A-Za-z_]\w*(?:\s*,\s*[A-Za-z_]\w*)*)\s+(\S[\s\S]*)$").captures(&decl) else { return };
            let ty = js_trim(&re!(r"\s+", "g").replace_all_str(named.text(2).unwrap_or(""), " ")).to_string();
            for name in named.text(1).unwrap_or("").split(',') {
                fields.push(GoField { name: js_trim(name).to_string(), ty: ty.clone(), embedded: false });
            }
        };
        let mut depth = 0;
        let mut start = open + 1;
        for (i, c) in f.text.bytes().enumerate().take(close).skip(open + 1) {
            match c {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => depth -= 1,
                b'\n' | b';' if depth == 0 => {
                    take(start, i, &mut fields);
                    start = i + 1;
                }
                _ => {}
            }
        }
        take(start, close, &mut fields);
        Some(fields)
    }

    // -- Typing a name inside a function ----------------------------------------

    fn static_calls(&self, func: &NodeRef) -> Rc<Vec<Edge>> {
        if let Some(hit) = self.static_calls.borrow().get(&func.id) {
            return hit;
        }
        let edges = q("getOutgoingEdges", self.ctx.store().get_outgoing_edges(&func.id, &[EdgeKind::Calls], None));
        let v = Rc::new(edges.into_iter().filter(|e| e.provenance != Some(codegraph_types::Provenance::Heuristic)).collect::<Vec<_>>());
        self.static_calls.borrow_mut().put(func.id.clone(), v)
    }

    /// What the right side of a declaration gives its `index`th name (of `names`): a composite
    /// literal's type, `new(T)`, a type assertion, or a resolved call's result.
    fn rhs_type(&self, f: &MaskedFile, func: &NodeRef, rhs_start: usize, index: usize, names: usize) -> Option<TypeText> {
        let end = statement_end(&f.text, rhs_start);
        let rhs = &f.text[rhs_start.min(f.text.len())..end.max(rhs_start).min(f.text.len())];
        if re!(r"^\s*range\b").is_match(rhs) {
            return None;
        }
        let lead = rhs.len() - rhs.trim_start_matches(is_js_space).len();
        let wrapped = format!("({rhs})");
        let values = list_items(&wrapped, 0, rhs.len() + 1);
        let value: String;
        let mut result = index;
        if names > 1 && values.len() == names {
            value = values[index].0.clone();
            result = 0;
        } else if values.len() == 1 {
            value = values[0].0.clone();
        } else {
            return None;
        }
        let type_at = |pattern: &str| -> Option<TypeText> {
            let m = Re::cached(pattern, "").captures(&value)?;
            if result == 0 {
                Some(TypeText { ty: m.text(1)?.to_string(), file: func.file_path.clone() })
            } else {
                None
            }
        };
        let qualified = format!(r"{IDENT}(?:\s*\.\s*{IDENT})?");
        if let Some(t) = type_at(&format!(r"^&?\s*({qualified})\s*(?:\[[^\]]*\])?\s*\{{")) {
            return Some(t);
        }
        if let Some(t) = type_at(&format!(r"^new\s*\(\s*({qualified})\s*(?:\[[^\]]*\])?\s*\)$")) {
            return Some(t);
        }
        if let Some(t) = type_at(&format!(r"\.\s*\(\s*(?!type\s*\))(\*?\s*{qualified})\s*\)$")) {
            return Some(t);
        }
        // `f(…)`, `pkg.F(…)`, `x.M(…)` — the whole value one call the resolver linked.
        let call = Re::cached(&format!(r"^({IDENT}(?:\s*\.\s*{IDENT})*)\s*\("), "").captures(&value)?;
        let open = call.get(0)?.end() - 1;
        if matching_bracket(&value, open) != Some(value.len() - 1) {
            return None;
        }
        let name = re!(r"\s+", "g").replace_all_str(call.text(1)?, "");
        let line = line_at(f, rhs_start + lead);
        let mut callees: Vec<String> = Vec::new();
        for e in self.static_calls(func).iter() {
            if e.line == Some(line) && e.metadata.as_ref().and_then(|m| m.get("refName")).and_then(|v| v.as_str()) == Some(name.as_str()) && !callees.contains(&e.target) {
                callees.push(e.target.clone());
            }
        }
        if callees.len() != 1 {
            return None;
        }
        let callee = self.ctx.node_by_id(&callees[0])?;
        if callee.kind != NodeKind::Function && callee.kind != NodeKind::Method {
            return None;
        }
        let ty = go_result_types(callee.signature.as_deref())?.get(result).cloned().filter(|t| !t.is_empty())?;
        Some(TypeText { ty, file: callee.file_path.clone() })
    }

    /// What `name` is where offset `at` uses it in `func`: the receiver, parameter or local the
    /// resolver's scope reader finds in scope there, with its type — the type its declaration
    /// writes, else what its value gives it.
    fn lookup_name(&self, func: &NodeRef, f: &MaskedFile, name: &str, at: usize) -> Lookup {
        let line = line_at(f, at);
        let line_start = f.line_starts[line as usize - 1];
        let site = UnresolvedRef::new(func.id.clone(), name, EdgeKind::Calls, line, utf16_len(&f.text[line_start..at.max(line_start)]) as u32, func.file_path.clone(), Language::Go);
        let Some(decl) = go_local_declaration_at(name, &site, self.ctx) else { return Lookup::Undefined };
        if let Some(ty) = decl.ty {
            return if ty.starts_with("...") { Lookup::Null } else { Lookup::Found(TypeText { ty, file: func.file_path.clone() }) };
        }
        // `r, err := newRefresher(…)`, `var x = f()`, `v, ok := y.(*T)`: the name's place in its declaration.
        let p = self.offset_of(f, decl.line, decl.column);
        let window = utf16_prefix(&f.text[p.min(f.text.len())..], 400);
        let names_from = Re::cached(&format!(r"^{IDENT}((?:\s*,\s*{IDENT})*)\s*(?::=|=(?!=))"), "");
        let Some(from) = names_from.captures(window) else { return Lookup::Null };
        let line_begin = f.line_starts.get(decl.line as usize - 1).copied().unwrap_or(0);
        let names_before = Re::cached(&format!(r"(?:{IDENT}\s*,\s*)*$"), "");
        let before = names_before.captures(&f.text[line_begin.min(p)..p.min(f.text.len())]).and_then(|m| m.text(0).map(String::from)).unwrap_or_default();
        let index = if before.is_empty() { 0 } else { before.split(',').count() - 1 };
        let names = index + from.text(1).filter(|s| !s.is_empty()).map_or(1, |s| s.split(',').count());
        match self.rhs_type(f, func, p + from.get(0).unwrap().end(), index, names) {
            Some(t) => Lookup::Found(t),
            None => Lookup::Null,
        }
    }

    /// The type of `name` where offset `at` uses it in `func`; `None` when it can't be told.
    fn type_of_name(&self, func: &NodeRef, f: &MaskedFile, name: &str, at: usize) -> Option<TypeText> {
        match self.lookup_name(func, f, name, at) {
            Lookup::Found(t) => Some(t),
            _ => None,
        }
    }

    /// The struct or interface `x` / `x.y` has where offset `at` uses it in `func`.
    fn type_of_expr(&self, func: &NodeRef, f: &MaskedFile, expr: &str, at: usize) -> Option<Resolved> {
        let segs: Vec<&str> = expr.split('.').collect();
        let base = self.type_of_name(func, f, segs[0], at);
        let mut t = base.and_then(|b| self.resolve_type(&b.ty, &b.file, 0));
        for seg in &segs[1..] {
            let Some(Resolved::Struct(s)) = t.clone() else { return None };
            let mut holder: Rc<GoStructType> = s;
            let mut fld = holder.fields.iter().position(|x| x.name == *seg);
            if fld.is_none() {
                // Promoted from a struct embedded one level down.
                let owners: Vec<Rc<GoStructType>> = holder
                    .fields
                    .iter()
                    .filter(|x| x.embedded)
                    .filter_map(|x| match self.resolve_type(&x.ty, &holder.node.file_path, 0) {
                        Some(Resolved::Struct(r)) if r.fields.iter().any(|x| x.name == *seg) => Some(r),
                        _ => None,
                    })
                    .collect();
                if owners.len() != 1 {
                    return None;
                }
                holder = owners[0].clone();
                fld = holder.fields.iter().position(|x| x.name == *seg);
            }
            t = fld.and_then(|i| self.resolve_type(&holder.fields[i].ty, &holder.node.file_path, 0));
        }
        t
    }

    // -- Values put in fields ---------------------------------------------------

    fn fn_refs(&self, owner: &NodeRef) -> Rc<Vec<Edge>> {
        if let Some(hit) = self.fn_refs.borrow().get(&owner.id) {
            return hit;
        }
        let edges = q("getOutgoingEdges", self.ctx.store().get_outgoing_edges(&owner.id, &[EdgeKind::References], None));
        let v = Rc::new(edges.into_iter().filter(|e| e.metadata.as_ref().and_then(|m| m.get("fnRef")).and_then(|v| v.as_bool()) == Some(true)).collect::<Vec<_>>());
        self.fn_refs.borrow_mut().put(owner.id.clone(), v)
    }

    /// The one method named `member` a type declares (an interface's, or a struct's own).
    fn method_of(&self, type_node: &NodeRef, member: &str) -> Option<NodeRef> {
        let hits: Vec<NodeRef> = outgoing(self.ctx, &type_node.id, &[EdgeKind::Contains])
            .iter()
            .filter_map(|e| self.ctx.node_by_id(&e.target))
            .filter(|n| n.kind == NodeKind::Method && n.name == member)
            .collect();
        if hits.len() == 1 {
            Some(hits[0].clone())
        } else {
            None
        }
    }

    /// What a value put in a field is: a function or method, another func field's values, or
    /// nothing known.
    fn value_of(&self, owner: &NodeRef, func: Option<&NodeRef>, f: &MaskedFile, value: &str, at: usize, line: u32) -> Option<ValueOutcome> {
        let mut v = js_trim(value).to_string();
        // A conversion to a func type is the value it converts: `HandlerFunc(h.serve)`.
        let conv_re = Re::cached(&format!(r"^({IDENT}(?:\s*\.\s*{IDENT})?)\s*\("), "");
        let converted = match conv_re.captures(&v) {
            Some(conv) if matching_bracket(&v, conv.get(0).unwrap().end() - 1) == Some(v.len() - 1) => {
                let named = self.named_types(conv.text(1).unwrap_or(""), &owner.file_path)?;
                if named.nodes.is_empty() || !named.nodes.iter().all(|n| self.is_func_type_node(n)) {
                    return None;
                }
                Some(js_trim(&v[conv.get(0).unwrap().end()..v.len() - 1]).to_string())
            }
            _ => None,
        };
        if let Some(inner) = converted {
            v = inner;
        }
        if !Re::cached(&format!(r"^{IDENT}(?:\s*\.\s*{IDENT})?$"), "").is_match(&v) {
            return None;
        }
        let name = re!(r"\s+", "g").replace_all_str(&v, "");
        let dot = name.find('.');
        let base = dot.map_or(name.as_str(), |d| &name[..d]);
        let member = dot.map_or("", |d| &name[d + 1..]);
        // What `base` names in the enclosing function: a local, a parameter or the receiver —
        // else something package-level.
        let local = func.map_or(Lookup::Undefined, |func| self.lookup_name(func, f, base, at));
        // The resolver's own reading of this function value.
        let mut refs: Vec<String> = Vec::new();
        for e in self.fn_refs(owner).iter() {
            if e.line == Some(line) && e.metadata.as_ref().and_then(|m| m.get("refName")).and_then(|v| v.as_str()) == Some(name.as_str()) && !refs.contains(&e.target) {
                refs.push(e.target.clone());
            }
        }
        let reference = if refs.len() == 1 { self.ctx.node_by_id(&refs[0]) } else { None };
        let ref_fn = reference.filter(|r| r.kind == NodeKind::Function || r.kind == NodeKind::Method);
        let is_undefined = matches!(local, Lookup::Undefined);
        let Some(dot) = dot else {
            // A bare name is a package-level function: Go names no method bare, and a local holds
            // some other function.
            if !is_undefined {
                return None;
            }
            if let Some(r) = ref_fn {
                return if r.kind == NodeKind::Function { Some(ValueOutcome::Fn(r)) } else { None };
            }
            if !refs.is_empty() {
                return None;
            }
            // Extraction keeps a bare function value only when its file declares or imports it.
            let owner_dir = pkg_dir(&owner.file_path);
            let fns: Vec<NodeRef> = self.nodes_named(&name).iter().filter(|n| n.kind == NodeKind::Function && pkg_dir(&n.file_path) == owner_dir).cloned().collect();
            return if fns.len() == 1 { Some(ValueOutcome::Fn(fns[0].clone())) } else { None };
        };
        let _ = dot;
        let t = match &local {
            Lookup::Found(l) => self.resolve_type(&l.ty, &l.file, 0),
            _ => None,
        };
        match &t {
            Some(Resolved::Interface(node)) => {
                if let Some(m) = self.method_of(node, member) {
                    return Some(ValueOutcome::Fn(m));
                }
            }
            Some(Resolved::Struct(s)) => {
                // `opts.RefreshF`: another func field, whose functions this one gets.
                if let Some(holder) = self.field_owner(s, member) {
                    return Some(ValueOutcome::Copy { holder, field: member.to_string() });
                }
                let m = if ref_fn.is_some() { None } else { self.method_of(&s.node, member) };
                if let Some(m) = m {
                    return Some(ValueOutcome::Fn(m));
                }
            }
            None => {}
        }
        if let Some(r) = ref_fn {
            return Some(ValueOutcome::Fn(r));
        }
        if !is_undefined || !refs.is_empty() {
            return None;
        }
        // `pkg.Func`.
        let dir = self.import_dir(base, &owner.file_path)?;
        let fns: Vec<NodeRef> = self.nodes_named(member).iter().filter(|n| n.kind == NodeKind::Function && pkg_dir(&n.file_path) == dir).cloned().collect();
        if fns.len() == 1 {
            Some(ValueOutcome::Fn(fns[0].clone()))
        } else {
            None
        }
    }

    /// Note what a value put in `into`'s field `field` is.
    fn record(&self, into: &GoStructType, field: &str, outcome: Option<ValueOutcome>, file: &str, line: u32) {
        let Some(outcome) = outcome else { return };
        let channel = format!("{}\0{}", into.key, field);
        match outcome {
            ValueOutcome::Copy { holder, field: copied } => {
                let from = format!("{}\0{}", holder.key, copied);
                if from == channel {
                    return;
                }
                // `refreshf: opts.RefreshF` reads `Options.RefreshF`; another package's is
                // `rest.Config.WrapTransport`.
                let label = format!("{}{}.{}", if holder.dir == into.dir { String::new() } else { format!("{}.", holder.pkg) }, holder.name, copied);
                let mut copies = self.copies.borrow_mut();
                let list = copies.entry(channel).or_default();
                if !list.iter().any(|c| c.0 == from) {
                    list.push((from, label));
                }
            }
            ValueOutcome::Fn(target) => {
                let value = FieldValue { target, site: format!("{file}:{line}"), test: file.ends_with("_test.go"), via: None };
                let mut direct = self.direct.borrow_mut();
                let list = direct.entry(channel).or_default();
                match list.iter().position(|v| v.target.id == value.target.id) {
                    None => list.push(value),
                    Some(had) => {
                        if wiring_order(&value, &list[had]).is_lt() {
                            list[had] = value;
                        }
                    }
                }
            }
        }
    }

    /// The functions a struct literal puts in func fields: `S{F: v}`, or positional `S{a, v}`.
    fn read_literal(&self, f: &MaskedFile, lit: &Literal) {
        let (s, owner) = (&lit.s, &lit.owner);
        // The literal's `{` is on its type's line: Go puts no line break between them.
        let line_start = f.line_starts.get(lit.line as usize - 1).copied().unwrap_or(0);
        let line_end = f.line_starts.get(lit.line as usize).copied().unwrap_or(f.text.len());
        let line_text = &f.text[line_start..line_end.max(line_start)];
        let pattern = format!(r"(?<![\w.])(?:({IDENT})\s*\.\s*)?{}\s*(?:\[[^\]{{}}]*\])?\s*\{{", escape_regex(&s.name));
        let mut open: Option<usize> = None;
        let mut nearest = usize::MAX;
        for m in Re::cached(&pattern, "g").captures_iter(line_text) {
            // The type as written must name this struct's package: `refresh.Options` through its
            // import, a bare name its own.
            let dir = match m.text(1).filter(|q| !q.is_empty()) {
                Some(q) => self.import_dir(q, &owner.file_path),
                None => Some(pkg_dir(&owner.file_path)),
            };
            if dir.as_deref() != Some(s.dir.as_str()) {
                continue;
            }
            let whole = m.get(0).unwrap();
            let index16 = utf16_len(&line_text[..whole.start()]);
            let distance = index16.abs_diff(lit.column as usize);
            if distance < nearest {
                nearest = distance;
                open = Some(line_start + whole.end() - 1);
            }
        }
        let Some(open) = open else { return };
        let Some(close) = matching_bracket(&f.text, open) else { return };
        let func = if owner.kind == NodeKind::Function || owner.kind == NodeKind::Method { Some(owner) } else { None };
        let items = list_items(&f.text, open, close);
        let key_re = re!(r"^([A-Za-z_]\w*)\s*:(?!=)\s*");
        let keys: Vec<Option<(String, usize)>> =
            items.iter().map(|it| key_re.captures(&it.0).map(|k| (k.text(1).unwrap_or("").to_string(), k.get(0).unwrap().end()))).collect();
        let mut fields_at: Vec<(String, String, usize)> = Vec::new();
        if keys.iter().all(|k| k.is_some()) {
            for (it, k) in items.iter().zip(&keys) {
                let (field, len) = k.as_ref().unwrap();
                fields_at.push((field.clone(), it.0[*len..].to_string(), it.1 + len));
            }
        } else if keys.iter().all(|k| k.is_none()) && items.len() == s.fields.len() {
            for (it, fld) in items.iter().zip(&s.fields) {
                fields_at.push((fld.name.clone(), it.0.clone(), it.1));
            }
        }
        for (field, value, at) in fields_at {
            if !s.func_fields.contains(&field) {
                continue;
            }
            let line = line_at(f, at);
            self.record(s, &field, self.value_of(owner, func, f, &value, at, line), &owner.file_path, line);
        }
    }

    fn values_of(&self, channel: &str, memo: &mut HashMap<String, Rc<Vec<FieldValue>>>) -> Rc<Vec<FieldValue>> {
        if let Some(out) = memo.get(channel) {
            return out.clone();
        }
        let mut by_target: IndexMap<String, FieldValue> = IndexMap::new();
        let add = |by_target: &mut IndexMap<String, FieldValue>, v: FieldValue| match by_target.get(&v.target.id) {
            Some(had) if wiring_order(&v, had).is_ge() => {}
            _ => {
                by_target.insert(v.target.id.clone(), v);
            }
        };
        let direct = self.direct.borrow();
        for v in direct.get(channel).into_iter().flatten() {
            add(&mut by_target, v.clone());
        }
        for (from, label) in self.copies.borrow().get(channel).into_iter().flatten() {
            for v in direct.get(from).into_iter().flatten() {
                add(&mut by_target, FieldValue { via: Some(label.clone()), ..v.clone() });
            }
        }
        let mut out: Vec<FieldValue> = by_target.into_values().collect();
        out.sort_by(wiring_order);
        out.truncate(MAX_CALLBACKS_PER_CHANNEL);
        let out = Rc::new(out);
        memo.insert(channel.to_string(), out.clone());
        out
    }
}

/// A type written as a name: `T`, `*T`, `pkg.T`, `T[int]`.
fn named_type_re() -> std::sync::Arc<Re> {
    Re::cached(&format!(r"^[*&\s]*({IDENT})(?:\s*\.\s*({IDENT}))?\s*(?:\[[\s\S]*\])?\s*$"), "")
}

/// Edges from each Go function that calls through a func-typed struct field to the functions the
/// field is given.
pub fn go_func_field_edges(ctx: &Ctx) -> Vec<Edge> {
    let synth = Synth {
        ctx,
        masked: RefCell::new(Memo::new(128)),
        by_name: RefCell::new(Memo::new(8192)),
        import_dirs: RefCell::new(Memo::new(65536)),
        specs: RefCell::new(HashMap::new()),
        func_type: RefCell::new(HashMap::new()),
        structs: RefCell::new(IndexMap::new()),
        resolve_memo: RefCell::new(Memo::new(8192)),
        static_calls: RefCell::new(Memo::new(256)),
        fn_refs: RefCell::new(Memo::new(256)),
        direct: RefCell::new(IndexMap::new()),
        copies: RefCell::new(IndexMap::new()),
    };

    // Generated code — protobuf messages, deep copies, clientsets, mocks — is left out: it is no
    // flow a reader follows.
    let go_files: Vec<String> = ctx.get_all_files().into_iter().filter(|f| f.ends_with(".go")).collect();
    let flagged = q("getGeneratedPathsAmong", ctx.store().get_generated_paths_among(&go_files));
    let is_generated = |file: &str| flagged.contains(file) || codegraph_search::is_generated_file(file);

    // Each file declaring a struct or another type is read once: its structs' fields, its specs'
    // types, and the names it uses as `.F(` / `.F =`, so the site scan below reads it again only
    // when one of those is a func field's.
    let mut decls_by_file: IndexMap<String, Vec<NodeRef>> = IndexMap::new();
    for kind in [NodeKind::Struct, NodeKind::TypeAlias] {
        each_node_in(ctx, &[kind], &[Language::Go], |n| {
            if is_generated(&n.file_path) {
                return;
            }
            decls_by_file.entry(n.file_path.clone()).or_default().push(n);
        });
    }
    let mut raw_structs: IndexMap<String, GoStructType> = IndexMap::new(); // `${dir}\0${name}`
    let mut selector_names: HashMap<String, String> = HashMap::new(); // file → ` F G … `
    let selector_use = re!(r"\.\s*([A-Za-z_]\w*)\s*(?:\(|=(?!=))", "g");
    for (file, decls) in &decls_by_file {
        let Some(f) = synth.masked_file(file) else { continue };
        let dir = pkg_dir(file);
        let pkg = re!(r"^\s*package\s+([A-Za-z_]\w*)", "m")
            .captures(&f.text)
            .and_then(|m| m.text(1).map(String::from))
            .unwrap_or_else(|| dir[dir.rfind('/').map_or(0, |s| s + 1)..].to_string());
        // The packages the types written here come through, looked up while the file's imports
        // are at hand rather than after it has left every cache.
        let qualifiers = |ty: Option<&str>| {
            if let Some(q) = ty.and_then(|t| named_type_re().captures(t)) {
                if let (Some(a), Some(b)) = (q.text(1), q.text(2)) {
                    if !b.is_empty() {
                        synth.import_dir(a, file);
                    }
                }
            }
        };
        for n in decls {
            if n.kind == NodeKind::TypeAlias {
                let at = synth.offset_of(&f, n.start_line, n.start_column);
                let window = utf16_prefix(&f.text[at.min(f.text.len())..], 400);
                let spec = Re::cached(&format!(r"^{IDENT}\s*(?:\[[^\]]*\])?\s*=?\s*([^\n;]+)"), "")
                    .captures(window)
                    .and_then(|m| m.text(1).map(|s| js_trim(s).to_string()));
                synth.specs.borrow_mut().insert(n.id.clone(), spec.clone());
                qualifiers(spec.as_deref());
                continue;
            }
            let key = format!("{dir}\0{}", n.name);
            if raw_structs.contains_key(&key) {
                continue; // a build-tag twin: the first declaration speaks for both
            }
            let Some(fields) = synth.read_fields(&f, n) else { continue };
            for fld in &fields {
                qualifiers(Some(&fld.ty));
            }
            raw_structs.insert(
                key.clone(),
                GoStructType { key, name: n.name.clone(), dir: dir.clone(), pkg: pkg.clone(), node: n.clone(), fields, func_fields: HashSet::new() },
            );
        }
        let mut used: IndexMap<String, ()> = IndexMap::new();
        for m in selector_use.captures_iter(&f.text) {
            used.insert(m.text(1).unwrap_or("").to_string(), ());
        }
        selector_names.insert(file.clone(), format!(" {} ", used.keys().cloned().collect::<Vec<_>>().join(" ")));
    }
    let mut func_field_names: HashSet<String> = HashSet::new();
    for s in raw_structs.values_mut() {
        let mut found: Vec<String> = Vec::new();
        for fld in &s.fields {
            if fld.embedded || !synth.is_func_field_type(&fld.ty, &s.node.file_path) {
                continue;
            }
            found.push(fld.name.clone());
        }
        for name in found {
            s.func_fields.insert(name.clone());
            func_field_names.insert(name);
        }
    }
    if func_field_names.is_empty() {
        return Vec::new();
    }
    *synth.structs.borrow_mut() = raw_structs.into_iter().map(|(k, v)| (k, Rc::new(v))).collect();

    // Composite literals of the structs with func fields, by the file they're in.
    let mut literals_by_file: IndexMap<String, Vec<Literal>> = IndexMap::new();
    let structs: Vec<Rc<GoStructType>> = synth.structs.borrow().values().cloned().collect();
    for s in &structs {
        if s.func_fields.is_empty() {
            continue;
        }
        for decl in synth.type_nodes(&s.name, &s.dir) {
            if decl.kind != NodeKind::Struct {
                continue;
            }
            for e in incoming(ctx, &decl.id, &[EdgeKind::Instantiates]) {
                let Some(line) = e.line.filter(|l| *l != 0) else { continue };
                let Some(owner) = ctx.node_by_id(&e.source) else { continue };
                if owner.language != Language::Go {
                    continue;
                }
                literals_by_file.entry(owner.file_path.clone()).or_default().push(Literal { s: s.clone(), owner, line, column: e.column.unwrap_or(0) });
            }
        }
    }

    // Each Go file once: its literals, its calls through a field, its assignments to one.
    let selector_call = Re::cached(&format!(r"(?<![\w.])({IDENT}(?:\s*\.\s*{IDENT})?)\s*\.\s*({IDENT})\s*\("), "g");
    let selector_assign = Re::cached(&format!(r"(?<![\w.])({IDENT}(?:\s*\.\s*{IDENT})?)\s*\.\s*({IDENT})\s*=(?!=)"), "g");
    let mut dispatches: Vec<Dispatch> = Vec::new();
    for file in &go_files {
        if is_generated(file) {
            continue;
        }
        let literals = literals_by_file.get(file);
        let mut mentions = false;
        if let Some(used) = selector_names.get(file) {
            mentions = used.split(' ').any(|name| func_field_names.contains(name));
        } else {
            let Some(raw) = ctx.read_file(file).filter(|r| !r.is_empty()) else { continue };
            for m in selector_use.captures_iter(&raw) {
                if func_field_names.contains(m.text(1).unwrap_or("")) {
                    mentions = true;
                    break;
                }
            }
        }
        if !mentions && literals.is_none() {
            continue;
        }
        let Some(f) = synth.masked_file(file) else { continue };
        for lit in literals.into_iter().flatten() {
            synth.read_literal(&f, lit);
        }
        if !mentions {
            continue;
        }
        let nodes = ctx.get_nodes_in_file(file);
        for m in selector_call.captures_iter(&f.text) {
            let field = m.text(2).unwrap_or("");
            if !func_field_names.contains(field) {
                continue;
            }
            let whole = m.get(0).unwrap();
            let line = line_at(&f, whole.start());
            let Some(func) = enclosing_fn(&nodes, line) else { continue };
            if func.language != Language::Go {
                continue;
            }
            let expr = re!(r"\s+", "g").replace_all_str(m.text(1).unwrap_or(""), "");
            let holder = match synth.type_of_expr(&func, &f, &expr, whole.start()) {
                Some(Resolved::Struct(s)) => synth.field_owner(&s, field),
                _ => None,
            };
            if let Some(holder) = holder {
                dispatches.push(Dispatch { func, line, channel: format!("{}\0{}", holder.key, field), field: format!("{}.{}", holder.name, field) });
            }
        }
        for m in selector_assign.captures_iter(&f.text) {
            let field = m.text(2).unwrap_or("");
            if !func_field_names.contains(field) {
                continue;
            }
            let whole = m.get(0).unwrap();
            let Some(func) = enclosing_fn(&nodes, line_at(&f, whole.start())) else { continue };
            if func.language != Language::Go {
                continue;
            }
            let expr = re!(r"\s+", "g").replace_all_str(m.text(1).unwrap_or(""), "");
            let holder = match synth.type_of_expr(&func, &f, &expr, whole.start()) {
                Some(Resolved::Struct(s)) => synth.field_owner(&s, field),
                _ => None,
            };
            let Some(holder) = holder else { continue };
            let value_start = whole.end();
            let value = &f.text[value_start..statement_end(&f.text, value_start).max(value_start)];
            let at = value_start + value.len() - value.trim_start_matches(is_js_space).len();
            let line = line_at(&f, at);
            synth.record(&holder, field, synth.value_of(&func, Some(&func), &f, value, at, line), file, line);
        }
    }

    // Edges.
    let mut values_memo: HashMap<String, Rc<Vec<FieldValue>>> = HashMap::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for d in &dispatches {
        let test_call = d.func.file_path.ends_with("_test.go");
        for v in synth.values_of(&d.channel, &mut values_memo).iter() {
            if v.target.id == d.func.id {
                continue;
            }
            // Each package's tests build a binary of their own: what one package's test puts in a
            // field, another package's test never calls.
            let site_file = &v.site[..v.site.rfind(':').unwrap_or(v.site.len())];
            if test_call && v.test && pkg_dir(site_file) != pkg_dir(&d.func.file_path) {
                continue;
            }
            if !seen.insert(format!("{}>{}", d.func.id, v.target.id)) {
                continue;
            }
            let mut md = meta! { "synthesizedBy" => "go-func-field", "field" => d.field.as_str() };
            if let Some(via) = v.via.as_ref().filter(|s| !s.is_empty()) {
                md.insert("via".to_string(), via.as_str().into());
            }
            md.insert("registeredAt".to_string(), v.site.as_str().into());
            edges.push(calls_edge(&d.func.id, &v.target.id, Some(d.line), md));
        }
    }
    edges
}
