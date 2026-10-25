//! Pascal / Delphi extraction — a port of languages/pascal.ts over the generic walk of
//! `TreeSitterExtractor` (src/extraction/tree-sitter.ts) with its Pascal branches
//! (visitPascalNode and the routines after it): a unit's module node, `declType` classes,
//! interfaces, enums and aliases, `uses` imports, implementation `defProc`s matched to the
//! routines their class or interface section declared (methodIndex), nested routines, the
//! calls of routine bodies — paren'd, paren-less (`Obj.Free;`) and the names that may run a
//! parameterless routine (#2514), with the source text standing in for a broken tree's
//! routines — the `@Handler` function values (#756) and the same-file constant reads.
//!
//! The walk reads the pre-parsed source; a routine's signature reads the file as written
//! (`signatureAsWritten`). The preParse keeps UTF-16 offsets, so a node's span maps onto the
//! file as written by its UTF-16 offsets.

use crate::buffers::{
    build_meta, edge_kind_index, node_kind_index, Arena, BoolFlags, EdgeRow, EmitOut, NodeRow, RefRow, StrRef,
    Tables, FLAG_IS_EXPORTED, FLAG_IS_STATIC, FUNCTION_REF_CODE, NONE, NONE_STR,
};
use crate::docstring::preceding_docstring;
use crate::ids;
use crate::textutil as util;
use crate::ts::Node;
use codegraph_types::jscompat::{is_js_space, js_trim, js_trim_end, js_trim_start};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

const MAX_VALUE_REF_NODES: usize = 20_000;

/// PASCAL_NOT_ROUTINES (tree-sitter.ts): bare names never a project routine's call.
fn is_not_routine(lower: &str) -> bool {
    matches!(lower, "self" | "result" | "true" | "false" | "nil" | "exit" | "break" | "continue")
}

/// PASCAL_SYSTEM_NAMES (languages/pascal-system.ts).
pub fn is_system_name(lower: &str) -> bool {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| {
        [
            "abs", "addr", "arctan", "assert", "assigned", "assignfile", "atomiccmpexchange", "atomicdecrement",
            "atomicexchange", "atomicincrement", "blockread", "blockwrite", "break", "chdir", "chr", "closefile",
            "concat", "continue", "copy", "cos", "dec", "default", "delete", "dispose", "eof", "eoln", "exclude",
            "exit", "exp", "fail", "filepos", "filesize", "fillchar", "finalize", "frac", "freemem", "getdir",
            "getmem", "gettypekind", "halt", "hasweakref", "hi", "high", "inc", "include", "initialize", "insert",
            "int", "ioresult", "isconstvalue", "ismanagedtype", "length", "ln", "lo", "low", "mkdir", "move", "new",
            "odd", "ord", "paramcount", "paramstr", "pi", "pred", "ptr", "random", "randomize", "read", "readln",
            "reallocmem", "returnaddress", "rmdir", "round", "runerror", "setlength", "setstring", "sin", "sizeof",
            "slice", "sqr", "sqrt", "str", "succ", "swap", "trunc", "typehandle", "typeinfo", "typeof",
            "uniquestring", "upcase", "val", "vararrayredim", "write", "writeln", "abort", "date", "datetostr",
            "floattostr", "format", "freeandnil", "inttostr", "lowercase", "now", "pos", "strtodate", "strtofloat",
            "strtoint", "time", "trim", "uppercase", "ansichar", "ansistring", "boolean", "byte", "bytebool",
            "cardinal", "char", "comp", "currency", "double", "extended", "int16", "int32", "int64", "int8",
            "integer", "longbool", "longint", "longword", "nativeint", "nativeuint", "olevariant", "pansichar",
            "pchar", "pointer", "pwidechar", "rawbytestring", "real", "shortint", "shortstring", "single",
            "smallint", "string", "uint16", "uint32", "uint64", "uint8", "unicodestring", "utf8string", "variant",
            "widechar", "widestring", "word", "wordbool",
        ]
        .into_iter()
        .collect()
    })
    .contains(lower)
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// `/^\w+$/` with JS's ASCII `\w`.
fn is_word(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_word_char)
}

/// `/^[A-Za-z_]\w*$/`.
fn is_ascii_ident(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && chars.all(is_word_char)
}

/// `/^[TI][A-Z]/` — the Delphi type-naming convention.
fn is_type_like(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && (b[0] == b'T' || b[0] == b'I') && b[1].is_ascii_uppercase()
}

/// PASCAL_DATA_NAME: `/^(?:F[A-Z][a-z]|[TE][A-Z][a-z]|[A-Z][A-Z0-9_]*$)/`.
fn is_data_name_shape(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() >= 3 && (b[0] == b'F' || b[0] == b'T' || b[0] == b'E') && b[1].is_ascii_uppercase() && b[2].is_ascii_lowercase() {
        return true;
    }
    !b.is_empty() && b[0].is_ascii_uppercase() && b[1..].iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_')
}

/// `/^[a-z][A-Za-z]*$/`.
fn is_lower_word(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && b[0].is_ascii_lowercase() && b[1..].iter().all(|c| c.is_ascii_alphabetic())
}

/// `/^F?On[A-Z]/` — an event's name.
fn is_event_name(s: &str) -> bool {
    let s = s.strip_prefix('F').unwrap_or(s);
    let b = s.as_bytes();
    b.len() >= 3 && b[0] == b'O' && b[1] == b'n' && b[2].is_ascii_uppercase()
}

/// `/^k[A-Z]/` on a node type — a keyword token.
fn is_keyword_kind(kind: &str) -> bool {
    let b = kind.as_bytes();
    b.len() >= 2 && b[0] == b'k' && b[1].is_ascii_uppercase()
}

/// `.replace(/<[^>]*>/g, '')`.
fn strip_angles(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        match rest[i..].find('>') {
            Some(j) => rest = &rest[i + j + 1..],
            None => {
                out.push('<');
                rest = &rest[i + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The lowercase routine keywords a header starts with.
fn is_routine_keyword(word: &str) -> bool {
    ["procedure", "function", "constructor", "destructor", "operator"].iter().any(|k| k.eq_ignore_ascii_case(word))
}

fn named_children<'t>(node: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn find_named<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    named_children(node).into_iter().find(|c| c.kind() == kind)
}

fn last_named<'t>(node: Node<'t>) -> Option<Node<'t>> {
    node.named_child_count().checked_sub(1).and_then(|i| node.named_child(i))
}

fn same(a: Option<Node>, b: Node) -> bool {
    a.is_some_and(|a| a.id() == b.id())
}

struct Scope {
    row: u32,
    kind: &'static str,
    name: String,
}

/// A node made so far, as the walk's lookups read `this.nodes`.
struct Rec {
    id: String,
    kind: &'static str,
    name: String,
    qualified_name: String,
}

/// A `pascalScopes` entry: the names a routine declares (lowercased → declared type).
struct PScope {
    locals: HashMap<String, Option<String>>,
    class_qn: Option<String>,
    routine: Option<String>,
    start_line: u32,
}

/// PascalTextRoutine: a routine header of the source text, for a broken tree's regions.
struct TextRoutine {
    line: u32,
    class_qn: Option<String>,
    routine: Option<String>,
    locals: HashMap<String, Option<String>>,
}

/// pascalScopeAt's answer: the class and routine a name belongs to, with the source text's
/// declarations (an index into the text routines) when the text says whose it is.
struct ScopeView {
    class_qn: Option<String>,
    routine: Option<String>,
    text: Option<usize>,
}

struct PendingRef {
    from: u32,
    name: String,
    line: u32,
    column: u32,
}

struct Cand {
    from: u32,
    name: String,
    line: u32,
    column: u32,
}

struct ValueScope<'t> {
    row: u32,
    node: Node<'t>,
    name: String,
}

#[derive(Default)]
struct Extra {
    docstring: Option<String>,
    signature: Option<String>,
    visibility: u8,
    is_exported: Option<bool>,
    is_static: Option<bool>,
    return_type: Option<String>,
    qualified_name: Option<String>,
}

/// The file as written, addressed by the pre-parsed source's spans.
struct Written<'t> {
    text: &'t str,
    /// Set when the preParse changed a byte length: the UTF-16 offset of each pre-parsed byte,
    /// and the written byte of each UTF-16 offset.
    map: Option<(Vec<u32>, Vec<usize>)>,
}

impl<'t> Written<'t> {
    fn new(pre: &str, text: &'t str) -> Self {
        if pre.len() == text.len() {
            return Written { text, map: None };
        }
        let mut pre_units = vec![0u32; pre.len() + 1];
        let mut units = 0u32;
        for (i, c) in pre.char_indices() {
            pre_units[i] = units;
            units += c.len_utf16() as u32;
        }
        pre_units[pre.len()] = units;
        let mut bytes = Vec::with_capacity(text.len() + 1);
        for (i, c) in text.char_indices() {
            for _ in 0..c.len_utf16() {
                bytes.push(i);
            }
        }
        bytes.push(text.len());
        Written { text, map: Some((pre_units, bytes)) }
    }

    fn slice(&self, node: Node) -> &'t str {
        let range = node.byte_range();
        match &self.map {
            None => self.text.get(range).unwrap_or(""),
            Some((pre_units, bytes)) => {
                let at = |b: usize| bytes.get(pre_units[b] as usize).copied().unwrap_or(self.text.len());
                let (s, e) = (at(range.start), at(range.end));
                self.text.get(s..e.max(s)).unwrap_or("")
            }
        }
    }
}

