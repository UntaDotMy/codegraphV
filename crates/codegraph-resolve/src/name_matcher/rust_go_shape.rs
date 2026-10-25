//! How a bare Rust or Go name is written at its call (`rustGoCallShape`) and which same-named
//! candidates that shape can reach (`isRustGoCallTarget`).
//!
//! The extractors keep one receiver level, so `sym.filename().map(From::from)` and
//! `child.Flags().String("f", …)` arrive as bare `map` / `String`.

use super::lang_shape::RefShape;
use super::method::shares_receiver_word;
use super::std_methods::{GO_STD_METHODS, RUST_STD_METHODS};
use super::util::*;
use crate::ctx::Ctx;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::is_js_space;
use codegraph_types::{Language, NodeKind};

/// `path` after `::` (left to the path strategies), `chained` after a `.` — with the receiver it
/// is written on, call and type arguments dropped — or `bare`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RustGoCallShape {
    Path,
    Bare,
    Chained(String),
}

pub fn rust_go_call_shape(r: &UnresolvedRef, ctx: &Ctx) -> Option<RustGoCallShape> {
    let lines = ctx.get_file_lines(&r.file_path)?;
    let mut line: String = lines.get((r.line as usize).checked_sub(1)?)?.clone();
    let name = r.reference_name.as_str();
    let at = Re::cached(&format!(r"(?<![\w$]){}\s*(?:\(|::<|!)", escape_regex(name)), "");
    let mut start: Option<usize> = if starts_with_at(&line, name, r.column) {
        Some(col_byte(&line, r.column))
    } else {
        at.find(&line).map(|m| m.start())
    };
    // A link further down a chain the call's line starts — `bat()\n  .arg(…)\n  .stdout(…)` —
    // is recorded at the chain's first line.
    let mut next = r.line as usize;
    while start.is_none() && next < lines.len().min(r.line as usize + 20) {
        let candidate = &lines[next];
        let m = if re!(r"^\s*\.").is_match(candidate) { at.find(candidate) } else { None };
        if let Some(m) = m {
            if re!(r"^\s*\.\s*$").is_match(&candidate[..m.start()]) {
                let head = line.trim_end_matches(is_js_space);
                let head = re!(r"[?;]+$").replace_first(head, |_| String::new());
                return Some(RustGoCallShape::Chained(rust_go_receiver_name(&head)));
            }
            line = candidate.clone();
            start = Some(m.start());
        }
        next += 1;
    }
    let before = &line[..start?];
    if re!(r"::\s*$").is_match(before) {
        return Some(RustGoCallShape::Path);
    }
    if !re!(r"\.\s*$").is_match(before) {
        return Some(RustGoCallShape::Bare);
    }
    let receiver = re!(r"\?\s*\.\s*$").replace_first(before, |_| String::new());
    Some(RustGoCallShape::Chained(if is_tuple_receiver(&receiver) { "()".to_string() } else { rust_go_receiver_name(&receiver) }))
}

/// `(a, b).method` — a tuple literal, not a call's argument list.
fn is_tuple_receiver(text: &str) -> bool {
    let src = re!(r"\s*\.\s*$").replace_first(text, |_| String::new());
    if !src.ends_with(')') {
        return false;
    }
    let bytes = src.as_bytes();
    let mut depth = 0i32;
    let mut comma = false;
    let mut i = bytes.len() as isize - 1;
    while i >= 0 {
        match bytes[i as usize] {
            b')' | b']' | b'}' => depth += 1,
            b'(' | b'[' | b'{' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            b',' if depth == 1 => comma = true,
            _ => {}
        }
        i -= 1;
    }
    i >= 0 && comma && !re!(r"[\w$>\])!?]\s*$").is_match(&src[..i as usize])
}

/// The name a call chain is written on, call and index arguments dropped: `a.b().c` → `a.b.c`'s
/// last link.
pub fn rust_go_receiver_name(text: &str) -> String {
    let src = re!(r"\s*\.\s*$").replace_first(text, |_| String::new());
    let bytes = src.as_bytes();
    let mut out: Vec<u8> = Vec::new();
    let mut i = bytes.len() as isize - 1;
    while i >= 0 {
        let ch = bytes[i as usize];
        if matches!(ch, b')' | b']' | b'}') {
            let open = match ch {
                b')' => b'(',
                b']' => b'[',
                _ => b'{',
            };
            let mut depth = 0i32;
            let mut j = i;
            while j >= 0 {
                if bytes[j as usize] == ch {
                    depth += 1;
                } else if bytes[j as usize] == open {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                j -= 1;
            }
            if j < 0 {
                return String::new();
            }
            if ch == b')' && out.is_empty() {
                if let Some(literal) = re!(r"^\(\s*&?\s*([A-Za-z_][\w.]*)\s*\{").captures(&src[j as usize..=i as usize]).and_then(|c| c.text(1).map(String::from)) {
                    return literal;
                }
            }
            i = j - 1;
        } else if ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'$' | b'.' | b':' | b'!') {
            if ch != b'!' {
                out.push(ch);
            }
            i -= 1;
        } else {
            break;
        }
    }
    out.reverse();
    let out = String::from_utf8(out).unwrap_or_default();
    out.trim_matches(|c| c == '.' || c == ':').to_string()
}

/// May this candidate be what a call written in `shape` names?
pub fn is_rust_go_call_target(n: &NodeRef, shape: &RustGoCallShape) -> bool {
    let receiver = match shape {
        RustGoCallShape::Path => return true,
        RustGoCallShape::Bare => return n.kind != NodeKind::Method,
        RustGoCallShape::Chained(receiver) => receiver.as_str(),
    };
    let member = n.kind == NodeKind::Method;
    // A tuple's method is no named type's: serde's `(self.ip(), self.port()).serialize(s)` went
    // to the `impl Serialize for Path` below it.
    if receiver == "()" {
        return false;
    }
    if !member {
        return n.kind != NodeKind::Function;
    }
    // Go's `w.Header().Get(…)` / `r.Header.Set(…)`: net/http's Header map.
    if n.language == Language::Go
        && re!(r"(?:^|\.)Header$").is_match(receiver)
        && re!(r"^(?:Get|Set|Add|Del|Values|Clone|Write)$").is_match(&n.name)
    {
        return false;
    }
    // A name the standard library's own types all carry (`unwrap`, `clone`, `iter`, Go's
    // `String` / `Get`) needs a receiver named after the owner; a project-specific one keeps
    // its match — clap's `flag("n").short('n')` is `Arg::short`, cobra's `c.Root().Name()`
    // `Command::Name`.
    let std = if n.language == Language::Go { GO_STD_METHODS.contains(n.name.as_str()) } else { RUST_STD_METHODS.contains(n.name.as_str()) };
    !std || re!(r"^(?:self|Self)$").is_match(receiver) || (!receiver.is_empty() && shares_receiver_word(receiver, &n.qualified_name))
}

/// The Rust / Go contribution to [`super::lang_shape::LangShape`]: only a bare call's name.
pub struct RustGoShape(RustGoCallShape);

impl RustGoShape {
    pub fn of(r: &UnresolvedRef, ctx: &Ctx) -> Option<RustGoShape> {
        let applies = matches!(r.language, Language::Rust | Language::Go) && is_calls(r) && re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name);
        if !applies {
            return None;
        }
        rust_go_call_shape(r, ctx).map(RustGoShape)
    }
}

impl RefShape for RustGoShape {
    fn excludes(&self, n: &NodeRef, _r: &UnresolvedRef, _ctx: &Ctx) -> bool {
        !is_rust_go_call_target(n, &self.0)
    }
}
