//! Grammars loaded from the TypeScript engine's own `.wasm` files.
//!
//! ArkTS, Pascal, VB.NET, COBOL, Erlang, Terraform and the three CFML grammars have
//! no crate at the revision the TypeScript engine ships, and two of them are patched and
//! regenerated (docs/grammars/). Loading the very `.wasm` files the TypeScript build ships makes
//! the parse tables identical by construction. build.rs embeds them deflated; each is inflated and
//! compiled by wasmtime once per process, the first time a file of its language is parsed
//! (about 0.2–1.3 s), then shared by every thread. A parser that uses one needs a wasm store of its
//! own: see `ts::with_parser`.

use std::sync::OnceLock;
use tree_sitter::wasmtime::Engine;
use tree_sitter::{Language, WasmStore};

struct Grammar {
    /// The codegraph language name, as `grammar_for` takes it.
    language: &'static str,
    /// The name the wasm module exports its language under (`tree_sitter_<export>`).
    export: &'static str,
    packed: &'static [u8],
}

macro_rules! packed {
    ($file:literal) => {
        include_bytes!(concat!(env!("OUT_DIR"), "/", $file, ".deflate"))
    };
}

const GRAMMARS: [Grammar; 9] = [
    Grammar { language: "arkts", export: "arkts", packed: packed!("tree-sitter-arkts.wasm") },
    Grammar { language: "pascal", export: "pascal", packed: packed!("tree-sitter-pascal.wasm") },
    Grammar { language: "vbnet", export: "vbnet", packed: packed!("tree-sitter-vbnet.wasm") },
    Grammar { language: "cobol", export: "COBOL", packed: packed!("tree-sitter-cobol.wasm") },
    Grammar { language: "erlang", export: "erlang", packed: packed!("tree-sitter-erlang.wasm") },
    Grammar { language: "terraform", export: "terraform", packed: packed!("tree-sitter-terraform.wasm") },
    Grammar { language: "cfml", export: "cfml", packed: packed!("tree-sitter-cfml.wasm") },
    Grammar { language: "cfscript", export: "cfscript", packed: packed!("tree-sitter-cfscript.wasm") },
    Grammar { language: "cfquery", export: "cfquery", packed: packed!("tree-sitter-cfquery.wasm") },
];

/// The one wasmtime engine every wasm grammar and store belongs to.
pub fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(Engine::default)
}

/// The wasm grammar for `language`, loaded on first use; `None` for a language without one, or
/// when the module fails to load.
pub fn grammar(language: &str) -> Option<Language> {
    static LOADED: [OnceLock<Option<Language>>; GRAMMARS.len()] = [const { OnceLock::new() }; GRAMMARS.len()];
    let index = GRAMMARS.iter().position(|g| g.language == language)?;
    LOADED[index].get_or_init(|| load(&GRAMMARS[index])).clone()
}

fn load(grammar: &Grammar) -> Option<Language> {
    let bytes = miniz_oxide::inflate::decompress_to_vec(grammar.packed).ok()?;
    // The loading store can go: a wasm language is reference-counted and is instantiated into
    // each parser's own store when that parser is given it.
    let mut store = WasmStore::new(engine()).ok()?;
    store.load_language(grammar.export, &bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_grammar_loads_and_parses_on_several_threads() {
        let samples = [
            ("arkts", "struct Index { build() { Column() {} } }\n"),
            ("pascal", "unit U;\ninterface\nprocedure A;\nimplementation\nprocedure A; begin end;\nend.\n"),
            ("vbnet", "Module M\n  Sub Main()\n  End Sub\nEnd Module\n"),
            ("cobol", "       IDENTIFICATION DIVISION.\n       PROGRAM-ID. HELLO.\n       PROCEDURE DIVISION.\n           STOP RUN.\n"),
            ("erlang", "-module(m).\nf(X) -> X.\n"),
            ("terraform", "resource \"a\" \"b\" {\n  x = 1\n}\n"),
            ("cfml", "<cfset x = 1>\n"),
            ("cfscript", "component { function f() { return 1; } }\n"),
            ("cfquery", "SELECT 1\n"),
        ];
        let handles: Vec<_> = (0..3)
            .map(|_| {
                std::thread::spawn(move || {
                    for (language, source) in samples {
                        let grammar = grammar(language).unwrap_or_else(|| panic!("{language} grammar"));
                        let tree = crate::ts::parse(&grammar, language, source).unwrap_or_else(|e| panic!("{language}: {e}"));
                        assert!(tree.root_node().child_count() > 0, "{language} parsed to nothing");
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn a_language_without_a_wasm_grammar_has_none() {
        assert!(grammar("python").is_none());
    }
}
