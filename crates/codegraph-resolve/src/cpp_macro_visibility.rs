//! Function-like macros visible at a C/C++ call — port of `src/resolution/cpp-macro-visibility.ts`.
//!
//! A "call" whose name is a function-like macro defined in the translation unit is a macro
//! expansion, not a call, and must never bind to a same-named function elsewhere (#1838).
//! The translation unit is walked from the referencing file with a three-valued (true / false /
//! unknown) model of `#if` conditions, so only a macro that is certainly defined at the call's
//! line is taken as one.

use crate::ctx::Ctx;
use crate::import_resolver::resolve_import_path;
use crate::types::{is_cpp_define_signature, UnresolvedRef};
use codegraph_extract::mask_cpp_raw_strings_source;
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::{nodepath, EdgeKind, Language, NodeKind};
use indexmap::IndexMap;
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

#[derive(Clone)]
enum FileEvent {
    Define { undef: bool, name: String, line: usize, function_like: bool, value: String, wraps_itself: bool },
    Include { quote: String, spec: String, line: usize },
    Branch { op: String, expression: String, guard: bool, line: usize },
    Once { line: usize },
}

#[derive(Clone, Copy)]
struct Event {
    line: usize,
    defined: Truth,
}

#[derive(Clone)]
struct Timeline {
    events: HashMap<String, Vec<Event>>,
    cutoff: usize,
}

#[derive(Default)]
struct Cache {
    summaries: HashMap<String, Rc<Vec<FileEvent>>>,
    includes: HashMap<String, Option<String>>,
    by_basename: Option<HashMap<String, Vec<String>>>,
    roots: IndexMap<String, Rc<Timeline>>,
}

#[derive(Default)]
struct CacheCell(RefCell<Cache>);

const ROOT_TIMELINE_CAP: usize = 32;
const WALK_EVENT_BUDGET: i64 = 1_000_000;

fn is_define(n: &codegraph_types::Node) -> bool {
    n.kind == NodeKind::Constant && is_cpp_define_signature(n.signature.as_deref().unwrap_or(""))
}

/// Is `r`, a C/C++ call, an expansion of a function-like macro defined before it in its
/// translation unit?
pub fn is_visible_cpp_macro(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if !matches!(r.language, Language::C | Language::Cpp) {
        return false;
    }
    if !r.is_kind(EdgeKind::Calls) || !re!(r"^\w+$").is_match(&r.reference_name) {
        return false;
    }
    let same_name = ctx.get_nodes_by_name(&r.reference_name);
    if !same_name.iter().any(|n| is_define(n)) {
        return false;
    }
    if !same_name.iter().any(|n| !is_define(n)) {
        return true;
    }
    let cache = ctx.memo::<CacheCell>();
    let root_key = format!("{}\0{}", r.language.as_str(), r.file_path);
    let existing = cache.0.borrow().roots.get(&root_key).cloned();
    let timeline = match existing {
        Some(t) => t,
        None => {
            let t = Rc::new(walk_translation_unit(&r.file_path, r.language, ctx, &cache));
            let mut c = cache.0.borrow_mut();
            if c.roots.len() >= ROOT_TIMELINE_CAP {
                c.roots.shift_remove_index(0);
            }
            c.roots.insert(root_key, Rc::clone(&t));
            t
        }
    };
    if r.line as usize >= timeline.cutoff {
        return false;
    }
    let before = timeline.events.get(&r.reference_name).map(|e| e.iter().filter(|e| e.line <= r.line as usize).last().copied());
    matches!(before, Some(Some(e)) if e.defined == Some(true))
}

