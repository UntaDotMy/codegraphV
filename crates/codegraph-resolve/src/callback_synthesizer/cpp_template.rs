//! A C++ template parameter reaches the classes the template is instantiated with (upstream
//! 01a4931, #3196; port of `src/resolution/cpp-template-instantiations.ts`).
//!
//! `Txn::PutEntity(…)` in rocksdb's `template <typename Txn> class SecondaryIndexMixin` is a
//! member of whatever class the template is given, so no lookup at the call can answer it. What
//! decides the target is written elsewhere, in `new SecondaryIndexMixin<WriteCommittedTxn>(…)`,
//! and with several instantiations there is no one target: each really runs its own. So this is
//! a whole-graph pass over the references that failed, not a resolver: one reference gets one
//! edge per class the project writes as the argument.
//!
//! What it reads, from each failed reference that goes through a parameter:
//!  - The template that declares the parameter, and which parameter it is: the function's own
//!    head, the class templates around it, or, defined outside its class, the head that matches
//!    `C<T>::` (`cpp_template_heads`). A gtest `TYPED_TEST(Fixture, Name)` body's `TypeParam` is
//!    the fixture's first.
//!  - An alias of a member of a parameter counts as that member:
//!    `using KeyGen = typename TypeParam::KeyGen;`.
//!  - A variable declared with such a type counts too: `shard.Clear()` on a `CacheShard& shard`.
//!  - A class template's instantiations are wherever `Name<…>` is written and `Name` is that
//!    class by C++ lookup. A function template's are the calls the graph already holds, read for
//!    the `f<Foo>(…)` they write, or for the type of the argument that decides what `f(x)` deduces.
//!  - An argument that is itself a parameter of a template around the site is followed to that
//!    template's instantiations, a few hops at most.
//!  - An argument is a project class by the lookup a declared type gets (`cpp_class_written`), a
//!    class local to the function that writes it first, in a file that sees it. Anything else
//!    (`std::string`, `int`, a pointer) gives no edge.
//!  - An instantiation a specialization takes runs the specialization's members.
//!
//! The argument's site is the edge's `registeredAt`: a file holding only an instantiation is what
//! the edge depends on, so a sync that touches it runs this pass again.
//!
//! Offsets into a file's text are byte offsets into its comment-blanked copy; a node's or a
//! reference's column (UTF-16) is converted where it meets that text.

use super::common::heuristic_edge;
use crate::cpp_constructor::match_cpp_constructor;
use crate::cpp_namespaces::{cpp_macro_namespace_frames, cpp_namespace_alias_walk};
use crate::cpp_raw_strings::{cpp_literals_blanked, cpp_non_code_lines};
use crate::cpp_supertypes::{cpp_class_written, CppNameSite};
use crate::cpp_type_aliases::{
    cpp_alias_written_at, cpp_aliased_class, cpp_class_named, cpp_declares_local_alias, cpp_parent_scope, cpp_template_heads,
    cpp_template_parameters_in, cpp_type_named, cpp_type_names_declared, cpp_type_segments, is_cpp_pointer_type, resolve_cpp_aliased_type,
    strip_cpp_template_arguments, CppNamedClass, CppNamedType, CppTemplateParameter, Visible,
};
use crate::ctx::{q, Ctx};
use crate::meta;
use crate::name_matcher::cpp_locals::is_cpp_local_name;
use crate::name_matcher::cpp_method::cpp_method_of;
use crate::name_matcher::cpp_overload::{cpp_argument_type_text, cpp_call_template_arguments, cpp_overload_parameters};
use crate::name_matcher::cpp_text::{cpp_call_arguments, split_cpp_top_level};
use crate::name_matcher::cpp_types::cpp_callers_member;
use crate::types::{NodeRef, Tri, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::utf16_from;
use codegraph_types::{Edge, EdgeKind, Language, Node, NodeKind, ReferenceKind};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// How many classes one parameter may reach before it reaches none. A template written with
/// more than this is a container or a test helper, whose every instantiation is no hop a flow
/// takes. A template over the cap gets no edges, not some.
pub const CPP_TEMPLATE_ARGUMENT_CAP: usize = 40;

/// How many files may write a class template's name, or how many calls a function template may
/// take, before it reaches nothing either: what a template reaches can only be known by reading
/// all of that. A parameter whose walk reaches such a template gets no edges, not some.
pub const CPP_TEMPLATE_SITE_CAP: usize = 1024;

/// How many templates an argument is followed through, from a parameter to the parameter it is
/// written as.
const MAX_TEMPLATE_HOPS: usize = 4;
/// How far an argument list may run: a gtest list of types takes a line per type.
const MAX_ARGUMENT_LIST: usize = 64 * 1024;
/// How much text before a `Name<` tells what is written there: a template head, at most.
const HEAD_WINDOW: usize = 400;
/// How many files' text and declarations are kept: every file of most projects, and a bound on
/// the rest.
const MAX_FILES_HELD: usize = 4096;
/// How many templates' instantiations are kept.
const MAX_TEMPLATES_HELD: usize = 1024;

/// How the index names a class it reads no name for, such as a specialization.
const ANONYMOUS: &str = "<anonymous>";

fn is_class_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Class | NodeKind::Struct | NodeKind::Union)
}

fn is_function_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Function | NodeKind::Method)
}

/// What a template argument is written in, and so what a site can sit in.
fn is_declaration_kind(kind: NodeKind) -> bool {
    is_class_kind(kind) || is_function_kind(kind) || matches!(kind, NodeKind::TypeAlias | NodeKind::Variable | NodeKind::Constant)
}

fn is_cpp_file(file: &str) -> bool {
    re!(r"\.(?:c|h|cc|cpp|cxx|hpp|hh|hxx|cppm|ipp|inl|tcc)$", "i").is_match(file)
}