struct Walker<'t> {
    src: &'t str,
    written: Written<'t>,
    file_path: &'t str,
    line_starts: Vec<usize>,
    has_error: bool,
    arena: Arena,
    node_id_allocator: ids::NodeIdAllocator,
    tables: Tables,
    stack: Vec<Scope>,
    nodes: Vec<Rec>,
    method_index: Option<HashMap<String, u32>>,
    pscopes: Vec<PScope>,
    text_routines: Option<Vec<TextRoutine>>,
    with_depth: u32,
    data_name_sets: HashMap<String, HashSet<String>>,
    reads: Vec<(PendingRef, String, String)>,
    assigned: HashMap<String, HashSet<String>>,
    imported_names: HashSet<String>,
    fn_ref_cands: Vec<Cand>,
    fs_values: HashMap<String, u32>,
    fs_value_counts: HashMap<String, u32>,
    value_scopes: Vec<ValueScope<'t>>,
}

pub fn extract(file_path: &str, source: &str, as_written: &str) -> Result<EmitOut, String> {
    let grammar = crate::langs::grammar_for("pascal").ok_or("no pascal grammar")?;
    let t0 = std::time::Instant::now();
    let tree = crate::ts::parse(&grammar, "pascal", source)?;
    let root = tree.root_node();
    let mut w = Walker {
        src: source,
        written: Written::new(source, as_written),
        file_path,
        line_starts: util::line_starts(source),
        has_error: root.has_error(),
        arena: Arena::default(),
        node_id_allocator: ids::NodeIdAllocator::default(),
        tables: Tables::default(),
        stack: Vec::new(),
        nodes: Vec::new(),
        method_index: None,
        pscopes: Vec::new(),
        text_routines: None,
        with_depth: 0,
        data_name_sets: HashMap::new(),
        reads: Vec::new(),
        assigned: HashMap::new(),
        imported_names: HashSet::new(),
        fn_ref_cands: Vec::new(),
        fs_values: HashMap::new(),
        fs_value_counts: HashMap::new(),
        value_scopes: Vec::new(),
    };

    // File node (tree-sitter.ts extract()).
    let line_count = source.bytes().filter(|b| *b == b'\n').count() as u32 + 1;
    let base_name = file_path.rsplit(['/', '\\']).next().unwrap_or(file_path);
    let mut flags = BoolFlags::default();
    flags.set(FLAG_IS_EXPORTED, false);
    let file_id = ids::file_node_id(file_path);
    let id_ref = w.arena.put(&file_id);
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
        id: id_ref,
        docstring: NONE_STR,
        signature: NONE_STR,
        decorators: NONE_STR,
        type_parameters: NONE_STR,
        return_type: NONE_STR,
        extra_json: NONE_STR,
    });
    w.nodes.push(Rec { id: file_id, kind: "file", name: base_name.to_string(), qualified_name: file_path.to_string() });
    w.stack.push(Scope { row: 0, kind: "file", name: base_name.to_string() });

    w.visit_node(root);
    w.flush_fn_ref_candidates();
    w.flush_value_refs(root);
    w.flush_pascal_reads();
    w.stack.pop();

    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let meta = build_meta(&w.tables, w.arena.len(), NONE_STR, duration_ms);
    Ok(EmitOut { meta, nodes: w.tables.nodes, edges: w.tables.edges, refs: w.tables.refs, arena: w.arena.into_vec() })
}

