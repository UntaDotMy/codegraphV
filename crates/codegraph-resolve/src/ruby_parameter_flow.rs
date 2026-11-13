//! Ruby's side of a parameter's class from its call sites (see `parameter_flow`). Port of
//! `src/resolution/ruby-parameter-flow.ts` (upstream 483a0988).
//!
//! Ruby writes no types, so all of it is read from source: a `def`'s parameters (its node carries
//! no signature), the arguments at a call, and the few shapes of value that say their class
//! exactly. Columns are read as character offsets into a line.

use crate::ctx::Ctx;
use crate::name_matcher::ruby::{ruby_ancestry, ruby_class_method_of, ruby_constant_qn, ruby_methods_of};
use crate::parameter_flow::{Answer, FlowMethod, FlowMethods, FlowParameter, FlowSite, FlowValue};
use crate::ruby_constant_paths::ivar_class_node;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::{EdgeKind, Language, NodeKind, ReferenceKind};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

/// The arguments of one call, as written.
pub struct RubyArguments {
    positional: Vec<String>,
    keywords: IndexMap<String, String>,
    /// Where the first `*splat` is among `positional`, past which nothing's place is known.
    splat_at: Option<usize>,
    keyword_splat: bool,
}

pub struct RubySignature {
    /// The parameters a call passes one value to.
    pub parameters: Vec<FlowParameter>,
    /// Names a call fills with several values, or with none it writes.
    gathering: HashSet<String>,
    /// What of the method follows the parameters on their last line.
    rest: String,
    required: usize,
    optional: usize,
    takes_rest: bool,
    takes_keywords: bool,
}

impl RubySignature {
    /// Whether a call with these arguments can be a call of this method.
    fn accepts(&self, args: &RubyArguments) -> bool {
        if args.splat_at.is_some() {
            return true;
        }
        let fits = |n: usize| n >= self.required && (self.takes_rest || n <= self.required + self.optional);
        let n = args.positional.len();
        if self.takes_keywords {
            return fits(n);
        }
        if !args.keywords.is_empty() {
            return fits(n + 1);
        }
        if args.keyword_splat {
            fits(n) || fits(n + 1)
        } else {
            fits(n)
        }
    }
}

/// What a stretch of source binds.
#[derive(Default)]
struct Bindings {
    /// Each `name = value` and `name ||= value` written as a statement.
    assigned: HashMap<String, Vec<String>>,
    /// Names bound any other way.
    other: HashSet<String>,
}

/// A file's lines, from offsets into its text.
struct SourceLines {
    source: Rc<str>,
    starts: Vec<usize>,
}

impl SourceLines {
    fn new(source: Rc<str>) -> SourceLines {
        let mut starts = vec![0];
        starts.extend(source.bytes().enumerate().filter(|(_, b)| *b == b'\n').map(|(i, _)| i + 1));
        starts.push(source.len() + 1);
        SourceLines { source, starts }
    }

    fn len(&self) -> usize {
        self.starts.len() - 1
    }

    /// Line `index`, 0-based, without its newline.
    fn at(&self, index: usize) -> Option<&str> {
        (index < self.len()).then(|| &self.source[self.starts[index]..self.starts[index + 1] - 1])
    }

    /// Lines `from` to `to`, `to` left out.
    fn text(&self, from: usize, to: usize) -> &str {
        let to = to.min(self.len());
        if from >= to {
            return "";
        }
        &self.source[self.starts[from]..self.starts[to] - 1]
    }
}

const CONSTANT_PATH: &str = r"^(?:::)?[A-Z]\w*(?:::[A-Z]\w*)*$";
const CONSTRUCTED: &str = r"^((?:::)?[A-Z]\w*(?:::[A-Z]\w*)*)\.new(\(.*\))?$";
const HEREDOC: &str = r#"<<[~-]?["'`A-Za-z_]"#;
const LOCAL_NAME: &str = r"^[a-z_]\w*$";
const INSTANCE_VARIABLE: &str = r"^@(\w+)$";
const MAX_LIST_LINES: usize = 40;
const STATEMENT_MODIFIER: &str = r"^(?:if|unless|while|until|rescue|do|and|or|then)\b";
const OTHER_SELF: &str = r"\b(?:instance_eval|instance_exec|class_eval|class_exec|module_eval|module_exec|define_method|define_singleton_method|Class\.new|Module\.new|Struct\.new|Data\.define)\b";
/// A line past this can't be read exactly: the patterns below take time by the square of a line's length.
const MAX_READ_LINE: usize = 2_000;
const STATEMENT_ASSIGNMENT: &str = r"^\s*(@?[a-zA-Z_]\w*)\s*(?:\|\|)?=(?![=~>])\s*(.+)";
const ANY_ASSIGNMENT: &str = r"(?:^|[^.\w@$:])(@?[a-zA-Z_]\w*)\s*(?:[-+*/%|&^]|\*\*|\|\||&&|<<|>>)?=(?![=~>])";
const BLOCK_PARAMETERS: &str = r"(?:\bdo|\{)\s*\|([^|\n]*)\|";
const LAMBDA_PARAMETERS: &str = r"->\s*\(?([^)\n{]*)";
const RESCUED_OR_MATCHED: &str = r"(?:\brescue\b|^\s*in\b)[^\n]*=>\s*(@?[a-zA-Z_]\w*)";
const FOR_VARIABLES: &str = r"\bfor\s+([^\n]*?)\s+in\b";
const SEVERAL_ASSIGNMENT: &str = r"^\s*(\*?@?\w+(?:\s*,\s*\*?@?\w+)+)\s*=(?![=~>])";
const PATTERN_CLAUSE: &str = r"^\s*in\s+(.+)$";
const WORD: &str = r"@?[a-zA-Z_]\w*";
/// `word.call` or `word&.call`: the word, and the call.
const CALL_ON_A_WORD: &str = r"(?<![\w.@$:])([a-z_]\w*)&?\.([a-zA-Z_]\w*[?!]?)";

