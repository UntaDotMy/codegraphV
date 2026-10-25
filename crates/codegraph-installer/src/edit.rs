use crate::Result;
use jsonc_parser::cst::{CstInputValue, CstRootNode};
use serde_json::Value;
// jsonc-parser 0.26's CST encoder escapes quotes but leaves backslashes and
// control characters literal. Escape those first, notably for Windows paths.
fn cst_string(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\\' => "\\\\".into(),
            '\n' => "\\n".into(),
            '\r' => "\\r".into(),
            '\t' => "\\t".into(),
            c if c < ' ' => format!("\\u{:04x}", c as u32),
            c => c.to_string(),
        })
        .collect()
}
fn input(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(b) => CstInputValue::Bool(*b),
        Value::Number(n) => CstInputValue::Number(n.to_string()),
        Value::String(s) => CstInputValue::String(cst_string(s)),
        Value::Array(a) => CstInputValue::Array(a.iter().map(input).collect()),
        Value::Object(o) => {
            CstInputValue::Object(o.iter().map(|(k, v)| (k.clone(), input(v))).collect())
        }
    }
}
pub fn json(text: &str, path: &[&str], value: Option<&Value>) -> Result<String> {
    if text.trim().is_empty() && value.is_none() {
        return Ok(text.into());
    }
    let source = if text.trim().is_empty() { "{}\n" } else { text };
    let root = CstRootNode::parse(source, &Default::default())?;
    let Some(mut object) = root.object_value() else {
        return Err("Agent configuration must be a JSON object; nothing was changed.".into());
    };
    let parsed =
        jsonc_parser::parse_to_serde_value(source, &Default::default())?.unwrap_or(Value::Null);
    let mut old = &parsed;
    for key in path {
        old = &old[key];
    }
    if value == Some(old) || value.is_none() && old.is_null() {
        return Ok(text.into());
    }
    for key in &path[..path.len() - 1] {
        match object.object_value(key) {
            Some(child) => object = child,
            None if value.is_none() => return Ok(text.into()),
            None => {
                if object.get(key).is_some() {
                    return Err(format!(
                        "Configuration key {key} must be an object; nothing was changed."
                    )
                    .into());
                }
                object = object.object_value_or_set(key);
            }
        }
    }
    let key = path[path.len() - 1];
    if let Some(value) = value {
        if let Some(prop) = object.get(key) {
            prop.set_value(input(value));
        } else {
            object.append(key, input(value));
        }
    } else if let Some(prop) = object.get(key) {
        prop.remove();
    }
    Ok(root.to_string())
}
pub fn toml(text: &str, server: &str, value: Option<&Value>) -> Result<String> {
    if text.trim().is_empty() && value.is_none() {
        return Ok(text.into());
    }
    let mut doc = text.parse::<toml_edit::DocumentMut>()?;
    if let Some(entry) = value {
        let expected_command = entry["command"].as_str().unwrap();
        let expected_args = entry["args"].as_array().unwrap();
        let existing = doc.get("mcp_servers").and_then(|m| m.get(server));
        if existing.is_some_and(|v| {
            v.get("command").and_then(|v| v.as_str()) == Some(expected_command)
                && v.get("args").and_then(|v| v.as_array()).is_some_and(|a| {
                    a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>()
                        == expected_args
                            .iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                })
        }) {
            return Ok(text.into());
        }
        if let Some(parent) = doc.get("mcp_servers") {
            if !parent.is_table() {
                return Err("mcp_servers is defined as a value; add CodeGraphV manually or use a TOML table. Nothing was changed.".into());
            }
        }
        if doc.get("mcp_servers").is_none() {
            let mut parent = toml_edit::Table::new();
            parent.set_implicit(true);
            doc["mcp_servers"] = toml_edit::Item::Table(parent);
        }
        let mut table = toml_edit::Table::new();
        table["command"] = toml_edit::value(expected_command);
        let mut args = toml_edit::Array::new();
        for arg in expected_args {
            args.push(arg.as_str().unwrap());
        }
        table["args"] = toml_edit::value(args);
        doc["mcp_servers"][server] = toml_edit::Item::Table(table);
    } else {
        let Some(parent) = doc.get_mut("mcp_servers").and_then(|v| v.as_table_mut()) else {
            return Ok(text.into());
        };
        if parent.remove(server).is_none() {
            return Ok(text.into());
        }
    }
    let result = doc.to_string();
    Ok(if text.contains("\r\n") {
        result.replace("\r\n", "\n").replace('\n', "\r\n")
    } else {
        result
    })
}
fn indent(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}
fn key(line: &str, wanted: &str) -> bool {
    line.trim_start()
        .strip_prefix(wanted)
        .is_some_and(|s| s.starts_with(':'))
}
fn end(lines: &[String], start: usize) -> usize {
    let depth = indent(&lines[start]);
    (start + 1..lines.len())
        .find(|i| {
            !lines[*i].trim().is_empty()
                && !lines[*i].trim_start().starts_with('#')
                && indent(&lines[*i]) <= depth
        })
        .unwrap_or(lines.len())
}
pub fn yaml_configured(text: &str, server: &str) -> bool {
    let lines: Vec<_> = text.lines().map(str::to_owned).collect();
    let Some(top) = lines
        .iter()
        .position(|l| indent(l) == 0 && key(l, "mcp_servers"))
    else {
        return false;
    };
    (top + 1..end(&lines, top)).any(|i| key(&lines[i], server))
}
pub fn yaml(text: &str, server: &str, value: Option<&Value>) -> Result<String> {
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<_> = text.lines().map(str::to_owned).collect();
    let top = lines
        .iter()
        .position(|l| indent(l) == 0 && key(l, "mcp_servers"));
    let top = match top {
        Some(i) => i,
        None => {
            if value.is_none() {
                return Ok(text.into());
            }
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push("mcp_servers:".into());
            lines.len() - 1
        }
    };
    if lines[top]
        .split_once(':')
        .unwrap()
        .1
        .trim()
        .trim_start_matches('#')
        .trim()
        .is_empty()
        || lines[top]
            .split_once(':')
            .unwrap()
            .1
            .trim()
            .starts_with('#')
    {
    } else {
        return Err("mcp_servers uses an inline YAML value; add CodeGraphV manually or use block YAML. Nothing was changed.".into());
    }
    let finish = end(&lines, top);
    let found = (top + 1..finish).find(|i| key(&lines[*i], server));
    match (value, found) {
        (None, None) => return Ok(text.into()),
        (None, Some(i)) => {
            let finish = end(&lines, i);
            lines.drain(i..finish);
        }
        (Some(entry), Some(i)) => {
            // User-owned settings, including enabled:false and custom env, stay intact.
            let tail = lines[i].split_once(':').unwrap().1.trim();
            if !tail.is_empty() && !tail.starts_with('#') {
                return Ok(text.into());
            }
            let finish = end(&lines, i);
            let depth = indent(&lines[i]) + 2;
            let mut missing = vec![];
            if !(i + 1..finish).any(|j| key(&lines[j], "command")) {
                missing.push(format!(
                    "{}command: {}",
                    " ".repeat(depth),
                    serde_json::to_string(&entry["command"])?
                ));
            }
            if !(i + 1..finish).any(|j| key(&lines[j], "args")) {
                missing.push(format!("{}args: [serve, --mcp]", " ".repeat(depth)));
            }
            if missing.is_empty() {
                return Ok(text.into());
            }
            lines.splice(finish..finish, missing);
        }
        (Some(entry), None) => {
            lines.splice(
                finish..finish,
                [
                    format!("  {server}:"),
                    format!("    command: {}", serde_json::to_string(&entry["command"])?),
                    "    args: [serve, --mcp]".into(),
                ],
            );
        }
    }
    Ok(lines.join(eol) + eol)
}
