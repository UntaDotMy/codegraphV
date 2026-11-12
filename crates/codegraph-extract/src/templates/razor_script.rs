//! The JavaScript in a Razor view's `<script>` block, with the Razor taken out. Port of
//! `src/extraction/razor-script.ts` (upstream 97492056).
//!
//! A view's script is a template: the server runs what follows an `@` and the browser gets the
//! result. A JavaScript grammar reads `var id = @Model.Id;` as an error, and
//! `@Html.Raw(Json.Serialize(Model))` as calls no script makes. So everything Razor is blanked
//! before the block is parsed, and lines keep their numbers:
//!
//!  - an expression (`@Model.Id`, `@T("x")`, `@(a + b)`, `@await F()`) becomes `0`;
//!  - a comment (`@* … *@`) and a code block (`@{ … }`) become blanks;
//!  - a control statement (`@if (…) { … } else { … }`, `@foreach (…) { … }`) loses its C#, and
//!    keeps the JavaScript it emits: what a `<text>` holds and an `@:` line.
//!
//! Scanning is over bytes (every delimiter is ASCII); a blanked character becomes as many spaces
//! as it has UTF-16 units, so columns match the TypeScript reference too.

use codegraph_jsre::re;

/// Razor's statement keywords: a block follows, not a value.
const CONTROL: [&str; 9] = ["if", "for", "foreach", "while", "switch", "lock", "using", "try", "do"];

#[derive(Clone, Copy, PartialEq)]
enum Mark {
    Keep,
    Blank,
    Zero,
}

fn closer(c: u8) -> Option<u8> {
    match c {
        b'(' => Some(b')'),
        b'[' => Some(b']'),
        b'{' => Some(b'}'),
        _ => None,
    }
}

/// `s[from..from+n]`, its end pulled back to a char boundary.
fn window(s: &str, from: usize, n: usize) -> &str {
    let from = from.min(s.len());
    let mut end = (from + n).min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[from..end]
}

/// Past a C# string or char literal at `at`.
fn skip_string(s: &[u8], at: usize) -> usize {
    let quote = s[at];
    let verbatim = at > 0 && s[at - 1] == b'@';
    let mut i = at + 1;
    while i < s.len() {
        let c = s[i];
        if !verbatim && c == b'\\' {
            i += 1;
        } else if c == quote {
            if verbatim && s.get(i + 1) == Some(&quote) {
                i += 1;
            } else {
                return i + 1;
            }
        } else if c == b'\n' && !verbatim {
            return i;
        }
        i += 1;
    }
    s.len()
}

/// Index just past the bracket closing the one at `open`. Strings and comments hold no brackets.
fn past_bracket(s: &[u8], open: usize) -> Option<usize> {
    let mut stack = vec![closer(s[open])?];
    let mut i = open + 1;
    while i < s.len() {
        let c = s[i];
        if c == b'"' || c == b'\'' {
            i = skip_string(s, i) - 1;
        } else if c == b'/' && s.get(i + 1) == Some(&b'/') {
            while i < s.len() && s[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && s.get(i + 1) == Some(&b'*') {
            let end = find(s, b"*/", i + 2)?;
            i = end + 1;
        } else if let Some(close) = closer(c) {
            stack.push(close);
        } else if Some(&c) == stack.last() {
            stack.pop();
            if stack.is_empty() {
                return Some(i + 1);
            }
        }
        i += 1;
    }
    None
}

fn find(s: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= s.len() {
        return None;
    }
    s[from..].windows(needle.len()).position(|w| w == needle).map(|p| p + from)
}

fn blank(out: &mut [Mark], s: &[u8], from: usize, to: usize) {
    for i in from..to.min(out.len()) {
        if s[i] != b'\n' && s[i] != b'\r' {
            out[i] = Mark::Blank;
        }
    }
}

/// Past an implicit expression starting at `at` (after the `@`): names, members, calls and indexers.
fn past_expression(src: &str, at: usize) -> usize {
    let s = src.as_bytes();
    let mut i = at;
    if let Some(awaited) = re!(r"^await\s+").find(window(src, i, 16)) {
        i += awaited.end();
    }
    let Some(name) = re!(r"^[A-Za-z_]\w*").find(window(src, i, 128)) else { return at };
    i += name.end();
    loop {
        let c = s.get(i).copied();
        match c {
            Some(b'(' | b'[') => match past_bracket(s, i) {
                Some(end) => i = end,
                None => return i,
            },
            // A generic call: `@Html.Partial<Foo>(…)`.
            Some(b'<') if re!(r"^<[A-Z][\w.,\s<>?]*>\(").is_match(window(src, i, 80)) => {
                i = find(s, b"(", i).unwrap_or(s.len());
            }
            Some(b'.') if s.get(i + 1).is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_') => {
                i += 1;
                i += re!(r"^\w+").find(window(src, i, 128)).map_or(0, |m| m.end());
            }
            Some(b'?') if s.get(i + 1) == Some(&b'.') && s.get(i + 2).is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_') => {
                i += 2;
                i += re!(r"^\w+").find(window(src, i, 128)).map_or(0, |m| m.end());
            }
            _ => return i,
        }
    }
}

