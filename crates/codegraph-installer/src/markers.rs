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
    checked_sections(text, start, end).unwrap_or_default()
}

pub fn validate(text: &str, start: &str, end: &str) -> Result<(), &'static str> {
    let blocks = checked_sections(text, start, end)?;
    if blocks.len() > 1 { return Err("Multiple instruction blocks: resolve ambiguous ownership before installing or removing"); }
    Ok(())
}

fn checked_sections(text: &str, start: &str, end: &str) -> Result<Vec<(usize, usize)>, &'static str> {
    let mut marks = Vec::new();
    let mut fence: Option<(u8, usize)> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let indentation = line.chars().take_while(|c| *c == ' ').count();
        if indentation >= 4 || line.starts_with('\t') {
            offset += line.len();
            continue;
        }
        let trimmed = line.trim_start_matches([' ', '\t', '\u{feff}']);
        let bytes = trimmed.as_bytes();
        let run = bytes.first().map_or(0, |first| bytes.iter().take_while(|b| *b == first).count());
        if let Some((ch, len)) = fence {
            if bytes.first() == Some(&ch) && run >= len && trimmed[run..].trim().is_empty() {
                fence = None;
            }
        } else if run >= 3 && matches!(bytes.first(), Some(b'`' | b'~')) {
            fence = Some((bytes[0], run));
        } else {
            for (marker, is_start) in [(start, true), (end, false)] {
                if let Some(i) = line.find(marker) {
                    let at = offset + i;
                    if on_own_line(text, at, marker.len()) { marks.push((at, is_start)); }
                }
            }
        }
        offset += line.len();
    }
    if fence.is_some() { return Err("Unclosed Markdown fence: refusing to edit instructions"); }
    marks.sort();
    let mut out = Vec::new();
    let mut open = None;
    for (at, is_start) in marks {
        if is_start {
            if open.is_some() { return Err("Nested instruction markers: refusing to edit instructions"); }
            open = Some(at);
        } else if let Some(s) = open.take() {
            out.push((s, at + end.len()));
        } else {
            return Err("Unmatched instruction end marker: refusing to edit instructions");
        }
    }
    if open.is_some() { return Err("Unclosed instruction marker: refusing to edit instructions"); }
    Ok(out)
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

// Metadata describes only bytes added by this installer, never a backup of user content.
// Keeping it inside the managed block lets reinstall retain exact append boundaries.
const FILE_META: &str = "<!-- codegraphV-file-v1 ";

fn file_meta(block: &str) -> Option<(bool, usize)> {
    let line = block.lines().nth(1)?;
    let payload = line.strip_prefix(FILE_META)?.strip_suffix(" -->")?;
    let (exists, separator) = payload.split_once(':')?;
    let separator: usize = separator.parse().ok()?;
    if separator > 4 { return None; }
    Some((match exists { "existing" => true, "created" => false, _ => return None }, separator))
}

fn with_meta(body: &str, eol: &str, existing: bool, separator: usize) -> String {
    let body = with_line_breaks(body, eol);
    let (first, rest) = body.split_once(eol).unwrap_or((&body, ""));
    let kind = if existing { "existing" } else { "created" };
    format!("{first}{eol}{FILE_META}{kind}:{separator} -->{eol}{rest}")
}

pub fn upsert_preserving(text: Option<&str>, body: &str, start: &str, end: &str) -> String {
    if let Some(text) = text {
        if let Some(&(s, e)) = sections(text, start, end).first() {
            if let Some((existing, separator)) = file_meta(&text[s..e]) {
                let body = with_meta(body, line_break_of(&text[s..e]), existing, separator);
                return format!("{}{body}{}", &text[..s], &text[e..]);
            }
            // Older installations have no append-boundary metadata.
            return upsert(Some(text), body, start, end);
        }
    }
    let before = text.unwrap_or("");
    let eol = line_break_of(before);
    let separator = if before.is_empty() { String::new() }
        else if before.ends_with(&eol.repeat(2)) { String::new() }
        else if before.ends_with(eol) { eol.to_owned() }
        else { eol.repeat(2) };
    let body = with_meta(body, eol, text.is_some(), separator.len());
    format!("{before}{separator}{body}{eol}")
}

/// Outer None means no block; inner None means a newly created file is now empty.
pub fn strip_preserving(text: &str, start: &str, end: &str) -> Option<Option<String>> {
    let found = sections(text, start, end);
    let &(s, e) = found.first()?;
    if let Some((existing, separator)) = file_meta(&text[s..e]) {
        let eol = line_break_of(&text[s..e]);
        let prefix = &text[..s];
        let owned_separator = eol.repeat(separator / eol.len());
        let prefix = prefix.strip_suffix(&owned_separator).unwrap_or(prefix);
        let suffix = text[e..].strip_prefix(eol).unwrap_or(&text[e..]);
        let rest = format!("{prefix}{suffix}");
        return Some((existing || !rest.is_empty()).then_some(rest));
    }
    // Legacy blocks cannot reconstruct a missing final newline or original file existence.
    strip(text, start, end).map(|rest| (!is_blank(&rest)).then_some(rest))
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

    #[test]
    fn fenced_examples_are_preserved() {
        for fence in ["```", "~~~~"] {
            let original = format!("# Example\n{fence}markdown\n{BODY}\n{fence}\n");
            let installed = upsert(Some(&original), BODY, S, E);
            assert!(installed.starts_with(&original));
            assert_eq!(installed.matches(S).count(), 2);
            assert_eq!(upsert(Some(&installed), BODY, S, E), installed);
            assert_eq!(strip(&installed, S, E).unwrap(), original);
        }
    }
}
