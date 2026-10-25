//! Feature-gated C functions — port of `src/resolution/c-feature-variants.ts`.
//!
//! A function defined under `#ifdef FEATURE` is a build variant: when no build file or header
//! defines `FEATURE`, a call that does not sit under the same condition cannot reach it. The
//! `#if` conditions are evaluated three-valued (true / false / unknown) so only a variant that
//! is certainly off is dropped.

use crate::cpp_macro_visibility::directive_lines;
use crate::ctx::Ctx;
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::Node;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

type Truth = Option<bool>;

fn and(a: Truth, b: Truth) -> Truth {
    if a == Some(false) || b == Some(false) {
        Some(false)
    } else if a == Some(true) && b == Some(true) {
        Some(true)
    } else {
        None
    }
}

fn or(a: Truth, b: Truth) -> Truth {
    if a == Some(true) || b == Some(true) {
        Some(true)
    } else if a == Some(false) && b == Some(false) {
        Some(false)
    } else {
        None
    }
}

fn not(a: Truth) -> Truth {
    a.map(|v| !v)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    If,
    Ifdef,
    Ifndef,
    Elifdef,
    Elifndef,
    Elif,
    Else,
    Endif,
}

impl Op {
    fn parse(s: &str) -> Op {
        match s {
            "if" => Op::If,
            "ifdef" => Op::Ifdef,
            "ifndef" => Op::Ifndef,
            "elifdef" => Op::Elifdef,
            "elifndef" => Op::Elifndef,
            "elif" => Op::Elif,
            "else" => Op::Else,
            _ => Op::Endif,
        }
    }
}

#[derive(Clone)]
struct Directive {
    op: Op,
    expression: String,
    line: usize,
}

struct FileDirectives {
    conditionals: Vec<Directive>,
    defines: HashSet<String>,
}

#[derive(Default)]
struct Memo {
    files: RefCell<HashMap<String, Rc<FileDirectives>>>,
    build_text: RefCell<HashMap<String, Rc<String>>>,
    components: RefCell<HashMap<String, String>>,
    c_files: RefCell<Option<Rc<Vec<String>>>>,
    defines: RefCell<HashMap<String, bool>>,
    answers: RefCell<HashMap<String, bool>>,
}

const PLATFORM_FLAGS: [&str; 4] = ["WIN32", "WIN64", "linux", "unix"];
const BUILD_FILES: [&str; 15] = [
    "Makefile", "makefile", "GNUmakefile", "Makefile.am", "Makefile.in", "CMakeLists.txt", "meson.build",
    "meson_options.txt", "configure.ac", "configure.in", "config.h.in", "BUILD", "BUILD.bazel", "SConstruct", "SConscript",
];

fn dir_of(file: &str) -> &str {
    &file[..file.rfind('/').unwrap_or(0)]
}

fn ancestors(file: &str) -> Vec<String> {
    let mut dirs = Vec::new();
    let mut dir = dir_of(file);
    loop {
        dirs.push(dir.to_string());
        if dir.is_empty() {
            return dirs;
        }
        dir = dir_of(dir);
    }
}

/// Is `function` a feature variant that is off, as seen from `r`?
pub fn is_c_feature_variant(function: &Node, r_file: &str, r_line: u32, ctx: &Ctx) -> bool {
    let memo = ctx.memo::<Memo>();
    let same_file = function.file_path == r_file;
    let key = format!(
        "v\0{}\0{}",
        function.id,
        if same_file { r_line.to_string() } else { dir_of(r_file).to_string() }
    );
    if let Some(hit) = memo.answers.borrow().get(&key) {
        return *hit;
    }
    let file = directives_of(&function.file_path, ctx, &memo);
    let mut groups = enclosing_tests(&file.conditionals, function.start_line as usize);
    if same_file {
        let around = enclosing_tests(&file.conditionals, r_line as usize);
        let mut shared = 0;
        while shared < groups.len()
            && shared < around.len()
            && groups[shared][0].line == around[shared][0].line
            && groups[shared].len() == around[shared].len()
        {
            shared += 1;
        }
        groups.drain(..shared);
    }
    let off = !groups.is_empty()
        && is_off(&groups, &file, &[function.file_path.as_str(), r_file], ctx, &memo);
    memo.answers.borrow_mut().insert(key, off);
    off
}

/// Is the code at `path:line` inside a conditional that is certainly off by default?
pub fn is_off_by_default(path: &str, line: usize, ctx: &Ctx) -> bool {
    let memo = ctx.memo::<Memo>();
    let key = format!("o\0{path}\0{line}");
    if let Some(hit) = memo.answers.borrow().get(&key) {
        return *hit;
    }
    let file = directives_of(path, ctx, &memo);
    let groups = enclosing_tests(&file.conditionals, line);
    let off = !groups.is_empty() && is_off(&groups, &file, &[path], ctx, &memo);
    memo.answers.borrow_mut().insert(key, off);
    off
}

