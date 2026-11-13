//! A parameter's class from its call sites. Port of `src/resolution/parameter-flow.ts` (upstream
//! 483a0988), less the incremental record a sync keeps: codegraphV runs the whole pass after a
//! full index and on a sync that touches Ruby.
//!
//! `table.foreign_key` in rails' `def add_to(table)` says nothing of `table`, and a guess by the
//! receiver's word takes whatever class is called `Table`. But `add_to` has one caller, and that
//! caller passes `self`. A call on a parameter of the calling method, when every call site in the
//! project passes a value of one known class, is a call on that class.
//!
//! The pass runs after the resolver's own, on what they left: it reads the calls edges and the
//! failed references, works out the flows, and moves only the calls a flow answers.
//!
//! Exact or nothing. Any call site that can't be typed, two that disagree, or anything in the
//! project that could be a call the graph never linked gives no answer.
//!
//! What a pass decides is a function of the graph without its own edges: a call a flow answered
//! counts as the unknown it was, so a pass over the graph as it stands comes to a full index's
//! answer, and an edge whose flow is gone goes back through normal resolution.

use crate::ctx::Ctx;
use crate::ruby_parameter_flow::RubyFlow;
use crate::types::{ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_db::parameter_flow_queries::{CallEdgeRow, CallEdgesOf, FailedCallRow, FailedCallsOf, NodeOutline};
use codegraph_db::Store;
use codegraph_types::{EdgeKind, Language, Metadata, NodeKind, ReferenceKind, UnresolvedReference};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

/// A value whose class is known exactly.
#[derive(Debug, Clone, PartialEq)]
pub struct FlowValue {
    /// The class's qualified name, or the name as written when the project doesn't declare it.
    pub ty: String,
    /// The class itself rather than an instance of it.
    pub is_class: bool,
    /// Declared nowhere in the project, so it has no project method.
    pub outside: bool,
}

/// A value, or why there is none.
pub type Answer = Result<FlowValue, &'static str>;

/// One parameter a value can flow into.
#[derive(Debug, Clone, PartialEq)]
pub struct FlowParameter {
    pub name: String,
    /// Its place among the positional parameters; -1 for a keyword.
    pub position: i32,
}

/// A method or function, as far as the pass reads it.
pub type FlowMethod = NodeOutline;

/// A call the project makes.
#[derive(Debug, Clone)]
pub struct FlowSite {
    pub caller_id: String,
    pub file_path: String,
    pub line: u32,
    pub column: u32,
    pub ref_name: String,
    /// What it resolved to; `None` for a call that failed.
    pub target_id: Option<String>,
    pub confidence: f64,
    /// The edge that holds it, or the failed row.
    pub edge_id: Option<i64>,
    pub row_id: Option<i64>,
    /// A parameter flow answered it: its edge says so, or its failed row was once a guess a flow
    /// ruled out.
    pub via_flow: bool,
    /// The method it calls, and its receiver as written.
    pub method: String,
    pub receiver: Option<String>,
}

impl FlowSite {
    /// The call as written, wherever the graph holds it.
    pub fn key(&self) -> String {
        format!("{}\0{}\0{}\0{}", self.file_path, self.line, self.column, self.ref_name)
    }

    /// What holds it: its edge or its failed row.
    fn hold_key(&self) -> String {
        match self.edge_id {
            Some(id) => format!("e{id}"),
            None => format!("r{}", self.row_id.unwrap_or(-1)),
        }
    }

    fn reference(&self, language: Language) -> UnresolvedReference {
        UnresolvedReference {
            from_node_id: self.caller_id.clone(),
            reference_name: self.ref_name.clone(),
            reference_kind: ReferenceKind::Edge(EdgeKind::Calls),
            line: self.line,
            column: self.column,
            file_path: Some(self.file_path.clone()),
            language: Some(language),
            candidates: None,
            row_id: None,
        }
    }
}

/// A language's declarations, each read as a pass comes to it: most never are.
pub struct FlowMethods<'s> {
    store: &'s Store,
    language: Language,
    read: RefCell<HashMap<String, Option<FlowMethod>>>,
    names: RefCell<HashMap<String, bool>>,
    scopes: RefCell<HashMap<String, Rc<Vec<FlowMethod>>>>,
}

