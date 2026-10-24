//! Interface / abstract dispatch bridges: base method → implementing override (Java, Kotlin, C#,
//! TypeScript, Swift, Scala, Go, Rust, ArkTS), Kotlin `expect`/`actual`, Go gRPC stub → impl.
//! Port of the matching section of `src/resolution/callback-synthesizer.ts`.

use super::common::*;
use super::go::{go_method_key, is_go_embedding};
use crate::ctx::{q, Ctx};
use crate::meta;
use crate::types::NodeRef;
use codegraph_jsre::re;
use codegraph_types::{Edge, EdgeKind, Language, NodeKind, Provenance};
use indexmap::{IndexMap, IndexSet};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// Languages whose static `implements`/`extends` edges bridge an interface (or abstract base)
/// method to the matching concrete-class method.
fn iface_override_lang(l: Language) -> bool {
    matches!(
        l,
        Language::Java
            | Language::Kotlin
            | Language::CSharp
            | Language::TypeScript
            | Language::JavaScript
            | Language::Swift
            | Language::Scala
            | Language::Go
            | Language::Rust
            | Language::ArkTs
    )
}

/// `expect class` is routinely fulfilled by an `actual typealias`, so the type-like kinds are
/// interchangeable across an expect/actual pair.
fn kmp_type_kind(k: NodeKind) -> bool {
    matches!(k, NodeKind::Class | NodeKind::Interface | NodeKind::Struct | NodeKind::Enum | NodeKind::TypeAlias)
}

fn kmp_kinds_compatible(a: NodeKind, b: NodeKind) -> bool {
    a == b || (kmp_type_kind(a) && kmp_type_kind(b))
}

/// Kotlin Multiplatform `expect`/`actual` linking: common declaration → each platform `actual`.
pub fn kotlin_expect_actual_edges(ctx: &Ctx) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    // SQL-side language+decorator pre-filter, streamed; the LIKE can over-match (substring), so
    // the exact decorator check stays.
    let mut actuals: Vec<NodeRef> = Vec::new();
    q(
        "iterateNodesByLanguageWithDecorator",
        ctx.store().for_each_node_by_language_with_decorator(Language::Kotlin, "actual", |n| {
            actuals.push(Rc::new(n));
            true
        }),
    );
    for act in &actuals {
        if !act.decorators.as_ref().is_some_and(|d| d.iter().any(|x| x == "actual")) {
            continue;
        }
        let mut added = 0;
        for cand in q("getNodesByQualifiedNameExact", ctx.store().get_nodes_by_qualified_name_exact(&act.qualified_name)) {
            if added >= MAX_CALLBACKS_PER_CHANNEL {
                break;
            }
            // The declaration side: same FQN + compatible kind, a different file, NOT itself an
            // `actual` (that would be a sibling platform impl, not the decl).
            if cand.language != Language::Kotlin || cand.id == act.id {
                continue;
            }
            if !kmp_kinds_compatible(cand.kind, act.kind) || cand.file_path == act.file_path {
                continue;
            }
            if cand.decorators.as_ref().is_some_and(|d| d.iter().any(|x| x == "actual")) {
                continue;
            }
            if !seen.insert(format!("{}>{}", cand.id, act.id)) {
                continue;
            }
            edges.push(calls_edge(
                &cand.id,
                &act.id,
                Some(cand.start_line),
                meta! {
                    "synthesizedBy" => "kotlin-expect-actual", "via" => act.name.as_str(),
                    "registeredAt" => format!("{}:{}", act.file_path, act.start_line),
                },
            ));
            added += 1;
        }
    }
    edges
}

/// The methods a Go struct runs for a method it does not declare, and the line of the embedding
/// they come through.
struct GoPromotion {
    methods: Vec<NodeRef>,
    line: u32,
}

/// Methods a class-like node contains, memoized: a popular base interface's list is otherwise
/// re-fetched once per implementer.
struct MethodsOf<'a> {
    ctx: &'a Ctx,
    memo: RefCell<HashMap<String, Rc<Vec<NodeRef>>>>,
}

