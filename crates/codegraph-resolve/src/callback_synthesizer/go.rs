//! Go's structural typing for the graph: implicit interface satisfaction, cross-file receiver
//! `contains`, gRPC arities. Port of the Go half of `src/resolution/callback-synthesizer.ts`.

use super::common::*;
use crate::ctx::Ctx;
use crate::go_stdlib_methods::go_stdlib_methods;
use crate::meta;
use crate::name_matcher::go::qualify::go_ref_qualification;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_types::jscompat::js_trim;
use codegraph_types::{Edge, EdgeKind, Language, NodeKind, Provenance};
use indexmap::{IndexMap, IndexSet};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// Whether a supertype edge out of a Go type is one of its embeddings (#2397): declared, never
/// synthesized (go-implements writes those), into a struct, an interface or a defined type, and
/// out of an interface only into an interface. `kind_of` answers those three kinds for a Go node id.
pub(super) fn is_go_embedding(e: &Edge, kind_of: &dyn Fn(&str) -> Option<NodeKind>) -> bool {
    if e.provenance == Some(Provenance::Heuristic) {
        return false;
    }
    let Some(to) = kind_of(&e.target) else { return false };
    kind_of(&e.source) != Some(NodeKind::Interface) || to == NodeKind::Interface
}

/// `GO_EXPORTED_NAME` — an exported Go identifier starts with an upper-case letter, in Unicode's
/// sense.
fn go_exported(name: &str) -> bool {
    Re::cached(r"^\p{Lu}", "u").is_match(name)
}

/// What a Go method matches an interface's method by: its name, and for an unexported name the
/// package that declares it as well.
pub(super) fn go_method_key(m: &codegraph_types::Node) -> String {
    if go_exported(&m.name) {
        return m.name.clone();
    }
    let file = m.file_path.replace('\\', "/");
    let dir = &file[..file.rfind('/').unwrap_or(0)];
    format!("{dir}\0{}", m.name)
}

/// `goMethodKey` for a method of a standard-library type.
fn go_stdlib_method_key(import_path: Option<&str>, name: &str) -> String {
    if go_exported(name) {
        name.to_string()
    } else {
        format!("std:{}\0{name}", import_path.unwrap_or(""))
    }
}

/// The top-level items of the Go bracketed list opening at `text[open]` and the index where the
/// list closes. Nested brackets, string literals and comments are skipped whole. `None` when the
/// list never closes.
fn go_list_items(text: &[char], open: usize) -> Option<(Vec<String>, usize)> {
    let mut items: Vec<String> = Vec::new();
    let mut depth: i32 = 0;
    let mut item = String::new();
    let mut i = open;
    while i < text.len() {
        let c = text[i];
        let next = text.get(i + 1).copied();
        if c == '/' && (next == Some('/') || next == Some('*')) {
            let end: isize = if next == Some('/') {
                text[i..].iter().position(|ch| *ch == '\n').map_or(-1, |p| (i + p) as isize)
            } else {
                let from = i + 2;
                let found = (from..text.len().saturating_sub(1)).find(|&k| text[k] == '*' && text[k + 1] == '/');
                found.map_or(-1, |p| p as isize) + 1
            };
            if end <= 0 {
                return None;
            }
            i = end as usize;
            item.push(' ');
            i += 1;
            continue;
        }
        if c == '"' || c == '`' || c == '\'' {
            let mut j = i + 1;
            while j < text.len() && text[j] != c {
                j += if c != '`' && text[j] == '\\' { 2 } else { 1 };
            }
            if j >= text.len() {
                return None;
            }
            item.extend(&text[i..=j]);
            i = j + 1;
            continue;
        }
        if c == '(' || c == '[' || c == '{' {
            let first = depth == 0;
            depth += 1;
            if first {
                i += 1;
                continue;
            }
        } else if c == ')' || c == ']' || c == '}' {
            depth -= 1;
            if depth == 0 {
                let t = js_trim(&item);
                if !t.is_empty() {
                    items.push(t.to_string());
                }
                return Some((items, i));
            }
        } else if c == ',' && depth == 1 {
            let t = js_trim(&item);
            if !t.is_empty() {
                items.push(t.to_string());
            }
            item.clear();
            i += 1;
            continue;
        }
        item.push(c);
        i += 1;
    }
    None
}

struct GoSignature {
    params: Vec<String>,
    results: Vec<String>,
}

