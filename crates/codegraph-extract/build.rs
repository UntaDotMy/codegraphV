//! Compiles the vendored tree-sitter grammars that have no usable crate.
//!
//! The C sources live in `codegraph-kernel/grammars/` for now (the napi kernel
//! this crate absorbs) and move here when the kernel is retired. Revision and
//! sha provenance for each is recorded in `codegraph-kernel/build.rs` and
//! `docs/design/rust-kernel-migration-plan.md`.

use std::path::PathBuf;

fn compile(root: &PathBuf, name: &str, c11: bool, silence_unused_function: bool) {
    let dir = root.join(name);
    let mut c = cc::Build::new();
    if c11 {
        c.std("c11");
    }
    c.include(&dir);
    c.file(dir.join("parser.c"));
    // A grammar without external tokens (solidity) ships no scanner.
    let scanner = dir.join("scanner.c");
    if scanner.exists() {
        c.file(scanner);
    }
    c.flag_if_supported("-Wno-unused-parameter");
    c.flag_if_supported("-Wno-unused-but-set-variable");
    c.flag_if_supported("-Wno-trigraphs");
    if silence_unused_function {
        c.flag_if_supported("-Wno-unused-function");
    }
    c.flag_if_supported("-utf-8"); // msvc
    c.compile(&format!("tree-sitter-{name}"));
    println!("cargo:rerun-if-changed={}", dir.display());
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest.join("..").join("..").join("codegraph-kernel").join("grammars");
    // kotlin: fwcd 0.3.8 (patched scanner); lua: v0.4.1; scala: v0.26.2;
    // dart: UserNobody14 d4d8f3e; swift: crate 0.7.3 src/ with the `try!` scanner fix.
    compile(&root, "kotlin", false, false);
    compile(&root, "lua", false, false);
    compile(&root, "scala", false, false);
    compile(&root, "dart", false, true);
    compile(&root, "swift", true, false);
    // nix: nix-community 3d0173d regenerated with tree-sitter-cli 0.25.10 (ABI 15).
    compile(&root, "nix", false, false);
    // solidity: JoranHonig b239a95 src/ as published (ABI 14) — the commit tree-sitter-wasms 0.1.13 built.
    compile(&root, "solidity", false, false);
    // objc: amaanq tree-sitter-objc 2.1.0 src/ (ABI 14, no scanner) — the npm release
    // tree-sitter-wasms 0.1.13 built its objc wasm from; parser.c is sha-identical in the npm and
    // crates.io tarballs. Native, because that wasm's old `dylink` section can't be loaded.
    compile(&root, "objc", false, false);
    deflate_wasm_grammars(&manifest);
}

/// The grammars the engine loads from the TypeScript engine's own `.wasm` files (see
/// src/wasm_grammars.rs), deflated into OUT_DIR so the binary embeds them small: COBOL's 16 MB
/// table is under 1 MB deflated.
fn deflate_wasm_grammars(manifest: &std::path::Path) {
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let shipped = manifest.join("..").join("..").join("src").join("extraction").join("wasm");
    for (dir, file) in [
        (&shipped, "tree-sitter-arkts.wasm"),
        (&shipped, "tree-sitter-pascal.wasm"),
        (&shipped, "tree-sitter-vbnet.wasm"),
        (&shipped, "tree-sitter-cobol.wasm"),
        (&shipped, "tree-sitter-erlang.wasm"),
        (&shipped, "tree-sitter-terraform.wasm"),
        (&shipped, "tree-sitter-cfml.wasm"),
        (&shipped, "tree-sitter-cfscript.wasm"),
        (&shipped, "tree-sitter-cfquery.wasm"),
    ] {
        let path = dir.join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let packed = miniz_oxide::deflate::compress_to_vec(&bytes, 9);
        std::fs::write(out.join(format!("{file}.deflate")), packed).unwrap();
    }
}
