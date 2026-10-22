//! Shopify/Jekyll Liquid tags, schema names and OS 2.0 JSON section references.
use super::Document;
use crate::source::now_ms;
use codegraph_jsre::{re, Re};
use codegraph_types::{
    jscompat::{is_js_space, utf16_len},
    ExtractionResult, Language, Node, NodeIdAllocator, NodeKind,
};
use serde_json::Value;
use std::collections::HashSet;

struct Occurrence<'a> {
    full: &'a str,
    tag: &'a str,
    name: &'a str,
    at: usize,
}

fn patterns(kind: u8) -> (&'static Re, &'static Re) {
    match kind {
        0 => (
            re!(r#"^\{%[-]?\s*(render|include)\s+['"]([^'"]+)['"]"#),
            re!(r#"^[ \t]*(render|include)[ \t]+['"]([^'"]+)['"]"#),
        ),
        1 => (
            re!(r#"^\{%[-]?\s*(section)\s+['"]([^'"]+)['"]"#),
            re!(r#"^[ \t]*(section)[ \t]+['"]([^'"]+)['"]"#),
        ),
        _ => (
            re!(r"^\{%[-]?\s*(assign)\s+(\w+)\s*="),
            re!(r"^[ \t]*(assign)[ \t]+(\w+)\s*="),
        ),
    }
}

fn occurrences(source: &str, kind: u8) -> Vec<Occurrence<'_>> {
    let (braced, bare) = patterns(kind);
    let mut found = Vec::new();
    let mut blocks = Vec::new();
    let mut suppressed = None;
    let closes = |name: &str, active: Option<&str>| {
        matches!(
            (name, active),
            ("endcomment", Some("comment")) | ("endraw", Some("raw"))
        )
    };
    for tag in re!(r"\{%[-]?\s*(\w+|#)([\s\S]*?)[-]?%\}", "g").captures_iter(source) {
        let name = tag.text(1).unwrap();
        if suppressed.is_some() {
            if closes(name, suppressed) {
                suppressed = None;
            }
            continue;
        }
        if matches!(name, "comment" | "raw") {
            suppressed = Some(name);
            continue;
        }
        if name == "liquid" {
            blocks.push((tag.get(1).unwrap().end(), tag.text(2).unwrap()));
            continue;
        }
        if let Some(c) = braced.captures(tag.text(0).unwrap()) {
            found.push(Occurrence {
                full: c.text(0).unwrap(),
                tag: c.text(1).unwrap(),
                name: c.text(2).unwrap(),
                at: tag.get(0).unwrap().start(),
            });
        }
    }
    for (mut offset, block) in blocks {
        let mut suppressed = None;
        for line in block.split('\n') {
            let name = re!(r"^[ \t]*(\w+)").captures(line).and_then(|c| c.text(1));
            if suppressed.is_some() {
                if name.is_some_and(|n| closes(n, suppressed)) {
                    suppressed = None;
                }
            } else if matches!(name, Some("comment" | "raw")) {
                suppressed = name;
            } else if let Some(c) = bare.captures(line) {
                let full = c.text(0).unwrap();
                let trimmed = full.trim_start_matches(is_js_space);
                found.push(Occurrence {
                    full: trimmed,
                    tag: c.text(1).unwrap(),
                    name: c.text(2).unwrap(),
                    at: offset + full.len() - trimmed.len(),
                });
            }
            offset += line.len() + 1;
        }
    }
    found.sort_by_key(|m| m.at);
    found
}

pub fn extract(file: &str, source: &str) -> ExtractionResult {
    let mut doc = Document::new(file, source, Language::Liquid);
    let file_id = doc.file_node();
    if file.ends_with(".json") {
        json_sections(&mut doc, &file_id, source);
        return doc.finish();
    }
    let mut ids = NodeIdAllocator::default();
    for (kind, folder) in [(0, "snippets"), (1, "sections")] {
        for m in occurrences(source, kind) {
            let (line, column) = doc.lines.position(m.at);
            let import_id = ids.generate(file, NodeKind::Import, m.name, line, column);
            let mut import = Node::new(
                &import_id,
                NodeKind::Import,
                m.name,
                format!("{file}::import:{}", m.name),
                file,
                Language::Liquid,
                line,
                line,
                column,
                column + utf16_len(m.full) as u32,
                now_ms(),
            );
            import.signature = Some(m.full.to_owned());
            doc.contains(&file_id, import);
            let id = ids.generate(
                file,
                NodeKind::Component,
                &format!("{}:{}", m.tag, m.name),
                line,
                column,
            );
            let node = Node::new(
                id,
                NodeKind::Component,
                m.name,
                format!("{file}::{}:{}", m.tag, m.name),
                file,
                Language::Liquid,
                line,
                line,
                column,
                column + utf16_len(m.full) as u32,
                now_ms(),
            );
            doc.contains(&file_id, node);
            doc.reference(
                &file_id,
                format!("{folder}/{}.liquid", m.name),
                line,
                column,
                false,
            );
        }
    }
    for c in re!(
        r"\{%[-]?\s*schema\s*[-]?%\}([\s\S]*?)\{%[-]?\s*endschema\s*[-]?%\}",
        "g"
    )
    .captures_iter(source)
    {
        let full = c.get(0).unwrap();
        let (line, column) = doc.lines.position(full.start());
        let name = schema_name(c.text(1).unwrap());
        let id = ids.generate(
            file,
            NodeKind::Constant,
            &format!("schema:{name}"),
            line,
            column,
        );
        let node = Node::new(
            id,
            NodeKind::Constant,
            &name,
            format!("{file}::schema:{name}"),
            file,
            Language::Liquid,
            line,
            doc.lines.row(full.end()) as u32 + 1,
            column,
            0,
            now_ms(),
        );
        // Raw schema defaults must never become a docstring (#383).
        doc.contains(&file_id, node);
    }
    for m in occurrences(source, 2) {
        let (line, column) = doc.lines.position(m.at);
        let id = ids.generate(file, NodeKind::Variable, m.name, line, column);
        let node = Node::new(
            id,
            NodeKind::Variable,
            m.name,
            format!("{file}::{}", m.name),
            file,
            Language::Liquid,
            line,
            line,
            column,
            column + utf16_len(m.full) as u32,
            now_ms(),
        );
        doc.contains(&file_id, node);
    }
    doc.finish()
}

fn json_sections(doc: &mut Document<'_>, file: &str, source: &str) {
    let Ok(parsed) = serde_json::from_str::<Value>(source) else {
        return;
    };
    let Some(sections) = parsed.get("sections") else {
        return;
    };
    let entries: Vec<_> = match sections {
        Value::Object(map) => map.values().collect(),
        Value::Array(array) => array.iter().collect(),
        _ => return,
    };
    let mut seen = HashSet::new();
    for entry in entries {
        if let Some(name) = entry
            .get("type")
            .and_then(Value::as_str)
            .filter(|name| seen.insert(*name))
        {
            doc.reference(file, format!("sections/{name}.liquid"), 1, 0, false);
        }
    }
}

fn schema_name(source: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<Value>(source) else {
        return "schema".to_owned();
    };
    let Some(name) = parsed.get("name") else {
        return "schema".to_owned();
    };
    if let Some(name) = name.as_str().filter(|s| !s.is_empty()) {
        return name.to_owned();
    }
    let first = match name {
        Value::Object(map) => map.values().next(),
        Value::Array(values) => values.first(),
        _ => None,
    };
    name.get("en")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| first.and_then(Value::as_str).filter(|s| !s.is_empty()))
        .unwrap_or("schema")
        .to_owned()
}