/// A Go signature as extraction stores it — the parameter list's text, then the result's — split
/// into its parameters and results. `None` when the text has another shape.
fn go_signature_parts(signature: Option<&str>) -> Option<GoSignature> {
    let sig = js_trim(signature?);
    let chars: Vec<char> = sig.chars().collect();
    if chars.first() != Some(&'(') {
        return None;
    }
    let (items, end) = go_list_items(&chars, 0)?;
    let rest_s: String = chars[end + 1..].iter().collect();
    let rest = js_trim(&rest_s);
    let rest_chars: Vec<char> = rest.chars().collect();
    if rest_chars.first() != Some(&'(') {
        return Some(GoSignature { params: items, results: if rest.is_empty() { vec![] } else { vec![rest.to_string()] } });
    }
    let (results, rend) = go_list_items(&rest_chars, 0)?;
    let tail: String = rest_chars[rend + 1..].iter().collect();
    if !js_trim(&tail).is_empty() {
        return None;
    }
    Some(GoSignature { params: items, results })
}

/// How many parameters and results a Go signature has, as `params/results`.
pub fn go_signature_arity(signature: Option<&str>) -> Option<String> {
    go_signature_parts(signature).map(|p| format!("{}/{}", p.params.len(), p.results.len()))
}

/// The arity a gRPC server implements an RPC with, read off the signature the generated client has
/// for it. `None` when it isn't a client's.
fn go_grpc_server_arity(signature: Option<&str>) -> Option<String> {
    let parts = go_signature_parts(signature)?;
    let opts = parts.params.last()?;
    if !re!(r"^(?:[A-Za-z_]\w*\s+)?\.\.\.\s*(?:grpc\.)?CallOption$").is_match(opts) {
        return None;
    }
    let unary = parts.results.len() == 2 && parts.results[0].starts_with('*');
    Some(format!("{}/{}", parts.params.len() - 1, if unary { 2 } else { 1 }))
}

type Arities = HashMap<String, Option<HashSet<String>>>;

/// What a Go type has, or an interface asks for.
#[derive(Clone)]
struct GoMethodNames {
    methods: Rc<HashSet<String>>,
    opaque: Rc<HashSet<String>>,
}

#[derive(Default)]
struct Outside {
    stdlib: Vec<HashMap<String, String>>,
    opaque: Vec<String>,
}

struct GoImplements<'a> {
    ctx: &'a Ctx,
    own_memo: RefCell<HashMap<String, Rc<HashSet<String>>>>,
    own_signatures: RefCell<HashMap<String, Rc<HashMap<String, Vec<Option<String>>>>>>,
    embed_memo: HashMap<String, Vec<String>>,
    outside: IndexMap<String, Outside>,
    arity_memo: RefCell<HashMap<String, Rc<Arities>>>,
}

