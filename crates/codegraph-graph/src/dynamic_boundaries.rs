//! Dynamic-dispatch boundary detection — port of `src/mcp/dynamic-boundaries.ts`.
//!
//! When a flow does not connect statically, the cause is almost always a
//! dynamic-dispatch site: a computed member call, `getattr`, reflection, a
//! string-keyed bus, a typed command dispatcher. Guessing the missing edge was
//! rejected (silent beats wrong), so this scanner ANNOUNCES the site instead —
//! deterministic regex over the comment/string-stripped body, at query time
//! only, never mutating the graph.
//!
//! Matching runs on the stripped text (so commented-out or string-embedded
//! code cannot fire) while snippets and keys are sliced from the ORIGINAL
//! source at the same offsets; both strippers blank contents in place and
//! preserve UTF-16 offsets precisely for this.
//!
//! Offsets here are UTF-16 units, exactly as the TypeScript original indexes its
//! strings. `Re` reports byte offsets, so each match is converted once with
//! [`Match::utf16_start`].

use codegraph_jsre::{utf16_len, Re};
use codegraph_text::{blank_string_contents, strip_comments_for_regex};
use std::collections::HashMap;

/// One dynamic-dispatch site found in a symbol's body.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundaryMatch {
    /// Stable form id, e.g. `computed-call` — used for per-form dedupe.
    pub form: String,
    /// Human label for the dispatch form, e.g. `computed member call`.
    pub label: String,
    /// One-line source snippet of the site (from the original, untrimmed text).
    pub snippet: String,
    /// 1-based line within the scanned body's FILE (absolute, ready to print).
    pub line: u32,
    /// Statically-visible dispatch key, when one exists: the string literal in
    /// `handlers['save']`, the `:symbol` in ruby `send`, the type name in
    /// `Send(new CreateCmd(...))`. `None` when the key is a runtime value.
    pub key: Option<String>,
    /// For typed-bus matches the key is a TYPE name (candidates ~ `${key}Handler`).
    pub key_is_type: bool,
    /// Additional sites of the same form+key in this body beyond the reported one.
    pub more_sites: u32,
}

#[derive(Debug, Clone, PartialEq)]
struct DerivedKey {
    key: String,
    key_is_type: bool,
}

type KeyFrom = fn(&str) -> Option<DerivedKey>;
type Accept = fn(&[u16], &[u16], usize) -> bool;

struct FormSpec {
    form: &'static str,
    label: &'static str,
    /// Languages this form applies to; `None` = all. Node.language values.
    langs: Option<&'static [&'static str]>,
    re: &'static str,
    key_from: Option<KeyFrom>,
    /// Extra ORIGINAL chars after the match end handed to `key_from`, capped at
    /// the first newline — for forms whose key trails the matched prefix.
    key_window: Option<usize>,
    /// Final say on a match, for forms whose argument decides it.
    accept: Option<Accept>,
}

const JS_FAMILY: &[&str] = &[
    "typescript",
    "javascript",
    "tsx",
    "jsx",
    "vue",
    "svelte",
    "astro",
    "arkts",
];
const PY: &[&str] = &["python"];
const RB: &[&str] = &["ruby"];
const PHP: &[&str] = &["php"];
const JVM_CS_GO: &[&str] = &["java", "kotlin", "scala", "csharp", "go"];
const SWIFT_OBJC: &[&str] = &["swift", "objc", "objcpp", "objective-c"];

