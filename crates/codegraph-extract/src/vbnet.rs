//! VB.NET extraction — a port of languages/vbnet.ts over the generic walk of
//! `TreeSitterExtractor` (src/extraction/tree-sitter.ts), with its vbnet branches: the call
//! extractor's three call shapes (VB writes calls and index reads alike), `New T(…)` parsed
//! as an array creation, the member reads sent with their receiver path (#2305), and
//! `Inherits` / `Implements` statements in a type's body.
//!
//! The grammar has no body wrapper: a type's members and a method's statements are its own
//! children (resolveBody returns the node), so each declaration is walked whole. Events and
//! constructors come from the visitNode hook. The first namespace of a file scopes the whole
//! file (extractFilePackage); others mint no node. VB.NET has no function-as-value, value-ref
//! or type-annotation pass, and its attributes are never decorators.

use crate::buffers::{
    build_meta, edge_kind_index, node_kind_index, Arena, BoolFlags, EdgeRow, EmitOut, NodeRow,
    RefRow, Tables, FLAG_IS_ASYNC, FLAG_IS_EXPORTED, FLAG_IS_STATIC, NONE, NONE_STR,
};
use crate::docstring::preceding_docstring;
use crate::ids;
use crate::textutil as util;
use crate::ts::Node;
use codegraph_types::jscompat::{is_js_space, js_trim, JS_SPACE_CLASS};
use regex::Regex;
use std::sync::OnceLock;

const CALL_TYPES: [&str; 3] = ["invocation_expression", "array_access_expression", "generic_invocation_expression"];
const METHOD_TYPES: [&str; 5] = [
    "method_declaration",
    "constructor_declaration",
    "external_method_declaration",
    "interface_method_declaration",
    "abstract_method_declaration",
];
const PROPERTY_TYPES: [&str; 3] = ["property_declaration", "interface_property_declaration", "abstract_property_declaration"];

/// VB_NON_TYPE_RECEIVERS (tree-sitter.ts): a receiver no declaration of the project types.
fn is_non_type_receiver(head: &str) -> bool {
    const NAMES: [&str; 20] = [
        "Global", "System", "Microsoft", "My", "Boolean", "Byte", "Char", "Date", "Decimal", "Double", "Integer",
        "Long", "Object", "SByte", "Short", "Single", "String", "UInteger", "ULong", "UShort",
    ];
    NAMES.iter().any(|n| n.eq_ignore_ascii_case(head))
}

fn is_me_like(text: &str) -> bool {
    ["me", "mybase", "myclass"].iter().any(|k| k.eq_ignore_ascii_case(text))
}

/// `/\(\s*Of\b[^)]*\)/gi` — VB generics, `List(Of Foo)` → `List`.
fn of_args_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(&format!(r"(?i)\([{JS_SPACE_CLASS}]*Of(?-u:\b)[^)]*\)")).unwrap())
}