impl GoImplements<'_> {
    fn embeds(&self, id: &str) -> &[String] {
        self.embed_memo.get(id).map_or(&[], |v| v.as_slice())
    }

    /// A type's own methods, each by its `goMethodKey`.
    fn own_methods(&self, id: &str) -> Rc<HashSet<String>> {
        if let Some(names) = self.own_memo.borrow().get(id) {
            return Rc::clone(names);
        }
        let mut names = HashSet::new();
        let mut signatures: HashMap<String, Vec<Option<String>>> = HashMap::new();
        for e in outgoing(self.ctx, id, &[EdgeKind::Contains]) {
            let Some(n) = node(self.ctx, &e.target) else { continue };
            if n.kind != NodeKind::Method {
                continue;
            }
            let key = go_method_key(&n);
            names.insert(key.clone());
            signatures.entry(key).or_default().push(n.signature.clone());
        }
        let names = Rc::new(names);
        self.own_memo.borrow_mut().insert(id.to_string(), Rc::clone(&names));
        self.own_signatures.borrow_mut().insert(id.to_string(), Rc::new(signatures));
        names
    }

    fn signatures_of(&self, id: &str) -> Rc<HashMap<String, Vec<Option<String>>>> {
        self.own_methods(id);
        Rc::clone(self.own_signatures.borrow().get(id).unwrap())
    }

    /// The type and every type it embeds, down to the last level.
    fn embed_closure(&self, id: &str) -> Vec<String> {
        if self.embeds(id).is_empty() {
            return vec![id.to_string()];
        }
        let mut reached: IndexSet<String> = IndexSet::new();
        reached.insert(id.to_string());
        let mut pending = vec![id.to_string()];
        while let Some(at) = pending.pop() {
            for t in self.embeds(&at) {
                if reached.contains(t) {
                    continue;
                }
                reached.insert(t.clone());
                pending.push(t.clone());
            }
        }
        reached.into_iter().collect()
    }

    /// Own methods plus every embedded type's, a standard-library one's too.
    fn method_names(&self, id: &str) -> GoMethodNames {
        let types = self.embed_closure(id);
        if types.len() == 1 && !self.outside.contains_key(id) {
            return GoMethodNames { methods: self.own_methods(id), opaque: Rc::new(HashSet::new()) };
        }
        let mut methods = HashSet::new();
        let mut opaque = HashSet::new();
        for t in &types {
            for m in self.own_methods(t).iter() {
                methods.insert(m.clone());
            }
            let Some(out) = self.outside.get(t) else { continue };
            for known in &out.stdlib {
                for key in known.keys() {
                    methods.insert(key.clone());
                }
            }
            for ty in &out.opaque {
                opaque.insert(ty.clone());
            }
        }
        GoMethodNames { methods: Rc::new(methods), opaque: Rc::new(opaque) }
    }

    /// Each method in a type's method set → the arities its declarations there have, or `None`
    /// when one of their signatures doesn't read.
    fn method_arities(&self, id: &str) -> Rc<Arities> {
        if let Some(a) = self.arity_memo.borrow().get(id) {
            return Rc::clone(a);
        }
        let mut arities: Arities = HashMap::new();
        for t in self.embed_closure(id) {
            let sigs = self.signatures_of(&t);
            for (name, signatures) in sigs.iter() {
                if let Some(None) = arities.get(name) {
                    continue;
                }
                arities.entry(name.clone()).or_insert_with(|| Some(HashSet::new()));
                let mut poisoned = false;
                for signature in signatures {
                    match go_signature_arity(signature.as_deref()) {
                        None => {
                            poisoned = true;
                            break;
                        }
                        Some(arity) => {
                            arities.get_mut(name).unwrap().as_mut().unwrap().insert(arity);
                        }
                    }
                }
                if poisoned {
                    arities.insert(name.clone(), None);
                }
            }
            if let Some(out) = self.outside.get(&t) {
                for methods in &out.stdlib {
                    for (name, arity) in methods {
                        if let Some(None) = arities.get(name) {
                            continue;
                        }
                        arities.entry(name.clone()).or_insert_with(|| Some(HashSet::new())).as_mut().unwrap().insert(arity.clone());
                    }
                }
            }
        }
        let arities = Rc::new(arities);
        self.arity_memo.borrow_mut().insert(id.to_string(), Rc::clone(&arities));
        arities
    }

    /// The arities an implementer may declare each of an interface's methods with: the
    /// interface's own, plus — for a gRPC client interface — its service's server side.
    fn wanted_arities(&self, iface: &NodeRef) -> Rc<Arities> {
        let own = self.method_arities(&iface.id);
        let mut wanted: Arities = HashMap::new();
        for t in self.embed_closure(&iface.id) {
            let sigs = self.signatures_of(&t);
            for (name, signatures) in sigs.iter() {
                if !wanted.contains_key(name) {
                    let start: HashSet<String> = own.get(name).cloned().flatten().unwrap_or_default();
                    wanted.insert(name.clone(), Some(start));
                }
                for signature in signatures {
                    let Some(served) = go_grpc_server_arity(signature.as_deref()) else { return own };
                    wanted.get_mut(name).unwrap().as_mut().unwrap().insert(served);
                }
            }
        }
        // What only a standard-library embedding asks for keeps the table's arity.
        for (name, arities) in own.iter() {
            if !wanted.contains_key(name) {
                wanted.insert(name.clone(), arities.clone());
            }
        }
        Rc::new(wanted)
    }

    /// Go accepts an implementation only when each method's signature is the interface's. A type
    /// with no declaration of a wanted name at a wanted arity can't satisfy it.
    fn arities_fit(&self, type_id: &str, wanted: &Arities) -> bool {
        let have = self.method_arities(type_id);
        for (name, arities) in wanted {
            let Some(arities) = arities else { continue };
            let Some(Some(got)) = have.get(name) else { continue };
            if !arities.iter().any(|a| got.contains(a)) {
                return false;
            }
        }
        true
    }
}

fn covers(have: &HashSet<String>, want: &HashSet<String>) -> bool {
    if have.len() < want.len() {
        return false;
    }
    want.iter().all(|m| have.contains(m))
}

