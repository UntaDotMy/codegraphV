//! Text helpers for C-family argument lists: splitting at top-level commas, finding the
//! parenthesised list after a name, dropping comments.

use super::util::lines_of;
use crate::ctx::Ctx;
use codegraph_jsre::{escape_regex, Re};
use codegraph_types::jscompat::utf16_byte_index;

fn skip_quoted(chars: &[char], i: usize, stop_at_newline: bool) -> usize {
    let quote = chars[i];
    let mut j = i + 1;
    while j < chars.len() && chars[j] != quote && !(stop_at_newline && chars[j] == '\n') {
        j += if chars[j] == '\\' { 2 } else { 1 };
    }
    j
}

fn slice(chars: &[char], from: usize, to_inclusive: usize) -> String {
    let end = (to_inclusive + 1).min(chars.len());
    if from >= end {
        String::new()
    } else {
        chars[from..end].iter().collect()
    }
}

/// `splitCppTopLevel`: the comma-separated pieces of `text`, outside brackets, quotes and
/// template argument lists.
pub fn split_cpp_top_level(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut depth: i64 = 0;
    let mut closes: Vec<isize> = Vec::new();
    let mut cur = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '"' || ch == '\'' {
            let j = skip_quoted(&chars, i, false);
            cur.push_str(&slice(&chars, i, j));
            i = j + 1;
            continue;
        }
        if ch == '(' || ch == '[' || ch == '{' {
            depth += 1;
        } else if ch == ')' || ch == ']' || ch == '}' {
            depth = (depth - 1).max(0);
        } else if ch == '<' {
            let end = cpp_template_arguments_end_chars(&chars, i);
            if end > i as isize {
                closes.push(end);
            }
        } else if ch == '>' && closes.last() == Some(&(i as isize)) {
            closes.pop();
        }
        if ch == ',' && depth == 0 && closes.is_empty() {
            out.push(cur.trim().to_string());
            cur.clear();
            i += 1;
            continue;
        }
        cur.push(ch);
        i += 1;
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn cpp_template_arguments_end_chars(chars: &[char], at: usize) -> isize {
    let before_is_word = at > 0 && (chars[at - 1].is_alphanumeric() || chars[at - 1] == '_');
    let next = chars.get(at + 1).copied();
    if !before_is_word || next == Some('<') || next == Some('=') {
        return -1;
    }
    let mut angles = 0i64;
    let mut others = 0i64;
    let mut i = at;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '"' || ch == '\'' {
            i = skip_quoted(&chars, i, false);
        } else if ch == '(' || ch == '[' || ch == '{' {
            others += 1;
        } else if ch == ')' || ch == ']' || ch == '}' {
            others -= 1;
            if others < 0 {
                return -1;
            }
        } else if others > 0 {
            i += 1;
            continue;
        } else if ch == '<' {
            angles += 1;
        } else if ch == '>' && (i == 0 || chars[i - 1] != '-') {
            angles -= 1;
            if angles == 0 {
                return i as isize;
            }
        }
        i += 1;
    }
    -1
}

/// The index (in UTF-16 units of `text`) of the `>` closing the template argument list that
/// opens at `at`, or -1.
pub fn cpp_template_arguments_end(text: &str, at: usize) -> isize {
    let chars: Vec<char> = text.chars().collect();
    // `at` is a UTF-16 index; callers of this variant pass ASCII-only argument text.
    cpp_template_arguments_end_chars(&chars, at)
}

/// `cppParenListIn`: the text between the parentheses of `name(...)` found at or after
/// UTF-16 `column` of `text`.
pub fn cpp_paren_list_in(text: &str, column: usize, name: &str) -> Option<String> {
    let from = utf16_byte_index(text, column);
    let rest = &text[from..];
    let re = Re::cached(&format!(r"\b{}\s*(?:<[^<>()]*>)?\s*\(", escape_regex(name)), "");
    let m = re.find(rest)?;
    let open = from + m.end() - 1;
    let bytes = text.as_bytes();
    let mut depth = 0i64;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        if b == b'(' {
            depth += 1;
        } else if b == b')' {
            depth -= 1;
            if depth == 0 {
                return Some(text[open + 1..i].to_string());
            }
        }
    }
    None
}

/// `cppParenListAfter`: the parameter/argument list of `name(...)` at `file:line` (1-based) from
/// UTF-16 `column`, looking at most a dozen lines ahead.
pub fn cpp_paren_list_after(file: &str, line: u32, column: u32, name: &str, ctx: &Ctx) -> Option<String> {
    let lines = lines_of(ctx, file);
    let start = (line as usize).saturating_sub(1).min(lines.len());
    let end = (line as usize + 11).min(lines.len());
    let text = if start < end { lines[start..end].join("\n") } else { String::new() };
    cpp_paren_list_in(&text, column as usize, name)
}

/// `cppWithoutComments`: `text` with line and block comments replaced by a space.
pub fn cpp_without_comments(text: &str) -> String {
    if !text.contains('/') {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' || c == '\'' {
            let j = skip_quoted(&chars, i, true);
            out.push_str(&slice(&chars, i, j));
            i = j + 1;
        } else if c == '/' && chars.get(i + 1) == Some(&'/') {
            match chars[i..].iter().position(|&x| x == '\n') {
                Some(p) => i += p,
                None => i = chars.len(),
            }
            out.push(' ');
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            let mut k = i + 2;
            let mut end = None;
            while k + 1 < chars.len() {
                if chars[k] == '*' && chars[k + 1] == '/' {
                    end = Some(k);
                    break;
                }
                k += 1;
            }
            match end {
                Some(e) => i = e + 2,
                None => i = chars.len(),
            }
            out.push(' ');
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// `cppCallArguments`: the written arguments of the call to `name` at the reference.
pub fn cpp_call_arguments(r: &crate::types::UnresolvedRef, name: &str, ctx: &Ctx) -> Option<Vec<String>> {
    let list = cpp_paren_list_after(&r.file_path, r.line, r.column, name, ctx)?;
    Some(split_cpp_top_level(&cpp_without_comments(&list)))
}
