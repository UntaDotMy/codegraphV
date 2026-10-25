//! C/C++ function-pointer dispatch synthesis (#932). Port of `src/resolution/c-fnptr-synthesizer.ts`.
//!
//! C/C++ polymorphism is the function pointer: a struct carries a fn-pointer field
//! (`int (*fn)(int)`, or a fn-pointer-typedef field `hook_func func`), concrete functions are
//! registered into it through a table (`static struct cmd cmds[] = {{"add", cmd_add}, …}`, a
//! designated `.fn = cmd_add`, or `x->fn = cmd_add`), and the dispatcher calls through it
//! (`p->fn(argv)`). This links the dispatcher to each registered function, keyed by
//! (struct type, fn-pointer field):
//!
//!   • registrations — positional initializer slots, designated `.field = fn`, `x->field = fn`;
//!   • dispatch — `recv->field(…)` where `recv` resolves to a value of struct type `S`, falling back
//!     to the field name when it is unique to one struct;
//!   • field←field propagation — `a->f = b->g` merges `B.g`'s handlers into `A.f`;
//!   • macro-built tables (`MAKE_CMD(…,proc,…)`), read as a unit with the includer's macro env, its
//!     `#ifdef`s evaluated against the includer's defined set, and an inline struct parsed in place;
//!   • bare arrays of function pointers (`opcode_t *opcodes[256] = {nop,…}` / `opcodes[op](…)`),
//!     keyed by the array variable's name.
//!
//! Fuse-then-link: one extraction sweep reads and strips each file once and leaves per-file
//! survival filters; the registration / propagation / dispatch stages re-read only the files that
//! survive. The TS pass has a native kernel path that is record-identical to its JS sweep; this
//! ports the JS sweep. Offsets are bytes into the stripped source.

use super::common::*;
use crate::ctx::Ctx;
use crate::meta;
use crate::types::NodeRef;
use codegraph_jsre::{escape_regex, Re};
use codegraph_types::jscompat::js_trim;
use codegraph_types::{nodepath, Edge, NodeKind};
use indexmap::IndexSet;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// A real command table (git ~150) is legitimate fan-out; this only stops pathological cases.
const FANOUT_CAP: usize = 300;

const C_CPP_EXT: &str = r"\.(c|h|cc|cpp|cxx|hpp|hh|hxx|cppm|ipp|inl|tcc)$";
const INCLUDABLE_EXT: &str = r"\.(def|inc|h|hh|hpp|hxx|c|cc|cpp|cxx|ipp|tcc|tbl)$";
/// A fn-pointer field looks like `… (*name)(…)`; a calling-convention macro may precede the `*`.
const FNPTR_DECL: &str = r"\(\s*(?:\w+\s+)*\*\s*(\w+)\s*\)\s*\(";
const FNPTR_TYPEDEF: &str = r"\btypedef\b[^;{}]*?\(\s*(?:\w+\s+)*\*\s*(\w+)\s*\)\s*\(";
const FNTYPE_TYPEDEF_STMT: &str = r"\btypedef\b([^;{}]*);";
const INCLUDE: &str = r##"#[ \t]*include[ \t]+"([^"\n]+)""##;
const OBJ_ALIAS: &str = r"^[ \t]*#[ \t]*define[ \t]+(\w+)[ \t]+(?:(?:struct|union)[ \t]+)*[A-Za-z_]\w*[ \t\r]*$";
const INIT: &str = r"(?:^|[;{}])\s*(?:(?:static|const|extern|register|volatile)\s+)*(?:(?:struct|union)\s+)?(\w+)\s+(\w+)\s*(\[[^\]]*\])?\s*=\s*\{";
const INLINE_STRUCT: &str = r"\b(?:struct|union)\s+(\w+)\s*\{";
const ARRAY_TABLE: &str = r"(?:^|[;{}])\s*(?:(?:static|const|extern|register|volatile)\s+)*(\w+)\s+(\*\s*)?(\w+)\s*\[[^\]]*\]\s*=\s*\{";
const DISPATCH: &str = r"((?:\w+(?:\s*\[[^\][]*\])?\s*(?:->|\.)\s*)+)(\w+)\s*\)?\s*\(";
const ARRAY_DISPATCH: &str = r"(?:\(\s*\*\s*)?\b(\w+)\s*\[[^\][]*\]\s*\)?\s*\(";
const FIELD_ASSIGN: &str = r"(\w+)\s*(?:->|\.)\s*(\w+)\s*=\s*(\w+)\s*(?:->|\.)\s*(\w+)";

/// Return-type keywords that must never be mistaken for a typedef's name.
const C_TYPE_KEYWORDS: [&str; 16] = [
    "void", "int", "char", "short", "long", "unsigned", "signed", "float", "double", "const", "struct", "union", "enum", "static", "volatile", "register",
];

fn is_c_type_keyword(name: &str) -> bool {
    C_TYPE_KEYWORDS.contains(&name) || name == "inline"
}

/// A struct field, in declaration order, flagged when it is a function pointer.
struct FieldInfo {
    name: String,
    index: usize,
    is_fn_ptr: bool,
    /// The declared type token, used to walk a chained receiver (`c->cmd->proc`).
    ty: String,
}

/// A struct field as parsed during the extraction sweep: structure only. A typedef-typed field's
/// fn-pointer-ness depends on the global typedef sets, so classification waits for the linking stage.
struct RawFieldDecl {
    name: Option<String>,
    index: usize,
    ptr: bool,
    ty: String,
}

/// A function-like macro: `#define NAME(p0,p1,…) expansion`.
#[derive(Clone)]
struct MacroDef {
    params: Vec<String>,
    expansion: String,
}

/// Per-file facts the extraction sweep leaves behind. Everything but `includes` is a survival
/// filter, over-approximate by construction.
#[derive(Default)]
struct FileFacts {
    init_tokens: Vec<String>,
    array_elems: Vec<String>,
    inline_ptr: bool,
    inline_types: Vec<String>,
    d_pairs: Vec<(String, String)>,
    dispatch_fields: Vec<String>,
    array_dispatch_names: Vec<String>,
    includes: Vec<String>,
}

// ── Text helpers ──────────────────────────────────────────────────────────────

/// Slice a node's body from a pre-split line array.
fn slice_lines_pre(lines: &[&str], start_line: u32, end_line: u32) -> String {
    if start_line == 0 {
        return String::new();
    }
    let a = ((start_line - 1) as usize).min(lines.len());
    let b = (end_line as usize).min(lines.len());
    if a >= b {
        return String::new();
    }
    lines[a..b].join("\n")
}