fn opt_str(arena: &mut Arena, s: Option<&str>) -> StrRef {
    match s {
        Some(v) => arena.put(v),
        None => NONE_STR,
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
    fn top_row(&self) -> u32 {
        self.stack.last().map(|s| s.row).unwrap_or(0)
    }
    fn inside_class_like(&self) -> bool {
        self.stack
            .last()
            .is_some_and(|s| matches!(s.kind, "class" | "struct" | "union" | "interface" | "trait" | "enum" | "module" | "enum_member"))
    }
    /// Push an existing node onto the scope stack (`nodeStack.push(id)`).
    fn push_row(&mut self, row: u32) {
        let rec = &self.nodes[row as usize];
        self.stack.push(Scope { row, kind: rec.kind, name: rec.name.clone() });
    }

    fn push_ref(&mut self, from_idx: u32, name: &str, kind: &str, at: Node) {
        let line = self.line_of(at);
        let column = self.col_of(at);
        self.push_ref_at(from_idx, name, kind, line, column);
    }

    fn push_ref_at(&mut self, from_idx: u32, name: &str, kind: &str, line: u32, column: u32) {
        let reference_name = self.arena.put(name);
        self.tables.push_ref(&RefRow {
            from_idx,
            kind: edge_kind_index(kind).unwrap(),
            line,
            column,
            reference_name,
            candidates: NONE_STR,
            from_id_str: NONE_STR,
        });
        if kind == "imports" {
            if util::simple_name().is_match(name) {
                self.imported_names.insert(name.to_string());
            } else if let Some(c) = util::qualified_import().captures(name) {
                self.imported_names.insert(c[1].to_string());
            }
        }
    }

    // --- createNode (tree-sitter.ts) ----------------------------------------

    fn create_node(&mut self, kind: &'static str, name: &str, node: Node<'t>, extra: Extra) -> Option<u32> {
        if name.is_empty() {
            return None;
        }
        let start_line = self.line_of(node);
        let column = self.col_of(node);
        let id = self.node_id_allocator.generate(self.file_path, kind, name, start_line, column);
        let qualified = match extra.qualified_name {
            Some(q) => q,
            None => {
                let mut parts: Vec<&str> = self.stack.iter().filter(|s| s.kind != "file").map(|s| s.name.as_str()).collect();
                parts.push(name);
                parts.join("::")
            }
        };
        let name_ref = self.arena.put(name);
        let qn_ref = self.arena.put(&qualified);
        let id_ref = self.arena.put(&id);
        let doc_ref = opt_str(&mut self.arena, extra.docstring.as_deref());
        let sig_ref = opt_str(&mut self.arena, extra.signature.as_deref());
        let ret_ref = opt_str(&mut self.arena, extra.return_type.as_deref());
        let mut flags = BoolFlags::default();
        if let Some(v) = extra.is_exported {
            flags.set(FLAG_IS_EXPORTED, v);
        }
        if let Some(v) = extra.is_static {
            flags.set(FLAG_IS_STATIC, v);
        }
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
            return_type: ret_ref,
            extra_json: NONE_STR,
        });
        self.nodes.push(Rec { id, kind, name: name.to_string(), qualified_name: qualified });

        let parent_row = self.top_row();
        let parent_kind = self.stack.last().map(|s| s.kind).unwrap_or("file");
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

        // captureValueRefScope: Pascal targets `constant` only.
        if kind == "constant"
            && util::utf16_len(name) >= 3
            && util::has_upper_or_underscore().is_match(name)
            && matches!(parent_kind, "file" | "class" | "module" | "struct" | "enum")
        {
            self.fs_values.insert(name.to_string(), row);
            *self.fs_value_counts.entry(name.to_string()).or_insert(0) += 1;
        }
        if matches!(kind, "function" | "method" | "constant" | "variable") {
            self.value_scopes.push(ValueScope { row, node, name: name.to_string() });
        }
        Some(row)
    }

    // --- languages/pascal.ts hooks ------------------------------------------

    /// extractName: the `name` field's text, else the first identifier child.
    fn extract_name(&self, node: Node<'t>) -> String {
        if let Some(n) = node.child_by_field_name("name") {
            return self.text(n).to_string();
        }
        match named_children(node).into_iter().find(|c| c.kind() == "identifier") {
            Some(c) => self.text(c).to_string(),
            None => "<anonymous>".to_string(),
        }
    }

    fn docstring_of(&self, node: Node<'t>) -> Option<String> {
        preceding_docstring(node, self.src)
    }

    /// getSignature: the parameters and `: ReturnType`, read from the file as written.
    fn signature_of(&self, node: Node<'t>) -> Option<String> {
        let args = node.child_by_field_name("args");
        let ret = find_named(node, "typeref");
        if args.is_none() && ret.is_none() {
            return None;
        }
        let mut sig = String::new();
        if let Some(a) = args {
            sig.push_str(self.written.slice(a));
        }
        if let Some(r) = ret {
            sig.push_str(": ");
            sig.push_str(self.written.slice(r));
        }
        (!sig.is_empty()).then_some(sig)
    }

    /// getReturnType: the bare class name of the `typeref` child.
    fn return_type_of(&self, node: Node<'t>) -> Option<String> {
        let typeref = find_named(node, "typeref")?;
        let id = find_named(typeref, "identifier").unwrap_or(typeref);
        let name = js_trim(self.text(id));
        is_ascii_ident(name).then(|| name.to_string())
    }

    /// getVisibility: the first visibility keyword of the nearest `declSection` that has one.
    fn visibility_of(&self, node: Node<'t>) -> u8 {
        let mut current = node.parent();
        while let Some(c) = current {
            if c.kind() == "declSection" {
                for i in 0..c.child_count() {
                    match c.child(i).map(|n| n.kind()) {
                        Some("kPublic" | "kPublished") => return 1,
                        Some("kPrivate") => return 2,
                        Some("kProtected") => return 3,
                        _ => {}
                    }
                }
            }
            current = c.parent();
        }
        0
    }

    /// isStatic: a `kClass` child (`class function`).
    fn is_static_of(&self, node: Node<'t>) -> bool {
        (0..node.child_count()).any(|i| node.child(i).is_some_and(|c| c.kind() == "kClass"))
    }

    // --- dispatchNode (generic walk with the Pascal branches) ---------------

    fn visit_node(&mut self, node: Node<'t>) {
        stack_guard!();
        if self.visit_pascal_node(node) {
            return;
        }
        self.maybe_capture_fn_refs(node);
        match node.kind() {
            "declProc" => {
                if self.inside_class_like() {
                    self.extract_method(node);
                } else {
                    self.extract_function(node);
                }
                return;
            }
            "declClass" => {
                self.extract_class(node);
                return;
            }
            "declIntf" => {
                self.extract_interface(node);
                return;
            }
            "declEnum" => {
                self.extract_enum(node);
                return;
            }
            _ => {}
        }
        for c in named_children(node) {
            self.visit_node(c);
        }
    }

    fn extract_function(&mut self, node: Node<'t>) {
        let name = self.extract_name(node);
        if name == "<anonymous>" {
            return; // no body field: nothing to walk
        }
        let extra = Extra {
            docstring: self.docstring_of(node),
            signature: self.signature_of(node),
            visibility: self.visibility_of(node),
            is_exported: Some(false),
            is_static: Some(self.is_static_of(node)),
            return_type: self.return_type_of(node),
            qualified_name: None,
        };
        self.create_node("function", &name, node, extra);
    }

    fn extract_method(&mut self, node: Node<'t>) {
        let name = self.extract_name(node);
        let extra = Extra {
            docstring: self.docstring_of(node),
            signature: self.signature_of(node),
            visibility: self.visibility_of(node),
            is_exported: None,
            is_static: Some(self.is_static_of(node)),
            return_type: self.return_type_of(node),
            qualified_name: None,
        };
        self.create_node("method", &name, node, extra);
    }

    fn extract_class(&mut self, node: Node<'t>) {
        let name = self.extract_name(node);
        let extra = Extra {
            docstring: self.docstring_of(node),
            visibility: self.visibility_of(node),
            is_exported: Some(false),
            ..Extra::default()
        };
        let Some(row) = self.create_node("class", &name, node, extra) else { return };
        self.push_row(row);
        let body = node.child_by_field_name("body").unwrap_or(node);
        for c in named_children(body) {
            self.visit_node(c);
        }
        self.stack.pop();
    }

    fn extract_interface(&mut self, node: Node<'t>) {
        let name = self.extract_name(node);
        let extra = Extra { docstring: self.docstring_of(node), is_exported: Some(false), ..Extra::default() };
        let Some(row) = self.create_node("interface", &name, node, extra) else { return };
        self.push_row(row);
        let body = node.child_by_field_name("body").unwrap_or(node);
        for c in named_children(body) {
            self.visit_node(c);
        }
        self.stack.pop();
    }

    fn extract_enum(&mut self, node: Node<'t>) {
        let Some(body) = node.child_by_field_name("body") else { return };
        let name = self.extract_name(node);
        let extra = Extra {
            docstring: self.docstring_of(node),
            visibility: self.visibility_of(node),
            is_exported: Some(false),
            ..Extra::default()
        };
        let Some(row) = self.create_node("enum", &name, node, extra) else { return };
        self.push_row(row);
        for c in named_children(body) {
            self.visit_node(c);
        }
        self.stack.pop();
    }

    // --- visitPascalNode ----------------------------------------------------

    fn visit_pascal_node(&mut self, node: Node<'t>) -> bool {
        stack_guard!();
        match node.kind() {
            "unit" | "program" | "library" => {
                let name = find_named(node, "moduleName").map(|m| self.text(m)).unwrap_or("");
                let module_name = if name.is_empty() { file_stem(self.file_path) } else { name.to_string() };
                self.create_node("module", &module_name, node, Extra::default());
                for c in named_children(node) {
                    self.visit_node(c);
                }
                true
            }
            "declType" => {
                self.extract_decl_type(node);
                true
            }
            "declUses" => {
                self.extract_uses(node);
                true
            }
            "declConsts" => {
                for c in named_children(node) {
                    if c.kind() == "declConst" {
                        self.extract_const(c);
                    }
                }
                true
            }
            "declConst" => {
                self.extract_const(node);
                true
            }
            "declTypes" | "declSection" | "interface" | "implementation" => {
                for c in named_children(node) {
                    self.visit_node(c);
                }
                true
            }
            "declVars" => {
                for c in named_children(node) {
                    if c.kind() == "declVar" {
                        if let Some(n) = c.child_by_field_name("name") {
                            let name = self.text(n).to_string();
                            self.create_node("variable", &name, c, Extra::default());
                        }
                    }
                }
                true
            }
            "defProc" => {
                self.extract_def_proc(node);
                true
            }
            "declProp" | "declField" => {
                if let Some(n) = node.child_by_field_name("name") {
                    let name = self.text(n).to_string();
                    let kind = if node.kind() == "declProp" { "property" } else { "field" };
                    let extra = Extra { visibility: self.visibility_of(node), ..Extra::default() };
                    self.create_node(kind, &name, node, extra);
                }
                true
            }
            "exprCall" => {
                self.extract_call(node, false);
                true
            }
            "block" | "blockTr" | "initialization" | "finalization" => {
                let outside = self.pscopes.is_empty();
                if outside {
                    let mut locals = HashMap::new();
                    locals.insert("result".to_string(), None);
                    self.pscopes.push(PScope { locals, class_qn: None, routine: None, start_line: 0 });
                }
                self.visit_block(node, node.kind() == "blockTr");
                if outside {
                    self.pscopes.pop();
                }
                true
            }
            _ => false,
        }
    }

    /// extractPascalDeclType: a class, interface, enum or type alias named by the `declType`.
    fn extract_decl_type(&mut self, node: Node<'t>) {
        let Some(name_node) = node.child_by_field_name("name") else { return };
        let name = self.text(name_node).to_string();
        let decl_class = find_named(node, "declClass");
        let decl_intf = find_named(node, "declIntf");
        let type_child = find_named(node, "type");
        if let Some(dc) = decl_class {
            let Some(row) = self.create_node("class", &name, node, Extra::default()) else { return };
            // extractPascalInheritance: the first typeref extends, the rest implement.
            let typerefs: Vec<Node> = named_children(dc).into_iter().filter(|c| c.kind() == "typeref").collect();
            for (i, r) in typerefs.into_iter().enumerate() {
                let ref_name = self.text(r).to_string();
                self.push_ref(row, &ref_name, if i == 0 { "extends" } else { "implements" }, r);
            }
            self.push_row(row);
            for c in named_children(dc) {
                self.visit_node(c);
            }
            self.stack.pop();
        } else if let Some(di) = decl_intf {
            let Some(row) = self.create_node("interface", &name, node, Extra::default()) else { return };
            self.push_row(row);
            for c in named_children(di) {
                self.visit_node(c);
            }
            self.stack.pop();
        } else if let Some(tc) = type_child {
            if let Some(de) = find_named(tc, "declEnum") {
                let Some(row) = self.create_node("enum", &name, node, Extra::default()) else { return };
                self.push_row(row);
                for c in named_children(de) {
                    if c.kind() == "declEnumValue" {
                        if let Some(m) = c.child_by_field_name("name") {
                            let member = self.text(m).to_string();
                            self.create_node("enum_member", &member, c, Extra::default());
                        }
                    }
                }
                self.stack.pop();
            } else {
                self.create_node("type_alias", &name, node, Extra::default());
            }
        } else {
            self.create_node("type_alias", &name, node, Extra::default());
        }
    }

    /// extractPascalUses: an import node and an `imports` ref per unit name.
    fn extract_uses(&mut self, node: Node<'t>) {
        let import_text = js_trim(self.text(node)).to_string();
        for c in named_children(node) {
            if c.kind() != "moduleName" {
                continue;
            }
            let unit = self.text(c).to_string();
            let extra = Extra { signature: Some(import_text.clone()), ..Extra::default() };
            self.create_node("import", &unit, c, extra);
            if !self.stack.is_empty() {
                let from = self.top_row();
                self.push_ref(from, &unit, "imports", c);
            }
        }
    }

    fn extract_const(&mut self, node: Node<'t>) {
        let Some(name_node) = node.child_by_field_name("name") else { return };
        let name = self.text(name_node).to_string();
        let sig = find_named(node, "defaultValue").map(|d| self.text(d).to_string());
        self.create_node("constant", &name, node, Extra { signature: sig, ..Extra::default() });
    }

    // --- implementation routines --------------------------------------------

    /// extractPascalDefProc: walk an implementation routine under the node its declaration made.
    fn extract_def_proc(&mut self, node: Node<'t>) {
        stack_guard!();
        let Some(decl_proc) = find_named(node, "declProc") else { return };
        let Some(name_node) = decl_proc.child_by_field_name("name") else { return };
        let full_name = js_trim(self.text(name_node)).to_string();
        let short_name = full_name.rsplit('.').next().unwrap_or("").to_string();
        let full_key = full_name.to_lowercase();
        let short_key = short_name.to_lowercase();

        if self.method_index.is_none() {
            let mut index: HashMap<String, u32> = HashMap::new();
            for (row, n) in self.nodes.iter().enumerate() {
                if n.kind != "method" && n.kind != "function" {
                    continue;
                }
                index.entry(n.name.to_lowercase()).or_insert(row as u32);
                if n.kind == "method" {
                    let parts: Vec<&str> = n.qualified_name.split("::").collect();
                    if parts.len() >= 2 {
                        for i in 0..parts.len() - 1 {
                            index.insert(parts[i..].join(".").to_lowercase(), row as u32);
                        }
                    }
                }
            }
            self.method_index = Some(index);
        }
        let index = self.method_index.as_ref().unwrap();
        let mut parent = index.get(&full_key).or_else(|| index.get(&short_key)).copied();

        if parent.is_none() && !full_name.contains('.') {
            let extra = Extra {
                signature: self.signature_of(decl_proc),
                visibility: self.visibility_of(decl_proc),
                ..Extra::default()
            };
            if let Some(row) = self.create_node("function", &full_name, decl_proc, extra) {
                parent = Some(row);
                let index = self.method_index.as_mut().unwrap();
                index.insert(full_key.clone(), row);
                index.entry(short_key.clone()).or_insert(row);
            }
        }
        let routine_row = match parent {
            Some(r) => r,
            None => match self.stack.last() {
                Some(s) => s.row,
                None => return,
            },
        };

        let owner = full_name.contains('.').then(|| {
            let stripped = strip_angles(&full_name);
            let parts: Vec<&str> = stripped.split('.').collect();
            parts[..parts.len() - 1].join("::")
        });
        let routine = self.nodes.get(routine_row as usize);
        let qualified_name = match routine {
            Some(r) if r.kind != "file" => r.qualified_name.clone(),
            _ => strip_angles(&full_name).split('.').collect::<Vec<_>>().join("::"),
        };
        let routine_name = match short_key.find('<') {
            Some(i) => short_key[..i].to_string(),
            None => short_key,
        };
        self.walk_routine(node, routine_row, owner, routine_name, qualified_name);
    }

    /// walkPascalRoutine: a routine's declarations and body, with the routine as their caller.
    fn walk_routine(&mut self, node: Node<'t>, routine_row: u32, class_qn: Option<String>, routine: String, qualified_name: String) {
        stack_guard!();
        let locals = self.routine_locals(node, true);
        self.pscopes.push(PScope {
            locals,
            class_qn: class_qn.clone(),
            routine: Some(routine),
            start_line: node.start_position().row as u32 + 1,
        });
        self.push_row(routine_row);
        let mut block = None;
        let mut misplaced = Vec::new();
        let broken = node.has_error();
        let col = node.start_position().column;
        // In order: a section's names join the locals a nested routine after it sees.
        for c in named_children(node) {
            match c.kind() {
                "declVars" | "declConsts" => {
                    for d in named_children(c) {
                        let (ty, names) = self.local_decl(d);
                        let locals = &mut self.pscopes.last_mut().unwrap().locals;
                        for n in names {
                            locals.insert(n, ty.clone());
                        }
                    }
                }
                // A nested routine comes before the body. Where the tree has errors, its parse
                // can swallow the unit's routines after it, which start their lines where it does.
                "defProc" => {
                    if block.is_some() || (broken && c.start_position().column <= col) {
                        misplaced.push(c);
                    } else {
                        self.extract_nested_routine(c, class_qn.clone(), &qualified_name);
                    }
                }
                // The first: `{$IFDEF}`s around two bodies leave a second.
                "block" if block.is_none() => block = Some(c),
                _ => {}
            }
        }
        if let Some(b) = block {
            self.visit_block(b, false);
        }
        self.stack.pop();
        self.pscopes.pop();
        for c in misplaced {
            self.extract_def_proc(c);
        }
    }

    /// extractPascalNestedRoutine: a routine nested in another, qualified under it.
    fn extract_nested_routine(&mut self, node: Node<'t>, class_qn: Option<String>, outer_qualified_name: &str) {
        let Some(decl_proc) = find_named(node, "declProc") else { return };
        let Some(name_node) = decl_proc.child_by_field_name("name") else { return };
        if name_node.kind() != "identifier" {
            return;
        }
        let name = self.text(name_node).to_string();
        let qualified_name = format!("{outer_qualified_name}::{name}");
        let extra = Extra {
            signature: self.signature_of(decl_proc),
            qualified_name: Some(qualified_name.clone()),
            ..Extra::default()
        };
        let Some(row) = self.create_node("function", &name, node, extra) else { return };
        self.walk_routine(node, row, class_qn, name.to_lowercase(), qualified_name);
    }

    /// pascalRoutineLocals: the names a routine (or an anonymous method) declares before its body.
    fn routine_locals(&self, routine: Node<'t>, header_only: bool) -> HashMap<String, Option<String>> {
        let mut names = HashMap::new();
        let header = if routine.kind() == "lambda" { Some(routine) } else { find_named(routine, "declProc") };
        names.insert("result".to_string(), self.type_name(header.and_then(|h| find_named(h, "typeref"))));
        if let Some(args) = header.and_then(|h| find_named(h, "declArgs")) {
            for arg in named_children(args) {
                if arg.kind() == "declArg" {
                    let (ty, ids) = self.local_decl(arg);
                    for n in ids {
                        names.insert(n, ty.clone());
                    }
                }
            }
        }
        if header_only {
            return names;
        }
        for c in named_children(routine) {
            if matches!(c.kind(), "declVars" | "declConsts") {
                for d in named_children(c) {
                    let (ty, ids) = self.local_decl(d);
                    for n in ids {
                        names.insert(n, ty.clone());
                    }
                }
            }
        }
        names
    }

    /// declarePascalLocal: the type a declaration gives and the lowercased names it declares.
    fn local_decl(&self, decl: Node<'t>) -> (Option<String>, Vec<String>) {
        let children = named_children(decl);
        let ty = self.type_name(children.iter().copied().find(|c| matches!(c.kind(), "type" | "typeref")));
        let names = children.iter().filter(|c| c.kind() == "identifier").map(|c| self.text(*c).to_lowercase()).collect();
        (ty, names)
    }

    /// declarePascalInline: a name the body declares where it stands joins the routine's locals.
    fn declare_inline(&mut self, node: Node<'t>) {
        if self.pscopes.is_empty() {
            return;
        }
        let (id, ty): (Option<Node>, Option<String>) = match node.kind() {
            "varAssignDef" | "varDef" => {
                let children = named_children(node);
                let mut ty = self.type_name(children.iter().copied().find(|c| matches!(c.kind(), "typeref" | "type")));
                let value = node.parent().filter(|p| p.kind() == "assignment").and_then(last_named);
                if ty.is_none() {
                    if let Some(v) = value.filter(|v| v.kind() == "exprDot") {
                        let cls = v.named_child(0);
                        let ctor = last_named(v);
                        if let (Some(cls), Some(ctor)) = (cls, ctor) {
                            if cls.kind() == "identifier" && ctor.kind() == "identifier" && self.text(ctor).eq_ignore_ascii_case("create") {
                                ty = Some(self.text(cls).to_string());
                            }
                        }
                    }
                }
                (children.into_iter().find(|c| c.kind() == "identifier"), ty)
            }
            "foreach" => (node.child_by_field_name("iterator"), None),
            "for" => (find_named(node, "assignment").and_then(|a| a.named_child(0)), None),
            "exceptionHandler" => (node.child_by_field_name("variable"), self.type_name(find_named(node, "typeref"))),
            _ => return,
        };
        let Some(id) = id.filter(|i| i.kind() == "identifier") else { return };
        let name = self.text(id).to_lowercase();
        let locals = &mut self.pscopes.last_mut().unwrap().locals;
        if ty.is_some() || !locals.contains_key(&name) {
            locals.insert(name, ty);
        }
    }

    /// pascalTypeName: the class or record name a declared type stands for.
    fn type_name(&self, ty: Option<Node<'t>>) -> Option<String> {
        let ty = ty?;
        let r = if ty.kind() == "type" { ty.named_child(0)? } else { ty };
        if r.kind() != "typeref" {
            return None;
        }
        let mut r = r.named_child(0)?;
        if r.kind() == "typerefDot" {
            r = last_named(r)?;
        } else if r.kind() == "typerefTpl" {
            r = r.named_child(0)?;
        }
        (r.kind() == "identifier").then(|| self.text(r).to_string())
    }

    // --- calls ----------------------------------------------------------------

    fn identifiers_joined(&self, node: Node<'t>) -> String {
        named_children(node).into_iter().filter(|c| c.kind() == "identifier").map(|c| self.text(c)).collect::<Vec<_>>().join(".")
    }

    /// extractPascalCall.
    fn extract_call(&mut self, node: Node<'t>, receiver_link: bool) {
        stack_guard!();
        let Some(caller) = self.stack.last().map(|s| s.row) else { return };
        let Some(first) = node.named_child(0) else { return };
        let mut callee = String::new();
        if first.kind() == "exprDot" {
            let children = named_children(first);
            let inner_call = children.iter().copied().find(|c| c.kind() == "exprCall");
            let outer_id = children.iter().copied().filter(|c| c.kind() == "identifier").last();
            let method = outer_id.map(|o| self.text(o)).unwrap_or("");
            if let (Some(ic), true) = (inner_call, !method.is_empty() && is_word(method)) {
                let inner_callee = match ic.named_child(0) {
                    Some(f) if f.kind() == "exprDot" => self.identifiers_joined(f),
                    Some(f) if f.kind() == "identifier" => self.text(f).to_string(),
                    _ => String::new(),
                };
                callee = if !inner_callee.is_empty() && is_type_like(&inner_callee) {
                    format!("{inner_callee}().{method}")
                } else {
                    method.to_string()
                };
            } else {
                callee = self.identifiers_joined(first);
            }
        } else if first.kind() == "identifier" {
            callee = self.text(first).to_string();
        }
        let vague = receiver_link && first.kind() == "exprDot" && !callee.contains('.');
        let with_member = self.with_depth > 0 && first.kind() == "identifier" && is_system_name(&callee.to_lowercase());
        if !callee.is_empty() && !vague && !with_member {
            self.push_ref(caller, &callee, "calls", node);
        }
        if first.kind() == "exprDot" {
            self.receiver_calls(first.named_child(0), true);
        } else if first.kind() != "identifier" {
            self.receiver_calls(Some(first), true);
        }
        if let Some(args) = find_named(node, "exprArgs") {
            self.visit_block(args, false);
        }
    }

    /// extractPascalReceiverCalls: the calls inside a call's or member access's receiver.
    fn receiver_calls(&mut self, node: Option<Node<'t>>, names: bool) {
        stack_guard!();
        let Some(node) = node else { return };
        match node.kind() {
            "identifier" => {
                if names {
                    self.parenless_name(node, None);
                }
            }
            "exprCall" => self.receiver_call(node, true),
            "exprDot" => {
                if names {
                    self.parenless_name(node, None);
                }
                self.receiver_calls(node.named_child(0), names);
            }
            "exprTpl" => self.receiver_calls(node.named_child(0), false),
            "exprSubscript" => {
                self.receiver_calls(node.named_child(0), names);
                if let Some(index) = find_named(node, "exprArgs") {
                    self.visit_block(index, false);
                }
            }
            _ => self.visit_block(node, false),
        }
    }

    /// extractPascalReceiverCall: a receiver call, its arguments' routine values included.
    fn receiver_call(&mut self, call: Node<'t>, receiver_link: bool) {
        self.extract_call(call, receiver_link);
        if let Some(args) = find_named(call, "exprArgs") {
            self.maybe_capture_fn_refs(args);
        }
    }

    /// extractPascalParenlessCall: a statement-level `Obj.Method;`.
    fn parenless_call(&mut self, node: Node<'t>) {
        stack_guard!();
        let Some(caller) = self.stack.last().map(|s| s.row) else { return };
        let receiver = node.named_child(0);
        let outer_id = named_children(node).into_iter().filter(|c| c.kind() == "identifier").last();
        let method = outer_id.map(|o| self.text(o)).unwrap_or("");
        if method.is_empty() {
            return;
        }
        let callee;
        match receiver.filter(|r| matches!(r.kind(), "exprDot" | "exprCall")) {
            Some(r) if is_word(method) => {
                let inner_node = if r.kind() == "exprCall" { r.named_child(0) } else { Some(r) };
                let inner = match inner_node {
                    None => String::new(),
                    Some(n) if n.kind() == "identifier" => self.text(n).to_string(),
                    Some(n) => self.identifiers_joined(n),
                };
                if !inner.is_empty() && is_type_like(&inner) {
                    callee = format!("{inner}().{method}");
                    if r.kind() == "exprCall" {
                        self.receiver_call(r, false);
                    } else {
                        self.parenless_call(r);
                    }
                } else {
                    callee = method.to_string();
                    self.receiver_calls(receiver, true);
                }
            }
            _ => {
                callee = self.identifiers_joined(node);
                self.receiver_calls(receiver, true);
            }
        }
        if !callee.is_empty() {
            self.push_ref(caller, &callee, "calls", node);
        }
    }

    /// visitPascalBlock: a block or statement tree's calls; with `declarations`, its
    /// declarations are visited as declarations.
    fn visit_block(&mut self, node: Node<'t>, declarations: bool) {
        stack_guard!();
        for child in named_children(node) {
            let kind = child.kind();
            if declarations && (kind.starts_with("decl") || matches!(kind, "defProc" | "interface" | "implementation")) {
                self.visit_node(child);
                continue;
            }
            self.maybe_capture_fn_refs(child);
            self.declare_inline(child);
            self.learn_variable(child);
            match kind {
                "exprCall" => {
                    self.extract_call(child, false);
                    if let Some(args) = find_named(child, "exprArgs") {
                        self.maybe_capture_fn_refs(args);
                    }
                }
                "exprDot" => {
                    if node.kind() == "statement" {
                        if !self.is_header_name(child) {
                            self.parenless_call(child);
                        }
                    } else {
                        let receiver = child.named_child(0);
                        match receiver {
                            Some(r) if r.kind() == "exprCall" => self.receiver_call(r, false),
                            _ => self.receiver_calls(receiver, true),
                        }
                        self.parenless_name(child, Some(node));
                    }
                }
                "identifier" => self.parenless_name(child, Some(node)),
                "with" => {
                    self.with_depth += 1;
                    self.visit_block(child, declarations);
                    self.with_depth -= 1;
                }
                "lambda" => {
                    let (class_qn, start_line) = match self.pscopes.last() {
                        Some(o) => (o.class_qn.clone(), o.start_line),
                        None => (None, 0),
                    };
                    let locals = self.routine_locals(child, false);
                    self.pscopes.push(PScope { locals, class_qn, routine: None, start_line });
                    self.visit_block(child, declarations);
                    self.pscopes.pop();
                }
                _ => self.visit_block(child, declarations && kind != "block"),
            }
        }
    }

    // --- paren-less names (#2514) ---------------------------------------------

    /// extractPascalParenlessName.
    fn parenless_name(&mut self, name: Node<'t>, parent: Option<Node<'t>>) {
        let Some(scope) = self.scope_at(name.start_position().row as u32 + 1) else { return };
        if self.is_header_name(name) {
            return;
        }
        let Some(caller) = self.stack.last().map(|s| s.row) else { return };
        if name.kind() == "identifier" {
            let lower = self.text(name).to_lowercase();
            if is_not_routine(&lower) || self.local_type(&lower, scope.text).is_some() {
                return;
            }
            if self.is_data_name(scope.class_qn.as_deref(), &lower) {
                return;
            }
        }
        let mode = match parent {
            Some(p) => match self.name_mode(name, p) {
                Some(m) => m,
                None => return,
            },
            None => "recv",
        };
        if self.with_depth > 0 && (mode != "ref" || name.kind() != "identifier") {
            return;
        }
        let mut path: Vec<String> = if name.kind() == "identifier" {
            vec![self.text(name).to_string()]
        } else {
            match self.identifier_path(name) {
                Some(p) => p,
                None => return,
            }
        };
        let lower = path[0].to_lowercase();
        let local = self.local_type(&lower, scope.text);
        let read = mode != "call" && mode != "ref";
        if read && is_data_name_shape(path.last().unwrap()) {
            return;
        }
        if path.len() == 1 {
            if is_not_routine(&lower) || local.is_some() {
                return;
            }
            if read && is_lower_word(&path[0]) {
                return;
            }
            if self.is_data_name(scope.class_qn.as_deref(), &lower) {
                return;
            }
        } else if let Some(local) = local {
            match local {
                Some(t) if mode != "recv" && !(t.len() >= 2 && t.as_bytes()[0] == b'P' && t.as_bytes()[1].is_ascii_uppercase()) => {
                    path[0] = format!(":{t}");
                }
                _ => return,
            }
        } else if lower != "self" && is_not_routine(&lower) {
            return;
        }
        let at = match scope.class_qn.as_deref() {
            Some(c) if !c.is_empty() => format!("@{c}"),
            _ => String::new(),
        };
        let ref_name = format!("{{{mode}{at}}}.{}", path.join("."));
        let line = self.line_of(name);
        let column = self.col_of(name);
        if path.len() == 1 && mode != "call" {
            let key = scope.class_qn.unwrap_or_default();
            self.reads.push((PendingRef { from: caller, name: ref_name, line, column }, key, lower));
        } else {
            self.push_ref_at(caller, &ref_name, "references", line, column);
        }
    }

    /// learnPascalVariable: a bare name assigned to, or passed to `Inc`/`Dec`, is data.
    fn learn_variable(&mut self, node: Node<'t>) {
        if self.with_depth > 0 {
            return;
        }
        let target = match node.kind() {
            "assignment" => node.named_child(0),
            "exprCall" => match node.named_child(0) {
                Some(callee)
                    if callee.kind() == "identifier"
                        && (self.text(callee).eq_ignore_ascii_case("inc") || self.text(callee).eq_ignore_ascii_case("dec")) =>
                {
                    find_named(node, "exprArgs").and_then(|a| a.named_child(0))
                }
                _ => None,
            },
            _ => None,
        };
        let Some(target) = target.filter(|t| t.kind() == "identifier") else { return };
        let Some(scope) = self.scope_at(target.start_position().row as u32 + 1) else { return };
        let name = self.text(target).to_lowercase();
        if scope.routine.as_deref() == Some(name.as_str()) || self.local_type(&name, scope.text).is_some() {
            return;
        }
        self.assigned.entry(scope.class_qn.unwrap_or_default()).or_default().insert(name);
    }

    /// flushPascalReads: the bare names read, less those the file shows are variables.
    fn flush_pascal_reads(&mut self) {
        for (r, scope, name) in std::mem::take(&mut self.reads) {
            if !self.assigned.get(&scope).is_some_and(|s| s.contains(&name)) {
                self.push_ref_at(r.from, &r.name, "references", r.line, r.column);
            }
        }
    }

    /// isPascalHeaderName: the name a routine's header declares, in a broken tree.
    fn is_header_name(&self, node: Node<'t>) -> bool {
        if !self.has_error {
            return false;
        }
        let row = node.start_position().row;
        let start = self.line_starts.get(row).copied().unwrap_or(0);
        let prefix = self.src.get(start..node.start_byte()).unwrap_or("");
        let t = js_trim_start(prefix);
        let core = js_trim_end(t);
        if core.len() == t.len() {
            return false;
        }
        if core.len() > 5 && core.is_char_boundary(5) && core[..5].eq_ignore_ascii_case("class") && core[5..].starts_with(is_js_space) {
            let kw = js_trim_start(&core[5..]);
            if is_routine_keyword(kw) {
                return true;
            }
        }
        is_routine_keyword(core)
    }

    /// pascalLocalType: the declared type of a name the enclosing routines declare.
    fn local_type(&self, lower: &str, text: Option<usize>) -> Option<Option<String>> {
        for s in self.pscopes.iter().rev() {
            if let Some(t) = s.locals.get(lower) {
                return Some(t.clone());
            }
        }
        let routines = self.text_routines.as_ref()?;
        text.and_then(|i| routines[i].locals.get(lower).cloned())
    }

    /// pascalScopeAt: the class and routine a name at `line` belongs to.
    fn scope_at(&mut self, line: u32) -> Option<ScopeView> {
        let scope = self.pscopes.last()?;
        let own = ScopeView { class_qn: scope.class_qn.clone(), routine: scope.routine.clone(), text: None };
        if !self.has_error {
            return Some(own);
        }
        let start_line = scope.start_line;
        if self.text_routines.is_none() {
            self.text_routines = Some(read_text_routines(self.src));
        }
        let routines = self.text_routines.as_ref().unwrap();
        let hit = routines.partition_point(|r| r.line <= line);
        if hit == 0 {
            return Some(own);
        }
        let t = &routines[hit - 1];
        if t.line <= start_line || (start_line > 0 && t.routine.is_none()) {
            return Some(own);
        }
        Some(ScopeView { class_qn: t.class_qn.clone(), routine: t.routine.clone(), text: Some(hit - 1) })
    }

    /// pascalNameMode: how a name in `parent` may run a parameterless routine.
    fn name_mode(&self, name: Node<'t>, parent: Node<'t>) -> Option<&'static str> {
        let is = |field: &str| same(parent.child_by_field_name(field), name);
        match parent.kind() {
            "statement" => (parent.named_child_count() == 1 && name.kind() == "identifier").then_some("call"),
            "exprSubscript" => same(parent.named_child(0), name).then_some("recv"),
            "assignment" => {
                let target = parent.named_child(0)?;
                if target.id() == name.id() {
                    return None;
                }
                let last = if target.kind() == "exprDot" {
                    named_children(target).into_iter().filter(|c| c.kind() == "identifier").last()
                } else {
                    Some(target)
                };
                Some(if last.is_some_and(|l| is_event_name(self.text(l))) { "ref" } else { "read" })
            }
            "exprArgs" => parent
                .parent()
                .is_some_and(|g| matches!(g.kind(), "exprCall" | "exprSubscript" | "inherited"))
                .then_some("read"),
            "exprBinary" => {
                let op = named_children(parent).into_iter().find(|c| c.id() != name.id() && is_keyword_kind(c.kind()));
                if op.is_some_and(|o| matches!(o.kind(), "kIs" | "kAs")) && !same(parent.named_child(0), name) {
                    None
                } else {
                    Some("read")
                }
            }
            "exprUnary" => (parent.child(0).map(|c| c.kind()) != Some("kAt")).then_some("read"),
            "if" | "ifElse" | "while" | "repeat" => is("condition").then_some("read"),
            "case" | "exprParens" => Some("read"),
            "foreach" => is("iterable").then_some("read"),
            "for" => is("end").then_some("read"),
            _ => None,
        }
    }

    /// pascalIdentifierPath: the identifiers of a plain dotted name.
    fn identifier_path(&self, dot: Node<'t>) -> Option<Vec<String>> {
        let mut links: Vec<String> = Vec::new();
        let mut n = Some(dot);
        while let Some(d) = n.filter(|d| d.kind() == "exprDot") {
            let last = last_named(d).filter(|l| l.kind() == "identifier")?;
            links.push(self.text(last).to_string());
            n = d.named_child(0);
        }
        let head = n.filter(|h| h.kind() == "identifier")?;
        links.push(self.text(head).to_string());
        links.reverse();
        Some(links)
    }

    /// pascalDataNames: whether `lower` is data for a routine of class `class_qn`.
    fn is_data_name(&mut self, class_qn: Option<&str>, lower: &str) -> bool {
        let key = class_qn.unwrap_or("").to_string();
        if !self.data_name_sets.contains_key(&key) {
            let prefix = class_qn.filter(|c| !c.is_empty()).map(|c| format!("{}::", c.to_lowercase()));
            let mut names = HashSet::new();
            for n in &self.nodes {
                if n.kind == "enum_member" {
                    names.insert(n.name.to_lowercase());
                    continue;
                }
                if !matches!(n.kind, "field" | "property" | "constant" | "variable") {
                    continue;
                }
                let qn = strip_angles(&n.qualified_name).to_lowercase();
                let own = prefix.as_ref().is_some_and(|p| qn.starts_with(p.as_str()) && !qn[p.len()..].contains("::"));
                if own || !qn.contains("::") {
                    names.insert(n.name.to_lowercase());
                }
            }
            self.data_name_sets.insert(key.clone(), names);
        }
        self.data_name_sets[&key].contains(lower)
    }

    // --- function-as-value (#756): PASCAL_SPEC ---------------------------------

    /// maybeCaptureFnRefs with PASCAL_SPEC. The spec is `addressOfOnly` with no file-scope
    /// value/list positions, so only an explicit form (`@Handler`, an `exprUnary`) can pass the
    /// flush gate: a bare identifier is never captured.
    fn maybe_capture_fn_refs(&mut self, node: Node<'t>) {
        let values: Vec<Node> = match node.kind() {
            "exprArgs" => named_children(node),
            "assignment" => {
                let Some(rhs) = node.child_by_field_name("rhs") else { return };
                let lhs = node
                    .child_by_field_name("left")
                    .or_else(|| node.child_by_field_name("lhs"))
                    .or_else(|| node.child_by_field_name("target"))
                    .or_else(|| if node.named_child_count() >= 2 { node.named_child(0) } else { None });
                let lhs_last = lhs.and_then(|l| trailing_js_ident(self.text(l)));
                if lhs_last.is_some_and(|l| l == js_trim(self.text(rhs))) {
                    return;
                }
                vec![rhs]
            }
            _ => return,
        };
        let Some(from) = self.stack.last().map(|s| s.row) else { return };
        for v in values {
            if v.kind() == "identifier" {
                continue;
            }
            let mut found = Vec::new();
            self.normalize_value(v, 0, &mut found);
            for (name, at) in found {
                if name.is_empty() || is_stoplisted(&name) {
                    continue;
                }
                let line = self.line_of(at);
                let column = self.col_of(at);
                self.fn_ref_cands.push(Cand { from, name, line, column });
            }
        }
    }

    /// normalizeValue with PASCAL_SPEC: an identifier, or an `exprUnary`'s operand.
    fn normalize_value(&self, node: Node<'t>, depth: u32, out: &mut Vec<(String, Node<'t>)>) {
        if depth > 4 {
            return;
        }
        match node.kind() {
            "identifier" => out.push((self.text(node).to_string(), node)),
            "exprUnary" => {
                if let Some(inner) = node.child_by_field_name("operand") {
                    self.normalize_value(inner, depth + 1, out);
                }
            }
            _ => {}
        }
    }

    /// flushFnRefCandidates: a name defined in this file or imported, once per caller.
    fn flush_fn_ref_candidates(&mut self) {
        let cands = std::mem::take(&mut self.fn_ref_cands);
        if cands.is_empty() || util::is_generated_file(self.file_path) {
            return;
        }
        let defined: HashSet<&str> =
            self.nodes.iter().filter(|n| n.kind == "function" || n.kind == "method").map(|n| n.name.as_str()).collect();
        let mut seen: HashSet<(u32, String)> = HashSet::new();
        let mut out = Vec::new();
        for c in cands {
            if !c.name.starts_with("this.") && !c.name.contains("::") && !defined.contains(c.name.as_str()) && !self.imported_names.contains(&c.name) {
                continue;
            }
            // fromNodeId|name: a row stands for one id.
            if !seen.insert((c.from, c.name.clone())) {
                continue;
            }
            out.push(c);
        }
        for c in out {
            let reference_name = self.arena.put(&c.name);
            self.tables.push_ref(&RefRow {
                from_idx: c.from,
                kind: FUNCTION_REF_CODE,
                line: c.line,
                column: c.column,
                reference_name,
                candidates: NONE_STR,
                from_id_str: NONE_STR,
            });
        }
    }

    // --- value refs (VALUE_REF_LANGS has 'pascal') ----------------------------

    fn flush_value_refs(&mut self, root: Node<'t>) {
        let scopes = std::mem::take(&mut self.value_scopes);
        let mut targets = std::mem::take(&mut self.fs_values);
        let counts = std::mem::take(&mut self.fs_value_counts);
        if std::env::var("CODEGRAPHV_VALUE_REFS").as_deref() == Ok("0") {
            return;
        }
        if targets.is_empty() || scopes.is_empty() || util::is_generated_file(self.file_path) {
            return;
        }
        // Shadow prune: a `declConst` / `declVar` of a target's name counts as a binding of it, and
        // so does an `assignment` — the switch's Python case, whose node type Pascal shares: its
        // target, or each named child of a target that is no identifier (`TFoo.Name := Name`).
        let mut decl_counts: HashMap<&str, u32> = HashMap::new();
        let src: &'t str = self.src;
        let bump = |nm: Option<Node>, counts: &mut HashMap<&'t str, u32>| {
            if let Some(nm) = nm.filter(|x| matches!(x.kind(), "identifier" | "simple_identifier")) {
                let t = &src[nm.byte_range()];
                if targets.contains_key(t) {
                    *counts.entry(t).or_insert(0) += 1;
                }
            }
        };
        let mut dstack: Vec<Node> = vec![root];
        let mut dvisited = 0usize;
        while let Some(n) = dstack.pop() {
            if dvisited >= MAX_VALUE_REF_NODES {
                break;
            }
            dvisited += 1;
            match n.kind() {
                "declConst" | "declVar" => bump(n.child_by_field_name("name"), &mut decl_counts),
                "assignment" => {
                    let left = n
                        .child_by_field_name("left")
                        .or_else(|| n.child_by_field_name("pattern"))
                        .or_else(|| n.named_child(0));
                    match left {
                        Some(l) if l.kind() == "identifier" => bump(Some(l), &mut decl_counts),
                        Some(l) => {
                            for c in named_children(l) {
                                bump(Some(c), &mut decl_counts);
                            }
                        }
                        None => {}
                    }
                }
                _ => {}
            }
            for i in 0..n.named_child_count() {
                if let Some(c) = n.named_child(i) {
                    dstack.push(c);
                }
            }
        }
        for (nm, c) in decl_counts {
            if c > counts.get(nm).copied().unwrap_or(1) {
                targets.remove(nm);
            }
        }
        if targets.is_empty() {
            return;
        }
        let refs_kind = edge_kind_index("references").unwrap();
        for scope in &scopes {
            let mut seen: HashSet<u32> = HashSet::new();
            let mut stack: Vec<Node> = vec![scope.node];
            // A Pascal `declProc`'s body is the `block` after it in its `defProc`; a nested
            // routine is stored as its whole `defProc`.
            if scope.node.kind() != "defProc" {
                if let Some(sib) = scope.node.next_named_sibling().filter(|s| matches!(s.kind(), "function_body" | "block")) {
                    stack.push(sib);
                }
            }
            let mut visited = 0usize;
            while let Some(n) = stack.pop() {
                if visited >= MAX_VALUE_REF_NODES {
                    break;
                }
                visited += 1;
                if matches!(n.kind(), "identifier" | "constant" | "name" | "simple_identifier") {
                    let ref_name = self.text(n);
                    if let Some(&target_row) = targets.get(ref_name) {
                        if self.nodes[target_row as usize].id != self.nodes[scope.row as usize].id
                            && ref_name != scope.name
                            && seen.insert(target_row)
                        {
                            let meta = self.arena.put(r#"{"valueRef":true}"#);
                            self.tables.push_edge(&EdgeRow {
                                source_idx: scope.row,
                                target_idx: target_row,
                                kind: refs_kind,
                                provenance: 0,
                                line: NONE,
                                column: NONE,
                                metadata_json: meta,
                                source_id_str: NONE_STR,
                                target_id_str: NONE_STR,
                            });
                        }
                    }
                }
                for i in 0..n.named_child_count() {
                    if let Some(c) = n.named_child(i) {
                        stack.push(c);
                    }
                }
            }
        }
    }
}