/// Where the `</text>` closing the `<text>` at `open` starts, nested ones counted.
fn closing_text(src: &str, open: usize, to: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut at = open;
    while let Some(m) = re!(r"<(/?)text>", "g").captures_at(src, at) {
        let start = m.get(0).unwrap().start();
        if start >= to {
            return None;
        }
        depth += if m.text(1).is_some_and(|t| !t.is_empty()) { -1 } else { 1 };
        if depth == 0 {
            return Some(start);
        }
        at = m.get(0).unwrap().end();
    }
    None
}

/// Keep what a statement block emits as JavaScript, and blank the C# around it.
fn keep_emitted_script(src: &str, out: &mut [Mark], from: usize, to: usize) {
    let s = src.as_bytes();
    let mut i = from;
    while i < to {
        let text_tag = find(s, b"<text>", i).filter(|&a| a < to);
        let line_tag = find(s, b"@:", i).filter(|&a| a < to);
        let next = match (text_tag, line_tag) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => {
                blank(out, s, i, to);
                return;
            }
        };
        blank(out, s, i, next);
        if Some(next) == line_tag {
            let stop = find(s, b"\n", next).filter(|&e| e <= to).unwrap_or(to);
            blank(out, s, next, next + 2);
            blank_razor(src, out, next + 2, stop);
            i = stop;
        } else {
            let Some(close) = closing_text(src, next, to) else {
                blank(out, s, next, to);
                return;
            };
            blank(out, s, next, next + "<text>".len());
            blank_razor(src, out, next + "<text>".len(), close);
            blank(out, s, close, close + "</text>".len());
            i = close + "</text>".len();
        }
    }
}

/// Blank a control statement whose keyword starts at `at`; returns where it ends.
fn blank_control(src: &str, out: &mut [Mark], at: usize, limit: usize) -> usize {
    let s = src.as_bytes();
    let mut at = at;
    let mut i = at;
    loop {
        // The header: everything up to the block's `{`, brackets balanced.
        while i < limit && s[i] != b'{' {
            if s[i] == b'(' {
                match past_bracket(s, i) {
                    Some(end) => i = end,
                    None => {
                        i = limit;
                        break;
                    }
                }
            } else {
                i += 1;
            }
        }
        if i >= limit {
            blank(out, s, at, limit);
            return limit;
        }
        let Some(end) = past_bracket(s, i).filter(|&e| e <= limit) else {
            blank(out, s, at, limit);
            return limit;
        };
        blank(out, s, at, i + 1);
        keep_emitted_script(src, out, i + 1, end - 1);
        blank(out, s, end - 1, end);
        if !re!(r"^\s*(?:else\s+if\b|else\b|catch\b|finally\b)").is_match(window(src, end, 32)) {
            return end;
        }
        at = end;
        i = end;
    }
}

