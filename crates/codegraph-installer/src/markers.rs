//! The marker-fenced instructions block, edited the way the TS installer does
//! (`src/installer/targets/shared.ts`): only the block and the whitespace touching it change,
//! in the file's own line-break style, so uninstall gives back what install found.

/// The file's line break: CRLF when its first line ends with one.
pub fn line_break_of(text: &str) -> &'static str {
    match text.find('\n') {
        Some(nl) if nl > 0 && text.as_bytes()[nl - 1] == b'\r' => "\r\n",
        _ => "\n",
    }
}

pub fn is_blank(text: &str) -> bool {
    text.trim_start_matches('\u{feff}').trim().is_empty()
}

fn with_line_breaks(text: &str, eol: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', eol)
}

fn on_own_line(text: &str, at: usize, len: usize) -> bool {
    let before = text[..at].rsplit('\n').next().unwrap_or("");
    let after = text[at + len..].split('\n').next().unwrap_or("");
    before.chars().all(|c| matches!(c, ' ' | '\t' | '\u{feff}'))
        && after.chars().all(|c| matches!(c, ' ' | '\t' | '\r'))
}

/// Well-formed `start … end` blocks, each marker alone on its line; a stray marker is left alone.
fn sections(text: &str, start: &str, end: &str) -> Vec<(usize, usize)> {
    let mut marks = Vec::new();
    for (marker, is_start) in [(start, true), (end, false)] {
        let mut from = 0;
        while let Some(i) = text[from..].find(marker) {
            let at = from + i;
            if on_own_line(text, at, marker.len()) {
                marks.push((at, is_start));
            }
            from = at + marker.len();
        }
    }
    marks.sort();
    let mut out = Vec::new();
    let mut open = None;
    for (at, is_start) in marks {
        if is_start {
            open = Some(at);
        } else if let Some(s) = open.take() {
            out.push((s, at + end.len()));
        }
    }
    out
}

fn drop_sections(text: &str, sections: &[(usize, usize)], eol: &str) -> String {
    const WS: [char; 4] = [' ', '\t', '\r', '\n'];
    let mut out = text[..sections[0].0].to_owned();
    for (i, &(_, end)) in sections.iter().enumerate() {
        let next = sections.get(i + 1).map_or(text.len(), |s| s.0);
        let left = out.trim_end_matches(WS).to_owned();
        let right = text[end..next].trim_start_matches(WS);
        out = if right.is_empty() {
            if is_blank(&left) { left } else { left + eol }
        } else if is_blank(&left) {
            left + right
        } else {
            format!("{left}{eol}{eol}{right}")
        };
    }
    out
}

/// `text` (None: the file does not exist) with `body` as its one block: an existing block is
/// replaced in place, otherwise the block is appended after a blank line.
pub fn upsert(text: Option<&str>, body: &str, start: &str, end: &str) -> String {
    let Some(text) = text else {
        return format!("{body}\n");
    };
    let found = sections(text, start, end);
    if let Some((&(s, e), duplicates)) = found.split_first() {
        let block = with_line_breaks(body, line_break_of(&text[s..e]));
        let next = format!("{}{block}{}", &text[..s], &text[e..]);
        if duplicates.is_empty() {
            return next;
        }
        let shift = |p: usize| (p + block.len()).wrapping_sub(e - s);
        let moved: Vec<_> = duplicates.iter().map(|&(a, b)| (shift(a), shift(b))).collect();
        return drop_sections(&next, &moved, line_break_of(text));
    }
    let eol = line_break_of(text);
    let block = with_line_breaks(body, eol);
    if is_blank(text) {
        let bom = if text.starts_with('\u{feff}') { "\u{feff}" } else { "" };
        return format!("{bom}{block}{eol}");
    }
    let tail = text.trim_end_matches([' ', '\t', '\r']);
    // Nothing already in the file is trimmed: only line breaks are added, up to a blank line.
    let sep = match tail.strip_suffix('\n') {
        Some(rest) if rest.strip_suffix('\r').unwrap_or(rest).trim_end_matches([' ', '\t']).ends_with('\n') => String::new(),
        Some(_) => eol.to_owned(),
        None => eol.repeat(2),
    };
    format!("{text}{sep}{block}{eol}")
}

/// `text` without its blocks, or None when it has none.
pub fn strip(text: &str, start: &str, end: &str) -> Option<String> {
    let found = sections(text, start, end);
    (!found.is_empty()).then(|| drop_sections(text, &found, line_break_of(text)))
}

#[cfg(test)]
mod tests {
    use super::*;
    const S: &str = "<!-- S -->";
    const E: &str = "<!-- E -->";
    const BODY: &str = "<!-- S -->\nhi\n<!-- E -->";

    #[test]
    fn crlf_file_round_trips() {
        let original = "# Notes\r\n\r\nKeep.\r\n";
        let installed = upsert(Some(original), BODY, S, E);
        assert_eq!(installed, "# Notes\r\n\r\nKeep.\r\n\r\n<!-- S -->\r\nhi\r\n<!-- E -->\r\n");
        assert_eq!(strip(&installed, S, E).unwrap(), original);
    }

    #[test]
    fn replaces_in_place_and_drops_duplicates() {
        let text = "a\n\n<!-- S -->\nold\n<!-- E -->\n\nb\n\n<!-- S -->\nx\n<!-- E -->\n";
        assert_eq!(upsert(Some(text), BODY, S, E), "a\n\n<!-- S -->\nhi\n<!-- E -->\n\nb\n");
    }

    #[test]
    fn blank_and_missing_files() {
        assert_eq!(upsert(None, BODY, S, E), format!("{BODY}\n"));
        assert!(is_blank(&strip(&upsert(None, BODY, S, E), S, E).unwrap()));
        assert_eq!(strip("no block\n", S, E), None);
        assert_eq!(strip("x <!-- S -->\n<!-- E -->\n", S, E), None, "marker not on its own line");
    }
}