fn rx(source: &str) -> Arc<Re> {
    Re::cached(source, "")
}

fn rxg(source: &str) -> Arc<Re> {
    Re::cached(source, "g")
}

fn owner_of(qualified_name: &str) -> &str {
    qualified_name.rfind("::").map_or("", |cut| &qualified_name[..cut])
}

fn chars_from(text: &[char], i: usize) -> String {
    text.get(i..).map(|s| s.iter().collect()).unwrap_or_default()
}

fn starts_at(text: &[char], at: usize, prefix: &str) -> bool {
    let p: Vec<char> = prefix.chars().collect();
    text.len() >= at + p.len() && text[at..at + p.len()] == p[..]
}

/// `text` cut at its commas outside any bracket or string.
fn split_top_level(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut open = 0usize;
    let mut quote: Option<char> = None;
    let mut current = String::new();
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if let Some(q) = quote {
            current.push(ch);
            if ch == '\\' {
                if let Some(n) = chars.next() {
                    current.push(n);
                }
            } else if ch == q {
                quote = None;
            }
        } else if ch == ',' && open == 0 {
            parts.push(current.trim().to_string());
            current.clear();
        } else {
            current.push(ch);
            if ch == '"' || ch == '\'' {
                quote = Some(ch);
            } else if matches!(ch, '(' | '[' | '{') {
                open += 1;
            } else if matches!(ch, ')' | ']' | '}') {
                open = open.saturating_sub(1);
            }
        }
    }
    if !current.trim().is_empty() {
        parts.push(current.trim().to_string());
    }
    parts
}

/// The arguments that follow a call's name, from the character after it, cut at the top-level
/// commas. `None` for what can't be read exactly: a heredoc, or a list longer than a call has.
fn read_argument_list(lines: &SourceLines, line: usize, column: usize) -> Option<Vec<String>> {
    let mut text: Vec<char> = lines.at(line)?.chars().collect();
    let parenthesized = text.get(column) == Some(&'(');
    let mut i = column + usize::from(parenthesized);
    if !parenthesized {
        // No space, or an operator, next: no arguments at all.
        let rest = chars_from(&text, i);
        if !rest.starts_with(char::is_whitespace) {
            return Some(Vec::new());
        }
        let after = rest.trim_start();
        if after.is_empty()
            || after.starts_with(['#', ';', '{'])
            || rx(STATEMENT_MODIFIER).is_match(after)
            || re!(r"^[-+*/%&|^<>=!?:.,)\]}]+(?:\s|$)").is_match(after)
        {
            return Some(Vec::new());
        }
    }
    let mut parts: Vec<String> = Vec::new();
    let mut open: Vec<char> = Vec::new();
    let mut quote: Option<char> = None;
    let mut current = String::new();
    let mut row = line;
    loop {
        if i >= text.len() {
            let continues = parenthesized || !open.is_empty() || quote.is_some() || re!(r"[,\\]\s*$").is_match(&current);
            if !continues {
                break;
            }
            row += 1;
            if row > line + MAX_LIST_LINES {
                return None;
            }
            text = lines.at(row)?.chars().collect();
            i = 0;
            current = format!("{} ", re!(r"\\\s*$").replace_first(&current, |_| String::new()));
            continue;
        }
        let ch = text[i];
        i += 1;
        if let Some(q) = quote {
            current.push(ch);
            if ch == '\\' {
                if let Some(&n) = text.get(i) {
                    current.push(n);
                }
                i += 1;
            } else if ch == q {
                quote = None;
            }
        } else if ch == '"' || ch == '\'' || ch == '`' {
            quote = Some(ch);
            current.push(ch);
        } else if ch == '#' && (i == 1 || text[i - 2].is_whitespace()) {
            i = text.len();
        } else if matches!(ch, '(' | '[' | '{') {
            open.push(ch);
            current.push(ch);
        } else if matches!(ch, ')' | ']' | '}') {
            if open.is_empty() {
                break;
            }
            open.pop();
            current.push(ch);
        } else if ch == ',' && open.is_empty() {
            parts.push(current.trim().to_string());
            current.clear();
        } else if !parenthesized
            && open.is_empty()
            && (ch == ';' || (ch.is_whitespace() && rx(STATEMENT_MODIFIER).is_match(&chars_from(&text, i))))
        {
            break;
        } else {
            current.push(ch);
        }
    }
    if !current.trim().is_empty() {
        parts.push(current.trim().to_string());
    }
    if parts.iter().any(|p| rx(HEREDOC).is_match(p)) {
        None
    } else {
        Some(parts)
    }
}