fn is_identifier(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && (b[0].is_ascii_alphabetic() || b[0] == b'_') && b.iter().all(|&c| is_word(c))
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

/// The largest char boundary of `s` at or below `at`.
fn floor_boundary(s: &str, at: usize) -> usize {
    let mut at = at.min(s.len());
    while !s.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// The smallest char boundary of `s` at or above `at`.
fn ceil_boundary(s: &str, at: usize) -> usize {
    let mut at = at.min(s.len());
    while !s.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// `s[from..to]`, with both ends moved onto char boundaries.
fn window(s: &str, from: usize, to: usize) -> &str {
    let from = floor_boundary(s, from);
    &s[from..ceil_boundary(s, to.max(from))]
}

fn strip_spaces(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TemplateKind {
    Class,
    Alias,
    Function,
}

/// A template's head, as the declaration it belongs to writes it.
struct Template {
    /// One per template, whichever declaration of it a reference is in.
    key: String,
    kind: TemplateKind,
    name: String,
    /// The declaration of a class or alias template; the very function for a function template.
    node: NodeRef,
    parameters: Rc<Vec<CppTemplateParameter>>,
    /// What a specialization writes after the name (`T*`, `int`); `None` for the template itself.
    pattern: Option<Vec<String>>,
}

type TemplateRef = Rc<Template>;

/// A parameter of a template.
#[derive(Clone)]
struct Slot {
    template: TemplateRef,
    index: usize,
}

fn slot_key(slot: &Slot) -> String {
    format!("{}#{}#{}", slot.template.key, slot.index, slot.template.pattern.as_ref().map(|p| p.join(",")).unwrap_or_default())
}

/// A reference that goes through a parameter.
struct Through {
    slot: Slot,
    /// The parameter as the code around the reference names it.
    parameter: String,
    /// Member types between the parameter and what is named: `Key` of `T::Key::Make`.
    types: Vec<String>,
    /// The member called: `Make`; `None` for the type itself.
    member: Option<String>,
    /// The argument count of a constructor call: `T x(a, b);`.
    arity: Option<u32>,
}

/// Where a type name is written: what it is looked up from.
#[derive(Clone)]
struct Place {
    file: String,
    line: usize,
    scope: String,
    /// The declaration around it.
    around: Option<NodeRef>,
    /// The templates around it, innermost first.
    templates: Rc<Vec<TemplateRef>>,
    /// The names that are parameters there, those of a head no node holds as well.
    parameters: Rc<HashSet<String>>,
    r: UnresolvedRef,
}

impl Place {
    fn at_line(&self, line: usize) -> Rc<Place> {
        Rc::new(Place { line, ..self.clone() })
    }
}

/// A template argument as written.
#[derive(Clone)]
struct Argument {
    text: String,
    place: Rc<Place>,
}

/// A written `Name<…>` that names a template.
struct Instantiation {
    args: Vec<Argument>,
}

/// A call of a function template.
struct Call {
    r: UnresolvedRef,
    place: Rc<Place>,
    /// The template arguments it writes; `None` for none, leaving them to be deduced.
    args: Option<Vec<Argument>>,
}

/// One argument a specialization writes: `T`, `T*`, `int`, or a condition on `T`.
struct PatternItem {
    text: String,
    /// The parameter it is, whole.
    parameter: Option<String>,
    pack: bool,
    /// What only the arguments decide.
    condition: bool,
    /// How it reads a parameter out of an argument, with the parameters it does.
    shape: Option<Re>,
    words: Vec<String>,
}

/// A specialization of a class template: `template <class T> class Name<T*>`.
struct Specialization {
    pattern: Vec<PatternItem>,
    /// The arguments as they compare, to find a declaration's own.
    written: String,
}

/// What a class template's name is written as across the project.
#[derive(Default)]
struct WrittenTemplate {
    instantiations: Vec<Instantiation>,
    specializations: Vec<Specialization>,
}

/// A file's text with comments blanked, and where each of its lines starts.
struct Source {
    text: String,
    newlines: Vec<usize>,
}

impl Source {
    fn new(text: String) -> Source {
        let newlines = text.bytes().enumerate().filter(|(_, b)| *b == b'\n').map(|(i, _)| i).collect();
        Source { text, newlines }
    }

    /// The 1-based line of byte offset `at`.
    fn line_at(&self, at: usize) -> usize {
        self.newlines.partition_point(|&p| p < at) + 1
    }

    /// The byte offset line `line` (1-based) starts at.
    fn line_start(&self, line: usize) -> usize {
        if line <= 1 {
            0
        } else {
            self.newlines.get(line - 2).map_or(self.text.len(), |p| p + 1)
        }
    }
}

/// The arguments between the `<` at `open` and the `>` at `close`, split at the top level, each
/// with where it starts.
fn arguments_between(text: &str, open: usize, close: usize) -> Vec<(String, usize)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut parens = 0i32;
    let mut start = open + 1;
    let push = |out: &mut Vec<(String, usize)>, start: usize, end: usize| {
        let raw = &text[start..end];
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let lead = raw.len() - raw.trim_start().len();
            out.push((trimmed.to_string(), start + lead));
        }
    };
    let mut i = open + 1;
    while i < close {
        let c = b[i];
        if c == b'(' || c == b'[' {
            parens += 1;
        } else if c == b')' || c == b']' {
            parens -= 1;
        } else if parens > 0 {
        } else if c == b'<' {
            depth += 1;
        } else if c == b'>' && b[i - 1] != b'-' {
            depth -= 1;
        } else if c == b',' && depth == 0 {
            push(&mut out, start, i);
            start = i + 1;
        }
        i += 1;
    }
    push(&mut out, start, close);
    out
}

/// The `>` closing the argument list opened at `open`, or `None` when `<` opens none there.
fn closing_angle(text: &str, open: usize) -> Option<usize> {
    let b = text.as_bytes();
    let mut depth = 0i32;
    let mut parens = 0i32;
    let end = b.len().min(open + MAX_ARGUMENT_LIST);
    for i in open..end {
        let c = b[i];
        if c == b'(' || c == b'[' {
            parens += 1;
        } else if c == b')' || c == b']' {
            parens -= 1;
            if parens < 0 {
                return None;
            }
        } else if parens > 0 {
        } else if c == b'<' {
            depth += 1;
        } else if c == b'>' {
            if i > 0 && b[i - 1] == b'-' {
                continue;
            }
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        } else if c == b';' || c == b'{' || c == b'}' {
            return None;
        }
    }
    None
}

/// Add the names `text` writes right before a `<`.
fn add_names_before_angle(text: &str, names: &mut HashSet<String>) {
    let b = text.as_bytes();
    for (open, _) in text.match_indices('<') {
        let mut end = open;
        while end > 0 && is_space(b[end - 1]) {
            end -= 1;
        }
        let mut start = end;
        while start > 0 && is_word(b[start - 1]) {
            start -= 1;
        }
        if start < end {
            names.insert(text[start..end].to_string());
        }
    }
}

/// The qualifiers `A<T>::B::` written right before `end`, outermost first, each with the
/// arguments it writes.
fn qualifiers_before(text: &str, end: usize) -> Vec<(String, Option<Vec<String>>)> {
    let b = text.as_bytes();
    let mut out: Vec<(String, Option<Vec<String>>)> = Vec::new();
    let mut at = end;
    loop {
        while at > 0 && is_space(b[at - 1]) {
            at -= 1;
        }
        if at < 2 || &b[at - 2..at] != b"::" {
            return out;
        }
        at -= 2;
        while at > 0 && is_space(b[at - 1]) {
            at -= 1;
        }
        let mut args = None;
        if at > 0 && b[at - 1] == b'>' {
            let mut depth = 0i32;
            let floor = at.saturating_sub(MAX_ARGUMENT_LIST);
            let mut found = None;
            let mut i = at - 1;
            loop {
                if b[i] == b'>' {
                    depth += 1;
                } else if b[i] == b'<' {
                    depth -= 1;
                    if depth == 0 {
                        found = Some(i);
                        break;
                    }
                }
                if i == floor {
                    break;
                }
                i -= 1;
            }
            let Some(open) = found else { return out };
            args = Some(arguments_between(text, open, at - 1).into_iter().map(|(t, _)| t).collect());
            at = open;
            while at > 0 && is_space(b[at - 1]) {
                at -= 1;
            }
        }
        let mut start = at;
        while start > 0 && is_word(b[start - 1]) {
            start -= 1;
        }
        if start == at {
            return out;
        }
        out.insert(0, (text[start..at].to_string(), args));
        at = start;
    }
}

/// A written template argument as it compares: spaced only between words.
fn argument_key(arg: &str) -> String {
    let tight = re!(r"\s*([^\w\s])\s*", "g").replace_all(arg, |c| Some(c.text(1).unwrap_or("").to_string()));
    re!(r"\s+", "g").replace_all_str(&tight, " ")
}

fn without_pack(arg: &str) -> &str {
    let t = arg.trim_end();
    t.strip_suffix("...").map(str::trim_end).unwrap_or(arg)
}

/// Do the arguments a declaration writes for its template repeat its head: `C<T, U>` under
/// `template <class T, class U>`?
fn is_own_head(args: &[String], head: &[CppTemplateParameter]) -> bool {
    args.len() == head.len() && args.iter().zip(head).all(|(a, p)| p.name.as_deref() == Some(without_pack(a)))
}

/// A specialization, from the arguments it writes (as they compare) and its head.
fn specialization_of(args: &[String], parameters: &[CppTemplateParameter]) -> Specialization {
    let names: HashSet<&str> = parameters.iter().filter_map(|p| p.name.as_deref()).collect();
    let pattern = args
        .iter()
        .map(|text| {
            let bare = text.strip_suffix("...").unwrap_or(text);
            if names.contains(bare) {
                return PatternItem { text: text.clone(), parameter: Some(bare.to_string()), pack: bare != text, condition: false, shape: None, words: Vec::new() };
            }
            let words: Vec<String> =
                re!(r"[A-Za-z_]\w*", "g").captures_iter(text).filter_map(|m| m.text(0).map(String::from)).filter(|w| names.contains(w.as_str())).collect();
            if words.is_empty() {
                return PatternItem { text: text.clone(), parameter: None, pack: false, condition: false, shape: None, words };
            }
            if re!(r"\b(?:enable_if|void_t|disjunction|conjunction|negation|conditional|requires)\w*|\bis_\w+").is_match(text) {
                return PatternItem { text: text.clone(), parameter: None, pack: false, condition: true, shape: None, words };
            }
            // `T*` reads `(.+)\*`: each parameter word captures what the argument writes there.
            let mut shape = String::from("^");
            let mut last = 0;
            for m in re!(r"[A-Za-z_]\w*", "g").captures_iter(text) {
                let Some(w) = m.get(0) else { continue };
                shape.push_str(&escape_regex(&text[last..w.start()]));
                if names.contains(w.as_str()) {
                    shape.push_str("(.+)");
                } else {
                    shape.push_str(&escape_regex(w.as_str()));
                }
                last = w.end();
            }
            shape.push_str(&escape_regex(&text[last..]));
            shape.push('$');
            PatternItem { text: text.clone(), parameter: None, pack: false, condition: false, shape: Re::try_new(&shape, "").ok(), words }
        })
        .collect();
    Specialization { pattern, written: args.join(",") }
}

/// A `namespace a { … }` block of a blanked file, by byte offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CppNamespaceBlock {
    pub start: usize,
    pub end: usize,
    pub path: Vec<String>,
}

/// The `namespace a { … }` blocks of a blanked file, by offset, outermost first. (A blanked
/// comment is a run of spaces, so each run in a head must lead to a name character.)
pub fn cpp_namespace_blocks(text: &str) -> Vec<CppNamespaceBlock> {
    let mut blocks: Vec<CppNamespaceBlock> = Vec::new();
    // Each open brace: the block it opens, or `None` for any other brace.
    let mut open: Vec<Option<usize>> = Vec::new();
    let mut path: Vec<String> = Vec::new();
    for m in re!(r"\bnamespace\b(?:\s+([A-Za-z_][\w:]*(?:\s+[\w:]+)*))?\s*\{|[{}]", "g").captures_iter(text) {
        let Some(whole) = m.get(0) else { continue };
        match whole.as_str() {
            "{" => open.push(None),
            "}" => {
                if let Some(Some(block)) = open.pop() {
                    blocks[block].end = whole.start();
                    path = open.iter().rev().flatten().next().map(|&b| blocks[b].path.clone()).unwrap_or_default();
                }
            }
            _ => {
                let names = strip_spaces(m.text(1).unwrap_or(""));
                let mut block_path = path.clone();
                if !names.is_empty() {
                    block_path.extend(names.split("::").map(String::from));
                }
                blocks.push(CppNamespaceBlock { start: whole.start(), end: text.len(), path: block_path.clone() });
                open.push(Some(blocks.len() - 1));
                path = block_path;
            }
        }
    }
    blocks
}

/// Who includes each file, by the include edges the graph holds, and what each file can see.
/// Shared with the name lookups a site makes (`CppNameSite::sees`).
struct Includes {
    included_by: HashMap<String, Vec<String>>,
    includers: RefCell<HashMap<String, Rc<HashSet<String>>>>,
    seen: RefCell<HashMap<(String, String), bool>>,
}

impl Includes {
    fn new(edges: Vec<(String, String)>) -> Includes {
        let mut included_by: HashMap<String, Vec<String>> = HashMap::new();
        for (source, target) in edges {
            included_by.entry(target).or_default().push(source);
        }
        Includes { included_by, includers: RefCell::default(), seen: RefCell::default() }
    }

    /// Every file that includes `file`, directly or through others.
    fn includers_of(&self, file: &str) -> Rc<HashSet<String>> {
        if let Some(hit) = self.includers.borrow().get(file) {
            return Rc::clone(hit);
        }
        let mut found: HashSet<String> = HashSet::new();
        let mut queue: Vec<String> = vec![file.to_string()];
        let mut i = 0;
        while i < queue.len() {
            if let Some(sources) = self.included_by.get(&queue[i]) {
                for source in sources {
                    if found.insert(source.clone()) {
                        queue.push(source.clone());
                    }
                }
            }
            i += 1;
        }
        let found = Rc::new(found);
        self.includers.borrow_mut().insert(file.to_string(), Rc::clone(&found));
        found
    }

    /// Can code in `file` see `decl`: its own, what it includes, or a header beside it in one
    /// translation unit? A project's vendored copy of itself declares the same names, and nothing
    /// it declares is any other file's.
    fn sees(&self, file: &str, decl: &Node) -> bool {
        if decl.file_path == file {
            return true;
        }
        let key = (file.to_string(), decl.file_path.clone());
        if let Some(&hit) = self.seen.borrow().get(&key) {
            return hit;
        }
        let includers = self.includers_of(&decl.file_path);
        let mine = self.includers_of(file);
        let seen = includers.contains(file) || mine.contains(&decl.file_path) || mine.iter().any(|unit| includers.contains(unit));
        self.seen.borrow_mut().insert(key, seen);
        seen
    }
}

/// A map that forgets everything once it holds `cap` entries: a bound on what a pass keeps, at
/// the cost of reading again what it dropped.
struct Held<V> {
    map: RefCell<HashMap<String, V>>,
    cap: usize,
}

impl<V: Clone> Held<V> {
    fn new(cap: usize) -> Held<V> {
        Held { map: RefCell::default(), cap }
    }

    fn get(&self, key: &str) -> Option<V> {
        self.map.borrow().get(key).cloned()
    }

    fn set(&self, key: &str, value: V) {
        let mut map = self.map.borrow_mut();
        if map.len() >= self.cap && !map.contains_key(key) {
            map.clear();
        }
        map.insert(key.to_string(), value);
    }
}

struct TemplateInstantiations<'a> {
    ctx: &'a Ctx,
    includes: Rc<Includes>,
    sources: Held<Option<Rc<Source>>>,
    declarations: Held<Rc<Vec<NodeRef>>>,
    namespaces: Held<Rc<Vec<CppNamespaceBlock>>>,
    templates: RefCell<HashMap<String, Rc<Vec<TemplateRef>>>>,
    written: Held<Rc<WrittenTemplate>>,
    calls: RefCell<HashMap<String, Option<Rc<Vec<Call>>>>>,
    values: RefCell<HashMap<String, Rc<HashSet<String>>>>,
    /// The C and C++ files, each with the names it writes before a `<`, read once.
    files_naming: HashMap<String, Vec<String>>,
    /// The files that use gtest's typed tests.
    typed_files: Vec<String>,
    suites: RefCell<Option<Rc<HashMap<String, Vec<(String, usize, String)>>>>>,
    blanked: RefCell<Option<(String, usize, String)>>,
    deduced_shapes: RefCell<HashMap<String, Rc<Option<Re>>>>,
}

