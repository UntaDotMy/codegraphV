//! Razor/Blazor markup dependencies and delegated embedded C# references.
use super::Document;
use crate::source::now_ms;
use codegraph_jsre::re;
use codegraph_types::{
    jscompat::{js_trim, utf16_len},
    node_id, ExtractionResult, Language, Node, NodeKind,
};

fn builtin(name: &str) -> bool {
    matches!(
        name,
        "Router"
            | "Found"
            | "NotFound"
            | "RouteView"
            | "AuthorizeRouteView"
            | "LayoutView"
            | "CascadingValue"
            | "CascadingAuthenticationState"
            | "AuthorizeView"
            | "Authorized"
            | "NotAuthorized"
            | "Authorizing"
            | "EditForm"
            | "DataAnnotationsValidator"
            | "ValidationSummary"
            | "ValidationMessage"
            | "InputText"
            | "InputNumber"
            | "InputCheckbox"
            | "InputSelect"
            | "InputDate"
            | "InputTextArea"
            | "InputRadio"
            | "InputRadioGroup"
            | "InputFile"
            | "PageTitle"
            | "HeadContent"
            | "HeadOutlet"
            | "Virtualize"
            | "DynamicComponent"
            | "ErrorBoundary"
            | "SectionContent"
            | "SectionOutlet"
            | "FocusOnNavigate"
            | "NavLink"
            | "Microsoft"
    )
}

fn last_segment(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}
fn type_names(expr: &str) -> impl Iterator<Item = &str> {
    re!(r"[^<>,\s]+", "g").captures_iter(expr).filter_map(|c| {
        let name = last_segment(c.text(0).unwrap());
        re!(r"^[A-Z][A-Za-z0-9_]*$").is_match(name).then_some(name)
    })
}