/// NAME_STOPLIST (function-ref.ts).
fn is_stoplisted(name: &str) -> bool {
    matches!(name, "this" | "self" | "super" | "null" | "nil" | "true" | "false" | "undefined" | "new" | "NULL" | "nullptr" | "None")
}

/// `text.match(/([A-Za-z_$][A-Za-z0-9_$]*)\s*$/)?.[1]`.
fn trailing_js_ident(text: &str) -> Option<&str> {
    let t = js_trim_end(text);
    let b = t.as_bytes();
    let mut start = b.len();
    while start > 0 && (b[start - 1].is_ascii_alphanumeric() || b[start - 1] == b'_' || b[start - 1] == b'$') {
        start -= 1;
    }
    let run = &t[start..];
    let first = run.find(|c: char| c.is_ascii_alphabetic() || c == '_' || c == '$')?;
    Some(&run[first..])
}

/// `path.basename(filePath).replace(/\.[^.]+$/, '')`.
fn file_stem(file_path: &str) -> String {
    let base = file_path.rsplit('/').next().unwrap_or(file_path);
    match base.rfind('.') {
        Some(i) if i + 1 < base.len() => base[..i].to_string(),
        _ => base.to_string(),
    }
}

// --- readPascalTextRoutines ---------------------------------------------------

