//! Solidity extraction — a port of languages/solidity.ts over the generic walk
//! of `TreeSitterExtractor` (src/extraction/tree-sitter.ts).
//!
//! The paths here are the ones that run for Solidity: the `visitNode` hook
//! (contracts, interfaces, libraries, structs, enums, events, errors), then the
//! dispatch ladder — functions/methods (extractFunction / extractMethod),
//! fields, file-scope variables, type aliases, imports, calls and `new`.
//! Solidity has no value refs, function-as-value capture, type annotations or
//! static-member refs (language gates). Modifier invocations in a function
//! header are `calls` from the function (extractDecoratorsFor).
//! The grammar is JoranHonig/tree-sitter-solidity @ b239a95, src/ as published
//! — the commit tree-sitter-wasms 0.1.13 built its wasm from.

use crate::buffers::{
    build_meta, edge_kind_index, node_kind_index, Arena, BoolFlags, EdgeRow, EmitOut, NodeRow,
    RefRow, Tables, FLAG_IS_EXPORTED, FLAG_IS_STATIC, NONE, NONE_STR,
};
use crate::docstring::preceding_docstring;
use crate::ids;
use crate::textutil as util;
use crate::ts::Node;
use codegraph_types::jscompat::{is_js_space, js_trim};

struct Scope {
    row: u32,
    kind: &'static str,
    name: String,
}

#[derive(Default)]
struct Extra {
    docstring: Option<String>,
    signature: Option<String>,
    visibility: u8,
    exported: Option<bool>,
    is_static: Option<bool>,
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
    let grammar = crate::langs::grammar_for("solidity").ok_or("no solidity grammar")?;
    let t0 = std::time::Instant::now();
    let tree = crate::ts::parse(&grammar, "solidity", source)?;
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

/// `node.fieldNameForNamedChild(i)` for every named child, in order.
fn named_children_with_fields<'t>(node: Node<'t>) -> Vec<(Node<'t>, Option<&'static str>)> {
    (0..node.child_count())
        .filter(|&i| node.is_named_child(i))
        .filter_map(|i| node.child(i).map(|c| (c, node.field_name_for_child(i))))
        .collect()
}

/// `node.descendantsOfType(kind)` — the node itself and its descendants, in document order.
fn descendants_of_type<'t>(node: Node<'t>, kind: &str, out: &mut Vec<Node<'t>>) {
    if node.kind() == kind {
        out.push(node);
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            descendants_of_type(child, kind, out);
        }
    }
}

fn is_function_type(kind: &str) -> bool {
    matches!(kind, "function_definition" | "modifier_definition")
}

fn is_method_type(kind: &str) -> bool {
    matches!(kind, "function_definition" | "modifier_definition" | "constructor_definition" | "fallback_receive_definition")
}

fn is_call_type(kind: &str) -> bool {
    matches!(kind, "call_expression" | "emit_statement" | "revert_statement" | "modifier_invocation")
}

/// INSTANTIATION_KINDS (tree-sitter.ts) — Solidity's grammar has `new_expression`.
fn is_instantiation_kind(kind: &str) -> bool {
    matches!(
        kind,
        "new_expression" | "object_creation_expression" | "instance_creation_expression" | "composite_literal" | "struct_expression" | "instance_expression"
    )
}

/// LITERAL_RECEIVER_TYPES (tree-sitter.ts).
fn is_literal_receiver(kind: &str) -> bool {
    matches!(
        kind,
        "string" | "string_literal" | "interpreted_string_literal" | "raw_string_literal" | "template_string"
            | "concatenated_string" | "formatted_string" | "f_string" | "line_string_literal" | "string_content"
            | "heredoc_body" | "number" | "number_literal" | "integer" | "integer_literal" | "float" | "float_literal"
            | "int_literal" | "decimal_integer_literal" | "real_literal" | "char_literal" | "character_literal"
            | "rune_literal" | "regex" | "regex_literal" | "true" | "false" | "boolean_literal" | "bool_literal"
            | "none" | "null" | "nil" | "null_literal" | "undefined" | "list" | "list_literal" | "array"
            | "array_literal" | "array_creation_expression" | "dictionary" | "dict_literal" | "object" | "tuple" | "set"
    )
}

fn is_class_like(kind: &str) -> bool {
    matches!(kind, "class" | "struct" | "union" | "interface" | "trait" | "enum" | "module" | "enum_member")
}

