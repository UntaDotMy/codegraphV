//! Svelte and Astro shell extraction, including the reference engine's exact
//! template scan order, script/module ownership and UTF-16 column semantics.
use super::{fold_script_result, sfc_file_node, Lines, ScriptFold};
use codegraph_jsre::re;
use codegraph_types::{
    jscompat::{js_trim, utf16_len},
    node_id, Edge, EdgeKind, ExtractionError, ExtractionResult, Language, Node, NodeKind, Severity,
    UnresolvedReference,
};

pub(super) struct ScriptBlock<'a> {
    pub content: &'a str,
    pub offset: u32,
    pub per_instance: bool,
    pub script_language: Language,
}

fn rune(name: &str) -> bool {
    matches!(
        name,
        "$props"
            | "$state"
            | "$derived"
            | "$effect"
            | "$bindable"
            | "$inspect"
            | "$host"
            | "$snippet"
    )
}

pub fn extract_component(file: &str, source: &str, language: Language) -> ExtractionResult {
    if language == Language::Vue {
        return super::vue::extract(file, source);
    }
    let started = std::time::Instant::now();
    let lines = Lines::new(source);
    let (id, mut out) = component_result(file, source, language, &lines);
    let mut covered = covered_lines(&lines);
    let mut blocks = Vec::new();
    if language == Language::Astro {
        if let Some((block, open, close)) = frontmatter(&lines) {
            covered[open..=close].fill(true);
            blocks.push(block);
        }
    }
    blocks.extend(script_blocks(source, &lines, language));
    for block in blocks {
        match crate::extract_file(file, block.content, block.script_language) {
            Ok(result) => fold_script_result(
                result,
                &ScriptFold {
                    file_path: file,
                    component_node_id: &id,
                    line_offset: block.offset,
                    language,
                    per_instance: block.per_instance,
                },
                &mut out,
            ),
            Err(error) => {
                out.errors.push(ExtractionError {
                    message: format!(
                        "{} extraction error: {error}",
                        if language == Language::Svelte {
                            "Svelte"
                        } else {
                            "Astro"
                        }
                    ),
                    severity: Severity::Error,
                    code: Some("parse_error".to_owned()),
                    file_path: None,
                    line: None,
                    column: None,
                });
                out.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
                return out;
            }
        }
    }
    template_calls(file, &id, language, &lines, &covered, &mut out);
    template_components(file, &id, language, &lines, &covered, &mut out);
    if language == Language::Svelte {
        out.unresolved_references
            .retain(|r| !rune(&r.reference_name));
    }
    out.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
    out
}

pub(super) fn component_result(
    file: &str,
    source: &str,
    language: Language,
    lines: &Lines<'_>,
) -> (String, ExtractionResult) {
    let name = file
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(file);
    let name = name
        .strip_suffix(&format!(".{}", language.as_str()))
        .unwrap_or(name);
    let id = node_id(file, NodeKind::Component, name, 1);
    let mut component = Node::new(
        &id,
        NodeKind::Component,
        name,
        format!("{file}::{name}"),
        file,
        language,
        1,
        lines.starts.len() as u32,
        0,
        utf16_len(lines.line(lines.starts.len() - 1)) as u32,
        super::now_ms(),
    );
    component.is_exported = Some(true);
    let out = ExtractionResult {
        nodes: vec![sfc_file_node(file, source, language), component],
        edges: vec![Edge::new(format!("file:{file}"), &id, EdgeKind::Contains)],
        ..Default::default()
    };
    (id, out)
}

