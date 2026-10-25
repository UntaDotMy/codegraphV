//! Parse a file with a vendored `.wasm` grammar through tree-sitter's wasmtime store and print the S-expression.
//!   wasm_probe <grammar.wasm> <name> <source-file>
use tree_sitter::{wasmtime::Engine, Parser, WasmStore};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let bytes = std::fs::read(&args[1]).expect("read wasm");
    let source = std::fs::read(&args[3]).expect("read source");
    let engine = Engine::default();
    let mut store = WasmStore::new(&engine).expect("wasm store");
    let language = store.load_language(&args[2], &bytes).expect("load language");
    let mut parser = Parser::new();
    parser.set_wasm_store(store).expect("set store");
    parser.set_language(&language).expect("set language");
    let tree = parser.parse(&source, None).expect("parse");
    println!("{}", tree.root_node().to_sexp());
}