/// Go implicit interface satisfaction (#584): a type satisfies an interface structurally when its
/// method set covers the interface's. Synthesize the missing `implements` edge by matching method
/// sets, so the interface-dispatch bridge can link a call through the interface to the concrete
/// overrides.
pub fn go_implements_edges(ctx: &Ctx) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Materializes GO types only. Structs and defined types arrive interleaved in one canonical
    // order, so the cap below takes implementers as the files declare them, whatever their kind.
    let mut go_implementers: Vec<NodeRef> = Vec::new();
    each_node_in(ctx, &[NodeKind::Struct, NodeKind::TypeAlias], &[Language::Go], |n| go_implementers.push(n));
    let mut go_interfaces: Vec<NodeRef> = Vec::new();
    each_node_in(ctx, &[NodeKind::Interface], &[Language::Go], |n| go_interfaces.push(n));

    let mut type_kind: IndexMap<String, NodeKind> = IndexMap::new();
    for n in &go_implementers {
        type_kind.insert(n.id.clone(), n.kind);
    }
    for i in &go_interfaces {
        type_kind.insert(i.id.clone(), NodeKind::Interface);
    }

    // The types one embeds: an interface embeds interfaces, a struct any of the three.
    let mut embed_memo: HashMap<String, Vec<String>> = HashMap::new();
    let type_ids: Vec<String> = type_kind.keys().cloned().collect();
    for chunk in type_ids.chunks(2000) {
        let found = crate::ctx::q(
            "getOutgoingEdgesFrom",
            ctx.store().get_outgoing_edges_from(chunk, &[EdgeKind::Extends, EdgeKind::Implements]),
        );
        for e in found {
            if !is_go_embedding(&e, &|id| type_kind.get(id).copied()) {
                continue;
            }
            embed_memo.entry(e.source).or_default().push(e.target);
        }
    }

    // The types each one embeds from outside the index, as the resolver's failed `extends` rows.
    let mut outside: IndexMap<String, Outside> = IndexMap::new();
    let refs = crate::ctx::q(
        "getUnresolvedReferencesFromNodes",
        ctx.store().get_unresolved_references_from_nodes(&type_ids, &[EdgeKind::Extends]),
    );
    for r in refs {
        let Some(file_path) = r.file_path.clone().filter(|f| !f.is_empty()) else { continue };
        if r.language != Some(Language::Go) {
            continue;
        }
        let name = r.reference_name.clone();
        let unresolved = UnresolvedRef {
            from_node_id: r.from_node_id.clone(),
            reference_name: r.reference_name.clone(),
            reference_kind: r.reference_kind,
            line: r.line,
            column: r.column,
            file_path: file_path.clone(),
            language: Language::Go,
            candidates: r.candidates.clone(),
            row_id: r.row_id,
            name_tail: None,
        };
        let q = go_ref_qualification(&unresolved, ctx);
        let (written, imported) = (q.written, q.imported);
        let import_path = imported.as_ref().map(|i| i.source.clone());
        let known = if imported.is_some() {
            go_stdlib_methods(import_path.as_deref(), &name)
        } else if written.is_some() {
            None
        } else {
            go_stdlib_methods(None, &name)
        };
        let found = outside.entry(r.from_node_id.clone()).or_default();
        if let Some(known) = known {
            found.stdlib.push(known.iter().map(|(m, arity)| (go_stdlib_method_key(import_path.as_deref(), m), arity.clone())).collect());
        } else if let Some(imp) = &imported {
            found.opaque.push(format!("{}.{}", imp.source, name));
        } else if let Some(w) = &written {
            found.opaque.push(format!("{w}.{name}"));
        } else {
            let norm = file_path.replace('\\', "/");
            let dir = &norm[..norm.rfind('/').unwrap_or(0)];
            found.opaque.push(format!("{dir}#{name}"));
        }
    }

    let st = GoImplements {
        ctx,
        own_memo: RefCell::new(HashMap::new()),
        own_signatures: RefCell::new(HashMap::new()),
        embed_memo,
        outside,
        arity_memo: RefCell::new(HashMap::new()),
    };

    // A type without a method, or an embedded type nothing describes, satisfies no interface this
    // pass looks at.
    let mut candidates: Vec<NodeRef> = Vec::new();
    let mut candidate_methods: HashMap<String, GoMethodNames> = HashMap::new();
    for s in &go_implementers {
        let have = st.method_names(&s.id);
        if have.methods.len() + have.opaque.len() == 0 {
            continue;
        }
        candidates.push(Rc::clone(s));
        candidate_methods.insert(s.id.clone(), have);
    }

    for iface in &go_interfaces {
        let want = st.method_names(&iface.id);
        if want.methods.len() + want.opaque.len() == 0 {
            continue; // empty interface (`any`) — would match everything
        }
        let mut added = 0;
        let mut link = |s: &NodeRef, added: &mut usize, edges: &mut Vec<Edge>| {
            let key = format!("{}>{}", s.id, iface.id);
            if !seen.insert(key) {
                return;
            }
            edges.push(heuristic_edge(
                &s.id,
                &iface.id,
                EdgeKind::Implements,
                Some(s.start_line),
                meta! {
                    "synthesizedBy" => "go-implements", "via" => iface.name.as_str(),
                    "registeredAt" => format!("{}:{}", s.file_path, s.start_line),
                },
            ));
            *added += 1;
        };
        // A struct that needs promoted methods waits for the types declaring all of them, every
        // defined type among them: under the cap, those are what the interface-dispatch bridge
        // links a call through the interface to.
        let mut through_embedding: Vec<NodeRef> = Vec::new();
        let mut wanted: Option<Rc<Arities>> = None;
        let mut covering = 0;
        let mut sole: Option<NodeRef> = None;
        for s in &candidates {
            if added >= MAX_CALLBACKS_PER_CHANNEL {
                break;
            }
            let have = candidate_methods.get(&s.id).unwrap();
            if !covers(&have.methods, &want.methods) {
                continue;
            }
            let w = wanted.get_or_insert_with(|| st.wanted_arities(iface));
            if !st.arities_fit(&s.id, w) {
                continue;
            }
            covering += 1;
            if st.embeds(&s.id).contains(&iface.id) {
                continue; // declared by embedding it
            }
            let covers_opaque = want.opaque.iter().all(|t| have.opaque.contains(t));
            if !covers_opaque {
                if covering == 1 {
                    sole = Some(Rc::clone(s));
                }
                continue;
            }
            let own = st.own_methods(&s.id);
            if !want.opaque.is_empty() || (!Rc::ptr_eq(&own, &have.methods) && !covers(&own, &want.methods)) {
                through_embedding.push(Rc::clone(s));
            } else {
                link(s, &mut added, &mut edges);
            }
        }
        if let Some(sole) = sole {
            if covering == 1 && !want.methods.is_empty() {
                through_embedding.push(sole);
            }
        }
        for s in &through_embedding {
            if added >= MAX_CALLBACKS_PER_CHANNEL {
                break;
            }
            link(s, &mut added, &mut edges);
        }
    }
    edges
}