fn is_method_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Method | NodeKind::Function)
}

impl<'s> FlowMethods<'s> {
    pub fn new(store: &'s Store, language: Language) -> FlowMethods<'s> {
        FlowMethods { store, language, read: RefCell::default(), names: RefCell::default(), scopes: RefCell::default() }
    }

    fn know(&self, methods: &[FlowMethod]) {
        let mut read = self.read.borrow_mut();
        for m in methods {
            read.insert(m.id.clone(), Some(m.clone()));
        }
    }

    /// A method or a function; a pass reads no others.
    pub fn get(&self, id: &str) -> Option<FlowMethod> {
        if let Some(m) = self.read.borrow().get(id) {
            return m.clone();
        }
        let node = self.store.pf_node_outline(id).ok().flatten();
        let method = node.filter(|n| n.language == self.language && is_method_kind(n.kind));
        self.read.borrow_mut().insert(id.to_string(), method.clone());
        method
    }

    /// Whether a method or a function has the name.
    pub fn is_name(&self, name: &str) -> bool {
        if let Some(&is) = self.names.borrow().get(name) {
            return is;
        }
        let is = self.store.pf_has_method_named(name, self.language).unwrap_or(false);
        self.names.borrow_mut().insert(name.to_string(), is);
        is
    }

    /// The classes and modules a file declares.
    pub fn scopes_in(&self, file: &str) -> Rc<Vec<FlowMethod>> {
        if let Some(s) = self.scopes.borrow().get(file) {
            return s.clone();
        }
        let s = Rc::new(self.store.pf_scopes_in_file(file).unwrap_or_default());
        self.scopes.borrow_mut().insert(file.to_string(), s.clone());
        s
    }
}

/// A value followed through parameters goes this many calls back.
const MAX_FLOW_DEPTH: usize = 4;
/// A name with more calls than this on unknown receivers isn't worth reading them all to rule out.
const MAX_UNKNOWN_CALLS: usize = 256;

const NO_CALLER: &str = "no-caller";
const UNTYPED_CALL: &str = "untyped-call";
const TOO_MANY_CALLS: &str = "too-many-calls";
const HANDED_ON: &str = "super";
const REBOUND: &str = "rebound";
const WRITTEN_ELSEWHERE: &str = "written-elsewhere";
const TOO_DEEP: &str = "too-deep";
const DISAGREE: &str = "sites-disagree";
const UNREAD_DECLARATION: &str = "unread-declaration";
/// Not an abstention: the argument is a parameter already being followed.
const CYCLE: &str = "cycle";

/// What a pass counted.
#[derive(Debug, Default, Clone)]
pub struct ParameterFlowStats {
    /// Parameters a flow typed, and the calls on them.
    pub typed: usize,
    pub calls: usize,
    /// Calls moved to the method of the flow's class, of which `added` had no edge before.
    pub moved: usize,
    pub added: usize,
    /// Calls whose answer the flow confirmed.
    pub confirmed: usize,
    /// Calls the class has no method for, of which `dropped` lost a guess.
    pub no_method: usize,
    pub dropped: usize,
    /// Calls that went back to normal resolution, the flow behind them gone.
    pub restored: usize,
    /// Candidates that got no type, by why.
    pub abstained: BTreeMap<&'static str, usize>,
}

/// One new answer: it replaces the edge or the failed row its site holds.
pub struct Retarget {
    pub resolved: ResolvedRef,
    pub edge_id: Option<i64>,
    pub row_id: Option<i64>,
}

/// What a pass wants done to the graph.
#[derive(Default)]
pub struct ParameterFlowPlan {
    pub retarget: Vec<Retarget>,
    /// Guesses a flow rules out. Each edge becomes a failed row marked as the flow's.
    pub drop: Vec<(i64, UnresolvedReference)>,
    /// What a flow did that no flow answers now: edges it made, and failed rows it marked.
    pub restore: Vec<(Option<i64>, Option<i64>, UnresolvedReference)>,
    pub stats: ParameterFlowStats,
}

/// A candidate a flow typed, with the names of the methods the flow went through.
struct TypedFlow {
    method: FlowMethod,
    parameter: FlowParameter,
    sites: Vec<FlowSite>,
    value: FlowValue,
    through: HashSet<String>,
}

#[derive(Default)]
struct Named {
    known: Vec<FlowSite>,
    unknown: Vec<FlowSite>,
    failed: Vec<FlowSite>,
}

/// Turns stored rows into sites.
struct Sites<'a> {
    methods: &'a FlowMethods<'a>,
}

