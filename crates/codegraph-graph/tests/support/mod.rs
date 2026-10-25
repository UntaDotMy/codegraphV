use codegraph_db::Store;
use codegraph_graph::named_symbol_flow::{
    find_all_symbols, flow_tokens, normalize_token, resolve_named_symbol_flow, resolve_named_tokens,
};
use codegraph_graph::symbol_lookup::{
    describe_symbol_node, group_definitions, is_qualified_symbol, last_qualifier_part,
    lookup_symbol_nodes, matches_symbol, qualified_symbol_matches, StoreLookup,
};
use codegraph_graph::synthesized_hop::describe_synthesized_hop;
use codegraph_graph::type_hierarchy::{build_type_hierarchy, count_implementers};
use codegraph_graph::{GraphQueryManager, GraphTraverser};
use codegraph_types::{Direction, EdgeKind, NodeKind, Subgraph, TraversalOptions};
use serde_json::{json, Value};

fn subgraph(g: Subgraph) -> Value {
    json!({ "nodes": g.nodes.into_values().collect::<Vec<_>>(), "edges": g.edges, "roots": g.roots })
}

pub fn run_query(store: &Store, q: &Value) -> Value {
    store.set_project_name_tokens(
        q["projectTokens"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
    );
    let t = GraphTraverser::new(store);
    let manager = GraphQueryManager::new(store);
    let id = q["id"].as_str().unwrap_or("missing");
    let depth = q["depth"].as_u64().unwrap_or(2) as u32;
    let kinds: Vec<EdgeKind> = q["kinds"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().and_then(EdgeKind::parse))
                .collect()
        })
        .unwrap_or_default();
    let search =
        |query: &str, limit: usize| -> codegraph_graph::Result<Vec<codegraph_types::Node>> {
            let key = format!("{query}|{limit}");
            let ids = q["searches"][&key]
                .as_array()
                .unwrap_or_else(|| panic!("missing oracle search inputs: {key}"));
            let native: Vec<_> = store
                .search_nodes(
                    query,
                    &codegraph_types::SearchOptions {
                        limit: Some(limit),
                        ..Default::default()
                    },
                )?
                .into_iter()
                .map(|r| r.node)
                .collect();
            assert_eq!(
                json!(native.iter().map(|n| &n.id).collect::<Vec<_>>()),
                json!(ids),
                "native search order for {key}"
            );
            Ok(native)
        };
    let lookup = StoreLookup {
        store,
        search: &search,
    };
    let go = |file: &str| q["goPackages"][file].as_str().map(str::to_owned);
    match q["op"].as_str().unwrap() {
        "dominantFile" => json!(store.get_dominant_file().unwrap()),
        "topRouteFile" => json!(store.get_top_route_file().unwrap()),
        "search" => {
            let options = search_options(&q["options"]);
            let deprioritized = |p: &str| {
                q["deprioritized"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|v| v == p))
            };
            json!(store
                .search_nodes_with_deprioritizer(
                    q["query"].as_str().unwrap(),
                    &options,
                    Some(&deprioritized)
                )
                .unwrap())
        }
        "searchExact" => {
            let names: Vec<String> = serde_json::from_value(q["names"].clone()).unwrap();
            json!(store
                .find_nodes_by_exact_name(&names, &search_options(&q["options"]))
                .unwrap())
        }
        "searchSubstring" => json!(store
            .find_nodes_by_name_substring(
                q["query"].as_str().unwrap(),
                &search_options(&q["options"]),
                q["options"]["excludePrefix"].as_bool().unwrap_or(false)
            )
            .unwrap()),
        "bfs" | "dfs" => {
            let opts = &q["opts"];
            let options = TraversalOptions {
                max_depth: opts["maxDepth"].as_u64().map(|n| n as u32),
                limit: opts["limit"].as_u64().map(|n| n as usize),
                include_start: opts["includeStart"].as_bool(),
                direction: opts["direction"].as_str().and_then(Direction::parse),
                edge_kinds: opts["edgeKinds"].as_array().map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().and_then(EdgeKind::parse))
                        .collect()
                }),
                node_kinds: opts["nodeKinds"].as_array().map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().and_then(NodeKind::parse))
                        .collect()
                }),
            };
            subgraph(
                if q["op"] == "bfs" {
                    t.traverse_bfs(id, &options)
                } else {
                    t.traverse_dfs(id, &options)
                }
                .unwrap(),
            )
        }
        "callers" => json!(t.get_callers(id, depth).unwrap()),
        "callees" => json!(t.get_callees(id, depth).unwrap()),
        "callGraph" => subgraph(t.get_call_graph(id, depth).unwrap()),
        "impact" => subgraph(t.get_impact_radius(id, depth).unwrap()),
        "path" => json!(t.find_path(id, q["to"].as_str().unwrap(), &kinds).unwrap()),
        "ancestors" => json!(t.get_ancestors(id).unwrap()),
        "children" => json!(t.get_children(id).unwrap()),
        "usages" => json!(t.find_usages(id).unwrap()),
        "legacyHierarchy" => subgraph(t.get_type_hierarchy(id).unwrap()),
        "context" => match manager.get_context(id) {
            Ok(v) => json!(v),
            Err(e) => json!({ "error": e.to_string() }),
        },
        "metrics" => json!(manager.get_node_metrics(id).unwrap()),
        "dependencies" => json!(manager
            .get_file_dependencies(q["file"].as_str().unwrap())
            .unwrap()),
        "dependents" => json!(manager
            .get_file_dependents(q["file"].as_str().unwrap())
            .unwrap()),
        "exports" => json!(manager
            .get_exported_symbols(q["file"].as_str().unwrap())
            .unwrap()),
        "modules" => json!(manager.get_module_structure().unwrap()),
        "cycles" => json!(manager.find_circular_dependencies().unwrap()),
        "deadCode" => json!(manager.find_dead_code(None).unwrap()),
        "qualified" => json!(manager
            .find_by_qualified_name(q["pattern"].as_str().unwrap())
            .unwrap()),
        "filtered" => subgraph(
            manager
                .get_filtered_subgraph(
                    |n| q["file"].as_str().is_none_or(|f| n.file_path == f),
                    q["includeEdges"].as_bool().unwrap_or(true),
                )
                .unwrap(),
        ),
        "hierarchy" => match store.get_node_by_id(id).unwrap() {
            None => Value::Null,
            Some(focus) => {
                json!(
                    build_type_hierarchy(store, &focus, q["overrides"].as_bool().unwrap_or(true))
                        .unwrap()
                )
            }
        },
        "implementers" => json!(count_implementers(store, id).unwrap()),
        "between" => {
            let ids: Vec<String> = serde_json::from_value(q["ids"].clone()).unwrap();
            json!(store.find_edges_between_nodes(&ids, &kinds).unwrap())
        }
        "hop" => json!(describe_synthesized_hop(
            serde_json::from_value::<Option<codegraph_types::Edge>>(q["edge"].clone())
                .unwrap()
                .as_ref()
        )),
        "flowTokens" => {
            json!({ "tokens": flow_tokens(q["query"].as_str().unwrap()), "normalized": normalize_token(q["query"].as_str().unwrap()) })
        }
        "symbol" => {
            let symbol = q["symbol"].as_str().unwrap();
            let node = store.get_node_by_id(id).unwrap().unwrap();
            json!({ "matches": matches_symbol(&node, symbol, Some(&go)), "lastPart": last_qualifier_part(symbol), "qualified": is_qualified_symbol(symbol), "description": describe_symbol_node(&node) })
        }
        "groupDefinitions" => json!(group_definitions(
            &store.get_all_nodes().unwrap(),
            q["fileFilter"].as_str()
        )),
        "lookupSymbol" => {
            json!(lookup_symbol_nodes(&lookup, q["symbol"].as_str().unwrap(), Some(&go)).unwrap())
        }
        "qualifiedSymbols" => {
            json!(
                qualified_symbol_matches(&lookup, q["symbol"].as_str().unwrap(), Some(&go))
                    .unwrap()
            )
        }
        "allSymbols" => {
            json!(find_all_symbols(&lookup, q["symbol"].as_str().unwrap(), Some(&go)).unwrap())
        }
        "flow" | "tokens" => {
            let options =
                serde_json::from_value(q.get("options").cloned().unwrap_or(json!({}))).unwrap();
            json!(if q["op"] == "flow" {
                resolve_named_symbol_flow(
                    store,
                    &lookup,
                    q["query"].as_str().unwrap(),
                    &options,
                    Some(&go),
                )
            } else {
                resolve_named_tokens(
                    store,
                    &lookup,
                    q["query"].as_str().unwrap(),
                    &options,
                    Some(&go),
                )
            }
            .unwrap())
        }
        other => panic!("unknown query {other}"),
    }
}

fn search_options(v: &Value) -> codegraph_types::SearchOptions {
    codegraph_types::SearchOptions {
        kinds: v["kinds"].as_array().map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().and_then(NodeKind::parse))
                .collect()
        }),
        languages: v["languages"].as_array().map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().and_then(codegraph_types::Language::parse))
                .collect()
        }),
        limit: v["limit"].as_u64().map(|n| n as usize),
        offset: v["offset"].as_u64().map(|n| n as usize),
        ..Default::default()
    }
}