impl<'a> TemplateInstantiations<'a> {
    /// Read every C and C++ file once, for the names each writes before a `<`, and return the
    /// names each that uses templates declares as a parameter or an alias.
    fn read(ctx: &'a Ctx) -> (TemplateInstantiations<'a>, IndexMap<String, Rc<HashSet<String>>>) {
        let mut declared: IndexMap<String, Rc<HashSet<String>>> = IndexMap::new();
        let mut files_naming: HashMap<String, Vec<String>> = HashMap::new();
        let mut typed_files = Vec::new();
        // (A lookup of a qualified name wants the namespace aliases, which are a walk over every
        // file too.)
        let timed = synth_timed();
        let started = std::time::Instant::now();
        let mut scan = std::time::Duration::ZERO;
        let mut walk_add = std::time::Duration::ZERO;
        let mut aliases = cpp_namespace_alias_walk(ctx);
        for file in ctx.get_all_files() {
            if !is_cpp_file(&file) {
                continue;
            }
            let read_at = std::time::Instant::now();
            let Some(text) = ctx.scan_file(&file) else { continue };
            scan += read_at.elapsed();
            if text.is_empty() {
                continue;
            }
            let walk_at = std::time::Instant::now();
            if let Some(walk) = aliases.as_mut() {
                walk.add(&file, &text);
            }
            walk_add += walk_at.elapsed();
            let mut names = HashSet::new();
            add_names_before_angle(&text, &mut names);
            for name in names {
                files_naming.entry(name).or_default().push(file.clone());
            }
            let typed = text.contains("TYPED_TEST");
            if typed {
                typed_files.push(file.clone());
            }
            if !typed && !text.contains("template") {
                continue;
            }
            let mut types = cpp_type_names_declared(&text);
            if typed {
                types.insert("TypeParam".to_string());
            }
            declared.insert(file, Rc::new(types));
        }
        let loop_ms = started.elapsed().as_millis();
        let finish_at = std::time::Instant::now();
        if let Some(walk) = aliases {
            walk.finish(ctx);
        }
        let finish_ms = finish_at.elapsed().as_millis();
        let includes_at = std::time::Instant::now();
        let includes = if declared.is_empty() { Vec::new() } else { q("getCppIncludeEdges", ctx.store().get_cpp_include_edges()) };
        if timed {
            eprintln!(
                "[phase-timing]       cppTemplateEdges read: files {loop_ms}ms (read {}ms, alias walk {}ms), alias finish {finish_ms}ms, includes {}ms ({} edges)",
                scan.as_millis(),
                walk_add.as_millis(),
                includes_at.elapsed().as_millis(),
                includes.len()
            );
        }
        let reader = TemplateInstantiations {
            ctx,
            includes: Rc::new(Includes::new(includes)),
            sources: Held::new(MAX_FILES_HELD),
            declarations: Held::new(MAX_FILES_HELD),
            namespaces: Held::new(MAX_FILES_HELD),
            templates: RefCell::default(),
            written: Held::new(MAX_TEMPLATES_HELD),
            calls: RefCell::default(),
            values: RefCell::default(),
            files_naming,
            typed_files,
            suites: RefCell::default(),
            blanked: RefCell::default(),
            deduced_shapes: RefCell::default(),
        };
        (reader, declared)
    }

    fn lines(&self, file: &str) -> Rc<Vec<String>> {
        self.ctx.get_file_lines(file).unwrap_or_default()
    }

    /// Lines `from..to` (0-based, end exclusive) of `file`, joined.
    fn joined(&self, file: &str, from: usize, to: usize) -> String {
        let lines = self.lines(file);
        let to = to.min(lines.len());
        if from >= to {
            return String::new();
        }
        lines[from..to].join("\n")
    }

    fn sees(&self, file: &str, decl: &Node) -> bool {
        self.includes.sees(file, decl)
    }

    /// The classes of a qualified name, in file and line order: the walks take the first that fits.
    fn classes_named(&self, qualified: &str) -> Vec<NodeRef> {
        let mut out: Vec<NodeRef> = self.ctx.get_nodes_by_qualified_name(qualified).iter().filter(|n| is_class_kind(n.kind)).cloned().collect();
        out.sort_by(|a, b| a.file_path.cmp(&b.file_path).then(a.start_line.cmp(&b.start_line)));
        out
    }

    /// The declaration of a class or function around `node`, whose lines hold it, in the scope
    /// its qualified name gives.
    fn declaration_around(&self, node: &Node) -> Option<NodeRef> {
        let mut scope = cpp_parent_scope(&node.qualified_name);
        while !scope.is_empty() {
            let named = self.ctx.get_nodes_by_qualified_name(&scope);
            let found = named.iter().find(|n| {
                n.id != node.id
                    && (is_class_kind(n.kind) || is_function_kind(n.kind))
                    && n.file_path == node.file_path
                    && n.start_line <= node.start_line
                    && n.end_line >= node.end_line
            });
            if let Some(found) = found {
                return Some(found.clone());
            }
            scope = cpp_parent_scope(&scope);
        }
        None
    }

    /// The class template a class declaration's head belongs to. A specialization has no name of
    /// its own in the index (`<anonymous>`), and a nested class defined outside its template
    /// names another class, so that is read from what the declaration writes,
    /// `struct Traits<T*, int>`, and found as any name is.
    fn template_of_class(&self, decl: &NodeRef, head: &Rc<Vec<CppTemplateParameter>>) -> Option<TemplateRef> {
        let start = (decl.start_line as usize).saturating_sub(1);
        let joined = self.joined(&decl.file_path, start, start + 8);
        let text = utf16_from(&joined, decl.start_column as usize);
        let written = re!(r"^(?:class|struct|union)\s+(?:[A-Z][A-Z0-9_]*(?:\([^()]*\))?\s+)*((?:[A-Za-z_]\w*\s*::\s*)*[A-Za-z_]\w*)\s*<").captures(text);
        let open = written.as_ref().and_then(|w| w.get(0)).map(|m| m.end() - 1);
        let close = open.and_then(|o| closing_angle(text, o));
        let args: Option<Vec<String>> = match (open, close) {
            (Some(o), Some(c)) => Some(arguments_between(text, o, c).into_iter().map(|(t, _)| t).collect()),
            _ => None,
        };
        let pattern = args.as_ref().filter(|a| !is_own_head(a, head)).cloned();
        // `template <class T> struct List<T>::Node {`: its head is `List`'s.
        let nested = close.is_some_and(|c| re!(r"^\s*::").is_match(&text[c + 1..]));
        if decl.name != ANONYMOUS && !nested {
            return Some(self.class_template(decl, head, pattern));
        }
        args.as_ref()?;
        let place = self.place_in(decl, decl.start_line as usize);
        let mut site = self.site(&place);
        site.scope = cpp_parent_scope(&decl.qualified_name);
        let name = strip_spaces(written.as_ref().and_then(|w| w.text(1)).unwrap_or(""));
        match cpp_class_written(&name, &site, &place.r, self.ctx) {
            Tri::Value(found) if found.exact && is_class_kind(found.node.kind) => Some(self.class_template(&found.node, head, pattern)),
            _ => None,
        }
    }