impl Sites<'_> {
    fn from_edge(&self, row: &CallEdgeRow) -> Option<FlowSite> {
        let caller = self.methods.get(&row.source)?;
        let line = row.line?;
        let metadata: serde_json::Map<String, Value> = serde_json::from_str(row.metadata.as_deref()?).ok()?;
        let ref_name = metadata.get("refName")?.as_str()?.to_string();
        Some(FlowSite {
            caller_id: row.source.clone(),
            file_path: caller.file_path,
            line,
            column: row.column.unwrap_or(0),
            ref_name,
            target_id: Some(row.target.clone()),
            confidence: metadata.get("confidence").and_then(Value::as_f64).unwrap_or(0.0),
            edge_id: Some(row.id),
            row_id: None,
            via_flow: metadata.contains_key("parameterFlow"),
            method: String::new(),
            receiver: None,
        })
    }

    fn from_failed(row: &FailedCallRow) -> FlowSite {
        FlowSite {
            caller_id: row.from_node_id.clone(),
            file_path: row.file_path.clone(),
            line: row.line,
            column: row.column,
            ref_name: row.reference_name.clone(),
            target_id: None,
            confidence: 0.0,
            edge_id: None,
            row_id: Some(row.id),
            via_flow: row.via_flow,
            method: String::new(),
            receiver: None,
        }
    }

    /// The sites whose call reads as one, with what it calls.
    fn read(&self, edges: &[CallEdgeRow], failed: &[FailedCallRow]) -> Vec<FlowSite> {
        edges
            .iter()
            .filter_map(|r| self.from_edge(r))
            .chain(failed.iter().map(Sites::from_failed))
            .filter_map(|mut site| {
                let (method, receiver) = RubyFlow::read_call(&site.ref_name)?;
                site.method = method;
                site.receiver = receiver;
                Some(site)
            })
            .collect()
    }
}

/// One language's flows over one state of the graph. The graph is read as a flow gets that far:
/// most end at the first call of their method.
struct FlowPass<'a> {
    adapter: &'a RubyFlow<'a>,
    methods: &'a FlowMethods<'a>,
    store: &'a Store,
    language: Language,
    incoming: RefCell<HashMap<String, Rc<Vec<FlowSite>>>>,
    named: RefCell<HashMap<String, Rc<Named>>>,
    families: RefCell<HashMap<String, Rc<HashSet<String>>>>,
    handed_on: RefCell<HashMap<String, bool>>,
    /// Names the project writes somewhere no call site accounts for.
    written_elsewhere: HashSet<String>,
    /// Parameters called on that no flow typed, by why.
    abstained: RefCell<BTreeMap<&'static str, usize>>,
}

