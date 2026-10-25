//! Viewer wire shapes. Graph derivations live in codegraph-graph; this module
//! only validates requests and presents those results.
use codegraph_core::{
    utils::{validate_path_within_root, ValidateOptions, CONFIG_LEAF_LANGUAGES},
    CodeGraph,
};
use codegraph_types::*;
use indexmap::IndexMap;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fmt,
};
pub type Query = Vec<(String, String)>;
#[derive(Debug)]
pub struct Error {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}
impl Error {
    pub fn bad(message: impl Into<String>) -> Self {
        Self {
            status: 400,
            code: "bad-request",
            message: message.into(),
        }
    }
    pub fn missing(message: impl Into<String>) -> Self {
        Self {
            status: 404,
            code: "not-found",
            message: message.into(),
        }
    }
    pub fn refused(message: impl Into<String>) -> Self {
        Self {
            status: 403,
            code: "refused",
            message: message.into(),
        }
    }
    pub fn body(&self) -> Value {
        json!({"error":self.message,"code":self.code})
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Error {}
impl From<codegraph_core::errors::CodeGraphError> for Error {
    fn from(e: codegraph_core::errors::CodeGraphError) -> Self {
        Self {
            status: 500,
            code: "internal",
            message: e.to_string(),
        }
    }
}
impl From<codegraph_db::DbError> for Error {
    fn from(e: codegraph_db::DbError) -> Self {
        Self {
            status: 500,
            code: "internal",
            message: e.to_string(),
        }
    }
}
pub type Result<T> = std::result::Result<T, Error>;
pub fn text<'a>(q: &'a Query, key: &str) -> Option<&'a str> {
    q.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}
