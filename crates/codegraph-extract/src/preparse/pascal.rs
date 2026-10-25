//! preParsePascal (languages/pascal-preparse.ts) — offset-preserving fixes the Pascal grammar
//! needs before it parses a Delphi unit: parameter attributes, `for var … in` loop variables,
//! property overrides, conditionals after a routine header, duplicate `begin`s across
//! conditional branches, anonymous record types, re-raises, labels that end a block, and
//! alternative statements where the grammar takes one. See the TS file for each one's why.
//!
//! The TS scanner indexes the source by UTF-16 code unit (`charCodeAt`), and its blanking writes
//! one space per unit, so this port scans a `Vec<u16>` of the source: offsets, length limits and
//! blank widths stay exactly the TS ones.

use codegraph_types::jscompat::JS_SPACE_CLASS;
use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

const MODIFIERS: [&str; 4] = ["const", "var", "out", "constref"];
const ROUTINE_KEYWORDS: [&str; 5] = ["procedure", "function", "constructor", "destructor", "operator"];
const MAX_ATTRIBUTE_LENGTH: usize = 2000;
const MAX_LOOP_TYPE_LENGTH: usize = 500;
const NOT_A_FOR_IN: [&str; 5] = ["do", "to", "downto", "begin", "end"];
const MAX_OVERRIDE_LENGTH: usize = 500;
const MAX_HEADER_PART_LENGTH: usize = 500;
const MAX_RECORD_LENGTH: usize = 20000;
const ROUTINE_HINTS: [&str; 45] = [
    "static", "virtual", "dynamic", "abstract", "override", "overload", "reintroduce", "inline",
    "stdcall", "cdecl", "pascal", "register", "safecall", "assembler", "noreturn", "local", "far",
    "near", "default", "nodefault", "deprecated", "experimental", "message", "platform",
    "unimplemented", "cppdecl", "cvar", "mwpascal", "nostackframe", "interrupt", "iocheck",
    "hardfloat", "softfloat", "ms_abi_default", "ms_abi_cdecl", "saveregisters", "sysv_abi_default",
    "sysv_abi_cdecl", "vectorcall", "varargs", "winapi", "public", "export", "alias", "dispid",
];
const LABEL_ENDS: [&str; 4] = ["end", "until", "except", "finally"];
const BLOCK_OPENERS: [&str; 5] = ["begin", "asm", "try", "case", "record"];
const NOT_IN_RECORD_FIELDS: [&str; 18] = [
    "begin", "class", "object", "interface", "implementation", "procedure", "function", "constructor",
    "destructor", "property", "type", "var", "const", "private", "protected", "public", "published", "strict",
];
const STATEMENT_KEYWORDS: [&str; 12] = [
    "begin", "if", "while", "for", "repeat", "case", "try", "with", "raise", "inherited", "goto", "asm",
];
const TYPE_KEYWORDS: [&str; 16] = [
    "procedure", "function", "reference", "class", "record", "object", "interface", "array", "set", "file",
    "packed", "string", "type", "const", "var", "out",
];
const STATEMENT_CONTINUES: [&str; 20] = [
    "then", "do", "else", "of", "and", "or", "xor", "not", "div", "mod", "shl", "shr", "in", "is", "as", "to",
    "downto", "until", "goto", "on",
];

const CH_TAB: u32 = 9;
const CH_LF: u32 = 10;
const CH_CR: u32 = 13;
const CH_SPACE: u32 = 32;
const CH_DOLLAR: u32 = 36;
const CH_AMP: u32 = 38;
const CH_QUOTE: u32 = 39;
const CH_LPAREN: u32 = 40;
const CH_RPAREN: u32 = 41;
const CH_STAR: u32 = 42;
const CH_COMMA: u32 = 44;
const CH_DOT: u32 = 46;
const CH_SLASH: u32 = 47;
const CH_COLON: u32 = 58;
const CH_SEMI: u32 = 59;
const CH_LT: u32 = 60;
const CH_EQ: u32 = 61;
const CH_GT: u32 = 62;
const CH_AT: u32 = 64;
const CH_LBRACKET: u32 = 91;
const CH_RBRACKET: u32 = 93;
const CH_LBRACE: u32 = 123;
const CH_RBRACE: u32 = 125;

/// `MAY_NEED_FIXES`: whether a file can hold anything to fix at all.
fn may_need_fixes() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let s = JS_SPACE_CLASS;
        let b = r"(?-u:\b)";
        Regex::new(&format!(
            r"(?i)\{{\$|\[|{b}for[{s}]+var{b}|{b}property{b}|{b}record{b}|{b}raise{b}|{b}goto{b}"
        ))
        .unwrap()
    })
}

/// The source as UTF-16 units, read the way `charCodeAt` reads it: out of range is NaN, which
/// every comparison rejects — here 0, which no test accepts either.
struct Src {
    u: Vec<u16>,
}

