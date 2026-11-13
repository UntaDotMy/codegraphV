//! DeepSeek Harness (`dsh`) keeps MCP servers as rows for the `@deepseek-ai/dsh-mcp-client`
//! plugin in a Cordis patch file, `$DSH_HOME/cordis.patch.yml` (upstream 61e16218). The file is
//! the user's, so it is edited as text, line by line: comments and other patches survive. dsh
//! refuses to start unless the file is a top-level array, so a comments-only result becomes `[]`.
//! A server is identified by its `serverName`; an entry the user already has is never rewritten.
use crate::Result;
use regex::Regex;

pub const PATCH_FILE: &str = "cordis.patch.yml";
const CLIENT: &str = "@deepseek-ai/dsh-mcp-client";

/// A span of lines: `start` up to, not including, `end`.
#[derive(Clone, Copy)]
struct Range {
    start: usize,
    end: usize,
}

struct Text {
    lines: Vec<String>,
    eol: &'static str,
}
impl Text {
    fn split(text: &str) -> Self {
        let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
        let body = text.strip_suffix(eol).unwrap_or(text);
        let lines = if text.is_empty() { Vec::new() } else { body.split(eol).map(str::to_owned).collect() };
        Self { lines, eol }
    }
    fn join(&self) -> String {
        if self.lines.is_empty() {
            return String::new();
        }
        self.lines.join(self.eol) + self.eol
    }
}