    fn class_template(&self, decl: &NodeRef, parameters: &Rc<Vec<CppTemplateParameter>>, pattern: Option<Vec<String>>) -> TemplateRef {
        Rc::new(Template {
            key: format!("class:{}@{}", decl.qualified_name, decl.file_path),
            kind: TemplateKind::Class,
            name: decl.name.clone(),
            node: decl.clone(),
            parameters: Rc::clone(parameters),
            pattern,
        })
    }

    fn function_template(&self, node: &NodeRef, parameters: &Rc<Vec<CppTemplateParameter>>) -> TemplateRef {
        Rc::new(Template {
            key: format!("function:{}", node.id),
            kind: TemplateKind::Function,
            name: node.name.clone(),
            node: node.clone(),
            parameters: Rc::clone(parameters),
            pattern: None,
        })
    }

    /// The class templates a definition outside its class writes in its declarator
    /// (`template <class T> void C<T>::f()`), innermost first, after a member template's own head.
    fn out_of_line_templates(&self, node: &NodeRef, heads: &[Rc<Vec<CppTemplateParameter>>]) -> Vec<TemplateRef> {
        let start = (node.start_line as usize).saturating_sub(1);
        let joined = self.joined(&node.file_path, start, start + 12);
        let text = utf16_from(&joined, node.start_column as usize);
        let written: Vec<(String, Vec<String>)> = find_qualified_name(text, &node.name)
            .map(|at| qualifiers_before(text, at + 2).into_iter().filter_map(|(n, a)| a.map(|a| (n, a))).collect())
            .unwrap_or_default();
        let mut out = Vec::new();
        if heads.len() != written.len() && heads.len() != written.len() + 1 {
            return out;
        }
        if heads.len() == written.len() + 1 {
            out.push(self.function_template(node, &heads[heads.len() - 1]));
        }
        let segments: Vec<&str> = node.qualified_name.split("::").collect();
        for i in (0..written.len()).rev() {
            let (name, args) = &written[i];
            let head = &heads[i];
            // `segments.lastIndexOf(q.name, segments.length - 2)`.
            let at = segments.len().checked_sub(2).and_then(|last| (0..=last).rev().find(|&k| segments[k] == name));
            let decls = at.map(|at| self.classes_named(&segments[..=at].join("::"))).unwrap_or_default();
            let decl = decls.iter().find(|n| n.file_path == node.file_path).or_else(|| decls.iter().find(|n| self.sees(&node.file_path, n)));
            let Some(decl) = decl else { continue };
            if head.is_empty() {
                continue;
            }
            let pattern = if is_own_head(args, head) { None } else { Some(args.clone()) };
            out.push(self.class_template(decl, head, pattern));
        }
        out
    }

    /// The typed fixture a gtest `TYPED_TEST` body is a member of, with `TypeParam` as its first
    /// parameter.
    fn typed_test_template(&self, caller: &NodeRef) -> Option<TemplateRef> {
        if caller.kind != NodeKind::Function || !re!(r"^TYPED_TEST(?:_P)?$").is_match(&caller.name) {
            return None;
        }
        let start = (caller.start_line as usize).saturating_sub(1);
        let lines = self.lines(&caller.file_path);
        let head = lines[start.min(lines.len())..(start + 2).min(lines.len())].join(" ");
        let fixture = re!(r"\bTYPED_TEST(?:_P)?\s*\(\s*((?:::\s*)?[A-Za-z_]\w*(?:\s*::\s*[A-Za-z_]\w*)*)\s*,").captures(&head)?.text(1).map(strip_spaces)?;
        let place = self.place_in(caller, caller.start_line as usize);
        match cpp_class_written(&fixture, &self.site(&place), &place.r, self.ctx) {
            Tri::Value(found) if found.exact && is_class_kind(found.node.kind) => {
                let parameters = Rc::new(vec![CppTemplateParameter { name: Some("TypeParam".to_string()), pack: false, fallback: None }]);
                Some(self.class_template(&found.node, &parameters, None))
            }
            _ => None,
        }
    }

    /// The templates around a declaration, innermost first, each with the parameters its body sees.
    fn templates_around(&self, node: &NodeRef) -> Rc<Vec<TemplateRef>> {
        if let Some(hit) = self.templates.borrow().get(&node.id) {
            return Rc::clone(hit);
        }
        self.templates.borrow_mut().insert(node.id.clone(), Rc::new(Vec::new()));
        let mut out: Vec<TemplateRef> = Vec::new();
        let heads: Vec<Rc<Vec<CppTemplateParameter>>> = cpp_template_heads(node, self.ctx).into_iter().map(Rc::new).collect();
        let around = self.declaration_around(node);
        if let Some(own) = heads.last() {
            if is_class_kind(node.kind) {
                if let Some(template) = self.template_of_class(node, own) {
                    out.push(template);
                }
            } else if node.kind == NodeKind::TypeAlias {
                out.push(Rc::new(Template {
                    key: format!("alias:{}", node.qualified_name),
                    kind: TemplateKind::Alias,
                    name: node.name.clone(),
                    node: node.clone(),
                    parameters: Rc::clone(own),
                    pattern: None,
                }));
            } else if is_function_kind(node.kind) {
                if around.is_some() || cpp_parent_scope(&node.qualified_name).is_empty() {
                    out.push(self.function_template(node, own));
                } else {
                    out.extend(self.out_of_line_templates(node, &heads));
                }
            }
        }
        if let Some(typed) = self.typed_test_template(node) {
            out.push(typed);
        }
        if let Some(around) = around {
            out.extend(self.templates_around(&around).iter().cloned());
        }
        let out = Rc::new(out);
        self.templates.borrow_mut().insert(node.id.clone(), Rc::clone(&out));
        out
    }

    fn slot_named(&self, name: &str, templates: &[TemplateRef]) -> Option<Slot> {
        templates.iter().find_map(|t| t.parameters.iter().position(|p| p.name.as_deref() == Some(name)).map(|index| Slot { template: Rc::clone(t), index }))
    }

    /// The parameter a type written as `segments` starts with where `r` is made, with the member
    /// types after it: itself, or through an alias (`using KeyGen = typename TypeParam::KeyGen;`).
    fn parameter_of(&self, segments: &[String], r: &UnresolvedRef, caller: &NodeRef) -> Option<(Slot, String, Vec<String>)> {
        let head = &segments[0];
        if let Some(direct) = self.slot_named(head, &self.templates_around(caller)) {
            return Some((direct, head.clone(), segments[1..].to_vec()));
        }
        let (text, alias) = cpp_alias_written_at(head, r, self.ctx)?;
        let written = cpp_type_segments(&text)?;
        let slot = self.slot_named(&written[0], &self.templates_around(alias.as_ref().unwrap_or(caller)))?;
        let mut types = written[1..].to_vec();
        types.extend(segments[1..].iter().cloned());
        Some((slot, written[0].clone(), types))
    }

    /// The names a function's lines declare with a type that starts with a name of `types`, a
    /// parameter or an alias: all a call on a variable of the function's own can go through.
    fn local_values(&self, caller: &Node, types: &HashSet<String>) -> Rc<HashSet<String>> {
        if let Some(hit) = self.values.borrow().get(&caller.id) {
            return Rc::clone(hit);
        }
        let start = (caller.start_line as usize).saturating_sub(1);
        let text = self.joined(&caller.file_path, start, caller.end_line.max(caller.start_line) as usize);
        let text = re!(r"\b(?:const|volatile|typename|mutable|static)\b", "g").replace_all_str(&text, " ");
        let mut values = HashSet::new();
        for m in re!(r"\b([A-Za-z_]\w*)(?:\s*::\s*[A-Za-z_]\w*)*(?:\s*<[^<>;{}()]*>)?[\s*&]+(?:[A-Z][A-Z0-9_]*\s+)*(?=([A-Za-z_]\w*)\b)", "g").captures_iter(&text) {
            if types.contains(m.text(1).unwrap_or("")) {
                values.insert(m.text(2).unwrap_or("").to_string());
            }
        }
        let values = Rc::new(values);
        self.values.borrow_mut().insert(caller.id.clone(), Rc::clone(&values));
        values
    }

    /// A call on a variable whose declared type is a parameter: `shard.Clear()` on a
    /// `CacheShard& shard`, `p->Clear()` on a `T* p`.
    fn through_value(&self, r: &UnresolvedRef, caller: &NodeRef, receiver: &str, member: &str, types: &HashSet<String>) -> Option<Through> {
        if self.templates_around(caller).is_empty() {
            return None;
        }
        let text = if self.local_values(caller, types).contains(receiver) {
            cpp_argument_type_text(receiver, r, self.ctx)
        } else {
            // (Typing an argument leaves out a member of a template parameter's type.)
            let field = cpp_callers_member(receiver, caller, r, self.ctx).map(|w| w.text);
            let head = field.as_deref().and_then(cpp_type_segments).map(|s| s[0].clone());
            field.filter(|_| head.is_some_and(|h| types.contains(&h)) && !is_cpp_local_name(receiver, caller, r, self.ctx))
        }?;
        let written = cpp_type_segments(&text)?;
        let (slot, parameter, types) = self.parameter_of(&written, r, caller)?;
        // A `->` on a value of the parameter's type is the argument's own `operator->`.
        let lines = self.lines(&r.file_path);
        let line = lines.get((r.line as usize).wrapping_sub(1)).map(String::as_str).unwrap_or("");
        if writes_arrow(line, receiver, member) != is_cpp_pointer_type(&text) {
            return None;
        }
        Some(Through { slot, parameter, types, member: Some(member.to_string()), arity: None })
    }