fn blank_razor(src: &str, out: &mut [Mark], from: usize, to: usize) {
    let s = src.as_bytes();
    let mut i = from;
    while i < to {
        let Some(at) = find(s, b"@", i).filter(|&a| a < to) else { return };
        // `name@host` is text to Razor, as it is here.
        if at > 0 && (s[at - 1].is_ascii_alphanumeric() || s[at - 1] == b'_') {
            i = at + 1;
            continue;
        }
        match s.get(at + 1).copied() {
            Some(b'@') => {
                blank(out, s, at, at + 1);
                i = at + 2;
            }
            Some(b'*') => {
                let stop = find(s, b"*@", at + 2).map(|e| e + 2).filter(|&e| e <= to).unwrap_or(to);
                blank(out, s, at, stop);
                i = stop;
            }
            Some(next @ (b'{' | b'(')) => {
                let Some(end) = past_bracket(s, at + 1).filter(|&e| e <= to) else { return };
                blank(out, s, at, end);
                if next == b'(' {
                    out[at] = Mark::Zero;
                }
                i = end;
            }
            Some(b':') => {
                blank(out, s, at, at + 2);
                i = at + 2;
            }
            _ => {
                let word = re!(r"^[A-Za-z_]\w*").find(window(src, at + 1, 32)).map(|m| m.as_str());
                match word {
                    None => i = at + 1,
                    Some(w) if CONTROL.contains(&w) => i = blank_control(src, out, at, to),
                    Some(_) => {
                        let end = past_expression(src, at + 1).min(to);
                        blank(out, s, at, end);
                        out[at] = Mark::Zero;
                        i = end.max(at + 1);
                    }
                }
            }
        }
    }
}

fn render(src: &str, out: &[Mark]) -> String {
    let mut text = String::with_capacity(src.len());
    for (at, c) in src.char_indices() {
        match out[at] {
            Mark::Keep => text.push(c),
            Mark::Zero => text.push('0'),
            Mark::Blank => {
                for _ in 0..c.len_utf16() {
                    text.push(' ');
                }
            }
        }
    }
    text
}

/// `source` with its Razor blanked; every line keeps its number and every UTF-16 column its place.
pub fn blank_razor_in_script(source: &str) -> String {
    if !source.contains('@') {
        return source.to_string();
    }
    let mut out = vec![Mark::Keep; source.len()];
    blank_razor(source, &mut out, 0, source.len());
    render(source, &out)
}

/// `source` with its `@* … *@` comments blanked, so a commented-out `<script>` is not a script.
pub fn blank_razor_comments(source: &str) -> String {
    if !source.contains("@*") {
        return source.to_string();
    }
    let s = source.as_bytes();
    let mut out = vec![Mark::Keep; source.len()];
    let mut from = 0;
    while let Some(at) = find(s, b"@*", from) {
        let Some(end) = find(s, b"*@", at + 2) else { break };
        blank(&mut out, s, at, end + 2);
        from = end + 2;
    }
    render(source, &out)
}

/// Whether a `<script>` element's attributes leave it JavaScript: no `type`, or a JavaScript one.
pub fn is_javascript_block(attrs: &str) -> bool {
    let Some(ty) = re!(r#"\btype\s*=\s*["']([^"']*)["']"#, "i").captures(attrs).and_then(|c| c.text(1).map(|t| t.trim().to_lowercase())) else {
        return true;
    };
    ty.is_empty() || re!(r"^(?:module|(?:text|application)/(?:javascript|ecmascript|typescript|babel|jsx)|text/(?:x-)?(?:babel|jsx))$").is_match(&ty)
}

#[cfg(test)]
mod tests {
    //! Port of razor-script.test.ts (upstream 97492056).
    use super::*;

    /// What survives, with runs of blanks closed up.
    fn left(source: &str) -> String {
        re!(r"\s+", "g").replace_all_str(&blank_razor_in_script(source), " ").trim().to_string()
    }

    #[test]
    fn keeps_every_offset_and_line() {
        let source = ["var id = @Model.Id;", "@if (Model.Show)", "{", "    <text>show();</text>", "}", "done();"].join("\r\n");
        let out = blank_razor_in_script(&source);
        assert_eq!(out.len(), source.len());
        assert_eq!(out.split('\n').count(), source.split('\n').count());
        assert_eq!(out.find("show();"), source.find("show();"));
        assert_eq!(out.find("done();"), source.find("done();"));
    }

    #[test]
    fn leaves_a_script_with_no_razor_as_it_is() {
        let source = "function save() { return fetch('/x'); }\n";
        assert_eq!(blank_razor_in_script(source), source);
    }

