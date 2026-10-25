//! CFML extraction (.cfc / .cfm / .cfs) — a port of `CfmlExtractor` (src/extraction/cfml-extractor.ts).
//!
//! tree-sitter-cfml has two grammars: `cfml` (tags — `<cfcomponent>`, `<cffunction>`, HTML) and
//! `cfscript` (`component { … }`). A file whose first real token is not `<` is cfscript, extracted
//! whole by the cfscript walker (tsjs `Variant::Cfscript`) with its anonymous component named after
//! the file. Any other file is walked tag by tag with the cfml grammar: `<cfscript>` bodies go to
//! the cfscript walker, `<cfquery>` bodies to the cfquery one, and the expressions written in tags
//! (`<cfset>`, `<cfif>`, `#hash#`, …) are copied into one cfscript source, extracted once, and each
//! reference handed back to the scope whose expression holds it.

use crate::textutil::{col16, line_starts, utf16_len};
use crate::ts::Node as SyntaxNode;
use codegraph_types::jscompat::js_trim;
use codegraph_types::{
    node_id, Edge, EdgeKind, ExtractionError, ExtractionResult, Language, Node, NodeIdAllocator, NodeKind, ReferenceKind,
    Severity, UnresolvedReference, Visibility,
};
use std::collections::HashSet;

/// Tags whose own children include an expression: `<cfset …>`, `<cfif …>`, `<cfelseif …>`,
/// `<cfreturn …>`, and `#…#`.
fn is_tag_expression_parent(kind: &str) -> bool {
    matches!(kind, "cf_set_tag" | "cf_if_tag" | "cf_elseif_tag" | "cf_return_tag" | "hash_expression")
}

/// The expression node types (shared with cfscript) that can contain a call.
fn is_tag_expression_type(kind: &str) -> bool {
    matches!(
        kind,
        "call_expression"
            | "member_expression"
            | "subscript_expression"
            | "new_expression"
            | "assignment_expression"
            | "augmented_assignment_expression"
            | "binary_expression"
            | "unary_expression"
            | "update_expression"
            | "ternary_expression"
            | "elvis_expression"
            | "parenthesized_expression"
            | "sequence_expression"
            | "string"
            | "array"
            | "object"
            | "ordered_struct"
            | "function_expression"
            | "arrow_function"
    )
}

/// An expression in tag markup and the scope that owns its calls.
struct TagExpression {
    start: usize,
    end: usize,
    /// Where it starts in the file (UTF-16 column).
    row: u32,
    column: u32,
    /// Its start column in the synthesized source.
    text_column: u32,
    scope_id: String,
}

struct Cfml<'a> {
    file_path: &'a str,
    source: &'a str,
    language: Language,
    line_starts: Vec<usize>,
    out: ExtractionResult,
    ids: NodeIdAllocator,
    tag_expressions: Vec<TagExpression>,
    now: i64,
    /// The tag walk reached the stack's red zone (the delegated walks arm their own guard).
    overflowed: bool,
}

/// Extract a CFML file. `language` is the detected one — `cfml` for `.cfc`/`.cfm`, `cfscript`
/// for `.cfs` — and is stamped onto every node and reference.
pub fn extract(file_path: &str, source: &str, language: Language) -> Result<ExtractionResult, String> {
    let started = std::time::Instant::now();
    let mut x = Cfml {
        file_path,
        source,
        language,
        line_starts: line_starts(source),
        out: ExtractionResult::default(),
        ids: NodeIdAllocator::new(),
        tag_expressions: Vec::new(),
        now: crate::decode::codegraph_now_ms(),
        overflowed: false,
    };
    if is_bare_script_cfml(source) {
        x.extract_bare_script()?;
    } else {
        x.extract_tag_based()?;
    }
    let mut out = x.out;
    out.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(out)
}

