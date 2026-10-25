//! Erlang extraction — a port of languages/erlang.ts over the generic walk of
//! `TreeSitterExtractor` (src/extraction/tree-sitter.ts), with the erlang branch of its
//! `extractCall`.
//!
//! Every symbol-bearing form goes through the `visitNode` hook: a function's clauses (one
//! `fun_decl` each) merge into one node per name/arity, qualified `mod::f/2`; `-spec` and
//! `-callback` bodies are consumed (their types parse as calls); records keep their fields.
//! Calls carry the call-site arity (`f/2`, `mod::f/2`). Erlang has no function-as-value or
//! value-ref pass. The grammar is the TypeScript engine's own WhatsApp tree-sitter-erlang wasm.

use crate::buffers::{
    build_meta, edge_kind_index, node_kind_index, Arena, BoolFlags, EdgeRow, EmitOut, NodeRow,
    RefRow, StrRef, Tables, FLAG_IS_EXPORTED, NONE, NONE_STR,
};
use crate::ids;
use crate::textutil as util;
use crate::ts::Node;
use codegraph_types::jscompat::{is_js_space, js_trim};
use std::collections::{HashMap, HashSet};

/// Compiler-predefined macros: no `-define` exists to link a use to.
const PREDEFINED_MACROS: [&str; 10] = [
    "MODULE", "MODULE_STRING", "FILE", "LINE", "MACHINE", "FUNCTION_NAME", "FUNCTION_ARITY", "OTP_RELEASE",
    "FEATURE_AVAILABLE", "FEATURE_ENABLED",
];

/// Calls that take their real callee as (Module, Function, Args) arguments.
const MFA_CALLS: [&str; 22] = [
    "spawn", "spawn_link", "spawn_monitor", "spawn_opt", "apply",
    "erlang:spawn", "erlang:spawn_link", "erlang:spawn_monitor", "erlang:spawn_opt", "erlang:apply",
    "proc_lib:spawn", "proc_lib:spawn_link", "proc_lib:spawn_opt", "proc_lib:start", "proc_lib:start_link",
    "timer:apply_after", "timer:apply_interval",
    "rpc:call", "rpc:cast", "rpc:async_call",
    "erpc:call", "erpc:cast",
];

const CALL_TYPES: [&str; 8] = [
    "call", "internal_fun", "external_fun", "record_expr", "record_update_expr", "record_index_expr",
    "record_field_expr", "macro_call_expr",
];

const IMPORT_TYPES: [&str; 3] = ["import_attribute", "pp_include", "pp_include_lib"];

struct Scope {
    row: u32,
    is_file: bool,
    name: String,
}

enum Exports {
    All,
    Keys(HashSet<String>),
}

/// The `-define` tables `gen_server` targets are read through.
struct ServerMacros {
    own: HashSet<String>,
    atoms: HashMap<String, String>,
}

struct Walker<'t> {
    src: &'t str,
    file_path: &'t str,
    line_starts: Vec<usize>,
    arena: Arena,
    node_id_allocator: ids::NodeIdAllocator,
    tables: Tables,
    /// Node rows, written to the table at the end: a merged clause extends its function's end line.
    nodes: Vec<NodeRow>,
    stack: Vec<Scope>,
    root: Node<'t>,
    exports: Option<Exports>,
    /// The last function created: (name, arity, row). A fun_decl repeating its name and arity
    /// is another clause of it.
    last_fn: Option<(String, usize, u32)>,
    server_macros: Option<ServerMacros>,
}