impl<'a> FlowPass<'a> {
    fn sites(&self) -> Sites<'a> {
        Sites { methods: self.methods }
    }

    fn edges(&self, of: CallEdgesOf<'_>) -> Vec<CallEdgeRow> {
        self.store.pf_call_edges(of).unwrap_or_default()
    }

    fn failed(&self, of: FailedCallsOf<'_>) -> Vec<FailedCallRow> {
        self.store.pf_failed_call_sites(self.language, of).unwrap_or_default()
    }

    /// The resolved calls that landed on `method`.
    fn calls_into(&self, method: &FlowMethod) -> Rc<Vec<FlowSite>> {
        if let Some(c) = self.incoming.borrow().get(&method.id) {
            return c.clone();
        }
        let calls = Rc::new(self.sites().read(&self.edges(CallEdgesOf::Target(&method.id)), &[]));
        self.incoming.borrow_mut().insert(method.id.clone(), calls.clone());
        calls
    }

    /// Every call of a name, by what the graph knows of it. A failed call it never does.
    fn calls_named(&self, name: &str) -> Rc<Named> {
        if let Some(c) = self.named.borrow().get(name) {
            return c.clone();
        }
        let mut calls = Named { failed: self.sites().read(&[], &self.failed(FailedCallsOf::Tail(name))), ..Named::default() };
        for site in self.sites().read(&self.edges(CallEdgesOf::TargetName(name)), &[]) {
            if self.adapter.is_known(&site) {
                calls.known.push(site);
            } else {
                calls.unknown.push(site);
            }
        }
        let calls = Rc::new(calls);
        self.named.borrow_mut().insert(name.to_string(), calls.clone());
        calls
    }

    /// How many calls of the name the graph holds, failed ones included.
    fn sites_of(&self, name: &str) -> usize {
        let c = self.calls_named(name);
        c.known.len() + c.unknown.len() + c.failed.len()
    }

    /// The calls of `receiver` in `method` that resolution guessed at or failed on. A confident
    /// edge on a parameter's name is a local's: its own pass typed it.
    fn sites_on(&self, method: &FlowMethod, receiver: &str) -> Vec<FlowSite> {
        let edges: Vec<FlowSite> = self
            .sites()
            .read(&self.edges(CallEdgesOf::Source(&method.id)), &[])
            .into_iter()
            .filter(|s| s.via_flow || s.confidence < 0.85)
            .collect();
        let failed = self.sites().read(&[], &self.failed(FailedCallsOf::Caller(&method.id)));
        edges.into_iter().chain(failed).filter(|s| s.receiver.as_deref() == Some(receiver)).collect()
    }

    fn family(&self, method: &FlowMethod) -> Rc<HashSet<String>> {
        let key = format!("{}\0{}", method.qualified_name, method.is_static);
        if let Some(f) = self.families.borrow().get(&key) {
            return f.clone();
        }
        let f = Rc::new(self.adapter.family(method));
        self.families.borrow_mut().insert(key, f.clone());
        f
    }

    fn hands_on(&self, name: &str) -> bool {
        if let Some(&h) = self.handed_on.borrow().get(name) {
            return h;
        }
        let h = self.adapter.hands_on(name);
        self.handed_on.borrow_mut().insert(name.to_string(), h);
        h
    }

    /// The value every call of `method` passes `parameter`, or why there is none. `path` holds the
    /// parameters being followed: a call passing one of them on adds nothing, as a walk over a tree
    /// passes its own `context` to itself, so only the calls from outside the cycle decide.
    /// `through` collects the method names the answer rests on.
    fn flow(&self, method: &FlowMethod, parameter: &FlowParameter, path: &[String], through: &RefCell<HashSet<String>>) -> Answer {
        let name = &method.name;
        if self.written_elsewhere.contains(name) {
            return Err(WRITTEN_ELSEWHERE);
        }
        let signature = self.adapter.signature(method).ok_or(UNREAD_DECLARATION)?;
        if self.adapter.rebinds(&parameter.name, method) {
            return Err(REBOUND);
        }
        let mut here = path.to_vec();
        here.push(format!("{}\0{}", method.id, parameter.name));
        let follow = |m: &FlowMethod, p: &FlowParameter| -> Answer {
            if here.contains(&format!("{}\0{}", m.id, p.name)) {
                return Err(CYCLE);
            }
            if here.len() >= MAX_FLOW_DEPTH {
                return Err(TOO_DEEP);
            }
            self.flow(m, p, &here, through)
        };
        let mut value: Option<FlowValue> = None;
        let passed_by = |site: &FlowSite, value: &mut Option<FlowValue>| -> Option<&'static str> {
            match self.adapter.argument_value(site, parameter, &follow) {
                Err(CYCLE) => None,
                Err(why) => Some(why),
                Ok(passed) => {
                    if value.as_ref().is_some_and(|v| v.ty != passed.ty || v.is_class != passed.is_class) {
                        return Some(DISAGREE);
                    }
                    *value = Some(passed);
                    None
                }
            }
        };
        // The calls of this very method first: most flows end at their first argument.
        for site in self.calls_into(method).iter() {
            if !self.adapter.is_known(site) {
                continue;
            }
            if let Some(stop) = passed_by(site, &mut value) {
                return Err(stop);
            }
        }
        if value.is_none() {
            return Err(NO_CALLER);
        }
        // Counted together: an answer a flow gave, which a later pass finds as an edge, is a
        // failed call in a fresh index, and the two have to agree.
        let named = self.calls_named(name);
        if named.unknown.len() + named.failed.len() > MAX_UNKNOWN_CALLS {
            return Err(TOO_MANY_CALLS);
        }
        if named.unknown.iter().chain(&named.failed).any(|site| !self.adapter.cannot_call(site, &signature)) {
            return Err(UNTYPED_CALL);
        }
        if self.hands_on(name) {
            return Err(HANDED_ON);
        }
        let others: Vec<&FlowSite> = named.known.iter().filter(|s| s.target_id.as_deref() != Some(method.id.as_str())).collect();
        if !others.is_empty() {
            let family = self.family(method);
            for site in others {
                if !site.target_id.as_ref().is_some_and(|t| family.contains(t)) {
                    continue;
                }
                if let Some(stop) = passed_by(site, &mut value) {
                    return Err(stop);
                }
            }
        }
        through.borrow_mut().insert(name.clone());
        value.ok_or(NO_CALLER)
    }

    fn abstain(&self, why: &'static str) {
        *self.abstained.borrow_mut().entry(why).or_default() += 1;
    }

    /// Every flow of `candidates` into a parameter something is called on: the typed ones, and
    /// the abstentions counted.
    fn run(&self, candidates: &[FlowMethod]) -> Vec<TypedFlow> {
        let mut typed = Vec::new();
        for method in candidates {
            if method.kind != NodeKind::Method {
                continue;
            }
            let Some(signature) = self.adapter.signature(method) else { continue };
            if signature.parameters.is_empty() {
                continue;
            }
            let receivers = self.adapter.receivers(method);
            for parameter in &signature.parameters {
                // A call no project method is named for has nothing for a flow to move.
                let called = receivers.get(&parameter.name).is_some_and(|names| names.iter().any(|n| self.methods.is_name(n)));
                if !called {
                    continue;
                }
                let through = RefCell::new(HashSet::new());
                match self.flow(method, parameter, &[], &through) {
                    Err(why) => self.abstain(why),
                    Ok(value) => typed.push(TypedFlow {
                        method: method.clone(),
                        parameter: parameter.clone(),
                        sites: self.sites_on(method, &parameter.name),
                        value,
                        through: through.into_inner(),
                    }),
                }
            }
        }
        typed
    }

    /// Whether a typed flow still stands now that `written_elsewhere` is known.
    fn stands(&self, flow: &TypedFlow) -> bool {
        match self.flow(&flow.method, &flow.parameter, &[], &RefCell::new(HashSet::new())) {
            Ok(_) => true,
            Err(why) => {
                self.abstain(why);
                false
            }
        }
    }
}