fn names_server(server: &str) -> Regex {
    Regex::new(&format!(r#"(^|[\s{{,])["']?serverName["']?\s*:\s*["']?{}["']?\s*([,}}#]|$)"#, regex::escape(server))).unwrap()
}
fn names_client() -> Regex {
    Regex::new(&format!(r#"(^|[\s{{,])["']?name["']?\s*:\s*["']?{}["']?"#, regex::escape(CLIENT))).unwrap()
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}
/// A line that holds no value: blank, a comment, or the `---` that opens a document.
fn is_trivia(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t == "---" || t.starts_with('#')
}
fn is_top_level_item(line: &str) -> bool {
    line == "-" || line.starts_with("- ") || line.starts_with("-\t")
}
fn is_nested_item(line: &str) -> bool {
    let rest = line.trim_start_matches(' ');
    rest.len() < line.len() && (rest == "-" || rest.starts_with("- ") || rest.starts_with("-\t"))
}
/// Where a span ends, before the blank and comment lines leading up to what follows it.
fn trimmed_end(lines: &[String], start: usize, mut end: usize) -> usize {
    while end > start + 1 && is_trivia(&lines[end - 1]) {
        end -= 1;
    }
    end
}
fn top_level_items(lines: &[String]) -> Vec<Range> {
    let starts: Vec<usize> = (0..lines.len()).filter(|&i| is_top_level_item(&lines[i])).collect();
    starts
        .iter()
        .enumerate()
        .map(|(n, &start)| Range { start, end: trimmed_end(lines, start, starts.get(n + 1).copied().unwrap_or(lines.len())) })
        .collect()
}
/// The rows of a `- insert:` item: its list items, at the indent of the first.
fn insert_rows(lines: &[String], item: Range) -> Vec<Range> {
    let head = Regex::new(r"^-\s+insert\s*:\s*(#.*)?$").unwrap();
    if !head.is_match(&lines[item.start]) {
        return Vec::new();
    }
    let Some(first) = (item.start + 1..item.end).find(|&i| !is_trivia(&lines[i])) else {
        return Vec::new();
    };
    if !is_nested_item(&lines[first]) {
        return Vec::new();
    }
    let indent = indent_of(&lines[first]);
    let mut starts = Vec::new();
    let mut stop = item.end;
    for i in first..item.end {
        let line = &lines[i];
        if is_trivia(line) {
            continue;
        }
        let at = indent_of(line);
        if at < indent {
            stop = i;
            break;
        }
        if at == indent {
            starts.push(i);
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(n, &start)| Range { start, end: trimmed_end(lines, start, starts.get(n + 1).copied().unwrap_or(stop)) })
        .collect()
}
/// Whether a row is `server`'s, by its own `name:`, not one nested in a group's `config`.
fn is_server_row(lines: &[String], row: Range, server: &Regex, client: &Regex) -> bool {
    let body: Vec<&String> = lines[row.start..row.end].iter().filter(|l| !is_trivia(l)).collect();
    if !body.iter().any(|l| server.is_match(l)) {
        return false;
    }
    let key_indent = indent_of(&body[0].replacen('-', " ", 1));
    body.iter().enumerate().any(|(i, l)| (i == 0 || indent_of(l) == key_indent) && client.is_match(l))
}
fn has_server_lines(lines: &[String], server: &Regex) -> bool {
    lines.iter().any(|l| !is_trivia(l) && server.is_match(l))
}

/// Whether `text` names `server` anywhere outside comments.
pub fn has_server(text: &str, server: &str) -> bool {
    has_server_lines(&Text::split(text).lines, &names_server(server))
}

enum Layout {
    List,
    Other,
    /// No patches yet; the line of a `[]`, if there is one.
    Empty(Option<usize>),
}
fn layout_of(lines: &[String]) -> Layout {
    let content: Vec<usize> = (0..lines.len()).filter(|&i| !is_trivia(&lines[i])).collect();
    let Some(&first) = content.first() else {
        return Layout::Empty(None);
    };
    if content.len() == 1 && Regex::new(r"^\[\s*\]\s*(#.*)?$").unwrap().is_match(&lines[first]) {
        return Layout::Empty(Some(first));
    }
    let all_items = content.iter().all(|&i| lines[i].starts_with([' ', '\t']) || is_top_level_item(&lines[i]));
    if is_top_level_item(&lines[first]) && all_items {
        Layout::List
    } else {
        Layout::Other
    }
}

/// The patch that adds `server`, launched as `command args…`.
pub fn render_entry(server: &str, command: &str, args: &[String]) -> Vec<String> {
    let mut out = vec![
        "- insert:".to_owned(),
        format!("    - id: mcp-{server}"),
        format!("      name: '{CLIENT}'"),
        "      config:".to_owned(),
        format!("        serverName: {server}"),
        "        transport: stdio".to_owned(),
        // A JSON string is a valid YAML double-quoted scalar: Windows paths and spaces survive.
        format!("        command: {}", serde_json::to_string(command).unwrap()),
        "        args:".to_owned(),
    ];
    out.extend(args.iter().map(|a| format!("          - {a}")));
    out
}

/// `text` with `server`'s patch added; unchanged when it is already there.
pub fn install(text: &str, path: &std::path::Path, server: &str, command: &str, args: &[String]) -> Result<String> {
    let mut t = Text::split(text);
    if has_server_lines(&t.lines, &names_server(server)) {
        return Ok(text.to_owned());
    }
    let entry = render_entry(server, command, args);
    match layout_of(&t.lines) {
        Layout::Other => {
            return Err(format!(
                "{} is not a block-style YAML list, so CodeGraphV can't add its server. Nothing was changed: add the entry from `codegraphV install --print-config dsh` yourself, then run this again.",
                path.display()
            )
            .into())
        }
        Layout::Empty(Some(at)) => {
            t.lines.splice(at..=at, entry);
        }
        layout => {
            while t.lines.last().is_some_and(|l| l.trim().is_empty()) {
                t.lines.pop();
            }
            if matches!(layout, Layout::List) {
                t.lines.push(String::new());
            }
            t.lines.extend(entry);
        }
    }
    Ok(t.join())
}

/// `text` without `server`'s rows (a `- insert:` holding nothing else goes with them, and the
/// blank line above it), and a note when an entry is left in a form this can't edit. `None` for
/// the text means nothing is left at all and the file may be deleted (when `may_delete`).
pub fn remove(text: &str, path: &std::path::Path, server: &str, may_delete: bool) -> (Option<String>, Option<String>) {
    let mut t = Text::split(text);
    let names = names_server(server);
    if !has_server_lines(&t.lines, &names) {
        return (Some(text.to_owned()), None);
    }
    let client = names_client();
    let mut removed = false;
    for item in top_level_items(&t.lines).into_iter().rev() {
        let rows = insert_rows(&t.lines, item);
        let ours: Vec<Range> = rows.iter().copied().filter(|r| is_server_row(&t.lines, *r, &names, &client)).collect();
        if ours.is_empty() {
            continue;
        }
        if ours.len() < rows.len() {
            for row in ours.iter().rev() {
                t.lines.drain(row.start..row.end);
            }
        } else if ours.last().unwrap().end == item.end {
            let start = if item.start > 0 && t.lines[item.start - 1].trim().is_empty() { item.start - 1 } else { item.start };
            t.lines.drain(start..item.end);
        } else {
            continue;
        }
        removed = true;
    }
    let note = has_server_lines(&t.lines, &names).then(|| {
        format!("{} sets CodeGraphV's server in a form CodeGraphV can't edit safely. Remove it from there yourself.", path.display())
    });
    if !removed {
        return (Some(text.to_owned()), note);
    }
    if matches!(layout_of(&t.lines), Layout::Empty(_)) {
        if may_delete && t.lines.iter().all(|l| l.trim().is_empty()) {
            return (None, note);
        }
        t.lines.push("[]".into());
    }
    (Some(t.join()), note)
}