    #[test]
    fn turns_an_expression_into_a_value() {
        assert_eq!(left("var id = @Model.Id;"), "var id = 0 ;");
        assert_eq!(left("var sum = @(a + (b * 2));"), "var sum = 0 ;");
        assert_eq!(left("var one = @Model.Items[0].Name;"), "var one = 0 ;");
        assert_eq!(left("var safe = @Model?.Name;"), "var safe = 0 ;");
        assert_eq!(left("var html = @Html.Raw(Json.Serialize(Model.Items));"), "var html = 0 ;");
        assert_eq!(left("var part = @await Html.PartialAsync(\"Row\");"), "var part = 0 ;");
        assert_eq!(left("var link = @Url.Action(\"Add\", new { id = 1, name = \")\" });"), "var link = 0 ;");
        assert_eq!(left("var typed = @Html.Partial<Row>(\"x\");"), "var typed = 0 ;");
    }

    #[test]
    fn leaves_what_is_around_an_expression_inside_a_string() {
        assert_eq!(left("$('#@Html.IdFor(m => m.Id)').hide();"), "$('#0 ').hide();");
        assert_eq!(left("alert('@T(\"Saved\")');"), "alert('0 ');");
    }

    #[test]
    fn takes_an_address_for_text_and_an_escaped_at_for_nothing() {
        assert_eq!(left("var a = 'me@example.com';"), "var a = 'me@example.com';");
        assert_eq!(left("var b = '@@scope/pkg';"), "var b = ' @scope/pkg';");
    }

    #[test]
    fn blanks_comments_and_code_blocks() {
        assert_eq!(left("a(); @* b(); *@ c();"), "a(); c();");
        assert_eq!(left("a(); @{ var x = \"}\"; Do(x); } c();"), "a(); c();");
    }

    #[test]
    fn keeps_what_a_control_statement_emits_and_drops_its_csharp() {
        assert_eq!(left("@if (Model.Show) { <text>show();</text> } else { var y = Compute(); }"), "show();");
        assert_eq!(
            left("@if (a) { <text>one();</text> } else if (b) { <text>two();</text> } else { <text>three();</text> } after();"),
            "one(); two(); three(); after();"
        );
        assert_eq!(left("@foreach (var x in Model.Items) { @:add('@x.Name');\n } after();"), "add('0 '); after();");
        assert_eq!(left("@try { Risky(); } catch (Exception e) { <text>failed();</text> } finally { Done(); }"), "failed();");
    }

    #[test]
    fn reads_razor_inside_what_a_statement_emits() {
        assert_eq!(left("@if (a) { <text>var id = @Model.Id; @if (b) { <text>nested();</text> } </text> }"), "var id = 0 ; nested();");
    }

    #[test]
    fn survives_what_it_cannot_close() {
        assert_eq!(left("a(); @if (open"), "a();");
        assert_eq!(left("a(); @(open"), "a(); @(open");
        assert_eq!(left("a(); @* open"), "a();");
    }

    #[test]
    fn blanks_a_commented_out_block_with_offsets_kept() {
        let source = "a @* <script>x();</script> *@ b";
        let out = blank_razor_comments(source);
        assert_eq!(out.len(), source.len());
        assert!(!out.contains("script"));
        assert_eq!(out.find('b'), source.find(" b").map(|i| i + 1));
    }

    #[test]
    fn a_blanked_non_ascii_character_keeps_its_utf16_width() {
        let out = blank_razor_in_script("a(@Model.Ünï); b();");
        assert_eq!(out.encode_utf16().count(), "a(@Model.Ünï); b();".encode_utf16().count());
    }

    #[test]
    fn is_javascript_block_by_type() {
        let table: &[(&str, bool)] = &[
            ("", true),
            (" asp-location=\"Footer\"", true),
            (" type=\"text/javascript\"", true),
            (" type=\"module\"", true),
            (" type='application/javascript'", true),
            (" type=\"text/babel\"", true),
            (" type=\"text/html\"", false),
            (" type=\"text/template\"", false),
            (" type=\"text/x-jsrender\"", false),
            (" type=\"application/json\"", false),
            (" type=\"application/ld+json\"", false),
            (" type=\"@pageType\"", false),
        ];
        for &(attrs, expected) in table {
            assert_eq!(is_javascript_block(attrs), expected, "{attrs}");
        }
    }
}
