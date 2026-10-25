//! Terraform extraction — a port of languages/terraform.ts over the generic walk of
//! `TreeSitterExtractor` (src/extraction/tree-sitter.ts).
//!
//! terraform.ts sets every type list empty and does all its work in `visitNode`: a top-level
//! `block` is read by its first identifier (resource, data, module, variable, output, provider,
//! locals, …) and its labels; the references are the dotted names in its expressions
//! (`var.x`, `local.y`, `module.m`, `data.t.n`, `aws_x.y`). Every other node is walked through.
//! The grammar is the TypeScript engine's own tree-sitter-hcl wasm.

use crate::buffers::{
    build_meta, edge_kind_index, node_kind_index, Arena, BoolFlags, EdgeRow, EmitOut, NodeRow,
    RefRow, Tables, FLAG_IS_EXPORTED, NONE, NONE_STR,
};
use crate::ids;
use crate::textutil as util;
use crate::ts::Node;
use std::collections::VecDeque;

/// Reference heads that never name a project symbol.
const BUILTIN_HEADS: [&str; 8] = ["each", "count", "self", "path", "terraform", "null", "true", "false"];

/// Module meta-arguments: they configure the call, not one of the child module's inputs.
const MODULE_META_ARGS: [&str; 6] = ["source", "version", "count", "for_each", "providers", "depends_on"];

struct Walker<'t> {
    src: &'t str,
    file_path: &'t str,
    line_starts: Vec<usize>,
    arena: Arena,
    node_id_allocator: ids::NodeIdAllocator,
    tables: Tables,
    /// The scopes' rows (the file node first).
    stack: Vec<u32>,
}

pub fn extract(file_path: &str, source: &str) -> Result<EmitOut, String> {
    let grammar = crate::langs::grammar_for("terraform").ok_or("no terraform grammar")?;
    let t0 = std::time::Instant::now();
    let tree = crate::ts::parse(&grammar, "terraform", source)?;
    let mut w = Walker {
        src: source,
        file_path,
        line_starts: util::line_starts(source),
        arena: Arena::default(),
        node_id_allocator: ids::NodeIdAllocator::default(),
        tables: Tables::default(),
        stack: Vec::new(),
    };

    // File node (tree-sitter.ts extract()).
    let line_count = source.bytes().filter(|b| *b == b'\n').count() as u32 + 1;
    let base_name = file_path.rsplit(['/', '\\']).next().unwrap_or(file_path);
    let mut flags = BoolFlags::default();
    flags.set(FLAG_IS_EXPORTED, false);
    let file_id = w.arena.put(&ids::file_node_id(file_path));
    let name_ref = w.arena.put(base_name);
    let qn_ref = w.arena.put(file_path);
    w.tables.push_node(&NodeRow {
        kind: node_kind_index("file").unwrap(),
        visibility: 0,
        flags,
        start_line: 1,
        end_line: line_count,
        start_column: 0,
        end_column: 0,
        name: name_ref,
        qualified_name: qn_ref,
        id: file_id,
        docstring: NONE_STR,
        signature: NONE_STR,
        decorators: NONE_STR,
        type_parameters: NONE_STR,
        return_type: NONE_STR,
        extra_json: NONE_STR,
    });
    w.stack.push(0);
    w.visit(tree.root_node());
    w.stack.pop();

    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let meta = build_meta(&w.tables, w.arena.len(), NONE_STR, duration_ms);
    Ok(EmitOut { meta, nodes: w.tables.nodes, edges: w.tables.edges, refs: w.tables.refs, arena: w.arena.into_vec() })
}

fn named_children<'t>(node: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn child_of_kind<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    named_children(node).into_iter().find(|c| c.kind() == kind)
}