/// The header regex of readPascalTextRoutines, from a line's start: `(\s*)(?:class\s+)?
/// (?:procedure|function|constructor|destructor|operator)\s+` and the dotted, generic name
/// after it. Returns (leading whitespace present, name text, byte end of the match).
fn match_header(line: &str) -> Option<(bool, &str, usize)> {
    let lead = line.len() - js_trim_start(line).len();
    let mut i = lead;
    let rest = &line[i..];
    let after_kw = |s: &str| -> Option<usize> {
        for kw in ["procedure", "function", "constructor", "destructor", "operator"] {
            if s.len() >= kw.len() && s.is_char_boundary(kw.len()) && s[..kw.len()].eq_ignore_ascii_case(kw) {
                let r = &s[kw.len()..];
                let ws = r.len() - js_trim_start(r).len();
                if ws > 0 {
                    return Some(kw.len() + ws);
                }
            }
        }
        None
    };
    // `(?:class\s+)?` first, falling back to none.
    let mut kw_end = None;
    if rest.len() > 5 && rest.is_char_boundary(5) && rest[..5].eq_ignore_ascii_case("class") {
        let r = &rest[5..];
        let ws = r.len() - js_trim_start(r).len();
        if ws > 0 {
            if let Some(k) = after_kw(&r[ws..]) {
                kw_end = Some(5 + ws + k);
            }
        }
    }
    if kw_end.is_none() {
        kw_end = after_kw(rest);
    }
    i += kw_end?;
    let name_start = i;
    let b = line.as_bytes();
    let ident = |mut j: usize| -> Option<usize> {
        if j < b.len() && (b[j].is_ascii_alphabetic() || b[j] == b'_') {
            j += 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            Some(j)
        } else {
            None
        }
    };
    // `(?:<[^>]*>)?` after an identifier.
    let generic = |j: usize| -> usize {
        if j < b.len() && b[j] == b'<' {
            if let Some(k) = line[j + 1..].find('>') {
                return j + 1 + k + 1;
            }
        }
        j
    };
    let mut j = generic(ident(i)?);
    loop {
        if j < b.len() && b[j] == b'.' {
            if let Some(k) = ident(j + 1) {
                j = generic(k);
                continue;
            }
        }
        break;
    }
    Some((lead > 0, &line[name_start..j], j))
}