/// Past the parenthesis that closes the one at (`line`, `column`).
fn closing_parenthesis(lines: &SourceLines, line: usize, column: usize) -> Option<(usize, usize)> {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut row = line;
    while row < lines.len() && row <= line + MAX_LIST_LINES {
        let text: Vec<char> = lines.at(row)?.chars().collect();
        let mut i = if row == line { column } else { 0 };
        while i < text.len() {
            let ch = text[i];
            if let Some(q) = quote {
                if ch == '\\' {
                    i += 1;
                } else if ch == q {
                    quote = None;
                }
            } else if ch == '"' || ch == '\'' {
                quote = Some(ch);
            } else if ch == '(' {
                depth += 1;
            } else if ch == ')' {
                depth -= 1;
                if depth == 0 {
                    return Some((row, i + 1));
                }
            }
            i += 1;
        }
        row += 1;
    }
    None
}

/// The character after the called name at a site, past whatever its receiver is written as.
fn after_called_name(site: &FlowSite, lines: &SourceLines) -> Option<(usize, usize)> {
    let mut line = (site.line as usize).checked_sub(1)?;
    let mut text: Vec<char> = lines.at(line)?.chars().collect();
    let mut at = site.column as usize;
    // To the next code, across lines: a chain may break before its dot.
    let skip_space = |text: &mut Vec<char>, line: &mut usize, at: &mut usize| -> bool {
        for _ in 0..8 {
            while *at < text.len() && text[*at].is_whitespace() {
                *at += 1;
            }
            if *at < text.len() && text[*at] != '#' {
                return true;
            }
            *line += 1;
            *at = 0;
            match lines.at(*line) {
                Some(next) => *text = next.chars().collect(),
                None => return false,
            }
        }
        false
    };
    match site.receiver.as_deref() {
        None => {
            // `self.foo` is recorded as a bare `foo`.
            if starts_at(&text, at, "self.") {
                at += 5;
            } else if starts_at(&text, at, "self&.") {
                at += 6;
            }
        }
        Some(receiver) if receiver.starts_with("new ") => {
            let constant = receiver.get(4..receiver.len().saturating_sub(2)).unwrap_or("");
            if !starts_at(&text, at, constant) {
                return None;
            }
            at += constant.chars().count();
            if !skip_space(&mut text, &mut line, &mut at) || !starts_at(&text, at, ".new") {
                return None;
            }
            at += 4;
            if text.get(at) == Some(&'(') {
                let (l, c) = closing_parenthesis(lines, line, at)?;
                line = l;
                at = c;
                text = lines.at(line)?.chars().collect();
            }
        }
        Some(receiver) => {
            let written: Vec<&str> = receiver.split('\n').collect();
            if !starts_at(&text, at, written[0].strip_suffix('\r').unwrap_or(written[0])) {
                return None;
            }
            if written.len() > 1 {
                line += written.len() - 1;
                text = lines.at(line)?.chars().collect();
                at = written[written.len() - 1].chars().count();
            } else {
                at += receiver.chars().count();
            }
        }
    }
    if site.receiver.is_some() {
        if !skip_space(&mut text, &mut line, &mut at) {
            return None;
        }
        if starts_at(&text, at, "&.") || starts_at(&text, at, "::") {
            at += 2;
        } else if text.get(at) == Some(&'.') {
            at += 1;
        } else {
            return None;
        }
        if !skip_space(&mut text, &mut line, &mut at) {
            return None;
        }
    }
    starts_at(&text, at, &site.method).then(|| (line, at + site.method.chars().count()))
}

/// A written list of arguments, sorted out.
fn sort_arguments(list: &[String]) -> RubyArguments {
    let mut args = RubyArguments { positional: Vec::new(), keywords: IndexMap::new(), splat_at: None, keyword_splat: false };
    for arg in list {
        if arg.starts_with('&') {
            continue;
        }
        if arg.starts_with("**") {
            args.keyword_splat = true;
        } else if arg.starts_with('*') {
            if args.splat_at.is_none() {
                args.splat_at = Some(args.positional.len());
            }
            args.positional.push(arg.clone());
        } else if let Some(label) = re!(r"^([a-zA-Z_]\w*[?!]?):(?!:)\s*(.*)$", "s").captures(arg) {
            let name = label.text(1).unwrap_or("").to_string();
            let value = label.text(2).unwrap_or("");
            args.keywords.insert(name.clone(), if value.is_empty() { name } else { value.to_string() });
        } else if arg.contains("=>") || re!(r#"^"[^"]*":"#).is_match(arg) {
            args.keywords.insert(arg.clone(), arg.clone());
        } else {
            args.positional.push(arg.clone());
        }
    }
    args
}