impl<'a> Cfml<'a> {
    fn text(&self, node: SyntaxNode) -> &'a str {
        &self.source[node.byte_range()]
    }
    fn col(&self, node: SyntaxNode) -> u32 {
        col16(self.source, &self.line_starts, node.start_position().row, node.start_byte())
    }
    fn end_col(&self, node: SyntaxNode) -> u32 {
        col16(self.source, &self.line_starts, node.end_position().row, node.end_byte())
    }
    fn line(node: SyntaxNode) -> u32 {
        node.start_position().row as u32 + 1
    }
    fn end_line(node: SyntaxNode) -> u32 {
        node.end_position().row as u32 + 1
    }

    fn component_name_from_path(&self) -> String {
        let file_name = self.file_path.rsplit(['/', '\\']).next().filter(|s| !s.is_empty()).unwrap_or(self.file_path);
        for ext in [".cfc", ".cfm", ".cfs"] {
            if file_name.len() >= ext.len() && file_name[file_name.len() - ext.len()..].eq_ignore_ascii_case(ext) {
                return file_name[..file_name.len() - ext.len()].to_string();
            }
        }
        file_name.to_string()
    }

    fn reference(&mut self, from: &str, name: &str, kind: EdgeKind, line: u32, column: u32) {
        self.out.unresolved_references.push(UnresolvedReference {
            from_node_id: from.to_string(),
            reference_name: name.to_string(),
            reference_kind: ReferenceKind::Edge(kind),
            line,
            column,
            file_path: Some(self.file_path.to_string()),
            language: Some(self.language),
            candidates: None,
            row_id: None,
        });
    }

    fn contains(&mut self, source: &str, target: &str) {
        self.out.edges.push(Edge::new(source, target, EdgeKind::Contains));
    }

    // --- bare script ------------------------------------------------------------------------

    /// Modern bare-script `.cfc`/`.cfm`: the whole file through the cfscript walker. The
    /// component has no name in source — it is the file's — so the walker's `<anonymous>` class
    /// (or interface) takes the file's name, and so do its members' scope chains.
    fn extract_bare_script(&mut self) -> Result<(), String> {
        let result = crate::extract_file(self.file_path, self.source, Language::CfScript)?;
        let component_name = self.component_name_from_path();
        for mut node in result.nodes {
            node.language = self.language;
            if node.name == "<anonymous>" && matches!(node.kind, NodeKind::Class | NodeKind::Interface) {
                node.name = component_name.clone();
                node.qualified_name = format!("{}::{component_name}", self.file_path);
            } else if node.qualified_name == "<anonymous>" || node.qualified_name.starts_with("<anonymous>::") {
                node.qualified_name = format!("{component_name}{}", &node.qualified_name["<anonymous>".len()..]);
            }
            self.out.nodes.push(node);
        }
        self.out.edges.extend(result.edges);
        for mut r in result.unresolved_references {
            r.language = Some(self.language);
            self.out.unresolved_references.push(r);
        }
        self.out.errors.extend(result.errors);
        Ok(())
    }

    // --- tag-based ----------------------------------------------------------------------------

    fn extract_tag_based(&mut self) -> Result<(), String> {
        let grammar = crate::langs::grammar_for("cfml").ok_or("cfml grammar not loaded")?;
        let tree = crate::ts::parse(&grammar, "cfml", self.source)?;
        let file_id = self.create_file_node();
        self.walk_program(tree.root_node(), &file_id)?;
        if self.overflowed {
            return Err("defer: nesting too deep for the native walker — wasm recovery handles it".to_string());
        }
        self.extract_tag_expressions()
    }

    /// The file's own node, spanning the whole source.
    fn create_file_node(&mut self) -> String {
        let id = node_id(self.file_path, NodeKind::File, self.file_path, 1);
        let name = self.file_path.rsplit(['/', '\\']).next().filter(|s| !s.is_empty()).unwrap_or(self.file_path);
        let last_line = self.source.rsplit('\n').next().unwrap_or("");
        self.out.nodes.push(Node::new(
            &id,
            NodeKind::File,
            name,
            self.file_path,
            self.file_path,
            self.language,
            1,
            self.line_starts.len() as u32,
            0,
            utf16_len(last_line) as u32,
            self.now,
        ));
        id
    }

    /// `program`'s children with one forward cursor: a component consumes the siblings that
    /// follow its open tag, so the walk resumes after whatever it consumed.
    fn walk_program(&mut self, root: SyntaxNode, file_id: &str) -> Result<(), String> {
        let mut child = root.named_child(0);
        while let Some(c) = child {
            if c.kind() == "cf_component_open_tag" {
                child = self.extract_component(c, file_id)?.next_sibling();
                continue;
            }
            self.visit_tag(c, root.kind(), file_id, None, true)?;
            child = c.next_sibling();
        }
        Ok(())
    }

    /// `<cfcomponent extends="Base" implements="IFoo,IBar">…</cfcomponent>`: the body is the open
    /// tag's following siblings up to the close tag (the grammar's implicit end tags).
    fn extract_component<'t>(&mut self, open_tag: SyntaxNode<'t>, container_id: &str) -> Result<SyntaxNode<'t>, String> {
        let name = self.tag_attr(open_tag, "name").unwrap_or_else(|| self.component_name_from_path());
        let line = Self::line(open_tag);
        let column = self.col(open_tag);
        let id = self.ids.generate(self.file_path, NodeKind::Class, &name, line, column);
        let mut class_node =
            Node::new(&id, NodeKind::Class, &name, format!("{}::{name}", self.file_path), self.file_path, self.language, line, line, column, self.end_col(open_tag), self.now);
        class_node.is_exported = Some(true);
        let class_index = self.out.nodes.len();
        self.out.nodes.push(class_node);
        self.contains(container_id, &id);

        if let Some(extends) = self.tag_attr(open_tag, "extends").filter(|s| !s.is_empty()) {
            self.reference(&id, &extends, EdgeKind::Extends, line, column);
        }
        let implements = self.tag_attr(open_tag, "implements");
        self.push_inheritance_refs(&id, implements, EdgeKind::Implements, open_tag);

        let parent_type = open_tag.parent().map(|p| p.kind()).unwrap_or("");
        let class_name = Some(name).filter(|n| !n.is_empty());
        let mut sibling = open_tag.next_sibling();
        let mut last = open_tag;
        while let Some(s) = sibling {
            if s.kind() == "cf_component_close_tag" {
                last = s;
                break;
            }
            self.visit_tag(s, parent_type, &id, class_name.as_deref(), true)?;
            last = s;
            sibling = s.next_sibling();
        }
        self.out.nodes[class_index].end_line = Self::end_line(last);
        Ok(last)
    }

    /// `<cfinterface extends="IBase,IOther">…</cfinterface>` (#2091): a generic `cf_tag` whose
    /// body holds the `<cffunction>` signatures.
    fn extract_interface_tag(&mut self, tag: SyntaxNode, container_id: &str) -> Result<(), String> {
        let start_tag = self.cf_start_tag(tag).unwrap_or(tag);
        let name = self.tag_attr(start_tag, "name").unwrap_or_else(|| self.component_name_from_path());
        let line = Self::line(tag);
        let column = self.col(tag);
        let id = self.ids.generate(self.file_path, NodeKind::Interface, &name, line, column);
        let mut node = Node::new(
            &id,
            NodeKind::Interface,
            &name,
            format!("{}::{name}", self.file_path),
            self.file_path,
            self.language,
            line,
            Self::end_line(tag),
            column,
            self.end_col(tag),
            self.now,
        );
        node.is_exported = Some(true);
        self.out.nodes.push(node);
        self.contains(container_id, &id);
        let extends = self.tag_attr(start_tag, "extends");
        self.push_inheritance_refs(&id, extends, EdgeKind::Extends, start_tag);
        let class_name = Some(name).filter(|n| !n.is_empty());
        self.delegate_nested_tags(tag, &id, class_name.as_deref(), true)
    }

    /// One `extends` / `implements` reference per name of a comma-separated attribute.
    fn push_inheritance_refs(&mut self, from: &str, list: Option<String>, kind: EdgeKind, tag: SyntaxNode) {
        let Some(list) = list.filter(|l| !l.is_empty()) else { return };
        let (line, column) = (Self::line(tag), self.col(tag));
        for name in list.split(',').map(js_trim).filter(|s| !s.is_empty()) {
            self.reference(from, name, kind, line, column);
        }
    }

    /// `<cffunction name="…" access="…" returntype="…">…</cffunction>`: a method when it sits in
    /// a component or interface (`parent_class_id`), else a function of the file.
    fn extract_function_tag(&mut self, tag: SyntaxNode, parent_class: bool, container_id: &str, parent_class_name: Option<&str>) -> Result<(), String> {
        let Some(name) = self.tag_attr(tag, "name").filter(|n| !n.is_empty()) else { return Ok(()) };
        let kind = if parent_class { NodeKind::Method } else { NodeKind::Function };
        let line = Self::line(tag);
        let column = self.col(tag);
        let id = self.ids.generate(self.file_path, kind, &name, line, column);
        let visibility = match self.tag_attr(tag, "access").as_deref() {
            Some("private") => Some(Visibility::Private),
            Some("package") => Some(Visibility::Internal),
            Some(a) if !a.is_empty() => Some(Visibility::Public),
            _ => None,
        };
        let qualified_name = match parent_class_name {
            Some(class) => format!("{class}::{name}"),
            None => format!("{}::{name}", self.file_path),
        };
        let mut node =
            Node::new(&id, kind, &name, qualified_name, self.file_path, self.language, line, Self::end_line(tag), column, self.end_col(tag), self.now);
        node.visibility = visibility;
        node.return_type = self.tag_attr(tag, "returntype");
        self.out.nodes.push(node);
        self.contains(container_id, &id);
        // Its body, at any depth: its <cfscript>/<cfquery> bodies and tag expressions are its own.
        self.delegate_nested_tags(tag, &id, None, false)
    }

    /// One node of tag markup (CfmlExtractor.visitTag). `container_id` owns its calls;
    /// `decl_scope` is true outside function bodies; `parent_class_name` is set at component or
    /// interface scope (and never empty — an empty name is no scope, as in the TS truthiness test).
    fn visit_tag(&mut self, node: SyntaxNode, parent_type: &str, container_id: &str, parent_class_name: Option<&str>, decl_scope: bool) -> Result<(), String> {
        if crate::stack::exhausted() {
            self.overflowed = true;
            return Ok(());
        }
        match node.kind() {
            "cf_script_tag" => return self.delegate_script_tag(node, container_id, parent_class_name),
            "cf_query_tag" => self.delegate_query_tag(node, container_id)?,
            "cf_function_tag" => {
                if decl_scope {
                    self.extract_function_tag(node, parent_class_name.is_some(), container_id, parent_class_name)?;
                }
                return Ok(());
            }
            "cf_selfclose_tag" => {
                if self.self_close_tag_name(node).as_deref() == Some("include") {
                    self.extract_include_tag(node, container_id);
                }
            }
            "cf_tag" => {
                let name = self.cf_tag_name(node);
                if name.as_deref() == Some("interface") && decl_scope && parent_class_name.is_none() {
                    return self.extract_interface_tag(node, container_id);
                }
                // `<cfloop condition="hasNext()">`: a plain-text value that is an expression.
                if name.as_deref() == Some("loop") {
                    let start = self.cf_start_tag(node).unwrap_or(node);
                    if let Some(Some(condition)) = self.tag_attr_value_node(start, "condition", false) {
                        self.add_tag_expression(condition, container_id);
                    }
                }
            }
            kind => {
                if is_tag_expression_parent(parent_type) && is_tag_expression_type(kind) {
                    self.add_tag_expression(node, container_id);
                    return Ok(());
                }
            }
        }
        self.delegate_nested_tags(node, container_id, parent_class_name, decl_scope && node.kind() != "ERROR")
    }

    fn delegate_nested_tags(&mut self, node: SyntaxNode, container_id: &str, parent_class_name: Option<&str>, decl_scope: bool) -> Result<(), String> {
        for i in 0..node.named_child_count() {
            if let Some(child) = node.named_child(i) {
                self.visit_tag(child, node.kind(), container_id, parent_class_name, decl_scope)?;
            }
        }
        Ok(())
    }

    fn add_tag_expression(&mut self, node: SyntaxNode, scope_id: &str) {
        if node.end_byte() <= node.start_byte() {
            return;
        }
        self.tag_expressions.push(TagExpression {
            start: node.start_byte(),
            end: node.end_byte(),
            row: node.start_position().row as u32,
            column: self.col(node),
            text_column: 0,
            scope_id: scope_id.to_string(),
        });
    }

    /// The tag expressions' calls, through one cfscript extraction: the expressions copied out in
    /// order, each ended with `;`, with the line breaks between them kept so every line is the
    /// file's. Each reference goes back to the scope whose expression holds it, its first-line
    /// column shifted back to the file's. Only references are kept, and only genuine errors.
    fn extract_tag_expressions(&mut self) -> Result<(), String> {
        if self.tag_expressions.is_empty() {
            return Ok(());
        }
        let mut exprs = std::mem::take(&mut self.tag_expressions);
        exprs.sort_by_key(|e| e.start);
        let src = self.source;
        let mut text_out = String::new();
        let mut pos = 0usize;
        let mut column = 0u32;
        for expr in exprs.iter_mut() {
            if pos > 0 {
                text_out.push(';');
                column += 1;
            }
            let breaks = if pos < expr.start { src[pos..expr.start].bytes().filter(|b| *b == b'\n').count() } else { 0 };
            if breaks > 0 {
                text_out.extend(std::iter::repeat_n('\n', breaks));
                column = 0;
            }
            expr.text_column = column;
            let text = &src[expr.start..expr.end];
            text_out.push_str(text);
            column = match text.rfind('\n') {
                None => column + utf16_len(text) as u32,
                Some(at) => utf16_len(&text[at + 1..]) as u32,
            };
            pos = expr.end;
        }

        let result = crate::extract_file(self.file_path, &text_out, Language::CfScript)?;
        for mut r in result.unresolved_references {
            let expr = tag_expression_at(&exprs, r.line.saturating_sub(1), r.column);
            if r.line.saturating_sub(1) == expr.row {
                r.column = (i64::from(r.column) + i64::from(expr.column) - i64::from(expr.text_column)) as u32;
            }
            r.from_node_id = expr.scope_id.clone();
            r.file_path = Some(self.file_path.to_string());
            r.language = Some(self.language);
            self.out.unresolved_references.push(r);
        }
        self.out.errors.extend(result.errors.into_iter().filter(|e| e.severity == Severity::Error));
        Ok(())
    }

    /// `<cfinclude template="x.cfm">` (#2651): an import node in its scope and an `imports`
    /// reference from that scope. A template built at runtime names no file.
    fn extract_include_tag(&mut self, tag: SyntaxNode, scope_id: &str) {
        let template = match self.tag_attr_value_node(tag, "template", true) {
            Some(Some(value)) => js_trim(self.text(value)).to_string(),
            _ => String::new(),
        };
        if template.is_empty() {
            return;
        }
        let prefix = match self.out.nodes.iter().find(|n| n.id == scope_id) {
            None => String::new(),
            Some(scope) if scope.kind == NodeKind::File => String::new(),
            Some(scope) if scope.kind == NodeKind::Method => scope.qualified_name.clone(),
            Some(scope) => scope.name.clone(),
        };
        let line = Self::line(tag);
        let column = self.col(tag);
        let id = self.ids.generate(self.file_path, NodeKind::Import, &template, line, column);
        let qualified_name = if prefix.is_empty() { template.clone() } else { format!("{prefix}::{template}") };
        let mut node =
            Node::new(&id, NodeKind::Import, &template, qualified_name, self.file_path, self.language, line, Self::end_line(tag), column, self.end_col(tag), self.now);
        node.signature = Some(js_trim(self.text(tag)).to_string());
        self.out.nodes.push(node);
        self.contains(scope_id, &id);
        self.reference(scope_id, &template, EdgeKind::Imports, line, column);
    }

    /// A `<cfscript>` body through the cfscript walker. At component scope its top-level
    /// functions are the component's methods and every symbol is qualified under it. The
    /// snippet's own file node is dropped; its calls — and those of a top-level `var x = …`,
    /// which in a function body is a local — belong to the enclosing scope.
    fn delegate_script_tag(&mut self, script_tag: SyntaxNode, parent_id: &str, parent_class_name: Option<&str>) -> Result<(), String> {
        let Some(content) = named_children(script_tag).into_iter().find(|c| c.kind() == "cf_script_content") else { return Ok(()) };
        let inner = self.text(content);
        let start_line = content.start_position().row as u32;
        let result = crate::extract_file(self.file_path, inner, Language::CfScript)?;

        let inner_file_id = result.nodes.iter().find(|n| n.kind == NodeKind::File).map(|n| n.id.clone());
        let is_inner_file = |id: &str| inner_file_id.as_deref() == Some(id);
        let top_level: HashSet<String> = result
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Contains && is_inner_file(&e.source))
            .map(|e| e.target.clone())
            .collect();
        let local_vars: HashSet<String> = result
            .nodes
            .iter()
            .filter(|n| top_level.contains(&n.id) && matches!(n.kind, NodeKind::Variable | NodeKind::Constant))
            .map(|n| n.id.clone())
            .collect();
        for mut node in result.nodes {
            if node.kind == NodeKind::File {
                continue;
            }
            node.start_line += start_line;
            node.end_line += start_line;
            node.language = self.language;
            if let Some(class) = parent_class_name {
                if node.kind == NodeKind::Function && top_level.contains(&node.id) {
                    node.kind = NodeKind::Method;
                }
                node.qualified_name = format!("{class}::{}", node.qualified_name);
            }
            let id = node.id.clone();
            self.out.nodes.push(node);
            self.contains(parent_id, &id);
        }
        for mut edge in result.edges {
            if is_inner_file(&edge.source) || is_inner_file(&edge.target) {
                continue;
            }
            if let Some(line) = edge.line.as_mut().filter(|l| **l != 0) {
                *line += start_line;
            }
            self.out.edges.push(edge);
        }
        for mut r in result.unresolved_references {
            r.line += start_line;
            r.file_path = Some(self.file_path.to_string());
            r.language = Some(self.language);
            if r.from_node_id.is_empty() || is_inner_file(&r.from_node_id) || local_vars.contains(&r.from_node_id) {
                r.from_node_id = parent_id.to_string();
            }
            self.out.unresolved_references.push(r);
        }
        self.add_block_errors(result.errors, script_tag, "cfscript", start_line);
        Ok(())
    }

    /// A `<cfquery>` body's `#…#` expressions through the cfquery walker: references only, those
    /// of the snippet's file node going to the enclosing scope.
    fn delegate_query_tag(&mut self, query_tag: SyntaxNode, parent_id: &str) -> Result<(), String> {
        let Some(content) = named_children(query_tag).into_iter().find(|c| c.kind() == "cf_query_content") else { return Ok(()) };
        let sql = self.text(content);
        let start_line = content.start_position().row as u32;
        let result = crate::extract_file(self.file_path, sql, Language::CfQuery)?;
        let inner_file_id = result.nodes.iter().find(|n| n.kind == NodeKind::File).map(|n| n.id.clone());
        for mut r in result.unresolved_references {
            r.line += start_line;
            r.file_path = Some(self.file_path.to_string());
            r.language = Some(self.language);
            if r.from_node_id.is_empty() || inner_file_id.as_deref() == Some(r.from_node_id.as_str()) {
                r.from_node_id = parent_id.to_string();
            }
            self.out.unresolved_references.push(r);
        }
        self.add_block_errors(result.errors, query_tag, "cfquery", start_line);
        Ok(())
    }

    /// A delegated block's errors as the file's (#2654): its "no symbols" warning becomes a
    /// partial-parse warning naming the block; other errors pass through, lines shifted.
    fn add_block_errors(&mut self, errors: Vec<ExtractionError>, tag: SyntaxNode, tag_name: &str, line_offset: u32) {
        for mut error in errors {
            if error.severity == Severity::Warning && error.code.as_deref() == Some("parse_error") {
                let line = Self::line(tag);
                self.out.errors.push(ExtractionError {
                    message: format!(
                        "{}: <{tag_name}> at line {line} has a syntax error — the rest of the file is indexed, but calls inside that block may be missing from the graph",
                        self.file_path
                    ),
                    file_path: None,
                    line: Some(line),
                    column: None,
                    severity: Severity::Warning,
                    code: Some("partial_parse".to_string()),
                });
                continue;
            }
            if let Some(line) = error.line.as_mut().filter(|l| **l != 0) {
                *line += line_offset;
            }
            self.out.errors.push(error);
        }
    }

    // --- tag helpers --------------------------------------------------------------------------

    /// A generic `cf_tag`'s start tag.
    fn cf_start_tag<'t>(&self, tag: SyntaxNode<'t>) -> Option<SyntaxNode<'t>> {
        named_children(tag).into_iter().find(|c| matches!(c.kind(), "cf_start_tag" | "cf_start_tag_with_selfclose"))
    }

    /// A generic `cf_tag`'s name, lowercased, without the `cf` prefix.
    fn cf_tag_name(&self, tag: SyntaxNode) -> Option<String> {
        let start = self.cf_start_tag(tag)?;
        let name = named_children(start).into_iter().find(|c| c.kind() == "cf_tag_name")?;
        Some(self.text(name).to_lowercase())
    }

    /// A void tag's name, lowercased, without the `cf` prefix: the text after `<cf`
    /// (`/^<cf([A-Za-z_]\w*)/i`).
    fn self_close_tag_name(&self, tag: SyntaxNode) -> Option<String> {
        let text = self.text(tag);
        let rest = text.get(..3).filter(|p| p.eq_ignore_ascii_case("<cf")).map(|_| &text[3..])?;
        let mut chars = rest.char_indices();
        let (_, first) = chars.next()?;
        if !(first.is_ascii_alphabetic() || first == '_') {
            return None;
        }
        let end = chars.find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_')).map(|(i, _)| i).unwrap_or(rest.len());
        Some(rest[..end].to_lowercase())
    }

    /// A tag attribute's value text: None when the tag has no such attribute, `""` when its
    /// value is not plain text.
    fn tag_attr(&self, tag: SyntaxNode, attr_name: &str) -> Option<String> {
        match self.tag_attr_value_node(tag, attr_name, false) {
            None => None,
            Some(None) => Some(String::new()),
            Some(Some(value)) => Some(self.text(value).to_string()),
        }
    }

    /// The `attribute_value` node of a tag attribute: outer None when the tag has no such
    /// attribute, inner None when its value is not plain text (empty, or `#hash#`). With
    /// `whole_value`, also inner None when only part of the value is plain text.
    fn tag_attr_value_node<'t>(&self, tag: SyntaxNode<'t>, attr_name: &str, whole_value: bool) -> Option<Option<SyntaxNode<'t>>> {
        let mut attrs = Vec::new();
        for child in named_children(tag) {
            if child.kind() == "cf_attribute" {
                attrs.push(child);
            } else if child.kind() == "cf_tag_attributes" {
                attrs.extend(named_children(child).into_iter().filter(|c| c.kind() == "cf_attribute"));
            }
        }
        for attr in attrs {
            let children = named_children(attr);
            let Some(name) = children.iter().find(|c| c.kind() == "cf_attribute_name") else { continue };
            if self.text(*name).to_lowercase() != attr_name.to_lowercase() {
                continue;
            }
            let wrapper = children.iter().find(|c| matches!(c.kind(), "quoted_cf_attribute_value" | "cf_attribute_value"));
            if whole_value && wrapper.map(|w| w.named_child_count()) != Some(1) {
                return Some(None);
            }
            return Some(wrapper.and_then(|w| named_children(*w).into_iter().find(|c| c.kind() == "attribute_value")));
        }
        None
    }
}

