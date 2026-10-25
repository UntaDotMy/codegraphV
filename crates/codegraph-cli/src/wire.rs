//! Stable JSON contracts of the existing command-line interface.
use codegraph_core::CodeGraph;
use codegraph_types::*;
use serde_json::{json, Value};
use std::collections::{HashSet, VecDeque};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
fn compact(n: &Value, id: bool) -> Value {
    let mut out = json!({"name":n["name"],"kind":n["kind"],"filePath":n["filePath"],"startLine":n["startLine"]});
    if id {
        out["id"] = n["id"].clone();
    }
    out
}
fn definition(n: &Value) -> Value {
    let mut out = compact(n, true);
    out["qualifiedName"] = n["qualifiedName"].clone();
    out["language"] = n["language"].clone();
    out
}
fn optional_filters(out: &mut Value, args: &Value) {
    if let Some(file) = args["file"].as_str() {
        out["file"] = json!(file);
        if out["filteredOut"] == true {
            out["note"]=json!(format!("no definition of \"{}\" matches file \"{file}\" — showing all definitions instead.",args["symbol"].as_str().unwrap_or_default()));
        }
    }
}
pub fn relations(cg: &CodeGraph, args: &Value, incoming: bool) -> Result<Value> {
    let rich = codegraph_mcp::render::relation_data(cg, args, incoming)?;
    let direction = if incoming { "callers" } else { "callees" };
    let endpoint = if incoming { "source" } else { "target" };
    let edges = rich["edges"].as_array().unwrap();
    let mut targets = vec![];
    let mut defs = vec![];
    let relationships = |id: &str| -> Value {
        let mut seen = HashSet::new();
        json!(edges
            .iter()
            .filter(|e| e[endpoint] == id)
            .filter_map(|e| e["kind"].as_str())
            .filter(|k| seen.insert(*k))
            .collect::<Vec<_>>())
    };
    for def in rich["definitions"].as_array().unwrap() {
        let overloads = def["overloads"].as_array().unwrap();
        targets.extend(overloads.iter().map(definition));
        let nodes = def["references"].as_array().unwrap();
        let shown: HashSet<_> = nodes
            .iter()
            .filter_map(|r| r["node"]["id"].as_str())
            .collect();
        let roots: HashSet<_> = overloads.iter().filter_map(|n| n["id"].as_str()).collect();
        let related = if incoming { "target" } else { "source" };
        let mut out = json!({"definition":definition(&def["definition"]),"roots":overloads.iter().map(|n|&n["id"]).collect::<Vec<_>>(),"edges":edges.iter().filter(|e|e[related].as_str().is_some_and(|id|roots.contains(id))&&e[endpoint].as_str().is_some_and(|id|shown.contains(id))).collect::<Vec<_>>(),"total":def["total"],"limit":rich["limit"],"truncated":def["truncated"]});
        out[direction] = json!(nodes
            .iter()
            .map(|r| {
                let mut n = compact(&r["node"], true);
                n["relationships"] = relationships(r["node"]["id"].as_str().unwrap());
                n
            })
            .collect::<Vec<_>>());
        defs.push(out);
    }
    let mut out = json!({"symbol":rich["symbol"],"targets":targets,"ambiguous":rich["ambiguous"],"aggregation":rich["aggregation"],"filteredOut":rich["filteredOut"],"definitions":defs,"total":rich["total"],"limit":rich["limit"],"truncated":rich["truncated"]});
    out[direction] = json!(rich[direction]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            let mut row = compact(n, false);
            row["relationships"] = relationships(n["id"].as_str().unwrap());
            row
        })
        .collect::<Vec<_>>());
    optional_filters(&mut out, args);
    Ok(out)
}
pub fn impact(cg: &CodeGraph, args: &Value) -> Result<Value> {
    let rich = codegraph_mcp::render::impact_data(cg, args)?;
    let mut defs = vec![];
    for def in rich["definitions"].as_array().unwrap() {
        let affected = def["affected"].as_array().unwrap();
        defs.push(json!({"definition":definition(&def["definition"]),"roots":def["roots"],"nodeCount":affected.len(),"edgeCount":def["edges"].as_array().unwrap().len(),"affected":affected.iter().map(|n|compact(n,true)).collect::<Vec<_>>(),"edges":def["edges"]}));
    }
    let mut out = json!({"symbol":rich["symbol"],"depth":rich["depth"],"targets":rich["targets"].as_array().unwrap().iter().map(definition).collect::<Vec<_>>(),"ambiguous":rich["ambiguous"],"aggregation":rich["aggregation"],"filteredOut":rich["filteredOut"],"definitions":defs,"nodeCount":rich["nodeCount"],"edgeCount":rich["edgeCount"],"affected":rich["affected"].as_array().unwrap().iter().map(|n|compact(n,false)).collect::<Vec<_>>()});
    optional_filters(&mut out, args);
    Ok(out)
}
pub fn glob(pattern: &str) -> Result<regex::Regex> {
    let mut out = String::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                out.push_str(".*");
            }
            '*' => out.push_str("[^/]*"),
            '?' => out.push_str("[^/]"),
            c => out.push_str(&regex::escape(&c.to_string())),
        }
    }
    Ok(regex::Regex::new(&out)?)
}
pub fn files(
    cg: &CodeGraph,
    filter: Option<&str>,
    pattern: Option<&str>,
) -> Result<Vec<FileRecord>> {
    let pattern = pattern.map(glob).transpose()?;
    Ok(cg
        .get_files()?
        .into_iter()
        .filter(|f| {
            filter.is_none_or(|p| f.path.starts_with(p) || f.path.starts_with(&format!("./{p}")))
                && pattern.as_ref().is_none_or(|p| p.is_match(&f.path))
        })
        .collect())
}
pub fn file_json(files: &[FileRecord]) -> Value {
    json!(files.iter().map(|f|{let errors=f.errors.as_deref().unwrap_or(&[]).iter().map(|e|{let mut out=json!({"severity":e.severity,"message":e.message});if let Some(code)=&e.code{out["code"]=json!(code);}if let Some(line)=e.line{out["line"]=json!(line);}out}).collect::<Vec<_>>();json!({"path":f.path,"language":f.language,"nodeCount":f.node_count,"size":f.size,"errors":errors})}).collect::<Vec<_>>())
}
pub fn affected(
    cg: &CodeGraph,
    files: &[String],
    depth: u32,
    filter: Option<&str>,
) -> Result<Value> {
    let changed: Vec<_> = files
        .iter()
        .map(|f| {
            let f = f.trim();
            let f = if std::path::Path::new(f).is_absolute() {
                nodepath::relative(cg.project_root(), f)
            } else {
                nodepath::relative(
                    cg.project_root(),
                    &nodepath::resolve(&[cg.project_root(), f]),
                )
            };
            f.replace('\\', "/").trim_start_matches("./").to_owned()
        })
        .filter(|f| !f.is_empty())
        .collect();
    let pattern = filter.map(glob).transpose()?;
    let is_test = |path: &str| {
        pattern
            .as_ref()
            .map(|p| p.is_match(path))
            .unwrap_or_else(|| codegraph_search::is_test_path(path))
    };
    let mut tests = std::collections::BTreeSet::new();
    let mut all = HashSet::new();
    for file in &changed {
        if is_test(file) {
            tests.insert(file.clone());
            continue;
        }
        let mut queue = VecDeque::from([(file.clone(), 0)]);
        let mut visited = HashSet::from([file.clone()]);
        while let Some((file, d)) = queue.pop_front() {
            if d >= depth {
                continue;
            }
            for dep in cg.get_file_dependents(&file)? {
                if !visited.insert(dep.clone()) {
                    continue;
                }
                all.insert(dep.clone());
                if is_test(&dep) {
                    tests.insert(dep);
                } else {
                    queue.push_back((dep, d + 1));
                }
            }
        }
    }
    Ok(json!({"changedFiles":changed,"affectedTests":tests,"totalDependentsTraversed":all.len()}))
}
