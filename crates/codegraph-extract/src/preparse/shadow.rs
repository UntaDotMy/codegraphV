//! The ASCII shadow the `preParse` ports run on.
//!
//! The TypeScript hooks index strings in UTF-16 units and always replace text
//! with equal-length blanks, so a line, column or offset survives parsing. A Rust
//! `&str` indexes bytes, which diverge from UTF-16 units on any non-ASCII text.
//! Rather than re-derive every offset, the hooks run on a *shadow*: the source
//! with each UTF-16 unit of a non-ASCII character replaced by one placeholder
//! byte. Byte `i` of the shadow is then exactly unit `i` of the original, every
//! regex and scanner can be a verbatim port, and afterwards the untouched
//! placeholders are swapped back for the original units.
//!
//! The placeholder is `0x1F` (matches no `\w` / `\s` / `\b`, and every negated
//! class), except for characters ECMAScript treats as whitespace, which become
//! `\x0B` so `\s` still sees them. (U+2028/U+2029 are line terminators in
//! JavaScript; here they are plain whitespace — see `jsre.rs`.)

use codegraph_types::jscompat::is_js_space;

const FILLER: u8 = 0x1F;
const SPACE_FILLER: u8 = 0x0B;

/// The shadow of `source`: each UTF-16 unit of a non-ASCII character is one
/// placeholder byte, so a byte offset in it is a UTF-16 offset in `source`.
pub(crate) fn shadow_of(source: &str) -> String {
    if source.is_ascii() {
        return source.to_string();
    }
    let mut shadow = String::with_capacity(source.len());
    for c in source.chars() {
        if c.is_ascii() {
            shadow.push(c);
        } else {
            let filler = if is_js_space(c) { SPACE_FILLER } else { FILLER } as char;
            for _ in 0..c.len_utf16() {
                shadow.push(filler);
            }
        }
    }
    shadow
}

/// True for the placeholder bytes [`shadow_of`] writes.
pub(crate) fn has_placeholder(text: &str) -> bool {
    text.bytes().any(is_filler)
}

/// Run `hook` — which must return a string of the same length — over the shadow
/// of `source`, and map the result back to real text.
pub(crate) fn with_shadow(source: &str, hook: impl FnOnce(&str) -> String) -> String {
    if source.is_ascii() {
        return hook(source);
    }
    let mut shadow = String::with_capacity(source.len());
    let mut units: Vec<u16> = Vec::with_capacity(source.len());
    let mut buf = [0u16; 2];
    for c in source.chars() {
        if c.is_ascii() {
            shadow.push(c);
            units.push(c as u16);
        } else {
            let filler = if is_js_space(c) { SPACE_FILLER } else { FILLER } as char;
            for u in c.encode_utf16(&mut buf) {
                shadow.push(filler);
                units.push(*u);
            }
        }
    }
    let out = hook(&shadow);
    let (sb, ob) = (shadow.as_bytes(), out.as_bytes());
    if ob.len() != sb.len() {
        return map_back_shifted(&shadow, &units, &out);
    }
    let rebuilt: Vec<u16> = (0..sb.len())
        .map(|i| if ob[i] == sb[i] { units[i] } else { ob[i] as u16 })
        .collect();
    String::from_utf16_lossy(&rebuilt)
}