fn unresolved_of(reference: &UnresolvedReference) -> UnresolvedRef {
    UnresolvedRef {
        from_node_id: reference.from_node_id.clone(),
        reference_name: reference.reference_name.clone(),
        reference_kind: reference.reference_kind,
        line: reference.line,
        column: reference.column,
        file_path: reference.file_path.clone().unwrap_or_default(),
        language: reference.language.unwrap_or(Language::Ruby),
        candidates: None,
        row_id: None,
        name_tail: None,
    }
}

/// What the Ruby flows want done to the graph as it is now; `None` for a project with no Ruby.
pub fn plan_parameter_flows(store: &Store, ctx: &Ctx) -> codegraph_db::Result<Option<ParameterFlowPlan>> {
    let language = Language::Ruby;
    let files = store.pf_file_paths_by_language(language)?;
    if files.is_empty() {
        return Ok(None);
    }
    let methods = FlowMethods::new(store, language);
    let adapter = RubyFlow::new(ctx, &methods);
    let mut pass = FlowPass {
        adapter: &adapter,
        methods: &methods,
        store,
        language,
        incoming: RefCell::default(),
        named: RefCell::default(),
        families: RefCell::default(),
        handed_on: RefCell::default(),
        written_elsewhere: HashSet::new(),
        abstained: RefCell::default(),
    };
    let candidates = store.pf_reached_methods(language)?;
    methods.know(&candidates);

    // A site whose answer the flow doesn't change needs no more proof. The others rest on the
    // graph holding every call of the methods the flow went through, which only the project's
    // text can show: it is read for those names, and only when there are any.
    let answers: Vec<(TypedFlow, Vec<Option<FlowMethod>>)> = pass
        .run(&candidates)
        .into_iter()
        .map(|flow| {
            let targets = flow.sites.iter().map(|s| adapter.method_on(&flow.value, &s.method)).collect();
            (flow, targets)
        })
        .collect();
    let drops = |flow: &TypedFlow, site: &FlowSite, target: &Option<FlowMethod>| -> bool {
        target.is_none() && site.edge_id.is_some() && !site.via_flow && adapter.answers_itself(&flow.value, &site.method)
    };
    let changes = |(flow, targets): &(TypedFlow, Vec<Option<FlowMethod>>)| -> bool {
        flow.sites.iter().zip(targets).any(|(site, target)| {
            site.via_flow || drops(flow, site, target) || target.as_ref().is_some_and(|t| site.target_id.as_deref() != Some(t.id.as_str()))
        })
    };
    let rests: HashSet<String> = answers.iter().filter(|a| changes(a)).flat_map(|(f, _)| f.through.iter().cloned()).collect();
    let written = adapter.written(&rests, &files);
    pass.written_elsewhere = rests
        .iter()
        .filter(|name| {
            let count: usize = written.get(*name).map_or(0, |by_file| by_file.values().sum());
            count > adapter.declared(name) + pass.sites_of(name)
        })
        .cloned()
        .collect();

    let mut plan = ParameterFlowPlan::default();
    // What a pass before this one did, until a flow answers it again.
    let mut unanswered: HashMap<String, FlowSite> = HashMap::new();
    let (edges, rows) = store.pf_flow_call_sites(language)?;
    let sites = pass.sites();
    for site in edges.iter().filter_map(|r| sites.from_edge(r)).chain(rows.iter().map(Sites::from_failed)) {
        unanswered.insert(site.hold_key(), site);
    }
    let stats = &mut plan.stats;
    for (flow, targets) in &answers {
        if flow.through.iter().any(|n| pass.written_elsewhere.contains(n)) && !pass.stands(flow) {
            continue;
        }
        stats.typed += 1;
        stats.calls += flow.sites.len();
        for (site, target) in flow.sites.iter().zip(targets) {
            let Some(target) = target else {
                stats.no_method += 1;
                if site.via_flow && site.row_id.is_some() && adapter.answers_itself(&flow.value, &site.method) {
                    unanswered.remove(&site.hold_key());
                }
                // The class has no such method. Where it answers the call itself, the guess is
                // wrong; otherwise it may hand the call on, as a delegate does, and the flow says
                // nothing of it.
                if drops(flow, site, &None) {
                    stats.dropped += 1;
                    plan.drop.push((site.edge_id.unwrap_or_default(), site.reference(language)));
                }
                continue;
            };
            unanswered.remove(&site.hold_key());
            if site.target_id.as_deref() == Some(target.id.as_str()) {
                stats.confirmed += 1;
                continue;
            }
            if site.edge_id.is_some() {
                stats.moved += 1;
            } else {
                stats.added += 1;
            }
            let mut metadata = Metadata::new();
            metadata.insert("parameterFlow".to_string(), Value::String(flow.parameter.name.clone()));
            let mut resolved = ResolvedRef::new(&unresolved_of(&site.reference(language)), target.id.clone(), 0.9, ResolvedBy::InstanceMethod);
            resolved.metadata = Some(metadata);
            plan.retarget.push(Retarget { resolved, edge_id: site.edge_id, row_id: site.row_id });
        }
    }
    let mut leftover: Vec<FlowSite> = unanswered.into_values().collect();
    leftover.sort_by(|a, b| a.hold_key().cmp(&b.hold_key()));
    for site in leftover {
        stats.restored += 1;
        plan.restore.push((site.edge_id, site.row_id, site.reference(language)));
    }
    stats.abstained = pass.abstained.borrow().clone();
    Ok(Some(plan))
}
