//! COBOL extraction — a port of languages/cobol.ts over the generic walk of
//! `TreeSitterExtractor` (src/extraction/tree-sitter.ts).
//!
//! Everything goes through the `visitNode` hook: a program is a module, PROCEDURE DIVISION
//! sections and paragraphs are functions whose extents are rebuilt from the flat header
//! positions, DATA DIVISION entries nest by level number, and PERFORM / GO TO / CALL 'x' /
//! EXEC CICS / EXEC SQL INCLUDE / COPY become references. The walker reads the source after
//! the COBOL preParse (preparse/cobol.rs). The grammar is the TypeScript engine's own patched
//! tree-sitter-cobol wasm.

use crate::buffers::{
    build_meta, edge_kind_index, node_kind_index, Arena, BoolFlags, EdgeRow, EmitOut, NodeRow,
    RefRow, Tables, FLAG_IS_EXPORTED, NONE, NONE_STR,
};
use crate::ids;
use crate::textutil as util;
use crate::ts::Node;
use codegraph_types::jscompat::{is_js_space, js_trim, utf16_len, JS_SPACE_CLASS};
use regex::Regex;
use std::sync::OnceLock;

struct Patterns {
    cics_program: Regex,
    cics_transid: Regex,
    sql_include: Regex,
    value_literal: Regex,
    transid: Regex,
    special_register: Regex,
    filler: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| {
        let s = format!("[{JS_SPACE_CLASS}]");
        let b = r"(?-u:\b)";
        Patterns {
            cics_program: Regex::new(&format!(
                r#"(?i){b}(?:LINK|XCTL){b}(?s:.)*?{b}PROGRAM{s}*\({s}*(?:['"]([A-Za-z0-9$#@-]+)['"]|([A-Za-z0-9-]+)){s}*\)"#
            ))
            .unwrap(),
            cics_transid: Regex::new(&format!(
                r#"(?i){b}(?:RETURN|START){b}(?s:.)*?{b}TRANSID{s}*\({s}*(?:['"]([A-Za-z0-9$#@]{{1,4}})['"]|([A-Za-z0-9-]+)){s}*\)"#
            ))
            .unwrap(),
            sql_include: Regex::new(&format!(r"(?i){b}SQL{b}(?s:.)*?{b}INCLUDE{s}+([A-Za-z0-9$#@-]+)")).unwrap(),
            value_literal: Regex::new(&format!(r#"(?i){b}VALUE{s}+['"]([A-Za-z0-9$#@-]+)['"]"#)).unwrap(),
            transid: Regex::new(r"^[A-Za-z0-9$#@]{1,4}$").unwrap(),
            special_register: Regex::new(r"(?i)^(RETURN-CODE|SQLCODE|SQLSTATE|TALLY|EIB[A-Z-]+|DFH[A-Z-]+|WHEN-COMPILED|LENGTH|ADDRESS)$").unwrap(),
            filler: Regex::new(r"(?i)^FILLER$").unwrap(),
        }
    })
}

struct Walker<'t> {
    src: &'t str,
    file_path: &'t str,
    line_starts: Vec<usize>,
    arena: Arena,
    node_id_allocator: ids::NodeIdAllocator,
    tables: Tables,
    stack: Vec<u32>,
    /// Each node row's name (the file node's first), for qualified names.
    scope_names: Vec<String>,
    /// The data items created so far (variable, field, constant): upper-cased name and signature.
    data_items: Vec<(String, String)>,
}