/// `/^\s*(?:begin|asm)\b/i`.
fn starts_body(line: &str) -> bool {
    let t = js_trim_start(line);
    for kw in ["begin", "asm"] {
        if t.len() >= kw.len() && t.is_char_boundary(kw.len()) && t[..kw.len()].eq_ignore_ascii_case(kw) {
            let next = t.as_bytes().get(kw.len()).copied();
            if !next.is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_') {
                return true;
            }
        }
    }
    false
}

/// `/^(?:end;|\s*(?:initialization|finalization)\b)/i`.
fn resumes_unit(line: &str) -> bool {
    if line.len() >= 4 && line.is_char_boundary(4) && line[..4].eq_ignore_ascii_case("end;") {
        return true;
    }
    let t = js_trim_start(line);
    for kw in ["initialization", "finalization"] {
        if t.len() >= kw.len() && t.is_char_boundary(kw.len()) && t[..kw.len()].eq_ignore_ascii_case(kw) {
            let next = t.as_bytes().get(kw.len()).copied();
            if !next.is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_') {
                return true;
            }
        }
    }
    false
}

/// `decl.replace(/\{[^}]*\}|\(\*[\s\S]*?\*\)|\/\/[^\n]*/g, ' ')`.
fn blank_comments(decl: &str) -> String {
    let mut out = String::with_capacity(decl.len());
    let mut i = 0;
    let b = decl.as_bytes();
    while i < b.len() {
        if b[i] == b'{' {
            if let Some(k) = decl[i + 1..].find('}') {
                out.push(' ');
                i = i + 1 + k + 1;
                continue;
            }
        } else if b[i] == b'(' && b.get(i + 1) == Some(&b'*') {
            if let Some(k) = decl[i + 2..].find("*)") {
                out.push(' ');
                i = i + 2 + k + 2;
                continue;
            }
        } else if b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
            let k = decl[i..].find('\n').map_or(decl.len(), |k| i + k);
            out.push(' ');
            i = k;
            continue;
        }
        let c = decl[i..].chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

fn ident_end(b: &[u8], mut j: usize) -> Option<usize> {
    if j < b.len() && (b[j].is_ascii_alphabetic() || b[j] == b'_') {
        j += 1;
        while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
            j += 1;
        }
        Some(j)
    } else {
        None
    }
}