/// findStringLit (terraform.ts): the first string_lit under an expression, breadth first.
fn find_string_lit(expr: Node) -> Option<Node> {
    let mut queue = VecDeque::from([expr]);
    while let Some(n) = queue.pop_front() {
        if n.kind() == "string_lit" {
            return Some(n);
        }
        queue.extend(named_children(n));
    }
    None
}

/// qualifyReference (terraform.ts): the names a reference chain `head.attrs…` is read as.
fn qualify_reference(head: &str, attrs: &[&str]) -> Vec<String> {
    let first = attrs.first().copied().filter(|a| !a.is_empty());
    let second = attrs.get(1).copied().filter(|a| !a.is_empty());
    match head {
        "var" => first.map(|a| vec![format!("var.{a}")]).unwrap_or_default(),
        "local" => first.map(|a| vec![format!("local.{a}")]).unwrap_or_default(),
        "module" => {
            let Some(m) = first else { return Vec::new() };
            let mut refs = vec![format!("module.{m}")];
            if let Some(out) = second {
                refs.push(format!("module.{m}:output.{out}"));
            }
            if second == Some("outputs") {
                if let Some(x) = attrs.get(2).copied().filter(|a| !a.is_empty()) {
                    refs.push(format!("module.{m}:remote-output.{x}"));
                }
            }
            refs
        }
        "data" => match (first, second) {
            (Some(t), Some(n)) => vec![format!("data.{t}.{n}")],
            _ => Vec::new(),
        },
        _ => first.map(|a| vec![format!("{head}.{a}")]).unwrap_or_default(),
    }
}

/// describeBlock (terraform.ts): (kind, name, qualifiedName, signature).
fn describe_block(kind: &str, labels: &[String]) -> Option<(&'static str, String, String, String)> {
    let first = labels.first().filter(|l| !l.is_empty());
    let second = labels.get(1).filter(|l| !l.is_empty());
    match kind {
        "resource" => {
            let (f, s) = (first?, second?);
            Some(("class", format!("{f}.{s}"), format!("{f}.{s}"), format!("resource \"{f}\" \"{s}\"")))
        }
        "data" => {
            let (f, s) = (first?, second?);
            Some(("class", format!("{f}.{s}"), format!("data.{f}.{s}"), format!("data \"{f}\" \"{s}\"")))
        }
        "module" => first.map(|f| ("module", f.clone(), format!("module.{f}"), format!("module \"{f}\""))),
        "variable" => first.map(|f| ("variable", f.clone(), format!("var.{f}"), format!("variable \"{f}\""))),
        "output" => first.map(|f| ("variable", f.clone(), format!("output.{f}"), format!("output \"{f}\""))),
        "provider" => first.map(|f| ("namespace", f.clone(), format!("provider.{f}"), format!("provider \"{f}\""))),
        _ => None,
    }
}