/// A `def`'s head: the text between its parentheses, or what follows it when it has none, and
/// whatever of the method follows on the line the list ends.
fn read_head(method: &FlowMethod, lines: &SourceLines) -> Option<(String, String)> {
    let first: String = lines.at((method.start_line as usize).checked_sub(1)?)?.chars().skip(method.start_column as usize).collect();
    let def = re!(r"\bdef\s+(?:[A-Za-z_:][\w:]*\.)?[^\s(;]+").find(&first)?;
    let mut header = first[def.end()..].to_string();
    if !header.starts_with('(') {
        let bare = re!(r"^(\s+[^;#=\n]*)?").find(&header).map_or("", |m| m.as_str()).to_string();
        let rest = header[bare.len()..].to_string();
        return Some((bare.trim().to_string(), rest));
    }
    let mut row = method.start_line as usize;
    loop {
        let mut depth = 0i32;
        for (i, ch) in header.char_indices() {
            if ch == '(' {
                depth += 1;
            } else if ch == ')' {
                depth -= 1;
                if depth == 0 {
                    return Some((header[1..i].to_string(), header[i + 1..].to_string()));
                }
            }
        }
        let next = lines.at(row)?;
        if row >= method.start_line as usize + MAX_LIST_LINES {
            return None;
        }
        header.push(' ');
        header.push_str(&re!(r"\s#.*$").replace_first(next, |_| String::new()));
        row += 1;
    }
}

fn read_signature(method: &FlowMethod, lines: &SourceLines) -> Option<RubySignature> {
    let (list, rest) = read_head(method, lines)?;
    let mut s = RubySignature {
        parameters: Vec::new(),
        gathering: HashSet::new(),
        rest,
        required: 0,
        optional: 0,
        takes_rest: false,
        takes_keywords: false,
    };
    for parameter in split_top_level(&list) {
        let keyword = re!(r"^([a-z_]\w*):").captures(&parameter);
        let plain = re!(r"^([a-z_]\w*)\s*(=)?").captures(&parameter);
        if let Some(k) = keyword {
            s.takes_keywords = true;
            s.parameters.push(FlowParameter { name: k.text(1).unwrap_or("").to_string(), position: -1 });
        } else if let Some(rest) = parameter.strip_prefix("**") {
            s.takes_keywords = true;
            s.gathering.insert(rest.to_string());
        } else if parameter.starts_with('*') || parameter == "..." {
            s.takes_rest = true;
            s.gathering.insert(parameter[1..].to_string());
        } else if let Some(rest) = parameter.strip_prefix('&') {
            s.gathering.insert(rest.to_string());
        } else if let Some(p) = plain {
            let name = p.text(1).unwrap_or("").to_string();
            let defaulted = p.text(2).is_some();
            // Past a splat, or a required one past an optional, its place depends on how many a
            // call passes.
            if s.takes_rest || (s.optional > 0 && !defaulted) {
                s.gathering.insert(name);
            } else {
                s.parameters.push(FlowParameter { name, position: (s.required + s.optional) as i32 });
            }
            if defaulted {
                s.optional += 1;
            } else {
                s.required += 1;
            }
        } else {
            s.required += 1; // `(a, b)`, destructured: a place, and no one name's.
        }
    }
    Some(s)
}

/// What a method's head line past its parameters, and lines `from` to `to` (0-based, `to` left
/// out), bind.
fn read_bindings(head_rest: &str, lines: &SourceLines, from: usize, to: usize) -> Bindings {
    let mut b = Bindings::default();
    fn add_words(other: &mut HashSet<String>, text: &str) {
        for m in rxg(WORD).captures_iter(text) {
            other.insert(m.get(0).unwrap().as_str().to_string());
        }
    }
    let mut i = from.saturating_sub(1);
    while i < to && i < lines.len() {
        let line = if i < from { head_rest } else { lines.at(i).unwrap_or("") };
        i += 1;
        if line.len() > MAX_READ_LINE {
            add_words(&mut b.other, line);
            continue;
        }
        let has_assignment = line.contains('=');
        let has_bars = line.contains('|');
        let has_arrow = line.contains("->");
        let has_word_in = line.contains(" in ") || line.starts_with("in ");
        if !has_assignment && !has_bars && !has_arrow && !has_word_in {
            continue;
        }
        if has_assignment {
            let statement = Re::cached(STATEMENT_ASSIGNMENT, "s").captures(line);
            if let Some(st) = &statement {
                let raw = st.text(2).unwrap_or("");
                let value = match re!(r"\s#").find(raw) {
                    Some(c) => &raw[..c.start()],
                    None => raw,
                }
                .trim_end()
                .to_string();
                b.assigned.entry(st.text(1).unwrap_or("").to_string()).or_default().push(value);
            }
            let offset = if statement.is_some() { line.find('=').map_or(0, |p| p + 1) } else { 0 };
            for m in rxg(ANY_ASSIGNMENT).captures_iter(&line[offset..]) {
                b.other.insert(m.text(1).unwrap_or("").to_string());
            }
            for m in rxg(RESCUED_OR_MATCHED).captures_iter(line) {
                b.other.insert(m.text(1).unwrap_or("").to_string());
            }
            if let Some(several) = rx(SEVERAL_ASSIGNMENT).captures(line) {
                add_words(&mut b.other, several.text(1).unwrap_or(""));
            }
        }
        if has_bars {
            for m in rxg(BLOCK_PARAMETERS).captures_iter(line) {
                add_words(&mut b.other, m.text(1).unwrap_or(""));
            }
        }
        if has_arrow {
            for m in rxg(LAMBDA_PARAMETERS).captures_iter(line) {
                add_words(&mut b.other, m.text(1).unwrap_or(""));
            }
        }
        if has_word_in {
            for m in rxg(FOR_VARIABLES).captures_iter(line) {
                add_words(&mut b.other, m.text(1).unwrap_or(""));
            }
            if let Some(clause) = rx(PATTERN_CLAUSE).captures(line) {
                add_words(&mut b.other, clause.text(1).unwrap_or(""));
            }
        }
    }
    b
}

