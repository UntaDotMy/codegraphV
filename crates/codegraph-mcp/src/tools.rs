use codegraph_core::{
    directory::{is_initialized, resolve_server_root},
    utils::validate_project_path,
    CodeGraph, OpenOptions, WatchOptions,
};
use codegraph_types::nodepath;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;

pub const TOOLS_JSON: &str = include_str!("assets/tools.json");
pub const INSTRUCTIONS: &str = include_str!("assets/server_instructions.md");
pub const NO_INDEX_INSTRUCTIONS: &str = include_str!("assets/server_instructions_no_root_index.md");

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub content: Vec<Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}
impl ToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![json!({"type":"text","text":text.into()})],
            is_error: false,
        }
    }
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            is_error: true,
            ..Self::text(text)
        }
    }
    pub fn message(&self) -> String {
        self.content
            .iter()
            .filter_map(|v| v["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub fn tool_definitions(require_project: bool) -> Vec<Value> {
    let mut definitions: Vec<Value> =
        serde_json::from_str(TOOLS_JSON).expect("bundled tool definitions");
    let raw = std::env::var("CODEGRAPH_MCP_TOOLS").unwrap_or_default();
    let selected: Vec<_> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.strip_prefix("codegraph_").unwrap_or(s))
        .collect();
    definitions.retain(|t| {
        let name = t["name"].as_str().unwrap().trim_start_matches("codegraph_");
        if raw.trim().is_empty() {
            name == "explore"
        } else {
            selected.is_empty() || selected.contains(&name)
        }
    });
    if require_project {
        for definition in &mut definitions {
            let schema = &mut definition["inputSchema"];
            if schema.get("required").is_none() {
                schema["required"] = json!([]);
            }
            let required = schema["required"].as_array_mut().unwrap();
            if !required.iter().any(|v| v == "projectPath") {
                required.push(json!("projectPath"));
            }
        }
    }
    definitions
}

pub struct ToolHandler {
    pub default_path: Option<String>,
    pub watch: bool,
    pub(crate) projects: IndexMap<String, CodeGraph>,
    pub(crate) shown: std::collections::HashMap<String, std::collections::HashSet<u32>>,
}
impl ToolHandler {
    pub fn new(default_path: Option<String>, watch: bool) -> Self {
        Self {
            default_path,
            watch,
            projects: IndexMap::new(),
            shown: Default::default(),
        }
    }
    pub fn default_root(&self) -> Option<String> {
        self.default_path
            .as_deref()
            .and_then(|p| resolve_server_root(p, true).root)
    }
    pub fn list_tools(&self) -> Vec<Value> {
        tool_definitions(self.default_root().is_none())
    }
    pub fn reset_session(&mut self) {
        self.shown.clear();
    }
    pub fn call(&mut self, name: &str, args: &Value) -> ToolResult {
        if !args.is_object() {
            return ToolResult::text("Tool arguments must be an object.");
        }
        let explicit = match args.get("projectPath") {
            Some(Value::String(s)) if !s.trim().is_empty() => Some(s.as_str()),
            Some(Value::Null) | None => None,
            _ => return ToolResult::text("projectPath must be a non-empty string."),
        };
        let input = explicit
            .or(self.default_path.as_deref())
            .map(str::to_string)
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
        let input = nodepath::resolve(&[&input]);
        if let Some(reason) = validate_project_path(&input) {
            return if reason.starts_with("Refusing") {
                ToolResult::error(reason)
            } else {
                ToolResult::text(reason)
            };
        }
        let resolution = resolve_server_root(&input, true);
        let Some(root) = resolution.root else {
            return ToolResult::text(format!("No CodeGraph index is available for {input}. Pass projectPath to an indexed project (one containing .codegraph/). Indexing is the user's decision; do not initialize a project automatically."));
        };
        if !is_initialized(&root) {
            return ToolResult::text(format!("Project is not indexed: {root}. Ask the user to run `codegraph init` if they want to index it."));
        }
        let root = codegraph_types::paths::real_path(Path::new(&root))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or(root);
        if !self.projects.contains_key(&root) {
            match CodeGraph::open(&root,OpenOptions::default()){
                Ok(mut cg)=>{
                    if self.watch{let _=cg.watch(WatchOptions::default());}
                    if self.projects.len()>=8{if let Some((_,old))=self.projects.shift_remove_index(0){old.close();}}
                    self.projects.insert(root.clone(),cg);
                },Err(error)=>return ToolResult::error(format!("Could not open CodeGraph: {error}. Retry once; if it persists, report the failure."))
            }
        }
        let (_, mut cg) = self.projects.shift_remove_entry(&root).unwrap();
        cg.reopen_if_replaced();
        let result = self.dispatch(&cg, name, args);
        let result = match result {
            Ok(result) => result,
            Err(error) => ToolResult::error(format!(
                "CodeGraph failed: {error}. Retry once; if it persists, report the failure."
            )),
        };
        let pending = cg.get_pending_files();
        let degraded = cg.get_watcher_degraded_reason();
        self.projects.insert(root, cg);
        if result.is_error {
            return result;
        }
        let mut text = String::new();
        if let Some(reason) = degraded {
            text.push_str(&format!("⚠ Index may be stale: {reason}\n\n"));
        }
        if !pending.is_empty() {
            text.push_str(&format!(
                "⚠ Pending edits: {}. The index is catching up.\n\n",
                pending
                    .iter()
                    .take(20)
                    .map(|p| p.path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if let Some(m) =
            codegraph_sync_notice(&input, self.projects.last().unwrap().1.project_root())
        {
            text.push_str(&m);
            text.push_str("\n\n");
        }
        text.push_str(&result.message());
        ToolResult::text(text)
    }
    fn dispatch(
        &mut self,
        cg: &CodeGraph,
        name: &str,
        args: &Value,
    ) -> Result<ToolResult, Box<dyn std::error::Error>> {
        use crate::render;
        let text = match name {
            "codegraph_search" => render::search(cg, args)?,
            "codegraph_callers" => render::relations(cg, args, true)?,
            "codegraph_callees" => render::relations(cg, args, false)?,
            "codegraph_impact" => render::impact(cg, args)?,
            "codegraph_node" => return render::node(cg, args),
            "codegraph_explore" => return render::explore(cg, args, &mut self.shown),
            "codegraph_files" => render::files(cg, args)?,
            "codegraph_status" => serde_json::to_string_pretty(&render::status(cg)?)?,
            _ => return Ok(ToolResult::text(format!("Unknown tool: {name}"))),
        };
        Ok(ToolResult::text(text))
    }
}
impl Drop for ToolHandler {
    fn drop(&mut self) {
        for (_, cg) in std::mem::take(&mut self.projects) {
            cg.close();
        }
    }
}
fn codegraph_sync_notice(start: &str, index: &str) -> Option<String> {
    codegraph_core::sync::worktree::detect_worktree_index_mismatch(start, index)
        .map(|m| codegraph_core::sync::worktree::worktree_mismatch_notice(&m))
}

pub fn number(args: &Value, name: &str, default: usize, min: usize, max: usize) -> usize {
    args.get(name)
        .and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| n as usize)
        .unwrap_or(default)
        .clamp(min, max)
}
pub fn required<'a>(args: &'a Value, name: &str) -> Result<&'a str, ToolResult> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.chars().count() <= 10_000)
        .ok_or_else(|| {
            ToolResult::text(format!(
                "{name} must be a non-empty string (at most 10000 characters)."
            ))
        })
}
