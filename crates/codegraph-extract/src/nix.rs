//! Nix extraction — a port of languages/nix.ts over the generic walk of
//! `TreeSitterExtractor` (src/extraction/tree-sitter.ts).
//!
//! nix.ts sets every type list empty and does all its work in `visitNode`, so
//! the walk is: file node, the hook, else recursion over named children. No
//! call extraction, value refs or fn-ref capture run for Nix (language gates).
//! The grammar is nix-community/tree-sitter-nix @ 3d0173d regenerated with
//! tree-sitter-cli 0.25.10 — the same build the vendored wasm came from.

use crate::buffers::{
    build_meta, edge_kind_index, node_kind_index, Arena, BoolFlags, EdgeRow, EmitOut, NodeRow,
    RefRow, Tables, FLAG_IS_EXPORTED, NONE, NONE_STR,
};
use crate::ids;
use crate::textutil as util;
use crate::ts::Node;
use codegraph_types::jscompat::js_trim;

struct Scope {
    row: u32,
    kind: &'static str,
    name: String,
}

struct Walker<'t> {
    src: &'t str,
    file_path: &'t str,
    line_starts: Vec<usize>,
    arena: Arena,
    node_id_allocator: ids::NodeIdAllocator,
    tables: Tables,
    stack: Vec<Scope>,
}

pub fn extract(file_path: &str, source: &str) -> Result<EmitOut, String> {
    let grammar = crate::langs::grammar_for("nix").ok_or("no nix grammar")?;
    let t0 = std::time::Instant::now();
    let tree = crate::ts::parse(&grammar, "nix", source)?;
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
    w.stack.push(Scope { row: 0, kind: "file", name: base_name.to_string() });
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

fn last_named_child<'t>(node: Node<'t>) -> Option<Node<'t>> {
    node.named_child_count().checked_sub(1).and_then(|i| node.named_child(i))
}

/// unwrapVariableExpression (nix.ts).
fn unwrap_variable_expression(node: Node) -> Node {
    if node.kind() != "variable_expression" {
        return node;
    }
    node.named_child(0).unwrap_or(node)
}

/// isStaticProjectPath (nix.ts): `./` or `../` with none of `\s{}()[];"'<>$`.
fn is_static_project_path(value: &str) -> bool {
    (value.starts_with("./") || value.starts_with("../"))
        && !value.chars().any(|c| {
            c.is_whitespace() || c == '\u{feff}' || matches!(c, '{' | '}' | '(' | ')' | '[' | ']' | ';' | '"' | '\'' | '<' | '>' | '$')
        })
}

/// isCallPackageName (nix.ts): the nixpkgs auto-wiring idiom.
fn is_call_package_name(name: &str) -> bool {
    name == "callPackage" || name == "callPackages" || name.ends_with(".callPackage") || name.ends_with(".callPackages")
}

/// formatFunctionSignature (nix.ts).
fn format_function_signature(params: &[String]) -> String {
    match params {
        [] => "()".into(),
        [param] => {
            if param.is_empty() {
                "()".into()
            } else if param.starts_with('(') || param.contains('{') || param.contains('@') {
                param.clone()
            } else {
                format!("({param})")
            }
        }
        _ => params.join(" : "),
    }
}

/// isReturnedAttrsetMember (nix.ts): an attrset ancestor reached without
/// crossing a let binding list, another binding, or formals.
fn is_returned_attrset_member(node: Node) -> bool {
    let mut current = node;
    let mut seen = false;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            "let_expression" => {
                let body = parent.child_by_field_name("body").or_else(|| parent.child_by_field_name("expression"));
                if body != Some(current) {
                    return false;
                }
            }
            "binding" if current != node => return false,
            "formal_parameters" | "formals" => return false,
            "attrset" | "rec_attrset" | "attrset_expression" | "rec_attrset_expression" => seen = true,
            _ => {}
        }
        current = parent;
    }
    seen
}

