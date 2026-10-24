//! Reading where a call sits on its line: whether anything is written before it as a receiver.

use super::util::*;
use crate::ctx::Ctx;
use crate::types::UnresolvedRef;
use codegraph_jsre::{escape_regex, re, Re};

/// Is the reference written without a receiver (`name(` rather than `x.name(`)?
pub fn has_no_receiver_on_line(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let Some(line) = line_at(ctx, &r.file_path, r.line).or_else(|| {
        let idx = (r.line as usize).checked_sub(1)?;
        ctx.read_file(&r.file_path).and_then(|s| s.split('\n').nth(idx).map(String::from))
    }) else {
        return true;
    };
    let lower = line.to_lowercase();
    let name = r.reference_name.to_lowercase();
    let name_len = codegraph_jsre::utf16_len(&name) as u32;
    let start: Option<usize> = if starts_with_at(&lower, &name, r.column) {
        Some(col_byte(&lower, r.column))
    } else if r.column >= name_len && starts_with_at(&lower, &name, r.column - name_len) {
        Some(col_byte(&lower, r.column - name_len))
    } else {
        Re::cached(&format!(r"(?<![\w$]){}\s*\(", escape_regex(&name)), "").find(&lower).map(|m| m.start())
    };
    let Some(start) = start else { return false };
    !re!(r"\.\s*$").is_match(&lower[..start])
}

/// `path.posix.dirname`.
pub fn posix_dirname(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return if path.starts_with('/') { "/".to_string() } else { ".".to_string() };
    }
    match trimmed.rfind('/') {
        None => ".".to_string(),
        Some(0) => "/".to_string(),
        Some(i) => {
            let head = trimmed[..i].trim_end_matches('/');
            if head.is_empty() {
                "/".to_string()
            } else {
                head.to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::posix_dirname;

    #[test]
    fn dirname_matches_node_posix() {
        assert_eq!(posix_dirname("a/b/c.cs"), "a/b");
        assert_eq!(posix_dirname("c.cs"), ".");
        assert_eq!(posix_dirname("/c.cs"), "/");
        assert_eq!(posix_dirname("a/"), ".");
        assert_eq!(posix_dirname("a/b/"), "a");
        assert_eq!(posix_dirname("."), ".");
        assert_eq!(posix_dirname("a"), ".");
    }
}