/// `/^\(\s*\*?\s*([A-Za-z_][\w.]*)\s*\)$/` (extractCall's parenthesized-callee
/// normalization): the inner name, or None.
fn parenthesized_callee(name: &str) -> Option<&str> {
    let inner = name.strip_prefix('(')?.strip_suffix(')')?;
    let inner = inner.trim_start_matches(is_js_space);
    let inner = inner.strip_prefix('*').unwrap_or(inner).trim_start_matches(is_js_space);
    let inner = inner.trim_end_matches(is_js_space);
    let mut chars = inner.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.').then_some(inner)
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
    fn inside_class_like(&self) -> bool {
        self.stack.last().is_some_and(|s| is_class_like(s.kind))
    }

    fn push_ref_from(&mut self, from_idx: u32, name: &str, kind: &str, node: Node) {
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

    fn push_ref(&mut self, name: &str, kind: &str, node: Node) {
        let from = self.top_row();
        self.push_ref_from(from, name, kind, node);
    }

    /// createNode (tree-sitter.ts).
    fn create_node(&mut self, kind: &'static str, name: &str, node: Node<'t>, extra: Extra) -> Option<u32> {
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
        if let Some(exported) = extra.exported {
            flags.set(FLAG_IS_EXPORTED, exported);
        }
        if let Some(is_static) = extra.is_static {
            flags.set(FLAG_IS_STATIC, is_static);
        }
        let name_ref = self.arena.put(name);
        let qn_ref = self.arena.put(&qualified);
        let id_ref = self.arena.put(&id);
        let doc_ref = extra.docstring.as_deref().map_or(NONE_STR, |s| self.arena.put(s));
        let sig_ref = extra.signature.as_deref().map_or(NONE_STR, |s| self.arena.put(s));
        let row = self.tables.push_node(&NodeRow {
            kind: node_kind_index(kind).unwrap(),
            visibility: extra.visibility,
            flags,
            start_line,
            end_line: node.end_position().row as u32 + 1,
            start_column: column,
            end_column: self.end_col_of(node),
            name: name_ref,
            qualified_name: qn_ref,
            id: id_ref,
            docstring: doc_ref,
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

    fn with_scope(&mut self, row: u32, kind: &'static str, name: &str, f: impl FnOnce(&mut Self)) {
        self.stack.push(Scope { row, kind, name: name.to_string() });
        f(self);
        self.stack.pop();
    }

    /// extractName (tree-sitter.ts) with solidity.ts's resolveName.
    fn extract_name(&self, node: Node<'t>) -> String {
        match node.kind() {
            "constructor_definition" => return "constructor".into(),
            "fallback_receive_definition" => {
                // fallbackReceiveName: the first child (named or not) whose text is the keyword.
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        let t = self.text(child);
                        if t == "fallback" || t == "receive" {
                            return t.into();
                        }
                    }
                }
                return "fallback".into();
            }
            _ => {}
        }
        if let Some(name) = node.child_by_field_name("name") {
            return self.text(name).into();
        }
        for child in named_children(node) {
            if matches!(child.kind(), "identifier" | "type_identifier" | "simple_identifier" | "constant") {
                return self.text(child).into();
            }
        }
        "<anonymous>".into()
    }

    /// getSignature (solidity.ts): `(params) visibility mutability returns`.
    fn signature(&self, node: Node<'t>) -> String {
        let mut params = Vec::new();
        let (mut return_type, mut visibility, mut mutability) = (None, None, None);
        for (child, field) in named_children_with_fields(node) {
            if child.kind() == "parameter" && field != Some("return_type") {
                params.push(self.text(child));
            } else if child.kind() == "return_type_definition" || field == Some("return_type") {
                return_type = Some(child);
            } else if child.kind() == "visibility" {
                visibility = Some(child);
            } else if child.kind() == "state_mutability" {
                mutability = Some(child);
            }
        }
        let mut parts = vec![format!("({})", params.join(", "))];
        for part in [visibility, mutability, return_type].into_iter().flatten() {
            parts.push(self.text(part).to_string());
        }
        parts.join(" ")
    }

    /// getVisibility (solidity.ts): 1 public (also `external`), 2 private, 4 internal.
    fn visibility(&self, node: Node<'t>) -> u8 {
        for child in named_children(node) {
            if child.kind() != "visibility" {
                continue;
            }
            match js_trim(self.text(child)) {
                "public" | "external" => return 1,
                "private" => return 2,
                "internal" => return 4,
                _ => {}
            }
        }
        0
    }

    fn visit(&mut self, node: Node<'t>) {
        stack_guard!();
        if self.hook(node) {
            return;
        }
        let kind = node.kind();
        let mut skip_children = false;
        if is_function_type(kind) {
            if self.inside_class_like() && is_method_type(kind) {
                self.extract_method(node);
            } else {
                self.extract_function(node);
            }
            skip_children = true;
        } else if matches!(kind, "contract_declaration" | "library_declaration") {
            // Reached only when the hook declined: a contract with no `name:`.
            self.extract_class(node);
            skip_children = true;
        } else if is_method_type(kind) {
            self.extract_method(node);
            skip_children = true;
        } else if kind == "interface_declaration" {
            self.extract_interface(node);
            skip_children = true;
        } else if kind == "user_defined_type_definition" {
            self.extract_type_alias(node);
        } else if matches!(kind, "state_variable_declaration" | "struct_member") && self.inside_class_like() {
            self.extract_field(node);
            skip_children = true;
        } else if matches!(kind, "state_variable_declaration" | "constant_variable_declaration") && !self.inside_class_like() {
            self.extract_variable(node);
            skip_children = true;
        } else if kind == "import_directive" {
            self.extract_import(node);
        } else if is_call_type(kind) {
            self.extract_call(node);
        } else if is_instantiation_kind(kind) {
            self.extract_instantiation(node);
        }
        if !skip_children {
            for child in named_children(node) {
                self.visit(child);
            }
        }
    }

    /// The visitNode hook (solidity.ts). Returns true when consumed.
    fn hook(&mut self, node: Node<'t>) -> bool {
        match node.kind() {
            "contract_declaration" | "library_declaration" | "interface_declaration" => {
                let ancestors = self.inheritance_ancestors(node);
                let Some(name_node) = node.child_by_field_name("name") else {
                    return false;
                };
                let body = node.child_by_field_name("body");
                let name = self.text(name_node);
                let kind = if node.kind() == "interface_declaration" { "interface" } else { "class" };
                let Some(row) = self.create_node(kind, name, node, Extra::default()) else {
                    return true;
                };
                for ancestor in &ancestors {
                    self.push_ref_from(row, ancestor, "extends", node);
                }
                self.with_scope(row, kind, name, |w| {
                    if let Some(body) = body {
                        for child in named_children(body) {
                            w.visit(child);
                        }
                    }
                });
                true
            }
            "struct_declaration" | "enum_declaration" => {
                let Some(name_node) = node.child_by_field_name("name") else {
                    return true;
                };
                let name = self.text(name_node);
                let kind = if node.kind() == "struct_declaration" { "struct" } else { "enum" };
                let Some(row) = self.create_node(kind, name, node, Extra::default()) else {
                    return true;
                };
                // solidity.ts skips `child === nameNode`, which never holds for the web
                // binding's fresh wrappers; visiting the name identifier does nothing.
                self.with_scope(row, kind, name, |w| {
                    for child in named_children(node) {
                        w.visit(child);
                    }
                });
                true
            }
            "enum_value" => {
                let name = self.text(node);
                self.create_node("enum_member", name, node, Extra::default());
                true
            }
            "event_definition" | "error_declaration" => {
                let Some(name_node) = node.child_by_field_name("name") else {
                    return true;
                };
                let name = self.text(name_node);
                let (signature, _) = util::slice_utf16(js_trim(self.text(node)), 200);
                self.create_node("field", name, node, Extra { signature: Some(signature), ..Extra::default() });
                true
            }
            _ => false,
        }
    }

    /// getInheritanceAncestors (solidity.ts): the last identifier of each `is` ancestor.
    fn inheritance_ancestors(&self, node: Node<'t>) -> Vec<String> {
        let mut ancestors = Vec::new();
        for child in named_children(node) {
            if child.kind() != "inheritance_specifier" {
                continue;
            }
            let Some(ancestor) = child.child_by_field_name("ancestor") else {
                continue;
            };
            let mut ids = Vec::new();
            descendants_of_type(ancestor, "identifier", &mut ids);
            if let Some(last) = ids.last() {
                ancestors.push(self.text(*last).to_string());
            }
        }
        ancestors
    }

    /// extractClass (tree-sitter.ts) for a contract or library the hook declined.
    fn extract_class(&mut self, node: Node<'t>) {
        let name = self.extract_name(node);
        let extra = Extra { docstring: preceding_docstring(node, self.src), visibility: self.visibility(node), ..Extra::default() };
        let Some(row) = self.create_node("class", &name, node, extra) else {
            return;
        };
        self.extract_decorators_for(node, row);
        let body = node.child_by_field_name("body").unwrap_or(node);
        self.with_scope(row, "class", &name, |w| {
            for child in named_children(body) {
                w.visit(child);
            }
        });
    }

    /// extractInterface (tree-sitter.ts) for an interface the hook declined.
    fn extract_interface(&mut self, node: Node<'t>) {
        let name = self.extract_name(node);
        let extra = Extra { docstring: preceding_docstring(node, self.src), ..Extra::default() };
        let Some(row) = self.create_node("interface", &name, node, extra) else {
            return;
        };
        let body = node.child_by_field_name("body").unwrap_or(node);
        self.with_scope(row, "interface", &name, |w| {
            for child in named_children(body) {
                w.visit(child);
            }
        });
    }

    /// extractFunction (tree-sitter.ts).
    fn extract_function(&mut self, node: Node<'t>) {
        let name = self.extract_name(node);
        let body = node.child_by_field_name("body");
        if name == "<anonymous>" {
            if let Some(body) = body {
                self.visit_function_body(body);
            }
            return;
        }
        let extra = Extra {
            docstring: preceding_docstring(node, self.src),
            signature: Some(self.signature(node)),
            visibility: self.visibility(node),
            ..Extra::default()
        };
        let Some(row) = self.create_node("function", &name, node, extra) else {
            return;
        };
        self.extract_decorators_for(node, row);
        self.with_scope(row, "function", &name, |w| {
            if let Some(body) = body {
                w.visit_function_body(body);
            }
        });
    }

    /// extractMethod (tree-sitter.ts).
    fn extract_method(&mut self, node: Node<'t>) {
        if !self.inside_class_like() {
            self.extract_function(node);
            return;
        }
        let name = self.extract_name(node);
        let extra = Extra {
            docstring: preceding_docstring(node, self.src),
            signature: Some(self.signature(node)),
            visibility: self.visibility(node),
            ..Extra::default()
        };
        let Some(row) = self.create_node("method", &name, node, extra) else {
            return;
        };
        self.extract_decorators_for(node, row);
        let body = node.child_by_field_name("body");
        self.with_scope(row, "method", &name, |w| {
            if let Some(body) = body {
                w.visit_function_body(body);
            }
        });
    }

    /// extractDecoratorsFor (tree-sitter.ts): a header `modifier_invocation`
    /// (`onlyOwner`, a base constructor `ERC20("T", "TOK")`) is a call from the
    /// decorated symbol. Solidity has no decorator/annotation nodes, so the
    /// other shapes and the preceding-sibling scan emit nothing.
    fn extract_decorators_for(&mut self, decl: Node<'t>, decorated_row: u32) {
        for child in named_children(decl) {
            if child.kind() != "modifier_invocation" {
                continue;
            }
            if let Some(target) = child.named_child(0).filter(|t| t.kind() == "identifier") {
                let name = self.text(target);
                if !name.is_empty() {
                    self.push_ref_from(decorated_row, name, "calls", child);
                }
            }
        }
    }

    /// visitFunctionBody (tree-sitter.ts): calls and `new`, recursing through
    /// everything. Solidity bodies hold statements only, so the nested
    /// function / class branches never fire.
    fn visit_function_body(&mut self, node: Node<'t>) {
        stack_guard!();
        let kind = node.kind();
        if is_call_type(kind) {
            self.extract_call(node);
        } else if is_instantiation_kind(kind) {
            self.extract_instantiation(node);
        }
        for child in named_children(node) {
            self.visit_function_body(child);
        }
    }

    /// extractField (tree-sitter.ts): Solidity has no declarators, so the
    /// fallback names the field by its `name:` (or first identifier).
    fn extract_field(&mut self, node: Node<'t>) {
        let docstring = preceding_docstring(node, self.src);
        let visibility = self.visibility(node);
        let name_node = node
            .child_by_field_name("name")
            .or_else(|| named_children(node).into_iter().find(|c| c.kind() == "identifier"));
        if let Some(name_node) = name_node {
            let name = self.text(name_node);
            self.create_node("field", name, node, Extra { docstring, visibility, is_static: Some(false), ..Extra::default() });
        }
    }

    /// extractVariable (tree-sitter.ts), generic fallback: each identifier child.
    fn extract_variable(&mut self, node: Node<'t>) {
        let kind = if node.kind() == "constant_variable_declaration" { "constant" } else { "variable" };
        let docstring = preceding_docstring(node, self.src);
        for child in named_children(node) {
            if child.kind() == "identifier" {
                let name = self.text(child);
                if !name.is_empty() && name != "<anonymous>" {
                    let extra = Extra { docstring: docstring.clone(), exported: Some(false), ..Extra::default() };
                    self.create_node(kind, name, child, extra);
                }
            }
        }
    }

    /// extractTypeAlias (tree-sitter.ts): `type Price is uint256;`. The children are walked after.
    fn extract_type_alias(&mut self, node: Node<'t>) {
        let name = self.extract_name(node);
        if name == "<anonymous>" {
            return;
        }
        let docstring = preceding_docstring(node, self.src);
        self.create_node("type_alias", &name, node, Extra { docstring, ..Extra::default() });
    }

    /// extractImport (tree-sitter.ts) with solidity.ts's hook: the source path is the module.
    fn extract_import(&mut self, node: Node<'t>) {
        let Some(source_field) = node.child_by_field_name("source") else {
            return;
        };
        let mut literals = Vec::new();
        descendants_of_type(source_field, "string_literal", &mut literals);
        let raw = self.text(literals.first().copied().unwrap_or(source_field));
        // `.replace(/^["']|["']$/g, '').trim()`
        let mut module = raw;
        if module.starts_with(['"', '\'']) {
            module = &module[1..];
        }
        if module.ends_with(['"', '\'']) {
            module = &module[..module.len() - 1];
        }
        let module = js_trim(module);
        if module.is_empty() {
            return;
        }
        let signature = js_trim(self.text(node)).to_string();
        self.create_node("import", module, node, Extra { signature: Some(signature), ..Extra::default() });
        if !self.stack.is_empty() {
            self.push_ref(module, "imports", node);
        }
    }

    /// extractCall (tree-sitter.ts), the generic path Solidity takes: the
    /// `function:` field (emit/revert/modifier: the first named child), a member
    /// callee as `receiver.method` for a plain identifier receiver.
    fn extract_call(&mut self, node: Node<'t>) {
        if self.stack.is_empty() {
            return;
        }
        let Some(func) = node.child_by_field_name("function").or_else(|| node.named_child(0)) else {
            return;
        };
        let mut callee = String::new();
        if matches!(func.kind(), "member_expression" | "attribute" | "selector_expression" | "navigation_expression" | "field_expression") {
            let property = func
                .child_by_field_name("property")
                .or_else(|| func.child_by_field_name("field"))
                .or_else(|| func.named_child(1));
            if let Some(property) = property {
                let method = self.text(property);
                let receiver = func
                    .child_by_field_name("object")
                    .or_else(|| func.child_by_field_name("operand"))
                    .or_else(|| func.child_by_field_name("argument"))
                    .or_else(|| func.named_child(0));
                if receiver.is_some_and(|r| is_literal_receiver(r.kind())) {
                    return;
                }
                callee = match receiver {
                    Some(r) if matches!(r.kind(), "identifier" | "simple_identifier" | "field_identifier") => {
                        let name = self.text(r);
                        if matches!(name, "self" | "this" | "cls" | "super") {
                            method.to_string()
                        } else {
                            format!("{name}.{method}")
                        }
                    }
                    _ => method.to_string(),
                };
            }
        } else {
            callee = self.text(func).to_string();
        }
        if let Some(inner) = parenthesized_callee(&callee) {
            callee = inner.to_string();
        }
        if !callee.is_empty() {
            self.push_ref(&callee, "calls", node);
        }
    }

    /// extractInstantiation (tree-sitter.ts): `new Token(…)` → `instantiates Token`.
    fn extract_instantiation(&mut self, node: Node<'t>) {
        if self.stack.is_empty() {
            return;
        }
        let ctor = node
            .child_by_field_name("constructor")
            .or_else(|| node.child_by_field_name("type"))
            .or_else(|| node.child_by_field_name("name"))
            .or_else(|| node.named_child(0));
        let Some(ctor) = ctor else {
            return;
        };
        let mut class_name = self.text(ctor);
        if let Some(lt) = class_name.find('<').filter(|&i| i > 0) {
            class_name = &class_name[..lt];
        }
        let last_dot = class_name.rfind('.');
        let last_colons = class_name.rfind("::");
        let cut = match (last_dot, last_colons) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        if let Some(cut) = cut {
            class_name = &class_name[cut + 1..];
            if class_name.starts_with([':', '.']) {
                class_name = &class_name[1..];
            }
        }
        let class_name = js_trim(class_name);
        if !class_name.is_empty() {
            self.push_ref(class_name, "instantiates", node);
        }
    }
}