fn summarize(file: &str, ctx: &Ctx, cache: &CacheCell) -> Rc<Vec<FileEvent>> {
    if let Some(hit) = cache.0.borrow().summaries.get(file) {
        return Rc::clone(hit);
    }
    let mut events: Vec<FileEvent> = Vec::new();
    let source = ctx.read_file(file);
    let lines = directive_lines(source.as_deref().unwrap_or(""));
    for (i, text) in lines.iter().enumerate() {
        if let Some(branch) = re!(r"^\s*#\s*(ifdef|ifndef|if|elif|else|endif)\b(.*)$").captures(text) {
            let op = branch.text(1).unwrap_or("").to_string();
            let expression = branch.text(2).unwrap_or("").to_string();
            events.push(FileEvent::Branch {
                guard: guards_itself(&lines, i, &op, &expression),
                op,
                expression,
                line: i + 1,
            });
            continue;
        }
        if let Some(directive) = re!(r"^\s*#\s*(define|undef)\s+(\w+)(\(?)").captures(text) {
            let whole = directive.text(0).unwrap_or("");
            let name = directive.text(2).unwrap_or("").to_string();
            let function_like = directive.text(3) == Some("(");
            events.push(FileEvent::Define {
                undef: directive.text(1) == Some("undef"),
                wraps_itself: function_like && calls_itself(&lines, i, &name),
                name,
                line: i + 1,
                function_like,
                value: text[whole.len()..].to_string(),
            });
            continue;
        }
        if let Some(include) = re!(r#"^\s*#\s*include\s*([<"])([^>"]+)[>"]"#).captures(text) {
            events.push(FileEvent::Include {
                quote: include.text(1).unwrap_or("").to_string(),
                spec: include.text(2).unwrap_or("").to_string(),
                line: i + 1,
            });
        }
        if re!(r"^\s*#\s*pragma\s+once\b").is_match(text) {
            events.push(FileEvent::Once { line: i + 1 });
        }
    }
    let events = Rc::new(events);
    cache.0.borrow_mut().summaries.insert(file.to_string(), Rc::clone(&events));
    events
}

/// The lines of `source` with raw strings masked, with comments and string literals cut out of
/// every preprocessor directive (and block comments blanked everywhere else).
pub fn directive_lines(source: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut in_block = false;
    let masked = mask_cpp_raw_strings_source(source);
    for raw in crate::ctx::split_lines(&masked) {
        let mut text: Vec<char> = raw.chars().collect();
        if in_block {
            let s: String = text.iter().collect();
            match s.find("*/") {
                None => {
                    out.push(String::new());
                    continue;
                }
                Some(end) => {
                    text = s[end + 2..].chars().collect();
                    in_block = false;
                }
            }
        }
        let directive = re!(r"^\s*#").is_match(&text.iter().collect::<String>());
        let mut kept = String::new();
        let mut quote: Option<char> = None;
        let mut i: usize = 0;
        while i < text.len() {
            let c = text[i];
            if let Some(q) = quote {
                if c == '\\' {
                    i += 1;
                } else if c == q {
                    quote = None;
                }
                i += 1;
                continue;
            }
            if c == '"' || c == '\'' {
                quote = Some(c);
                i += 1;
                continue;
            }
            if c == '/' && text.get(i + 1) == Some(&'/') {
                kept = text[..i].iter().collect();
                break;
            }
            if c == '/' && text.get(i + 1) == Some(&'*') {
                let rest: String = text[i + 2..].iter().collect();
                match rest.find("*/") {
                    None => {
                        in_block = true;
                        kept = text[..i].iter().collect();
                        break;
                    }
                    Some(end) => {
                        let mut next: Vec<char> = text[..i].to_vec();
                        next.push(' ');
                        next.extend(rest[end + 2..].chars());
                        text = next;
                        // `i--` then the loop's `i++`: stay on this index.
                        continue;
                    }
                }
            }
            i += 1;
        }
        let full: String = text.iter().collect();
        out.push(if directive { if kept.is_empty() { full } else { kept } } else { raw });
    }
    out
}

fn calls_itself(lines: &[String], index: usize, name: &str) -> bool {
    let mut text = lines[index].clone();
    let mut j = index;
    while re!(r"\\\s*$").is_match(&lines[j]) && j + 1 < lines.len() {
        text.push(' ');
        text.push_str(&lines[j + 1]);
        j += 1;
    }
    let open = text.find('(').map(|p| p + 1).unwrap_or(0);
    let body = &text[open..];
    let n = escape_regex(name);
    Re::cached(&format!(r"(?:\b{n}\s*\(|\(\s*{n}\s*\)\s*\()"), "").is_match(body)
}

fn guards_itself(lines: &[String], index: usize, op: &str, expression: &str) -> bool {
    let name: Option<String> = if op == "ifndef" {
        Some(expression.trim().to_string())
    } else {
        re!(r"^\s*!\s*defined\s*(?:\(\s*(\w+)\s*\)|(\w+))\s*$")
            .captures(expression)
            .and_then(|c| c.text(1).filter(|s| !s.is_empty()).or_else(|| c.text(2).filter(|s| !s.is_empty())).map(String::from))
    };
    let Some(name) = name.filter(|n| re!(r"^\w+$").is_match(n)) else { return false };
    for text in lines.iter().skip(index + 1) {
        if !re!(r"^\s*#").is_match(text) {
            continue;
        }
        return Re::cached(&format!(r"^\s*#\s*define\s+{name}(?:\s*$|\()"), "").is_match(text);
    }
    false
}