fn single_string_literal(text: &str) -> Option<String> {
    let re = Re::cached(r#"^[^'"`]*(['"`])([\w.:-]{2,64})\1[^'"`]*$"#, "");
    re.captures(text).and_then(|c| c.text(2).map(str::to_owned))
}

fn key_single_literal(orig: &str) -> Option<DerivedKey> {
    single_string_literal(orig).map(|key| DerivedKey {
        key,
        key_is_type: false,
    })
}

fn key_computed_call(orig: &str) -> Option<DerivedKey> {
    let inner = Re::cached(r"\[([^[\]\n]{1,80})\]\s*\($", "");
    let caps = inner.captures(orig)?;
    key_single_literal(caps.text(1)?)
}

fn key_ruby_send(orig: &str) -> Option<DerivedKey> {
    let re = Re::cached(r":(\w+)", "");
    re.captures(orig).and_then(|c| {
        c.text(1).map(|key| DerivedKey {
            key: key.to_owned(),
            key_is_type: false,
        })
    })
}

fn key_typed_bus(orig: &str) -> Option<DerivedKey> {
    let re = Re::cached(r"new\s+([A-Z]\w*)$", "");
    re.captures(orig).and_then(|c| {
        c.text(1).map(|key| DerivedKey {
            key: key.to_owned(),
            key_is_type: true,
        })
    })
}

fn key_selector(orig: &str) -> Option<DerivedKey> {
    let re = Re::cached(r"#selector\s*\(\s*([\w.]+)", "");
    let caps = re.captures(orig)?;
    let last = caps.text(1)?.rsplit('.').next().unwrap_or_default();
    Some(DerivedKey {
        key: last.to_owned(),
        key_is_type: false,
    })
}

const FORMS: &[FormSpec] = &[
    // handlers[action.type](payload) / registry[key](args) / table[k](...)
    FormSpec {
        form: "computed-call",
        label: "computed member call",
        langs: None,
        re: r"[\w$)\]]\s*\[([^[\]\n]{1,80})\]\s*\(",
        key_from: Some(key_computed_call),
        key_window: None,
        accept: None,
    },
    // import(expr) / require(expr) with a NON-literal argument.
    FormSpec {
        form: "dynamic-import",
        label: "dynamic import",
        langs: Some(JS_FAMILY),
        re: r"\b(?:import|require)\s*\(",
        key_from: None,
        key_window: None,
        accept: Some(is_runtime_import_argument),
    },
    FormSpec {
        form: "dynamic-import",
        label: "dynamic import",
        langs: Some(PY),
        re: r"\bimportlib\.import_module\s*\(|\b__import__\s*\(",
        key_from: None,
        key_window: None,
        accept: None,
    },
    // obj.send(:method_name) / public_send / method(:name) — ruby metaprogramming.
    FormSpec {
        form: "ruby-send",
        label: "send dispatch",
        langs: Some(RB),
        re: r"\.(?:public_)?send\s*\(\s*:?\w+|\bmethod\s*\(\s*:\w+\s*\)",
        key_from: Some(key_ruby_send),
        key_window: None,
        accept: None,
    },
    // call_user_func([$this, 'method']) / $this->$method() / $callback().
    FormSpec {
        form: "php-dynamic",
        label: "dynamic call",
        langs: Some(PHP),
        re: r"\bcall_user_func(?:_array)?\s*\(|\$this\s*->\s*\$\w+\s*\(|\$\w+\s*\(",
        key_from: Some(key_single_literal),
        key_window: Some(80),
        accept: None,
    },
    // Reflection: Method.invoke / getMethod("x") / Class.forName / Go
    // reflect MethodByName / C# Activator.CreateInstance, GetMethod.
    FormSpec {
        form: "reflection",
        label: "reflective dispatch",
        langs: Some(JVM_CS_GO),
        re: r"\.invoke\s*\(|\.get(?:Declared)?Method\s*\(|\.GetMethod\s*\(|MethodByName\s*\(|Activator\.CreateInstance|Class\.forName\s*\(",
        key_from: Some(key_single_literal),
        key_window: Some(80),
        accept: None,
    },
    // new Proxy(target, handler) / Reflect.get|apply — JS metaobject dispatch.
    FormSpec {
        form: "proxy-reflect",
        label: "Proxy/Reflect dispatch",
        langs: Some(JS_FAMILY),
        re: r"\bnew\s+Proxy\s*\(|\bReflect\.(?:get|apply|construct)\s*\(",
        key_from: None,
        key_window: None,
        accept: None,
    },
    // mediator.Send(new CreateTodoItemCommand(...)) / bus.publish(new OrderEvent(...))
    FormSpec {
        form: "typed-bus",
        label: "typed message dispatch",
        langs: None,
        re: r"\.(?:[Ss]end|[Pp]ublish|[Dd]ispatch|[Ee]xecute|[Pp]ost|[Ee]mit)(?:Async)?\s*(?:<[^<>\n]{0,80}>)?\s*\(\s*new\s+([A-Z]\w*)",
        key_from: Some(key_typed_bus),
        key_window: None,
        accept: None,
    },
    // emitter.emit(eventVar, ...) / store.dispatch(action) — runtime key.
    FormSpec {
        form: "var-key-dispatch",
        label: "string-keyed dispatch (runtime key)",
        langs: None,
        re: r"\.(?:emit|dispatch|trigger|fire|publish|broadcast)\s*\(\s*[A-Za-z_$][\w$]*(?:\.[\w$]+){0,3}\s*[,)]",
        key_from: None,
        key_window: None,
        accept: None,
    },
    // Swift/ObjC: #selector(name) / NSClassFromString — runtime selector dispatch.
    FormSpec {
        form: "selector",
        label: "selector dispatch",
        langs: Some(SWIFT_OBJC),
        re: r"#selector\s*\(\s*([\w.]+)|NSClassFromString\s*\(",
        key_from: Some(key_selector),
        key_window: None,
        accept: None,
    },
];

/// Map a Node.language to the comment-stripper's language set.
fn comment_lang(language: &str) -> Option<&'static str> {
    Some(match language {
        "python" => "python",
        "ruby" => "ruby",
        "rust" => "rust",
        "php" => "php",
        "go" => "go",
        "javascript" | "jsx" => "javascript",
        "typescript" | "tsx" | "vue" | "svelte" | "astro" | "arkts" => "typescript",
        "java" | "kotlin" | "scala" | "dart" => "java",
        "csharp" => "csharp",
        "swift" => "swift",
        "c" | "cpp" | "objc" | "objcpp" => "java",
        _ => return None,
    })
}

