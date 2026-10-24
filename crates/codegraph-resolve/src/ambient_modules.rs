//! TypeScript ambient modules and namespaces — port of `src/resolution/ambient-modules.ts`.
//!
//! `declare module 'x' { … }` blocks in `.d.ts` files, the `namespace`/`module` blocks of any TS
//! file, and the global scope of a script file, found with a small tokenizer (UTF-16 columns).

use crate::ctx::Ctx;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_search::query_utils::is_test_file;
use codegraph_types::{EdgeKind, NodeKind, ReferenceKind};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

#[derive(Debug, Clone)]
pub struct AmbientModuleBlock {
    pub specifier: String,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub default_name: Option<String>,
    pub export_aliases: IndexMap<String, String>,
    pub pattern: bool,
}

/// A region with a start and an end position.
pub trait Bounds {
    fn bounds(&self) -> (u32, u32, u32, u32);
}

impl Bounds for AmbientModuleBlock {
    fn bounds(&self) -> (u32, u32, u32, u32) {
        (self.start_line, self.start_column, self.end_line, self.end_column)
    }
}

impl Bounds for NamespaceBlock {
    fn bounds(&self) -> (u32, u32, u32, u32) {
        (self.start_line, self.start_column, self.end_line, self.end_column)
    }
}

impl Bounds for AmbientModule {
    fn bounds(&self) -> (u32, u32, u32, u32) {
        self.block.bounds()
    }
}

pub struct AmbientModuleScan {
    pub script: bool,
    pub blocks: Vec<AmbientModuleBlock>,
}

pub fn is_declaration_file(file: &str) -> bool {
    re!(r"\.d\.[cm]?ts$", "i").is_match(file)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Word,
    Str,
    Template,
    Punct,
}

fn is_default_declaration_word(t: &str) -> bool {
    matches!(t, "class" | "function" | "interface" | "abstract" | "async" | "enum")
}

struct AmbientScanner {
    blocks: Vec<AmbientModuleBlock>,
    script: bool,
    declare: u8,
    declare_line: u32,
    declare_column: u32,
    declare_name: String,
    import_tokens: Option<Vec<(Kind, String)>>,
    open: Option<AmbientModuleBlock>,
    export_state: u8,
    clause: Vec<String>,
}

impl AmbientScanner {
    fn settle_import(&mut self) {
        let t = self.import_tokens.take().unwrap_or_default();
        if let Some((Kind::Punct, text)) = t.first() {
            if text == "(" || text == "." {
                return;
            }
        }
        if matches!(t.first(), Some((Kind::Word, _)))
            && t.get(1).is_some_and(|x| x.1 == "=")
            && matches!(t.get(2), Some((Kind::Word, text)) if text != "require")
        {
            return;
        }
        self.script = false;
    }

    fn close_clause(&mut self) {
        let clause = std::mem::take(&mut self.clause);
        let open = self.open.as_mut().unwrap();
        let mut i = 0;
        while i < clause.len() {
            let local = clause[i].clone();
            if clause.get(i + 1).map(String::as_str) == Some("as") && clause.get(i + 2).is_some() {
                let exported = clause[i + 2].clone();
                if exported == "default" {
                    if open.default_name.is_none() {
                        open.default_name = Some(local);
                    }
                } else {
                    open.export_aliases.insert(exported, local);
                }
                i += 3;
            } else {
                open.export_aliases.insert(local.clone(), local);
                i += 1;
            }
        }
    }

    fn set_default(&mut self, text: &str) {
        let open = self.open.as_mut().unwrap();
        if open.default_name.is_none() {
            open.default_name = Some(text.to_string());
        }
    }