pub(super) fn script_blocks<'a>(
    source: &'a str,
    lines: &Lines<'_>,
    language: Language,
) -> Vec<ScriptBlock<'a>> {
    re!(r"<script(\s[^>]*)?>(?<content>[\s\S]*?)</script>", "g")
        .captures_iter(source)
        .map(|c| {
            let attrs = c.text(1).unwrap_or("");
            let content = c.get(2).unwrap();
            let module = language == Language::Svelte
                && re!(r#"context\s*=\s*["']module["']|(?:^|\s)module(?=[\s=]|$)"#).is_match(attrs);
            let typescript = language == Language::Astro
                || re!(r#"lang\s*=\s*["'](ts|typescript)["']"#).is_match(attrs);
            // A Vue block can also be `tsx` (#2570): TypeScript with JSX, which only the TSX
            // grammar reads (sfcScriptLanguage).
            let tsx = !typescript && language == Language::Vue && re!(r#"lang\s*=\s*["']tsx["']"#).is_match(attrs);
            let per_instance = if language == Language::Vue {
                re!(r"\bsetup\b").is_match(attrs)
            } else {
                !module
            };
            ScriptBlock {
                content: content.as_str(),
                offset: lines.row(content.start()) as u32,
                per_instance,
                script_language: if typescript {
                    Language::TypeScript
                } else if tsx {
                    Language::Tsx
                } else {
                    Language::JavaScript
                },
            }
        })
        .collect()
}

fn frontmatter<'a>(lines: &Lines<'a>) -> Option<(ScriptBlock<'a>, usize, usize)> {
    let open = (0..lines.starts.len()).find(|&i| !js_trim(lines.line(i)).is_empty())?;
    if js_trim(lines.line(open)) != "---" {
        return None;
    }
    let close = (open + 1..lines.starts.len()).find(|&i| js_trim(lines.line(i)) == "---")?;
    let start = lines.starts[open + 1];
    let end = if close == open + 1 {
        start
    } else {
        lines.starts[close] - 1
    };
    Some((
        ScriptBlock {
            content: &lines.source[start..end],
            offset: (open + 1) as u32,
            per_instance: true,
            script_language: Language::TypeScript,
        },
        open,
        close,
    ))
}

pub(super) fn covered_lines(lines: &Lines<'_>) -> Vec<bool> {
    let mut covered = vec![false; lines.starts.len()];
    for c in re!(r"<(script|style)(\s[^>]*)?>[\s\S]*?</\1>", "g").captures_iter(lines.source) {
        let m = c.get(0).unwrap();
        covered[lines.row(m.start())..=lines.row(m.end())].fill(true);
    }
    covered
}

#[allow(clippy::too_many_arguments)]
fn add_ref(
    out: &mut ExtractionResult,
    file: &str,
    component: &str,
    language: Language,
    name: &str,
    kind: EdgeKind,
    row: usize,
    column: usize,
) {
    out.unresolved_references.push(UnresolvedReference {
        from_node_id: component.to_owned(),
        reference_name: name.to_owned(),
        reference_kind: kind.into(),
        line: row as u32 + 1,
        column: column as u32,
        file_path: Some(file.to_owned()),
        language: Some(language),
        candidates: None,
        row_id: None,
    });
}

fn expression_calls(
    out: &mut ExtractionResult,
    file: &str,
    id: &str,
    language: Language,
    expr: &str,
    row: usize,
    column: usize,
) {
    for c in re!(r"\b([a-zA-Z_$][\w$.]*)\s*\(", "g").captures_iter(expr) {
        let name = c.text(1).unwrap();
        if language == Language::Svelte
            && (rune(name) || matches!(name, "if" | "else" | "each" | "await"))
        {
            continue;
        }
        if language == Language::Astro && matches!(name, "if" | "await" | "function") {
            continue;
        }
        add_ref(
            out,
            file,
            id,
            language,
            name,
            EdgeKind::Calls,
            row,
            column + c.get(0).unwrap().utf16_start(),
        );
    }
}

fn template_calls(
    file: &str,
    id: &str,
    language: Language,
    lines: &Lines<'_>,
    covered: &[bool],
    out: &mut ExtractionResult,
) {
    for (row, is_covered) in covered.iter().enumerate() {
        if *is_covered {
            continue;
        }
        let line = lines.line(row);
        if language == Language::Svelte {
            for c in re!(r"\{([^}#/:@][^}]*)\}", "g").captures_iter(line) {
                expression_calls(
                    out,
                    file,
                    id,
                    language,
                    c.text(1).unwrap(),
                    row,
                    c.get(0).unwrap().utf16_start(),
                );
            }
        } else {
            for c in re!(r"\{([^}/][^}]*)\}", "g").captures_iter(line) {
                expression_calls(
                    out,
                    file,
                    id,
                    language,
                    c.text(1).unwrap(),
                    row,
                    c.get(0).unwrap().utf16_start(),
                );
            }
            let remainder = re!(r"\{([^}/][^}]*)\}", "g").replace_all_str(line, "");
            if let Some(c) = re!(r"\{([^}/][^}]*)$").captures(&remainder) {
                let column = line.rfind('{').map(|i| utf16_len(&line[..i])).unwrap_or(0);
                expression_calls(out, file, id, language, c.text(1).unwrap(), row, column);
            };
        }
    }
}

fn template_components(
    file: &str,
    id: &str,
    language: Language,
    lines: &Lines<'_>,
    covered: &[bool],
    out: &mut ExtractionResult,
) {
    for (row, is_covered) in covered.iter().enumerate() {
        if *is_covered {
            continue;
        }
        for c in re!(r"<([A-Z][a-zA-Z0-9_$]*)\b", "g").captures_iter(lines.line(row)) {
            let name = c.text(1).unwrap();
            if language == Language::Astro && matches!(name, "Fragment" | "Code" | "Debug") {
                continue;
            }
            add_ref(
                out,
                file,
                id,
                language,
                name,
                EdgeKind::References,
                row,
                c.get(0).unwrap().utf16_start() + 1,
            );
        }
    }
}