const MAX_MATCHES_PER_BODY: usize = 3;
const MAX_BODY_CHARS: usize = 60_000;

/// JavaScript's `\s`, the set `/\s/.test(c)` answers for.
fn is_js_space(u: u16) -> bool {
    matches!(
        u,
        0x09 | 0x0A | 0x0B | 0x0C | 0x0D | 0x20 | 0xA0 | 0x1680 | 0x2000..=0x200A
            | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000 | 0xFEFF
    )
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn units_to_string(u: &[u16]) -> String {
    String::from_utf16_lossy(u)
}

/// `String.prototype.trim` over UTF-16 units.
fn js_trim(u: &[u16]) -> &[u16] {
    let start = u.iter().position(|c| !is_js_space(*c)).unwrap_or(u.len());
    let end = u
        .iter()
        .rposition(|c| !is_js_space(*c))
        .map(|i| i + 1)
        .unwrap_or(start);
    &u[start..end]
}

/// Whether the call argument starting at `start` is anything but ONE complete
/// string literal: `import('./a')` or a backtick string without a substitution
/// is an ordinary import, while a template with a `${}` substitution, a
/// concatenation or a bare expression picks the module at runtime.
fn is_runtime_import_argument(stripped: &[u16], original: &[u16], start: usize) -> bool {
    let mut i = start;
    while i < stripped.len() && is_js_space(stripped[i]) {
        i += 1;
    }
    let Some(&quote) = stripped.get(i) else {
        return false;
    };
    if quote == u16::from(b')') {
        return false; // `import()` — nothing to resolve
    }
    if quote != u16::from(b'"') && quote != u16::from(b'\'') && quote != u16::from(b'`') {
        return true;
    }
    let Some(close) = (i + 1..stripped.len()).find(|&k| stripped[k] == quote) else {
        return false;
    };
    if quote == u16::from(b'`') {
        let mut k = i + 1;
        while k < close {
            // Skip escape pairs: \${ is literal text, but \\${ interpolates.
            if original[k] == u16::from(b'\\') {
                k += 2;
                continue;
            }
            if original[k] == u16::from(b'$') && original.get(k + 1) == Some(&u16::from(b'{')) {
                return true;
            }
            k += 1;
        }
    }
    let mut j = close + 1;
    while j < stripped.len() && is_js_space(stripped[j]) {
        j += 1;
    }
    // `)` ends the call; `,` starts import()'s options argument. Anything else
    // (`+`, `.concat(`, …) builds the specifier at runtime.
    match stripped.get(j) {
        Some(&c) => c != u16::from(b')') && c != u16::from(b','),
        None => true,
    }
}

/// `body` truncated to `max` UTF-16 units.
fn truncate_utf16(s: &str, max: usize) -> &str {
    let mut n = 0;
    for (i, c) in s.char_indices() {
        let len = c.len_utf16();
        if n + len > max {
            return &s[..i];
        }
        n += len;
    }
    s
}

fn count_newlines(u: &[u16], end: usize) -> u32 {
    u[..end.min(u.len())]
        .iter()
        .filter(|c| **c == u16::from(b'\n'))
        .count() as u32
}

/// The full source line containing `index`, trimmed and capped for display.
fn snippet_around(u: &[u16], index: usize) -> String {
    let line_start = u[..index.min(u.len())]
        .iter()
        .rposition(|c| *c == u16::from(b'\n'))
        .map(|i| i + 1)
        .unwrap_or(0);
    let line_end = u[index.min(u.len())..]
        .iter()
        .position(|c| *c == u16::from(b'\n'))
        .map(|i| i + index.min(u.len()))
        .unwrap_or(u.len());
    let line = js_trim(&u[line_start..line_end]);
    if line.len() > 120 {
        format!("{}...", units_to_string(&line[..117]))
    } else {
        units_to_string(line)
    }
}

fn match_balanced_paren(text: &[u16], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let end = text.len().min(open + 300);
    for (i, c) in text.iter().enumerate().take(end).skip(open) {
        if *c == u16::from(b'(') {
            depth += 1;
        } else if *c == u16::from(b')') {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// Scan one symbol's body for dynamic-dispatch sites.
///
/// `file_start_line` is the 1-based line where `body` starts in its file —
/// returned line numbers are absolute file lines.
pub fn scan_dynamic_dispatch(body: &str, language: &str, file_start_line: u32) -> Vec<BoundaryMatch> {
    let original = truncate_utf16(body, MAX_BODY_CHARS);
    let orig_units = units(original);
    let stripped_source = match comment_lang(language) {
        Some(lang) => strip_comments_for_regex(original, lang),
        None => original.to_owned(),
    };
    let stripped = blank_string_contents(&stripped_source, None);
    let stripped_units = units(&stripped);

    let mut out: Vec<BoundaryMatch> = Vec::new();
    // form+key → index in `out`, for the per-form dedupe that counts extras.
    let mut seen: HashMap<String, usize> = HashMap::new();

    if language == "python" {
        scan_python_getattr(
            &stripped,
            &stripped_units,
            &orig_units,
            file_start_line,
            &mut out,
            &mut seen,
        );
    }

    for spec in FORMS {
        if out.len() >= MAX_MATCHES_PER_BODY {
            break;
        }
        if let Some(langs) = spec.langs {
            if !langs.contains(&language) {
                continue;
            }
        }
        let re = Re::cached(spec.re, "");
        for caps in re.captures_iter(&stripped) {
            let Some(whole) = caps.get(0) else { continue };
            let start = whole.utf16_start();
            let end = start + utf16_len(whole.as_str());
            if let Some(accept) = spec.accept {
                if !accept(&stripped_units, &orig_units, end) {
                    continue;
                }
            }
            let mut slice_end = end;
            if let Some(window) = spec.key_window {
                let window_end = orig_units.len().min(slice_end + window);
                let newline = orig_units[slice_end.min(orig_units.len())..]
                    .iter()
                    .position(|c| *c == u16::from(b'\n'))
                    .map(|i| i + slice_end.min(orig_units.len()));
                slice_end = match newline {
                    Some(nl) if nl < window_end => nl,
                    _ => window_end,
                };
            }
            let orig_slice =
                units_to_string(&orig_units[start.min(orig_units.len())..slice_end]);
            let derived = spec.key_from.and_then(|f| f(&orig_slice));
            let dedupe_key = format!(
                "{}|{}",
                spec.form,
                derived.as_ref().map(|d| d.key.as_str()).unwrap_or("")
            );
            if let Some(&prior) = seen.get(&dedupe_key) {
                out[prior].more_sites += 1;
                continue;
            }
            let match_ = BoundaryMatch {
                form: spec.form.to_owned(),
                label: spec.label.to_owned(),
                snippet: snippet_around(&orig_units, start),
                line: file_start_line + count_newlines(&orig_units, start),
                key: derived.as_ref().map(|d| d.key.clone()),
                key_is_type: derived.as_ref().is_some_and(|d| d.key_is_type),
                more_sites: 0,
            };
            seen.insert(dedupe_key, out.len());
            out.push(match_);
            if out.len() >= MAX_MATCHES_PER_BODY {
                return out;
            }
        }
    }
    out
}

/// Python `getattr` dispatch — handled in code, not the FORMS table, because
/// real getattr calls have nested-call arguments spanning lines
/// (`getattr(self, request.method.lower(),\n  self.http_method_not_allowed)`)
/// that a regex argument class cannot bound. Two shapes:
///   getattr(obj, name)(args)                      → immediate call
///   handler = getattr(obj, name) ... handler(...)  → assigned, called later
fn scan_python_getattr(
    stripped: &str,
    stripped_units: &[u16],
    orig_units: &[u16],
    file_start_line: u32,
    out: &mut Vec<BoundaryMatch>,
    seen: &mut HashMap<String, usize>,
) {
    let re = Re::cached(r"\bgetattr\s*\(", "");
    for caps in re.captures_iter(stripped) {
        if out.len() >= MAX_MATCHES_PER_BODY {
            return;
        }
        let Some(whole) = caps.get(0) else { continue };
        let m_start = whole.utf16_start();
        let open = m_start + utf16_len(whole.as_str()) - 1;
        let Some(close) = match_balanced_paren(stripped_units, open) else {
            continue;
        };

        // Immediate call: getattr(...)(
        let after = &stripped_units[(close + 1).min(stripped_units.len())
            ..(close + 8).min(stripped_units.len())];
        let mut k = 0;
        while k < after.len() && is_js_space(after[k]) {
            k += 1;
        }
        let (form, label) = if after.get(k) == Some(&u16::from(b'(')) {
            ("getattr-call", "getattr dispatch")
        } else {
            // Assigned form: look back for `name =` and forward for `name(`.
            let line_start = stripped_units[..m_start.min(stripped_units.len())]
                .iter()
                .rposition(|c| *c == u16::from(b'\n'))
                .map(|i| i + 1)
                .unwrap_or(0);
            let before = units_to_string(&stripped_units[line_start..m_start.min(stripped_units.len())]);
            let assign = Re::cached(r"(\w+)\s*=\s*$", "");
            let name = assign
                .captures(&before)
                .and_then(|c| c.text(1).map(str::to_owned));
            let called_later = name
                .map(|n| {
                    let pat = format!(r"\b{}\s*\(", n);
                    Re::cached(&pat, "").is_match(&units_to_string(
                        &stripped_units[(close + 1).min(stripped_units.len())..],
                    ))
                })
                .unwrap_or(false);
            if !called_later {
                continue;
            }
            (
                "getattr-assign",
                "getattr dispatch (assigned, called later)",
            )
        };

        let key = single_string_literal(&units_to_string(
            &orig_units[(open + 1).min(orig_units.len())..close.min(orig_units.len())],
        ));
        let dedupe_key = format!("{}|{}", form, key.as_deref().unwrap_or(""));
        if let Some(&prior) = seen.get(&dedupe_key) {
            out[prior].more_sites += 1;
            continue;
        }
        let match_ = BoundaryMatch {
            form: form.to_owned(),
            label: label.to_owned(),
            snippet: snippet_around(orig_units, m_start),
            line: file_start_line + count_newlines(orig_units, m_start),
            key,
            key_is_type: false,
            more_sites: 0,
        };
        seen.insert(dedupe_key, out.len());
        out.push(match_);
    }
}
