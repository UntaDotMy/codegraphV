//! The original codegraph's user-visible names in codegraphV's spelling, for comparing
//! codegraphV's output with output recorded from the original TypeScript engine.
//! `scripts/check_names.py` applies the same rules to the source's string literals.

/// The MCP tools, which are `codegraph_<tool>` in the original and `codegraphV_<tool>` here.
const TOOLS: [&str; 10] = ["explore", "node", "search", "callers", "callees", "impact", "files", "status", "context", "trace"];

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// `text` with the original's names renamed: `codegraph` (the command, server and product) →
/// `codegraphV`, `codegraph_<tool>` → `codegraphV_<tool>`, `mcp__codegraph__` →
/// `mcp__codegraphV__`, `.codegraph` (the index folder) → `.codegraphV`, `CODEGRAPH_*` →
/// `CODEGRAPHV_*`, `CodeGraph` → `CodeGraphV`. Paths and package names (`src/codegraph.ts`,
/// `@colbymchenry/codegraph`, `codegraph-cli`, `codegraph.db`) are left alone.
pub fn from_original(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len() + 16);
    let mut i = 0;
    while i < b.len() {
        let rest = &text[i..];
        let prev = i.checked_sub(1).map(|j| b[j]);
        let prev2 = i.checked_sub(2).map(|j| b[j]);
        // In JSON text a `\n`, `\r` or `\t` escape ends the previous word.
        let escape = matches!(prev, Some(b'n' | b'r' | b't')) && prev2 == Some(b'\\');
        if rest.starts_with("mcp__codegraph__") {
            out.push_str("mcp__codegraphV__");
            i += "mcp__codegraph__".len();
            continue;
        }
        if rest.starts_with("CODEGRAPH_") && (escape || !prev.is_some_and(is_word)) {
            out.push_str("CODEGRAPHV_");
            i += "CODEGRAPH_".len();
            continue;
        }
        if rest.starts_with("CodeGraph") && !rest[9..].starts_with('V') {
            out.push_str("CodeGraphV");
            i += 9;
            continue;
        }
        if rest.starts_with("codegraph") {
            let after = &rest[9..];
            let next = after.bytes().next();
            // `</codegraph_context>` closes a tag; `.codegraph` is the index folder.
            let before_ok = escape || match prev {
                None => true,
                Some(b'/') => prev2 == Some(b'<'),
                Some(b'.') => {
                    !prev2.is_some_and(|c| is_word(c) || b"/@-".contains(&c))
                        && next.is_none_or(|c| b"/\\\"`' ".contains(&c))
                }
                // `_codegraph_node ..._`: Markdown italics, not a longer name.
                Some(b'_') => {
                    let prev3 = i.checked_sub(3).map(|j| b[j]);
                    let escaped = matches!(prev2, Some(b'n' | b'r' | b't')) && prev3 == Some(b'\\');
                    escaped || !prev2.is_some_and(is_word)
                }
                Some(c) => !(is_word(c) || b"@-".contains(&c)),
            };
            let tool = after.strip_prefix('_').is_some_and(|t| {
                TOOLS.iter().any(|name| t.starts_with(name) && !t[name.len()..].bytes().next().is_some_and(is_word))
            });
            let bare = !next.is_some_and(|c| is_word(c) || c == b'.' || c == b'-');
            if before_ok && (tool || bare) {
                out.push_str("codegraphV");
                i += 9;
                continue;
            }
        }
        let len = rest.chars().next().map_or(1, char::len_utf8);
        out.push_str(&rest[..len]);
        i += len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::from_original;

    #[test]
    fn renames_the_originals_names_only() {
        for (from, to) in [
            ("make ANOTHER codegraph_explore call", "make ANOTHER codegraphV_explore call"),
            ("Run `codegraph sync` now", "Run `codegraphV sync` now"),
            ("CodeGraph file lock", "CodeGraphV file lock"),
            ("<codegraph_context note=\"x\">y</codegraph_context>", "<codegraphV_context note=\"x\">y</codegraphV_context>"),
            ("allow mcp__codegraph__* and CODEGRAPH_DIR", "allow mcp__codegraphV__* and CODEGRAPHV_DIR"),
            ("see .codegraph/ and `.codegraph`", "see .codegraphV/ and `.codegraphV`"),
            ("[codegraph] ready", "[codegraphV] ready"),
            ("paths.\n\n_codegraph_node any symbol_", "paths.\n\n_codegraphV_node any symbol_"),
            (r#""paths.\n\n_codegraph_node any symbol_""#, r#""paths.\n\n_codegraphV_node any symbol_""#),
            (r#""a\ncodegraph sync\tcodegraph_node""#, r#""a\ncodegraphV sync\tcodegraphV_node""#),
            // Left alone: paths, packages, crates, files, and names already renamed.
            ("src/codegraph.ts @colbymchenry/codegraph codegraph-cli codegraph.db", "src/codegraph.ts @colbymchenry/codegraph codegraph-cli codegraph.db"),
            ("my_codegraph_explore codegraph_core", "my_codegraph_explore codegraph_core"),
            ("codegraphV CodeGraphV codegraphV_node", "codegraphV CodeGraphV codegraphV_node"),
        ] {
            assert_eq!(from_original(from), to, "{from}");
        }
    }
}