    /// How a reference goes through a parameter, or `None` when it doesn't.
    fn through(&self, r: &UnresolvedRef, caller: &NodeRef, types: &HashSet<String>) -> Option<Through> {
        if let Some(value) = re!(r"^([A-Za-z_]\w*)\.([A-Za-z_]\w*)$").captures(&r.reference_name) {
            return self.through_value(r, caller, value.text(1)?, value.text(2)?, types);
        }
        let constructor = re!(r"^(.*)::([^:]+)/(\d+)$").captures(&r.reference_name);
        let name = constructor.as_ref().and_then(|c| c.text(1)).unwrap_or(&r.reference_name);
        let name = re!(r"::\s*template\s+", "g").replace_all_str(name, "::");
        let segments: Vec<String> = name.split("::").map(String::from).collect();
        if !segments.iter().all(|s| is_identifier(s)) {
            return None;
        }
        let (slot, parameter, mut types) = self.parameter_of(&segments, r, caller)?;
        if let Some(c) = constructor {
            let arity = c.text(3)?.parse().ok()?;
            return Some(Through { slot, parameter, types, member: None, arity: Some(arity) });
        }
        if r.reference_kind == ReferenceKind::Edge(EdgeKind::Instantiates) {
            return Some(Through { slot, parameter, types, member: None, arity: None });
        }
        let member = types.pop()?;
        Some(Through { slot, parameter, types, member: Some(member), arity: None })
    }

    fn source(&self, file: &str) -> Option<Rc<Source>> {
        if let Some(hit) = self.sources.get(file) {
            return hit;
        }
        let source = self.ctx.read_file(file).map(|raw| Rc::new(Source::new(codegraph_text::strip_comments_for_regex(&raw, "cpp"))));
        self.sources.set(file, source.clone());
        source
    }

    fn declarations_in(&self, file: &str) -> Rc<Vec<NodeRef>> {
        if let Some(hit) = self.declarations.get(file) {
            return hit;
        }
        let decls: Vec<NodeRef> = self
            .ctx
            .get_nodes_in_file(file)
            .iter()
            .filter(|n| is_declaration_kind(n.kind) && matches!(n.language, Language::Cpp | Language::C))
            .cloned()
            .collect();
        let decls = Rc::new(decls);
        self.declarations.set(file, Rc::clone(&decls));
        decls
    }

    /// The tightest declaration whose lines hold `line`.
    fn declaration_at(&self, file: &str, line: usize) -> Option<NodeRef> {
        let mut best: Option<&NodeRef> = None;
        let decls = self.declarations_in(file);
        for n in decls.iter() {
            let (start, end) = (n.start_line as usize, n.end_line.max(n.start_line) as usize);
            if start > line || end < line {
                continue;
            }
            let better = match best {
                None => true,
                Some(b) => start > b.start_line as usize || (start == b.start_line as usize && end < b.end_line.max(b.start_line) as usize),
            };
            if better {
                best = Some(n);
            }
        }
        best.cloned()
    }

    /// The namespace at `offset`, outside every declaration: the index holds no node for one.
    fn namespace_at(&self, file: &str, offset: usize, line: usize) -> String {
        let Some(source) = self.source(file) else { return String::new() };
        let blocks = match self.namespaces.get(file) {
            Some(b) => b,
            None => {
                let b = Rc::new(cpp_namespace_blocks(&source.text));
                self.namespaces.set(file, Rc::clone(&b));
                b
            }
        };
        let mut path: &[String] = &[];
        for block in blocks.iter() {
            if block.start > offset {
                break;
            }
            if offset < block.end {
                path = &block.path;
            }
        }
        let frames = cpp_macro_namespace_frames(file, self.ctx);
        let mut all: Vec<String> = frames.iter().filter(|f| f.start <= line && f.end >= line).flat_map(|f| f.path.iter().cloned()).collect();
        all.extend(path.iter().cloned());
        all.join("::")
    }

    fn place_in(&self, around: &NodeRef, line: usize) -> Place {
        let file = around.file_path.clone();
        let scope = if is_class_kind(around.kind) { around.qualified_name.clone() } else { cpp_parent_scope(&around.qualified_name) };
        let r = UnresolvedRef::new(around.id.clone(), "", EdgeKind::References, line as u32, 0, file.clone(), Language::Cpp);
        Place { file, line, scope, around: Some(around.clone()), templates: Rc::default(), parameters: Rc::default(), r }
    }

    fn parameters_of(templates: &[TemplateRef]) -> HashSet<String> {
        templates.iter().flat_map(|t| t.parameters.iter().filter_map(|p| p.name.clone())).collect()
    }

    /// Where a caller's call, or the body of a template, writes a name.
    fn place_of_caller(&self, caller: &NodeRef, line: usize) -> Place {
        let templates = self.templates_around(caller);
        let parameters = Rc::new(Self::parameters_of(&templates));
        Place { templates, parameters, ..self.place_in(caller, line) }
    }

    /// Where the text at `offset` of `file` is written.
    fn place_at(&self, file: &str, offset: usize) -> Place {
        let source = self.source(file).expect("a site is read from a file with text");
        let line = source.line_at(offset);
        let mut place = match self.declaration_at(file, line) {
            Some(around) => self.place_of_caller(&around, line),
            None => Place {
                file: file.to_string(),
                line,
                scope: self.namespace_at(file, offset, line),
                around: None,
                templates: Rc::default(),
                parameters: Rc::default(),
                r: UnresolvedRef::new("", "", EdgeKind::References, line as u32, 0, file, Language::Cpp),
            },
        };
        // A head no node holds: a prototype's, or a default's in a head.
        let before = window(&source.text, offset.saturating_sub(HEAD_WINDOW), offset);
        let cut = [before.rfind(';'), before.rfind('{'), before.rfind('}')].into_iter().flatten().max().map_or(0, |i| i + 1);
        let statement = &before[cut..];
        if statement.contains("template") {
            let mut parameters = (*place.parameters).clone();
            parameters.extend(cpp_type_names_declared(&format!("{statement}>")));
            place.parameters = Rc::new(parameters);
        }
        place
    }

    fn site(&self, place: &Place) -> CppNameSite {
        let includes = Rc::clone(&self.includes);
        let file = place.file.clone();
        CppNameSite {
            scope: place.scope.clone(),
            file: place.file.clone(),
            line: place.line,
            parameters: Rc::clone(&place.parameters),
            self_id: None,
            class_scope: None,
            declared: false,
            sees: Some(Rc::new(move |n: &Node| includes.sees(&file, n))),
        }
    }

    /// Is the line a preprocessor directive, or a continuation of one?
    fn is_directive(&self, file: &str, line: usize) -> bool {
        let lines = self.lines(file);
        let mut at = line.saturating_sub(1);
        while at > 0 && lines.get(at - 1).is_some_and(|l| l.trim_end().ends_with('\\')) {
            at -= 1;
        }
        lines.get(at).is_some_and(|l| l.trim_start().starts_with('#'))
    }

    /// Is what the line holds at the (UTF-16) column code, not a string or a comment?
    fn is_code(&self, file: &str, line: usize, column: usize, name: &str) -> bool {
        // (Sites come in order, and one line can hold thousands of them.)
        let mut blanked = self.blanked.borrow_mut();
        if !blanked.as_ref().is_some_and(|(f, l, _)| f == file && *l == line) {
            let lines = self.lines(file);
            let text = cpp_literals_blanked(lines.get(line.wrapping_sub(1)).map(String::as_str).unwrap_or(""), line, &cpp_non_code_lines(file, self.ctx));
            *blanked = Some((file.to_string(), line, text));
        }
        let text = &blanked.as_ref().expect("set above").2;
        utf16_from(text, column).starts_with(name)
    }

    /// Is the declaration a lookup found in `file` `template`? Two headers can declare one name
    /// for separate translation units, and so can two source files.
    fn is_template(&self, found: &Node, template: &Template, file: &str) -> bool {
        found.qualified_name == template.node.qualified_name && self.sees(file, &template.node)
    }

    /// Every `name<…>` a class or alias template is written as, and its specializations, in file
    /// and offset order.
    fn written_as(&self, template: &Template) -> Rc<WrittenTemplate> {
        if let Some(hit) = self.written.get(&template.key) {
            return hit;
        }
        let mut out = WrittenTemplate::default();
        let name = template.name.as_str();
        let files = self.files_naming.get(name).cloned().unwrap_or_default();
        for file in &files {
            let Some(source) = self.source(file) else { continue };
            let text = source.text.as_str();
            let b = text.as_bytes();
            for (at, _) in text.match_indices(name) {
                if at > 0 && is_word(b[at - 1]) {
                    continue;
                }
                let mut open = at + name.len();
                while open < b.len() && is_space(b[open]) {
                    open += 1;
                }
                if b.get(open) != Some(&b'<') {
                    continue;
                }
                let Some(close) = closing_angle(text, open) else { continue };
                let before = window(text, at.saturating_sub(HEAD_WINDOW), at);
                // (`x.get<Foo>()` and `p->get<Foo>()` call a member template.)
                if re!(r"(?:\.|->)\s*(?:template\s+)?$").is_match(before) || re!(r"\bfriend\s+(?:class|struct)\s+[\w:\s]*$").is_match(before) {
                    continue;
                }
                let line = source.line_at(at);
                let line_start = source.line_start(line);
                let column = text[line_start..at].encode_utf16().count();
                if self.is_directive(file, line) || !self.is_code(file, line, column, name) {
                    continue;
                }
                let place = self.place_at(file, at);
                let mut written: Vec<String> = qualifiers_before(text, at).into_iter().map(|(n, _)| n).collect();
                written.push(name.to_string());
                let mut site = self.site(&place);
                site.declared = template.kind == TemplateKind::Alias;
                let found = match cpp_class_written(&written.join("::"), &site, &place.r, self.ctx) {
                    Tri::Value(found) if found.exact => found.node,
                    _ => continue,
                };
                if !self.is_template(&found, template, file) {
                    continue;
                }
                let args: Vec<Argument> =
                    arguments_between(text, open, close).into_iter().map(|(t, offset)| Argument { text: t, place: place.at_line(source.line_at(offset)) }).collect();
                let after = window(text, close + 1, close + 8);
                // `template class Name<Foo>;` instantiates; `template <…> class Name<T*> {` specializes.
                let keyword = re!(r"\b(?:class|struct|union)\s+(?:[A-Z][A-Z0-9_]*(?:\([^()]*\))?\s+)*(?:[\w:]+::\s*)?$").find(before);
                let head = keyword
                    .filter(|_| re!(r"^\s*(?:[{;]|:(?!:)|final\b)").is_match(after))
                    .and_then(|k| re!(r"\btemplate\s*<([^;{}]*)>\s*$").captures(&before[..k.start()]).and_then(|c| c.text(1).map(String::from)));
                if let Some(head) = head {
                    let keys: Vec<String> = args.iter().map(|a| argument_key(&a.text)).collect();
                    out.specializations.push(specialization_of(&keys, &cpp_template_parameters_in(&head)));
                    continue;
                }
                out.instantiations.push(Instantiation { args });
            }
        }
        if template.kind == TemplateKind::Class {
            out.instantiations.extend(self.typed_lists(template));
        }
        let out = Rc::new(out);
        self.written.set(&template.key, Rc::clone(&out));
        out
    }