pub fn extract(file: &str, source: &str) -> ExtractionResult {
    let mut doc = Document::new(file, source, Language::Razor);
    let filename = file
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(file);
    let name = re!(r"\.(razor|cshtml)$", "i").replace_first(filename, |_| String::new());
    let id = node_id(file, NodeKind::Component, &name, 1);
    let mut component = Node::new(
        &id,
        NodeKind::Component,
        &name,
        format!("{file}::{name}"),
        file,
        Language::Razor,
        1,
        doc.lines.starts.len() as u32,
        0,
        utf16_len(doc.lines.line(doc.lines.starts.len() - 1)) as u32,
        now_ms(),
    );
    component.is_exported = Some(true);
    doc.out.nodes.push(component);
    for (row, line) in source.split('\n').enumerate() {
        for pattern in [
            re!(r"^\s*@(?:model|inherits)\s+([A-Za-z_][\w.]*(?:\s*<[^>]+>)?)"),
            re!(r"^\s*@inject\s+([A-Za-z_][\w.]*(?:\s*<[^>]+>)?)\s+[A-Za-z_]"),
        ] {
            if let Some(c) = pattern.captures(line) {
                for name in type_names(c.text(1).unwrap()) {
                    doc.reference(&id, name, row as u32 + 1, 0, true);
                }
            }
        }
        for c in re!(r"@typeof\(\s*([A-Za-z_][\w.]*)\s*\)", "g").captures_iter(line) {
            let name = last_segment(c.text(1).unwrap());
            if re!(r"^[A-Z]").is_match(name) {
                doc.reference(
                    &id,
                    name,
                    row as u32 + 1,
                    c.get(0).unwrap().utf16_start() as u32,
                    true,
                );
            }
        }
    }
    if file.to_lowercase().ends_with(".razor") {
        for (row, line) in source.split('\n').enumerate() {
            for c in re!(r"<([A-Z][A-Za-z0-9_]*)\b([^>]*)>", "g").captures_iter(line) {
                let name = c.text(1).unwrap();
                if builtin(name) {
                    continue;
                }
                doc.reference(
                    &id,
                    name,
                    row as u32 + 1,
                    c.get(0).unwrap().utf16_start() as u32 + 1,
                    true,
                );
                for attr in re!(r#"\bT[A-Za-z]*\s*=\s*"([A-Za-z_][\w.]*)""#, "g")
                    .captures_iter(c.text(2).unwrap_or(""))
                {
                    let name = last_segment(attr.text(1).unwrap());
                    if re!(r"^[A-Z]").is_match(name) {
                        doc.reference(&id, name, row as u32 + 1, 0, true);
                    }
                }
            }
        }
    }
    let mut at = 0;
    while let Some(c) = re!(r"@(?:code|functions)\b\s*\{|@\{", "g").captures_at(source, at) {
        let open = c.get(0).unwrap().end() - 1;
        let Some(close) = matching_brace(source, open) else {
            at = c.get(0).unwrap().end();
            continue;
        };
        at = close;
        let content = &source[open + 1..close];
        if js_trim(content).is_empty() {
            continue;
        }
        let wrapper = format!("class __RazorCode__ {{\n{content}\n}}");
        // Like the reference, malformed/unavailable embedded code does not
        // discard dependencies already found in the markup.
        if let Ok(result) = crate::extract_file(file, &wrapper, Language::CSharp) {
            let offset = doc.lines.row(open + 1) as u32;
            for mut reference in result.unresolved_references {
                reference.from_node_id = id.clone();
                reference.line = reference.line + offset - 1;
                reference.file_path = Some(file.to_owned());
                reference.language = Some(Language::Razor);
                doc.out.unresolved_references.push(reference);
            }
        }
    }
    process_script_blocks(&mut doc, file, source, &id);
    doc.finish()
}

/// Hand each inline `<script>` to the JavaScript or TypeScript extractor and keep what it
/// references, as the view's (upstream 97492056): one component node per file, as for `@code`.
/// The references stay `javascript` / `typescript`, never `razor`: `razor` is in the `dotnet`
/// family, and the name matcher's family gate would keep them from a `.js` symbol. What the view's
/// blocks declare for themselves is nobody else's. A `<script src>` has no body, and a
/// `type="text/html"` template or a JSON island is no script.
fn process_script_blocks(doc: &mut Document, file: &str, source: &str, id: &str) {
    use super::razor_script::{blank_razor_comments, blank_razor_in_script, is_javascript_block};
    let source = blank_razor_comments(source);
    let mut blocks: Vec<(ExtractionResult, Language, u32)> = Vec::new();
    for m in re!(r"<script(\s[^>]*?)?(?:/>|>(?<content>[\s\S]*?)</script\s*>)", "g").captures_iter(&source) {
        let attrs = m.text(1).unwrap_or("");
        let Some(content) = m.get(2).filter(|c| !js_trim(c.as_str()).is_empty()) else { continue };
        if !is_javascript_block(attrs) {
            continue;
        }
        let language = if re!(r#"lang\s*=\s*["'](ts|typescript)["']"#).is_match(attrs) { Language::TypeScript } else { Language::JavaScript };
        let whole = m.get(0).unwrap();
        let tag_end = whole.start() + whole.as_str().find('>').map_or(0, |i| i + 1);
        let line_offset = source[..tag_end].matches('\n').count() as u32;
        // Grammar unavailable / parse failure — skip this block.
        if let Ok(result) = crate::extract_file(file, &blank_razor_in_script(content.as_str()), language) {
            blocks.push((result, language, line_offset));
        }
    }
    let own: std::collections::HashSet<String> = blocks
        .iter()
        .flat_map(|(result, _, _)| result.nodes.iter())
        .filter(|n| n.kind != NodeKind::File && n.kind != NodeKind::Import)
        .map(|n| n.name.clone())
        .collect();
    for (result, language, line_offset) in blocks {
        for mut reference in result.unresolved_references {
            if own.contains(reference.reference_name.split('.').next().unwrap_or("")) {
                continue;
            }
            reference.from_node_id = id.to_owned();
            reference.line += line_offset;
            reference.file_path = Some(file.to_owned());
            reference.language = Some(language);
            doc.out.unresolved_references.push(reference);
        }
    }
}

fn matching_brace(source: &str, open: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth = 0;
    let mut i = open;
    while i < bytes.len() {
        let c = bytes[i];
        if matches!(c, b'\'' | b'"') {
            i += 1;
            while i < bytes.len() && bytes[i] != c {
                if bytes[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
            i = source[i..].find('\n').map(|p| i + p).unwrap_or(bytes.len());
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i = source[i + 2..]
                .find("*/")
                .map(|p| i + p + 3)
                .unwrap_or(bytes.len());
        } else if c == b'{' {
            depth += 1;
        } else if c == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}
