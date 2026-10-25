use codegraph_context::{builder::build_call_paths_section, formatter::*, ContextBuilder};
use codegraph_db::Store;
use codegraph_types::*;
use serde_json::{json, Value};

pub fn run(store: &Store, root: &str, q: &Value) -> Value {
    store.set_project_name_tokens(
        serde_json::from_value(q.get("projectTokens").cloned().unwrap_or(json!([]))).unwrap(),
    );
    let deprioritized: Vec<String> =
        serde_json::from_value(q.get("deprioritized").cloned().unwrap_or(json!([]))).unwrap();
    let deprioritized = |p: &str| deprioritized.iter().any(|d| d == p);
    let b = ContextBuilder::new(root, store).with_deprioritizer(&deprioritized);
    let opts = q.get("options").cloned().unwrap_or(json!({}));
    let query = q["query"].as_str().unwrap_or("");
    match q["op"].as_str().unwrap() {
        "find" => {
            let g = b
                .find_relevant_context(query, &serde_json::from_value(opts).unwrap())
                .unwrap();
            json!({"nodes": g.nodes.into_values().map(|n| n.id).collect::<Vec<_>>(), "edges": g.edges.into_iter().map(|e| json!([e.source,e.target,e.kind,e.line,e.column])).collect::<Vec<_>>(), "roots":g.roots,"confidence":g.confidence})
        }
        "build" => {
            let input: TaskInput =
                serde_json::from_value(q.get("input").cloned().unwrap_or(json!(query))).unwrap();
            let c = b
                .build_task_context(&input, &serde_json::from_value(opts).unwrap())
                .unwrap();
            context_json(&c)
        }
        "markdown" | "json" => {
            let input: TaskInput =
                serde_json::from_value(q.get("input").cloned().unwrap_or(json!(query))).unwrap();
            match b
                .build_context(&input, &serde_json::from_value(opts).unwrap())
                .unwrap()
            {
                codegraph_context::BuildOutput::Text(s) => json!(s),
                _ => unreachable!(),
            }
        }
        "tree" | "paths" => {
            let g = b
                .find_relevant_context(query, &serde_json::from_value(opts).unwrap())
                .unwrap();
            if q["op"] == "paths" {
                json!(build_call_paths_section(&g))
            } else {
                let entries: Vec<_> = g
                    .roots
                    .iter()
                    .filter_map(|id| g.nodes.get(id).cloned())
                    .collect();
                json!(format_subgraph_tree(&g, &entries))
            }
        }
        "code" => json!(b.get_code(q["id"].as_str().unwrap()).unwrap()),
        "bytes" => json!(format_bytes(q["bytes"].as_f64().unwrap())),
        "segments" => json!(codegraph_context::segments::get_segment_matches(
            store,
            &serde_json::from_value::<Vec<String>>(q["words"].clone()).unwrap(),
            q["limit"].as_u64().unwrap_or(6) as usize
        )
        .unwrap()),
        _ => panic!("unknown query: {q}"),
    }
}