    fn block_token(&mut self, kind: Kind, text: &str, depth: u32) {
        if self.export_state == 5 {
            if kind == Kind::Punct && text == "}" && depth == 1 {
                self.close_clause();
                self.export_state = 0;
            } else if kind == Kind::Word {
                self.clause.push(text.to_string());
            }
            return;
        }
        if depth != 1 {
            return;
        }
        match self.export_state {
            0 => {
                if kind == Kind::Word && text == "export" {
                    self.export_state = 1;
                }
            }
            1 => {
                self.export_state = if kind == Kind::Word && text == "default" {
                    2
                } else if kind == Kind::Punct && text == "=" {
                    4
                } else if kind == Kind::Punct && text == "{" {
                    5
                } else {
                    0
                };
            }
            2 => {
                if kind == Kind::Word && is_default_declaration_word(text) {
                    self.export_state = 3;
                } else {
                    if kind == Kind::Word {
                        self.set_default(text);
                    }
                    self.export_state = 0;
                }
            }
            3 => {
                if (kind == Kind::Word && is_default_declaration_word(text)) || (kind == Kind::Punct && text == "*") {
                    return;
                }
                if kind == Kind::Word {
                    self.set_default(text);
                }
                self.export_state = 0;
            }
            4 => {
                if kind == Kind::Word {
                    self.set_default(text);
                }
                self.export_state = 0;
            }
            _ => {}
        }
    }

    fn token(&mut self, kind: Kind, text: &str, line: u32, column: u32, depth: u32) {
        if let Some(t) = self.import_tokens.as_mut() {
            t.push((kind, text.to_string()));
            let settle = t.len() >= 3 || t[0].0 != Kind::Word || (t.len() == 2 && t[1].1 != "=");
            if settle {
                self.settle_import();
            }
        }
        if self.open.is_some() {
            if kind == Kind::Punct && text == "}" && depth == 0 {
                let mut open = self.open.take().unwrap();
                open.end_line = line;
                open.end_column = column;
                open.pattern = open.specifier.contains('*');
                self.blocks.push(open);
                self.export_state = 0;
                self.clause.clear();
                return;
            }
            self.block_token(kind, text, depth);
            return;
        }
        if depth != 0 {
            self.declare = 0;
            return;
        }
        if kind == Kind::Word {
            if text == "import" {
                self.import_tokens = Some(Vec::new());
                self.declare = 0;
            } else if text == "export" {
                self.script = false;
                self.declare = 0;
            } else if text == "declare" {
                self.declare = 1;
                self.declare_line = line;
                self.declare_column = column;
            } else {
                self.declare = if self.declare == 1 && text == "module" { 2 } else { 0 };
            }
            return;
        }
        if kind == Kind::Str && self.declare == 2 {
            self.declare = 3;
            self.declare_name = text.to_string();
            return;
        }
        if kind == Kind::Punct && text == "{" && self.declare == 3 && !self.declare_name.is_empty() {
            self.open = Some(AmbientModuleBlock {
                specifier: self.declare_name.clone(),
                start_line: self.declare_line,
                start_column: self.declare_column,
                end_line: 0,
                end_column: 0,
                default_name: None,
                export_aliases: IndexMap::new(),
                pattern: false,
            });
        }
        self.declare = 0;
    }
}

pub fn scan_ambient_modules(source: &str) -> AmbientModuleScan {
    let mut s = AmbientScanner {
        blocks: Vec::new(),
        script: true,
        declare: 0,
        declare_line: 0,
        declare_column: 0,
        declare_name: String::new(),
        import_tokens: None,
        open: None,
        export_state: 0,
        clause: Vec::new(),
    };
    tokenize(source, &mut |k, t, l, c, d| s.token(k, t, l, c, d));
    if s.import_tokens.is_some() {
        s.settle_import();
    }
    AmbientModuleScan { script: s.script, blocks: s.blocks }
}

pub struct GlobalScopeScan {
    pub script: bool,
    pub global_words: HashSet<String>,
    pub module_blocks: Vec<AmbientModuleBlock>,
    pub import_aliases: HashSet<String>,
}