pub fn extract(file_path: &str, source: &str) -> Result<EmitOut, String> {
    let grammar = crate::langs::grammar_for("cobol").ok_or("no cobol grammar")?;
    let t0 = std::time::Instant::now();
    let tree = crate::ts::parse(&grammar, "cobol", source)?;
    let mut w = Walker {
        src: source,
        file_path,
        line_starts: util::line_starts(source),
        arena: Arena::default(),
        node_id_allocator: ids::NodeIdAllocator::default(),
        tables: Tables::default(),
        stack: Vec::new(),
        scope_names: Vec::new(),
        data_items: Vec::new(),
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
    w.scope_names.push(base_name.to_string());
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

fn end_line_of(node: Node) -> u32 {
    node.end_position().row as u32 + 1
}

/// collapse (cobol.ts): whitespace runs as one space, capped at 120 UTF-16 units with an ellipsis.
fn collapse(text: &str) -> String {
    let mut flat = String::with_capacity(text.len());
    let mut in_ws = false;
    for c in text.chars() {
        if is_js_space(c) {
            if !in_ws {
                flat.push(' ');
                in_ws = true;
            }
        } else {
            flat.push(c);
            in_ws = false;
        }
    }
    let flat = js_trim(&flat);
    if utf16_len(flat) > 120 {
        format!("{}…", util::slice_utf16(flat, 119).0)
    } else {
        flat.to_string()
    }
}

/// `.replace(/^['"]|['"]$/g, '')`: one quote off each end.
fn strip_quotes(text: &str) -> &str {
    let text = text.strip_prefix(['\'', '"']).unwrap_or(text);
    text.strip_suffix(['\'', '"']).unwrap_or(text)
}

/// `parseInt(text, 10)`, NaN as None.
fn parse_int(text: &str) -> Option<i64> {
    let t = text.trim_start_matches(is_js_space);
    let (negative, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let end = digits.bytes().position(|b| !b.is_ascii_digit()).unwrap_or(digits.len());
    let value: i64 = digits[..end].parse().ok()?;
    Some(if negative { -value } else { value })
}

fn is_top_level(level: i64) -> bool {
    level == 1 || level == 66 || level == 77
}

/// targetBaseName (cobol.ts): the first WORD under an assignment target.
fn target_base_name(target: Node) -> Option<Node> {
    if target.kind() == "WORD" {
        return Some(target);
    }
    named_children(target).into_iter().find_map(target_base_name)
}

struct Item<'t> {
    node: Node<'t>,
    level: i64,
    name: Option<String>,
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
        self.stack.last().copied().unwrap_or(0)
    }

    /// addRef (cobol.ts): nothing without a source or a name.
    fn add_ref(&mut self, from: Option<u32>, name: &str, kind: &str, at: Node) {
        let Some(from_idx) = from else { return };
        if name.is_empty() {
            return;
        }
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

    /// createNode (tree-sitter.ts): qualified by the scopes above it, with cobol.ts's end line
    /// and signature.
    fn create_node(&mut self, kind: &'static str, name: &str, node: Node<'t>, end_line: Option<u32>, signature: Option<&str>) -> Option<u32> {
        if name.is_empty() {
            return None;
        }
        let start_line = self.line_of(node);
        let column = self.col_of(node);
        let id = self.node_id_allocator.generate(self.file_path, kind, name, start_line, column);
        let mut names: Vec<String> = Vec::new();
        for row in self.stack.iter().skip(1) {
            names.push(self.scope_names[*row as usize].clone());
        }
        names.push(name.to_string());
        let qualified = names.join("::");
        let name_ref = self.arena.put(name);
        let qn_ref = self.arena.put(&qualified);
        let id_ref = self.arena.put(&id);
        let sig_ref = signature.map_or(NONE_STR, |s| self.arena.put(s));
        let row = self.tables.push_node(&NodeRow {
            kind: node_kind_index(kind).unwrap(),
            visibility: 0,
            flags: BoolFlags::default(),
            start_line,
            end_line: end_line.unwrap_or_else(|| end_line_of(node)),
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
        self.scope_names.push(name.to_string());
        if matches!(kind, "variable" | "field" | "constant") {
            self.data_items.push((name.to_uppercase(), signature.unwrap_or_default().to_string()));
        }
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

    /// The visitNode hook (cobol.ts). Returns true when consumed.
    fn hook(&mut self, node: Node<'t>) -> bool {
        match node.kind() {
            "program_definition" => {
                let row = self.program_name(node).and_then(|name| self.create_node("module", &name, node, None, None));
                if let Some(row) = row {
                    self.stack.push(row);
                }
                for child in named_children(node) {
                    self.visit(child);
                }
                if row.is_some() {
                    self.stack.pop();
                }
                true
            }
            "procedure_division" => {
                self.walk_procedure_children(&named_children(node), end_line_of(node));
                true
            }
            "working_storage_section" | "record_description_list" => {
                self.walk_data_entries(&named_children(node));
                true
            }
            "copybook_fragment" => {
                let children = named_children(node);
                if children.iter().any(|c| c.kind() == "record_description_list") {
                    for child in children {
                        self.visit(child);
                    }
                } else {
                    self.walk_procedure_children(&children, end_line_of(node));
                }
                true
            }
            "copy_statement" => {
                self.handle_copy(node);
                true
            }
            "exec_statement" => {
                let from = Some(self.top_row());
                self.handle_exec(node, from);
                true
            }
            _ => false,
        }
    }

    /// programName (cobol.ts): identification_division > program_name.
    fn program_name(&self, program: Node<'t>) -> Option<String> {
        let id_div = named_children(program).into_iter().find(|c| c.kind() == "identification_division")?;
        let name = named_children(id_div).into_iter().find(|c| c.kind() == "program_name")?;
        let text = strip_quotes(js_trim(self.text(name)));
        Some(text.strip_suffix('.').unwrap_or(text).to_string())
    }

    /// handleCopy (cobol.ts): COPY <book> — an import node and an `imports` ref.
    fn handle_copy(&mut self, node: Node<'t>) {
        let Some(book) = node.child_by_field_name("book") else { return };
        let name = strip_quotes(js_trim(self.text(book))).to_string();
        if name.is_empty() {
            return;
        }
        let signature = collapse(self.text(node));
        self.create_node("import", &name, node, None, Some(&signature));
        let from = Some(self.top_row());
        self.add_ref(from, &name, "imports", node);
    }

    /// derefSameFileValue (cobol.ts): the VALUE literal of the first same-named data item.
    fn deref_same_file_value(&self, name: &str) -> Option<String> {
        let upper = name.to_uppercase();
        let (_, signature) = self.data_items.iter().find(|(n, _)| *n == upper)?;
        patterns().value_literal.captures(signature).map(|c| c[1].to_string())
    }

    /// handleExec (cobol.ts): CICS LINK/XCTL, CICS RETURN/START TRANSID and SQL INCLUDE.
    fn handle_exec(&mut self, node: Node<'t>, from: Option<u32>) {
        let text = self.text(node);
        let p = patterns();
        if let Some(c) = p.cics_program.captures(text) {
            let program = match c.get(1) {
                Some(literal) => Some(literal.as_str().to_string()),
                None => c.get(2).and_then(|name| self.deref_same_file_value(name.as_str())),
            };
            if let Some(program) = program {
                self.add_ref(from, &program, "calls", node);
            }
        }
        if let Some(c) = p.cics_transid.captures(text) {
            let tx = match c.get(1) {
                Some(literal) => Some(literal.as_str().to_string()),
                None => c.get(2).and_then(|name| self.deref_same_file_value(name.as_str())),
            };
            if let Some(tx) = tx.filter(|t| p.transid.is_match(t)) {
                self.add_ref(from, &format!("cics-transid:{}", tx.to_uppercase()), "calls", node);
            }
        }
        if let Some(member) = p.sql_include.captures(text).and_then(|c| c.get(1)) {
            let member = member.as_str();
            let signature = collapse(text);
            self.create_node("import", member, node, None, Some(&signature));
            let from = from.or(Some(self.top_row()));
            self.add_ref(from, member, "imports", node);
        }
    }

    /// walkDataEntries (cobol.ts): level numbers drive the nesting; 88-levels are constants.
    fn walk_data_entries(&mut self, entries: &[Node<'t>]) {
        let items: Vec<Option<Item<'t>>> = entries
            .iter()
            .map(|e| {
                if e.kind() != "data_description" {
                    return None;
                }
                let children = named_children(*e);
                let level = children.iter().find(|c| c.kind() == "level_number").and_then(|l| parse_int(self.text(*l)));
                let name = children.iter().find(|c| c.kind() == "entry_name").map(|n| js_trim(self.text(*n)).to_string());
                Some(Item { node: *e, level: level.unwrap_or(1), name })
            })
            .collect();
        let group_end = |i: usize| -> u32 {
            let item = items[i].as_ref().unwrap();
            let mut end = end_line_of(item.node);
            for it in items.iter().skip(i + 1).flatten() {
                if it.level != 88 && (it.level <= item.level || is_top_level(it.level)) {
                    break;
                }
                end = end.max(end_line_of(it.node));
            }
            end
        };

        let mut open: Vec<i64> = Vec::new();
        for (i, entry) in entries.iter().enumerate() {
            match entry.kind() {
                "copy_statement" => {
                    self.handle_copy(*entry);
                    continue;
                }
                "exec_statement" => {
                    let from = Some(self.top_row());
                    self.handle_exec(*entry, from);
                    continue;
                }
                _ => {}
            }
            let Some(item) = &items[i] else { continue };
            let is_condition = item.level == 88;
            if !is_condition {
                let close_to = if is_top_level(item.level) { 0 } else { item.level };
                while open.last().is_some_and(|l| *l >= close_to) {
                    open.pop();
                    self.stack.pop();
                }
            }
            let Some(name) = item.name.as_deref().filter(|n| !n.is_empty() && !patterns().filler.is_match(n)) else { continue };
            let kind = if is_condition {
                "constant"
            } else if open.is_empty() {
                "variable"
            } else {
                "field"
            };
            let end_line = if is_condition { end_line_of(item.node) } else { group_end(i) };
            let signature = collapse(self.text(item.node));
            let created = self.create_node(kind, name, item.node, Some(end_line), Some(&signature));
            if let (Some(row), false) = (created, is_condition) {
                self.stack.push(row);
                open.push(item.level);
            }
        }
        for _ in open {
            self.stack.pop();
        }
    }

    /// emitWriteRefs (cobol.ts): a `references` ref for each data item a statement writes.
    fn emit_write_refs(&mut self, statement: Node<'t>, fields: &[&str], from: Option<u32>) {
        for field in fields {
            let targets: Vec<Node<'t>> = {
                let mut cursor = statement.walk();
                statement.children_by_field_name(field, &mut cursor).collect()
            };
            for target in targets {
                let Some(word) = target_base_name(target) else { continue };
                let name = js_trim(self.text(word));
                if name.is_empty() || patterns().special_register.is_match(name) {
                    continue;
                }
                self.add_ref(from, name, "references", word);
            }
        }
    }

    /// collectRefs (cobol.ts): a statement's calls, writes and imports.
    fn collect_refs(&mut self, node: Node<'t>, from: Option<u32>) {
        stack_guard!();
        match node.kind() {
            "move_statement" => self.emit_write_refs(node, &["dst"], from),
            "add_statement" => self.emit_write_refs(node, &["to", "giving"], from),
            "compute_statement" => self.emit_write_refs(node, &["left"], from),
            "subtract_statement" => {
                let has_giving = {
                    let mut cursor = node.walk();
                    let found = node.children_by_field_name("giving", &mut cursor).next().is_some();
                    found
                };
                self.emit_write_refs(node, &[if has_giving { "giving" } else { "from" }], from);
            }
            "perform_statement_call_proc" => {
                if let Some(procedure) = node.child_by_field_name("procedure") {
                    for label in named_children(procedure) {
                        if label.kind() == "label" {
                            let name = js_trim(self.text(label));
                            self.add_ref(from, name, "calls", label);
                        }
                    }
                }
            }
            "call_statement" => {
                if let Some(x) = node.child_by_field_name("x").filter(|x| x.kind() == "string") {
                    let name = js_trim(strip_quotes(self.text(x)));
                    self.add_ref(from, name, "calls", x);
                }
            }
            "goto_statement" => {
                if let Some(to) = node.child_by_field_name("to") {
                    let name = js_trim(self.text(to));
                    self.add_ref(from, name, "calls", to);
                }
            }
            "exec_statement" => self.handle_exec(node, from),
            "copy_statement" => self.handle_copy(node),
            _ => {
                for child in named_children(node) {
                    self.collect_refs(child, from);
                }
            }
        }
    }

    /// headerName (cobol.ts): "PARA-NAME." → "PARA-NAME"; "SEC SECTION." → "SEC".
    fn header_name(&self, header: Node<'t>) -> String {
        let text = js_trim(self.text(header));
        let text = js_trim(text.strip_suffix('.').unwrap_or(text));
        text.split(is_js_space).next().unwrap_or(text).to_string()
    }

    /// walkProcedureChildren (cobol.ts): section and paragraph extents from header positions;
    /// the statements between headers belong to the open paragraph.
    fn walk_procedure_children(&mut self, children: &[Node<'t>], division_end_line: u32) {
        let region_end = |i: usize, sections_only: bool| -> u32 {
            for c in &children[i + 1..] {
                if c.kind() == "section_header" || (!sections_only && c.kind() == "paragraph_header") {
                    return (c.start_position().row as u32).max(children[i].start_position().row as u32 + 1);
                }
            }
            division_end_line
        };
        let mut current = Some(self.top_row());
        let mut section_pushed = false;
        for (i, child) in children.iter().enumerate() {
            match child.kind() {
                "section_header" => {
                    if section_pushed {
                        self.stack.pop();
                        section_pushed = false;
                    }
                    let name = self.header_name(*child);
                    if let Some(row) = self.create_node("function", &name, *child, Some(region_end(i, true)), Some("SECTION")) {
                        self.stack.push(row);
                        section_pushed = true;
                        current = Some(row);
                    }
                }
                "paragraph_header" => {
                    let name = self.header_name(*child);
                    if let Some(row) = self.create_node("function", &name, *child, Some(region_end(i, false)), None) {
                        current = Some(row);
                    }
                }
                _ => self.collect_refs(*child, current),
            }
        }
        if section_pushed {
            self.stack.pop();
        }
    }
}