    /// gtest's typed fixtures, by their last name, with where each is given its list of types.
    fn typed_suites(&self) -> Rc<HashMap<String, Vec<(String, usize, String)>>> {
        if let Some(hit) = self.suites.borrow().as_ref() {
            return Rc::clone(hit);
        }
        let mut suites: HashMap<String, Vec<(String, usize, String)>> = HashMap::new();
        for file in &self.typed_files {
            let Some(source) = self.source(file) else { continue };
            for m in re!(
                r"\b(?:TYPED_TEST_(?:SUITE|CASE)\s*\(|INSTANTIATE_TYPED_TEST_(?:SUITE|CASE)_P\s*\(\s*[A-Za-z_]\w*\s*,)\s*((?:::\s*)?[A-Za-z_]\w*(?:\s*::\s*[A-Za-z_]\w*)*)\s*,",
                "g"
            )
            .captures_iter(&source.text)
            {
                let Some(whole) = m.get(0) else { continue };
                if self.is_directive(file, source.line_at(whole.start())) {
                    continue;
                }
                let fixture = strip_spaces(m.text(1).unwrap_or(""));
                let last = fixture.rsplit(':').next().unwrap_or("").to_string();
                suites.entry(last).or_default().push((file.clone(), whole.end(), fixture));
            }
        }
        let suites = Rc::new(suites);
        *self.suites.borrow_mut() = Some(Rc::clone(&suites));
        suites
    }

    /// The instantiations `TYPED_TEST_SUITE(Fixture, Types)` writes for a fixture: one per listed type.
    fn typed_lists(&self, template: &Template) -> Vec<Instantiation> {
        let mut out = Vec::new();
        let suites = self.typed_suites();
        for (file, offset, fixture) in suites.get(&template.name).map(Vec::as_slice).unwrap_or(&[]) {
            let place = self.place_at(file, *offset);
            let found = match cpp_class_written(fixture, &self.site(&place), &place.r, self.ctx) {
                Tri::Value(found) if found.exact => found.node,
                _ => continue,
            };
            if !self.is_template(&found, template, file) {
                continue;
            }
            for arg in self.type_list(file, *offset, 0) {
                out.push(Instantiation { args: vec![arg] });
            }
        }
        out
    }

    /// The types a `::testing::Types<…>` written at `offset` of `file` lists, directly or through
    /// an alias of it.
    fn type_list(&self, file: &str, offset: usize, hops: usize) -> Vec<Argument> {
        let Some(source) = self.source(file) else { return Vec::new() };
        if hops > 3 {
            return Vec::new();
        }
        let text = source.text.as_str();
        let offset = floor_boundary(text, offset);
        let from = window(text, offset, offset + HEAD_WINDOW);
        let list = re!(r"\bTypes\s*<").find(from);
        let end = re!(r"[,;)]").find(from).map_or(0, |m| m.start());
        if let Some(list) = list.filter(|l| l.start() < end.max(1)) {
            let open = offset + list.end() - 1;
            let Some(close) = closing_angle(text, open) else { return Vec::new() };
            let place = self.place_at(file, open);
            return arguments_between(text, open, close).into_iter().map(|(t, at)| Argument { text: t, place: place.at_line(source.line_at(at)) }).collect();
        }
        let name = strip_spaces(&from[..end]);
        if cpp_type_segments(&name).is_none() {
            return Vec::new();
        }
        let place = self.place_at(file, offset);
        let mut site = self.site(&place);
        site.declared = true;
        let alias = match cpp_class_written(&name, &site, &place.r, self.ctx) {
            Tri::Value(found) if found.node.kind == NodeKind::TypeAlias => found.node,
            _ => return Vec::new(),
        };
        let Some(aliased) = self.source(&alias.file_path) else { return Vec::new() };
        let start = aliased.line_start(alias.start_line as usize);
        let line_end = aliased.text[start..].find('\n').map_or(aliased.text.len(), |i| start + i);
        let column = codegraph_text::utf16_to_byte_offset(&aliased.text[start..line_end], alias.start_column as usize);
        self.type_list(&alias.file_path, start + column, hops + 1)
    }

    /// A function template's calls, in file and line order, with the arguments each writes;
    /// `None` past the cap.
    fn calls_of(&self, template: &Template) -> Option<Rc<Vec<Call>>> {
        if let Some(hit) = self.calls.borrow().get(&template.key) {
            return hit.clone();
        }
        let edges = q("getIncomingEdges", self.ctx.store().get_incoming_edges(&template.node.id, &[EdgeKind::Calls, EdgeKind::References]));
        if edges.len() > CPP_TEMPLATE_SITE_CAP {
            self.calls.borrow_mut().insert(template.key.clone(), None);
            return None;
        }
        let mut out: Vec<Call> = Vec::new();
        for edge in edges {
            let Some(caller) = self.ctx.node_by_id(&edge.source) else { continue };
            let Some(line) = edge.line else { continue };
            if !self.sees(&caller.file_path, &template.node) {
                continue;
            }
            let r = UnresolvedRef::new(caller.id.clone(), template.name.clone(), EdgeKind::Calls, line, edge.column.unwrap_or(0), caller.file_path.clone(), Language::Cpp);
            let place = Rc::new(self.place_of_caller(&caller, line as usize));
            let args = cpp_call_template_arguments(&r, self.ctx).map(|a| a.into_iter().map(|text| Argument { text, place: Rc::clone(&place) }).collect());
            out.push(Call { r, place, args });
        }
        out.sort_by(|a, b| a.r.file_path.cmp(&b.r.file_path).then(a.r.line.cmp(&b.r.line)).then(a.r.column.cmp(&b.r.column)));
        let out = Rc::new(out);
        self.calls.borrow_mut().insert(template.key.clone(), Some(Rc::clone(&out)));
        Some(out)
    }

    /// The pattern a function's own parameter is matched with, to find the one whose whole type
    /// is the template parameter `name` (`T`, `const T&`, `T&&`, `T*`): compiled once per name.
    fn deduced_shape(&self, name: &str) -> Rc<Option<Re>> {
        if let Some(hit) = self.deduced_shapes.borrow().get(name) {
            return Rc::clone(hit);
        }
        let shape = Rc::new(
            Re::try_new(
                &format!(r"^(?:const\s+)?{}\s*(?:const\s*)?(\*|&&?)?\s*(?:const\s+)?(?:[A-Za-z_]\w*)?\s*(?:=.*)?$", escape_regex(name)),
                "s",
            )
            .ok(),
        );
        self.deduced_shapes.borrow_mut().insert(name.to_string(), Rc::clone(&shape));
        shape
    }

    /// The argument a call deduces for a parameter a function's own parameter has as its whole
    /// type, when the call's argument there has a written type: the deduction has nothing else
    /// to go on, and what it deduces is exact.
    fn deduced(&self, template: &Template, index: usize, call: &Call) -> Option<Argument> {
        let name = template.parameters[index].name.as_deref()?;
        let list = cpp_overload_parameters(&template.node, &template.name, self.ctx)?;
        let shape = self.deduced_shape(name);
        let shape = shape.as_ref().as_ref()?;
        let parameters: Vec<String> = split_cpp_top_level(&list).iter().map(|p| re!(r"\s+", "g").replace_all_str(p.trim(), " ")).collect();
        let position = parameters.iter().position(|p| shape.is_match(p))?;
        let given = cpp_call_arguments(&call.r, &template.name, self.ctx)?.get(position)?.clone();
        let argument = cpp_argument_type_text(&given, &call.r, self.ctx)?;
        let value = re!(r"&+\s*$").replace_all_str(&argument, "").trim().to_string();
        let pointer = shape.captures(&parameters[position]).and_then(|c| c.text(1).map(String::from));
        if pointer.as_deref() != Some("*") {
            return Some(Argument { text: value, place: Rc::clone(&call.place) });
        }
        let stripped = re!(r"\*\s*(?:const\s*)?$");
        stripped.is_match(&value).then(|| Argument { text: stripped.replace_all_str(&value, "").trim().to_string(), place: Rc::clone(&call.place) })
    }

