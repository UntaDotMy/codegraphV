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
    c.file(dir.join("scanner.c"));
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
}