/// `/^[A-Za-z_]\w*$/` with JS's ASCII `\w`.
fn is_ascii_ident(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `.replace(/\s+/g, ' ')` with JS's `\s`.
fn collapse_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_ws = false;
    for c in text.chars() {
        if is_js_space(c) {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

fn named_children<'t>(node: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

struct Scope {
    row: u32,
    kind: &'static str,
    name: String,
}

#[derive(Default)]
struct Extra {
    docstring: Option<String>,
    signature: Option<String>,
    visibility: Option<u8>,
    is_exported: Option<bool>,
    is_async: Option<bool>,
    is_static: Option<bool>,
    return_type: Option<String>,
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
    let grammar = crate::langs::grammar_for("vbnet").ok_or("no vbnet grammar")?;
    let t0 = std::time::Instant::now();
    let tree = crate::ts::parse(&grammar, "vbnet", source)?;
    let root = tree.root_node();
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

    // extractFilePackage: the file's first namespace scopes every declaration in it.
    let mut pushed_package = false;
    if let Some(pkg) = named_children(root).into_iter().find(|c| c.kind() == "namespace_declaration") {
        let name = pkg.child_by_field_name("name").map(|n| w.text(n).to_string()).unwrap_or_default();
        if let Some(row) = w.create_node("namespace", &name, pkg, Extra::default()) {
            w.stack.push(Scope { row, kind: "namespace", name });
            pushed_package = true;
        }
    }
    w.visit_node(root);
    if pushed_package {
        w.stack.pop();
    }
    w.stack.pop();

    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let meta = build_meta(&w.tables, w.arena.len(), NONE_STR, duration_ms);
    Ok(EmitOut { meta, nodes: w.tables.nodes, edges: w.tables.edges, refs: w.tables.refs, arena: w.arena.into_vec() })
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

    /// createNode (tree-sitter.ts).
    fn create_node(&mut self, kind: &'static str, name: &str, node: Node<'t>, extra: Extra) -> Option<u32> {
        if name.is_empty() {
            return None;
        }
        let start_line = self.line_of(node);
        let column = self.col_of(node);
        let id = self.node_id_allocator.generate(self.file_path, kind, name, start_line, column);
        let mut parts: Vec<&str> = self.stack.iter().filter(|s| s.kind != "file").map(|s| s.name.as_str()).collect();
        parts.push(name);
        let qualified = parts.join("::");
        let mut flags = BoolFlags::default();
        if let Some(v) = extra.is_exported {
            flags.set(FLAG_IS_EXPORTED, v);
        }
        if let Some(v) = extra.is_async {
            flags.set(FLAG_IS_ASYNC, v);
        }
        if let Some(v) = extra.is_static {
            flags.set(FLAG_IS_STATIC, v);
        }
        let name_ref = self.arena.put(name);
        let qn_ref = self.arena.put(&qualified);
        let id_ref = self.arena.put(&id);
        let doc_ref = self.arena.put_opt(extra.docstring.as_deref());
        let sig_ref = self.arena.put_opt(extra.signature.as_deref());
        let ret_ref = self.arena.put_opt(extra.return_type.as_deref());
        let row = self.tables.push_node(&NodeRow {
            kind: node_kind_index(kind).unwrap(),
            visibility: extra.visibility.unwrap_or(0),
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
            return_type: ret_ref,
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

    fn push_scope(&mut self, row: u32, kind: &'static str, name: &str) {
        self.stack.push(Scope { row, kind, name: name.to_string() });
    }

    /// isInsideClassLikeNode (tree-sitter.ts).
    fn inside_class_like(&self) -> bool {
        self.stack.len() > 0
            && matches!(
                self.stack.last().map(|s| s.kind),
                Some("class" | "struct" | "union" | "interface" | "trait" | "enum" | "module" | "enum_member")
            )
    }

    // --- languages/vbnet.ts hooks -----------------------------------------------------

    /// hasModifier: a `member_modifier` child whose text is the keyword, in any case.
    fn has_modifier(&self, node: Node, keyword: &str) -> bool {
        (0..node.child_count())
            .filter_map(|i| node.child(i))
            .any(|c| c.kind() == "member_modifier" && self.text(c).eq_ignore_ascii_case(keyword))
    }

    /// getVisibility: private / protected / friend (internal), else public.
    fn visibility_of(&self, node: Node) -> u8 {
        if self.has_modifier(node, "private") {
            2
        } else if self.has_modifier(node, "protected") {
            3
        } else if self.has_modifier(node, "friend") {
            4
        } else {
            1
        }
    }

    fn is_static(&self, node: Node) -> bool {
        self.has_modifier(node, "shared")
    }

    fn is_const(&self, node: Node) -> bool {
        self.has_modifier(node, "const") || (self.has_modifier(node, "shared") && self.has_modifier(node, "readonly"))
    }

    /// extractVbnetReturnType: the `as_clause` type, past nullability and `(Of …)`, as its
    /// last dotted segment; none for predefined and array types.
    fn return_type(&self, node: Node) -> Option<String> {
        let as_clause = named_children(node).into_iter().find(|c| c.kind() == "as_clause")?;
        let type_node = as_clause.child_by_field_name("declared_type")?;
        if matches!(type_node.kind(), "predefined_type" | "array_type") {
            return None;
        }
        let t = js_trim(self.text(type_node)).trim_end_matches('?');
        let t = of_args_re().replace_all(t, "");
        let last = js_trim(t.rsplit('.').next().unwrap_or(""));
        is_ascii_ident(last).then(|| last.to_string())
    }

    /// extractName / extractNameRaw (tree-sitter.ts): the `name` field, else the first
    /// identifier-like child.
    fn extract_name(&self, node: Node) -> String {
        if let Some(name) = node.child_by_field_name("name") {
            return self.text(name).to_string();
        }
        named_children(node)
            .into_iter()
            .find(|c| matches!(c.kind(), "identifier" | "type_identifier" | "simple_identifier" | "constant"))
            .map_or_else(|| "<anonymous>".to_string(), |c| self.text(c).to_string())
    }

    fn docstring(&self, node: Node) -> Option<String> {
        preceding_docstring(node, self.src)
    }

    /// The visitNode hook (vbnet.ts): events are `field` nodes (a Custom Event's accessors run
    /// code, which is the event's); `Sub New` is the method `New`. True when consumed.
    fn hook(&mut self, node: Node<'t>) -> bool {
        match node.kind() {
            "event_declaration" | "custom_event_declaration" => {
                let name = node.child_by_field_name("name").map(|n| self.text(n));
                let event = name.and_then(|n| self.create_node("field", n, node, Extra::default()));
                if let (Some(row), true) = (event, node.kind() == "custom_event_declaration") {
                    self.push_scope(row, "field", name.unwrap_or(""));
                    self.visit_body(node, None);
                    self.stack.pop();
                }
                true
            }
            "constructor_declaration" => {
                if let Some(row) = self.create_node("method", "New", node, Extra::default()) {
                    self.push_scope(row, "method", "New");
                    self.visit_body(node, None);
                    self.stack.pop();
                }
                true
            }
            _ => false,
        }
    }

    // --- dispatchNode ----------------------------------------------------------------

    fn visit_node(&mut self, node: Node<'t>) {
        stack_guard!();
        if self.hook(node) {
            return;
        }
        let kind = node.kind();
        let mut skip_children = false;
        if matches!(kind, "class_declaration" | "module_declaration") {
            self.extract_class(node);
            skip_children = true;
        } else if METHOD_TYPES.contains(&kind) {
            self.extract_method(node);
            skip_children = true;
        } else if kind == "interface_declaration" {
            self.extract_interface(node);
            skip_children = true;
        } else if kind == "structure_declaration" {
            self.extract_struct(node);
            skip_children = true;
        } else if kind == "enum_declaration" {
            self.extract_enum(node);
            skip_children = true;
        } else if kind == "delegate_declaration" {
            self.extract_type_alias(node);
        } else if PROPERTY_TYPES.contains(&kind) && self.inside_class_like() {
            if let Some((row, name)) = self.extract_property(node) {
                self.push_scope(row, "property", &name);
                self.visit_body(node, None);
                self.stack.pop();
            }
            skip_children = true;
        } else if kind == "field_declaration" && self.inside_class_like() {
            self.extract_field(node);
            skip_children = true;
        } else if kind == "declaration_statement" && !self.inside_class_like() {
            self.extract_variable(node);
            skip_children = true;
        } else if kind == "imports_statement" {
            self.extract_import(node);
        } else if CALL_TYPES.contains(&kind) {
            self.extract_call(node);
        } else if kind == "object_creation_expression" || self.is_ctor_shaped_array_creation(node) {
            self.extract_instantiation(node);
        }
        if !skip_children {
            for child in named_children(node) {
                self.visit_node(child);
            }
        }
    }

    /// visitFunctionBody (tree-sitter.ts) as it runs for VB.NET: calls, instantiations and
    /// member reads all the way down; a type declared in a body is extracted.
    fn visit_body(&mut self, node: Node<'t>, parent: Option<Node<'t>>) {
        stack_guard!();
        let kind = node.kind();
        if CALL_TYPES.contains(&kind) {
            self.extract_call(node);
        } else if kind == "object_creation_expression" || self.is_ctor_shaped_array_creation(node) {
            self.extract_instantiation(node);
        }
        self.extract_member_read(node, parent);
        match kind {
            "class_declaration" | "module_declaration" => return self.extract_class(node),
            "structure_declaration" => return self.extract_struct(node),
            "enum_declaration" => return self.extract_enum(node),
            "interface_declaration" => return self.extract_interface(node),
            _ => {}
        }
        for child in named_children(node) {
            self.visit_body(child, Some(node));
        }
    }

    // --- declarations ----------------------------------------------------------------

    fn extract_class(&mut self, node: Node<'t>) {
        let name = self.extract_name(node);
        let extra = Extra { docstring: self.docstring(node), visibility: Some(self.visibility_of(node)), ..Extra::default() };
        let Some(row) = self.create_node("class", &name, node, extra) else { return };
        self.extract_inheritance(node, row);
        self.push_scope(row, "class", &name);
        for child in named_children(node) {
            self.visit_node(child);
        }
        self.stack.pop();
    }

    fn extract_interface(&mut self, node: Node<'t>) {
        let name = self.extract_name(node);
        let extra = Extra { docstring: self.docstring(node), ..Extra::default() };
        let Some(row) = self.create_node("interface", &name, node, extra) else { return };
        self.extract_inheritance(node, row);
        self.push_scope(row, "interface", &name);
        for child in named_children(node) {
            self.visit_node(child);
        }
        self.stack.pop();
    }

    /// extractAggregate for a `Structure`.
    fn extract_struct(&mut self, node: Node<'t>) {
        let name = self.extract_name(node);
        let extra = Extra { docstring: self.docstring(node), visibility: Some(self.visibility_of(node)), ..Extra::default() };
        let Some(row) = self.create_node("struct", &name, node, extra) else { return };
        self.extract_inheritance(node, row);
        self.push_scope(row, "struct", &name);
        for child in named_children(node) {
            self.visit_node(child);
        }
        self.stack.pop();
    }

    fn extract_enum(&mut self, node: Node<'t>) {
        let name = self.extract_name(node);
        let extra = Extra { docstring: self.docstring(node), visibility: Some(self.visibility_of(node)), ..Extra::default() };
        let Some(row) = self.create_node("enum", &name, node, extra) else { return };
        self.extract_inheritance(node, row);
        self.push_scope(row, "enum", &name);
        for child in named_children(node) {
            if child.kind() == "enum_member_declaration" {
                self.extract_enum_members(child);
            } else {
                self.visit_node(child);
            }
        }
        self.stack.pop();
    }

    /// extractEnumMembers (tree-sitter.ts).
    fn extract_enum_members(&mut self, node: Node<'t>) {
        if let Some(name) = node.child_by_field_name("name") {
            let text = self.text(name);
            self.create_node("enum_member", text, node, Extra::default());
            return;
        }
        let mut found = false;
        for child in named_children(node) {
            if matches!(child.kind(), "simple_identifier" | "identifier" | "property_identifier") {
                let text = self.text(child);
                self.create_node("enum_member", text, child, Extra::default());
                found = true;
            }
        }
        if !found && node.named_child_count() == 0 {
            let text = self.text(node);
            self.create_node("enum_member", text, node, Extra::default());
        }
    }

    /// extractMethod — outside a type, extractFunction.
    fn extract_method(&mut self, node: Node<'t>) {
        let name = self.extract_name(node);
        let inside = self.inside_class_like();
        if !inside && name == "<anonymous>" {
            self.visit_body(node, None);
            return;
        }
        let kind = if inside { "method" } else { "function" };
        let extra = Extra {
            docstring: self.docstring(node),
            visibility: Some(self.visibility_of(node)),
            is_async: Some(self.has_modifier(node, "async")),
            is_static: Some(self.is_static(node)),
            return_type: self.return_type(node),
            ..Extra::default()
        };
        let Some(row) = self.create_node(kind, &name, node, extra) else { return };
        self.push_scope(row, kind, &name);
        self.visit_body(node, None);
        self.stack.pop();
    }

    /// extractTypeAlias for a `Delegate`: a type_alias node; the children are walked after.
    fn extract_type_alias(&mut self, node: Node<'t>) {
        let name = self.extract_name(node);
        if name == "<anonymous>" {
            return;
        }
        let extra = Extra { docstring: self.docstring(node), ..Extra::default() };
        self.create_node("type_alias", &name, node, extra);
    }

    /// extractProperty (tree-sitter.ts): the signature is the first child that is not a
    /// modifier, the name or an accessor list — a `Public` modifier, as often as a type.
    fn extract_property(&mut self, node: Node<'t>) -> Option<(u32, String)> {
        let children = named_children(node);
        let name_node = node
            .child_by_field_name("name")
            .or_else(|| node.child_by_field_name("property"))
            .or_else(|| children.iter().copied().find(|c| c.kind() == "identifier"))?;
        let name = self.text(name_node).to_string();
        if name.is_empty() {
            return None;
        }
        let type_node = children.iter().copied().find(|c| {
            !matches!(c.kind(), "modifier" | "modifiers" | "identifier" | "accessor_list" | "accessors" | "equals_value_clause")
        });
        let type_text = type_node.map(|t| strip_colon_prefix(self.text(t)).to_string());
        let signature = match type_text {
            Some(t) if !t.is_empty() => format!("{t} {name}"),
            _ => name.clone(),
        };
        let extra = Extra {
            docstring: self.docstring(node),
            signature: Some(signature),
            visibility: Some(self.visibility_of(node)),
            is_static: Some(self.is_static(node)),
            ..Extra::default()
        };
        let row = self.create_node("property", &name, node, extra)?;
        Some((row, name))
    }

    /// extractField (tree-sitter.ts): a `field` per declarator, each declarator walked whole
    /// with its field on the stack.
    fn extract_field(&mut self, node: Node<'t>) {
        let docstring = self.docstring(node);
        let visibility = self.visibility_of(node);
        let is_static = self.is_static(node);
        let children = named_children(node);
        let var_decl = children.iter().copied().find(|c| c.kind() == "variable_declaration");
        let mut declarators: Vec<Node<'t>> = children.iter().copied().filter(|c| c.kind() == "variable_declarator").collect();
        if declarators.is_empty() {
            if let Some(v) = var_decl {
                declarators = named_children(v).into_iter().filter(|c| c.kind() == "variable_declarator").collect();
            }
        }
        if declarators.is_empty() {
            let name_node = node
                .child_by_field_name("name")
                .or_else(|| children.iter().copied().find(|c| c.kind() == "identifier"));
            if let Some(n) = name_node {
                let name = self.text(n);
                let extra = Extra { docstring, visibility: Some(visibility), is_static: Some(is_static), ..Extra::default() };
                self.create_node("field", name, node, extra);
            }
            return;
        }
        let type_search = var_decl.map_or_else(|| children.clone(), named_children);
        let type_text = type_search
            .into_iter()
            .find(|c| {
                !matches!(
                    c.kind(),
                    "modifiers" | "modifier" | "variable_declarator" | "variable_declaration" | "marker_annotation" | "annotation"
                )
            })
            .map(|t| self.text(t));
        for decl in declarators {
            let name_node = decl
                .child_by_field_name("name")
                .or_else(|| named_children(decl).into_iter().find(|c| c.kind() == "identifier"));
            let Some(name_node) = name_node else { continue };
            let name = self.text(name_node);
            let signature = match type_text {
                Some(t) if !t.is_empty() => format!("{t} {name}"),
                _ => name.to_string(),
            };
            let extra = Extra {
                docstring: docstring.clone(),
                signature: Some(signature),
                visibility: Some(visibility),
                is_static: Some(is_static),
                ..Extra::default()
            };
            if let Some(row) = self.create_node("field", name, decl, extra) {
                self.push_scope(row, "field", name);
                self.visit_body(decl, None);
                self.stack.pop();
            }
        }
    }

    /// extractVariable's generic fallback, for a `Dim` outside a type: a node per declarator,
    /// its initializer not walked.
    fn extract_variable(&mut self, node: Node<'t>) {
        let kind = if self.is_const(node) { "constant" } else { "variable" };
        let docstring = self.docstring(node);
        for child in named_children(node) {
            let name = match child.kind() {
                "identifier" => self.text(child).to_string(),
                "variable_declarator" => self.extract_name(child),
                _ => continue,
            };
            if !name.is_empty() && name != "<anonymous>" {
                let extra = Extra { docstring: docstring.clone(), is_exported: Some(false), ..Extra::default() };
                self.create_node(kind, &name, child, extra);
            }
        }
    }

    /// extractImport with the vbnet hook: the last name child is the module; an `imports` ref
    /// from the scope.
    fn extract_import(&mut self, node: Node<'t>) {
        let name_node = named_children(node)
            .into_iter()
            .rev()
            .find(|c| matches!(c.kind(), "qualified_name" | "simple_name" | "global_qualified_name" | "identifier"));
        let Some(name_node) = name_node else { return };
        let module = self.text(name_node);
        let extra = Extra { signature: Some(js_trim(self.text(node)).to_string()), ..Extra::default() };
        self.create_node("import", module, node, extra);
        if !module.is_empty() {
            let from = self.top_row();
            self.push_ref(from, module, "imports", node);
        }
    }

    /// The vbnet branch of extractInheritance: `Inherits` / `Implements` statements among the
    /// type's children, each name past `(Of …)` and its dotted path.
    fn extract_inheritance(&mut self, node: Node<'t>, class_row: u32) {
        for child in named_children(node) {
            let kind = match child.kind() {
                "inherits_statement" => "extends",
                "implements_statement" => "implements",
                _ => continue,
            };
            for r in named_children(child) {
                if !matches!(r.kind(), "simple_name" | "qualified_name" | "generic_name" | "global_qualified_name") {
                    continue;
                }
                let stripped = of_args_re().replace_all(self.text(r), "");
                let name = js_trim(stripped.rsplit('.').next().unwrap_or(""));
                if !name.is_empty() {
                    let name = name.to_string();
                    self.push_ref(class_row, &name, kind, r);
                }
            }
        }
    }

    // --- calls, instantiations, member reads ------------------------------------------------

    /// The vbnet branch of extractCall: `f(x)`, `f()` and `f(Of T)()` are all call sites —
    /// the callee under the `function` / `array` field, a simple receiver kept.
    fn extract_call(&mut self, node: Node<'t>) {
        let Some(func) = node.child_by_field_name("function").or_else(|| node.child_by_field_name("array")) else {
            return;
        };
        let callee = match func.kind() {
            "member_access_expression" => {
                let member = func.child_by_field_name("member").map_or("", |m| self.text(m));
                if member.is_empty() {
                    return;
                }
                match func.child_by_field_name("object") {
                    Some(r) if r.kind() == "identifier" && !is_me_like(self.text(r)) => format!("{}.{member}", self.text(r)),
                    _ => member.to_string(),
                }
            }
            "identifier" => self.text(func).to_string(),
            "with_member_access_expression" => {
                let member = func.named_child(0);
                let receiver = self.with_target(func).and_then(|t| self.receiver_path(t, 0));
                let (Some(member), Some(receiver)) = (member.filter(|m| m.kind() == "identifier"), receiver) else {
                    return;
                };
                if is_non_type_receiver(receiver.split('.').next().unwrap_or("")) {
                    return;
                }
                let member = self.text(member);
                if is_me_like(&receiver) {
                    member.to_string()
                } else {
                    format!("{receiver}.{member}")
                }
            }
            _ => return,
        };
        if !callee.is_empty() {
            let from = self.top_row();
            self.push_ref(from, &callee, "calls", node);
        }
    }

    /// isVbnetConstructorShapedArrayCreation: `New Point(1, 2)` parses as an array creation;
    /// it is a construction when its type is a named one and it has no `{…}` initializer.
    fn is_ctor_shaped_array_creation(&self, node: Node) -> bool {
        if node.kind() != "array_creation_expression" {
            return false;
        }
        let Some(t) = node.child_by_field_name("type") else { return false };
        if matches!(t.kind(), "predefined_type" | "array_type") {
            return false;
        }
        !named_children(node).iter().any(|c| c.kind() == "array_initializer")
    }

    /// extractInstantiation (tree-sitter.ts) as it runs for VB.NET.
    fn extract_instantiation(&mut self, node: Node<'t>) {
        let ctor = node
            .child_by_field_name("constructor")
            .or_else(|| node.child_by_field_name("type"))
            .or_else(|| node.child_by_field_name("name"))
            .or_else(|| node.named_child(0));
        let Some(ctor) = ctor else { return };
        let mut class_name = self.text(ctor);
        if let Some(i) = class_name.find('<').filter(|&i| i > 0) {
            class_name = &class_name[..i];
        }
        if let Some(i) = class_name.find('(').filter(|&i| i > 0) {
            class_name = &class_name[..i];
        }
        let last_dot = class_name.rfind('.').max(class_name.rfind("::"));
        if let Some(i) = last_dot {
            class_name = &class_name[i + 1..];
            if class_name.starts_with([':', '.']) {
                class_name = &class_name[1..];
            }
        }
        let class_name = js_trim(class_name);
        if !class_name.is_empty() {
            let name = class_name.to_string();
            let from = self.top_row();
            self.push_ref(from, &name, "instantiates", node);
        }
    }

    /// extractVbMemberRead (tree-sitter.ts, #2305): a member read or written through a
    /// receiver is one `references` ref naming `receiver.member`; the resolver types it.
    fn extract_member_read(&mut self, node: Node<'t>, known_parent: Option<Node<'t>>) {
        let (member, receiver) = match node.kind() {
            "member_access_expression" => {
                let object = node.child_by_field_name("object");
                let receiver = object.filter(|o| !is_split_new(*o)).and_then(|o| self.receiver_path(o, 0));
                (node.child_by_field_name("member"), receiver)
            }
            "with_member_access_expression" => {
                (node.named_child(0), self.with_target(node).and_then(|t| self.receiver_path(t, 0)))
            }
            "member_initializer" => {
                let creation = known_parent.or_else(|| node.parent()).and_then(|p| p.parent());
                let receiver = creation.filter(|c| c.kind() == "object_creation_expression").and_then(|c| self.receiver_path(c, 0));
                (node.child_by_field_name("name"), receiver)
            }
            _ => return,
        };
        let (Some(receiver), Some(member)) = (receiver, member.filter(|m| m.kind() == "identifier")) else { return };
        if is_non_type_receiver(receiver.split('.').next().unwrap_or("")) {
            return;
        }
        if node.kind() != "member_initializer" {
            if let Some(parent) = known_parent.or_else(|| node.parent()).filter(|p| CALL_TYPES.contains(&p.kind())) {
                let callee = parent
                    .child_by_field_name("function")
                    .or_else(|| parent.child_by_field_name("method"))
                    .or_else(|| parent.named_child(0));
                if callee.is_some_and(|c| c.start_byte() == node.start_byte()) {
                    return;
                }
            }
        }
        let name = format!("{receiver}.{}", self.text(member));
        let from = self.top_row();
        self.push_ref(from, &name, "references", node);
    }

    /// vbReceiverPath (tree-sitter.ts): a name, `Me` / `MyClass` / `MyBase`, a member of one,
    /// a `With` block's `.Inner`, or `{T}` for a cast's or a `New`'s written type.
    fn receiver_path(&self, node: Node<'t>, depth: u32) -> Option<String> {
        if depth > 6 {
            return None;
        }
        match node.kind() {
            "identifier" => Some(self.text(node).to_string()),
            "me_expression" => Some("Me".to_string()),
            "myclass_expression" => Some("MyClass".to_string()),
            "mybase_expression" => Some("MyBase".to_string()),
            "parenthesized_expression" => node.named_child(0).and_then(|i| self.receiver_path(i, depth + 1)),
            "member_access_expression" | "with_member_access_expression" => {
                let through = node.kind() == "member_access_expression";
                let member = if through { node.child_by_field_name("member") } else { node.named_child(0) };
                let member = member.filter(|m| m.kind() == "identifier")?;
                let object = if through { node.child_by_field_name("object") } else { self.with_target(node) };
                if through && object.is_some_and(is_split_new) {
                    return None;
                }
                let head = object.and_then(|o| self.receiver_path(o, depth + 1))?;
                Some(format!("{head}.{}", self.text(member)))
            }
            "cast_expression" | "object_creation_expression" => {
                let mut ty = node.child_by_field_name("type");
                // `New T()` parses as an array type whose rank is the constructor's `()`.
                if node.kind() == "object_creation_expression" {
                    if let Some(t) = ty.filter(|t| t.kind() == "array_type") {
                        let rank = named_children(t).into_iter().find(|c| c.kind() == "array_rank_specifier");
                        ty = rank
                            .filter(|r| is_empty_parens(self.text(*r)))
                            .and_then(|_| t.child_by_field_name("element_type"));
                    }
                }
                let ty = ty.filter(|t| !matches!(t.kind(), "predefined_type" | "array_type"))?;
                let written = collapse_ws(self.text(ty));
                let lower = written.to_ascii_lowercase();
                if lower.starts_with("system.") || lower.starts_with("microsoft.") || lower.starts_with("my.") {
                    None
                } else {
                    Some(format!("{{{written}}}"))
                }
            }
            _ => None,
        }
    }

    /// vbWithTarget (tree-sitter.ts): what a `.Member` is read through.
    fn with_target(&self, node: Node<'t>) -> Option<Node<'t>> {
        let mut child = node;
        let mut p = node.parent();
        while let Some(parent) = p {
            match parent.kind() {
                "with_statement" => {
                    let expression = parent.child_by_field_name("expression");
                    let is_own = expression
                        .is_some_and(|e| e.start_byte() == child.start_byte() && e.end_byte() == child.end_byte());
                    if !is_own {
                        return expression;
                    }
                }
                "object_initializer" => {
                    return parent.parent().filter(|g| g.kind() == "object_creation_expression");
                }
                "anonymous_object_creation_expression" => return None,
                _ => {}
            }
            child = parent;
            p = parent.parent();
        }
        None
    }
}

/// isVbSplitNew: `New System` of `New System.Drawing.Size(…)` — a qualified type after `New`
/// read as members of a `New` of its first name.
fn is_split_new(node: Node) -> bool {
    node.kind() == "object_creation_expression"
        && node.child_by_field_name("arguments").is_none()
        && node.child_by_field_name("initializer").is_none()
        && node.child_by_field_name("type").map(|t| t.kind()) != Some("array_type")
}

/// `/^\(\s*\)$/` with JS's `\s`.
fn is_empty_parens(text: &str) -> bool {
    text.strip_prefix('(')
        .and_then(|t| t.strip_suffix(')'))
        .is_some_and(|inner| inner.chars().all(is_js_space))
}

/// `.replace(/^:\s*/, '')` with JS's `\s`.
fn strip_colon_prefix(text: &str) -> &str {
    match text.strip_prefix(':') {
        Some(rest) => rest.trim_start_matches(is_js_space),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use codegraph_types::Language;

    #[test]
    fn members_constructors_events_and_vb_call_shapes() {
        let src = "Namespace N\nPublic Class Shape\n    Inherits Base\n    Public Event Changed As EventHandler\n    Public Sub New()\n        Init()\n    End Sub\n    Private Sub Init()\n        Log(\"x\")\n        Dim p = New Point(1, 2)\n        Helpers.Normalize(Me)\n    End Sub\nEnd Class\nEnd Namespace";
        // No trailing newline: the preParse appends the one the grammar needs.
        let r = crate::extract_source("a.vb", src, Language::parse("vbnet").unwrap()).unwrap();
        let qns: Vec<&str> = r.nodes.iter().map(|n| n.qualified_name.as_str()).collect();
        assert!(qns.contains(&"N::Shape::New"));
        assert!(qns.contains(&"N::Shape::Changed"));
        assert!(qns.contains(&"N::Shape::Init"));
        let refs: Vec<&str> = r.unresolved_references.iter().map(|u| u.reference_name.as_str()).collect();
        for want in ["Base", "Init", "Log", "Point", "Helpers.Normalize"] {
            assert!(refs.contains(&want), "missing {want} in {refs:?}");
        }
    }
}