    /// The arguments a site gives a parameter of the template itself; its default stands in for
    /// one it doesn't write.
    fn arguments_for(&self, slot: &Slot, args: &[Argument]) -> Vec<Argument> {
        let parameter = &slot.template.parameters[slot.index];
        if parameter.pack {
            return args.get(slot.index..).map(<[Argument]>::to_vec).unwrap_or_default();
        }
        if let Some(given) = args.get(slot.index) {
            return vec![given.clone()];
        }
        let Some(fallback) = parameter.fallback.as_ref().filter(|_| slot.template.kind != TemplateKind::Function) else { return Vec::new() };
        let decl = &slot.template.node;
        vec![Argument { text: fallback.clone(), place: Rc::new(self.place_of_caller(decl, decl.start_line as usize)) }]
    }

    /// What the specialization binds its parameters to, when `args` can be taken by it: `Foo` for
    /// `T` of `Name<T*>` given `Name<Foo*>`. `None` when what each side writes tells it isn't
    /// taken. A condition (`enable_if_t<…>`) can't be told from what is written, and counts as taken.
    fn bindings(spec: &Specialization, primary: &[CppTemplateParameter], args: &[String]) -> Option<HashMap<String, Vec<String>>> {
        let mut bound: HashMap<String, Vec<String>> = HashMap::new();
        for (i, item) in spec.pattern.iter().enumerate() {
            if let Some(parameter) = &item.parameter {
                let given: Vec<String> = if item.pack { args.get(i..).map(<[String]>::to_vec).unwrap_or_default() } else { args.get(i).cloned().into_iter().collect() };
                bound.entry(parameter.clone()).or_default().extend(given);
                continue;
            }
            if item.condition {
                continue;
            }
            let given = args.get(i).cloned().or_else(|| primary.get(i).and_then(|p| p.fallback.as_deref()).filter(|f| !f.is_empty()).map(argument_key))?;
            let Some(shape) = &item.shape else {
                if given != item.text && !given.ends_with(&format!("::{}", item.text)) && !item.text.ends_with(&format!("::{given}")) {
                    return None;
                }
                continue;
            };
            let m = shape.captures(&given)?;
            for (k, w) in item.words.iter().enumerate() {
                bound.insert(w.clone(), vec![m.text(k + 1).unwrap_or("").to_string()]);
            }
        }
        Some(bound)
    }

    /// The arguments each instantiation gives a parameter, once specializations have taken
    /// theirs; `None` past the cap.
    fn arguments_of(&self, slot: &Slot) -> Option<Vec<Argument>> {
        let template = &slot.template;
        if template.kind == TemplateKind::Function {
            let calls = self.calls_of(template)?;
            let mut out = Vec::new();
            for call in calls.iter() {
                let written = self.arguments_for(slot, call.args.as_deref().unwrap_or(&[]));
                match (written.is_empty(), written.is_empty().then(|| self.deduced(template, slot.index, call)).flatten()) {
                    (true, Some(deduced)) => out.push(deduced),
                    _ => out.extend(written),
                }
            }
            return Some(out);
        }
        if self.files_naming.get(&template.name).map_or(0, Vec::len) > CPP_TEMPLATE_SITE_CAP {
            return None;
        }
        let written = self.written_as(template);
        let primary: Rc<Vec<CppTemplateParameter>> =
            if written.specializations.is_empty() { Rc::clone(&template.parameters) } else { self.primary_parameters(template) };
        let own: Option<String> = template.pattern.as_ref().map(|p| p.iter().map(|a| argument_key(a)).collect::<Vec<_>>().join(","));
        let mut out = Vec::new();
        for site in &written.instantiations {
            let given: Vec<String> = if written.specializations.is_empty() { Vec::new() } else { site.args.iter().map(|a| argument_key(&a.text)).collect() };
            let taken: Vec<(&Specialization, HashMap<String, Vec<String>>)> =
                written.specializations.iter().filter_map(|spec| Self::bindings(spec, &primary, &given).map(|b| (spec, b))).collect();
            let Some(own) = own.as_ref() else {
                if taken.is_empty() {
                    out.extend(self.arguments_for(slot, &site.args));
                }
                continue;
            };
            // A specialization's own members, from the instantiations only it takes.
            if taken.len() != 1 || &taken[0].0.written != own {
                continue;
            }
            let (Some(name), Some(first)) = (template.parameters.get(slot.index).and_then(|p| p.name.as_ref()), site.args.first()) else { continue };
            for text in taken[0].1.get(name).map(Vec::as_slice).unwrap_or(&[]) {
                out.push(Argument { text: text.clone(), place: Rc::clone(&first.place) });
            }
        }
        Some(out)
    }

    /// The head of the template itself, whichever declaration a reference is in.
    fn primary_parameters(&self, template: &Template) -> Rc<Vec<CppTemplateParameter>> {
        if template.pattern.is_none() {
            return Rc::clone(&template.parameters);
        }
        for decl in self.classes_named(&template.node.qualified_name) {
            let Some(head) = cpp_template_heads(&decl, self.ctx).pop().map(Rc::new) else { continue };
            if self.template_of_class(&decl, &head).is_none_or(|t| t.pattern.is_none()) {
                return head;
            }
        }
        Rc::default()
    }

    /// The classes a parameter reaches, each once, with the argument that writes it: those its own
    /// instantiations write, then those of each template whose parameter they pass on, a hop at a
    /// time and each parameter once, so a cycle ends. It stops past the class cap, and at a
    /// template past the site cap, whose instantiations go unread: `None`.
    fn classes_of(&self, start: &Slot) -> Option<IndexMap<String, (NodeRef, Argument)>> {
        let mut classes: IndexMap<String, (NodeRef, Argument)> = IndexMap::new();
        let mut seen: HashSet<String> = HashSet::from([slot_key(start)]);
        let mut frontier = vec![start.clone()];
        let mut hop = 0;
        while hop <= MAX_TEMPLATE_HOPS && !frontier.is_empty() {
            let mut next = Vec::new();
            for slot in &frontier {
                for arg in self.arguments_of(slot)? {
                    let text = without_pack(&arg.text).trim().to_string();
                    let name = re!(r"\b(?:const|volatile|typename)\b", "g").replace_all_str(&text, " ").trim().to_string();
                    let outer = if is_identifier(&name) { self.slot_named(&name, &arg.place.templates) } else { None };
                    if let Some(outer) = outer {
                        if seen.insert(slot_key(&outer)) {
                            next.push(outer);
                        }
                        continue;
                    }
                    if let Some(node) = self.class_written(&text, &arg.place) {
                        if !classes.contains_key(&node.id) {
                            classes.insert(node.id.clone(), (node, arg.clone()));
                        }
                    }
                    if classes.len() > CPP_TEMPLATE_ARGUMENT_CAP {
                        return Some(classes);
                    }
                }
            }
            frontier = next;
            hop += 1;
        }
        Some(classes)
    }

    /// The project class an argument names, or `None`.
    fn class_written(&self, text: &str, place: &Place) -> Option<NodeRef> {
        let plain = strip_cpp_template_arguments(text);
        if re!(r"[&*()\[\]]").is_match(&plain) || is_cpp_pointer_type(text) || re!(r"\bdecltype\b").is_match(&plain) {
            return None;
        }
        let segments = cpp_type_segments(text)?;
        if let Some(fn_) = place.around.as_ref().filter(|a| is_function_kind(a.kind)) {
            if segments.len() == 1 {
                // A class local to the function that writes it, which the index names after the
                // function alone.
                let decls = self.declarations_in(&place.file);
                let local = decls.iter().filter(|n| {
                    is_class_kind(n.kind)
                        && n.name == segments[0]
                        && n.start_line >= fn_.start_line
                        && n.end_line <= fn_.end_line.max(fn_.start_line)
                        && n.start_line as usize <= place.line
                });
                if let Some(last) = local.last() {
                    return Some(last.clone());
                }
            }
            if cpp_declares_local_alias(text, &place.r, self.ctx) {
                let named = match resolve_cpp_aliased_type(text, &place.r, self.ctx) {
                    Tri::Value(aliased) if !aliased.pointer => cpp_aliased_class(&aliased, self.ctx, Some(&place.r)),
                    _ => None,
                };
                return named.and_then(|named| self.classes_named(&named).into_iter().find(|n| self.sees(&place.file, n)));
            }
        }
        // (A lookup falls back on what it can't see when that is all a name has: a copy of the
        // project declares it plainly where the project's own macro hides it.)
        match cpp_class_written(text, &self.site(place), &place.r, self.ctx) {
            Tri::Value(found) if found.exact && is_class_kind(found.node.kind) && self.sees(&place.file, &found.node) => Some(found.node),
            _ => None,
        }
    }

    /// What a reference through a parameter names in the class its argument is, or `None`.
    fn target(&self, cls: &NodeRef, through: &Through, r: &UnresolvedRef) -> Option<NodeRef> {
        let spelled = through.types.join("::");
        let file = cls.file_path.clone();
        let visible_fn = |n: &Node| self.sees(&file, n);
        let visible: Visible = Some(&visible_fn);
        let scopes = [cls.qualified_name.clone()];
        if through.member.is_none() && through.arity.is_none() {
            if through.types.is_empty() {
                return Some(cls.clone());
            }
            return match cpp_type_named(&spelled, &scopes, r, self.ctx, visible) {
                Tri::Value(CppNamedType::Class(CppNamedClass::Alias(alias))) => Some(alias),
                Tri::Value(CppNamedType::Enums(enums)) => enums.into_iter().next(),
                Tri::Value(CppNamedType::Class(CppNamedClass::Cls(named))) => self.class_node(&named, cls),
                _ => None,
            };
        }
        let mut owner = Some(cls.clone());
        if !through.types.is_empty() {
            owner = match cpp_class_named(&spelled, &scopes, r, self.ctx, visible) {
                Tri::Value(CppNamedClass::Cls(named)) => self.class_node(&named, cls),
                _ => None,
            };
        }
        let owner = owner?;
        if let Some(arity) = through.arity {
            let mut made = r.clone();
            made.reference_name = format!("::{}::{}/{}", owner.qualified_name, owner.name, arity);
            return match_cpp_constructor(&made, self.ctx).and_then(|m| self.ctx.node_by_id(&m.target_node_id));
        }
        cpp_method_of(&owner, through.member.as_deref()?, r, self.ctx)
    }