impl Src {
    #[inline]
    fn len(&self) -> usize {
        self.u.len()
    }
    #[inline]
    fn at(&self, i: usize) -> u32 {
        self.u.get(i).map_or(0, |&c| c as u32)
    }
    /// `charCodeAt(i - 1)`, NaN (0) at the start.
    #[inline]
    fn before(&self, i: usize) -> u32 {
        if i == 0 { 0 } else { self.at(i - 1) }
    }
    /// `s.slice(from, to).toLowerCase()`.
    fn lower(&self, from: usize, to: usize) -> String {
        let to = to.min(self.len());
        if from >= to {
            return String::new();
        }
        String::from_utf16_lossy(&self.u[from..to]).to_lowercase()
    }
    /// `s.indexOf(needle, from)` for an ASCII needle.
    fn index_of(&self, needle: &[u8], from: usize) -> Option<usize> {
        if needle.is_empty() || self.len() < needle.len() || from > self.len() {
            return None;
        }
        let n = needle.len();
        (from..=self.len().saturating_sub(n)).find(|&i| needle.iter().enumerate().all(|(k, &b)| self.u[i + k] as u32 == b as u32))
    }
    /// `s.lastIndexOf(needle, from)` for an ASCII needle; a negative `from` searches from 0, as in JS.
    fn last_index_of(&self, needle: &[u8], from: isize) -> Option<usize> {
        if needle.is_empty() || self.len() < needle.len() {
            return None;
        }
        let n = needle.len();
        let start = (from.max(0) as usize).min(self.len() - n);
        (0..=start).rev().find(|&i| needle.iter().enumerate().all(|(k, &b)| self.u[i + k] as u32 == b as u32))
    }
}

fn is_ident_start(c: u32) -> bool {
    (65..=90).contains(&c) || (97..=122).contains(&c) || c == 95 || c == CH_AMP || c >= 128
}
fn is_ident_part(c: u32) -> bool {
    (65..=90).contains(&c) || (97..=122).contains(&c) || c == 95 || (48..=57).contains(&c) || c >= 128
}
fn is_space(c: u32) -> bool {
    c == CH_SPACE || c == CH_TAB || c == CH_LF || c == CH_CR
}
fn is_digit(c: u32) -> bool {
    (48..=57).contains(&c)
}

fn word_end(s: &Src, mut i: usize) -> usize {
    while i < s.len() && is_ident_part(s.at(i)) {
        i += 1;
    }
    i
}

/// The index after the comment or string that starts at `i`, or None when none does.
fn comment_or_string_end(s: &Src, i: usize) -> Option<usize> {
    let c = s.at(i);
    if c == CH_LBRACE {
        return Some(s.index_of(b"}", i + 1).map_or(s.len(), |e| e + 1));
    }
    if c == CH_LPAREN && s.at(i + 1) == CH_STAR {
        return Some(s.index_of(b"*)", i + 2).map_or(s.len(), |e| e + 2));
    }
    if c == CH_SLASH && s.at(i + 1) == CH_SLASH {
        return Some(s.index_of(b"\n", i + 2).unwrap_or(s.len()));
    }
    if c == CH_QUOTE {
        let mut j = i + 1;
        while j < s.len() {
            let d = s.at(j);
            if d == CH_QUOTE {
                if s.at(j + 1) != CH_QUOTE {
                    return Some(j + 1);
                }
                j += 2;
                continue;
            }
            if d == CH_LF {
                return Some(j);
            }
            j += 1;
        }
        return Some(j);
    }
    None
}

fn is_comment_start(s: &Src, i: usize) -> bool {
    let c = s.at(i);
    c == CH_LBRACE || (c == CH_LPAREN && s.at(i + 1) == CH_STAR) || (c == CH_SLASH && s.at(i + 1) == CH_SLASH)
}

fn skip_trivia(s: &Src, mut i: usize) -> usize {
    while i < s.len() {
        if is_space(s.at(i)) {
            i += 1;
        } else if is_comment_start(s, i) {
            i = comment_or_string_end(s, i).unwrap_or(i + 1);
        } else {
            break;
        }
    }
    i
}

/// The index after the last character before `i` that is neither whitespace nor in a `{…}` or
/// `(*…*)` comment.
fn token_end_before(s: &Src, mut i: usize) -> usize {
    loop {
        while i > 0 && is_space(s.at(i - 1)) {
            i -= 1;
        }
        if i > 0 && s.at(i - 1) == CH_RBRACE {
            match s.last_index_of(b"{", i as isize - 2) {
                Some(open) => i = open,
                None => return i,
            }
        } else if i > 1 && s.at(i - 1) == CH_RPAREN && s.at(i - 2) == CH_STAR {
            match s.last_index_of(b"(*", i as isize - 3) {
                Some(open) => i = open,
                None => return i,
            }
        } else {
            return i;
        }
    }
}

/// The start of the identifier that ends at `i`, or None.
fn word_start_before(s: &Src, i: usize) -> Option<usize> {
    let mut k = i;
    while k > 0 && is_ident_part(s.at(k - 1)) {
        k -= 1;
    }
    if k == i {
        return None;
    }
    if k > 0 && s.at(k - 1) == CH_AMP {
        k -= 1;
    }
    is_ident_start(s.at(k)).then_some(k)
}