fn resolve_include(file: &str, quote: &str, spec: &str, language: Language, ctx: &Ctx, cache: &CacheCell) -> Option<String> {
    let key = format!("{}\0{file}\0{quote}{spec}", language.as_str());
    if let Some(hit) = cache.0.borrow().includes.get(&key) {
        return hit.clone();
    }
    let dir = nodepath::posix::dirname(file);
    let local = nodepath::posix::normalize(&nodepath::posix::join(&[&dir, &spec.replace('\\', "/")]));
    let mut target = if quote == "\"" && !local.starts_with("../") && !nodepath::posix::is_absolute(&local) && ctx.file_exists(&local) {
        Some(local)
    } else {
        resolve_import_path(spec, file, language, ctx)
    };
    if target.is_none() {
        if cache.0.borrow().by_basename.is_none() {
            let mut by_basename: HashMap<String, Vec<String>> = HashMap::new();
            for f in ctx.get_all_files() {
                let base = f[f.rfind('/').map_or(0, |i| i + 1)..].to_string();
                by_basename.entry(base).or_default().push(f);
            }
            cache.0.borrow_mut().by_basename = Some(by_basename);
        }
        let normalized = spec.replace('\\', "/");
        let base = &normalized[normalized.rfind('/').map_or(0, |i| i + 1)..];
        let c = cache.0.borrow();
        let matches: Vec<&String> = c
            .by_basename
            .as_ref()
            .and_then(|m| m.get(base))
            .map(|v| v.iter().filter(|f| **f == normalized || f.ends_with(&format!("/{normalized}"))).collect())
            .unwrap_or_default();
        if matches.len() == 1 {
            target = Some(matches[0].clone());
        }
    }
    cache.0.borrow_mut().includes.insert(key, target.clone());
    target
}

#[derive(Clone)]
struct Definition {
    defined: Truth,
    value: Truth,
    macro_: Truth,
}

struct Visit {
    inherited: Truth,
    start: u64,
    end: u64,
    cuts: Vec<String>,
}

struct Walker<'a> {
    ctx: &'a Ctx,
    cache: &'a CacheCell,
    language: Language,
    timeline: HashMap<String, Vec<Event>>,
    definitions: HashMap<String, Definition>,
    scanning: HashSet<String>,
    macro_names: HashSet<String>,
    once: HashMap<String, Truth>,
    version: u64,
    cuts: Vec<String>,
    visits: HashMap<String, Visit>,
    budget: i64,
    cutoff: usize,
}