impl MethodsOf<'_> {
    fn get(&self, class_id: &str) -> Rc<Vec<NodeRef>> {
        if let Some(hit) = self.memo.borrow().get(class_id) {
            return Rc::clone(hit);
        }
        let methods: Vec<NodeRef> =
            contained_nodes(self.ctx, class_id).into_iter().filter(|n| n.kind == NodeKind::Method).collect();
        let methods = Rc::new(methods);
        self.memo.borrow_mut().insert(class_id.to_string(), Rc::clone(&methods));
        methods
    }
}

/// Which embedded type's method a Go struct runs for a method it does not declare, picked the way
/// Go's selector picks it: breadth-first through the struct's embeddings, the shallowest depth
/// holding the method wins, and it selects nothing when it occurs more than once at that depth.
struct GoPromotions<'a> {
    ctx: &'a Ctx,
    methods_of: &'a MethodsOf<'a>,
    kinds: RefCell<HashMap<String, Option<NodeKind>>>,
    embed_memo: RefCell<HashMap<String, Rc<Vec<Edge>>>>,
    iface_memo: RefCell<HashMap<String, Rc<HashSet<String>>>>,
    memo: RefCell<HashMap<String, Option<Rc<GoPromotion>>>>,
}

impl<'a> GoPromotions<'a> {
    fn new(ctx: &'a Ctx, methods_of: &'a MethodsOf<'a>) -> Self {
        GoPromotions {
            ctx,
            methods_of,
            kinds: RefCell::new(HashMap::new()),
            embed_memo: RefCell::new(HashMap::new()),
            iface_memo: RefCell::new(HashMap::new()),
            memo: RefCell::new(HashMap::new()),
        }
    }

    fn kind_of(&self, id: &str) -> Option<NodeKind> {
        if let Some(k) = self.kinds.borrow().get(id) {
            return *k;
        }
        let kind = node(self.ctx, id).and_then(|n| {
            (n.language == Language::Go && matches!(n.kind, NodeKind::Struct | NodeKind::Interface | NodeKind::TypeAlias))
                .then_some(n.kind)
        });
        self.kinds.borrow_mut().insert(id.to_string(), kind);
        kind
    }

    fn embeds(&self, id: &str) -> Rc<Vec<Edge>> {
        if let Some(out) = self.embed_memo.borrow().get(id) {
            return Rc::clone(out);
        }
        let mut targets: HashSet<String> = HashSet::new();
        let out: Vec<Edge> = outgoing(self.ctx, id, &[EdgeKind::Extends, EdgeKind::Implements])
            .into_iter()
            .filter(|e| {
                if !is_go_embedding(e, &|n| self.kind_of(n)) || targets.contains(&e.target) {
                    return false;
                }
                targets.insert(e.target.clone());
                true
            })
            .collect();
        let out = Rc::new(out);
        self.embed_memo.borrow_mut().insert(id.to_string(), Rc::clone(&out));
        out
    }

    fn iface_methods(&self, id: &str) -> Rc<HashSet<String>> {
        if let Some(names) = self.iface_memo.borrow().get(id) {
            return Rc::clone(names);
        }
        let mut names = HashSet::new();
        let mut reached: HashSet<String> = HashSet::from([id.to_string()]);
        let mut pending = vec![id.to_string()];
        while let Some(at) = pending.pop() {
            for m in self.methods_of.get(&at).iter() {
                names.insert(go_method_key(m));
            }
            for e in self.embeds(&at).iter() {
                if !reached.insert(e.target.clone()) {
                    continue;
                }
                pending.push(e.target.clone());
            }
        }
        let names = Rc::new(names);
        self.iface_memo.borrow_mut().insert(id.to_string(), Rc::clone(&names));
        names
    }

    fn provides(&self, id: &str, key: &str) -> bool {
        if self.kind_of(id) == Some(NodeKind::Interface) {
            self.iface_methods(id).contains(key)
        } else {
            self.methods_of.get(id).iter().any(|m| go_method_key(m) == key)
        }
    }

