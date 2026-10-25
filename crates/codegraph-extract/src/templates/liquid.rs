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
    /// The first argument group that took part in the match: the snippet, section or variable.
    name: &'a str,
    at: usize,
}

#[derive(Clone, Copy)]
enum Tags {
    Snippet,
    Section,
    Assign,
    Capture,
}

/// The braced (`{% tag … %}`) and bare (a line of a `{% liquid %}` body) spelling of each tag.
fn patterns(kind: Tags) -> (&'static Re, &'static Re) {
    match kind {
        Tags::Snippet => (
            re!(r#"^\{%[-]?\s*(render|include)\s+['"]([^'"]+)['"]"#),
            re!(r#"^[ \t]*(render|include)[ \t]+['"]([^'"]+)['"]"#),
        ),
        Tags::Section => (
            re!(r#"^\{%[-]?\s*(sections?)\s+['"]([^'"]+)['"]"#),
            re!(r#"^[ \t]*(sections?)[ \t]+['"]([^'"]+)['"]"#),
        ),
        Tags::Assign => (
            re!(r"^\{%[-]?\s*(assign)\s+(\w+)\s*="),
            re!(r"^[ \t]*(assign)[ \t]+(\w+)\s*="),
        ),
        // Liquid reads a quoted name (`{% capture 'title' %}`) as the name inside the quotes. A
        // name that goes on with `-` or `.`, which the assign rule doesn't read either, gets no
        // node rather than a cut-short one.
        Tags::Capture => (
            re!(r#"^\{%[-]?\s*(capture)\s+(?:'(\w+)'|"(\w+)"|(\w+))(?=\s|-?%\}|$)"#),
            re!(r#"^[ \t]*(capture)[ \t]+(?:'(\w+)'|"(\w+)"|(\w+))(?=\s|-?%\}|$)"#),
        ),
    }
}

fn tag_re() -> &'static Re {
    re!(r"\{%[-]?\s*(\w+|#)([\s\S]*?)[-]?%\}")
}

/// The regions Liquid runs nothing in: `{% comment %}…{% endcomment %}` and `{% raw %}…{% endraw
/// %}`, each from its opening tag to the end of its closing one (or to the end of the file, when it
/// is never closed). The tags, schemas and declarations written inside one are examples, not code.
fn silent_regions(source: &str) -> Vec<(usize, usize)> {
    let mut regions = Vec::new();
    let mut open: Option<(String, usize)> = None;
    for tag in tag_re().captures_iter(source) {
        let name = tag.text(1).unwrap_or("");
        let whole = tag.get(0).unwrap();
        match &open {
            Some((opened, start)) => {
                if name.strip_prefix("end") == Some(opened.as_str()) {
                    regions.push((*start, whole.end()));
                    open = None;
                }
            }
            None if matches!(name, "comment" | "raw") => open = Some((name.to_owned(), whole.start())),
            None => {}
        }
    }
    if let Some((_, start)) = open {
        regions.push((start, source.len()));
    }
    regions
}

fn silent_region_at(regions: &[(usize, usize)], index: usize) -> Option<(usize, usize)> {
    regions.iter().copied().find(|&(start, end)| index >= start && index < end)
}

fn first_name<'t>(c: &codegraph_jsre::Caps<'t>) -> &'t str {
    (2..c.len()).find_map(|i| c.text(i)).unwrap_or("")
}

/// Every occurrence of a Liquid tag, in both spellings it can be written in, outside the silent
/// regions; sorted by offset.
fn occurrences<'a>(source: &'a str, kind: Tags, silent: &[(usize, usize)]) -> Vec<Occurrence<'a>> {
    let (braced, bare) = patterns(kind);
    let mut found = Vec::new();
    let mut blocks = Vec::new();
    // Consume complete braced tags so strings and comments cannot start a second match inside the
    // same tag; skip silent regions entirely.
    let mut pos = 0;
    while let Some(tag) = tag_re().captures_at(source, pos) {
        let whole = tag.get(0).unwrap();
        if let Some((_, end)) = silent_region_at(silent, whole.start()) {
            pos = end;
            continue;
        }
        pos = whole.end();
        let name = tag.text(1).unwrap();
        if name == "liquid" {
            blocks.push((tag.get(1).unwrap().end(), tag.text(2).unwrap()));
            continue;
        }
        if let Some(c) = braced.captures(whole.as_str()) {
            found.push(Occurrence { full: c.text(0).unwrap(), tag: c.text(1).unwrap(), name: first_name(&c), at: whole.start() });
        }
    }
    // Inside a `{% liquid %}` body each tag starts its own line. Anchoring on the line start keeps
    // prose, filters and inline `#` comments out.
    for (mut offset, block) in blocks {
        let mut suppressed: Option<&str> = None;
        for line in block.split('\n') {
            let name = re!(r"^[ \t]*(\w+)").captures(line).and_then(|c| c.text(1));
            if let Some(open) = suppressed {
                if name.and_then(|n| n.strip_prefix("end")) == Some(open) {
                    suppressed = None;
                }
            } else if matches!(name, Some("comment" | "raw")) {
                suppressed = name;
            } else if let Some(c) = bare.captures(line) {
                let full = c.text(0).unwrap();
                let trimmed = full.trim_start_matches(is_js_space);
                found.push(Occurrence { full: trimmed, tag: c.text(1).unwrap(), name: first_name(&c), at: offset + full.len() - trimmed.len() });
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
    let silent = silent_regions(source);
    let mut ids = NodeIdAllocator::default();
    for kind in [Tags::Snippet, Tags::Section] {
        for m in occurrences(source, kind, &silent) {
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
            let id = ids.generate(file, NodeKind::Component, &format!("{}:{}", m.tag, m.name), line, column);
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
            // A section is `sections/<name>.liquid`; a section group, which a layout renders with
            // the plural tag, is the JSON file `sections/<group>.json` that lists its sections.
            let target = match (kind, m.tag) {
                (Tags::Snippet, _) => format!("snippets/{}.liquid", m.name),
                (_, "sections") => format!("sections/{}.json", m.name),
                _ => format!("sections/{}.liquid", m.name),
            };
            doc.reference(&file_id, target, line, column, false);
        }
    }
    let schema = re!(r"\{%[-]?\s*schema\s*[-]?%\}([\s\S]*?)\{%[-]?\s*endschema\s*[-]?%\}");
    let mut pos = 0;
    while let Some(c) = schema.captures_at(source, pos) {
        let full = c.get(0).unwrap();
        // A schema inside {% comment %} or {% raw %} is an example, not the section's: look again
        // after that region ends.
        if let Some((_, end)) = silent_region_at(&silent, full.start()) {
            pos = end;
            continue;
        }
        pos = full.end();
        let (line, column) = doc.lines.position(full.start());
        let name = schema_name(c.text(1).unwrap());
        let id = ids.generate(file, NodeKind::Constant, &format!("schema:{name}"), line, column);
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
    // Captures come after assigns, so an assign keeps its id when a capture of the same name
    // shares its line.
    for kind in [Tags::Assign, Tags::Capture] {
        for m in occurrences(source, kind, &silent) {
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
    }
    doc.finish()
}

/// Shopify OS 2.0 JSON template / section group: each section's `type` names a
/// `sections/<type>.liquid` file, linked from where the `type` value is written. The theme editor
/// writes a block comment at the top of every file it saves, and Shopify's theme JSON allows
/// comments and trailing commas; anything else that is not JSON links nothing.
fn json_sections(doc: &mut Document<'_>, file: &str, source: &str) {
    let Some(Jsonc::Object(root)) = jsonc::parse(source) else { return };
    let Some(Jsonc::Object(sections)) = jsonc::first(&root, "sections") else { return };
    let mut seen = HashSet::new();
    for (_, section) in sections {
        let Jsonc::Object(section) = section else { continue };
        let Some(Jsonc::String(name, at)) = jsonc::first(section, "type") else { continue };
        if !seen.insert(name.clone()) {
            continue;
        }
        let (line, column) = doc.lines.position(*at);
        doc.reference(file, format!("sections/{name}.liquid"), line, column, false);
    }
}

use jsonc::Jsonc;

/// The parse `jsonc-parser`'s `parseTree(text, errors, { allowTrailingComma: true })` accepts:
/// JSON plus `//` and `/* */` comments and trailing commas. Any error is `None`.
mod jsonc {
    pub enum Jsonc {
        Object(Vec<(String, Jsonc)>),
        /// The value and the byte offset of its opening quote.
        String(String, usize),
        Other,
    }

    /// The value of the first property named `key` (as `findNodeAtLocation` takes it).
    pub fn first<'a>(object: &'a [(String, Jsonc)], key: &str) -> Option<&'a Jsonc> {
        object.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn parse(text: &str) -> Option<Jsonc> {
        let mut p = Parser { s: text.as_bytes(), text, pos: 0 };
        p.skip()?;
        let value = p.value(0)?;
        p.skip()?;
        (p.pos == p.s.len()).then_some(value)
    }

    const MAX_DEPTH: usize = 512;

    struct Parser<'a> {
        s: &'a [u8],
        text: &'a str,
        pos: usize,
    }

    impl Parser<'_> {
        fn peek(&self) -> Option<u8> {
            self.s.get(self.pos).copied()
        }

        /// Skip whitespace (space, tab, CR, LF) and comments; an unclosed block comment fails.
        fn skip(&mut self) -> Option<()> {
            loop {
                match self.peek() {
                    Some(b' ' | b'\t' | b'\n' | b'\r') => self.pos += 1,
                    Some(b'/') if self.s.get(self.pos + 1) == Some(&b'/') => {
                        while !matches!(self.peek(), None | Some(b'\n' | b'\r')) {
                            self.pos += 1;
                        }
                    }
                    Some(b'/') if self.s.get(self.pos + 1) == Some(&b'*') => {
                        let close = self.text[self.pos + 2..].find("*/")?;
                        self.pos += 2 + close + 2;
                    }
                    _ => return Some(()),
                }
            }
        }

        fn value(&mut self, depth: usize) -> Option<Jsonc> {
            if depth > MAX_DEPTH {
                return None;
            }
            match self.peek()? {
                b'{' => self.object(depth),
                b'[' => self.array(depth),
                b'"' => {
                    let at = self.pos;
                    Some(Jsonc::String(self.string()?, at))
                }
                b'-' | b'0'..=b'9' => self.number().map(|_| Jsonc::Other),
                _ => {
                    // A literal is a run of characters that are not structure, space or `/`.
                    let start = self.pos;
                    while self.peek().is_some_and(|c| !matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'{' | b'}' | b'[' | b']' | b'"' | b':' | b',' | b'/')) {
                        self.pos += 1;
                    }
                    matches!(&self.text[start..self.pos], "true" | "false" | "null").then_some(Jsonc::Other)
                }
            }
        }

        /// `{` (property (`,` property)* `,`?)? `}`.
        fn object(&mut self, depth: usize) -> Option<Jsonc> {
            self.pos += 1;
            let mut props = Vec::new();
            loop {
                self.skip()?;
                if self.peek()? == b'}' {
                    self.pos += 1;
                    return Some(Jsonc::Object(props));
                }
                if self.peek()? != b'"' {
                    return None;
                }
                let key = self.string()?;
                self.skip()?;
                if self.peek()? != b':' {
                    return None;
                }
                self.pos += 1;
                self.skip()?;
                let value = self.value(depth + 1)?;
                props.push((key, value));
                self.skip()?;
                match self.peek()? {
                    b',' => self.pos += 1,
                    b'}' => {}
                    _ => return None,
                }
            }
        }

        fn array(&mut self, depth: usize) -> Option<Jsonc> {
            self.pos += 1;
            loop {
                self.skip()?;
                if self.peek()? == b']' {
                    self.pos += 1;
                    return Some(Jsonc::Other);
                }
                self.value(depth + 1)?;
                self.skip()?;
                match self.peek()? {
                    b',' => self.pos += 1,
                    b']' => {}
                    _ => return None,
                }
            }
        }

        /// A JSON string; control characters, line breaks and unknown escapes fail.
        fn string(&mut self) -> Option<String> {
            self.pos += 1;
            let mut out = String::new();
            let mut units: Vec<u16> = Vec::new();
            loop {
                let rest = &self.text[self.pos..];
                let c = rest.chars().next()?;
                if c != '\\' && !units.is_empty() {
                    out.push_str(&String::from_utf16_lossy(&units));
                    units.clear();
                }
                match c {
                    '"' => {
                        self.pos += 1;
                        return Some(out);
                    }
                    '\u{0}'..='\u{1f}' => return None,
                    '\\' => {
                        let e = *self.s.get(self.pos + 1)?;
                        self.pos += 2;
                        let simple = match e {
                            b'"' => Some('"'),
                            b'\\' => Some('\\'),
                            b'/' => Some('/'),
                            b'b' => Some('\u{8}'),
                            b'f' => Some('\u{c}'),
                            b'n' => Some('\n'),
                            b'r' => Some('\r'),
                            b't' => Some('\t'),
                            b'u' => None,
                            _ => return None,
                        };
                        match simple {
                            Some(ch) => {
                                if !units.is_empty() {
                                    out.push_str(&String::from_utf16_lossy(&units));
                                    units.clear();
                                }
                                out.push(ch);
                            }
                            None => {
                                let hex = self.text.get(self.pos..self.pos + 4)?;
                                if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                                    return None;
                                }
                                units.push(u16::from_str_radix(hex, 16).ok()?);
                                self.pos += 4;
                            }
                        }
                    }
                    _ => {
                        out.push(c);
                        self.pos += c.len_utf8();
                    }
                }
            }
        }

        /// `-`? (`0` | [1-9][0-9]*) (`.` [0-9]+)? ([eE] [+-]? [0-9]+)?
        fn number(&mut self) -> Option<()> {
            if self.peek() == Some(b'-') {
                self.pos += 1;
            }
            match self.peek()? {
                b'0' => self.pos += 1,
                b'1'..=b'9' => {
                    while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                        self.pos += 1;
                    }
                }
                _ => return None,
            }
            if self.peek() == Some(b'.') {
                self.pos += 1;
                if !self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    return None;
                }
                while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.pos += 1;
                }
            }
            if matches!(self.peek(), Some(b'e' | b'E')) {
                self.pos += 1;
                if matches!(self.peek(), Some(b'+' | b'-')) {
                    self.pos += 1;
                }
                if !self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    return None;
                }
                while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.pos += 1;
                }
            }
            Some(())
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
