//! What an object-like macro renames a C or C++ name to — port of
//! `src/resolution/c-macro-renames.ts`.
//!
//! hiredis's `sdscompat.h` has `#define sds hisds`, so a `sds` member in `redis-cli.c`,
//! which includes it, is a `hisds`, whatever a namesake in `src/sds.h` says. The
//! preprocessor sees this and the parser does not. A macro with no parameters gets no
//! node, so only the source says what one does.
//!
//! Headers are read once per resolver, in one pass over their `#define`s, into an index by
//! macro name: a name no header renames costs one lookup, however many files write it. So are
//! the source files something includes (`#include "impl.c"`), which reach their includers as a
//! header does. A vendor's register maps can run to hundreds of megabytes, so the pass stops at
//! a budget, past which a name is left as written. Any other source file reaches itself alone,
//! so it is read when a name in it is asked about, for its own answers: the index is fixed once
//! built, and no answer depends on which files have asked (upstream 329dd12). A rename counts
//! where the translation unit from the asking file has certainly defined it
//! ([`is_cpp_macro_defined_at`]): a macro under a flag no file sets, or written further down,
//! renames nothing. Two renames of one name that disagree (`#ifdef` arms) say nothing either.

use crate::cpp_includers::{cpp_included_sources, cpp_includers};
use crate::cpp_macro_visibility::{directive_lines, is_cpp_macro_defined_at};
use crate::ctx::Ctx;
use codegraph_jsre::re;
use codegraph_types::Language;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
struct Memo {
    /// Whether the one-time header scan has run.
    initialized: bool,
    /// Macro name -> the headers and included source files with a `#define` of it to one name.
    by_name: HashMap<String, Vec<String>>,
    /// A source file asked about -> the names it renames itself.
    own: HashMap<String, std::rc::Rc<HashSet<String>>>,
    /// `file\0name` -> what the name is renamed to there (None: nothing).
    answers: HashMap<String, Option<String>>,
}

#[derive(Default)]
struct MemoCell(RefCell<Memo>);

/// Characters of the index one resolver reads for renames: redis has 2 MB of headers, rocksdb
/// 9 MB, betaflight 109 MB.
const HEADER_SCAN_BUDGET: usize = 16_000_000;

fn is_header(file: &str) -> bool {
    re!(r"\.(?:h|hh|hpp|hxx)$", "i").is_match(file)
}

/// Add the renames `source` writes to the index, by a search for `define`: most of a file
/// is no directive.
fn index(file: &str, source: &str, by_name: &mut HashMap<String, Vec<String>>) {
    let bytes = source.as_bytes();
    let mut search = 0;
    while let Some(offset) = source[search..].find("define") {
        let at = search + offset;
        search = at + 6;
        let next = bytes.get(at + 6).copied();
        if next != Some(b' ') && next != Some(b'\t') {
            continue; // `defined(X)`
        }
        let line_start = source[..at].rfind('\n').map_or(0, |i| i + 1);
        let line_end = source[at..].find('\n').map_or(source.len(), |i| at + i);
        let Some(captures) = re!(r"^[ \t]*#[ \t]*define[ \t]+([A-Za-z_]\w*)[ \t]+([A-Za-z_]\w*)[ \t]*(?:\/[/*].*)?\r?$")
            .captures(&source[line_start..line_end])
        else {
            continue;
        };
        let Some(name) = captures.text(1) else { continue };
        let files = by_name.entry(name.to_string()).or_default();
        if files.last().map(String::as_str) != Some(file) {
            files.push(file.to_string());
        }
    }
}

/// What the source file `file` renames itself.
fn own_renames(file: &str, memo: &MemoCell, ctx: &Ctx) -> std::rc::Rc<HashSet<String>> {
    if let Some(hit) = memo.0.borrow().own.get(file) {
        return hit.clone();
    }
    let mut names = HashMap::new();
    index(file, &ctx.read_file(file).unwrap_or_default(), &mut names);
    let own = std::rc::Rc::new(names.into_keys().collect::<HashSet<String>>());
    memo.0.borrow_mut().own.insert(file.to_string(), own.clone());
    own
}

fn memo_for(ctx: &Ctx) -> std::rc::Rc<MemoCell> {
    let memo = ctx.memo::<MemoCell>();
    if memo.0.borrow().initialized {
        return memo;
    }
    let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
    let mut budget = HEADER_SCAN_BUDGET;
    let mut read = |file: &str, budget: &mut usize| {
        if let Some(source) = ctx.read_file(file) {
            index(file, &source, &mut by_name);
            *budget = budget.saturating_sub(source.len());
        }
    };
    for file in ctx.get_all_files() {
        if budget == 0 {
            break;
        }
        if is_header(&file) {
            read(&file, &mut budget);
        }
    }
    for file in cpp_included_sources(ctx) {
        if budget == 0 {
            break;
        }
        if !is_header(&file) {
            read(&file, &mut budget);
        }
    }
    {
        let mut state = memo.0.borrow_mut();
        if !state.initialized {
            state.by_name = by_name;
            state.initialized = true;
        }
    }
    memo
}

/// The one name `name` is renamed to at `line` of `file`, by the file or by what it
/// includes.
pub fn c_macro_rename(name: &str, file: &str, line: usize, language: Language, ctx: &Ctx) -> Option<String> {
    let memo = memo_for(ctx);
    let indexed = memo.0.borrow().by_name.get(name).cloned().unwrap_or_default();
    let own = !is_header(file) && own_renames(file, &memo, ctx).contains(name);
    if !own && indexed.is_empty() {
        return None;
    }
    let files = if own && !indexed.iter().any(|f| f == file) {
        std::iter::once(file.to_string()).chain(indexed).collect()
    } else {
        indexed
    };
    let key = format!("{file}\0{name}");
    let cached = memo.0.borrow().answers.get(&key).cloned();
    let renamed = match cached {
        Some(answer) => answer,
        None => {
            let mut renames: HashSet<String> = HashSet::new();
            for candidate in &files {
                if candidate != file && !cpp_includers(candidate, ctx).contains(file) {
                    continue;
                }
                let source = ctx.read_file(candidate).unwrap_or_default();
                for directive in directive_lines(&source) {
                    let Some(captures) = re!(r"^[ \t]*#[ \t]*define[ \t]+([A-Za-z_]\w*)[ \t]+([A-Za-z_]\w*)[ \t]*(?:\/[/*].*)?\r?$")
                        .captures(&directive)
                    else {
                        continue;
                    };
                    if captures.text(1) == Some(name) {
                        if let Some(to) = captures.text(2) {
                            renames.insert(to.to_string());
                        }
                    }
                }
            }
            let answer = if renames.len() == 1 { renames.into_iter().next() } else { None };
            memo.0.borrow_mut().answers.insert(key, answer.clone());
            answer
        }
    };
    let renamed = renamed?;
    is_cpp_macro_defined_at(name, file, line, language, ctx).then_some(renamed)
}