/// getFirstApplyArgument (nix.ts): innermost argument of a curried apply chain.
fn first_apply_argument(node: Node) -> Option<Node> {
    let mut inner = node;
    loop {
        match inner.child_by_field_name("function").or_else(|| inner.named_child(0)) {
            Some(f) if f.kind() == "apply_expression" => inner = f,
            _ => break,
        }
    }
    inner.child_by_field_name("argument").or_else(|| inner.named_child(1))
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
    fn top_row(&self) -> u32 {
        self.stack.last().map(|s| s.row).unwrap_or(0)
    }

    fn push_ref(&mut self, name: &str, kind: &str, node: Node) {
        let from_idx = self.top_row();
        let reference_name = self.arena.put(name);
        self.tables.push_ref(&RefRow {
            from_idx,
            kind: edge_kind_index(kind).unwrap(),
            line: self.line_of(node),
            column: self.col_of(node),
            reference_name,
            candidates: NONE_STR,
            from_id_str: NONE_STR,
        });
    }

    /// createNode (tree-sitter.ts) with nix.ts's extras: a signature and isExported.
    fn create_node(&mut self, kind: &'static str, name: &str, node: Node<'t>, signature: Option<&str>, exported: Option<bool>) -> Option<u32> {
        if name.is_empty() {
            return None;
        }
        let start_line = self.line_of(node);
        let column = self.col_of(node);
        let id = self.node_id_allocator.generate(self.file_path, kind, name, start_line, column);
        let mut qualified: Vec<&str> = self.stack.iter().filter(|s| s.kind != "file").map(|s| s.name.as_str()).collect();
        qualified.push(name);
        let qualified = qualified.join("::");
        let mut flags = BoolFlags::default();
        if let Some(exported) = exported {
            flags.set(FLAG_IS_EXPORTED, exported);
        }
        let name_ref = self.arena.put(name);
        let qn_ref = self.arena.put(&qualified);
        let id_ref = self.arena.put(&id);
        let sig_ref = signature.map_or(NONE_STR, |s| self.arena.put(s));
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
        let parent_row = self.top_row();
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

    /// The visitNode hook (nix.ts). Returns true when consumed.
    fn hook(&mut self, node: Node<'t>) -> bool {
        match node.kind() {
            "binding" => self.hook_binding(node),
            "function_expression" => {
                if let Some(body) = last_named_child(node) {
                    self.visit(body);
                }
                true
            }
            "inherit" | "inherit_from" => {
                let attrs = named_children(node).into_iter().find(|c| c.kind() == "inherited_attrs");
                if let Some(attrs) = attrs {
                    for child in named_children(attrs) {
                        let name = js_trim(self.text(child));
                        if !name.is_empty() {
                            self.create_node("variable", name, child, None, Some(is_returned_attrset_member(child)));
                        }
                    }
                }
                for child in named_children(node) {
                    if child.kind() != "inherited_attrs" {
                        self.visit(child);
                    }
                }
                true
            }
            "apply_expression" => self.hook_apply(node),
            _ => false,
        }
    }

    fn hook_binding(&mut self, node: Node<'t>) -> bool {
        let Some(attrpath) = node.child_by_field_name("attrpath").or_else(|| node.named_child(0)) else {
            return false;
        };
        let name = js_trim(self.text(attrpath));
        if name.is_empty() {
            return false;
        }
        let value = node
            .child_by_field_name("expression")
            .or_else(|| node.child_by_field_name("value"))
            .or_else(|| node.named_child(1));
        let Some(value) = value else {
            return false;
        };
        let exported = is_returned_attrset_member(node);
        if value.kind() == "function_expression" {
            let (params, body) = self.curried_params_and_body(value);
            let signature = format_function_signature(&params);
            if let Some(row) = self.create_node("function", name, node, Some(&signature), Some(exported)) {
                self.stack.push(Scope { row, kind: "function", name: name.to_string() });
                if let Some(body) = body {
                    self.visit(body);
                }
                self.stack.pop();
            }
        } else {
            let init = util::init_signature(self.text(value));
            let signature = (init != "= ").then_some(init.as_str());
            self.create_node("variable", name, node, signature, Some(exported));
            // `imports = [ ./hardware.nix ]` / `modules = [ … ]`: literal path entries only.
            let last = name.rsplit('.').next().unwrap_or(name);
            if (last == "imports" || last == "modules") && value.kind() == "list_expression" {
                for child in named_children(value) {
                    if child.kind() == "path_expression" {
                        let path = js_trim(self.text(child));
                        if is_static_project_path(path) {
                            self.emit_file_import(path, child);
                        }
                    }
                }
            }
            self.visit(value);
        }
        true
    }

    fn hook_apply(&mut self, node: Node<'t>) -> bool {
        let direct = self.direct_callee_name(node);
        let is_direct_import = matches!(direct, Some("import" | "builtins.import"));
        let is_callee_of_parent = node
            .parent()
            .filter(|p| p.kind() == "apply_expression")
            .and_then(|p| p.child_by_field_name("function").or_else(|| p.named_child(0)))
            == Some(node);
        if !(is_callee_of_parent && !is_direct_import) {
            if is_direct_import {
                let arg = node.child_by_field_name("argument").or_else(|| node.named_child(1));
                if let Some(path) = arg.and_then(|a| self.static_import_path(a)) {
                    self.emit_file_import(&path, node);
                }
            } else {
                let callee = self.callee_name(node);
                if let Some(callee) = callee {
                    if callee != "import" && callee != "builtins.import" && !callee.is_empty() && !self.stack.is_empty() {
                        self.push_ref(callee, "calls", node);
                    }
                    if is_call_package_name(callee) {
                        if let Some(path) = first_apply_argument(node).and_then(|a| self.static_import_path(a)) {
                            self.emit_file_import(&path, node);
                        }
                    }
                }
            }
        }
        for child in named_children(node) {
            self.visit(child);
        }
        true
    }

    /// emitFileImport (nix.ts): import node + `imports` ref from the enclosing scope.
    fn emit_file_import(&mut self, path: &str, anchor: Node<'t>) {
        let (signature, _) = util::slice_utf16(js_trim(self.text(anchor)), 100);
        if self.create_node("import", path, anchor, Some(&signature), None).is_some() && !self.stack.is_empty() {
            self.push_ref(path, "imports", anchor);
        }
    }

    /// getCalleeName (nix.ts): the head of a curried apply chain, when it is an
    /// identifier or a select expression.
    fn callee_name(&self, node: Node<'t>) -> Option<&'t str> {
        let mut current = node;
        while current.kind() == "apply_expression" {
            match current.child_by_field_name("function").or_else(|| current.named_child(0)) {
                Some(f) => current = f,
                None => break,
            }
        }
        let current = unwrap_variable_expression(current);
        matches!(current.kind(), "identifier" | "select_expression").then(|| js_trim(self.text(current)))
    }

    /// getDirectCalleeName (nix.ts).
    fn direct_callee_name(&self, node: Node<'t>) -> Option<&'t str> {
        let f = node.child_by_field_name("function").or_else(|| node.named_child(0))?;
        Some(js_trim(self.text(unwrap_variable_expression(f))))
    }

    /// getStaticImportPath (nix.ts): a literal `./`/`../` path, quotes stripped.
    fn static_import_path(&self, arg: Node<'t>) -> Option<String> {
        let mut current = arg;
        while current.kind() == "parenthesized_expression" {
            match current.named_child(0) {
                Some(inner) => current = inner,
                None => break,
            }
        }
        let mut text = js_trim(self.text(current));
        let quoted = (text.starts_with('"') && text.ends_with('"')) || (text.starts_with('\'') && text.ends_with('\''));
        if quoted && util::utf16_len(text) >= 2 {
            text = &text[1..text.len() - 1];
        }
        is_static_project_path(text).then(|| text.to_string())
    }

    /// getCurriedParamsAndBody (nix.ts): `a: b: body` → (["a", "b"], body).
    fn curried_params_and_body(&self, node: Node<'t>) -> (Vec<String>, Option<Node<'t>>) {
        let mut params = Vec::new();
        let mut current = node;
        while current.kind() == "function_expression" && current.named_child_count() > 0 {
            let Some(body) = last_named_child(current) else {
                break;
            };
            let part = js_trim(&self.src[current.start_byte()..body.start_byte()]);
            let param = match part.strip_suffix(':') {
                Some(p) => js_trim(p),
                None => part,
            };
            if !param.is_empty() {
                params.push(param.to_string());
            }
            if body.kind() == "function_expression" {
                current = body;
            } else {
                return (params, Some(body));
            }
        }
        let body = if current.named_child_count() > 0 { last_named_child(current) } else { None };
        (params, body)
    }
}