/// The recorded expression holding a synthesized-source position: the last one starting at or
/// before it.
fn tag_expression_at(exprs: &[TagExpression], row: u32, column: u32) -> &TagExpression {
    let (mut lo, mut hi, mut found) = (0isize, exprs.len() as isize - 1, 0usize);
    while lo <= hi {
        let mid = ((lo + hi) >> 1) as usize;
        let e = &exprs[mid];
        if e.row < row || (e.row == row && e.text_column <= column) {
            found = mid;
            lo = mid as isize + 1;
        } else {
            hi = mid as isize - 1;
        }
    }
    &exprs[found]
}

fn named_children(node: SyntaxNode) -> Vec<SyntaxNode> {
    (0..node.named_child_count()).filter_map(|i| node.named_child(i)).collect()
}

/// isBareScriptCfml: the first real token — past a BOM, whitespace and `//` / `/* */`
/// comments — is not `<`. An empty file counts as script.
pub fn is_bare_script_cfml(source: &str) -> bool {
    let mut rest = source;
    loop {
        let Some(ch) = rest.chars().next() else { return true };
        if matches!(ch, ' ' | '\t' | '\n' | '\r' | '\u{FEFF}') {
            rest = &rest[ch.len_utf8()..];
        } else if rest.starts_with("//") {
            rest = rest.find('\n').map_or("", |nl| &rest[nl + 1..]);
        } else if rest.starts_with("/*") {
            rest = rest[2..].find("*/").map_or("", |end| &rest[2 + end + 2..]);
        } else {
            return ch != '<';
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_script_sniffing() {
        assert!(is_bare_script_cfml(""));
        assert!(is_bare_script_cfml("\u{FEFF} // c\n/* x */ component {}"));
        assert!(!is_bare_script_cfml("\u{FEFF}\n<cfcomponent>"));
        assert!(!is_bare_script_cfml("/* a */<cfset x = 1>"));
        assert!(is_bare_script_cfml("/* unterminated"));
    }
}