pub fn int(q: &Query, key: &str, default: usize, max: usize) -> Result<usize> {
    match text(q, key) {
        None => Ok(default),
        Some(s) => s
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .map(|n| n.min(max))
            .ok_or_else(|| Error::bad(format!("{key} must be a positive integer"))),
    }
}
pub fn boolean(q: &Query, key: &str) -> bool {
    matches!(text(q, key), Some("true" | "1"))
}
pub fn list(items: Vec<Value>, total: usize) -> Value {
    json!({"total":total,"shown":items.len(),"truncated":items.len()<total,"items":items})
}
pub fn refs(nodes: &[Node]) -> Vec<Value> {
    nodes.iter().map(node_ref).collect()
}
pub fn node_ref(n: &Node) -> Value {
    let mut value = json!({"id":n.id,"kind":n.kind,"name":n.name,"qualifiedName":n.qualified_name,"file":n.file_path.replace('\\',"/"),"line":n.start_line,"endLine":n.end_line,"language":n.language,"test":codegraph_search::is_test_file(&n.file_path)});
    if let Some(s) = &n.signature {
        value["signature"] = json!(s);
    }
    if n.is_exported == Some(true) {
        value["exported"] = json!(true);
    }
    value
}
pub fn node_detail(n: &Node) -> Value {
    let mut v = node_ref(n);
    v["startColumn"] = json!(n.start_column);
    v["endColumn"] = json!(n.end_column);
    v["lines"] = json!(n.end_line.saturating_sub(n.start_line) + 1);
    for (key, value) in [
        ("docstring", json!(n.docstring)),
        ("visibility", json!(n.visibility)),
        ("async", json!(n.is_async)),
        ("static", json!(n.is_static)),
        ("abstract", json!(n.is_abstract)),
        ("decorators", json!(n.decorators)),
        ("typeParameters", json!(n.type_parameters)),
        ("returnType", json!(n.return_type)),
    ] {
        if !value.is_null() && value != false {
            v[key] = value;
        }
    }
    v
}
pub fn edge(e: &Edge) -> Value {
    let mut v = json!({"kind":e.kind});
    if let Some(line) = e.line {
        v["line"] = json!(line);
    }
    if let Some(col) = e.column {
        v["col"] = json!(col);
    }
    if let Some(p) = e.provenance {
        v["provenance"] = json!(p);
    }
    if let Some(m) = &e.metadata {
        for key in [
            "confidence",
            "resolvedBy",
            "synthesizedBy",
            "via",
            "registeredAt",
            "when",
        ] {
            if let Some(value) = m.get(key) {
                v[key] = value.clone();
            }
        }
        if m.get("fnRef") == Some(&Value::Bool(true)) {
            v["valueRef"] = json!(true);
        }
    }
    v
}
fn confidence(e: &Edge) -> Option<f64> {
    e.metadata
        .as_ref()
        .and_then(|m| m.get("confidence"))
        .and_then(Value::as_f64)
}
pub fn relations(cg: &CodeGraph, edges: &[Edge], incoming: bool) -> Result<Vec<Value>> {
    let mut grouped = IndexMap::<String, Vec<&Edge>>::new();
    for e in edges {
        if e.kind != EdgeKind::Contains {
            grouped
                .entry(if incoming {
                    e.source.clone()
                } else {
                    e.target.clone()
                })
                .or_default()
                .push(e);
        }
    }
    let nodes = cg
        .store()
        .get_nodes_by_ids(&grouped.keys().collect::<Vec<_>>())?;
    let mut out = vec![];
    for (id, edges) in grouped {
        let Some(n) = nodes.get(&id) else { continue };
        let kinds: HashSet<_> = edges.iter().map(|e| e.kind.as_str()).collect();
        let mut lines: Vec<_> = edges.iter().filter_map(|e| e.line).collect();
        lines.sort_unstable();
        lines.dedup();
        let conf = edges.iter().filter_map(|e| confidence(e)).reduce(f64::max);
        let synthesized = edges
            .iter()
            .any(|e| e.provenance == Some(Provenance::Heuristic));
        let mut kinds: Vec<_> = kinds.into_iter().collect();
        kinds.sort();
        out.push(json!({"node":node_ref(n),"edgeKinds":kinds,"edges":edges.iter().take(40).map(|e|edge(e)).collect::<Vec<_>>(),"edgeCount":edges.len(),"lines":lines,"confidence":conf,"uncertain":conf.is_some_and(|n|n<0.6),"synthesized":synthesized}));
    }
    Ok(out)
}
pub fn resolve_file(cg: &CodeGraph, file: &str) -> Result<(FileRecord, String)> {
    if file.is_empty()
        || std::path::Path::new(file).is_absolute()
        || file.contains('\\')
        || file.split('/').any(|s| s == "..")
        || file.contains('\0')
    {
        return Err(Error::refused(
            "Requested path must stay within the indexed project",
        ));
    }
    let absolute = validate_path_within_root(cg.project_root(), file, ValidateOptions::default())
        .ok_or_else(|| Error::refused("Requested path leaves the indexed project"))?;
    let record = cg
        .get_file(file)?
        .ok_or_else(|| Error::missing(format!("{file} is not in this CodeGraphV index")))?;
    Ok((record, absolute))
}
pub fn drift(cg: &CodeGraph, file: &str) -> bool {
    resolve_file(cg, file)
        .ok()
        .and_then(|(r, p)| {
            std::fs::read(p)
                .ok()
                .map(|bytes| format!("{:x}", Sha256::digest(bytes)) != r.content_hash)
        })
        .unwrap_or(false)
}
pub fn source(cg: &CodeGraph, q: &Query) -> Result<Value> {
    let file = text(q, "file").ok_or_else(|| Error::bad("file is required"))?;
    let (record, path) = resolve_file(cg, file)?;
    let mut v = json!({"file":file,"language":record.language,"contentHash":record.content_hash,"indexedAt":record.indexed_at,"generated":record.generated.unwrap_or(false),"drift":false,"showing":"none","totalLines":null});
    if CONFIG_LEAF_LANGUAGES.contains(&record.language.as_str()) {
        v["reason"] =
            json!("Configuration values may be secrets; only indexed key names are available.");
        return Ok(v);
    }
    let bytes = std::fs::read(path)
        .map_err(|_| Error::missing("The indexed source file is no longer readable"))?;
    if bytes.len() > 10_000_000 {
        return Err(Error::bad("Source file is too large to display"));
    }
    let changed = format!("{:x}", Sha256::digest(&bytes)) != record.content_hash;
    v["drift"] = json!(changed);
    let content = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<_> = content
        .split('\n')
        .map(|l| l.trim_end_matches('\r').to_owned())
        .collect();
    if lines.len() > 1 && lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    v["totalLines"] = json!(lines.len());
    if changed && text(q, "ondrift") != Some("current") {
        v["reason"]=json!("File changed since indexing; sync the index, or request ondrift=current to view current source.");
        return Ok(v);
    }
    let from = int(q, "from", 1, 1_000_000)?;
    let to = int(q, "to", lines.len().max(1), 1_000_000)?;
    if to < from {
        return Err(Error::bad("to must be at least from"));
    }
    let end = to.min(lines.len()).min(from.saturating_add(1999));
    let chosen: Vec<_> = lines
        .iter()
        .skip(from - 1)
        .take(end.saturating_sub(from - 1))
        .cloned()
        .collect();
    v["from"] = json!(from);
    v["to"] = json!(end);
    v["lines"] = json!(chosen);
    v["truncated"] = json!(end < to.min(lines.len()));
    v["showing"] = json!(if changed { "current" } else { "indexed" });
    v["highlight"] = json!({"engine":"plain","classes":[],"lines":chosen.iter().map(|l|json!([[0,l]])).collect::<Vec<_>>()});
    Ok(v)
}
pub fn stats(cg: &CodeGraph) -> Result<Value> {
    let s = cg.get_stats()?;
    let b = cg.get_index_build_info();
    Ok(
        json!({"project":{"root":cg.project_root(),"name":std::path::Path::new(cg.project_root()).file_name().unwrap_or_default().to_string_lossy()},"index":{"state":cg.get_index_state().map(|s|s.as_str()),"lastIndexedAt":cg.get_last_indexed_at()?,"stale":cg.is_index_stale(),"version":b.version,"extractionVersion":b.extraction_version,"backend":cg.store().backend(),"journalMode":cg.store().journal_mode()?,"pendingReferences":cg.get_pending_reference_count()?,"generatedFiles":cg.get_generated_file_count()?,"watching":false,"watcherDegraded":false},"graph":{"nodes":s.node_count,"edges":s.edge_count,"files":s.file_count,"nodesByKind":s.nodes_by_kind,"edgesByKind":s.edges_by_kind,"filesByLanguage":s.files_by_language,"dbSizeBytes":s.db_size_bytes,"walSizeBytes":s.wal_size_bytes},"frameworks":cg.get_detected_frameworks(),"thresholds":{"hub":40,"uncertainBelow":0.6},"blastScale":{"maxDirect":0,"maxWithinHops":0,"hops":3,"sampled":0,"estimated":true}}),
    )
}
pub fn search(cg: &CodeGraph, q: &Query) -> Result<Value> {
    let query = text(q, "q").unwrap_or("");
    let limit = int(q, "limit", 50, 200)?;
    let results = cg.search_nodes(
        query,
        &SearchOptions {
            limit: Some(400),
            ..Default::default()
        },
    )?;
    let mut items: Vec<_> = results
        .into_iter()
        .map(|r| {
            let mut v = node_ref(&r.node);
            v["matchKind"] = json!(if r.node.name.eq_ignore_ascii_case(query) {
                "exact"
            } else if r
                .node
                .name
                .to_lowercase()
                .starts_with(&query.to_lowercase())
            {
                "prefix"
            } else if r.node.name.to_lowercase().contains(&query.to_lowercase()) {
                "substring"
            } else {
                "related"
            });
            v
        })
        .collect();
    let total = items.len();
    items.truncate(limit);
    let mut groups = IndexMap::<String, Vec<Value>>::new();
    for item in &items {
        groups
            .entry(item["kind"].as_str().unwrap().into())
            .or_default()
            .push(item.clone());
    }
    Ok(
        json!({"query":query,"text":query,"filters":{"kinds":[],"languages":[],"paths":[],"names":[]},"results":list(items,total),"groups":groups.into_iter().map(|(kind,items)|json!({"kind":kind,"count":items.len(),"items":items})).collect::<Vec<_>>()}),
    )
}
pub fn node(cg: &CodeGraph, id: &str) -> Result<Value> {
    let n = cg.get_node(id)?.ok_or_else(|| {
        Error::missing("No symbol with that id is in this index; search by name after re-indexing")
    })?;
    let incoming = cg.get_incoming_edges(id)?;
    let outgoing = cg.get_outgoing_edges(id)?;
    let ins = relations(cg, &incoming, true)?;
    let outs = relations(cg, &outgoing, false)?;
    let children = cg.get_children(id)?;
    let mut members = vec![];
    for child in &children {
        let mut v = node_ref(child);
        v["parentId"] = json!(id);
        v["depth"] = json!(1);
        v["fanIn"] = json!(cg.get_incoming_edges(&child.id)?.len());
        v["fanOut"] = json!(cg.get_outgoing_edges(&child.id)?.len());
        members.push(v);
    }
    let impact = cg.get_impact_radius(id, 3)?;
    let mut byfile = IndexMap::<String, usize>::new();
    for v in impact.nodes.values().filter(|v| v.id != id) {
        *byfile.entry(v.file_path.clone()).or_default() += 1;
    }
    let tests: Vec<_> = byfile
        .keys()
        .filter(|f| codegraph_search::is_test_file(f))
        .cloned()
        .collect();
    let hierarchy=codegraph_graph::type_hierarchy::build_type_hierarchy(cg.store(),&n,true).map_err(|e|Error{status:500,code:"internal",message:e.to_string()})?.map(|h|{let convert=|entries:Vec<codegraph_graph::type_hierarchy::HierarchyEntry>|{let total=entries.len();list(entries.into_iter().map(|entry|{let mut v=node_ref(&entry.node);v["depth"]=json!(entry.depth);v["parentId"]=json!(entry.parent_id);v["relation"]=json!(entry.relation);v["synthesized"]=json!(entry.synthesized);v["hiddenSubtypes"]=json!(entry.hidden_subtypes);v}).collect(),total)};json!({"ancestors":convert(h.ancestors),"descendants":convert(h.descendants),"direct":h.direct_subtypes,"implementers":h.direct_implementers,"bounded":h.bounded,"polymorphic":h.polymorphic})});
    let unresolved = cg.store().get_unresolved_references_from(id)?;
    let mut kinds = HashMap::<String, usize>::new();
    for r in &unresolved {
        *kinds.entry(r.reference_kind.as_str().into()).or_default() += 1;
    }
    Ok(
        json!({"node":node_detail(&n),"ancestors":refs(&cg.get_ancestors(id)?),"members":list(members,children.len()),"hierarchy":hierarchy,"incoming":list(ins.iter().take(300).cloned().collect(),ins.len()),"outgoing":list(outs.iter().take(200).cloned().collect(),outs.len()),"typesUsed":[],"counts":{"callers":ins.len(),"callees":outs.len(),"typesUsed":0,"fanIn":incoming.iter().filter(|e|e.kind!=EdgeKind::Contains).count(),"fanOut":outgoing.iter().filter(|e|e.kind!=EdgeKind::Contains).count(),"members":children.len(),"hub":ins.len()>=40},"tests":{"reached":!tests.is_empty(),"hops":if tests.is_empty(){None}else{Some(3)},"fileCount":tests.len(),"files":tests.iter().take(6).collect::<Vec<_>>(),"exhaustive":false,"hopsSearched":3},"outsideIndex":{"total":unresolved.len(),"byKind":kinds,"samples":unresolved.iter().take(40).map(|r|json!({"name":r.reference_name,"kind":r.reference_kind,"line":r.line,"col":r.column})).collect::<Vec<_>>()},"blast":{"direct":ins.len(),"withinHops":impact.nodes.len().saturating_sub(1),"hops":3,"files":byfile.len(),"testFiles":tests.len(),"routes":impact.nodes.values().filter(|n|n.kind==NodeKind::Route).count(),"topFiles":byfile.into_iter().take(8).map(|(file,symbols)|json!({"test":codegraph_search::is_test_file(&file),"file":file,"symbols":symbols})).collect::<Vec<_>>()},"drift":drift(cg,&n.file_path)}),
    )
}
pub fn file(cg: &CodeGraph, path: &str) -> Result<Value> {
    let (record, _) = resolve_file(cg, path)?;
    let nodes = cg.get_nodes_by_file(path)?;
    let ids: Vec<_> = nodes.iter().map(|n| n.id.clone()).collect();
    let incoming = cg.store().get_incoming_edges_to(&ids, &[])?;
    let outgoing = cg.store().get_outgoing_edges_from(&ids, &[])?;
    let counts_in = cg.store().count_incoming_edges(&ids)?;
    let counts_out = cg.store().count_outgoing_edges(&ids)?;
    let mut outline = vec![];
    for n in nodes
        .iter()
        .filter(|n| !matches!(n.kind, NodeKind::File | NodeKind::Import | NodeKind::Export))
    {
        let mut v = node_ref(n);
        let ancestors = cg.get_ancestors(&n.id)?;
        let parents: Vec<_> = ancestors
            .iter()
            .filter(|n| n.kind != NodeKind::File)
            .collect();
        v["parentId"] = json!(parents.last().map(|n| &n.id));
        v["depth"] = json!(parents.len());
        v["fanIn"] = json!(counts_in.get(&n.id).copied().unwrap_or(0));
        v["fanOut"] = json!(counts_out.get(&n.id).copied().unwrap_or(0));
        outline.push(v);
    }
    outline.sort_by_key(|n| n["line"].as_u64());
    let import_rows = |edges: &[Edge], incoming: bool| -> Result<Value> {
        let es: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Imports)
            .cloned()
            .collect();
        let relations = relations(cg, &es, incoming)?;
        let mut files = IndexMap::<String, Vec<Value>>::new();
        for rel in relations {
            let n = &rel["node"];
            let file = n["file"].as_str().unwrap();
            if file != path {
                files
                    .entry(file.into())
                    .or_default()
                    .push(json!({"id":n["id"],"name":n["name"],"kind":n["kind"],"line":n["line"]}));
            }
        }
        let total = files.len();
        Ok(list(files.into_iter().take(300).map(|(file,symbols)|json!({"test":codegraph_search::is_test_file(&file),"file":file,"symbolCount":symbols.len(),"symbols":symbols.into_iter().take(12).collect::<Vec<_>>()})).collect(),total))
    };
    let id = nodes
        .iter()
        .find(|n| n.kind == NodeKind::File)
        .map(|n| &n.id);
    let unresolved = cg.store().get_unresolved_references_in_file(path, 100)?;
    Ok(
        json!({"file":{"path":path,"language":record.language,"size":record.size,"modifiedAt":record.modified_at,"indexedAt":record.indexed_at,"contentHash":record.content_hash,"nodeCount":record.node_count,"generated":record.generated.unwrap_or(false),"test":codegraph_search::is_test_file(path),"errors":record.errors,"id":id},"topLevel":{"calls":outgoing.iter().filter(|e|Some(&e.source)==id&&matches!(e.kind,EdgeKind::Calls|EdgeKind::Instantiates)).count()},"drift":drift(cg,path),"outline":list(outline.clone(),outline.len()),"imports":import_rows(&outgoing,false)?,"importedBy":import_rows(&incoming,true)?,"unresolvedImports":unresolved.iter().filter(|r|r.reference_kind.as_str()=="imports").map(|r|json!({"name":r.reference_name,"line":r.line})).collect::<Vec<_>>(),"dependencies":cg.get_file_dependencies(path)?,"dependents":cg.get_file_dependents(path)?}),
    )
}
