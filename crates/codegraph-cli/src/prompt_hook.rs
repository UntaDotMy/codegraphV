//! `codegraph prompt-hook` (hidden; src/bin/codegraph.ts): Claude Code's `UserPromptSubmit`
//! hook. It reads `{prompt, cwd}` JSON on stdin and, for a structural / flow / impact prompt,
//! prints CodeGraph context that Claude injects into the agent's turn — so the agent's reflex
//! grep/read has nothing left to find.
//!
//! LOAD-BEARING: it must never break the user's prompt. Every failure path — kill-switch,
//! non-structural prompt, no index, engine error — prints nothing and exits 0.
use codegraph_core::{
    directory::plan_frontload,
    prompt_hook::{
        cap_prompt_hook_injection_default, extract_code_tokens, has_structural_keyword, is_agent_message,
        is_cross_session_message, is_task_notification, strip_system_reminders,
    },
    CodeGraph, OpenOptions,
};
use codegraph_search::identifier_segments::extract_prose_candidates;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{IsTerminal, Read},
};

fn nudge(projects: &[String], lead: &str) -> String {
    let lines: Vec<String> = projects.iter().map(|p| format!("  - projectPath: \"{p}\"")).collect();
    format!("{lead}\n{}\n", lines.join("\n"))
}

/// What the hook prints for `prompt` issued from `cwd`; None prints nothing.
pub fn frontload(cwd: &str, prompt: &str) -> Option<String> {
    // The desktop app's <system-reminder> blocks are host status, not what the user typed.
    let prompt = strip_system_reminders(prompt);
    // Task notifications, subagent hand-backs, cross-session messages and nothing at all are
    // not user prompts.
    if prompt.is_empty() || is_task_notification(&prompt) || is_agent_message(&prompt) || is_cross_session_message(&prompt) {
        return None;
    }
    // HIGH: a structural keyword, or a code-shaped token verified in the index → explore.
    // MEDIUM: prose words matching indexed symbol-name segments → name them, the agent queries.
    let keyworded = has_structural_keyword(&prompt);
    let code_tokens = if keyworded { Vec::new() } else { extract_code_tokens(&prompt) };
    let prose_words = if keyworded { Vec::new() } else { extract_prose_candidates(&prompt) };
    if !keyworded && code_tokens.is_empty() && prose_words.is_empty() {
        return None;
    }
    let plan = plan_frontload(cwd, &prompt);
    let Some(root) = plan.explore_root.clone() else {
        if plan.nudge_projects.is_empty() {
            return None;
        }
        // Several indexed sub-projects, none a clear match: say they exist and how to query one.
        return Some(format!(
            "<codegraphV_context note=\"CodeGraphV is available for this workspace's indexed sub-projects — query one by passing projectPath to codegraphV_explore.\">\n{}</codegraphV_context>\n",
            nudge(
                &plan.nudge_projects,
                "This workspace's CodeGraphV indexes live in sub-projects. To use CodeGraphV, call codegraphV_explore with the projectPath of the relevant one:"
            )
        ));
    };
    // Read-only: the hook runs on its own for every structural prompt, so it must never run the
    // writer's repairs inside a full index, and it stays out of a running rebuild altogether: the
    // index is half-built until it ends, and the hook would hold the file the rebuild replaces.
    // Its one write, filling an empty segment vocabulary, goes through a writer of its own.
    let cg = CodeGraph::open(&root, OpenOptions { read_only: true, ..OpenOptions::default() }).ok()?;
    if cg.is_rebuild_in_progress() {
        cg.close();
        return None;
    }
    let out = frontload_project(&cg, &root, &prompt, keyworded, &code_tokens, &prose_words, &plan.nudge_projects, plan.via_sub_scan);
    cg.close();
    out
}

#[allow(clippy::too_many_arguments)]
fn frontload_project(
    cg: &CodeGraph,
    root: &str,
    prompt: &str,
    keyworded: bool,
    code_tokens: &[String],
    prose_words: &[String],
    nudge_projects: &[String],
    via_sub_scan: bool,
) -> Option<String> {
    let others = if nudge_projects.is_empty() {
        String::new()
    } else {
        format!("\n{}", nudge(nudge_projects, "Other indexed projects in this workspace — pass projectPath to query them:"))
    };
    // A token is only a candidate: it must name a real symbol in THIS index.
    let token_verified =
        !keyworded && code_tokens.iter().any(|t| cg.get_nodes_by_name(t).is_ok_and(|nodes| !nodes.is_empty()));
    if keyworded || token_verified {
        let result = codegraph_mcp::render::explore(cg, &json!({ "query": prompt }), &mut HashMap::new()).ok()?;
        let text = result.content.first().and_then(|c| c.get("text")).and_then(Value::as_str).unwrap_or_default();
        if result.is_error || text.trim().is_empty() {
            return None;
        }
        let body = cap_prompt_hook_injection_default(text);
        let more = if via_sub_scan {
            format!("call codegraphV_explore with projectPath: \"{root}\" for more")
        } else {
            "call codegraphV_explore for more".to_string()
        };
        return Some(format!(
            "<codegraphV_context note=\"Structural context from CodeGraphV for this prompt — treat returned source as already read; {more}.\">\n{body}{others}\n</codegraphV_context>\n"
        ));
    }
    // MEDIUM. A database indexed before the vocab table existed starts with it empty; heal it.
    if !cg.heal_segment_vocab_if_empty() {
        return None;
    }
    let related = cg.get_segment_matches(prose_words, 6).ok()?;
    if related.is_empty() {
        return None;
    }
    let lines: Vec<String> = related
        .iter()
        .map(|m| format!("  - {} ({} — {}:{})", m.name, m.kind.as_str(), m.file_path, m.start_line))
        .collect();
    let example_query = related.iter().take(3).map(|m| m.name.as_str()).collect::<Vec<_>>().join(" ");
    let project_hint = if via_sub_scan { format!(" with projectPath: \"{root}\"") } else { String::new() };
    Some(format!(
        "<codegraphV_context note=\"CodeGraphV found indexed symbols matching this prompt — query the graph before searching files.\">\nThis project's CodeGraphV index contains symbols matching this request:\n{}\nCall codegraphV_explore ONCE{project_hint} with the relevant names in one query (e.g. \"{example_query}\") to get their source, call paths, and blast radius — cheaper and more complete than Read/Grep.\n{others}</codegraphV_context>\n",
        lines.join("\n")
    ))
}

/// The command: reads the hook payload and prints the context, never failing.
pub fn run() {
    // Kill-switch: disable the nudge without uninstalling or editing settings.json.
    if std::env::var("CODEGRAPHV_NO_PROMPT_HOOK").as_deref() == Ok("1")
        || std::env::var("CODEGRAPHV_PROMPT_HOOK").as_deref() == Ok("0")
    {
        return;
    }
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return; // invoked by hand, no piped payload
    }
    let mut raw = String::new();
    if stdin.lock().read_to_string(&mut raw).is_err() {
        return;
    }
    let Ok(input) = serde_json::from_str::<Value>(&raw) else { return };
    let prompt = match &input["prompt"] {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    let cwd = input["cwd"]
        .as_str()
        .filter(|c| !c.is_empty())
        .map(String::from)
        .or_else(|| std::env::current_dir().ok().map(|d| d.to_string_lossy().into_owned()))
        .unwrap_or_default();
    // Degradable by contract: a panic anywhere below prints nothing, not even its message.
    std::panic::set_hook(Box::new(|_| {}));
    if let Ok(Some(text)) = std::panic::catch_unwind(|| frontload(&cwd, &prompt)) {
        print!("{text}");
    }
}