/// Whether `argument_list`, from the `(` on, closes at its very end.
fn closes_at_end(argument_list: &str) -> bool {
    let mut depth = 0i32;
    let n = argument_list.chars().count();
    for (i, ch) in argument_list.chars().enumerate() {
        if ch == '(' {
            depth += 1;
        } else if ch == ')' {
            depth -= 1;
            if depth == 0 {
                return i == n - 1;
            }
        }
    }
    false
}

/// The class a `Foo.new(…)` constructs, or `None` for any other text.
fn constructed_class(text: &str) -> Option<String> {
    let made = Re::cached(CONSTRUCTED, "s").captures(text)?;
    match made.text(2) {
        Some(list) if !closes_at_end(list) => None,
        _ => made.text(1).map(str::to_string),
    }
}

/// The Ruby side of one pass over one state of the graph.
pub struct RubyFlow<'a> {
    ctx: &'a Ctx,
    methods: &'a FlowMethods<'a>,
    texts: RefCell<HashMap<String, Rc<SourceLines>>>,
    signatures: RefCell<HashMap<String, Option<Rc<RubySignature>>>>,
    bindings: RefCell<HashMap<String, Rc<Bindings>>>,
    arguments: RefCell<HashMap<String, Option<Rc<RubyArguments>>>>,
    self_memo: RefCell<HashMap<String, bool>>,
}

/// Follow a value back through a parameter of the method passing it.
pub type Follow<'f> = dyn Fn(&FlowMethod, &FlowParameter) -> Answer + 'f;