    fn class_node(&self, qualified: &str, near: &Node) -> Option<NodeRef> {
        let classes = self.classes_named(qualified);
        classes.iter().find(|n| n.file_path == near.file_path).or_else(|| classes.iter().find(|n| self.sees(&near.file_path, n))).cloned()
    }
}

/// Where `::name(` or `::name<` (a `template` or `~` between allowed) is written in `text`: the
/// offset of its `::`.
fn find_qualified_name(text: &str, name: &str) -> Option<usize> {
    let b = text.as_bytes();
    let mut from = 0;
    while let Some(found) = text[from..].find("::") {
        let at = from + found;
        from = at + 2;
        let mut i = at + 2;
        let skip = |i: &mut usize| {
            while *i < b.len() && is_space(b[*i]) {
                *i += 1;
            }
        };
        skip(&mut i);
        if text[i..].starts_with("template") && b.get(i + 8).is_some_and(|c| is_space(*c)) {
            i += 8;
            skip(&mut i);
        }
        if b.get(i) == Some(&b'~') {
            i += 1;
        }
        if !text[i..].starts_with(name) {
            continue;
        }
        i += name.len();
        skip(&mut i);
        if matches!(b.get(i), Some(b'(') | Some(b'<')) {
            return Some(at);
        }
    }
    None
}

/// Does `line` write `receiver->member` (a `template` between allowed), each a whole word?
fn writes_arrow(line: &str, receiver: &str, member: &str) -> bool {
    let b = line.as_bytes();
    for (at, _) in line.match_indices(receiver) {
        if at > 0 && is_word(b[at - 1]) {
            continue;
        }
        let mut i = at + receiver.len();
        while i < b.len() && is_space(b[i]) {
            i += 1;
        }
        if !line[i..].starts_with("->") {
            continue;
        }
        i += 2;
        while i < b.len() && is_space(b[i]) {
            i += 1;
        }
        if line[i..].starts_with("template") && b.get(i + 8).is_some_and(|c| is_space(*c)) {
            i += 8;
            while i < b.len() && is_space(b[i]) {
                i += 1;
            }
        }
        if line[i..].starts_with(member) && !b.get(i + member.len()).is_some_and(|c| is_word(*c)) {
            return true;
        }
    }
    false
}

/// One edge per class a failed reference through a template parameter reaches (see the top of
/// this file). Reads the graph and source only.
/// `CODEGRAPHV_SYNTH_TIMINGS` set: the pass reports where its time goes.
fn synth_timed() -> bool {
    std::env::var("CODEGRAPHV_SYNTH_TIMINGS").is_ok_and(|v| !v.is_empty())
}

pub fn cpp_template_argument_edges(ctx: &Ctx) -> Vec<Edge> {
    let timed = synth_timed();
    let started = std::time::Instant::now();
    // A name in a template's head or an alias, by file: most references go no further.
    let (reader, declared) = TemplateInstantiations::read(ctx);
    let read_ms = started.elapsed().as_millis();
    if declared.is_empty() {
        return Vec::new();
    }
    let files: Vec<String> = declared.keys().cloned().collect();
    let mut refs: Vec<UnresolvedRef> = Vec::new();
    let query_at = std::time::Instant::now();
    let rows = q("iterateFailedCppTypeReferences", ctx.store().get_failed_cpp_type_references(&files));
    let (query_ms, row_count) = (query_at.elapsed().as_millis(), rows.len());
    for row in rows {
        let file = row.file_path.clone().unwrap_or_default();
        let Some(head) = re!(r"^[A-Za-z_]\w*").find(&row.reference_name).map(|m| m.as_str().to_string()) else { continue };
        if !(row.reference_name.contains('.') || declared.get(&file).is_some_and(|d| d.contains(&head))) {
            continue;
        }
        refs.push(UnresolvedRef::new(row.from_node_id, row.reference_name, row.reference_kind, row.line, row.column, file, Language::Cpp));
    }
    refs.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.line.cmp(&b.line))
            .then(a.column.cmp(&b.column))
            .then(a.reference_name.cmp(&b.reference_name))
            .then(a.reference_kind.as_str().cmp(b.reference_kind.as_str()))
    });
    let ref_count = refs.len();
    let group_at = std::time::Instant::now();
    let mut groups: IndexMap<String, (Slot, Vec<(UnresolvedRef, Through)>)> = IndexMap::new();
    for r in refs {
        let Some(caller) = ctx.node_by_id(&r.from_node_id) else { continue };
        let Some(types) = declared.get(&r.file_path) else { continue };
        let Some(through) = reader.through(&r, &caller, types) else { continue };
        groups.entry(slot_key(&through.slot)).or_insert_with(|| (through.slot.clone(), Vec::new())).1.push((r, through));
    }
    let group_ms = group_at.elapsed().as_millis();
    let classes_at = std::time::Instant::now();
    let mut edges = Vec::new();
    for (slot, group) in groups.values() {
        let Some(classes) = reader.classes_of(slot) else { continue };
        if classes.len() > CPP_TEMPLATE_ARGUMENT_CAP {
            continue;
        }
        for (r, through) in group {
            for (node, at) in classes.values() {
                let Some(target) = reader.target(node, through, r) else { continue };
                if target.id == r.from_node_id {
                    continue;
                }
                let kind = match r.reference_kind {
                    ReferenceKind::Edge(EdgeKind::Instantiates) => EdgeKind::Instantiates,
                    ReferenceKind::FunctionRef => EdgeKind::References,
                    _ => EdgeKind::Calls,
                };
                let mut md = meta! {
                    "synthesizedBy" => "cpp-template-argument",
                    "via" => through.parameter.as_str(),
                    "argument" => node.name.as_str(),
                    "registeredAt" => format!("{}:{}", at.place.file, at.place.line),
                };
                if r.reference_kind == ReferenceKind::FunctionRef {
                    md.insert("fnRef".to_string(), true.into());
                }
                let mut edge = heuristic_edge(&r.from_node_id, &target.id, kind, Some(r.line), md);
                edge.column = Some(r.column);
                edges.push(edge);
            }
        }
    }
    if timed {
        eprintln!(
            "[phase-timing]       cppTemplateEdges: read {read_ms}ms ({} files declare), failed refs {query_ms}ms ({row_count} rows, {ref_count} kept), through {group_ms}ms ({} slots), classes {}ms ({} edges)",
            files.len(),
            groups.len(),
            classes_at.elapsed().as_millis(),
            edges.len()
        );
    }
    edges
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(text: &str) -> Vec<String> {
        cpp_namespace_blocks(text).into_iter().map(|b| b.path.join("::")).collect()
    }

    #[test]
    fn reads_every_namespace_head() {
        let forty = " ".repeat(40);
        let cases: Vec<(&str, String, Vec<&str>)> = vec![
            ("one name", "namespace a { }".into(), vec!["a"]),
            ("a qualified name", "namespace a::b { }".into(), vec!["a::b"]),
            ("spaces in a qualified name", "namespace a :: b { }".into(), vec!["a::b"]),
            ("no name", "namespace { }".into(), vec![""]),
            ("an inline namespace", "inline namespace v1 { }".into(), vec!["v1"]),
            ("a brace on the next line", "namespace a\n{\n}".into(), vec!["a"]),
            ("a blanked comment in the head", format!("namespace a{forty}{{ }}"), vec!["a"]),
            ("a blanked comment before the name", format!("namespace{forty}a {{ }}"), vec!["a"]),
            ("nesting", "namespace a { namespace b { } }".into(), vec!["a", "a::b"]),
            ("an alias, which opens nothing", "namespace a = b::c;".into(), vec![]),
            ("a using directive, which opens nothing", "using namespace a;".into(), vec![]),
        ];
        for (form, text, expected) in cases {
            assert_eq!(names(&text), expected, "reads {form}");
        }
    }

    #[test]
    fn keeps_where_each_block_ends() {
        let text = "namespace a { void f() { } } int x;";
        assert_eq!(cpp_namespace_blocks(text), [CppNamespaceBlock { start: 0, end: text.find("} int").unwrap(), path: vec!["a".to_string()] }]);
    }

    #[test]
    fn passes_a_long_run_of_spaces_after_a_name_in_linear_time() {
        let text = format!("namespace a{};", " ".repeat(160 * 1024));
        let start = std::time::Instant::now();
        assert_eq!(cpp_namespace_blocks(&text), []);
        assert!(start.elapsed().as_millis() < 1000, "{:?}", start.elapsed());
    }

    #[test]
    fn splits_arguments_at_the_top_level_past_a_trailing_return_arrow() {
        let text = "Bound<auto(int)->int, Foo<A, B>> x";
        let open = text.find('<').unwrap();
        let close = closing_angle(text, open).unwrap();
        let args: Vec<String> = arguments_between(text, open, close).into_iter().map(|(t, _)| t).collect();
        assert_eq!(args, ["auto(int)->int", "Foo<A, B>"]);
    }

    #[test]
    fn reads_qualifiers_with_their_arguments() {
        let text = "void Pair<M, A, B>::Go()";
        let at = text.find("::").unwrap();
        assert_eq!(qualifiers_before(text, at + 2), [("Pair".to_string(), Some(vec!["M".to_string(), "A".to_string(), "B".to_string()]))]);
    }

    #[test]
    fn an_argument_compares_spaced_only_between_words() {
        assert_eq!(argument_key("const  Foo < int > *"), "const Foo<int>*");
    }
}