    fn promotion(&self, struct_id: &str, key: &str) -> Option<Rc<GoPromotion>> {
        let memo_key = format!("{struct_id}>{key}");
        if let Some(hit) = self.memo.borrow().get(&memo_key) {
            return hit.clone();
        }
        let mut found: Option<Rc<GoPromotion>> = None;
        // Each type at the current depth, with how many paths reach it and the line of the
        // struct's own embedding the first of them starts with.
        let mut level: IndexMap<String, (u32, u32)> = IndexMap::new();
        level.insert(struct_id.to_string(), (1, 0));
        let mut met: HashSet<String> = HashSet::from([struct_id.to_string()]);
        let mut depth = 0;
        while !level.is_empty() {
            let mut next: IndexMap<String, (u32, u32)> = IndexMap::new();
            for (id, at) in &level {
                if self.kind_of(id) == Some(NodeKind::Interface) {
                    continue; // its embeddings are its method set
                }
                for e in self.embeds(id).iter() {
                    if met.contains(&e.target) {
                        continue;
                    }
                    if let Some(to) = next.get_mut(&e.target) {
                        to.0 += at.0;
                    } else {
                        next.insert(e.target.clone(), (at.0, if depth == 0 { e.line.unwrap_or(0) } else { at.1 }));
                    }
                }
            }
            let mut paths = 0;
            let mut provider: Option<String> = None;
            for (id, at) in &next {
                met.insert(id.clone());
                if !self.provides(id, key) {
                    continue;
                }
                paths += at.0;
                provider = Some(id.clone());
            }
            if let Some(provider) = provider {
                if paths == 1 && self.kind_of(&provider) != Some(NodeKind::Interface) {
                    found = Some(Rc::new(GoPromotion {
                        methods: self.methods_of.get(&provider).iter().filter(|m| go_method_key(m) == key).cloned().collect(),
                        line: next[&provider].1,
                    }));
                }
                break;
            }
            level = next;
            depth += 1;
        }
        self.memo.borrow_mut().insert(memo_key, found.clone());
        found
    }
}