impl Walker<'_> {
    fn condition(&self, expression: &str) -> Truth {
        let text = expression.trim();
        if re!(r"^(?:0x[\da-f]+|\d+)[uUlL]*$", "i").is_match(text) {
            let digits = re!(r"[uUlL]+$").replace_first(text, |_| String::new());
            let v = if let Some(h) = digits.strip_prefix("0x").or_else(|| digits.strip_prefix("0X")) {
                u128::from_str_radix(h, 16).map(|n| n != 0).unwrap_or(true)
            } else {
                digits.parse::<f64>().map(|n| n != 0.0).unwrap_or(true)
            };
            return Some(v);
        }
        if let Some(def) = re!(r"^(!)?\s*defined\s*(?:\(\s*(\w+)\s*\)|(\w+))$").captures(text) {
            let name = def.text(2).filter(|s| !s.is_empty()).or_else(|| def.text(3)).unwrap_or("");
            let known = self.definitions.get(name).and_then(|d| d.defined);
            return if def.text(1).is_some() { not(known) } else { known };
        }
        if re!(r"^\w+$").is_match(text) {
            self.definitions.get(text).and_then(|d| d.value)
        } else {
            None
        }
    }

    fn scan(&mut self, file: &str, inherited: Truth, include_line: Option<usize>) {
        if inherited == Some(false) || self.once.get(file).copied() == Some(Some(true)) || self.cutoff != usize::MAX {
            return;
        }
        if self.scanning.contains(file) {
            self.cuts.push(file.to_string());
            return;
        }
        if let Some(seen) = self.visits.get(file) {
            if seen.inherited == inherited
                && seen.start == seen.end
                && seen.end == self.version
                && seen.cuts.iter().all(|c| self.scanning.contains(c))
            {
                let cuts = seen.cuts.clone();
                self.cuts.extend(cuts);
                return;
            }
        }
        let start = self.version;
        let first_cut = self.cuts.len();
        self.scanning.insert(file.to_string());
        let mut active: Truth = inherited;
        struct Frame {
            parent: Truth,
            taken: Truth,
        }
        let mut frames: Vec<Frame> = Vec::new();
        let events = summarize(file, self.ctx, self.cache);
        for ev in events.iter() {
            let ev_line = match ev {
                FileEvent::Define { line, .. } | FileEvent::Include { line, .. } | FileEvent::Branch { line, .. } | FileEvent::Once { line } => *line,
            };
            let line = include_line.unwrap_or(ev_line);
            if self.cutoff == usize::MAX {
                self.budget -= 1;
                if self.budget < 0 {
                    self.cutoff = line;
                }
            }
            if self.cutoff != usize::MAX {
                break;
            }
            match ev {
                FileEvent::Branch { op, expression, guard, .. } => {
                    if op == "if" || op == "ifdef" || op == "ifndef" {
                        let known = self.definitions.get(expression.trim()).and_then(|d| d.defined);
                        let mut selected = if op == "if" {
                            self.condition(expression)
                        } else if op == "ifndef" {
                            not(known)
                        } else {
                            known
                        };
                        if selected.is_none() && *guard {
                            selected = Some(true);
                        }
                        frames.push(Frame { parent: active, taken: selected });
                        active = and(active, selected);
                    } else if op == "endif" {
                        active = frames.pop().map(|f| f.parent).unwrap_or(inherited);
                    } else if let Some(frame) = frames.last_mut() {
                        let test = if op == "else" { Some(true) } else { self.condition_of(expression) };
                        active = and(frame.parent, and(not(frame.taken), test));
                        frame.taken = or(frame.taken, test);
                    }
                    continue;
                }
                _ => {}
            }
            if active == Some(false) {
                continue;
            }
            match ev {
                FileEvent::Once { .. } => {
                    let prior = Some(self.once.get(file).copied().flatten().unwrap_or(false));
                    let next = or(prior, active);
                    if next != prior {
                        self.version += 1;
                    }
                    self.once.insert(file.to_string(), next);
                }
                FileEvent::Include { quote, spec, .. } => {
                    if let Some(target) = resolve_include(file, quote, spec, self.language, self.ctx, self.cache) {
                        self.scan(&target, active, Some(line));
                    }
                }
                FileEvent::Define { undef, name, function_like, value, wraps_itself, .. } => {
                    let prior = self.definitions.get(name).cloned();
                    let defining = !*undef;
                    let macro_ = defining && *function_like && !*wraps_itself;
                    let now: Truth = if active == Some(true) {
                        Some(macro_)
                    } else if prior.as_ref().and_then(|p| p.macro_) == Some(macro_) {
                        Some(macro_)
                    } else {
                        None
                    };
                    let prior_defined = prior.as_ref().and_then(|p| p.defined);
                    let next = Definition {
                        defined: if defining { or(prior_defined, active) } else { and(prior_defined, not(active)) },
                        value: if defining && active == Some(true) { self.condition(value) } else { None },
                        macro_: now,
                    };
                    if next.defined != prior_defined
                        || next.value != prior.as_ref().and_then(|p| p.value)
                        || next.macro_ != prior.as_ref().and_then(|p| p.macro_)
                    {
                        self.version += 1;
                    }
                    self.definitions.insert(name.clone(), next);
                    if *function_like && !self.macro_names.contains(name) {
                        self.macro_names.insert(name.clone());
                        self.version += 1;
                    }
                    if self.macro_names.contains(name) {
                        self.timeline.entry(name.clone()).or_default().push(Event { line, defined: now });
                    }
                }
                FileEvent::Branch { .. } => {}
            }
        }
        self.scanning.remove(file);
        let own: Vec<String> = if self.cuts.len() > first_cut {
            let drained: Vec<String> = self.cuts.drain(first_cut..).collect();
            let mut seen = HashSet::new();
            drained.into_iter().filter(|c| seen.insert(c.clone())).collect()
        } else {
            Vec::new()
        };
        self.cuts.extend(own.iter().cloned());
        self.visits.insert(file.to_string(), Visit { inherited, start, end: self.version, cuts: own });
    }

    fn condition_of(&self, expression: &str) -> Truth {
        self.condition(expression)
    }
}

fn walk_translation_unit(root_file: &str, language: Language, ctx: &Ctx, cache: &CacheCell) -> Timeline {
    let mut walker = Walker {
        ctx,
        cache,
        language,
        timeline: HashMap::new(),
        definitions: HashMap::new(),
        scanning: HashSet::new(),
        macro_names: HashSet::new(),
        once: HashMap::new(),
        version: 0,
        cuts: Vec::new(),
        visits: HashMap::new(),
        budget: WALK_EVENT_BUDGET,
        cutoff: usize::MAX,
    };
    walker.scan(root_file, Some(true), None);
    Timeline { events: walker.timeline, cutoff: walker.cutoff }
}