/// The index after the first `;` from `i` outside parentheses and brackets, within `max`.
fn semicolon_end(s: &Src, mut i: usize, max: usize) -> Option<usize> {
    let limit = s.len().min(i + max);
    let mut depth: i64 = 0;
    while i < limit {
        if let Some(k) = comment_or_string_end(s, i) {
            i = k;
            continue;
        }
        let c = s.at(i);
        if c == CH_LPAREN || c == CH_LBRACKET {
            depth += 1;
        } else if c == CH_RPAREN || c == CH_RBRACKET {
            depth -= 1;
        } else if c == CH_SEMI && depth <= 0 {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

/// The index after the `]` closing the attribute group whose `[` is at `i`.
fn attribute_end(s: &Src, i: usize) -> Option<usize> {
    let limit = s.len().min(i + MAX_ATTRIBUTE_LENGTH);
    let mut depth: i64 = 0;
    let mut j = i;
    while j < limit {
        if let Some(k) = comment_or_string_end(s, j) {
            j = k;
            continue;
        }
        let c = s.at(j);
        if c == CH_LBRACKET || c == CH_LPAREN {
            depth += 1;
        } else if c == CH_RBRACKET || c == CH_RPAREN {
            depth -= 1;
            if depth == 0 {
                return (c == CH_RBRACKET).then_some(j + 1);
            }
        } else if c == CH_SEMI {
            return None;
        }
        j += 1;
    }
    None
}

/// The index after a routine's name (`Name`, `TFoo.Bar`, `TFoo<T>.Bar<U>`) read from `i`.
fn routine_name_end(s: &Src, mut i: usize) -> usize {
    loop {
        i = skip_trivia(s, i);
        if !is_ident_start(s.at(i)) {
            return i;
        }
        i = word_end(s, i + 1);
        let mut j = skip_trivia(s, i);
        if s.at(j) == CH_LT {
            let mut depth: i64 = 0;
            while j < s.len() {
                let c = s.at(j);
                if c == CH_LT {
                    depth += 1;
                } else if c == CH_GT {
                    depth -= 1;
                    if depth == 0 {
                        j += 1;
                        break;
                    }
                } else if c == CH_SEMI || c == CH_LPAREN || c == CH_RPAREN || c == CH_LF {
                    return i;
                }
                j += 1;
            }
            i = j;
            j = skip_trivia(s, i);
        }
        if s.at(j) != CH_DOT {
            return i;
        }
        i = j + 1;
    }
}

/// Walk the parameter list whose `(` is at `open`, pushing each parameter's attribute groups.
fn scan_parameter_list(s: &Src, open: usize, ranges: &mut Vec<usize>) -> usize {
    let mut depth: i64 = 0;
    let mut i = open;
    let mut at_parameter_start = false;
    while i < s.len() {
        if at_parameter_start {
            at_parameter_start = false;
            loop {
                i = skip_trivia(s, i);
                let c = s.at(i);
                if c == CH_LBRACKET {
                    let Some(end) = attribute_end(s, i) else { break };
                    ranges.push(i);
                    ranges.push(end);
                    i = end;
                } else if is_ident_start(c) {
                    let e = word_end(s, i + 1);
                    if !MODIFIERS.contains(&s.lower(i, e).as_str()) {
                        break;
                    }
                    i = e;
                } else {
                    break;
                }
            }
            continue;
        }
        if let Some(k) = comment_or_string_end(s, i) {
            i = k;
            continue;
        }
        let c = s.at(i);
        if c == CH_LPAREN || c == CH_LBRACKET {
            depth += 1;
            if depth == 1 {
                at_parameter_start = true;
            }
        } else if c == CH_RPAREN || c == CH_RBRACKET {
            depth -= 1;
            if depth <= 0 {
                return i + 1;
            }
        } else if c == CH_SEMI && depth == 1 {
            at_parameter_start = true;
        }
        i += 1;
    }
    i
}

fn lower_word_at(s: &Src, i: usize) -> String {
    if is_ident_start(s.at(i)) { s.lower(i, word_end(s, i + 1)) } else { String::new() }
}

/// After a `for` ending at `for_end`, blank `var` and `: Type` of `for var Name[: Type] in`.
fn scan_for_in_variable(s: &Src, for_end: usize, ranges: &mut Vec<usize>) -> usize {
    let var_start = skip_trivia(s, for_end);
    if lower_word_at(s, var_start) != "var" {
        return for_end;
    }
    let var_end = var_start + 3;
    let name_start = skip_trivia(s, var_end);
    if !is_ident_start(s.at(name_start)) {
        return for_end;
    }
    let mut i = skip_trivia(s, word_end(s, name_start + 1));
    let mut type_start = None;
    if s.at(i) == CH_COLON && s.at(i + 1) != CH_EQ {
        type_start = Some(i);
        let limit = s.len().min(i + MAX_LOOP_TYPE_LENGTH);
        let mut depth: i64 = 0;
        i += 1;
        loop {
            if i >= limit {
                return for_end;
            }
            if let Some(k) = comment_or_string_end(s, i) {
                i = k;
                continue;
            }
            let c = s.at(i);
            if c == CH_LT || c == CH_LPAREN || c == CH_LBRACKET {
                depth += 1;
            } else if c == CH_GT || c == CH_RPAREN || c == CH_RBRACKET {
                depth -= 1;
                if depth < 0 {
                    return for_end;
                }
            } else if c == CH_SEMI || c == CH_EQ {
                return for_end;
            } else if is_ident_start(c) {
                let word = lower_word_at(s, i);
                if depth == 0 && word == "in" {
                    break;
                }
                if NOT_A_FOR_IN.contains(&word.as_str()) {
                    return for_end;
                }
                i = word_end(s, i + 1);
                continue;
            }
            i += 1;
        }
    }
    if lower_word_at(s, i) != "in" {
        return for_end;
    }
    ranges.push(var_start);
    ranges.push(var_end);
    if let Some(t) = type_start {
        ranges.push(t);
        ranges.push(i);
    }
    i
}

/// After a `property` keyword, blank a property override (`property OwnsObjects;`).
fn scan_property_override(s: &Src, start: usize, property_end: usize, ranges: &mut Vec<usize>) -> usize {
    let name_start = skip_trivia(s, property_end);
    if !is_ident_start(s.at(name_start)) {
        return property_end;
    }
    let i = skip_trivia(s, word_end(s, name_start + 1));
    let next = s.at(i);
    if next == CH_COLON || next == CH_LBRACKET {
        return property_end;
    }
    let Some(end) = semicolon_end(s, i, MAX_OVERRIDE_LENGTH) else { return property_end };
    let class_start = class_before(s, start);
    ranges.push(class_start.unwrap_or(start));
    ranges.push(end);
    end
}

/// A conditional directive group: `bounds` holds each directive's `[start, end)`, `nested[b]`
/// whether another group sits in branch `b`.
struct ConditionalGroup {
    bounds: Vec<usize>,
    nested: Vec<bool>,
}

#[derive(PartialEq)]
enum Cond {
    None,
    Open,
    Else,
    Close,
}

fn conditional_kind(s: &Src, i: usize) -> Cond {
    let mut j = i + 2;
    while j < s.len() {
        let c = s.at(j) | 32;
        if !(97..=122).contains(&c) {
            break;
        }
        j += 1;
    }
    match s.lower(i + 2, j).as_str() {
        "if" | "ifdef" | "ifndef" | "ifopt" => Cond::Open,
        "else" | "elseif" => Cond::Else,
        "endif" | "ifend" => Cond::Close,
        _ => Cond::None,
    }
}

/// Every closed conditional group, innermost first.
fn conditional_groups(s: &Src) -> Vec<ConditionalGroup> {
    let mut groups = Vec::new();
    let mut open: Vec<ConditionalGroup> = Vec::new();
    let n = s.len();
    let mut i = 0;
    while i < n {
        let c = s.at(i);
        let k = if c == CH_LBRACE || c == CH_QUOTE || c == CH_LPAREN || c == CH_SLASH {
            comment_or_string_end(s, i)
        } else {
            None
        };
        let Some(k) = k else {
            i += 1;
            continue;
        };
        if c == CH_LBRACE && s.at(i + 1) == CH_DOLLAR {
            let kind = conditional_kind(s, i);
            if kind == Cond::Open {
                if let Some(top) = open.last_mut() {
                    *top.nested.last_mut().unwrap() = true;
                }
                open.push(ConditionalGroup { bounds: vec![i, k], nested: vec![false] });
            } else if kind == Cond::Else {
                if let Some(top) = open.last_mut() {
                    top.bounds.push(i);
                    top.bounds.push(k);
                    top.nested.push(false);
                }
            } else if kind == Cond::Close {
                if let Some(mut top) = open.pop() {
                    top.bounds.push(i);
                    top.bounds.push(k);
                    groups.push(top);
                }
            }
        }
        i = k;
    }
    groups
}

/// The text of the unit at `i` as a one-unit string (`s[i]`).
fn unit_str(s: &Src, i: usize) -> String {
    String::from_utf16_lossy(&s.u[i..i + 1])
}

/// Whether the code from `from` to `to` is a routine's locals and body.
fn holds_body(s: &Src, from: usize, to: usize) -> bool {
    let mut blocks: Vec<String> = Vec::new();
    let mut pending: i64 = 0;
    let mut own = false;
    let mut last = String::new();
    let mut before_last = String::new();
    let mut i = from;
    while i < to {
        if let Some(k) = comment_or_string_end(s, i) {
            if s.at(i) == CH_QUOTE {
                before_last = std::mem::replace(&mut last, "'".to_string());
            }
            i = k;
            continue;
        }
        let c = s.at(i);
        if is_ident_start(c) {
            let e = word_end(s, i + 1);
            let word = if e - i <= 11 { s.lower(i, e) } else { "word".to_string() };
            if blocks.is_empty() {
                if ROUTINE_KEYWORDS.contains(&word.as_str()) && (last.is_empty() || last == ";") {
                    pending += 1;
                } else if (word == "forward" || word == "external") && pending > 0 {
                    pending -= 1;
                }
            }
            if word == "end" {
                if blocks.pop().is_none() {
                    return true;
                }
            } else if word == "begin" || word == "asm" {
                if blocks.is_empty() {
                    own = pending == 0;
                    if !own {
                        pending -= 1;
                    }
                }
                blocks.push(word.clone());
            } else if word == "try" || word == "record" || (word == "case" && blocks.last().map(String::as_str) != Some("record")) {
                blocks.push(word.clone());
            }
            before_last = std::mem::replace(&mut last, word);
            i = e;
            continue;
        }
        if !is_space(c) {
            before_last = std::mem::replace(&mut last, unit_str(s, i));
        }
        i += 1;
    }
    blocks.is_empty() && own && before_last == "end" && last == ";"
}

/// Whether the code from `from` to `to` holds a `begin` or an `asm`.
fn holds_block(s: &Src, from: usize, to: usize) -> bool {
    let mut i = from;
    while i < to {
        if let Some(k) = comment_or_string_end(s, i) {
            i = k;
            continue;
        }
        if is_ident_start(s.at(i)) {
            let e = word_end(s, i + 1);
            if e - i == 3 || e - i == 5 {
                let word = s.lower(i, e);
                if word == "begin" || word == "asm" {
                    return true;
                }
            }
            i = e;
            continue;
        }
        i += 1;
    }
    false
}

fn skip_trivia_to_group(s: &Src, mut i: usize, group_at: &HashMap<usize, usize>) -> usize {
    while i < s.len() {
        if is_space(s.at(i)) {
            i += 1;
        } else if is_comment_start(s, i) && !group_at.contains_key(&i) {
            i = comment_or_string_end(s, i).unwrap_or(i + 1);
        } else {
            break;
        }
    }
    i
}

/// From the index after a routine header's `;`, blank the directives of each conditional that
/// follows the header unless every branch holds a whole body.
fn blank_conditionals_after_header(
    s: &Src,
    mut i: usize,
    groups: &[ConditionalGroup],
    group_at: &HashMap<usize, usize>,
    ranges: &mut Vec<usize>,
) {
    loop {
        i = skip_trivia_to_group(s, i, group_at);
        if let Some(&g) = group_at.get(&i) {
            let bounds = &groups[g].bounds;
            let mut bodies = true;
            let mut b = 1;
            while b < bounds.len() - 1 && bodies {
                bodies = holds_body(s, bounds[b], bounds[b + 1]);
                b += 2;
            }
            if bodies {
                return;
            }
            ranges.extend_from_slice(bounds);
            i = if holds_block(s, bounds[1], bounds[bounds.len() - 2]) { bounds[bounds.len() - 1] } else { bounds[1] };
            continue;
        }
        if !ROUTINE_HINTS.contains(&lower_word_at(s, i).as_str()) {
            return;
        }
        match semicolon_end(s, i, MAX_HEADER_PART_LENGTH) {
            Some(e) => i = e,
            None => return,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Open {
    Begin,
    Asm,
    Other,
}

/// The blocks the code from `from` to `to` leaves open, or None when it closes one it didn't open.
fn open_blocks(s: &Src, from: usize, to: usize) -> Option<Vec<(usize, Open)>> {
    let mut open = Vec::new();
    let mut i = from;
    while i < to {
        if let Some(k) = comment_or_string_end(s, i) {
            i = k;
            continue;
        }
        if is_ident_start(s.at(i)) {
            let e = word_end(s, i + 1);
            if (3..=6).contains(&(e - i)) {
                let word = s.lower(i, e);
                if word == "end" {
                    open.pop()?;
                } else if BLOCK_OPENERS.contains(&word.as_str()) {
                    let kind = match word.as_str() {
                        "begin" => Open::Begin,
                        "asm" => Open::Asm,
                        _ => Open::Other,
                    };
                    open.push((i, kind));
                }
            }
            i = e;
            continue;
        }
        i += 1;
    }
    Some(open)
}

/// Blank the `begin`s a conditional's later branches open again (see the TS doc comment).
fn blank_duplicate_begins(s: &Src, groups: &[ConditionalGroup], ranges: &mut Vec<usize>) {
    for g in groups {
        let (bounds, nested) = (&g.bounds, &g.nested);
        if nested.len() < 2 || nested.contains(&true) {
            continue;
        }
        let mut open: Vec<Vec<(usize, Open)>> = Vec::new();
        for b in 0..nested.len() {
            match open_blocks(s, bounds[2 * b + 1], bounds[2 * b + 2]) {
                Some(blocks) if !blocks.is_empty() => open.push(blocks),
                _ => break,
            }
        }
        if open.len() < nested.len() {
            continue;
        }
        let kinds = |kind: Open| open.iter().filter(|o| o.iter().all(|(_, k)| *k == kind)).count();
        if kinds(Open::Begin) == open.len() && open.iter().all(|o| o.len() == open[0].len()) {
            for o in &open[1..] {
                for (at, _) in o {
                    ranges.push(*at);
                    ranges.push(at + 5);
                }
            }
        } else if open.iter().all(|o| o.len() == 1)
            && kinds(Open::Asm) > 0
            && kinds(Open::Begin) > 0
            && kinds(Open::Begin) + kinds(Open::Asm) == open.len()
        {
            let mut kept = false;
            for (b, o) in open.iter().enumerate() {
                let (at, kind) = o[0];
                if kind == Open::Asm {
                    ranges.push(bounds[2 * b + 1]);
                    ranges.push(bounds[2 * b + 2]);
                } else if kept {
                    ranges.push(at);
                    ranges.push(at + 5);
                } else {
                    kept = true;
                }
            }
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Shape {
    None,
    Statements,
    StatementsSemi,
    Empty,
}

#[derive(PartialEq, Clone, Copy)]
enum Start {
    None,
    Statement,
    Call,
    Name,
}

/// Whether a conditional's branch holds whole statements (see the TS doc comment).
fn statements_shape(s: &Src, from: usize, to: usize, bare: bool) -> Shape {
    let mut blocks: Vec<bool> = Vec::new();
    let mut depth: i64 = 0;
    let mut heads: i64 = 0;
    let mut tokens = 0usize;
    let mut start = Start::None;
    let mut name = 0u8;
    let mut last = String::new();
    let mut i = from;
    while i < to {
        if let Some(k) = comment_or_string_end(s, i) {
            if s.at(i) == CH_QUOTE {
                tokens += 1;
                name = 0;
                last = "'".to_string();
            }
            i = k;
            continue;
        }
        let c = s.at(i);
        if is_space(c) {
            i += 1;
            continue;
        }
        tokens += 1;
        if is_ident_start(c) {
            let e = word_end(s, i + 1);
            let word = if e - i <= 9 { s.lower(i, e) } else { "word".to_string() };
            match word.as_str() {
                "begin" | "try" | "case" | "asm" => blocks.push(false),
                "repeat" => blocks.push(true),
                "end" => {
                    if blocks.pop() != Some(false) {
                        return Shape::None;
                    }
                }
                "until" => {
                    if blocks.pop() != Some(true) {
                        return Shape::None;
                    }
                }
                "if" | "while" | "for" | "with" | "on" => heads += 1,
                "then" | "do" => heads -= 1,
                _ => {}
            }
            if tokens == 1 {
                if STATEMENT_KEYWORDS.contains(&word.as_str()) {
                    start = Start::Statement;
                } else if !TYPE_KEYWORDS.contains(&word.as_str())
                    && !STATEMENT_CONTINUES.contains(&word.as_str())
                    && word != "end"
                {
                    name = 1;
                }
            } else {
                name = if name == 2 { 1 } else { 0 };
            }
            last = word;
            i = e;
            continue;
        }
        if is_digit(c) {
            name = 0;
            last = "0".to_string();
            i = word_end(s, i + 1);
            continue;
        }
        if c == CH_COLON && s.at(i + 1) == CH_EQ {
            if depth == 0 && start != Start::Call {
                start = Start::Statement;
            }
            name = 0;
            last = ":=".to_string();
            i += 2;
            continue;
        }
        if c == CH_LPAREN || c == CH_LBRACKET {
            if c == CH_LPAREN && name == 1 && start == Start::None {
                start = Start::Call;
            }
            depth += 1;
        } else if c == CH_RPAREN || c == CH_RBRACKET {
            depth -= 1;
            if depth < 0 {
                return Shape::None;
            }
        } else if c == CH_SEMI && name == 1 && start == Start::None {
            start = Start::Name;
        }
        name = if c == CH_DOT && name == 1 { 2 } else { 0 };
        last = unit_str(s, i);
        i += 1;
    }
    if tokens == 0 {
        return Shape::Empty;
    }
    if depth != 0 || !blocks.is_empty() || heads != 0 {
        return Shape::None;
    }
    if name == 1 && start == Start::None {
        start = Start::Name;
    }
    if start == Start::None || (start == Start::Name && !bare) {
        return Shape::None;
    }
    let first = last.encode_utf16().next().map_or(0, u32::from);
    let ends = matches!(last.as_str(), ";" | ")" | "]" | "^" | "'" | "0")
        || (is_ident_start(first) && !STATEMENT_CONTINUES.contains(&last.as_str()));
    if !ends {
        return Shape::None;
    }
    if last == ";" { Shape::StatementsSemi } else { Shape::Statements }
}

fn overlaps_range(ranges: &[usize], from: usize, to: usize) -> bool {
    ranges.chunks(2).any(|r| r[0] < to && r[1] > from)
}

fn on_one_line(s: &Src, at: usize, length: usize) -> bool {
    (at..at + length).all(|i| {
        let c = s.at(i);
        c != CH_LF && c != CH_CR
    })
}

/// Read each conditional of alternative statements as one statement (see the TS doc comment).
fn wrap_alternative_statements(
    s: &Src,
    groups: &[ConditionalGroup],
    label: &HashMap<usize, bool>,
    ranges: &[usize],
    words: &mut Vec<(usize, usize, &'static str)>,
) {
    for g in groups {
        let (bounds, nested) = (&g.bounds, &g.nested);
        let Some(&after_label) = label.get(&bounds[0]) else { continue };
        if nested.len() < 2 || nested.contains(&true) {
            continue;
        }
        let mut filled = 0;
        let mut semi = false;
        let mut breaks = after_label;
        let mut ok = true;
        let mut b = 0;
        while b < nested.len() && ok {
            let shape = statements_shape(s, bounds[2 * b + 1], bounds[2 * b + 2], !after_label);
            b += 1;
            if shape == Shape::Empty {
                continue;
            }
            if shape == Shape::None {
                ok = false;
            }
            if filled > 0 && !semi {
                breaks = true;
            }
            filled += 1;
            semi = shape == Shape::StatementsSemi;
        }
        if !ok || filled < 2 || !breaks || !on_one_line(s, bounds[0], 5) {
            continue;
        }
        let mut clear = true;
        let mut d = 0;
        while d < bounds.len() && clear {
            clear = !overlaps_range(ranges, bounds[d], bounds[d + 1]);
            d += 2;
        }
        if !clear {
            continue;
        }
        words.push((bounds[0], bounds[1], "begin"));
        let mut d = 2;
        while d + 2 < bounds.len() {
            words.push((bounds[d], bounds[d + 1], ";"));
            d += 2;
        }
        words.push((bounds[bounds.len() - 2], bounds[bounds.len() - 1], if semi { "end;" } else { "end" }));
    }
}

/// 1 for a case label's `:`, 0 for `then` / `else` / `do`, -1 for anything else.
fn statement_position(s: &Src, at: usize, end: usize) -> i8 {
    let length = end - at;
    if length == 1 {
        return if s.at(at) == CH_COLON { 1 } else { -1 };
    }
    if !(2..=4).contains(&length) {
        return -1;
    }
    match s.lower(at, end).as_str() {
        "then" | "else" | "do" => 0,
        _ => -1,
    }
}

/// The start of the declaration the `record` at `i` gives an anonymous type to, or None.
fn anonymous_record_decl_start(s: &Src, i: usize) -> Option<usize> {
    let mut j = token_end_before(s, i);
    let mut w = word_start_before(s, j);
    if let Some(ws) = w.filter(|&ws| lower_word_at(s, ws) == "packed") {
        j = token_end_before(s, ws);
        w = word_start_before(s, j);
    }
    let mut array = false;
    if let Some(ws) = w.filter(|&ws| lower_word_at(s, ws) == "of") {
        j = token_end_before(s, ws);
        if j > 0 && s.at(j - 1) == CH_RBRACKET {
            let open = s.last_index_of(b"[", j as isize - 1)?;
            j = token_end_before(s, open);
        }
        w = word_start_before(s, j);
        let ws = w.filter(|&ws| lower_word_at(s, ws) == "array")?;
        j = token_end_before(s, ws);
        w = word_start_before(s, j);
        if let Some(ws) = w.filter(|&ws| lower_word_at(s, ws) == "packed") {
            j = token_end_before(s, ws);
        }
        array = true;
    }
    let c = s.before(j);
    if c != CH_COLON && !(c == CH_EQ && array) {
        return None;
    }
    let mut start = word_start_before(s, token_end_before(s, j - 1))?;
    if c == CH_EQ {
        return Some(start);
    }
    loop {
        let k = token_end_before(s, start);
        if s.before(k) != CH_COMMA {
            return Some(start);
        }
        start = word_start_before(s, token_end_before(s, k - 1))?;
    }
}

/// The index after the `end` closing a record whose fields start at `i`.
fn record_end(s: &Src, mut i: usize) -> Option<usize> {
    let limit = s.len().min(i + MAX_RECORD_LENGTH);
    let mut depth = 1i64;
    let mut brackets = 0i64;
    let mut prev = String::new();
    while i < limit {
        if let Some(k) = comment_or_string_end(s, i) {
            if s.at(i) == CH_QUOTE {
                prev = "'".to_string();
            }
            i = k;
            continue;
        }
        let c = s.at(i);
        if is_ident_start(c) {
            let e = word_end(s, i + 1);
            let word = s.lower(i, e);
            if brackets == 0 {
                if word == "end" {
                    depth -= 1;
                    if depth == 0 {
                        return Some(e);
                    }
                } else if word == "record" {
                    depth += 1;
                } else if NOT_IN_RECORD_FIELDS.contains(&word.as_str()) && prev != ":" && prev != "of" && prev != "to" {
                    return None;
                }
            }
            prev = word;
            i = e;
            continue;
        }
        if c == CH_LPAREN || c == CH_LBRACKET {
            brackets += 1;
        } else if c == CH_RPAREN || c == CH_RBRACKET {
            brackets -= 1;
        } else if c == CH_EQ && brackets == 0 {
            return None;
        }
        if !is_space(c) {
            prev = unit_str(s, i);
        }
        i += 1;
    }
    None
}

/// At a `record` keyword, write a field's anonymous record type over with `string`.
fn scan_anonymous_record(
    s: &Src,
    start: usize,
    end: usize,
    ranges: &mut Vec<usize>,
    words: &mut Vec<(usize, usize, &'static str)>,
) -> usize {
    if anonymous_record_decl_start(s, start).is_none() {
        return end;
    }
    let first = lower_word_at(s, skip_trivia(s, end));
    if first.is_empty() || NOT_IN_RECORD_FIELDS.contains(&first.as_str()) {
        return end;
    }
    let Some(close) = record_end(s, end) else { return end };
    let next = skip_trivia(s, close);
    let c = s.at(next);
    let last = lower_word_at(s, next) == "end";
    if c != CH_SEMI && c != CH_EQ && c != CH_RPAREN && !last {
        return end;
    }
    let packed = word_start_before(s, token_end_before(s, start));
    let at = match packed {
        Some(p) if lower_word_at(s, p) == "packed" => p,
        _ => start,
    };
    words.push((at, at + 6, "string"));
    if last {
        ranges.push(at + 6);
        ranges.push(close - 3);
        words.push((close - 3, close, ";"));
    } else {
        ranges.push(at + 6);
        ranges.push(close);
    }
    close
}

/// The source with every range blanked (newlines kept) and every word written over its span.
fn apply_edits(s: &Src, ranges: &[usize], words: &[(usize, usize, &'static str)]) -> Option<Vec<u16>> {
    if ranges.is_empty() && words.is_empty() {
        return None;
    }
    let mut edits: Vec<(usize, usize, &str)> = words.to_vec();
    for r in ranges.chunks(2) {
        edits.push((r[0], r[1], ""));
    }
    // Array.prototype.sort is stable.
    edits.sort_by_key(|e| e.0);
    let n = s.len();
    let mut out: Vec<u16> = Vec::with_capacity(n);
    let mut last = 0usize;
    for (start, to, word) in edits {
        let from = start.max(last);
        if to <= from {
            continue;
        }
        let to_c = to.min(n);
        let written: &str = if from == start && word.len() <= to - from { word } else { "" };
        out.extend_from_slice(&s.u[last.min(n)..from.min(n)]);
        out.extend(written.encode_utf16());
        for &c in &s.u[(from + written.len()).min(to_c)..to_c] {
            out.push(if c == b'\r' as u16 || c == b'\n' as u16 { c } else { b' ' as u16 });
        }
        last = to;
    }
    out.extend_from_slice(&s.u[last.min(n)..]);
    Some(out)
}

/// The start of the `class` word before index `i`, past whitespace.
fn class_before(s: &Src, i: usize) -> Option<usize> {
    let mut j = i;
    while j > 0 && is_space(s.at(j - 1)) {
        j -= 1;
    }
    let w = j.checked_sub(5)?;
    (s.lower(w, j) == "class" && (w == 0 || !is_ident_part(s.at(w - 1)))).then_some(w)
}

/// Blank what the Pascal grammar can't parse. The source unchanged when there is nothing to.
pub(crate) fn pre_parse_pascal(source: &str) -> String {
    if !may_need_fixes().is_match(source) {
        return source.to_string();
    }
    let s = Src { u: source.encode_utf16().collect() };
    let directives = source.contains("{$");
    let mut ranges: Vec<usize> = Vec::new();
    let mut words: Vec<(usize, usize, &'static str)> = Vec::new();
    let groups = if directives { conditional_groups(&s) } else { Vec::new() };
    let mut group_at: HashMap<usize, usize> = HashMap::new();
    for (g, group) in groups.iter().enumerate() {
        group_at.insert(group.bounds[0], g);
    }
    let mut label: HashMap<usize, bool> = HashMap::new();
    let mut last_at: Option<usize> = None;
    let mut last_end = 0usize;
    let n = s.len();
    let mut i = 0;
    while i < n {
        if let Some(k) = comment_or_string_end(&s, i) {
            if s.at(i) == CH_QUOTE {
                last_at = Some(i);
                last_end = k;
            } else if let Some(la) = last_at {
                if s.at(i + 1) == CH_DOLLAR && group_at.contains_key(&i) {
                    let position = statement_position(&s, la, last_end);
                    if position >= 0 {
                        label.insert(i, position == 1);
                    }
                }
            }
            i = k;
            continue;
        }
        let c = s.at(i);
        if is_ident_start(c) {
            let e = word_end(&s, i + 1);
            let len = e - i;
            let word = if (6..=11).contains(&len) { s.lower(i, e) } else { String::new() };
            if ROUTINE_KEYWORDS.contains(&word.as_str()) && (word != "operator" || class_before(&s, i).is_some()) {
                let open = skip_trivia(&s, routine_name_end(&s, e));
                let next = if s.at(open) == CH_LPAREN { scan_parameter_list(&s, open, &mut ranges) } else { e };
                if !group_at.is_empty() {
                    if let Some(header_end) = semicolon_end(&s, next, MAX_HEADER_PART_LENGTH) {
                        blank_conditionals_after_header(&s, header_end, &groups, &group_at, &mut ranges);
                    }
                }
                i = next;
                last_at = None;
                continue;
            } else if word == "property" {
                i = scan_property_override(&s, i, e, &mut ranges);
                last_at = None;
                continue;
            } else if word == "record" {
                i = scan_anonymous_record(&s, i, e, &mut ranges, &mut words);
                last_at = None;
                continue;
            } else if len == 3 && s.lower(i, e) == "for" {
                i = scan_for_in_variable(&s, e, &mut ranges);
                last_at = None;
                continue;
            } else if len == 5 && s.lower(i, e) == "raise" {
                let next = skip_trivia(&s, e);
                let after = lower_word_at(&s, next);
                if s.at(next) == CH_SEMI || after == "else" {
                    ranges.push(i);
                    ranges.push(e);
                } else if LABEL_ENDS.contains(&after.as_str()) {
                    words.push((i, e, ";"));
                }
            } else {
                let mut colon = e;
                while s.at(colon) == CH_SPACE || s.at(colon) == CH_TAB {
                    colon += 1;
                }
                if s.at(colon) == CH_COLON
                    && s.at(colon + 1) != CH_EQ
                    && s.before(i) != CH_AT
                    && LABEL_ENDS.contains(&lower_word_at(&s, skip_trivia(&s, colon + 1)).as_str())
                {
                    ranges.push(i);
                    ranges.push(colon + 1);
                }
            }
            last_at = Some(i);
            last_end = e;
            i = e;
        } else if is_digit(c) {
            last_at = Some(i);
            i = word_end(&s, i + 1);
            last_end = i;
        } else if is_space(c) {
            i += 1;
        } else {
            last_at = Some(i);
            i += if c == CH_COLON && s.at(i + 1) == CH_EQ { 2 } else { 1 };
            last_end = i;
        }
    }
    if !groups.is_empty() {
        blank_duplicate_begins(&s, &groups, &mut ranges);
        if !label.is_empty() {
            wrap_alternative_statements(&s, &groups, &label, &ranges, &mut words);
        }
    }
    match apply_edits(&s, &ranges, &words) {
        Some(out) => String::from_utf16_lossy(&out),
        None => source.to_string(),
    }
}