fn skip_js_ws(s: &str, mut j: usize) -> usize {
    while let Some(c) = s[j..].chars().next() {
        if !is_js_space(c) {
            break;
        }
        j += c.len_utf8();
    }
    j
}

/// `[A-Za-z_][\w.]*` from `j`.
fn dotted_end(b: &[u8], mut j: usize) -> Option<usize> {
    if j < b.len() && (b[j].is_ascii_alphabetic() || b[j] == b'_') {
        j += 1;
        while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'.') {
            j += 1;
        }
        Some(j)
    } else {
        None
    }
}

/// `/^\s*(?:\([^)]*\))?\s*:\s*([A-Za-z_][\w.]*)/.exec(decl)?.[1]`.
fn result_type(decl: &str) -> Option<&str> {
    let b = decl.as_bytes();
    let start = skip_js_ws(decl, 0);
    let attempt = |from: usize| -> Option<&str> {
        let j = skip_js_ws(decl, from);
        if b.get(j) != Some(&b':') {
            return None;
        }
        let k = skip_js_ws(decl, j + 1);
        let e = dotted_end(b, k)?;
        Some(&decl[k..e])
    };
    if b.get(start) == Some(&b'(') {
        if let Some(close) = decl[start + 1..].find(')') {
            if let Some(t) = attempt(start + 1 + close + 1) {
                return Some(t);
            }
        }
    }
    attempt(start)
}