pub fn extract(file_path: &str, source: &str) -> Result<EmitOut, String> {
    let grammar = crate::langs::grammar_for("erlang").ok_or("no erlang grammar")?;
    let t0 = std::time::Instant::now();
    let tree = crate::ts::parse(&grammar, "erlang", source)?;
    let root = tree.root_node();
    let mut w = Walker {
        src: source,
        file_path,
        line_starts: util::line_starts(source),
        arena: Arena::default(),
        node_id_allocator: ids::NodeIdAllocator::default(),
        tables: Tables::default(),
        nodes: Vec::new(),
        stack: Vec::new(),
        root,
        exports: None,
        last_fn: None,
        server_macros: None,
    };

    // File node (tree-sitter.ts extract()).
    let line_count = source.bytes().filter(|b| *b == b'\n').count() as u32 + 1;
    let base_name = file_path.rsplit(['/', '\\']).next().unwrap_or(file_path);
    let mut flags = BoolFlags::default();
    flags.set(FLAG_IS_EXPORTED, false);
    let file_id = w.arena.put(&ids::file_node_id(file_path));
    let name_ref = w.arena.put(base_name);
    let qn_ref = w.arena.put(file_path);
    w.nodes.push(NodeRow {
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
    w.stack.push(Scope { row: 0, is_file: true, name: base_name.to_string() });

    // extractFilePackage: `-module(m)` wraps the file's declarations in a namespace.
    let package = named_children(root).into_iter().find(|c| c.kind() == "module_attribute");
    let mut pushed_package = false;
    if let Some(pkg) = package {
        if let Some(name) = pkg.child_by_field_name("name").map(|n| w.atom_text(n)) {
            if let Some(row) = w.create_node("namespace", &name, pkg, NodeExtra::default()) {
                w.stack.push(Scope { row, is_file: false, name });
                pushed_package = true;
            }
        }
    }
    w.visit(root);
    if pushed_package {
        w.stack.pop();
    }
    w.stack.pop();

    for row in std::mem::take(&mut w.nodes) {
        w.tables.push_node(&row);
    }
    let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let meta = build_meta(&w.tables, w.arena.len(), NONE_STR, duration_ms);
    Ok(EmitOut { meta, nodes: w.tables.nodes, edges: w.tables.edges, refs: w.tables.refs, arena: w.arena.into_vec() })
}

#[derive(Default)]
struct NodeExtra {
    docstring: Option<String>,
    signature: Option<String>,
    exported: Option<bool>,
}

fn named_children<'t>(node: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

/// `text.replace(/\s+/g, ' ').trim()`.
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
    js_trim(&out).to_string()
}

/// The first `n` UTF-16 units of `text` (`String.prototype.slice(0, n)`).
fn prefix16(text: &str, n: usize) -> String {
    util::slice_utf16(text, n).0
}

/// namedArgCount (tree-sitter-helpers.ts): named children that are not comments.
fn named_arg_count(args: Option<Node>) -> usize {
    args.map_or(0, |a| named_children(a).iter().filter(|c| c.kind() != "comment").count())
}

/// nodeArity (erlang.ts): the arguments of a clause's or a sig's `args` field.
fn node_arity(with_args: Node) -> usize {
    named_arg_count(with_args.child_by_field_name("args"))
}

fn is_app_resource_file(file_path: &str) -> bool {
    let lower = file_path.to_ascii_lowercase();
    lower.ends_with(".app") || lower.ends_with(".app.src")
}

impl<'t> Walker<'t> {
    fn text(&self, node: Node) -> &'t str {
        &self.src[node.byte_range()]
    }
    /// atomText (erlang.ts): an atom's text with quoted-atom quotes stripped.
    fn atom_text(&self, node: Node) -> String {
        let text = self.text(node);
        if text.len() >= 2 && text.starts_with('\'') && text.ends_with('\'') {
            text[1..text.len() - 1].to_string()
        } else {
            text.to_string()
        }
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

    fn push_ref(&mut self, name: &str, kind: &str, at: Node) {
        let from_idx = self.top_row();
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

    fn qualified(&self, name: &str) -> String {
        let mut parts: Vec<&str> = self.stack.iter().filter(|s| !s.is_file).map(|s| s.name.as_str()).collect();
        parts.push(name);
        parts.join("::")
    }

    /// createNode (tree-sitter.ts) with the extras erlang.ts passes.
    fn create_node(&mut self, kind: &'static str, name: &str, node: Node<'t>, extra: NodeExtra) -> Option<u32> {
        if name.is_empty() {
            return None;
        }
        let start_line = self.line_of(node);
        let column = self.col_of(node);
        let id = self.node_id_allocator.generate(self.file_path, kind, name, start_line, column);
        let qualified = self.qualified(name);
        let mut flags = BoolFlags::default();
        if let Some(exported) = extra.exported {
            flags.set(FLAG_IS_EXPORTED, exported);
        }
        let name_ref = self.arena.put(name);
        let qn_ref = self.arena.put(&qualified);
        let id_ref = self.arena.put(&id);
        let doc_ref: StrRef = self.arena.put_opt(extra.docstring.as_deref());
        let sig_ref: StrRef = self.arena.put_opt(extra.signature.as_deref());
        let row = self.nodes.len() as u32;
        self.nodes.push(NodeRow {
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

    /// dispatchNode (tree-sitter.ts) as it runs for Erlang: the hook, then imports, then calls,
    /// then the named children.
    fn visit(&mut self, node: Node<'t>) {
        stack_guard!();
        if self.hook(node) {
            return;
        }
        let kind = node.kind();
        if IMPORT_TYPES.contains(&kind) {
            self.extract_import(node);
        } else if CALL_TYPES.contains(&kind) {
            self.extract_call(node);
        }
        for child in named_children(node) {
            self.visit(child);
        }
    }

    /// visitFunctionBody (tree-sitter.ts) as it runs for Erlang: every call-type node, all the
    /// way down.
    fn visit_body(&mut self, node: Node<'t>) {
        stack_guard!();
        if CALL_TYPES.contains(&node.kind()) {
            self.extract_call(node);
        }
        for child in named_children(node) {
            self.visit_body(child);
        }
    }

    /// The visitNode hook (erlang.ts). Returns true when the subtree is consumed.
    fn hook(&mut self, node: Node<'t>) -> bool {
        match node.kind() {
            "fun_decl" => self.fun_decl(node),
            "record_decl" => self.record_decl(node),
            "type_alias" | "opaque" => {
                let name = node.child_by_field_name("name").and_then(|t| t.child_by_field_name("name"));
                if let Some(name) = name {
                    let name = self.atom_text(name);
                    let signature = prefix16(&collapse_ws(self.text(node)), 200);
                    self.create_node("type_alias", &name, node, NodeExtra { signature: Some(signature), ..Default::default() });
                }
                true
            }
            "pp_define" => self.pp_define(node),
            "behaviour_attribute" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let name = self.atom_text(name);
                    self.push_ref(&name, "implements", node);
                }
                true
            }
            "spec" | "callback" => true,
            "tuple" => {
                let is_app = node.parent().is_some_and(|p| p.kind() == "source_file")
                    && is_app_resource_file(self.file_path)
                    && node.named_child(0).is_some_and(|a| a.kind() == "atom" && self.atom_text(a) == "application");
                if is_app {
                    self.app_resource_tuple(node);
                }
                is_app
            }
            _ => false,
        }
    }

    /// moduleExports (erlang.ts): the file's exported `name/arity` keys, read once.
    fn is_exported(&mut self, name: &str, arity: usize) -> bool {
        if self.exports.is_none() {
            let mut keys = HashSet::new();
            let mut all = false;
            for form in named_children(self.root) {
                if form.kind() == "compile_options_attribute" && self.text(form).contains("export_all") {
                    all = true;
                    break;
                }
                if form.kind() == "export_attribute" {
                    for fa in named_children(form) {
                        if fa.kind() != "fa" {
                            continue;
                        }
                        let Some(fun) = fa.child_by_field_name("fun") else { continue };
                        let fun_name = self.atom_text(fun);
                        let arity = fa.child_by_field_name("arity").and_then(|a| a.child_by_field_name("value"));
                        keys.insert(match arity {
                            Some(a) => format!("{fun_name}/{}", self.text(a)),
                            None => fun_name,
                        });
                    }
                }
            }
            self.exports = Some(if all { Exports::All } else { Exports::Keys(keys) });
        }
        match self.exports.as_ref().unwrap() {
            Exports::All => true,
            Exports::Keys(keys) => keys.contains(&format!("{name}/{arity}")) || keys.contains(name),
        }
    }

    /// precedingSpec (erlang.ts): the -spec right above a function (comments between) naming it
    /// at its arity.
    fn preceding_spec(&self, node: Node<'t>, name: &str, arity: usize) -> Option<Node<'t>> {
        let mut prev = node.prev_named_sibling();
        while let Some(p) = prev.filter(|p| p.kind() == "comment") {
            prev = p.prev_named_sibling();
        }
        let prev = prev.filter(|p| p.kind() == "spec")?;
        let fun = prev.child_by_field_name("fun")?;
        if self.atom_text(fun) != name {
            return None;
        }
        let sigs: Vec<Node> = named_children(prev).into_iter().filter(|c| c.kind() == "type_sig").collect();
        (sigs.is_empty() || sigs.iter().any(|s| node_arity(*s) == arity)).then_some(prev)
    }

    /// handleFunDecl (erlang.ts).
    fn fun_decl(&mut self, node: Node<'t>) -> bool {
        let clauses: Vec<Node<'t>> = named_children(node).into_iter().filter(|c| c.kind() == "function_clause").collect();
        let Some(&first) = clauses.first() else { return true };
        let Some(name_node) = first.child_by_field_name("name") else { return true };
        let name = self.atom_text(name_node);
        if name.is_empty() {
            return true;
        }
        let arity = node_arity(first);

        if let Some((_, _, row)) = self.last_fn.as_ref().filter(|(n, a, _)| *n == name && *a == arity) {
            let row = *row;
            let end_line = node.end_position().row as u32 + 1;
            let existing = &mut self.nodes[row as usize];
            if end_line > existing.end_line {
                existing.end_line = end_line;
            }
            self.walk_clauses(row, &name, &clauses);
            return true;
        }

        let spec = self.preceding_spec(node, &name, arity);
        let exported = self.is_exported(&name, arity);
        let docstring = crate::docstring::preceding_docstring(spec.unwrap_or(node), self.src);
        let signature = match spec {
            Some(spec) => Some(prefix16(&collapse_ws(self.text(spec)), 300)),
            None => {
                let end = first.child_by_field_name("body").map_or(first.end_byte(), |b| b.start_byte());
                Some(collapse_ws(&self.src[first.start_byte()..end])).filter(|s| !s.is_empty())
            }
        };
        let Some(row) = self.create_node("function", &name, node, NodeExtra { docstring, signature, exported: Some(exported) }) else {
            return true;
        };
        // Arity is part of an Erlang function's identity: `mod::f/2`. The name stays bare.
        let qualified = format!("{}/{arity}", self.qualified(&name));
        self.nodes[row as usize].qualified_name = self.arena.put(&qualified);
        self.walk_clauses(row, &name, &clauses);
        self.last_fn = Some((name, arity, row));
        true
    }

    /// Each clause walked whole (arguments and guards too) with the function as the scope.
    fn walk_clauses(&mut self, row: u32, name: &str, clauses: &[Node<'t>]) {
        self.stack.push(Scope { row, is_file: false, name: name.to_string() });
        for clause in clauses {
            self.visit_body(*clause);
        }
        self.stack.pop();
    }

    /// handleRecordDecl (erlang.ts): the record and its fields; types and defaults not walked.
    fn record_decl(&mut self, node: Node<'t>) -> bool {
        let Some(name_node) = node.child_by_field_name("name") else { return true };
        let name = self.atom_text(name_node);
        let docstring = crate::docstring::preceding_docstring(node, self.src);
        let signature = Some(prefix16(&collapse_ws(self.text(node)), 300));
        if let Some(row) = self.create_node("struct", &name, node, NodeExtra { docstring, signature, exported: None }) {
            self.stack.push(Scope { row, is_file: false, name });
            for field in named_children(node) {
                if field.kind() != "record_field" {
                    continue;
                }
                if let Some(field_name) = field.child_by_field_name("name") {
                    let field_name = self.atom_text(field_name);
                    self.create_node("field", &field_name, field, NodeExtra::default());
                }
            }
            self.stack.pop();
        }
        true
    }

    /// handlePpDefine (erlang.ts): the macro is a constant; its replacement's calls are its own.
    fn pp_define(&mut self, node: Node<'t>) -> bool {
        let Some(name_node) = node.child_by_field_name("lhs").and_then(|l| l.child_by_field_name("name")) else {
            return true;
        };
        let name = self.text(name_node).to_string();
        let signature = Some(prefix16(&collapse_ws(self.text(node)), 200));
        let row = self.create_node("constant", &name, node, NodeExtra { signature, ..Default::default() });
        if let (Some(row), Some(replacement)) = (row, node.child_by_field_name("replacement")) {
            self.stack.push(Scope { row, is_file: false, name });
            self.visit_body(replacement);
            self.stack.pop();
        }
        true
    }

    /// handleAppResourceTuple (erlang.ts): `{mod, {M, _}}` and the apps an `.app.src` depends on.
    fn app_resource_tuple(&mut self, node: Node<'t>) {
        let Some(props) = node.named_child(2).filter(|p| p.kind() == "list") else { return };
        for prop in named_children(props) {
            if prop.kind() != "tuple" || prop.named_child_count() < 2 {
                continue;
            }
            let (Some(key), Some(value)) = (prop.named_child(0), prop.named_child(1)) else { continue };
            if key.kind() != "atom" {
                continue;
            }
            let key_name = self.atom_text(key);
            if key_name == "mod" && value.kind() == "tuple" {
                if let Some(module) = value.named_child(0).filter(|m| m.kind() == "atom") {
                    self.app_ref(module, "references");
                }
            } else if (key_name == "applications" || key_name == "included_applications") && value.kind() == "list" {
                for app in named_children(value) {
                    if app.kind() == "atom" {
                        self.app_ref(app, "imports");
                    }
                }
            }
        }
    }

    fn app_ref(&mut self, name_node: Node<'t>, kind: &str) {
        let name = self.atom_text(name_node);
        if !name.is_empty() {
            self.push_ref(&name, kind, name_node);
        }
    }

    /// extractImport (tree-sitter.ts) with erlang.ts's hook: an import node and an `imports` ref.
    fn extract_import(&mut self, node: Node<'t>) {
        let (module_name, signature) = if node.kind() == "import_attribute" {
            let Some(module) = node.child_by_field_name("module") else { return };
            (self.atom_text(module), prefix16(&collapse_ws(self.text(node)), 200))
        } else {
            let Some(file) = node.child_by_field_name("file") else { return };
            let text = self.text(file);
            let text = text.strip_prefix('"').unwrap_or(text);
            let text = text.strip_suffix('"').unwrap_or(text);
            if text.is_empty() {
                return;
            }
            (text.to_string(), js_trim(self.text(node)).to_string())
        };
        self.create_node("import", &module_name, node, NodeExtra { signature: Some(signature), ..Default::default() });
        if !module_name.is_empty() {
            self.push_ref(&module_name, "imports", node);
        }
    }

    fn macro_name_is(&self, node: Node, name: &str) -> bool {
        node.kind() == "macro_call_expr" && node.child_by_field_name("name").is_some_and(|n| self.text(n) == name)
    }

    /// The erlang branch of extractCall (tree-sitter.ts).
    fn extract_call(&mut self, node: Node<'t>) {
        match node.kind() {
            "call" => self.call(node),
            "internal_fun" | "external_fun" => {
                let Some(fun) = node.child_by_field_name("fun").filter(|f| f.kind() == "atom") else { return };
                let mut name = self.atom_text(fun);
                if node.kind() == "external_fun" {
                    let module = node.child_by_field_name("module").and_then(|m| m.child_by_field_name("name"));
                    let Some(module) = module.filter(|m| m.kind() == "atom") else { return };
                    name = format!("{}::{name}", self.atom_text(module));
                }
                if let Some(arity) = node.child_by_field_name("arity").and_then(|a| a.child_by_field_name("value")) {
                    name = format!("{name}/{}", self.text(arity));
                }
                self.push_ref(&name, "references", node);
            }
            "macro_call_expr" => {
                let Some(name) = node.child_by_field_name("name") else { return };
                let name = self.text(name);
                if PREDEFINED_MACROS.contains(&name) {
                    return;
                }
                let kind = if node.child_by_field_name("args").is_some() { "calls" } else { "references" };
                self.push_ref(name, kind, node);
            }
            _ => {
                let record = node
                    .child_by_field_name("name")
                    .filter(|n| n.kind() == "record_name")
                    .and_then(|n| n.child_by_field_name("name"))
                    .filter(|a| a.kind() == "atom");
                if let Some(record) = record {
                    let name = self.atom_text(record);
                    self.push_ref(&name, "references", node);
                }
            }
        }
    }

    fn call(&mut self, node: Node<'t>) {
        let mut callee = node.child_by_field_name("expr");
        let mut module_node = None;
        if let Some(parent) = node.parent().filter(|p| p.kind() == "remote") {
            module_node = parent.child_by_field_name("module");
        } else if let Some(remote) = callee.filter(|c| c.kind() == "remote") {
            module_node = remote.child_by_field_name("module");
            callee = remote.child_by_field_name("fun");
        }
        let Some(callee) = callee.filter(|c| c.kind() == "atom") else { return };
        let fn_bare = self.atom_text(callee);
        let mut callee_name = fn_bare.clone();
        let module_expr = module_node.and_then(|m| m.child_by_field_name("module"));
        let module_atom = module_expr.filter(|m| m.kind() == "atom").map(|m| self.atom_text(m));
        if let Some(module) = &module_atom {
            callee_name = format!("{module}::{callee_name}");
        } else if let Some(expr) = module_expr {
            // `?MODULE:f(X)` keeps the bare name; any other dynamic module stays silent.
            if !self.macro_name_is(expr, "MODULE") {
                return;
            }
        }
        let args = node.child_by_field_name("args");
        let arity = named_arg_count(args);
        self.push_ref(&format!("{callee_name}/{arity}"), "calls", node);

        // gen_server dispatch onto the target module's handler.
        if module_atom.as_deref() == Some("gen_server") && matches!(fn_bare.as_str(), "call" | "cast" | "send_request") {
            let target = args.and_then(|a| a.named_child(0));
            if let Some(module) = target.and_then(|t| self.gen_server_target(t)) {
                let handler = if fn_bare == "cast" { "handle_cast/2" } else { "handle_call/3" };
                self.push_ref(&format!("{module}::{handler}"), "calls", node);
            }
        }

        // The spawn/apply family: lift a static (Module, Function) argument pair.
        let family = match &module_atom {
            Some(module) => format!("{module}:{fn_bare}"),
            None => fn_bare.clone(),
        };
        if MFA_CALLS.contains(&family.as_str()) {
            let exprs = args.map(named_children).unwrap_or_default();
            for i in 0..exprs.len().saturating_sub(1) {
                let (m, f) = (exprs[i], exprs[i + 1]);
                if f.kind() != "atom" {
                    continue;
                }
                let is_local_module = self.macro_name_is(m, "MODULE");
                if m.kind() != "atom" && !is_local_module {
                    continue;
                }
                let arity_tail = match exprs.get(i + 2).filter(|l| l.kind() == "list") {
                    Some(list) => format!("/{}", named_arg_count(Some(*list))),
                    None => String::new(),
                };
                let name = if is_local_module {
                    self.atom_text(f)
                } else {
                    format!("{}::{}", self.atom_text(m), self.atom_text(f))
                };
                self.push_ref(&format!("{name}{arity_tail}"), "calls", f);
                break;
            }
        }
    }

    /// resolveErlangGenServerTarget (tree-sitter.ts): the module a gen_server target names.
    fn gen_server_target(&mut self, target: Node<'t>) -> Option<String> {
        let base = self.file_path.rsplit('/').next().unwrap_or(self.file_path);
        let own = base.strip_suffix(".erl").unwrap_or(base).to_string();
        let own = (!own.is_empty()).then_some(own);
        if target.kind() == "atom" {
            let name = self.atom_text(target);
            return (!name.is_empty()).then_some(name);
        }
        if target.kind() != "macro_call_expr" {
            return None;
        }
        let macro_name = self.text(target.child_by_field_name("name")?).to_string();
        if macro_name == "MODULE" {
            return own;
        }
        if self.server_macros.is_none() {
            let mut macros = ServerMacros { own: HashSet::new(), atoms: HashMap::new() };
            for form in named_children(self.root) {
                if form.kind() != "pp_define" {
                    continue;
                }
                let def_name = form.child_by_field_name("lhs").and_then(|l| l.child_by_field_name("name"));
                let (Some(def_name), Some(replacement)) = (def_name, form.child_by_field_name("replacement")) else { continue };
                if self.macro_name_is(replacement, "MODULE") {
                    macros.own.insert(self.text(def_name).to_string());
                } else if replacement.kind() == "atom" {
                    macros.atoms.insert(self.text(def_name).to_string(), self.atom_text(replacement));
                }
            }
            self.server_macros = Some(macros);
        }
        let macros = self.server_macros.as_ref().unwrap();
        if macros.own.contains(&macro_name) {
            return own;
        }
        macros.atoms.get(&macro_name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use codegraph_types::{Language, NodeKind};

    #[test]
    fn clauses_merge_per_arity_and_calls_carry_the_call_site_arity() {
        let source = "-module(b).\n-export([f/1]).\nf(1) -> ok;\nf(X) -> f(X - 1).\nf(X, Y) -> lists:map(X, Y), gen_server:call(?MODULE, x).\n";
        let result = crate::extract_file("src/b.erl", source, Language::parse("erlang").unwrap()).unwrap();
        let functions: Vec<_> = result.nodes.iter().filter(|n| n.kind == NodeKind::Function).collect();
        let summary: Vec<_> = functions.iter().map(|n| (n.qualified_name.as_str(), n.start_line, n.end_line, n.is_exported)).collect();
        assert_eq!(summary, [("b::f/1", 3, 4, Some(true)), ("b::f/2", 5, 5, Some(false))]);
        let refs: Vec<_> = result.unresolved_references.iter().map(|r| r.reference_name.as_str()).collect();
        assert_eq!(refs, ["f/1", "lists::map/2", "gen_server::call/2", "b::handle_call/3"]);
    }
}