/// Cross-file Go method → receiver-type `contains` edges (#583, cross-file half). A Go method's
/// receiver type is declared in the SAME PACKAGE (directory) as the method, so this is a
/// deterministic structural link: add the missing `contains` edge (no provenance, as same-file
/// extraction does).
pub fn go_cross_file_method_contains_edges(ctx: &Ctx) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let type_kinds =
        [NodeKind::Struct, NodeKind::Class, NodeKind::Interface, NodeKind::Enum, NodeKind::TypeAlias];
    let dir_of = |p: &str| -> String {
        let norm = p.replace('\\', "/");
        match norm.rfind('/') {
            Some(i) => p[..i].to_string(),
            None => String::new(),
        }
    };

    each_node_in(ctx, &[NodeKind::Method], &[Language::Go], |method| {
        // The receiver type is encoded in the method's qualifiedName as `Recv::name`.
        let qn = &method.qualified_name;
        if qn.is_empty() {
            return;
        }
        let Some(sep) = qn.rfind("::") else { return };
        if sep == 0 {
            return;
        }
        let receiver = &qn[..sep];

        // Already attached to its type (same-file case handled at extraction)?
        let has_type_parent = incoming(ctx, &method.id, &[EdgeKind::Contains])
            .iter()
            .any(|e| node(ctx, &e.source).is_some_and(|src| type_kinds.contains(&src.kind)));
        if has_type_parent {
            return;
        }

        // Find the receiver type in the SAME directory (= same Go package).
        let dir = dir_of(&method.file_path);
        let owner = ctx
            .get_nodes_by_name(receiver)
            .iter()
            .find(|n| n.language == Language::Go && type_kinds.contains(&n.kind) && dir_of(&n.file_path) == dir)
            .cloned();
        let Some(owner) = owner else { return };

        if !seen.insert(format!("{}>{}", owner.id, method.id)) {
            return;
        }
        let mut e = Edge::new(owner.id.clone(), method.id.clone(), EdgeKind::Contains);
        e.line = Some(method.start_line);
        e.metadata = Some(meta! { "synthesizedBy" => "go-method-contains" });
        edges.push(e);
    });
    edges
}