/// Phase 5.5: interface / abstract dispatch. A call through an injected interface or an abstract
/// base dispatches at runtime to the implementing class's override — link each base/interface
/// method → the class's same-name method.
pub fn interface_override_edges(ctx: &Ctx) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let methods_of = MethodsOf { ctx, memo: RefCell::new(HashMap::new()) };
    // A Swift protocol's methods live in its extensions: requirements are not extracted as
    // methods, and `extension EventMonitor { func request(…) }` is where the default
    // implementations a conformer overrides are. A class-kind node sharing a protocol's name is
    // one of its extensions.
    let protocol_memo: RefCell<HashMap<String, Rc<Vec<NodeRef>>>> = RefCell::new(HashMap::new());
    let base_methods_of = |base: &NodeRef| -> Rc<Vec<NodeRef>> {
        if base.language != Language::Swift || base.kind != NodeKind::Interface {
            return methods_of.get(&base.id);
        }
        if let Some(hit) = protocol_memo.borrow().get(&base.id) {
            return Rc::clone(hit);
        }
        let mut methods: Vec<NodeRef> = methods_of.get(&base.id).iter().cloned().collect();
        for n in ctx.get_nodes_by_name(&base.name).iter() {
            if n.language == Language::Swift && n.kind == NodeKind::Class {
                methods.extend(methods_of.get(&n.id).iter().cloned());
            }
        }
        let methods = Rc::new(methods);
        protocol_memo.borrow_mut().insert(base.id.clone(), Rc::clone(&methods));
        methods
    };
    // A Go struct also satisfies an interface with the methods its embedded types promote into it,
    // and a call through the interface then runs the embedded type's method. Those links are made
    // after the loop, from what the struct's cap has left.
    let go_promotion = GoPromotions::new(ctx, &methods_of);
    struct Promoted {
        cls: NodeRef,
        methods: Vec<NodeRef>,
        left: usize,
    }
    let mut promoted: Vec<Promoted> = Vec::new();

    let mut visit = |cls: NodeRef| {
        // A class can only emit here if it HAS a supertype edge — check that before
        // materializing its methods.
        let sups = outgoing(ctx, &cls.id, &[EdgeKind::Implements, EdgeKind::Extends]);
        if sups.is_empty() {
            return;
        }
        let impl_methods: Vec<NodeRef> =
            methods_of.get(&cls.id).iter().filter(|n| iface_override_lang(n.language)).cloned().collect();
        // A Go struct that satisfies an interface (a synthesized edge) may do so with promoted
        // methods alone.
        let go_struct = cls.language == Language::Go && cls.kind == NodeKind::Struct;
        if impl_methods.is_empty() && !(go_struct && sups.iter().any(|s| s.provenance == Some(Provenance::Heuristic))) {
            return;
        }
        for sup in &sups {
            let Some(base) = node(ctx, &sup.target) else { continue };
            if !iface_override_lang(base.language) || base.id == cls.id {
                continue;
            }
            // Go has no inheritance: a Go type's supertype edge to a struct or a defined type is
            // an embedding, and a call on the embedded type runs its own method.
            if cls.language == Language::Go && base.kind != NodeKind::Interface {
                continue;
            }
            let promotes = go_struct && sup.provenance == Some(Provenance::Heuristic) && base.kind == NodeKind::Interface;
            // Group impl methods by name to handle OVERLOADS; a Go interface's unexported method
            // is matched in its own package only (goMethodKey).
            let go_iface = base.language == Language::Go && base.kind == NodeKind::Interface;
            let key_of = |m: &NodeRef| -> String { if go_iface { go_method_key(m) } else { m.name.clone() } };
            let mut impls_by_key: IndexMap<String, Vec<NodeRef>> = IndexMap::new();
            for m in &impl_methods {
                impls_by_key.entry(key_of(m)).or_default().push(Rc::clone(m));
            }
            let mut added = 0;
            let mut unmatched: Vec<NodeRef> = Vec::new();
            for bm in base_methods_of(&base).iter() {
                if added >= MAX_CALLBACKS_PER_CHANNEL {
                    break;
                }
                let Some(impls) = impls_by_key.get(&key_of(bm)) else {
                    if promotes {
                        unmatched.push(Rc::clone(bm));
                    }
                    continue;
                };
                for m in impls {
                    if added >= MAX_CALLBACKS_PER_CHANNEL {
                        break;
                    }
                    if bm.id == m.id {
                        continue;
                    }
                    if !seen.insert(format!("{}>{}", bm.id, m.id)) {
                        continue;
                    }
                    edges.push(calls_edge(
                        &bm.id,
                        &m.id,
                        Some(bm.start_line),
                        meta! {
                            "synthesizedBy" => "interface-impl", "via" => m.name.as_str(),
                            "registeredAt" => format!("{}:{}", m.file_path, m.start_line),
                        },
                    ));
                    added += 1;
                }
            }
            if !unmatched.is_empty() && added < MAX_CALLBACKS_PER_CHANNEL {
                promoted.push(Promoted { cls: Rc::clone(&cls), methods: unmatched, left: MAX_CALLBACKS_PER_CHANNEL - added });
            }
        }
    };
    // Concrete-side kinds vary by language: `class` covers Java / Kotlin / C# / TS / Swift-classes
    // / Scala-classes; `struct` covers Swift value types that conform to protocols. A Go defined
    // type holds its methods as a struct does, as a `type_alias`; that kind is walked for Go alone.
    for kind in [NodeKind::Class, NodeKind::Struct, NodeKind::Union] {
        each_node(ctx, kind, &mut visit);
    }
    each_node_in(ctx, &[NodeKind::TypeAlias], &[Language::Go], &mut visit);

    // The wiring site of a promoted method is the struct's embedding it comes through.
    for Promoted { cls, methods, left } in &promoted {
        let mut budget = *left as i64;
        for bm in methods {
            if budget <= 0 {
                break;
            }
            let Some(promotion) = go_promotion.promotion(&cls.id, &go_method_key(bm)) else { continue };
            for m in &promotion.methods {
                if budget <= 0 {
                    break;
                }
                if !seen.insert(format!("{}>{}", bm.id, m.id)) {
                    continue;
                }
                let line = if promotion.line != 0 { promotion.line } else { cls.start_line };
                edges.push(calls_edge(
                    &bm.id,
                    &m.id,
                    Some(bm.start_line),
                    meta! {
                        "synthesizedBy" => "interface-impl", "via" => m.name.as_str(), "promotedInto" => cls.name.as_str(),
                        "registeredAt" => format!("{}:{}", cls.file_path, line),
                    },
                ));
                budget -= 1;
            }
        }
    }
    edges
}