pub fn scan_global_scope(source: &str) -> GlobalScopeScan {
    let AmbientModuleScan { script, blocks: module_blocks } = scan_ambient_modules(source);
    let mut global_words: HashSet<String> = HashSet::new();
    let mut import_aliases: HashSet<String> = HashSet::new();
    let mut declare = 0u8;
    let mut in_global = false;
    let mut alias = 0u8;
    let mut alias_name = String::new();
    tokenize(source, &mut |kind, text, line, column, depth| {
        if kind == Kind::Word && text == "import" {
            alias = 1;
        } else if alias == 1 && kind == Kind::Word && text != "type" {
            alias = 2;
            alias_name = text.to_string();
        } else if alias == 2 && kind == Kind::Punct && text == "=" {
            import_aliases.insert(alias_name.clone());
            alias = 0;
        } else if !(alias == 1 && kind == Kind::Word) {
            alias = 0;
        }
        if in_global {
            if kind == Kind::Punct && text == "}" && depth == 0 {
                in_global = false;
            } else if kind == Kind::Word && depth == 1 {
                global_words.insert(format!("{line}:{column}"));
            }
            return;
        }
        if depth != 0 {
            declare = 0;
            return;
        }
        if kind == Kind::Word {
            if script {
                global_words.insert(format!("{line}:{column}"));
            }
            declare = if text == "declare" {
                1
            } else if declare == 1 && text == "global" {
                2
            } else {
                0
            };
            return;
        }
        if kind == Kind::Punct && text == "{" && declare == 2 {
            in_global = true;
        }
        declare = 0;
    });
    GlobalScopeScan { script, global_words, module_blocks, import_aliases }
}

#[derive(Debug, Clone)]
pub struct NamespaceBlock {
    pub path: String,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub global: bool,
}

enum Open {
    Block(usize),
    Global,
    Module,
    Other,
}

pub fn scan_namespace_blocks(source: &str, declaration_file: bool) -> Vec<NamespaceBlock> {
    let top_level_global = declaration_file && scan_ambient_modules(source).script;
    let mut blocks: Vec<NamespaceBlock> = Vec::new();
    let mut open: Vec<Open> = Vec::new();
    let mut names: Option<Vec<String>> = None;
    let mut expect_name = false;
    let mut keyword = (0u32, 0u32);
    let mut declare = 0u8;
    let mut ambient_module = false;
    let mut after_dot = false;
    tokenize(source, &mut |kind, text, line, column, _depth| {
        if kind == Kind::Punct && text == "{" {
            let header: Option<Vec<String>> = match &names {
                Some(n) if !n.is_empty() && !expect_name => Some(n.clone()),
                _ => None,
            };
            let outer = open.last();
            if declare == 2 {
                open.push(Open::Global);
            } else if ambient_module {
                open.push(Open::Module);
            } else if let (Some(header), true) = (header, !matches!(outer, Some(Open::Other))) {
                let parent: Option<usize> = match outer {
                    Some(Open::Block(i)) => Some(*i),
                    _ => None,
                };
                let global = match parent {
                    Some(p) => blocks[p].global,
                    None => matches!(outer, Some(Open::Global)) || (outer.is_none() && top_level_global),
                };
                let path = format!("{}{}", parent.map_or(String::new(), |p| format!("{}.", blocks[p].path)), header.join("."));
                blocks.push(NamespaceBlock {
                    path,
                    start_line: keyword.0,
                    start_column: keyword.1,
                    end_line: line,
                    end_column: column,
                    global,
                });
                open.push(Open::Block(blocks.len() - 1));
            } else {
                open.push(Open::Other);
            }
            names = None;
            declare = 0;
            ambient_module = false;
            after_dot = false;
            return;
        }
        if kind == Kind::Punct && text == "}" {
            if let Some(Open::Block(i)) = open.pop() {
                blocks[i].end_line = line;
                blocks[i].end_column = column;
            }
            names = None;
            declare = 0;
            ambient_module = false;
            after_dot = false;
            return;
        }
        ambient_module = names.as_ref().is_some_and(|n| n.is_empty()) && expect_name && kind == Kind::Str;
        if let Some(n) = names.as_mut() {
            if expect_name && kind == Kind::Word {
                n.push(text.to_string());
                expect_name = false;
                return;
            }
            if !expect_name && kind == Kind::Punct && text == "." {
                expect_name = true;
                return;
            }
            names = None;
        }
        if kind == Kind::Word {
            if (text == "namespace" || text == "module") && !after_dot {
                names = Some(Vec::new());
                expect_name = true;
                keyword = (line, column);
            }
            declare = if text == "declare" {
                1
            } else if declare == 1 && text == "global" {
                2
            } else {
                0
            };
            after_dot = false;
            return;
        }
        declare = 0;
        after_dot = kind == Kind::Punct && text == ".";
    });
    blocks
}