fn is_off(groups: &[Vec<Directive>], file: &FileDirectives, scope: &[&str], ctx: &Ctx, memo: &Memo) -> bool {
    let mut dirs: Vec<String> = Vec::new();
    for f in scope {
        for d in ancestors(f) {
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
    }
    let flag = |name: &str| -> Truth {
        if name.starts_with('_') || PLATFORM_FLAGS.contains(&name) || file.defines.contains(name) {
            return None;
        }
        let named = Re::cached(&format!(r"(?:^|[^\w]|[-/]D){}\b", escape_regex(name)), "m");
        if dirs.iter().any(|dir| named.is_match(&build_text_of(dir, ctx, memo))) {
            return None;
        }
        let mut components: Vec<String> = Vec::new();
        for f in scope {
            let c = component_of(dir_of(f), ctx, memo);
            if !components.contains(&c) {
                components.push(c);
            }
        }
        if components.iter().any(|component| is_defined_in(name, component, ctx, memo)) {
            None
        } else {
            Some(false)
        }
    };
    groups.iter().any(|tests| branch_truth(tests, &flag) == Some(false))
}

fn enclosing_tests(conditionals: &[Directive], line: usize) -> Vec<Vec<Directive>> {
    let mut groups: Vec<Vec<Directive>> = Vec::new();
    for d in conditionals {
        if d.line >= line {
            break;
        }
        match d.op {
            Op::If | Op::Ifdef | Op::Ifndef => groups.push(vec![d.clone()]),
            Op::Endif => {
                groups.pop();
            }
            _ => {
                if let Some(last) = groups.last_mut() {
                    last.push(d.clone());
                }
            }
        }
    }
    groups
}

fn branch_truth(tests: &[Directive], flag: &dyn Fn(&str) -> Truth) -> Truth {
    let mut earlier: Truth = Some(false);
    for t in &tests[..tests.len() - 1] {
        earlier = or(earlier, test_truth(t, flag));
        if earlier == Some(true) {
            return Some(false);
        }
    }
    and(not(earlier), test_truth(&tests[tests.len() - 1], flag))
}

fn test_truth(d: &Directive, flag: &dyn Fn(&str) -> Truth) -> Truth {
    let trimmed = d.expression.trim();
    let name = re!(r"^\w+$").find(trimmed).map(|m| m.as_str().to_string());
    match d.op {
        Op::Ifdef | Op::Elifdef => name.and_then(|n| flag(&n)),
        Op::Ifndef | Op::Elifndef => name.and_then(|n| not(flag(&n))),
        Op::Else => Some(true),
        _ => evaluate(&d.expression, flag),
    }
}

struct Parser<'a> {
    tokens: Vec<String>,
    i: usize,
    flag: &'a dyn Fn(&str) -> Truth,
}

impl Parser<'_> {
    fn tok(&self, at: usize) -> Option<&str> {
        self.tokens.get(at).map(String::as_str)
    }

    /// The grammar check: `term (('&&' | '||') term)*`.
    fn shape(&mut self) -> bool {
        if !self.term_shape() {
            return false;
        }
        while matches!(self.tok(self.i), Some("&&") | Some("||")) {
            self.i += 1;
            if !self.term_shape() {
                return false;
            }
        }
        true
    }

    fn term_shape(&mut self) -> bool {
        let t = self.tokens.get(self.i).cloned();
        self.i += 1;
        match t.as_deref() {
            Some("!") => self.term_shape(),
            Some("(") => {
                let ok = self.shape();
                let close = self.tokens.get(self.i).map(String::as_str) == Some(")");
                self.i += 1;
                ok && close
            }
            Some("defined") => {
                let paren = self.tok(self.i) == Some("(");
                if paren {
                    self.i += 1;
                }
                let ident = self.tokens.get(self.i).cloned().unwrap_or_default();
                self.i += 1;
                if !re!(r"^[A-Za-z_]\w*$").is_match(&ident) {
                    return false;
                }
                if paren {
                    let close = self.tok(self.i) == Some(")");
                    self.i += 1;
                    return close;
                }
                true
            }
            Some(t) => re!(r"^(0x[\da-f]+|\d+)[ul]*$", "i").is_match(t) || re!(r"^[A-Za-z_]\w*$").is_match(t),
            None => false,
        }
    }

    fn disjunction(&mut self) -> Truth {
        let mut value = self.conjunction();
        while self.tok(self.i) == Some("||") {
            self.i += 1;
            let rhs = self.conjunction();
            value = or(value, rhs);
        }
        value
    }

    fn conjunction(&mut self) -> Truth {
        let mut value = self.term();
        while self.tok(self.i) == Some("&&") {
            self.i += 1;
            let rhs = self.term();
            value = and(value, rhs);
        }
        value
    }

    fn term(&mut self) -> Truth {
        let t = self.tokens.get(self.i).cloned().unwrap_or_default();
        self.i += 1;
        if t == "!" {
            return not(self.term());
        }
        if t == "(" {
            let value = self.disjunction();
            self.i += 1;
            return value;
        }
        if t == "defined" {
            if self.tok(self.i) == Some("(") {
                let name = self.tokens.get(self.i + 1).cloned().unwrap_or_default();
                let value = (self.flag)(&name);
                self.i += 3;
                return value;
            }
            let name = self.tokens.get(self.i).cloned().unwrap_or_default();
            self.i += 1;
            return (self.flag)(&name);
        }
        if re!(r"^\d|^0x", "i").is_match(&t) {
            let digits = re!(r"[ul]+$", "i").replace_first(&t, |_| String::new());
            let hex = re!(r"^0x", "i").is_match(&t);
            let v = if hex {
                i64::from_str_radix(&digits[2..], 16).map(|n| n != 0).unwrap_or(false)
            } else {
                // `parseInt` reads the leading digits.
                let lead: String = digits.chars().take_while(|c| c.is_ascii_digit()).collect();
                lead.parse::<u128>().map(|n| n != 0).unwrap_or(false)
            };
            return Some(v);
        }
        if (self.flag)(&t) == Some(false) {
            Some(false)
        } else {
            None
        }
    }
}