/// Go gRPC stub → impl bridge: each protoc-emitted `UnimplementedXxxServer` method → the
/// hand-written struct method of the same name whose method set covers the stub's.
pub fn go_grpc_stub_impl_edges(ctx: &Ctx) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let stub_re = re!(r"^Unimplemented.*Server$");
    // gRPC internal-helper methods that appear on every Unimplemented*Server; not part of the
    // service contract.
    let is_internal_marker = |n: &str| n.starts_with("mustEmbed") || n == "testEmbeddedByValue";

    // Methods directly contained by each Go struct. Built once.
    let mut method_names_by_struct: HashMap<String, HashSet<String>> = HashMap::new();
    let mut method_nodes_by_struct: HashMap<String, Vec<NodeRef>> = HashMap::new();
    let mut go_structs: Vec<NodeRef> = Vec::new();
    each_node_in(ctx, &[NodeKind::Struct], &[Language::Go], |s| {
        let ms: Vec<NodeRef> = contained_nodes(ctx, &s.id).into_iter().filter(|n| n.kind == NodeKind::Method).collect();
        method_names_by_struct.insert(s.id.clone(), ms.iter().map(|m| m.name.clone()).collect());
        method_nodes_by_struct.insert(s.id.clone(), ms);
        go_structs.push(s);
    });

    for stub in &go_structs {
        if !stub_re.is_match(&stub.name) {
            continue;
        }
        // The stub MUST live in a generated file — that's what tells us this is a protoc-emitted
        // scaffold rather than someone naming a struct `UnimplementedXxxServer` by hand.
        if !codegraph_search::is_generated_file(&stub.file_path) {
            continue;
        }
        let stub_methods: Vec<&NodeRef> = method_nodes_by_struct
            .get(&stub.id)
            .map(|v| v.iter().filter(|m| !is_internal_marker(&m.name)).collect())
            .unwrap_or_default();
        if stub_methods.is_empty() {
            continue;
        }
        for cand in &go_structs {
            if cand.id == stub.id {
                continue;
            }
            // Skip generated-file candidates — they're siblings whose method sets coincidentally
            // match.
            if codegraph_search::is_generated_file(&cand.file_path) {
                continue;
            }
            let Some(cand_names) = method_names_by_struct.get(&cand.id) else { continue };
            // Subset: every RPC method must exist on the candidate by name.
            if !stub_methods.iter().all(|m| cand_names.contains(&m.name)) {
                continue;
            }
            let empty = Vec::new();
            let cand_methods = method_nodes_by_struct.get(&cand.id).unwrap_or(&empty);
            let mut added = 0;
            for sm in &stub_methods {
                if added >= MAX_CALLBACKS_PER_CHANNEL {
                    break;
                }
                for cm in cand_methods {
                    if added >= MAX_CALLBACKS_PER_CHANNEL {
                        break;
                    }
                    if cm.name != sm.name {
                        continue;
                    }
                    if !seen.insert(format!("{}>{}", sm.id, cm.id)) {
                        continue;
                    }
                    edges.push(calls_edge(
                        &sm.id,
                        &cm.id,
                        Some(sm.start_line),
                        meta! {
                            "synthesizedBy" => "go-grpc-stub-impl", "via" => cm.name.as_str(),
                            "registeredAt" => format!("{}:{}", cm.file_path, cm.start_line),
                        },
                    ));
                    added += 1;
                }
            }
        }
    }
    edges
}

#[allow(dead_code)]
fn _unused(_: IndexSet<String>) {}
