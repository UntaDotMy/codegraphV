use crate::api::{self, Error, Query, Result};
use codegraph_core::CodeGraph;
use codegraph_types::*;
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
impl From<codegraph_graph::GraphError> for Error {
    fn from(e: codegraph_graph::GraphError) -> Self {
        Self {
            status: 500,
            code: "internal",
            message: e.to_string(),
        }
    }
}
fn stamp(cg: &CodeGraph) -> Result<Value> {
    let stats = cg.get_stats()?;
    Ok(
        json!({"lastIndexedAt":cg.get_last_indexed_at()?,"edges":stats.edge_count,"files":stats.file_count}),
    )
}
fn route_root(cg: &CodeGraph, route: &Node) -> Result<Option<(Node, bool)>> {
    let edges = cg.get_outgoing_edges(&route.id)?;
    let mut targets = vec![];
    for e in &edges {
        if e.kind == EdgeKind::References
            && e.metadata.as_ref().and_then(|m| m.get("layout")) != Some(&Value::Bool(true))
        {
            if let Some(n) = cg.get_node(&e.target)? {
                if matches!(
                    n.kind,
                    NodeKind::Function
                        | NodeKind::Method
                        | NodeKind::Class
                        | NodeKind::Component
                        | NodeKind::Constant
                        | NodeKind::Variable
                ) {
                    targets.push(n);
                }
            }
        }
    }
    targets.sort_by_key(|n| match n.kind {
        NodeKind::Function | NodeKind::Method => 0,
        NodeKind::Component => 1,
        NodeKind::Class => 2,
        _ => 3,
    });
    if let Some(n) = targets.first() {
        return Ok(Some((n.clone(), false)));
    }
    for e in edges
        .iter()
        .filter(|e| matches!(e.kind, EdgeKind::Calls | EdgeKind::Instantiates))
    {
        if let Some(n) = cg.get_node(&e.target)? {
            if n.kind == NodeKind::Component
                || n.kind == NodeKind::Function
                    && n.name.chars().next().is_some_and(char::is_uppercase)
            {
                return Ok(Some((n, false)));
            }
        }
    }
    Ok(if edges.iter().any(|e| e.kind == EdgeKind::Calls) {
        Some((route.clone(), true))
    } else {
        None
    })
}
pub fn routes(cg: &CodeGraph, q: &Query) -> Result<Value> {
    let nodes = cg.get_nodes_by_kind(NodeKind::Route)?;
    let limit = api::int(q, "limit", 300, 10000)?;
    let mut entries = vec![];
    let mut counts = HashMap::<String, usize>::new();
    for route in &nodes {
        let Some((handler, inline)) = route_root(cg, route)? else {
            continue;
        };
        let (method, path) = route
            .name
            .split_once(' ')
            .filter(|(m, _)| {
                matches!(
                    *m,
                    "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "OPTIONS" | "HEAD" | "ALL"
                )
            })
            .map(|(m, p)| (Some(m), p))
            .unwrap_or((None, &route.name));
        *counts.entry(handler.file_path.clone()).or_default() += 1;
        entries.push(json!({"url":route.name,"method":method,"path":path,"handler":handler.name,"handlerKind":handler.kind,"file":handler.file_path,"line":handler.start_line,"handlerId":if inline{None}else{Some(&handler.id)},"routeFile":route.file_path,"routeLine":route.start_line,"routeId":route.id,"inline":inline}));
    }
    entries.sort_by(|a, b| a["url"].as_str().cmp(&b["url"].as_str()));
    let total = entries.len();
    entries.truncate(limit);
    let top = counts.iter().max_by_key(|(_, n)| *n);
    Ok(
        json!({"routed":!nodes.is_empty(),"routeCount":nodes.len(),"shown":entries.len(),"truncated":total>entries.len(),"topHandlerFile":top.map(|(f,_)|f),"topHandlerFileCount":top.map(|(_,n)|n).copied().unwrap_or(0),"entries":entries}),
    )
}
pub fn entrypoints(cg: &CodeGraph) -> Result<Value> {
    let route_data = routes(cg, &vec![])?;
    let file_nodes = cg.get_nodes_by_kind(NodeKind::File)?;
    let mut files = vec![];
    let mut tests = vec![];
    for n in &file_nodes {
        let out = cg.get_outgoing_edges(&n.id)?;
        let deps = cg.get_file_dependencies(&n.file_path)?;
        let dependents = cg.get_file_dependents(&n.file_path)?;
        let calls = out
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::Calls | EdgeKind::Instantiates))
            .count();
        let mut row = api::node_ref(n);
        row["calls"] = json!(calls);
        row["reaches"] = json!(deps.len());
        row["dependents"] = json!(dependents.len());
        if codegraph_search::is_test_file(&n.file_path) {
            row["refs"] = json!(out.len());
            tests.push(row);
        } else if calls > 0 || dependents.is_empty() {
            files.push(row);
        }
    }
    files.sort_by_key(|v| std::cmp::Reverse(v["calls"].as_u64().unwrap_or(0)));
    tests.sort_by_key(|v| std::cmp::Reverse(v["reaches"].as_u64().unwrap_or(0)));
    let nodes = cg.get_all_nodes()?;
    let ids: Vec<_> = nodes.iter().map(|n| n.id.clone()).collect();
    let counts = cg.store().count_incoming_edges(&ids)?;
    let mut hubs: Vec<_> = nodes
        .iter()
        .filter(|n| {
            !matches!(n.kind, NodeKind::File | NodeKind::Import | NodeKind::Export)
                && counts.get(&n.id).copied().unwrap_or(0) > 0
        })
        .map(|n| {
            let mut row = api::node_ref(n);
            row["dependents"] = json!(counts[&n.id]);
            row
        })
        .collect();
    hubs.sort_by_key(|v| std::cmp::Reverse(v["dependents"].as_u64().unwrap_or(0)));
    let stats = cg.get_stats()?;
    let entries = route_data["entries"].as_array().unwrap().clone();
    Ok(
        json!({"frameworks":cg.get_detected_frameworks(),"routes":{"routed":route_data["routed"],"routeCount":route_data["routeCount"],"items":api::list(entries.clone(),entries.len())},"files":api::list(files.iter().take(40).cloned().collect(),files.len()),"tests":api::list(tests.iter().take(40).cloned().collect(),tests.len()),"hubs":api::list(hubs.iter().take(40).cloned().collect(),hubs.len()),"index":{"lastIndexedAt":cg.get_last_indexed_at()?,"files":stats.file_count},"timing":{"elapsedMs":0,"cached":false}}),
    )
}
pub fn flow(cg: &CodeGraph, q: &Query) -> Result<Value> {
    use codegraph_graph::named_symbol_flow::{
        resolve_named_symbol_flow, FlowMode, NamedSymbolFlowOptions,
    };
    let from = api::text(q, "from");
    let to = api::text(q, "to");
    let symbols = api::text(q, "symbols");
    if symbols.is_none() && (from.is_none() || to.is_none()) {
        return Err(Error::bad("Pass symbols, or both from and to"));
    }
    let directed = from.is_some() && to.is_some();
    let query = symbols
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{} {}", from.unwrap(), to.unwrap()));
    let flow = resolve_named_symbol_flow(
        cg.store(),
        cg.store().as_ref(),
        &query,
        &NamedSymbolFlowOptions {
            mode: Some(if directed {
                FlowMode::Directed
            } else {
                FlowMode::Named
            }),
            from: from.map(str::to_owned),
            to: to.map(str::to_owned),
            ..Default::default()
        },
        None,
    )?;
    let mut flows = vec![];
    for chain in &flow.chains {
        let mut hops = vec![];
        for step in &chain.steps {
            let n = &step.node;
            let source = api::source(
                cg,
                &vec![
                    ("file".into(), n.file_path.clone()),
                    ("from".into(), n.start_line.to_string()),
                    ("to".into(), n.end_line.to_string()),
                ],
            )
            .ok();
            let edge = step.edge.as_ref().map(|e| {
                let mut wire = api::edge(e);
                let hop = codegraph_graph::synthesized_hop::describe_synthesized_hop(Some(e));
                wire["label"] = json!(hop.map(|h| h.summary).unwrap_or_else(|| e.kind.to_string()));
                wire["upward"] = json!(false);
                wire["uncertain"] = json!(e
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("confidence"))
                    .and_then(Value::as_f64)
                    .is_some_and(|v| v < 0.6));
                wire["synthesized"] = json!(e.provenance == Some(Provenance::Heuristic));
                wire
            });
            hops.push(json!({"node":api::node_ref(n),"edge":edge,"callRef":null,"source":source}));
        }
        flows.push(json!({"id":chain.steps.iter().map(|s|s.node.id.as_str()).collect::<Vec<_>>().join(">"),"label":format!("{} → {}",chain.steps.first().unwrap().node.name,chain.steps.last().unwrap().node.name),"hops":hops,"boundary":null,"partial":false}));
    }
    let unresolved: Vec<_> = flow
        .tokens
        .iter()
        .filter(|t| flow.token_nodes.get(*t).is_none_or(Vec::is_empty))
        .collect();
    let ambiguous:Vec<_>=flow.token_family.iter().filter(|(_,nodes)|nodes.len()>1).map(|(token,nodes)|json!({"token":token,"chosen":nodes.first().map(api::node_ref),"others":nodes.iter().skip(1).map(api::node_ref).collect::<Vec<_>>()})).collect();
    Ok(
        json!({"query":{"kind":if directed{"directed"}else{"symbols"},"from":from,"to":to,"symbols":flow.tokens},"flows":flows,"ambiguous":ambiguous,"unresolved":unresolved,"reason":if flow.chains.is_empty(){Some("No recorded call path connects these symbols. Dynamic dispatch or code outside the index may be a boundary.")}else{None},"index":stamp(cg)?,"timing":{"elapsedMs":0}}),
    )
}
pub fn filecode(cg: &CodeGraph, path: &str) -> Result<Value> {
    let mut file = api::file(cg, path)?;
    let source = api::source(cg, &vec![("file".into(), path.into())])?;
    file["file"]["totalLines"] = source["totalLines"].clone();
    let nodes = cg.get_nodes_by_file(path)?;
    let mut calls = vec![];
    let mut outside = vec![];
    let mut intra = 0;
    for n in &nodes {
        let edges = cg.get_outgoing_edges(&n.id)?;
        let relevant: Vec<_> = edges
            .into_iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    EdgeKind::Calls | EdgeKind::Instantiates | EdgeKind::References
                )
            })
            .collect();
        for relation in api::relations(cg, &relevant, false)? {
            if relation["node"]["file"] == path {
                intra += relation["edgeCount"].as_u64().unwrap_or(0);
            }
            calls.push(json!({"ownerId":n.id,"ownerLine":n.start_line,"relation":relation}));
        }
        for r in cg.store().get_unresolved_references_from(&n.id)? {
            outside.push(json!({"line":r.line,"col":r.column,"name":r.reference_name,"kind":r.reference_kind}));
        }
    }
    Ok(
        json!({"file":file["file"],"drift":source["drift"],"reason":source["reason"],"outline":file["outline"],"calls":api::list(calls.clone(),calls.len()),"outside":api::list(outside.clone(),outside.len()),"intraFileCalls":intra,"timing":{"elapsedMs":0}}),
    )
}
pub fn map(cg: &CodeGraph, q: &Query) -> Result<Value> {
    let root = api::text(q, "root").unwrap_or("").trim_matches('/');
    let depth = api::int(q, "depth", 1, 8)?;
    if root.contains("..") || root.contains('\\') {
        return Err(Error::bad("root must be a project directory"));
    }
    let files = cg.get_files()?;
    let files: Vec<_> = files
        .iter()
        .filter(|f| root.is_empty() || f.path.starts_with(&format!("{root}/")))
        .collect();
    let module_of = |path: &str| {
        let relative = path
            .strip_prefix(root)
            .unwrap_or(path)
            .trim_start_matches('/');
        let parts: Vec<_> = relative.split('/').collect();
        if parts.len() == 1 {
            "(root files)".into()
        } else {
            let name = parts[..depth.min(parts.len() - 1)].join("/");
            if root.is_empty() {
                name
            } else {
                format!("{root}/{name}")
            }
        }
    };
    let mut groups = IndexMap::<String, Vec<&FileRecord>>::new();
    let mut file_module = HashMap::new();
    for f in &files {
        let id = module_of(&f.path);
        file_module.insert(f.path.clone(), id.clone());
        groups.entry(id).or_default().push(f);
    }
    let nodes = cg.get_all_nodes()?;
    let ids: Vec<_> = nodes.iter().map(|n| n.id.clone()).collect();
    let node_by_id: HashMap<_, _> = nodes.iter().map(|n| (&n.id, n)).collect();
    let edges = cg.store().get_outgoing_edges_from(&ids, &[])?;
    let mut links = IndexMap::<(String, String), Vec<&Edge>>::new();
    let mut dependents = HashMap::<String, HashSet<String>>::new();
    let mut uncertain = 0;
    for edge in &edges {
        if edge.kind == EdgeKind::Contains {
            continue;
        }
        if edge
            .metadata
            .as_ref()
            .and_then(|m| m.get("confidence"))
            .and_then(Value::as_f64)
            .is_some_and(|v| v < 0.6)
        {
            uncertain += 1;
            continue;
        }
        let (Some(from), Some(to)) = (node_by_id.get(&edge.source), node_by_id.get(&edge.target))
        else {
            continue;
        };
        let (Some(a), Some(b)) = (
            file_module.get(&from.file_path),
            file_module.get(&to.file_path),
        ) else {
            continue;
        };
        if a != b {
            links.entry((a.clone(), b.clone())).or_default().push(edge);
            dependents
                .entry(b.clone())
                .or_default()
                .insert(from.file_path.clone());
        }
    }
    let modules:Vec<_>=groups.into_iter().map(|(id,files)|{let mut languages=IndexMap::<String,usize>::new();for f in &files{*languages.entry(f.language.to_string()).or_default()+=1;}let dep=dependents.get(&id);let dep_modules:HashSet<_>=dep.into_iter().flatten().filter_map(|f|file_module.get(f)).collect();let generated:Vec<_>=files.iter().filter(|f|f.generated==Some(true)).map(|f|&f.path).collect();json!({"label":id,"id":id,"files":files.len(),"symbols":files.iter().map(|f|f.node_count as u64).sum::<u64>(),"languages":languages.into_iter().map(|(language,files)|json!({"language":language,"files":files})).collect::<Vec<_>>(),"test":files.iter().filter(|f|codegraph_search::is_test_file(&f.path)).count()*2>files.len(),"generated":generated.len(),"generatedFiles":generated,"facade":false,"fileList":api::list(files.iter().take(300).map(|f|json!(f.path)).collect(),files.len()),"dependents":{"files":dep.map_or(0,HashSet::len),"modules":dep_modules.len()}})}).collect();
    let links:Vec<_>=links.into_iter().map(|((source,target),edges)|{let mut kinds=IndexMap::<String,usize>::new();for e in &edges{*kinds.entry(e.kind.to_string()).or_default()+=1;}json!({"source":source,"target":target,"count":edges.len(),"declared":edges.iter().filter(|e|e.provenance!=Some(Provenance::Heuristic)).count(),"byKind":kinds.into_iter().map(|(kind,count)|json!({"kind":kind,"count":count})).collect::<Vec<_>>(),"topPairs":[]})}).collect();
    let cycles = cg.find_circular_dependencies()?;
    let cycles: Vec<_> = cycles
        .into_iter()
        .map(|files| {
            let modules: HashSet<_> = files.iter().filter_map(|f| file_module.get(f)).collect();
            json!({"size":files.len(),"files":files,"modules":modules})
        })
        .collect();
    Ok(
        json!({"root":root,"depth":depth,"roots":[{"root":"","label":"Repository","files":files.len()}],"modules":modules,"links":links,"cycles":api::list(cycles.clone(),cycles.len()),"excluded":{"uncertainEdges":uncertain,"confidenceBelow":0.6},"index":stamp(cg)?,"timing":{"elapsedMs":0,"cached":false}}),
    )
}
pub fn deadcode(cg: &CodeGraph, q: &Query) -> Result<Value> {
    use codegraph_graph::dead_code::{build_dead_code_report, DeadCodeQuery};
    let query = DeadCodeQuery {
        project_root: cg.project_root(),
        limit: api::int(q, "limit", 200, 300)?,
        include_exported: api::boolean(q, "includeExported"),
        include_tests: api::boolean(q, "includeTests"),
        include_generated: api::boolean(q, "includeGenerated"),
        ..Default::default()
    };
    let report = build_dead_code_report(cg.store(), &query)?;
    let mut groups = IndexMap::<String, Vec<Value>>::new();
    let mut rows = vec![];
    for entry in &report.entries {
        let mut row = api::node_ref(&entry.node);
        row["lines"] = json!(entry.lines);
        row["members"] = api::list(
            entry.members.iter().take(12).map(api::node_ref).collect(),
            entry.members.len(),
        );
        groups
            .entry(entry.node.file_path.clone())
            .or_default()
            .push(row.clone());
        rows.push(row);
    }
    let excluded = &report.excluded;
    let exclusions = [
        ("tests", excluded.tests),
        ("generated", excluded.generated),
        ("exported", excluded.exported),
        ("exportsUnknown", excluded.exports_unknown),
        ("declarations", excluded.declarations),
        ("decorated", excluded.decorated),
        ("overriding", excluded.overriding),
        ("hooks", excluded.hooks),
        ("implicit", excluded.implicit),
        ("vendored", excluded.vendored),
        ("testScope", excluded.test_scope),
        ("markup", excluded.markup),
        ("unreachableFile", excluded.unreachable_file),
        ("unresolvedName", excluded.unresolved_name),
        ("ambiguousName", excluded.ambiguous_name),
        ("mentioned", excluded.mentioned),
        ("unreadable", excluded.unreadable),
        ("nested", excluded.nested),
    ];
    Ok(
        json!({"rows":api::list(rows,report.total),"groups":groups.into_iter().map(|(file,rows)|json!({"test":codegraph_search::is_test_file(&file),"generated":cg.get_file(&file).ok().flatten().is_some_and(|f|f.generated==Some(true)),"file":file,"lines":rows.iter().filter_map(|v|v["lines"].as_u64()).sum::<u64>(),"rows":rows})).collect::<Vec<_>>(),"candidates":report.candidates,"excluded":exclusions.iter().filter(|(_,count)|*count>0).map(|(reason,count)|json!({"reason":reason,"count":count,"label":reason})).collect::<Vec<_>>(),"excludedTotal":exclusions.iter().map(|(_,n)|n).sum::<usize>(),"kinds":report.kinds,"includeExported":report.include_exported,"includeTests":query.include_tests,"includeGenerated":query.include_generated,"bounded":report.bounded,"corroborated":report.corroborated,"timing":{"elapsedMs":0}}),
    )
}