fn is_word_start(c: u16) -> bool {
    (65..=90).contains(&c) || (97..=122).contains(&c) || c == 95 || c == 36 || c > 127
}

fn is_word_char(c: u16) -> bool {
    is_word_start(c) || (48..=57).contains(&c)
}

fn regex_after_word(w: &str) -> bool {
    matches!(w, "return" | "typeof" | "case" | "do" | "else" | "in" | "of" | "instanceof" | "new" | "delete" | "void" | "throw" | "yield" | "await")
}

fn tokenize(source: &str, visit: &mut dyn FnMut(Kind, &str, u32, u32, u32)) {
    let src: Vec<u16> = source.encode_utf16().collect();
    let n = src.len();
    let at = |k: usize| -> u16 { src.get(k).copied().unwrap_or(0) };
    let mut i = 0usize;
    let mut line = 1u32;
    let mut line_start = 0usize;
    let mut depth = 0u32;
    let mut templates: Vec<u32> = Vec::new();
    let mut regex_allowed = true;

    // Reads template text up to the closing backtick or a `${`.
    fn template_text(src: &[u16], i: &mut usize, line: &mut u32, line_start: &mut usize, templates: &mut Vec<u32>, depth: u32) {
        let n = src.len();
        let at = |k: usize| -> u16 { src.get(k).copied().unwrap_or(0) };
        while *i < n {
            let c = src[*i];
            if c == 92 {
                if at(*i + 1) == 10 {
                    *line += 1;
                    *line_start = *i + 2;
                }
                *i += 2;
                continue;
            }
            if c == 96 {
                *i += 1;
                return;
            }
            if c == 36 && at(*i + 1) == 123 {
                templates.push(depth);
                *i += 2;
                return;
            }
            if c == 10 {
                *line += 1;
                *line_start = *i + 1;
            }
            *i += 1;
        }
    }

    while i < n {
        let c = src[i];
        if c == 10 {
            line += 1;
            line_start = i + 1;
            i += 1;
            continue;
        }
        if c == 32 || c == 9 || c == 13 || c == 0xfeff || c == 0xa0 {
            i += 1;
            continue;
        }
        let column = (i - line_start) as u32;
        if c == 47 {
            let next = at(i + 1);
            if next == 47 {
                while i < n && src[i] != 10 {
                    i += 1;
                }
                continue;
            }
            if next == 42 {
                i += 2;
                while i < n && !(src[i] == 42 && at(i + 1) == 47) {
                    if src[i] == 10 {
                        line += 1;
                        line_start = i + 1;
                    }
                    i += 1;
                }
                i += 2;
                continue;
            }
            if regex_allowed {
                i += 1;
                let mut in_class = false;
                while i < n {
                    let r = src[i];
                    if r == 10 {
                        break;
                    }
                    if r == 92 {
                        i += 1;
                    } else if r == 91 {
                        in_class = true;
                    } else if r == 93 {
                        in_class = false;
                    } else if r == 47 && !in_class {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                while i < n && is_word_char(src[i]) {
                    i += 1;
                }
                regex_allowed = false;
                visit(Kind::Template, "", line, column, depth);
                continue;
            }
        }
        if c == 39 || c == 34 {
            let mut j = i + 1;
            let mut text: Vec<u16> = Vec::new();
            let mut from = j;
            while j < n {
                let s = src[j];
                if s == c || s == 10 {
                    break;
                }
                if s == 92 {
                    text.extend_from_slice(&src[from.min(j)..j]);
                    j += 2;
                    from = j;
                    continue;
                }
                j += 1;
            }
            let end = j.min(n);
            if from < end {
                text.extend_from_slice(&src[from..end]);
            }
            i = if j < n && src[j] == c { j + 1 } else { j };
            regex_allowed = false;
            visit(Kind::Str, &String::from_utf16_lossy(&text), line, column, depth);
            continue;
        }
        if c == 96 {
            i += 1;
            template_text(&src, &mut i, &mut line, &mut line_start, &mut templates, depth);
            regex_allowed = false;
            visit(Kind::Template, "", line, column, depth);
            continue;
        }
        if c == 123 {
            visit(Kind::Punct, "{", line, column, depth);
            depth += 1;
            i += 1;
            regex_allowed = true;
            continue;
        }
        if c == 125 {
            i += 1;
            if templates.last() == Some(&depth) {
                templates.pop();
                template_text(&src, &mut i, &mut line, &mut line_start, &mut templates, depth);
                regex_allowed = false;
                continue;
            }
            depth = depth.saturating_sub(1);
            visit(Kind::Punct, "}", line, column, depth);
            regex_allowed = true;
            continue;
        }
        if is_word_start(c) {
            let mut j = i + 1;
            while j < n && is_word_char(src[j]) {
                j += 1;
            }
            let word = String::from_utf16_lossy(&src[i..j]);
            i = j;
            regex_allowed = regex_after_word(&word);
            visit(Kind::Word, &word, line, column, depth);
            continue;
        }
        if (48..=57).contains(&c) {
            while i < n && (is_word_char(src[i]) || src[i] == 46) {
                i += 1;
            }
            regex_allowed = false;
            continue;
        }
        i += 1;
        regex_allowed = c != 41 && c != 93;
        visit(Kind::Punct, &String::from_utf16_lossy(&[c]), line, column, depth);
    }
}

#[derive(Debug, Clone)]
pub struct AmbientModule {
    pub block: AmbientModuleBlock,
    pub file: String,
    pub script: bool,
    pub test: bool,
}

impl std::ops::Deref for AmbientModule {
    type Target = AmbientModuleBlock;
    fn deref(&self) -> &AmbientModuleBlock {
        &self.block
    }
}

struct Scanned {
    by_file: IndexMap<String, Vec<Rc<AmbientModule>>>,
    by_specifier: HashMap<String, Vec<Rc<AmbientModule>>>,
}

#[derive(Default)]
struct Memo {
    scanned: RefCell<Option<Rc<Scanned>>>,
    members: RefCell<HashMap<String, Rc<HashMap<String, Vec<NodeRef>>>>>,
    declaration_ids: RefCell<Option<Rc<HashMap<String, String>>>>,
    other_blocks: RefCell<HashMap<String, Rc<Vec<AmbientModuleBlock>>>>,
    declarations: RefCell<HashMap<String, Rc<Vec<Rc<AmbientModule>>>>>,
    merged: RefCell<HashMap<String, bool>>,
    namespaces: RefCell<HashMap<String, Rc<Vec<NamespaceBlock>>>>,
}

/// Cached namespace blocks of `file`.
pub fn namespace_blocks(file: &str, ctx: &Ctx) -> Rc<Vec<NamespaceBlock>> {
    let memo = ctx.memo::<Memo>();
    if let Some(hit) = memo.namespaces.borrow().get(file) {
        return Rc::clone(hit);
    }
    let blocks = match ctx.read_file(file) {
        Some(source) if re!(r"\b(?:namespace|module)\s+[A-Za-z_$]").is_match(&source) => scan_namespace_blocks(&source, is_declaration_file(file)),
        _ => Vec::new(),
    };
    let blocks = Rc::new(blocks);
    memo.namespaces.borrow_mut().insert(file.to_string(), Rc::clone(&blocks));
    blocks
}

fn scanned(ctx: &Ctx) -> Rc<Scanned> {
    let memo = ctx.memo::<Memo>();
    if let Some(hit) = memo.scanned.borrow().as_ref() {
        return Rc::clone(hit);
    }
    let mut by_file: IndexMap<String, Vec<Rc<AmbientModule>>> = IndexMap::new();
    let mut by_specifier: HashMap<String, Vec<Rc<AmbientModule>>> = HashMap::new();
    let mut files = ctx.get_all_files();
    files.sort();
    for file in files {
        if !is_declaration_file(&file) || !ctx.file_contains(&file, "declare") {
            continue;
        }
        let Some(source) = ctx.read_file(&file).filter(|s| !s.is_empty()) else { continue };
        if !re!(r#"\bdeclare\s+module\s*['"]"#).is_match(&source) {
            continue;
        }
        let scan = scan_ambient_modules(&source);
        let test = is_test_file(&file);
        let modules: Vec<Rc<AmbientModule>> =
            scan.blocks.into_iter().map(|block| Rc::new(AmbientModule { block, file: file.clone(), script: scan.script, test })).collect();
        if modules.is_empty() {
            continue;
        }
        for m in &modules {
            if m.pattern {
                continue;
            }
            by_specifier.entry(m.specifier.clone()).or_default().push(Rc::clone(m));
        }
        by_file.insert(file.clone(), modules);
    }
    let s = Rc::new(Scanned { by_file, by_specifier });
    *memo.scanned.borrow_mut() = Some(Rc::clone(&s));
    s
}

fn is_declaration_kind(k: NodeKind) -> bool {
    matches!(
        k,
        NodeKind::Function
            | NodeKind::Class
            | NodeKind::Interface
            | NodeKind::TypeAlias
            | NodeKind::Enum
            | NodeKind::Constant
            | NodeKind::Variable
            | NodeKind::Component
    )
}

/// Does the position `(line, column)` fall inside `block`?
pub fn in_block(line: u32, column: u32, block: &dyn Bounds) -> bool {
    let (sl, sc, el, ec) = block.bounds();
    let after_start = line > sl || (line == sl && column >= sc);
    let before_end = line < el || (line == el && column <= ec);
    after_start && before_end
}

fn is_block_level(node: &codegraph_types::Node) -> bool {
    is_declaration_kind(node.kind) && node.qualified_name == node.name
}

fn block_members(m: &AmbientModule, ctx: &Ctx) -> Rc<HashMap<String, Vec<NodeRef>>> {
    let memo = ctx.memo::<Memo>();
    let key = format!("{}\0{}\0{}", m.file, m.start_line, m.start_column);
    if let Some(hit) = memo.members.borrow().get(&key) {
        return Rc::clone(hit);
    }
    let mut nodes: Vec<NodeRef> =
        ctx.get_nodes_in_file(&m.file).iter().filter(|n| is_block_level(n) && in_block(n.start_line, n.start_column, m)).cloned().collect();
    nodes.sort_by(|a, b| a.start_line.cmp(&b.start_line).then(a.start_column.cmp(&b.start_column)));
    let mut members: HashMap<String, Vec<NodeRef>> = HashMap::new();
    for n in nodes {
        members.entry(n.name.clone()).or_default().push(n);
    }
    let members = Rc::new(members);
    memo.members.borrow_mut().insert(key, Rc::clone(&members));
    members
}

fn declarations_for(specifier: &str, from_file: &str, ctx: &Ctx) -> Rc<Vec<Rc<AmbientModule>>> {
    let s = scanned(ctx);
    let Some(all) = s.by_specifier.get(specifier) else { return Rc::new(Vec::new()) };
    let memo = ctx.memo::<Memo>();
    let key = format!("{specifier}\0{from_file}");
    if let Some(hit) = memo.declarations.borrow().get(&key) {
        return Rc::clone(hit);
    }
    let from_test = is_test_file(from_file);
    let dir = &from_file[..from_file.rfind('/').map_or(0, |i| i + 1)];
    let usable: Vec<Rc<AmbientModule>> = all.iter().filter(|m| m.script && (from_test || !m.test)).cloned().collect();
    let from: Vec<&str> = dir.split('/').collect();
    let shared = |file: &str| -> usize {
        let parts: Vec<&str> = file.split('/').collect();
        let mut k = 0;
        while k + 1 < from.len() && k + 1 < parts.len() && from[k] == parts[k] {
            k += 1;
        }
        k
    };
    let ordered: Vec<Rc<AmbientModule>> = if usable.len() < 2 {
        usable
    } else {
        let mut v: Vec<(Rc<AmbientModule>, usize, usize)> = usable.into_iter().enumerate().map(|(i, m)| (m.clone(), i, shared(&m.file))).collect();
        v.sort_by(|a, b| (a.0.test as i32).cmp(&(b.0.test as i32)).then(b.2.cmp(&a.2)).then(a.1.cmp(&b.1)));
        v.into_iter().map(|x| x.0).collect()
    };
    let ordered = Rc::new(ordered);
    memo.declarations.borrow_mut().insert(key, Rc::clone(&ordered));
    ordered
}

fn kind_rank(ref_kind: ReferenceKind, kind: NodeKind) -> i32 {
    let callable = matches!(kind, NodeKind::Function | NodeKind::Class | NodeKind::Constant | NodeKind::Variable | NodeKind::Component);
    let ty = matches!(kind, NodeKind::Interface | NodeKind::Class | NodeKind::TypeAlias | NodeKind::Enum);
    match ref_kind {
        ReferenceKind::Edge(EdgeKind::Calls) | ReferenceKind::FunctionRef => {
            if callable {
                0
            } else {
                1
            }
        }
        ReferenceKind::Edge(EdgeKind::Instantiates) => {
            if kind == NodeKind::Class {
                0
            } else if callable {
                1
            } else {
                2
            }
        }
        ReferenceKind::Edge(EdgeKind::Extends | EdgeKind::Implements) => {
            if matches!(kind, NodeKind::Class | NodeKind::Interface) {
                0
            } else {
                1
            }
        }
        ReferenceKind::Edge(EdgeKind::References) => {
            if ty {
                0
            } else {
                1
            }
        }
        _ => 0,
    }
}

pub struct AmbientWant<'a> {
    pub is_default: bool,
    pub is_namespace: bool,
    pub exported_name: &'a str,
    pub member_name: Option<&'a str>,
}

/// The node an ambient module declaring `specifier` exports under `want`.
pub fn find_ambient_module_export(specifier: &str, want: &AmbientWant, r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    if want.is_namespace && want.member_name.is_none_or(|m| m.is_empty() || m.contains('.')) {
        return None;
    }
    for m in declarations_for(specifier, &r.file_path, ctx).iter() {
        let members = block_members(m, ctx);
        let found: Option<Vec<NodeRef>> = if want.is_default {
            m.default_name.as_ref().and_then(|d| members.get(d).cloned())
        } else {
            let name = if want.is_namespace { want.member_name.unwrap() } else { want.exported_name };
            match m.export_aliases.get(name) {
                Some(local) => members.get(local).cloned(),
                None => members.get(name).map(|v| v.iter().filter(|n| n.is_exported == Some(true)).cloned().collect()),
            }
        };
        if let Some(found) = found.filter(|f| !f.is_empty()) {
            return found.into_iter().min_by_key(|n| kind_rank(r.reference_kind, n.kind));
        }
    }
    None
}

/// The file node of the first usable declaration of `specifier` (not `from_file` itself).
pub fn find_ambient_module_file(specifier: &str, from_file: &str, ctx: &Ctx) -> Option<NodeRef> {
    let decls = declarations_for(specifier, from_file, ctx);
    let m = decls.first()?;
    if m.file == from_file {
        return None;
    }
    ctx.get_nodes_in_file(&m.file).iter().find(|n| n.kind == NodeKind::File).cloned()
}

pub fn ambient_module_declares(specifier: &str, name: &str, is_default: bool, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    find_ambient_module_export(specifier, &AmbientWant { is_default, is_namespace: false, exported_name: name, member_name: None }, r, ctx).is_some()
}

fn block_declaring(node: &codegraph_types::Node, ctx: &Ctx) -> Option<Rc<AmbientModule>> {
    if !is_block_level(node) || !is_declaration_file(&node.file_path) {
        return None;
    }
    scanned(ctx).by_file.get(&node.file_path)?.iter().find(|m| in_block(node.start_line, node.start_column, &***m)).cloned()
}

/// Node id → specifier of the ambient module block declaring it.
pub fn ambient_module_declarations(ctx: &Ctx) -> Rc<HashMap<String, String>> {
    let memo = ctx.memo::<Memo>();
    if let Some(hit) = memo.declaration_ids.borrow().as_ref() {
        return Rc::clone(hit);
    }
    let mut ids: HashMap<String, String> = HashMap::new();
    let s = scanned(ctx);
    for (file, modules) in s.by_file.iter() {
        for node in ctx.get_nodes_in_file(file).iter() {
            if !is_block_level(node) {
                continue;
            }
            if let Some(m) = modules.iter().find(|m| in_block(node.start_line, node.start_column, &***m)) {
                ids.insert(node.id.clone(), m.specifier.clone());
            }
        }
    }
    let ids = Rc::new(ids);
    *memo.declaration_ids.borrow_mut() = Some(Rc::clone(&ids));
    ids
}

pub fn is_in_module_block(file: &str, line: u32, column: u32, specifier: &str, ctx: &Ctx) -> bool {
    let s = scanned(ctx);
    if let Some(mods) = s.by_file.get(file) {
        return mods.iter().any(|m| m.specifier == specifier && in_block(line, column, &***m));
    }
    let blocks: Rc<Vec<AmbientModuleBlock>> = if !is_declaration_file(file) {
        let memo = ctx.memo::<Memo>();
        let cached = memo.other_blocks.borrow().get(file).cloned();
        match cached {
            Some(b) => b,
            None => {
                let blocks = match ctx.read_file(file) {
                    Some(source) if re!(r#"\bdeclare\s+module\s*['"]"#).is_match(&source) => scan_ambient_modules(&source).blocks,
                    _ => Vec::new(),
                };
                let blocks = Rc::new(blocks);
                memo.other_blocks.borrow_mut().insert(file.to_string(), Rc::clone(&blocks));
                blocks
            }
        }
    } else {
        Rc::new(Vec::new())
    };
    blocks.iter().any(|b| b.specifier == specifier && in_block(line, column, b))
}

/// Is `node` declared more than once across the ambient blocks of its module (declaration merging)?
pub fn is_merged_ambient_declaration(node: &codegraph_types::Node, ctx: &Ctx) -> bool {
    let memo = ctx.memo::<Memo>();
    if let Some(&hit) = memo.merged.borrow().get(&node.id) {
        return hit;
    }
    let mut merged = false;
    if let Some(m) = block_declaring(node, ctx) {
        if !m.pattern {
            let mut count = 0;
            let s = scanned(ctx);
            for other in s.by_specifier.get(&m.specifier).map(|v| v.as_slice()).unwrap_or(&[]) {
                count += block_members(other, ctx).get(&node.name).map_or(0, |v| v.len());
                if count > 1 {
                    merged = true;
                    break;
                }
            }
        }
    }
    memo.merged.borrow_mut().insert(node.id.clone(), merged);
    merged
}