/// The one hook that is not length-preserving (C's `va_arg ( … )` rewrite drops the
/// gap before `(`) shifts positions within the lines it touches — and only there:
/// no pass adds or removes a newline. So the output is mapped back line by line.
/// An unshifted line is mapped by position; a shortened line is walked against its
/// shadow, where the only way the two can part is deleted space/tab (anything else
/// that differs is a blank, which the output writes as a space).
fn map_back_shifted(shadow: &str, units: &[u16], out: &str) -> String {
    let shadow_lines: Vec<&str> = shadow.split('\n').collect();
    let out_lines: Vec<&str> = out.split('\n').collect();
    let mut rebuilt: Vec<u16> = Vec::with_capacity(out.len());
    let mut offset = 0usize;
    for (idx, ol) in out_lines.iter().enumerate() {
        // Line counts differ only if a pass ate a newline, which none does; keep
        // the output text as it is rather than guess.
        let Some(sl) = shadow_lines.get(idx) else {
            rebuilt.extend(ol.bytes().map(u16::from));
            rebuilt.push(u16::from(b'\n'));
            continue;
        };
        let (sb, ob) = (sl.as_bytes(), ol.as_bytes());
        let line_units = &units[offset..offset + sb.len()];
        match align_line(sb, ob) {
            Some(map) => {
                rebuilt.extend(ob.iter().zip(&map).map(|(o, &i)| if *o == sb[i] { line_units[i] } else { u16::from(*o) }))
            }
            // Unalignable (a pass blanked the very text a gap was dropped from):
            // a placeholder that survived has no original left to find.
            None => rebuilt.extend(ob.iter().map(|&o| if is_filler(o) { u16::from(b' ') } else { u16::from(o) })),
        }
        rebuilt.push(u16::from(b'\n'));
        offset += sb.len() + 1;
    }
    rebuilt.pop(); // the join's trailing newline
    String::from_utf16_lossy(&rebuilt)
}

fn is_filler(b: u8) -> bool {
    b == FILLER || b == SPACE_FILLER
}

/// For each byte of `out`, the index of the `shadow` byte it came from — or `None`
/// when the line is not `shadow` with only blanks and deleted space/tab applied.
fn align_line(shadow: &[u8], out: &[u8]) -> Option<Vec<usize>> {
    if shadow.len() == out.len() {
        return Some((0..out.len()).collect());
    }
    let mut map = Vec::with_capacity(out.len());
    let mut i = 0;
    for &o in out {
        loop {
            let s = *shadow.get(i)?;
            if s == o || o == b' ' {
                break;
            }
            if s != b' ' && s != b'\t' {
                return None;
            }
            i += 1;
        }
        map.push(i);
        i += 1;
    }
    (i == shadow.len()).then_some(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untouched_text_round_trips() {
        let s = "é // 😀 x\n#if A\n";
        assert_eq!(with_shadow(s, |t| t.to_string()), s);
    }

    #[test]
    fn blanks_land_on_unit_positions() {
        // "é" is 1 unit, "😀" is 2: the blanked `x` and `y` keep their columns.
        let s = "é😀 x y";
        let out = with_shadow(s, |t| t.replace('x', " "));
        assert_eq!(out, "é😀   y");
        let out = with_shadow(s, |t| {
            let mut b = t.as_bytes().to_vec();
            b[0] = b' ';
            String::from_utf8(b).unwrap()
        });
        assert_eq!(out, " 😀 x y");
    }

    #[test]
    fn shortened_lines_keep_their_original_text() {
        // The hook drops the gap in `va_arg (` on line 2 and blanks the `é` on line 1.
        let s = "é x\nva_arg \t(ap, é😀 T)\n\u{A0}y";
        let out = with_shadow(s, |t| {
            let blanked = t.replacen(FILLER as char, " ", 1);
            blanked.replace("va_arg \t(", "va_arg(")
        });
        assert_eq!(out, "  x\nva_arg(ap, é😀 T)\n\u{A0}y");
    }

    #[test]
    fn unalignable_shortened_lines_lose_their_placeholders_not_the_file() {
        // The dropped gap sits inside text the hook then blanked.
        let s = "é\nva_arg  ( é )";
        let out = with_shadow(s, |t| {
            let lines: Vec<&str> = t.split('\n').collect();
            format!("{}\n{}", lines[0], " ".repeat(lines[1].len() - 2))
        });
        assert_eq!(out, format!("é\n{}", " ".repeat(11)));
    }

    #[test]
    fn unicode_spaces_read_as_whitespace() {
        let s = "a\u{A0}b";
        with_shadow(s, |t| {
            assert_eq!(t.as_bytes()[1], SPACE_FILLER);
            t.to_string()
        });
    }
}
