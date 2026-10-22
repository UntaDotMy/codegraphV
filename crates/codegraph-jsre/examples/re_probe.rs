//! `re_probe <cases.json> <lines.json>` — runs every regex over every line and prints
//! one canonical result string per regex (see `scripts/rust-golden/regex-diff.mjs`).

use codegraph_jsre::{utf16_len, Re};
use std::io::Write;

fn main() {
    let mut args = std::env::args().skip(1);
    let case_strings = parse_strings(&std::fs::read_to_string(args.next().unwrap()).unwrap());
    let cases: Vec<(String, String)> = case_strings.chunks(2).map(|c| (c[0].clone(), c[1].clone())).collect();
    let lines: Vec<String> = parse_strings(&std::fs::read_to_string(args.next().unwrap()).unwrap());
    let mut out = std::io::BufWriter::new(std::io::stdout());
    for (src, flags) in &cases {
        let re = match Re::try_new(src, flags) {
            Ok(r) => r,
            Err(e) => {
                writeln!(out, "COMPILE-ERROR {}", e.replace('\n', " ")).unwrap();
                continue;
            }
        };
        let mut res = String::new();
        let sticky = flags.contains('y');
        for (li, line) in lines.iter().enumerate() {
            let mut n = 0;
            if sticky {
                if let Some(c) = re.captures_at(line, 0) {
                    push(&mut res, li, line, &c);
                }
                continue;
            }
            for c in re.captures_iter(line) {
                push(&mut res, li, line, &c);
                n += 1;
                if n >= 50 {
                    break;
                }
            }
        }
        writeln!(out, "{}", res).unwrap();
    }
}

fn push(res: &mut String, li: usize, line: &str, c: &codegraph_jsre::Caps) {
    let m = c.get(0).unwrap();
    res.push_str(&format!("{li}:{}:[", utf16_len(&line[..m.start()])));
    for i in 0..c.len() {
        if i > 0 {
            res.push('|');
        }
        match c.text(i) {
            Some(t) => res.push_str(t),
            None => res.push('\u{0}'),
        }
    }
    res.push_str("]\u{1}");
}

/// Every string literal of a JSON array of strings (the probe has no serde dependency).
fn parse_strings(s: &str) -> Vec<String> {
    let c: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        if c[i] == '"' {
            i += 1;
            let mut cur = String::new();
            while c[i] != '"' {
                if c[i] == '\\' {
                    i += 1;
                    match c[i] {
                        'n' => cur.push('\n'),
                        'r' => cur.push('\r'),
                        't' => cur.push('\t'),
                        'b' => cur.push('\u{8}'),
                        'f' => cur.push('\u{c}'),
                        'u' => {
                            let hi = u32::from_str_radix(&c[i + 1..i + 5].iter().collect::<String>(), 16).unwrap();
                            i += 4;
                            if (0xD800..0xDC00).contains(&hi) && c.get(i + 1) == Some(&'\\') && c.get(i + 2) == Some(&'u') {
                                let lo = u32::from_str_radix(&c[i + 3..i + 7].iter().collect::<String>(), 16).unwrap();
                                i += 6;
                                cur.push(char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)).unwrap_or('\u{FFFD}'));
                            } else {
                                cur.push(char::from_u32(hi).unwrap_or('\u{FFFD}'));
                            }
                        }
                        other => cur.push(other),
                    }
                } else {
                    cur.push(c[i]);
                }
                i += 1;
            }
            out.push(cur);
        }
        i += 1;
    }
    out
}