impl<'a> RubyFlow<'a> {
    pub fn new(ctx: &'a Ctx, methods: &'a FlowMethods<'a>) -> RubyFlow<'a> {
        RubyFlow {
            ctx,
            methods,
            texts: RefCell::default(),
            signatures: RefCell::default(),
            bindings: RefCell::default(),
            arguments: RefCell::default(),
            self_memo: RefCell::default(),
        }
    }

    /// A file's text, as the index reads it: nothing for one it wouldn't take.
    fn text_of(&self, file: &str) -> Rc<str> {
        self.ctx.read_file(file).unwrap_or_else(|| Rc::from(""))
    }

    fn lines_of(&self, file: &str) -> Rc<SourceLines> {
        if let Some(lines) = self.texts.borrow().get(file) {
            return lines.clone();
        }
        let lines = Rc::new(SourceLines::new(self.text_of(file)));
        self.texts.borrow_mut().insert(file.to_string(), lines.clone());
        lines
    }

    /// The innermost class or module around a line.
    fn scope_at(&self, file: &str, line: u32) -> Option<FlowMethod> {
        let mut innermost: Option<FlowMethod> = None;
        for node in self.methods.scopes_in(file).iter() {
            if node.start_line <= line
                && node.end_line >= line
                && innermost.as_ref().is_none_or(|i| node.end_line - node.start_line < i.end_line - i.start_line)
            {
                innermost = Some(node.clone());
            }
        }
        innermost
    }

    /// The method's parameters and arity, read from its declaration.
    pub fn signature(&self, method: &FlowMethod) -> Option<Rc<RubySignature>> {
        if let Some(s) = self.signatures.borrow().get(&method.id) {
            return s.clone();
        }
        let s = read_signature(method, &self.lines_of(&method.file_path)).map(Rc::new);
        self.signatures.borrow_mut().insert(method.id.clone(), s.clone());
        s
    }

    /// What the body of a method binds.
    fn bindings_of(&self, node: &FlowMethod) -> Rc<Bindings> {
        if let Some(b) = self.bindings.borrow().get(&node.id) {
            return b.clone();
        }
        let rest = self.signature(node).map(|s| s.rest.clone()).unwrap_or_default();
        let b = Rc::new(read_bindings(&rest, &self.lines_of(&node.file_path), node.start_line as usize, node.end_line as usize));
        self.bindings.borrow_mut().insert(node.id.clone(), b.clone());
        b
    }

    fn arguments_at(&self, site: &FlowSite) -> Option<Rc<RubyArguments>> {
        let key = site.key();
        if let Some(a) = self.arguments.borrow().get(&key) {
            return a.clone();
        }
        let lines = self.lines_of(&site.file_path);
        let args = after_called_name(site, &lines)
            .and_then(|(line, column)| read_argument_list(&lines, line, column))
            .map(|list| Rc::new(sort_arguments(&list)));
        self.arguments.borrow_mut().insert(key, args.clone());
        args
    }

    /// Whether `method`, a `def` directly in its class, can only ever run with that class's `self`.
    fn self_is_the_class(&self, method: &FlowMethod, scope: &FlowMethod) -> bool {
        if let Some(&a) = self.self_memo.borrow().get(&method.id) {
            return a;
        }
        let lines = self.lines_of(&method.file_path);
        let mut answer = true;
        let mut i = (method.start_line as usize).saturating_sub(1);
        while answer && i < method.end_line as usize && i < lines.len() {
            if rx(OTHER_SELF).is_match(lines.at(i).unwrap_or("")) {
                answer = false;
            }
            i += 1;
        }
        // A `def` inside a block of the class body (`included do`, `Struct.new do`) belongs to
        // whatever the block runs on.
        let indent_of = |line: &str| line.find(|c: char| !c.is_whitespace());
        let mut indent = lines.at((method.start_line as usize).saturating_sub(1)).and_then(indent_of).unwrap_or(0);
        let mut i = method.start_line as i64 - 2;
        while answer && i >= scope.start_line as i64 {
            let line = lines.at(i as usize).unwrap_or("");
            i -= 1;
            let Some(at) = indent_of(line) else { continue };
            if at >= indent || line[at..].starts_with('#') {
                continue;
            }
            indent = at;
            if re!(r"(?:\bdo(?:\s*\|[^|]*\|)?|\{(?:\s*\|[^|]*\|)?)\s*(?:#.*)?$").is_match(line) {
                answer = false;
            }
        }
        self.self_memo.borrow_mut().insert(method.id.clone(), answer);
        answer
    }

    /// The value of `self` at a site: only inside a method of a class says which.
    fn self_value(&self, site: &FlowSite) -> Answer {
        let caller = self.methods.get(&site.caller_id).ok_or("self-outside-a-method")?;
        let scope = self.scope_at(&site.file_path, site.line).filter(|s| s.kind == NodeKind::Class && owner_of(&caller.qualified_name) == s.qualified_name);
        let scope = scope.ok_or("self-in-a-module")?;
        if !self.self_is_the_class(&caller, &scope) {
            return Err("self-in-a-block");
        }
        Ok(FlowValue { ty: scope.qualified_name.clone(), is_class: caller.is_static, outside: false })
    }

    /// The class a constant written at a site names, itself or as what its `.new` makes.
    fn constant_value(&self, written: &str, is_class: bool, site: &FlowSite) -> Answer {
        let top = written.starts_with("::");
        let name = if top { &written[2..] } else { written };
        let scope = if top { String::new() } else { self.scope_at(&site.file_path, site.line).map(|s| s.qualified_name.clone()).unwrap_or_default() };
        let qn = ruby_constant_qn(name, &scope, self.ctx);
        let declared: Vec<NodeRef> = self.ctx.get_nodes_by_qualified_name(&qn).iter().filter(|n| n.language == Language::Ruby).cloned().collect();
        if declared.iter().any(|n| n.kind == NodeKind::Class) {
            return Ok(FlowValue { ty: qn, is_class, outside: false });
        }
        if !declared.is_empty() {
            return Err("constant-is-no-class");
        }
        // Nothing of the name anywhere is a class from outside. A namesake elsewhere may be what
        // an inherited constant lookup finds, which this can't follow.
        let last = &name[name.rfind(':').map_or(0, |p| p + 1)..];
        if self.ctx.get_nodes_by_name(last).iter().any(|n| n.language == Language::Ruby) {
            return Err("constant-not-found");
        }
        Ok(FlowValue { ty: name.to_string(), is_class, outside: true })
    }

    /// The value of a word at a site: a parameter of the calling method, or a local it assigns.
    fn word_value(&self, word: &str, site: &FlowSite, follow: &Follow<'_>, depth: usize) -> Answer {
        let caller = self.methods.get(&site.caller_id).ok_or("word-outside-a-method")?;
        let signature = self.signature(&caller).ok_or("unread-declaration")?;
        if let Some(parameter) = signature.parameters.iter().find(|p| p.name == word) {
            return follow(&caller, parameter);
        }
        if signature.gathering.contains(word) {
            return Err("gathered-parameter");
        }
        let bound = self.bindings_of(&caller);
        if bound.other.contains(word) {
            return Err("local-bound-otherwise");
        }
        let assigned = bound.assigned.get(word).ok_or("word-is-a-call")?;
        let mut value: Option<FlowValue> = None;
        for text in assigned {
            let one = self.value_of(text, site, follow, depth + 1)?;
            if value.as_ref().is_some_and(|v| v.ty != one.ty || v.is_class != one.is_class) {
                return Err("sites-disagree");
            }
            value = Some(one);
        }
        value.ok_or("word-is-a-call")
    }

    /// What a value written at a site is, when its text says so exactly.
    fn value_of(&self, text: &str, site: &FlowSite, follow: &Follow<'_>, depth: usize) -> Answer {
        if depth > 4 {
            return Err("too-deep");
        }
        if text == "self" {
            return self.self_value(site);
        }
        if let Some(made) = constructed_class(text) {
            return self.constant_value(&made, false, site);
        }
        if rx(CONSTANT_PATH).is_match(text) {
            return self.constant_value(text, true, site);
        }
        if rx(LOCAL_NAME).is_match(text) {
            return self.word_value(text, site, follow, depth);
        }
        if let Some(ivar) = rx(INSTANCE_VARIABLE).captures(text) {
            // Typed the way a call on the variable is, by the class's `initialize`. In the site's
            // own file, or a sync that edits it has no way here.
            if let Some(scope) = self.scope_at(&site.file_path, site.line) {
                if self.ctx.get_nodes_by_qualified_name(&format!("{}::initialize", scope.qualified_name)).iter().any(|n| n.file_path != site.file_path) {
                    return Err("typed-in-another-file");
                }
            }
            let r = UnresolvedRef {
                from_node_id: site.caller_id.clone(),
                reference_name: text.to_string(),
                reference_kind: ReferenceKind::Edge(EdgeKind::Calls),
                line: site.line,
                column: site.column,
                file_path: site.file_path.clone(),
                language: Language::Ruby,
                candidates: None,
                row_id: None,
                name_tail: None,
            };
            return ivar_class_node(ivar.text(1).unwrap_or(""), &r, self.ctx)
                .map(|cls| FlowValue { ty: cls.qualified_name.clone(), is_class: false, outside: false })
                .ok_or("untyped-instance-variable");
        }
        Err("untyped-argument")
    }

    /// The method a reference calls, and its receiver as written (`None` for none).
    pub fn read_call(reference_name: &str) -> Option<(String, Option<String>)> {
        let dot = reference_name.rfind('.');
        let method = dot.map_or(reference_name, |d| &reference_name[d + 1..]);
        re!(r"^[A-Za-z_]\w*[?!]?$").is_match(method).then(|| (method.to_string(), dot.map(|d| reference_name[..d].to_string())))
    }

    /// The names the body of `method` calls something on, with the names of those calls.
    pub fn receivers(&self, method: &FlowMethod) -> HashMap<String, HashSet<String>> {
        let mut calls: HashMap<String, HashSet<String>> = HashMap::new();
        let mut add = |text: &str| {
            for m in rxg(CALL_ON_A_WORD).captures_iter(text) {
                calls.entry(m.text(1).unwrap_or("").to_string()).or_default().insert(m.text(2).unwrap_or("").to_string());
            }
        };
        add(&self.signature(method).map(|s| s.rest.clone()).unwrap_or_default());
        if method.end_line > method.start_line {
            add(self.lines_of(&method.file_path).text(method.start_line as usize, method.end_line as usize));
        }
        calls
    }

    /// Whether the site's edge says what the call reaches, rather than a guess at it.
    pub fn is_known(&self, site: &FlowSite) -> bool {
        let Some(target_id) = site.target_id.as_deref() else { return false };
        if site.via_flow {
            return false;
        }
        if site.receiver.is_some() {
            return site.confidence >= 0.85;
        }
        // A call with no receiver is a call on `self`. In a class, resolution took it only from
        // what the class answers. In a module or a script it found it by its name, unless the
        // module declares it.
        let (Some(scope), Some(target)) = (self.scope_at(&site.file_path, site.line), self.methods.get(target_id)) else { return false };
        scope.kind == NodeKind::Class || owner_of(&target.qualified_name) == scope.qualified_name
    }

    /// Whether a site whose receiver is unknown can't be a call of a method of this signature.
    pub fn cannot_call(&self, site: &FlowSite, signature: &RubySignature) -> bool {
        // A class named as the receiver was asked for a method of its own, so no instance's is
        // reached. A constant holding a value may be anything.
        if let Some(receiver) = site.receiver.as_deref() {
            if rx(CONSTANT_PATH).is_match(receiver) {
                let last = &receiver[receiver.rfind(':').map_or(0, |p| p + 1)..];
                if !self
                    .ctx
                    .get_nodes_by_name(last)
                    .iter()
                    .any(|n| n.language == Language::Ruby && matches!(n.kind, NodeKind::Constant | NodeKind::Variable))
                {
                    return true;
                }
            }
        }
        self.arguments_at(site).is_some_and(|args| !signature.accepts(&args))
    }

    /// The methods a call of `method` may dispatch to: overrides up and down its class's
    /// ancestry, itself included.
    pub fn family(&self, method: &FlowMethod) -> HashSet<String> {
        let owner = owner_of(&method.qualified_name);
        let up = ruby_ancestry(owner, self.ctx, true);
        let mut family = HashSet::from([method.id.clone()]);
        for other in self.ctx.get_nodes_by_name(&method.name).iter() {
            if other.kind != NodeKind::Method || other.language != Language::Ruby || (other.is_static == Some(true)) != method.is_static {
                continue;
            }
            let theirs = owner_of(&other.qualified_name);
            if up.contains(theirs) || ruby_ancestry(theirs, self.ctx, true).contains(owner) {
                family.insert(other.id.clone());
            }
        }
        family
    }

    /// Whether a method of this name hands its call on to a method the graph can't name (`super`).
    pub fn hands_on(&self, name: &str) -> bool {
        self.ctx.get_nodes_by_name(name).iter().any(|n| {
            if n.kind != NodeKind::Method || n.language != Language::Ruby {
                return false;
            }
            let lines = self.lines_of(&n.file_path);
            let from = (n.start_line as usize).saturating_sub(1);
            (from..(n.end_line as usize).min(lines.len())).any(|i| lines.at(i).is_some_and(|l| l.contains("super") && re!(r"\bsuper\b").is_match(l)))
        })
    }

    /// Whether anything inside `method` binds `name` over the parameter.
    pub fn rebinds(&self, name: &str, method: &FlowMethod) -> bool {
        let bound = self.bindings_of(method);
        bound.other.contains(name) || bound.assigned.contains_key(name)
    }

    /// The value of the argument the site passes to `parameter`.
    pub fn argument_value(&self, site: &FlowSite, parameter: &FlowParameter, follow: &Follow<'_>) -> Answer {
        let args = self.arguments_at(site).ok_or("unread-call")?;
        let text = if parameter.position < 0 {
            let text = args.keywords.get(&parameter.name);
            if text.is_none() && args.keyword_splat {
                return Err("keyword-splat");
            }
            text
        } else {
            let position = parameter.position as usize;
            if args.splat_at.is_some_and(|s| s <= position) {
                return Err("splat");
            }
            args.positional.get(position)
        };
        let text = text.ok_or("not-passed")?;
        self.value_of(text, site, follow, 0)
    }

    /// The method `name` a value answers; `None` when the project declares none.
    pub fn method_on(&self, value: &FlowValue, name: &str) -> Option<FlowMethod> {
        if value.outside {
            return None;
        }
        let first = |mut found: Vec<NodeRef>| -> Option<FlowMethod> {
            found.sort_by(|a, b| a.file_path.cmp(&b.file_path).then(a.start_line.cmp(&b.start_line)));
            found.first().and_then(|n| self.methods.get(&n.id))
        };
        if value.is_class {
            return first(ruby_methods_of(&value.ty, name, self.ctx).into_iter().filter(|n| n.is_static == Some(true)).collect())
                .or_else(|| ruby_class_method_of(&value.ty, name, self.ctx).and_then(|n| self.methods.get(&n.id)));
        }
        for owner in ruby_ancestry(&value.ty, self.ctx, true).iter() {
            if let Some(m) = first(ruby_methods_of(owner, name, self.ctx).into_iter().filter(|n| n.is_static != Some(true)).collect()) {
                return Some(m);
            }
        }
        None
    }

    /// Whether a value that has no method `name` answers it some way that calls no method of the
    /// project.
    pub fn answers_itself(&self, value: &FlowValue, name: &str) -> bool {
        if value.outside {
            return true;
        }
        if value.is_class {
            return false;
        }
        // `attr_reader :name`, in a list that may run over lines.
        let reader = Re::cached(&format!(r"\battr_(?:reader|accessor)\s+(?::\w+[?!]?\s*,\s*)*:{}(?![\w?!])", escape_regex(name)), "");
        for owner in ruby_ancestry(&value.ty, self.ctx, true).iter() {
            for declaration in self.ctx.get_nodes_by_qualified_name(owner).iter() {
                if declaration.language != Language::Ruby || !matches!(declaration.kind, NodeKind::Class | NodeKind::Module) {
                    continue;
                }
                let lines = self.lines_of(&declaration.file_path);
                if reader.is_match(lines.text((declaration.start_line as usize).saturating_sub(1), declaration.end_line as usize)) {
                    return true;
                }
            }
        }
        false
    }

    /// How many methods of the name the project declares.
    pub fn declared(&self, name: &str) -> usize {
        self.ctx.get_nodes_by_name(name).iter().filter(|n| n.language == Language::Ruby && matches!(n.kind, NodeKind::Method | NodeKind::Function)).count()
    }

    /// How often each of `files` writes each of `names`, whatever for: a symbol, a string, an
    /// alias, or a call the graph doesn't record. A comment line calls nothing.
    pub fn written(&self, names: &HashSet<String>, files: &[String]) -> HashMap<String, HashMap<String, usize>> {
        let mut written: HashMap<String, HashMap<String, usize>> = HashMap::new();
        if names.is_empty() {
            return written;
        }
        let mut sorted: Vec<&String> = names.iter().collect();
        sorted.sort();
        let word = Re::cached(&format!(r"(?<![\w@$])(?:{})(?![\w?!])", sorted.iter().map(|n| escape_regex(n)).collect::<Vec<_>>().join("|")), "g");
        for file in files {
            let text = self.text_of(file);
            let mut line_end: Option<usize> = None;
            let mut in_comment = false;
            for m in word.captures_iter(&text) {
                let whole = m.get(0).unwrap();
                if line_end.is_none_or(|end| whole.start() > end) {
                    let line_start = text[..whole.start()].rfind('\n').map_or(0, |p| p + 1);
                    let end = text[whole.start()..].find('\n').map_or(text.len(), |p| whole.start() + p);
                    line_end = Some(end);
                    in_comment = re!(r"^[ \t]*#").is_match(&text[line_start..end]);
                }
                if in_comment {
                    continue;
                }
                *written.entry(whole.as_str().to_string()).or_default().entry(file.clone()).or_default() += 1;
            }
        }
        written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_at_top_level_commas_only() {
        assert_eq!(split_top_level(r#"a, "b,c", f(1, 2), [3, 4]"#), [r#"a"#, r#""b,c""#, "f(1, 2)", "[3, 4]"]);
    }

    #[test]
    fn reads_what_a_constructor_makes() {
        assert_eq!(constructed_class("Foo::Bar.new(x, [1, 2])").as_deref(), Some("Foo::Bar"));
        assert_eq!(constructed_class("Foo.new.tap { |t| t }"), None);
        assert_eq!(constructed_class("Foo.new(a).b(c)"), None);
    }

    #[test]
    fn sorts_positional_keyword_and_splat_arguments() {
        let args = sort_arguments(&["self".into(), "flag: 1".into(), "*rest".into(), "&blk".into()]);
        assert_eq!(args.positional, ["self", "*rest"]);
        assert_eq!(args.keywords.get("flag").map(String::as_str), Some("1"));
        assert_eq!(args.splat_at, Some(1));
    }
}
