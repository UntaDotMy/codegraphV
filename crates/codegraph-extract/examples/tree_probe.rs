//! `tree_probe <lang> <file>` — the file's S-expression parsed as UTF-8 and as
//! UTF-16, after the language's `preParse`. Lets a differential diff on an erroring
//! file be traced to the parser (the wasm runtime reads UTF-16) rather than a walker.

use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (lang, path) = (&args[1], &args[2]);
    let source = std::fs::read_to_string(path).expect("read");
    let language = codegraph_types::Language::parse(lang).expect("language");
    let pre = codegraph_extract::pre_parse(path, &source, language);
    let grammar = codegraph_extract::grammar_for(lang).expect("grammar");
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&grammar).unwrap();
    let utf8 = parser.parse(&pre, None).unwrap();
    let units: Vec<u16> = pre.encode_utf16().collect();
    parser.reset();
    let utf16 = parser.parse_utf16_le(&units, None).unwrap();
    let out = std::io::stdout();
    let mut out = out.lock();
    writeln!(out, "utf8 has_error={} sexp_len={}", utf8.root_node().has_error(), utf8.root_node().to_sexp().len()).unwrap();
    writeln!(out, "utf16 has_error={} sexp_len={}", utf16.root_node().has_error(), utf16.root_node().to_sexp().len()).unwrap();
    writeln!(out, "UTF8:{}", utf8.root_node().to_sexp()).unwrap();
    writeln!(out, "UTF16:{}", utf16.root_node().to_sexp()).unwrap();
}