fn evaluate(expression: &str, flag: &dyn Fn(&str) -> Truth) -> Truth {
    let tokens: Vec<String> =
        re!(r"0x[\da-f]+\w*|\d\w*|[A-Za-z_]\w*|&&|\|\||\S", "gi").captures_iter(expression).map(|c| c.text(0).unwrap_or("").to_string()).collect();
    let mut p = Parser { tokens, i: 0, flag };
    if p.tokens.is_empty() || !p.shape() || p.i != p.tokens.len() {
        return None;
    }
    p.i = 0;
    p.disjunction()
}

fn directives_of(file: &str, ctx: &Ctx, memo: &Memo) -> Rc<FileDirectives> {
    if let Some(hit) = memo.files.borrow().get(file) {
        return Rc::clone(hit);
    }
    let source = ctx.read_file(file);
    let lines = directive_lines(source.as_deref().unwrap_or(""));
    let mut conditionals: Vec<Directive> = Vec::new();
    let mut defines: HashSet<String> = HashSet::new();
    for i in 0..lines.len() {
        if let Some(define) = re!(r"^\s*#\s*define\s+(\w+)").captures(&lines[i]) {
            defines.insert(define.text(1).unwrap_or("").to_string());
            continue;
        }
        let Some(m) = re!(r"^\s*#\s*(ifdef|ifndef|if|elifdef|elifndef|elif|else|endif)\b(.*)$").captures(&lines[i]) else { continue };
        let mut expression = m.text(2).unwrap_or("").to_string();
        let mut j = i;
        while re!(r"\\\s*$").is_match(&lines[j]) && j + 1 < lines.len() {
            expression = re!(r"\\\s*$").replace_first(&expression, |_| " ".to_string()) + &lines[j + 1];
            j += 1;
        }
        conditionals.push(Directive { op: Op::parse(m.text(1).unwrap_or("")), expression, line: i + 1 });
    }
    let directives = Rc::new(FileDirectives { conditionals, defines });
    memo.files.borrow_mut().insert(file.to_string(), Rc::clone(&directives));
    directives
}

fn build_text_of(dir: &str, ctx: &Ctx, memo: &Memo) -> Rc<String> {
    if let Some(hit) = memo.build_text.borrow().get(dir) {
        return Rc::clone(hit);
    }
    let text: Vec<String> = BUILD_FILES
        .iter()
        .map(|name| {
            let p = if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") };
            ctx.read_file(&p).map(|s| s.to_string()).unwrap_or_default()
        })
        .collect();
    let text = Rc::new(text.join("\n"));
    memo.build_text.borrow_mut().insert(dir.to_string(), Rc::clone(&text));
    text
}

fn component_of(dir: &str, ctx: &Ctx, memo: &Memo) -> String {
    if let Some(hit) = memo.components.borrow().get(dir) {
        return hit.clone();
    }
    let component = if dir.is_empty() || !build_text_of(dir, ctx, memo).trim().is_empty() {
        dir.to_string()
    } else {
        component_of(dir_of(dir), ctx, memo)
    };
    memo.components.borrow_mut().insert(dir.to_string(), component.clone());
    component
}

fn is_defined_in(name: &str, component: &str, ctx: &Ctx, memo: &Memo) -> bool {
    let key = format!("{name}\0{component}");
    if let Some(hit) = memo.defines.borrow().get(&key) {
        return *hit;
    }
    if memo.c_files.borrow().is_none() {
        let files: Vec<String> = ctx
            .get_all_files()
            .into_iter()
            .filter(|f| re!(r"\.(c|h|cc|cpp|cxx|c\+\+|hpp|hh|hxx|h\+\+|inl|ipp|tcc|inc|m|mm)$", "i").is_match(f))
            .collect();
        *memo.c_files.borrow_mut() = Some(Rc::new(files));
    }
    let c_files = memo.c_files.borrow().clone().unwrap();
    let prefix = if component.is_empty() { String::new() } else { format!("{component}/") };
    let define = Re::cached(&format!(r"^[ \t]*#[ \t]*define[ \t]+{}\b", escape_regex(name)), "m");
    let defined = c_files.iter().any(|file| {
        file.starts_with(&prefix)
            && ctx.file_contains(file, name)
            && define.is_match(ctx.read_file(file).as_deref().unwrap_or(""))
    });
    memo.defines.borrow_mut().insert(key, defined);
    defined
}
