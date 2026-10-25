//! COBOL's `preParse` (languages/cobol.ts). Unlike the other hooks it is not offset-preserving: a
//! free-format file is shifted right by 7 columns so the fixed-format grammar reads it, and the
//! walker reads the shifted text, as the TypeScript extractor does.

use codegraph_types::jscompat::JS_SPACE_CLASS;
use regex::Regex;
use std::sync::OnceLock;

fn marker() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let s = format!("[{JS_SPACE_CLASS}]");
        Regex::new(&format!(r"(?i)^([ \t]*)(IDENTIFICATION{s}+DIVISION|ID{s}+DIVISION|PROGRAM-ID(?-u:\b)|[0-9]{{2}}[ \t]+[A-Za-z])")).unwrap()
    })
}

fn sql_include_line() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let s = format!("[{JS_SPACE_CLASS}]");
        Regex::new(&format!(r"(?i)^([ \t]*(?:[0-9]{{6}})?[ \t]+EXEC{s}+SQL{s}+INCLUDE{s}+[A-Za-z0-9$#@-]+{s}+END-EXEC)([ \t]|$)")).unwrap()
    })
}

fn end_exec_period() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(&format!(r"(?i)END-EXEC[{JS_SPACE_CLASS}]*\.")).unwrap())
}

fn end_exec() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)END-EXEC").unwrap())
}

/// terminateSqlIncludes (cobol.ts): a single-line `EXEC SQL INCLUDE x END-EXEC` with no sentence
/// period gets one, written over the character after END-EXEC or appended.
fn terminate_sql_includes(source: &str) -> String {
    source
        .split('\n')
        .map(|l| {
            if !end_exec().is_match(l) || end_exec_period().is_match(l) {
                return l.to_string();
            }
            let Some(m) = sql_include_line().captures(l) else { return l.to_string() };
            let head = m.get(1).unwrap();
            if m.get(2).unwrap().as_str().is_empty() {
                format!("{}.", head.as_str())
            } else {
                format!("{}.{}", head.as_str(), &l[head.end() + 1..])
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn pre_parse_cobol(source: &str) -> String {
    let mut free_format = false;
    for line in source.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(m) = marker().captures(line) {
            free_format = m.get(1).unwrap().as_str().len() < 7;
            break;
        }
    }
    if !free_format {
        return terminate_sql_includes(source);
    }
    // "CGWIDE" in the first line's sequence area relaxes the scanner's column-72 margin.
    let shifted: Vec<String> = source
        .split('\n')
        .enumerate()
        .map(|(i, l)| {
            if i == 0 {
                format!("CGWIDE {l}")
            } else if l.is_empty() {
                String::new()
            } else {
                format!("       {l}")
            }
        })
        .collect();
    terminate_sql_includes(&shifted.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_free_format_file_shifts_right_and_a_fixed_one_keeps_its_columns() {
        assert_eq!(pre_parse_cobol("IDENTIFICATION DIVISION.\nPROGRAM-ID. A.\n\n"), "CGWIDE IDENTIFICATION DIVISION.\n       PROGRAM-ID. A.\n\n");
        let fixed = "       IDENTIFICATION DIVISION.\n";
        assert_eq!(pre_parse_cobol(fixed), fixed);
    }

    #[test]
    fn a_bare_sql_include_gets_its_sentence_period() {
        assert_eq!(terminate_sql_includes("           EXEC SQL INCLUDE SQLCA END-EXEC"), "           EXEC SQL INCLUDE SQLCA END-EXEC.");
        assert_eq!(terminate_sql_includes("           EXEC SQL INCLUDE SQLCA END-EXEC  X"), "           EXEC SQL INCLUDE SQLCA END-EXEC. X");
        let done = "           EXEC SQL INCLUDE SQLCA END-EXEC.";
        assert_eq!(terminate_sql_includes(done), done);
    }
}
