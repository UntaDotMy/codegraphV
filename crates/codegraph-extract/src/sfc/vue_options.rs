//! Options API member discovery, ported from `vue-options-api.ts`. This is
//! intentionally lexical: its accepted wrappers and members match the reference.
use codegraph_jsre::re;
use codegraph_types::jscompat::{is_js_space, js_trim};

pub struct OptionsMember {
    pub name: String,
    pub start: usize,
    pub end: usize,
}
struct RawMember {
    name: String,
    start: usize,
    value_at: usize,
    end: usize,
    shorthand: bool,
}

fn component_function(name: &str) -> bool {
    matches!(
        name,
        "data"
            | "setup"
            | "render"
            | "beforeCreate"
            | "created"
            | "beforeMount"
            | "mounted"
            | "beforeUpdate"
            | "updated"
            | "activated"
            | "deactivated"
            | "beforeDestroy"
            | "destroyed"
            | "beforeUnmount"
            | "unmounted"
            | "errorCaptured"
            | "renderTracked"
            | "renderTriggered"
            | "serverPrefetch"
            | "asyncData"
            | "fetch"
            | "head"
            | "validate"
            | "middleware"
    )
}

/// Matching and quoted-template skipping use heap frames, so malformed or
/// deeply nested user code cannot overflow a native thread's stack.
enum Frame {
    Group(i64),
    Quote(u8),
}
fn skip_balanced(s: &str, at: usize, quote: Option<u8>) -> Option<usize> {
    let b = s.as_bytes();
    let mut frames = vec![quote.map(Frame::Quote).unwrap_or(Frame::Group(0))];
    let mut i = if quote.is_some() { at + 1 } else { at };
    while i < b.len() {
        match frames.last_mut().unwrap() {
            Frame::Quote(q) => {
                if b[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if *q == b'`' && b[i] == b'$' && b.get(i + 1) == Some(&b'{') {
                    frames.push(Frame::Group(0));
                    i += 1;
                    continue;
                }
                if b[i] == *q {
                    frames.pop();
                    if frames.is_empty() {
                        return Some(i + 1);
                    }
                }
                i += 1;
            }
            Frame::Group(depth) => match b[i] {
                b'\'' | b'"' | b'`' => {
                    frames.push(Frame::Quote(b[i]));
                    i += 1;
                }
                b'/' if b.get(i + 1) == Some(&b'/') => {
                    i = s[i..].find('\n').map(|p| i + p + 1).unwrap_or(b.len());
                }
                b'/' if b.get(i + 1) == Some(&b'*') => {
                    i = s[i + 2..].find("*/").map(|p| i + p + 4).unwrap_or(b.len());
                }
                b'{' | b'[' | b'(' => {
                    *depth += 1;
                    i += 1;
                }
                b'}' | b']' | b')' => {
                    *depth -= 1;
                    if *depth == 0 {
                        frames.pop();
                        if frames.is_empty() {
                            return Some(i);
                        }
                    }
                    i += 1;
                }
                _ => i += 1,
            },
        }
    }
    None
}
fn match_close(s: &str, at: usize) -> Option<usize> {
    skip_balanced(s, at, None)
}
fn skip_quoted(s: &str, at: usize) -> Option<usize> {
    skip_balanced(s, at, Some(s.as_bytes()[at]))
}

fn object_members(s: &str, open: usize) -> Vec<RawMember> {
    let Some(close) = match_close(s, open) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut i = open + 1;
    while i < close {
        if let Some(ws) = re!(r"^(?:\s|,|//[^\n]*|/\*[\s\S]*?\*/)+").find(&s[i..close]) {
            i += ws.end();
            continue;
        }
        let start = i;
        let Some(head) = re!(r#"^(?:(?:async|get|set)\s+(?=[\w$'"[]))?\*?\s*(?:([A-Za-z_$][\w$]*)|(['"])((?:(?!\2).)*)\2|(\[)|(\.\.\.))"#).captures(&s[i..close]) else { i += s[i..].chars().next().unwrap().len_utf8(); continue; };
        let name = head.text(1).or_else(|| head.text(3));
        let mut j = i + head.get(0).unwrap().end();
        // Preserve the reference capture indices, including spread handling.
        if head.text(5).is_some() {
            let Some(end) = match_close(s, j - 1) else {
                return out;
            };
            j = end + 1;
        }
        let mut k = j;
        while k < close {
            match s.as_bytes()[k] {
                b'\'' | b'"' | b'`' => {
                    let Some(end) = skip_quoted(s, k) else {
                        return out;
                    };
                    k = end;
                }
                b'/' if matches!(s.as_bytes().get(k + 1), Some(b'/' | b'*')) => {
                    k = if s.as_bytes()[k + 1] == b'/' {
                        s[k..].find('\n').map(|n| k + n + 1).unwrap_or(close)
                    } else {
                        s[k + 2..].find("*/").map(|n| k + n + 4).unwrap_or(close)
                    };
                }
                b'{' | b'[' | b'(' => {
                    let Some(end) = match_close(s, k) else {
                        return out;
                    };
                    k = end + 1;
                }
                b',' => break,
                _ => k += 1,
            }
        }
        let rest = &s[j..k];
        let colon = re!(r"^\s*:").find(rest);
        if let Some(name) = name {
            out.push(RawMember {
                name: name.to_owned(),
                start,
                value_at: colon.map(|m| j + m.end()).unwrap_or(j),
                end: k,
                shorthand: colon.is_none() && re!(r"^\s*\(").is_match(rest),
            });
        }
        i = k + 1;
    }
    out
}
fn is_function(s: &str, m: &RawMember) -> bool {
    if m.shorthand {
        return true;
    }
    let value = s[m.value_at..m.end].trim_start_matches(is_js_space);
    re!(r"^(?:async\s+)?function\b").is_match(value)
        || re!(r"^(?:async\s*)?(?:\([^)]*\)|[A-Za-z_$][\w$]*)\s*=>").is_match(value)
}
pub fn options_members(script: &str) -> Vec<OptionsMember> {
    let Some(m) = re!(r#"\bexport\s+default\s+(?:(?:defineComponent|defineNuxtComponent|Vue\s*\.\s*extend)\s*\(\s*|Vue\s*\.\s*component\s*\(\s*(['"])[^'"]*\1\s*,\s*)?\{"#).find(script) else { return Vec::new(); };
    let mut out = Vec::new();
    for member in object_members(script, m.end() - 1) {
        if component_function(&member.name) && is_function(script, &member) {
            out.push(OptionsMember {
                name: member.name,
                start: member.start,
                end: member.end,
            });
            continue;
        }
        if !matches!(member.name.as_str(), "methods" | "computed" | "watch") || member.shorthand {
            continue;
        }
        let Some(at) = script[member.value_at..]
            .find('{')
            .map(|n| member.value_at + n)
        else {
            continue;
        };
        if at >= member.end || !js_trim(&script[member.value_at..at]).is_empty() {
            continue;
        }
        for entry in object_members(script, at) {
            let value = script[entry.value_at..entry.end].trim_start_matches(is_js_space);
            if is_function(script, &entry) || (member.name != "methods" && value.starts_with('{')) {
                out.push(OptionsMember {
                    name: entry.name,
                    start: entry.start,
                    end: entry.end,
                });
            }
        }
    }
    out
}