impl<'t> Walker<'t> {
    fn text(&self, node: Node) -> &'t str {
        &self.src[node.byte_range()]
    }
    fn line_of(&self, node: Node) -> u32 {
        node.start_position().row as u32 + 1
    }
    fn col_of(&self, node: Node) -> u32 {
        util::col16(self.src, &self.line_starts, node.start_position().row, node.start_byte())
    }
    fn end_col_of(&self, node: Node) -> u32 {
        util::col16(self.src, &self.line_starts, node.end_position().row, node.end_byte())
    }

    fn push_ref(&mut self, from_idx: u32, name: &str, kind: &str, at: Node) {
        let reference_name = self.arena.put(name);
        self.tables.push_ref(&RefRow {
            from_idx,
            kind: edge_kind_index(kind).unwrap(),
            line: self.line_of(at),
            column: self.col_of(at),
            reference_name,
            candidates: NONE_STR,
            from_id_str: NONE_STR,
        });
    }

    /// createNode (tree-sitter.ts) with terraform.ts's explicit qualified name and signature.
    fn create_node(&mut self, kind: &'static str, name: &str, node: Node<'t>, qualified: &str, signature: &str, exported: Option<bool>) -> Option<u32> {
        if name.is_empty() {
            return None;
        }
        let start_line = self.line_of(node);
        let column = self.col_of(node);
        let id = self.node_id_allocator.generate(self.file_path, kind, name, start_line, column);
        let mut flags = BoolFlags::default();
        if let Some(exported) = exported {
            flags.set(FLAG_IS_EXPORTED, exported);
        }
        let name_ref = self.arena.put(name);
        let qn_ref = self.arena.put(qualified);
        let id_ref = self.arena.put(&id);
        let sig_ref = self.arena.put(signature);
        let row = self.tables.push_node(&NodeRow {
            kind: node_kind_index(kind).unwrap(),
            visibility: 0,
            flags,
            start_line,
            end_line: node.end_position().row as u32 + 1,
            start_column: column,
            end_column: self.end_col_of(node),
            name: name_ref,
            qualified_name: qn_ref,
            id: id_ref,
            docstring: NONE_STR,
            signature: sig_ref,
            decorators: NONE_STR,
            type_parameters: NONE_STR,
            return_type: NONE_STR,
            extra_json: NONE_STR,
        });
        let parent_row = self.stack.last().copied().unwrap_or(0);
        self.tables.push_edge(&EdgeRow {
            source_idx: parent_row,
            target_idx: row,
            kind: edge_kind_index("contains").unwrap(),
            provenance: 0,
            line: NONE,
            column: NONE,
            metadata_json: NONE_STR,
            source_id_str: NONE_STR,
            target_id_str: NONE_STR,
        });
        Some(row)
    }

    fn visit(&mut self, node: Node<'t>) {
        stack_guard!();
        if self.hook(node) {
            return;
        }
        for child in named_children(node) {
            self.visit(child);
        }
    }

    /// The identifier an attribute sets.
    fn attr_name(&self, attr: Node<'t>) -> Option<&'t str> {
        child_of_kind(attr, "identifier").map(|id| self.text(id))
    }

    /// stringLitValue (terraform.ts): the literal between the quotes; "" for an empty string.
    fn string_lit_value(&self, lit: Node) -> String {
        child_of_kind(lit, "template_literal").map(|l| self.text(l).to_string()).unwrap_or_default()
    }

    /// The visitNode hook (terraform.ts). Returns true when consumed.
    fn hook(&mut self, node: Node<'t>) -> bool {
        if node.kind() != "block" {
            // A .tfvars assignment sets the root module's variable of that name.
            let top_level = node.parent().is_some_and(|p| p.kind() == "body" && p.parent().is_some_and(|g| g.kind() == "config_file"));
            if node.kind() == "attribute" && self.file_path.ends_with(".tfvars") && top_level {
                if let Some(name) = self.attr_name(node) {
                    self.push_ref(0, &format!("var.{name}"), "references", node);
                }
                return true;
            }
            return false;
        }
        let named = named_children(node);
        let Some(first) = named.first().filter(|f| f.kind() == "identifier") else { return false };
        let kind = self.text(*first);
        let mut labels = Vec::new();
        for child in &named[1..] {
            match child.kind() {
                "string_lit" => labels.push(self.string_lit_value(*child)),
                "identifier" => labels.push(self.text(*child).to_string()),
                _ => break,
            }
        }
        let body = child_of_kind(node, "body");

        if labels.is_empty() {
            match kind {
                "locals" => {
                    if let Some(body) = body {
                        self.emit_locals(body);
                    }
                    return true;
                }
                "terraform" => return true,
                // State-migration blocks and check assertions: their references belong to the file.
                "moved" | "import" | "removed" | "assert" => {
                    if let Some(body) = body {
                        self.emit_references_in_body(body, 0, true, &[]);
                    }
                    return true;
                }
                _ => {}
            }
        }

        let Some((node_kind, mut name, mut qualified, mut signature)) = describe_block(kind, &labels) else {
            return false;
        };
        // provider "aws" { alias = "east" } is addressed as `aws.east`.
        if let (true, Some(body), Some(provider)) = (kind == "provider", body, labels.first().filter(|l| !l.is_empty())) {
            if let Some(alias) = self.read_string_attr(body, "alias").filter(|a| !a.is_empty()) {
                name = format!("{provider}.{alias}");
                qualified = format!("provider.{provider}.{alias}");
                signature = format!("provider \"{provider}\" alias=\"{alias}\"");
            }
        }
        let Some(row) = self.create_node(node_kind, &name, node, &qualified, &signature, Some(node_kind == "variable")) else {
            return true;
        };
        if let Some(body) = body {
            self.stack.push(row);
            let mut skip: Vec<&str> = Vec::new();
            if kind == "resource" || kind == "data" {
                self.emit_provider_selection_ref(body, row);
                skip.push("provider");
            }
            if kind == "module" {
                self.emit_module_providers_refs(body, row);
                skip.push("providers");
            }
            self.emit_references_in_body(body, row, false, &skip);
            if kind == "module" {
                if let Some(module) = labels.first().filter(|l| !l.is_empty()) {
                    self.emit_module_wiring(module, node, body, row);
                }
            }
            self.stack.pop();
        }
        true
    }

    /// emitLocals (terraform.ts): each attribute of a `locals` block is a constant.
    fn emit_locals(&mut self, body: Node<'t>) {
        for attr in named_children(body) {
            if attr.kind() != "attribute" {
                continue;
            }
            let Some(name) = self.attr_name(attr) else { continue };
            let qualified = format!("local.{name}");
            let Some(row) = self.create_node("constant", name, attr, &qualified, &qualified, None) else { continue };
            if let Some(expr) = child_of_kind(attr, "expression") {
                self.stack.push(row);
                self.collect_references(expr, row, false);
                self.stack.pop();
            }
        }
    }

    /// emitReferencesInBody (terraform.ts): every expression under `body`, breadth first.
    fn emit_references_in_body(&mut self, body: Node<'t>, from: u32, suppress_scoped: bool, skip_top: &[&str]) {
        let mut queue = VecDeque::new();
        for child in named_children(body) {
            if !skip_top.is_empty() && child.kind() == "attribute" {
                if let Some(name) = self.attr_name(child) {
                    if skip_top.contains(&name) {
                        continue;
                    }
                }
            }
            queue.push_back(child);
        }
        while let Some(n) = queue.pop_front() {
            if n.kind() == "expression" {
                self.collect_references(n, from, suppress_scoped);
                continue;
            }
            queue.extend(named_children(n));
        }
    }

    /// collectReferences (terraform.ts): a reference for each variable_expr chain, breadth first.
    fn collect_references(&mut self, expr: Node<'t>, from: u32, suppress_scoped: bool) {
        let mut queue = VecDeque::from([expr]);
        while let Some(n) = queue.pop_front() {
            if n.kind() == "variable_expr" {
                for name in self.reference_names(n) {
                    if suppress_scoped && name.contains(':') {
                        continue;
                    }
                    self.push_ref(from, &name, "references", n);
                }
            }
            queue.extend(named_children(n));
        }
    }

    /// emitRefFromVariableExpr (terraform.ts): the head and its `.attr` chain.
    fn reference_names(&self, var_expr: Node<'t>) -> Vec<String> {
        let Some(id) = child_of_kind(var_expr, "identifier") else { return Vec::new() };
        let head = self.text(id);
        if BUILTIN_HEADS.contains(&head) {
            return Vec::new();
        }
        let mut attrs = Vec::new();
        let mut cursor = var_expr.next_named_sibling();
        while let Some(c) = cursor {
            match c.kind() {
                "get_attr" => {
                    let Some(attr) = child_of_kind(c, "identifier") else { break };
                    attrs.push(self.text(attr));
                    cursor = c.next_named_sibling();
                }
                "index" | "new_index" | "legacy_index" | "splat" | "attr_splat" | "full_splat" => cursor = c.next_named_sibling(),
                _ => break,
            }
        }
        qualify_reference(head, &attrs)
    }

    /// readStringAttr (terraform.ts): the string value of the first `name = "…"` attribute.
    fn read_string_attr(&self, body: Node<'t>, name: &str) -> Option<String> {
        let attr = named_children(body).into_iter().find(|a| a.kind() == "attribute" && self.attr_name(*a) == Some(name))?;
        let lit = child_of_kind(attr, "expression").and_then(find_string_lit)?;
        Some(self.string_lit_value(lit))
    }

    /// emitProviderSelectionRef (terraform.ts): `provider = aws.east` → `provider.aws.east`.
    fn emit_provider_selection_ref(&mut self, body: Node<'t>, from: u32) {
        let Some(attr) = named_children(body).into_iter().find(|a| a.kind() == "attribute" && self.attr_name(*a) == Some("provider")) else {
            return;
        };
        let Some(expr) = child_of_kind(attr, "expression") else { return };
        if let Some(selection) = self.provider_selection(expr) {
            self.push_ref(from, &format!("provider.{selection}"), "references", attr);
        }
    }

    /// emitModuleProvidersRefs (terraform.ts): each value of a module's `providers` map.
    fn emit_module_providers_refs(&mut self, body: Node<'t>, from: u32) {
        let Some(attr) = named_children(body).into_iter().find(|a| a.kind() == "attribute" && self.attr_name(*a) == Some("providers")) else {
            return;
        };
        let mut queue = VecDeque::from([attr]);
        while let Some(n) = queue.pop_front() {
            if n.kind() == "object_elem" {
                if let Some(selection) = n.child_by_field_name("val").and_then(|v| self.provider_selection(v)) {
                    self.push_ref(from, &format!("provider.{selection}"), "references", n);
                }
                continue;
            }
            queue.extend(named_children(n));
        }
    }

    /// providerSelectionFromExpr (terraform.ts): `aws`, `aws.east`; None for anything dynamic.
    fn provider_selection(&self, expr: Node<'t>) -> Option<String> {
        let mut queue = VecDeque::from([expr]);
        while let Some(n) = queue.pop_front() {
            if n.kind() == "variable_expr" {
                let head = self.text(child_of_kind(n, "identifier")?);
                let next = n.next_named_sibling();
                if let Some(next) = next.filter(|x| x.kind() == "get_attr") {
                    let attr = child_of_kind(next, "identifier")?;
                    if next.next_named_sibling().is_some() {
                        return None;
                    }
                    return Some(format!("{head}.{}", self.text(attr)));
                }
                return if next.is_some() { None } else { Some(head.to_string()) };
            }
            if matches!(n.kind(), "function_call" | "conditional" | "for_expr") {
                return None;
            }
            queue.extend(named_children(n));
        }
        None
    }

    /// emitModuleWiring (terraform.ts): the `:`-scoped refs the Terraform resolver bridges.
    fn emit_module_wiring(&mut self, module: &str, block: Node<'t>, body: Node<'t>, from: u32) {
        for attr in named_children(body) {
            if attr.kind() != "attribute" {
                continue;
            }
            let Some(name) = self.attr_name(attr) else { continue };
            if name == "source" {
                let source = child_of_kind(attr, "expression")
                    .and_then(find_string_lit)
                    .map(|lit| self.string_lit_value(lit))
                    .unwrap_or_default();
                if source.starts_with("./") || source.starts_with("../") {
                    self.push_ref(from, &format!("module.{module}:file"), "imports", block);
                }
                continue;
            }
            if MODULE_META_ARGS.contains(&name) {
                continue;
            }
            self.push_ref(from, &format!("module.{module}:var.{name}"), "references", attr);
        }
    }
}