/// Every match of `/([A-Za-z_]\w*(?:\s*,\s*[A-Za-z_]\w*)*)\s*:(?!=)\s*([A-Za-z_][\w.]*)?/g`:
/// (the names, the type).
fn declared_names(decl: &str) -> Vec<(Vec<&str>, Option<&str>)> {
    let b = decl.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let Some(first_end) = ident_end(b, i) else {
            i += decl[i..].chars().next().map_or(1, |c| c.len_utf8());
            continue;
        };
        // The list's elements, greedily: each element's [start, end).
        let mut elems = vec![(i, first_end)];
        let mut j = first_end;
        loop {
            let k = skip_js_ws(decl, j);
            if b.get(k) != Some(&b',') {
                break;
            }
            let k = skip_js_ws(decl, k + 1);
            match ident_end(b, k) {
                Some(e) => {
                    elems.push((k, e));
                    j = e;
                }
                None => break,
            }
        }
        // Backtrack over the list's length until `\s*:(?!=)` follows.
        let mut matched = None;
        for n in (1..=elems.len()).rev() {
            let end = elems[n - 1].1;
            let c = skip_js_ws(decl, end);
            if b.get(c) == Some(&b':') && b.get(c + 1) != Some(&b'=') {
                matched = Some((n, c + 1));
                break;
            }
        }
        let Some((n, after_colon)) = matched else {
            i += 1;
            continue;
        };
        let names_text = &decl[i..elems[n - 1].1];
        let k = skip_js_ws(decl, after_colon);
        let (ty, end) = match dotted_end(b, k) {
            Some(e) => (Some(&decl[k..e]), e),
            None => (None, k),
        };
        out.push((names_text.split(',').collect(), ty));
        i = end.max(i + 1);
    }
    out
}

/// readPascalTextRoutines: each routine header of the source text, with its class, name and
/// what it declares before its body, and where the unit's own code resumes.
fn read_text_routines(src: &str) -> Vec<TextRoutine> {
    let lines: Vec<&str> = split_lines(src);
    let mut routines: Vec<TextRoutine> = Vec::new();
    let mut outer: Option<usize> = None;
    for (i, text) in lines.iter().enumerate() {
        if resumes_unit(text) {
            routines.push(TextRoutine { line: i as u32 + 1, class_qn: None, routine: None, locals: HashMap::new() });
            outer = None;
            continue;
        }
        let Some((indented, name, end)) = match_header(text) else { continue };
        let nested = indented && outer.is_some();
        let stripped = strip_angles(name);
        let mut parts: Vec<&str> = stripped.split('.').collect();
        let routine = parts.pop().unwrap_or("").to_lowercase();
        let mut decl = text[end..].to_string();
        let mut j = i + 1;
        while j < lines.len() && j <= i + 200 && !starts_body(lines[j]) && match_header(lines[j]).is_none() {
            decl.push('\n');
            decl.push_str(lines[j]);
            j += 1;
        }
        let decl = blank_comments(&decl);
        let mut locals: HashMap<String, Option<String>> = match (nested, outer) {
            (true, Some(o)) => routines[o].locals.clone(),
            _ => HashMap::new(),
        };
        locals.insert("result".to_string(), result_type(&decl).map(|t| t.rsplit('.').next().unwrap_or("").to_string()));
        for (names, ty) in declared_names(&decl) {
            let ty = ty.map(|t| t.rsplit('.').next().unwrap_or("").to_string());
            for n in names {
                locals.insert(js_trim(n).to_lowercase(), ty.clone());
            }
        }
        if nested {
            let o = &routines[outer.unwrap()];
            let (class_qn, routine) = (o.class_qn.clone(), o.routine.clone());
            routines.push(TextRoutine { line: i as u32 + 1, class_qn, routine, locals });
        } else {
            let class_qn = (!parts.is_empty()).then(|| parts.join("::"));
            routines.push(TextRoutine { line: i as u32 + 1, class_qn, routine: Some(routine), locals });
            outer = Some(routines.len() - 1);
        }
    }
    routines
}

/// `source.split(/\r?\n/)`.
fn split_lines(src: &str) -> Vec<&str> {
    src.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_helpers_follow_the_js_regexes() {
        assert_eq!(trailing_js_ident("Self.FOnChange "), Some("FOnChange"));
        assert_eq!(trailing_js_ident("x.9b"), Some("b"));
        assert_eq!(trailing_js_ident("List[0]"), None);
        assert_eq!(strip_angles("TList<T>.Add<U>"), "TList.Add");
        assert_eq!(strip_angles("a<b"), "a<b");
        assert_eq!(file_stem("src/Edge.pas"), "Edge");
        assert_eq!(result_type("(const A: string): TFoo.TBar;"), Some("TFoo.TBar"));
        assert_eq!(result_type(": Integer;"), Some("Integer"));
        let names = declared_names("(const A, B: string; var C: Integer): Boolean;\nvar\n  I := 0; S: TStrings;");
        let flat: Vec<(Vec<&str>, Option<&str>)> = names;
        assert_eq!(flat[0], (vec!["A", " B"], Some("string")));
        assert_eq!(flat[1], (vec!["C"], Some("Integer")));
        assert!(flat.iter().any(|(n, t)| n == &vec!["S"] && *t == Some("TStrings")));
        assert!(match_header("  class procedure TFoo<T>.Bar(X: Integer);").is_some_and(|(ind, n, _)| ind && n == "TFoo<T>.Bar"));
        assert!(match_header("function Helper: Integer;").is_some_and(|(ind, n, _)| !ind && n == "Helper"));
        assert!(match_header("procedurex Foo;").is_none());
    }

    #[test]
    fn units_routines_calls_and_function_values() {
        let src = "unit Edge;\ninterface\nuses System.SysUtils;\nconst MAX_ITEMS = 100;\ntype\n  TShape = class(TObject, IShape)\n  private\n    FName: string;\n    procedure DoChange;\n  public\n    constructor Create(const [ref] AName: string);\n  end;\nfunction Helper(X: Integer): Integer;\nimplementation\nfunction Helper(X: Integer): Integer;\nbegin\n  Result := X + MAX_ITEMS;\nend;\nconstructor TShape.Create(const [ref] AName: string);\n  procedure Inner;\n  begin\n    Helper(1);\n  end;\nbegin\n  Inner;\n  FName := AName;\n  Register(@DoChange);\n  Self.DoChange;\nend;\nprocedure TShape.DoChange;\nbegin\nend;\nend.\n";
        let r = crate::extract_source("Edge.pas", src, codegraph_types::Language::parse("pascal").unwrap()).unwrap();
        let has = |kind: &str, qn: &str| r.nodes.iter().any(|n| n.kind.as_str() == kind && n.qualified_name == qn);
        assert!(has("module", "Edge"));
        assert!(has("class", "TShape"));
        assert!(has("method", "TShape::Create"));
        assert!(has("function", "Helper"));
        assert!(has("function", "TShape::Create::Inner"));
        assert!(has("constant", "MAX_ITEMS"));
        let ctor = r.nodes.iter().find(|n| n.qualified_name == "TShape::Create").unwrap();
        assert_eq!(ctor.signature.as_deref(), Some("(const [ref] AName: string)"));
        let refs: Vec<(&str, &str)> =
            r.unresolved_references.iter().map(|x| (x.reference_kind.as_str(), x.reference_name.as_str())).collect();
        assert!(refs.contains(&("extends", "TObject")));
        assert!(refs.contains(&("implements", "IShape")));
        assert!(refs.contains(&("imports", "System.SysUtils")));
        assert!(refs.contains(&("calls", "Helper")));
        assert!(refs.contains(&("calls", "Self.DoChange")));
        assert!(refs.contains(&("function_ref", "DoChange")));
        assert!(refs.contains(&("references", "{call@TShape}.Inner")));
    }
}