/// Index of the `}` matching the `{` at `open`; `None` if unbalanced.
fn match_brace(src: &str, open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in src.bytes().enumerate().skip(open) {
        if c == b'{' {
            depth += 1;
        } else if c == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// Index of the `)` matching the `(` at `open`; `None` if unbalanced.
fn match_paren(src: &str, open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in src.bytes().enumerate().skip(open) {
        if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// Split `body` on `sep` at brace/paren/bracket depth 0.
fn split_top_level(body: &str, sep: u8) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in body.bytes().enumerate() {
        if c == b'{' || c == b'(' || c == b'[' {
            depth += 1;
        } else if c == b'}' || c == b')' || c == b']' {
            depth -= 1;
        } else if c == sep && depth == 0 {
            out.push(&body[start..i]);
            start = i + 1;
        }
    }
    out.push(&body[start..]);
    out
}

fn has_define(stripped: &str) -> bool {
    stripped.contains("#define") || stripped.contains("# define")
}

/// Join `\`-continuations.
fn join_continuations(stripped: &str) -> String {
    Re::cached(r"\\\r?\n", "g").replace_all_str(stripped, " ")
}

/// Function-like macros from comment-stripped source; variadic macros are skipped.
fn parse_function_macros(stripped: &str) -> HashMap<String, MacroDef> {
    let mut out = HashMap::new();
    if !has_define(stripped) {
        return out;
    }
    let joined = join_continuations(stripped);
    for m in Re::cached(r"^[ \t]*#[ \t]*define[ \t]+(\w+)\(([^)]*)\)\s+(.+)$", "gm").captures_iter(&joined) {
        let params: Vec<String> = m.text(2).unwrap_or("").split(',').map(|p| js_trim(p).to_string()).filter(|p| !p.is_empty()).collect();
        if params.iter().any(|p| p == "..." || p.ends_with("...")) {
            continue;
        }
        out.insert(m.text(1).unwrap_or("").to_string(), MacroDef { params, expansion: js_trim(m.text(3).unwrap_or("")).to_string() });
    }
    out
}

/// Object-like macros `#define NAME value` (NAME not immediately followed by `(`).
fn parse_object_macros(stripped: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if !has_define(stripped) {
        return out;
    }
    let joined = join_continuations(stripped);
    for m in Re::cached(r"^[ \t]*#[ \t]*define[ \t]+(\w+)[ \t]+(\S[^\n]*)$", "gm").captures_iter(&joined) {
        out.insert(m.text(1).unwrap_or("").to_string(), js_trim(m.text(2).unwrap_or("")).to_string());
    }
    out
}

/// Every macro name a file `#define`s — the "defined" set for `#ifdef`.
fn parse_defined_names(stripped: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    if !has_define(stripped) {
        return out;
    }
    for m in Re::cached(r"^[ \t]*#[ \t]*define[ \t]+(\w+)", "gm").captures_iter(stripped) {
        out.insert(m.text(1).unwrap_or("").to_string());
    }
    out
}

struct CondFrame {
    parent_active: bool,
    active: bool,
    taken: bool,
}

/// Drop the inactive arms of `#ifdef`/`#ifndef`/`#if defined(X)`/`#else`/`#elif`/`#endif` given a
/// set of defined macro names, keeping line offsets (inactive lines are blanked, not removed). A
/// conditional it can't evaluate keeps its body.
fn eval_conditionals(text: &str, defined: &HashSet<String>) -> String {
    if !Re::cached(r"#\s*if", "").is_match(text) {
        return text.to_string();
    }
    let mut lines: Vec<String> = text.split('\n').map(String::from).collect();
    let mut stack: Vec<CondFrame> = Vec::new();
    let cond_defined = |expr: &str| -> Option<bool> {
        if let Some(m) = Re::cached(r"^defined\s*\(?\s*(\w+)\s*\)?$", "").captures(expr) {
            return Some(defined.contains(m.text(1).unwrap_or("")));
        }
        if let Some(m) = Re::cached(r"^!\s*defined\s*\(?\s*(\w+)\s*\)?$", "").captures(expr) {
            return Some(!defined.contains(m.text(1).unwrap_or("")));
        }
        None
    };
    for line in lines.iter_mut() {
        let t = js_trim(line).to_string();
        let active_now = |stack: &Vec<CondFrame>| stack.last().map_or(true, |f| f.active);
        if let Some(m) = Re::cached(r"^#\s*ifdef\s+(\w+)", "").captures(&t) {
            let pa = active_now(&stack);
            let cond = defined.contains(m.text(1).unwrap_or(""));
            stack.push(CondFrame { parent_active: pa, active: pa && cond, taken: cond });
            line.clear();
            continue;
        }
        if let Some(m) = Re::cached(r"^#\s*ifndef\s+(\w+)", "").captures(&t) {
            let pa = active_now(&stack);
            let cond = !defined.contains(m.text(1).unwrap_or(""));
            stack.push(CondFrame { parent_active: pa, active: pa && cond, taken: cond });
            line.clear();
            continue;
        }
        if let Some(m) = Re::cached(r"^#\s*if\s+(.+)$", "").captures(&t) {
            let pa = active_now(&stack);
            let c = cond_defined(js_trim(m.text(1).unwrap_or("")));
            let cond = c.unwrap_or(true); // unevaluable → keep
            stack.push(CondFrame { parent_active: pa, active: pa && cond, taken: cond });
            line.clear();
            continue;
        }
        if Re::cached(r"^#\s*elif\b", "").is_match(&t) || Re::cached(r"^#\s*else\b", "").is_match(&t) {
            if let Some(top) = stack.last_mut() {
                top.active = top.parent_active && !top.taken;
                top.taken = true;
            }
            line.clear();
            continue;
        }
        if Re::cached(r"^#\s*endif\b", "").is_match(&t) {
            stack.pop();
            line.clear();
            continue;
        }
        if !active_now(&stack) {
            line.clear(); // blank an inactive line (keep the newline)
        }
    }
    lines.join("\n")
}

/// Resolve a type token through object-like macro aliases (transitive, capped).
fn resolve_type_name(name: &str, obj_env: &HashMap<String, String>) -> String {
    let mut n = name.to_string();
    for _ in 0..5 {
        let Some(v) = obj_env.get(&n) else { break };
        let Some(t) = Re::cached(r"^(?:(?:struct|union)\s+)?(\w+)$", "").captures(js_trim(v)) else { break };
        n = t.text(1).unwrap_or("").to_string();
    }
    n
}

/// Substitute call args for the macro's params (whole-token) in its expansion.
fn substitute_macro(def: &MacroDef, args: &[String]) -> String {
    let map: HashMap<&str, &str> = def.params.iter().enumerate().map(|(i, p)| (p.as_str(), args.get(i).map_or("", |a| a.as_str()))).collect();
    Re::cached(r"\b\w+\b", "g").replace_all(&def.expansion, |m| map.get(m.text(0).unwrap_or("")).map(|v| v.to_string()))
}

/// Expand known function-like macro calls in `text` to a fixpoint (depth-capped).
fn expand_macro_calls(text: &str, env: &HashMap<String, MacroDef>) -> String {
    if env.is_empty() {
        return text.to_string();
    }
    let mut out = text.to_string();
    let re = Re::cached(r"\b(\w+)\s*\(", "g");
    for _ in 0..6 {
        let mut found: Option<(usize, usize, String)> = None;
        {
            let mut pos = 0;
            while let Some(m) = re.captures_at(&out, pos) {
                let whole = m.get(0).unwrap();
                pos = whole.end();
                let Some(def) = env.get(m.text(1).unwrap_or("")) else { continue };
                let open = whole.end() - 1;
                let Some(close) = match_paren(&out, open) else { continue };
                let args: Vec<String> = split_top_level(&out[open + 1..close], b',').into_iter().map(|a| js_trim(a).to_string()).collect();
                found = Some((whole.start(), close, substitute_macro(def, &args)));
                break; // restart scan — offsets shifted
            }
        }
        match found {
            Some((start, close, replacement)) => out.replace_range(start..=close, &replacement),
            None => break,
        }
    }
    out
}

/// Parse a struct body (the text between its `{` and `}`) into ordered fields, structure only.
fn parse_struct_fields_raw(inner: &str) -> Vec<RawFieldDecl> {
    let mut fields = Vec::new();
    let mut idx = 0;
    let fnptr = Re::cached(FNPTR_DECL, "");
    let typed = Re::cached(r"(\w+)\s+\**\s*(\w+)\s*$", "");
    let declarator = Re::cached(r"^\**\s*(\w+)", "");
    for raw_decl in split_top_level(inner, b';') {
        let decl = js_trim(raw_decl);
        if decl.is_empty() {
            continue;
        }
        // A field decl can declare several names sharing a leading type: `struct redisCommand *cmd, *lastcmd;`.
        let parts = split_top_level(decl, b',');
        let first_typed = typed.captures(parts[0]);
        let shared_type = first_typed.as_ref().and_then(|m| m.text(1)).unwrap_or("").to_string();
        for (pi, part) in parts.iter().enumerate() {
            let p = js_trim(part);
            let mut name: Option<String> = None;
            let mut ty = String::new();
            let mut ptr = false;
            if let Some(pm) = fnptr.captures(p) {
                name = pm.text(1).map(String::from); // `… (*name)(…)` — a function pointer
                ptr = true;
            } else if pi == 0 {
                if let Some(ft) = &first_typed {
                    name = ft.text(2).map(String::from);
                    ty = shared_type.clone();
                }
            } else if let Some(dm) = declarator.captures(p) {
                name = dm.text(1).map(String::from);
                ty = shared_type.clone();
            }
            // Always advance the positional index: an unparsed field still occupies one slot.
            fields.push(RawFieldDecl { name, index: idx, ptr, ty });
            idx += 1;
        }
    }
    fields
}

// ── The pass ──────────────────────────────────────────────────────────────────

struct Pass<'a> {
    ctx: &'a Ctx,
    src_cache: RefCell<Memo<Rc<str>>>,
    fn_macro_cache: RefCell<Memo<Rc<HashMap<String, MacroDef>>>>,
    obj_macro_cache: RefCell<Memo<Rc<HashMap<String, String>>>>,
    defined_cache: RefCell<Memo<Rc<HashSet<String>>>>,
    include_cache: RefCell<Memo<Rc<Vec<String>>>>,
    fn_ptr_typedefs: HashSet<String>,
    fn_type_typedefs: HashSet<String>,
    raw_fields_by_node: HashMap<String, Vec<RawFieldDecl>>,
    facts: HashMap<String, FileFacts>,
    /// Every inline-struct candidate tag anywhere.
    inline_tags: HashSet<String>,
    /// Object-macro names with an alias-shaped value anywhere.
    alias_names: HashSet<String>,
    /// struct name → ordered fields, for structs with ≥1 fn-pointer field (last-wins).
    struct_layout: HashMap<String, Rc<Vec<FieldInfo>>>,
    /// EVERY struct name → ALL its field layouts (a name can be reused across files).
    all_struct_fields: HashMap<String, Vec<Rc<Vec<FieldInfo>>>>,
    /// fn-pointer field name → the struct names that declare it.
    field_to_structs: HashMap<String, HashSet<String>>,
    /// `struct.field` → registered function ids, in registration order.
    reg: HashMap<String, IndexSet<String>>,
    /// Bare arrays of fn pointers: array variable name → per-file sets of function ids.
    array_reg: HashMap<String, Vec<(String, IndexSet<String>)>>,
    /// Global variable → struct type, for a dispatch through a file-scope table by subscript.
    global_var_type: HashMap<String, String>,
}

fn is_fn_kind(k: NodeKind) -> bool {
    k == NodeKind::Function || k == NodeKind::Method
}

impl<'a> Pass<'a> {
    fn src(&self, file: &str) -> Rc<str> {
        if let Some(hit) = self.src_cache.borrow().get(file) {
            return hit;
        }
        let s: Rc<str> = match self.ctx.read_file(file) {
            Some(r) => Rc::from(codegraph_text::strip_comments_for_regex(&r, "c")),
            None => Rc::from(""),
        };
        self.src_cache.borrow_mut().put(file.to_string(), s)
    }

    /// Resolve a quoted include relative to the includer's directory, then the project root.
    fn resolve_include(&self, includer: &str, inc: &str) -> Option<String> {
        let dir = nodepath::posix::dirname(&includer.replace('\\', "/"));
        let cand = nodepath::posix::normalize(&nodepath::posix::join(&[&dir, inc]));
        if self.ctx.file_exists(&cand) {
            return Some(cand);
        }
        if self.ctx.file_exists(inc) {
            return Some(inc.to_string());
        }
        None
    }

    /// Exact per-file include resolution (from RAW source — string contents survive).
    fn scan_includes(&self, file: &str) -> Vec<String> {
        let Some(raw) = self.ctx.read_file(file).filter(|r| r.contains("include")) else { return Vec::new() };
        let ext = Re::cached(INCLUDABLE_EXT, "i");
        let mut out = Vec::new();
        for im in Re::cached(INCLUDE, "g").captures_iter(&raw) {
            let cap = im.text(1).unwrap_or("");
            if !ext.is_match(cap) {
                continue;
            }
            if let Some(t) = self.resolve_include(file, cap) {
                out.push(t);
            }
        }
        out
    }

    /// Indexed files answer from their facts; non-indexed includes fall back to a bounded lazy scan.
    fn local_includes_of(&self, file: &str) -> Rc<Vec<String>> {
        if let Some(f) = self.facts.get(file) {
            return Rc::new(f.includes.clone());
        }
        if let Some(hit) = self.include_cache.borrow().get(file) {
            return hit;
        }
        let out = Rc::new(self.scan_includes(file));
        self.include_cache.borrow_mut().put(file.to_string(), out)
    }

    fn file_fn_macros(&self, file: &str) -> Rc<HashMap<String, MacroDef>> {
        if let Some(hit) = self.fn_macro_cache.borrow().get(file) {
            return hit;
        }
        let m = Rc::new(parse_function_macros(&self.src(file)));
        self.fn_macro_cache.borrow_mut().put(file.to_string(), m)
    }

    fn file_obj_macros(&self, file: &str) -> Rc<HashMap<String, String>> {
        if let Some(hit) = self.obj_macro_cache.borrow().get(file) {
            return hit;
        }
        let m = Rc::new(parse_object_macros(&self.src(file)));
        self.obj_macro_cache.borrow_mut().put(file.to_string(), m)
    }

    fn file_defined_names(&self, file: &str) -> Rc<HashSet<String>> {
        if let Some(hit) = self.defined_cache.borrow().get(file) {
            return hit;
        }
        let d = Rc::new(parse_defined_names(&self.src(file)));
        self.defined_cache.borrow_mut().put(file.to_string(), d)
    }

    /// A file's effective macro environment = its own `#define`s PLUS those of the headers it
    /// `#include`s. First writer wins, so the file's own defs override included ones.
    fn build_env(&self, file: &str, depth: i32, seen: &mut HashSet<String>, fn_env: &mut HashMap<String, MacroDef>, obj: &mut HashMap<String, String>, def: &mut HashSet<String>) {
        if depth < 0 || !seen.insert(file.to_string()) {
            return;
        }
        for (k, v) in self.file_fn_macros(file).iter() {
            fn_env.entry(k.clone()).or_insert_with(|| v.clone());
        }
        for (k, v) in self.file_obj_macros(file).iter() {
            obj.entry(k.clone()).or_insert_with(|| v.clone());
        }
        for n in self.file_defined_names(file).iter() {
            def.insert(n.clone());
        }
        for inc in self.local_includes_of(file).iter() {
            self.build_env(inc, depth - 1, seen, fn_env, obj, def);
        }
    }

    // -- Stage A: the extraction sweep ------------------------------------------

    fn classify_fields(&self, raw: &[RawFieldDecl]) -> Vec<FieldInfo> {
        raw.iter()
            .map(|f| FieldInfo {
                name: f.name.clone().unwrap_or_default(),
                index: f.index,
                is_fn_ptr: f.name.is_some() && (f.ptr || (!f.ty.is_empty() && (self.fn_ptr_typedefs.contains(&f.ty) || self.fn_type_typedefs.contains(&f.ty)))),
                ty: f.ty.clone(),
            })
            .collect()
    }

    fn sweep_file(&mut self, file: &str) {
        let s = self.src(file);
        if s.is_empty() {
            return;
        }

        // Typedefs (cross-file).
        if s.contains("typedef") {
            for m in Re::cached(FNPTR_TYPEDEF, "g").captures_iter(&s) {
                self.fn_ptr_typedefs.insert(m.text(1).unwrap_or("").to_string());
            }
            for m in Re::cached(FNTYPE_TYPEDEF_STMT, "g").captures_iter(&s) {
                let guts = m.text(1).unwrap_or("");
                if guts.contains("(*") || guts.contains("( *") {
                    continue; // pointer form — handled above
                }
                if let Some(fm) = Re::cached(r"\b(\w+)\s*\(", "").captures(guts) {
                    let name = fm.text(1).unwrap_or("");
                    if !is_c_type_keyword(name) {
                        self.fn_type_typedefs.insert(name.to_string());
                    }
                }
            }
        }

        // Struct-node field declarations (registered later in kind-scan order).
        let file_nodes = self.ctx.get_nodes_in_file(file);
        let mut lines: Option<Vec<&str>> = None;
        for st in file_nodes.iter() {
            if st.kind != NodeKind::Struct && st.kind != NodeKind::Union {
                continue;
            }
            let lines = lines.get_or_insert_with(|| s.split('\n').collect());
            let body = slice_lines_pre(lines, st.start_line, st.end_line);
            let Some(open) = body.find('{') else { continue };
            let Some(close) = match_brace(&body, open) else { continue };
            self.raw_fields_by_node.insert(st.id.clone(), parse_struct_fields_raw(&body[open + 1..close]));
        }

        // Registration filters: full-file, NO-SKIP scans — a superset of what the registration
        // pass can act on.
        let mut init_tokens: HashSet<String> = HashSet::new();
        let mut array_elems: HashSet<String> = HashSet::new();
        let mut inline_types: HashSet<String> = HashSet::new();
        let mut inline_ptr = false;
        if s.contains('{') {
            let re = Re::cached(INLINE_STRUCT, "g");
            let mut pos = 0;
            while let Some(im) = re.captures_at(&s, pos) {
                let whole = im.get(0).unwrap();
                pos = whole.end();
                let s_open = whole.end() - 1;
                let Some(s_close) = match_brace(&s, s_open) else { continue };
                // After `}`, expect `var [opt] [= {…}]` to be a table candidate.
                let Some(vm) = Re::cached(r"^\s*(\w+)\s*(\[[^\]]*\])?\s*(=\s*\{)?", "").captures(&s[s_close + 1..]) else { continue };
                if vm.text(1).map_or(true, |v| v.is_empty()) {
                    continue;
                }
                self.inline_tags.insert(im.text(1).unwrap_or("").to_string());
                for f in parse_struct_fields_raw(&s[s_open + 1..s_close]) {
                    if f.name.is_none() {
                        continue;
                    }
                    if f.ptr {
                        inline_ptr = true;
                    } else if !f.ty.is_empty() {
                        inline_types.insert(f.ty);
                    }
                }
            }
            if s.contains('=') {
                let re = Re::cached(INIT, "g");
                let mut pos = 0;
                while let Some(m) = re.captures_at(&s, pos) {
                    pos = m.get(0).unwrap().end();
                    init_tokens.insert(m.text(1).unwrap_or("").to_string());
                }
                let re = Re::cached(ARRAY_TABLE, "g");
                let mut pos = 0;
                while let Some(m) = re.captures_at(&s, pos) {
                    pos = m.get(0).unwrap().end();
                    array_elems.insert(format!("{}{}", if m.text(2).is_some() { "*" } else { "" }, m.text(1).unwrap_or("")));
                }
            }
        }

        // Alias-shaped object macros (registration filter support).
        if has_define(&s) {
            let joined = join_continuations(&s);
            for m in Re::cached(OBJ_ALIAS, "gm").captures_iter(&joined) {
                self.alias_names.insert(m.text(1).unwrap_or("").to_string());
            }
        }

        // Propagation + dispatch filters.
        let mut d_pairs: HashSet<(String, String)> = HashSet::new();
        if s.contains('=') {
            for m in Re::cached(FIELD_ASSIGN, "g").captures_iter(&s) {
                d_pairs.insert((m.text(2).unwrap_or("").to_string(), m.text(4).unwrap_or("").to_string()));
            }
        }
        let dispatch_fields: HashSet<String> = Re::cached(DISPATCH, "g").captures_iter(&s).map(|m| m.text(2).unwrap_or("").to_string()).collect();
        let array_names: HashSet<String> = Re::cached(ARRAY_DISPATCH, "g").captures_iter(&s).map(|m| m.text(1).unwrap_or("").to_string()).collect();

        let includes = self.scan_includes(file);
        if !init_tokens.is_empty()
            || !array_elems.is_empty()
            || inline_ptr
            || !inline_types.is_empty()
            || !d_pairs.is_empty()
            || !dispatch_fields.is_empty()
            || !array_names.is_empty()
            || !includes.is_empty()
        {
            self.facts.insert(
                file.to_string(),
                FileFacts {
                    init_tokens: init_tokens.into_iter().collect(),
                    array_elems: array_elems.into_iter().collect(),
                    inline_ptr,
                    inline_types: inline_types.into_iter().collect(),
                    d_pairs: d_pairs.into_iter().collect(),
                    dispatch_fields: dispatch_fields.into_iter().collect(),
                    array_dispatch_names: array_names.into_iter().collect(),
                    includes,
                },
            );
        }
    }

    // -- Stage B: struct field layouts ------------------------------------------

    /// Register a parsed struct under `name` into the three indexes.
    fn register_struct_layout(&mut self, name: &str, fields: Vec<FieldInfo>) {
        let fields = Rc::new(fields);
        self.all_struct_fields.entry(name.to_string()).or_default().push(Rc::clone(&fields));
        for f in fields.iter() {
            if !f.name.is_empty() && f.is_fn_ptr {
                self.field_to_structs.entry(f.name.clone()).or_default().insert(name.to_string());
            }
        }
        if fields.iter().any(|f| f.is_fn_ptr) {
            self.struct_layout.insert(name.to_string(), fields);
        }
    }

    fn fn_ptr_field_of(&self, struct_name: &str, field: &str) -> bool {
        self.struct_layout.get(struct_name).map_or(false, |l| l.iter().any(|f| f.name == field && f.is_fn_ptr))
    }

    // -- Stage C: registrations -------------------------------------------------

    /// Prefer a function in the same file.
    fn resolve_fn(&self, name: &str, prefer_file: &str) -> Option<NodeRef> {
        let cands: Vec<NodeRef> = self.ctx.get_nodes_by_name(name).iter().filter(|n| is_fn_kind(n.kind)).cloned().collect();
        if cands.is_empty() {
            return None;
        }
        if cands.len() == 1 {
            return Some(cands[0].clone());
        }
        if let Some(same) = cands.iter().find(|n| n.file_path == prefer_file) {
            return Some(same.clone());
        }
        Some(cands[0].clone())
    }

    fn add_reg(&mut self, struct_name: &str, field: &str, func: &NodeRef) {
        self.reg.entry(format!("{struct_name}.{field}")).or_default().insert(func.id.clone());
    }

    fn add_array_reg(&mut self, name: &str, file: &str, func: &NodeRef) {
        let entries = self.array_reg.entry(name.to_string()).or_default();
        let idx = match entries.iter().position(|e| e.0 == file) {
            Some(i) => i,
            None => {
                entries.push((file.to_string(), IndexSet::new()));
                entries.len() - 1
            }
        };
        entries[idx].1.insert(func.id.clone());
    }

    /// A struct value `{ … }` (one element): register its function entries to the struct's
    /// fields, by `.field = fn` designators or by positional slot.
    fn register_struct_value(&mut self, struct_name: &str, value_body: &str, file: &str, env: &HashMap<String, MacroDef>) {
        let Some(layout) = self.struct_layout.get(struct_name).cloned() else { return };
        let mut body = if env.is_empty() { value_body.to_string() } else { expand_macro_calls(value_body, env) };
        // A macro can expand to a whole brace-wrapped element; peel one outer layer.
        body = js_trim(&body).to_string();
        if body.starts_with('{') {
            if let Some(e) = match_brace(&body, 0).filter(|e| *e > 0) {
                if js_trim(&body[e + 1..]).is_empty() {
                    body = body[1..e].to_string();
                }
            }
        }
        let designated = Re::cached(r"^\.\s*(\w+)\s*=\s*(?:&\s*)?(\w+)\s*$", "");
        let bare = Re::cached(r"^&?\s*(\w+)\s*$", "");
        let mut pos = 0usize;
        for raw_item in split_top_level(&body, b',') {
            let item = js_trim(raw_item);
            if item.is_empty() {
                continue;
            }
            if let Some(des) = designated.captures(item) {
                let field = des.text(1).unwrap_or("");
                if self.fn_ptr_field_of(struct_name, field) {
                    if let Some(f) = self.resolve_fn(des.text(2).unwrap_or(""), file) {
                        self.add_reg(struct_name, field, &f);
                    }
                }
                // a designated item does not advance positional counting
                continue;
            }
            if let Some(field) = layout.iter().find(|f| f.index == pos) {
                if field.is_fn_ptr {
                    if let Some(id) = bare.captures(item) {
                        if let Some(f) = self.resolve_fn(id.text(1).unwrap_or(""), file) {
                            self.add_reg(struct_name, &field.name, &f);
                        }
                    }
                }
            }
            pos += 1;
        }
    }

    /// The literal function entries of an array-of-fn-pointers initializer, registered under the
    /// array's variable name. No index tracking: a runtime subscript fans the dispatch out to the
    /// whole set.
    fn register_array_value(&mut self, name: &str, body: &str, file: &str, env: &HashMap<String, MacroDef>) {
        let body = if env.is_empty() { body.to_string() } else { expand_macro_calls(body, env) };
        let designator = Re::cached(r"^\[[^\]]*\]\s*=\s*([\s\S]*)$", "");
        let cast = Re::cached(r"^\((?:[\w\s*]+)\)\s*", "");
        let amp = Re::cached(r"^&\s*", "");
        let ident = Re::cached(r"^(\w+)$", "");
        for raw_item in split_top_level(&body, b',') {
            let mut item = js_trim(raw_item).to_string();
            if item.is_empty() {
                continue;
            }
            let rest = designator
                .captures(&item)
                .map(|des| js_trim(des.text(1).unwrap_or("")).to_string());
            if let Some(rest) = rest {
                item = rest;
            }
            let item = js_trim(&amp.replace_all_str(&cast.replace_all_str(&item, ""), "")).to_string();
            let Some(id) = ident.captures(&item) else { continue };
            if let Some(f) = self.resolve_fn(id.text(1).unwrap_or(""), file) {
                self.add_array_reg(name, file, &f);
            }
        }
    }

    /// Process a `{ … }` initializer body (array of elements or a single struct).
    fn process_init(&mut self, struct_name: &str, body: &str, is_array: bool, file: &str, env: &HashMap<String, MacroDef>) {
        if is_array {
            for el in split_top_level(body, b',') {
                let t = js_trim(el);
                if t.starts_with('{') {
                    if let Some(e) = match_brace(t, 0).filter(|e| *e > 0) {
                        self.register_struct_value(struct_name, &t[1..e], file, env);
                    }
                } else if !t.is_empty() {
                    // an element built by a macro (`MAKE_CMD(…)`/`FUNCTION(…)`) or a bare value
                    self.register_struct_value(struct_name, t, file, env);
                }
            }
        } else {
            self.register_struct_value(struct_name, body, file, env);
        }
    }

    /// Process ONE unit's text and discard it.
    fn process_unit(&mut self, s: &str, file: &str, env: &HashMap<String, MacroDef>, obj_env: &HashMap<String, String>) {
        if s.is_empty() || !s.contains('{') {
            return;
        }

        let re = Re::cached(INLINE_STRUCT, "g");
        let mut pos = 0;
        while let Some(im) = re.captures_at(s, pos) {
            let whole = im.get(0).unwrap();
            pos = whole.end();
            let tag = im.text(1).unwrap_or("").to_string();
            let s_open = whole.end() - 1; // the struct body's `{`
            let Some(s_close) = match_brace(s, s_open) else { continue };
            // After `}`, expect `var [opt] [= {…}]` to be a table; else it's a plain type.
            let after = &s[s_close + 1..];
            let Some(vm) = Re::cached(r"^\s*(\w+)\s*(\[[^\]]*\])?\s*(=\s*\{)?", "").captures(after) else { continue };
            let Some(var_name) = vm.text(1).filter(|v| !v.is_empty()).map(String::from) else { continue };
            let is_array = vm.text(2).is_some();
            let has_init = vm.text(3).is_some();
            let vm_len = vm.get(0).unwrap().end();
            drop(vm);
            let fields = self.classify_fields(&parse_struct_fields_raw(&s[s_open + 1..s_close]));
            if !fields.iter().any(|f| f.is_fn_ptr) {
                continue; // only tables of fn pointers matter
            }
            if !self.struct_layout.contains_key(&tag) {
                self.register_struct_layout(&tag, fields);
            }
            self.global_var_type.insert(var_name, tag.clone());
            if has_init {
                let a_open = s_close + 1 + (vm_len - 1);
                if let Some(a_close) = match_brace(s, a_open).filter(|c| *c > 0) {
                    self.process_init(&tag, &s[a_open + 1..a_close], is_array, file, env);
                    pos = a_close;
                }
            }
        }

        if !s.contains('=') {
            return;
        }
        let re = Re::cached(INIT, "g");
        let mut pos = 0;
        while let Some(m) = re.captures_at(s, pos) {
            let whole = m.get(0).unwrap();
            pos = whole.end();
            let mut struct_name = m.text(1).unwrap_or("").to_string();
            if !self.struct_layout.contains_key(&struct_name) {
                struct_name = resolve_type_name(&struct_name, obj_env);
            }
            if !self.struct_layout.contains_key(&struct_name) {
                continue;
            }
            let is_array = m.text(3).is_some();
            let open = whole.end() - 1; // points at the `{`
            let Some(close) = match_brace(s, open) else { continue };
            self.global_var_type.insert(m.text(2).unwrap_or("").to_string(), struct_name.clone());
            drop(m);
            self.process_init(&struct_name, &s[open + 1..close], is_array, file, env);
            pos = close;
        }

        // Bare arrays-of-function-pointers (no struct, no field). Gated on the element type being
        // a function typedef — a fn-TYPE typedef needs the `*`, a fn-pointer typedef does not.
        let re = Re::cached(ARRAY_TABLE, "g");
        let mut pos = 0;
        while let Some(am) = re.captures_at(s, pos) {
            let whole = am.get(0).unwrap();
            pos = whole.end();
            let elem_type = am.text(1).unwrap_or("");
            let has_star = am.text(2).is_some();
            if !((self.fn_type_typedefs.contains(elem_type) && has_star) || self.fn_ptr_typedefs.contains(elem_type)) {
                continue;
            }
            let open = whole.end() - 1; // the `{`
            let Some(close) = match_brace(s, open) else { continue };
            let name = am.text(3).unwrap_or("").to_string();
            drop(am);
            self.register_array_value(&name, &s[open + 1..close], file, env);
            pos = close;
        }
    }

    fn typedef_hit(&self, t: &str) -> bool {
        self.fn_ptr_typedefs.contains(t) || self.fn_type_typedefs.contains(t)
    }

    /// Can this file's OWN unit have any side effect? Over-approximate.
    fn reg_survives(&self, f: &FileFacts) -> bool {
        f.inline_ptr
            || f.inline_types.iter().any(|t| self.typedef_hit(t))
            || f.init_tokens.iter().any(|t| self.struct_layout.contains_key(t) || self.inline_tags.contains(t) || self.alias_names.contains(t))
            || f.array_elems.iter().any(|e| match e.strip_prefix('*') {
                Some(rest) => self.typedef_hit(rest),
                None => self.fn_ptr_typedefs.contains(e),
            })
    }

    fn register_all(&mut self, files: &[String], indexed: &HashSet<&str>) {
        let mut seen_include: HashSet<String> = HashSet::new();
        for file in files {
            let Some(facts) = self.facts.get(file) else { continue };
            let survives = self.reg_survives(facts);
            let includes = facts.includes.clone();
            if !survives && includes.is_empty() {
                continue;
            }
            let mut env: HashMap<String, MacroDef> = HashMap::new();
            let mut obj_env: HashMap<String, String> = HashMap::new();
            let mut defined: HashSet<String> = HashSet::new();
            self.build_env(file, 2, &mut HashSet::new(), &mut env, &mut obj_env, &mut defined);
            if survives {
                let s = self.src(file);
                if !s.is_empty() {
                    self.process_unit(&s, file, &env, &obj_env);
                }
            }
            for target in &includes {
                let key = format!("{file}>{target}");
                if seen_include.contains(&key) {
                    continue;
                }
                let inc_src = self.src(target);
                if inc_src.is_empty() {
                    continue;
                }
                if indexed.contains(target.as_str()) {
                    // Re-scan an indexed header only when this includer unlocks guarded code.
                    let own_def = self.file_defined_names(target);
                    let adds = defined.iter().any(|n| !own_def.contains(n));
                    if !adds || !Re::cached(r"#\s*if", "").is_match(&inc_src) {
                        continue;
                    }
                }
                seen_include.insert(key);
                // The include is pasted into the includer — evaluate its conditionals in the
                // includer's defined set, and re-parse its OWN macros from that resolved text so a
                // conditionally defined macro takes the arm that is active here.
                let text = eval_conditionals(&inc_src, &defined);
                let mut inc_env = env.clone();
                for (k, v) in parse_function_macros(&text) {
                    inc_env.insert(k, v);
                }
                let mut inc_obj_env = obj_env.clone();
                for (k, v) in parse_object_macros(&text) {
                    inc_obj_env.insert(k, v);
                }
                self.process_unit(&text, target, &inc_env, &inc_obj_env);
            }
        }
    }

    // -- Receiver-type resolution within a function's source --------------------

    /// `(?:struct )?TYPE [*]recv` declared in the params or body → TYPE (if a known fn-pointer-bearing struct).
    fn recv_type_in(&self, fn_src: &str, recv: &str) -> Option<String> {
        let re = Re::try_cached(&format!(r"(?:(?:struct|union)\s+)?(\w+)\s*\*?\s*\b{recv}\b\s*(?:[,)=;]|\[)"), "g")?;
        let mut pos = 0;
        while let Some(m) = re.captures_at(fn_src, pos) {
            pos = m.get(0).unwrap().end();
            let t = m.text(1).unwrap_or("");
            if self.struct_layout.contains_key(t) {
                return Some(t.to_string());
            }
        }
        None
    }

    /// Declared type of a local/param `v` — ANY type token. Falls back to a file-scope table variable.
    fn var_type_in(&self, fn_src: &str, v: &str) -> Option<String> {
        if let Some(re) = Re::try_cached(&format!(r"(?:(?:struct|union)\s+)?(\w+)\s*\*?\s*\b{}\b\s*(?:[,)=;]|\[)", escape_regex(v)), "g") {
            let mut pos = 0;
            while let Some(m) = re.captures_at(fn_src, pos) {
                pos = m.get(0).unwrap().end();
                let t = m.text(1).unwrap_or("");
                if !is_c_type_keyword(t) {
                    return Some(t.to_string());
                }
            }
        }
        self.global_var_type.get(v).cloned()
    }

    /// Resolve a member-access chain (`c->cmd`, or just `p`) to a struct type, walking each
    /// segment's declared field type. Array subscripts are stripped.
    fn resolve_chain_type(&self, fn_src: &str, chain: &str) -> Option<String> {
        let stripped = Re::cached(r"\s*\[[^\]]*\]", "g").replace_all_str(chain, "");
        let segs: Vec<&str> = Re::cached(r"\s*(?:->|\.)\s*", "").split(&stripped).into_iter().filter(|s| !s.is_empty()).collect();
        if segs.is_empty() {
            return None;
        }
        let mut t = self.var_type_in(fn_src, segs[0]);
        for seg in &segs[1..] {
            let Some(cur) = t else { break };
            let mut next: Option<String> = None;
            for fields in self.all_struct_fields.get(&cur).into_iter().flatten() {
                if let Some(f) = fields.iter().find(|fl| fl.name == *seg && !fl.ty.is_empty()) {
                    next = Some(f.ty.clone());
                    break;
                }
            }
            t = next;
        }
        t
    }

    // -- Stage D: field←field propagation ---------------------------------------

    fn propagate(&mut self, files: &[String]) {
        let mut propagations: Vec<(String, String)> = Vec::new(); // (to, from)
        let assign = Re::cached(FIELD_ASSIGN, "g");
        for file in files {
            let Some(facts) = self.facts.get(file) else { continue };
            if !facts.d_pairs.iter().any(|(l, r)| self.field_to_structs.contains_key(l) && self.field_to_structs.contains_key(r)) {
                continue;
            }
            let s = self.src(file);
            if s.is_empty() || !s.contains('=') {
                continue;
            }
            let fns = self.ctx.get_nodes_in_file(file);
            let d_lines: Vec<&str> = s.split('\n').collect();
            for func in fns.iter() {
                if !is_fn_kind(func.kind) {
                    continue;
                }
                let body = slice_lines_pre(&d_lines, func.start_line, func.end_line);
                if !body.contains('=') {
                    continue;
                }
                let mut pos = 0;
                while let Some(m) = assign.captures_at(&body, pos) {
                    pos = m.get(0).unwrap().end();
                    let (lrecv, lfield, rrecv, rfield) = (m.text(1).unwrap_or(""), m.text(2).unwrap_or(""), m.text(3).unwrap_or(""), m.text(4).unwrap_or(""));
                    // Pre-gate on field NAMES: only fn-pointer fields of SOME struct can pass.
                    if !self.field_to_structs.contains_key(lfield) || !self.field_to_structs.contains_key(rfield) {
                        continue;
                    }
                    let (Some(lt), Some(rt)) = (self.recv_type_in(&body, lrecv), self.recv_type_in(&body, rrecv)) else { continue };
                    if self.fn_ptr_field_of(&lt, lfield) && self.fn_ptr_field_of(&rt, rfield) {
                        propagations.push((format!("{lt}.{lfield}"), format!("{rt}.{rfield}")));
                    }
                }
            }
        }
        for _ in 0..3 {
            if propagations.is_empty() {
                break;
            }
            let mut changed = false;
            for (to, from) in &propagations {
                let Some(from_set) = self.reg.get(from) else { continue };
                let ids: Vec<String> = from_set.iter().cloned().collect();
                let to_set = self.reg.entry(to.clone()).or_default();
                for id in ids {
                    if to_set.insert(id) {
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
    }

    // -- Stage E: dispatch sites → edges ----------------------------------------

    fn dispatch_edges(&self, files: &[String]) -> Vec<Edge> {
        let mut edges: Vec<Edge> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let dispatch = Re::cached(DISPATCH, "g");
        let array_dispatch = Re::cached(ARRAY_DISPATCH, "g");
        let trailing_arrow = Re::cached(r"\s*(?:->|\.)\s*$", "");
        let subscripts = Re::cached(r"\s*\[[^\]]*\]", "g");
        let arrows = Re::cached(r"\s*(?:->|\.)\s*", "");
        for file in files {
            let Some(facts) = self.facts.get(file) else { continue };
            let survives = facts.dispatch_fields.iter().any(|f| self.field_to_structs.contains_key(f))
                || (!self.array_reg.is_empty() && facts.array_dispatch_names.iter().any(|n| self.array_reg.contains_key(n)));
            if !survives {
                continue;
            }
            let s = self.src(file);
            if s.is_empty() {
                continue;
            }
            let fns = self.ctx.get_nodes_in_file(file);
            let e_lines: Vec<&str> = s.split('\n').collect();
            for func in fns.iter() {
                if !is_fn_kind(func.kind) {
                    continue;
                }
                let body = slice_lines_pre(&e_lines, func.start_line, func.end_line);
                let mut added = 0usize;
                // Incremental line counting: matches arrive in ascending index order.
                let mut lc_idx = 0usize;
                let mut lc_line = func.start_line;
                let line_at = |idx: usize, lc_idx: &mut usize, lc_line: &mut u32| -> u32 {
                    for b in &body.as_bytes()[*lc_idx..idx.max(*lc_idx)] {
                        if *b == b'\n' {
                            *lc_line += 1;
                        }
                    }
                    *lc_idx = idx;
                    *lc_line
                };
                let mut pos = 0;
                while let Some(m) = dispatch.captures_at(&body, pos) {
                    let whole = m.get(0).unwrap();
                    pos = whole.end();
                    if added >= FANOUT_CAP {
                        break;
                    }
                    let base_chain = js_trim(&trailing_arrow.replace_all_str(m.text(1).unwrap_or(""), "")).to_string();
                    let field = m.text(2).unwrap_or("");
                    let Some(owners) = self.field_to_structs.get(field).filter(|o| !o.is_empty()) else { continue };
                    // 1) resolve the receiver chain's struct type precisely (handles c->cmd->proc);
                    // 2) else the last segment as a simple local/param of a fn-pointer-bearing struct;
                    // 3) else fall back to a field name that belongs to exactly one struct.
                    let mut struct_name = self.resolve_chain_type(&body, &base_chain);
                    if struct_name.as_ref().map_or(true, |t| !owners.contains(t)) {
                        let stripped = subscripts.replace_all_str(&base_chain, "");
                        let last_seg = arrows
                            .split(&stripped)
                            .into_iter()
                            .last()
                            .unwrap_or("")
                            .to_string();
                        let t = self.recv_type_in(&body, &last_seg);
                        struct_name = t.filter(|t| owners.contains(t));
                    }
                    if struct_name.as_ref().map_or(true, |t| !owners.contains(t)) {
                        struct_name = if owners.len() == 1 { owners.iter().next().cloned() } else { None };
                    }
                    let Some(struct_name) = struct_name else { continue };
                    let Some(targets) = self.reg.get(&format!("{struct_name}.{field}")) else { continue };
                    let line = line_at(whole.start(), &mut lc_idx, &mut lc_line);
                    for tid in targets {
                        if *tid == func.id {
                            continue;
                        }
                        if !seen.insert(format!("{}>{}", func.id, tid)) {
                            continue;
                        }
                        edges.push(calls_edge(
                            &func.id,
                            tid,
                            Some(line),
                            meta! {
                                "synthesizedBy" => "fn-pointer-dispatch",
                                "via" => format!("{struct_name}.{field}"),
                                "registeredAt" => format!("{}:{}", func.file_path, line),
                            },
                        ));
                        added += 1;
                        if added >= FANOUT_CAP {
                            break;
                        }
                    }
                }

                // ---- bare array-of-fn-pointers dispatch (`tbl[i](…)`) ----
                if !self.array_reg.is_empty() && added < FANOUT_CAP {
                    // Fresh scan from the body's start — rewind the line-count cursor too.
                    lc_idx = 0;
                    lc_line = func.start_line;
                    let mut pos = 0;
                    while let Some(m) = array_dispatch.captures_at(&body, pos) {
                        let whole = m.get(0).unwrap();
                        pos = whole.end();
                        if added >= FANOUT_CAP {
                            break;
                        }
                        let name = m.text(1).unwrap_or("");
                        let Some(entries) = self.array_reg.get(name) else { continue };
                        // Same-file table wins on a name collision; a unique name resolves
                        // cross-file; otherwise ambiguous — bail.
                        let ids = if entries.len() == 1 { Some(&entries[0].1) } else { entries.iter().find(|e| e.0 == func.file_path).map(|e| &e.1) };
                        let Some(ids) = ids else { continue };
                        let line = line_at(whole.start(), &mut lc_idx, &mut lc_line);
                        for tid in ids {
                            if *tid == func.id {
                                continue;
                            }
                            if !seen.insert(format!("{}>{}", func.id, tid)) {
                                continue;
                            }
                            edges.push(calls_edge(
                                &func.id,
                                tid,
                                Some(line),
                                meta! {
                                    "synthesizedBy" => "fn-pointer-dispatch",
                                    "via" => format!("{name}[]"),
                                    "registeredAt" => format!("{}:{}", func.file_path, line),
                                },
                            ));
                            added += 1;
                            if added >= FANOUT_CAP {
                                break;
                            }
                        }
                    }
                }
            }
        }
        edges
    }
}

/// Edges from each C/C++ function that calls through a struct's function-pointer field (or a
/// subscripted array of function pointers) to the functions registered into it.
pub fn c_fn_pointer_dispatch_edges(ctx: &Ctx) -> Vec<Edge> {
    let ext = Re::cached(C_CPP_EXT, "i");
    let files: Vec<String> = ctx.get_all_files().into_iter().filter(|f| ext.is_match(f)).collect();
    if files.is_empty() {
        return Vec::new();
    }

    // Hold every stripped file when the corpus is modest; otherwise keep within-stage locality.
    let full_cache_cap = (files.len() as f64 * 1.05).ceil() as usize + 512;
    let cache_cap = if files.len() <= 8192 { full_cache_cap } else { 128 };
    let mut pass = Pass {
        ctx,
        src_cache: RefCell::new(Memo::new(cache_cap)),
        fn_macro_cache: RefCell::new(Memo::new(256)),
        obj_macro_cache: RefCell::new(Memo::new(256)),
        defined_cache: RefCell::new(Memo::new(256)),
        include_cache: RefCell::new(Memo::new(1024)),
        fn_ptr_typedefs: HashSet::new(),
        fn_type_typedefs: HashSet::new(),
        raw_fields_by_node: HashMap::new(),
        facts: HashMap::new(),
        inline_tags: HashSet::new(),
        alias_names: HashSet::new(),
        struct_layout: HashMap::new(),
        all_struct_fields: HashMap::new(),
        field_to_structs: HashMap::new(),
        reg: HashMap::new(),
        array_reg: HashMap::new(),
        global_var_type: HashMap::new(),
    };

    // Stage A: ONE read + strip per file.
    for file in &files {
        pass.sweep_file(file);
    }

    // Stage B: replay the struct kind-scan (rowid order): same-name layout precedence depends on it.
    for kind in [NodeKind::Struct, NodeKind::Union] {
        each_node(ctx, kind, |st| {
            if !ext.is_match(&st.file_path) {
                return;
            }
            let Some(raw_fields) = pass.raw_fields_by_node.remove(&st.id) else { return };
            let fields = pass.classify_fields(&raw_fields);
            pass.register_struct_layout(&st.name, fields);
        });
    }
    pass.raw_fields_by_node.clear();

    // Stage C: stream each surviving file (and every file's qualifying local includes) through process_unit.
    let indexed: HashSet<&str> = files.iter().map(|f| f.as_str()).collect();
    pass.register_all(&files, &indexed);

    // Stage D: field←field propagation.
    pass.propagate(&files);
    if pass.reg.is_empty() && pass.array_reg.is_empty() {
        return Vec::new();
    }

    // Stage E: dispatch sites → edges.
    pass.dispatch_edges(&files)
}
