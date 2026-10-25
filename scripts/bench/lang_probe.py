"""Index one sample file per README language with the Rust binary and check each yields symbols.

    python scripts/bench/lang_probe.py [--parity]

Writes the samples into a temporary directory, runs `target/release/codegraphV init .` there and
requires every file to be stored under its expected language with at least one node besides the
file node. --parity then runs lang_parity.py on the same samples (TS vs Rust graph diff).
Exits 0 when every language passes (and, with --parity, the graphs are identical).
"""
import argparse
import shutil
import sqlite3
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import bench  # noqa: E402

COBOL = """       IDENTIFICATION DIVISION.
       PROGRAM-ID. GREET.
       PROCEDURE DIVISION.
       MAIN-PARA.
           DISPLAY "HI".
           STOP RUN.
"""

# file name -> (stored language, source)
SAMPLES = {
    'a.ts': ('typescript', 'export function greet(n: string): string { return n; }\n'),
    'a.js': ('javascript', 'function greet(n) { return n; }\n'),
    'a.ets': ('arkts', 'export function greet(): void {}\n'),
    'a.py': ('python', 'def greet(n):\n    return n\n'),
    'a.go': ('go', 'package main\n\nfunc Greet() {}\n'),
    'a.rs': ('rust', 'pub fn greet() {}\n'),
    'Greeter.java': ('java', 'class Greeter { void greet() {} }\n'),
    'a.cs': ('csharp', 'class Greeter { void Greet() {} }\n'),
    'a.php': ('php', '<?php\nfunction greet() {}\n'),
    'a.rb': ('ruby', 'def greet\nend\n'),
    'a.c': ('c', 'int greet(void) { return 0; }\n'),
    'a.cpp': ('cpp', 'class Greeter { public: void greet(); };\n'),
    'a.m': ('objc', '@interface Greeter : NSObject\n- (void)greet;\n@end\n'),
    'a.metal': ('cpp', 'kernel void add(device float* a [[buffer(0)]]) {}\n'),
    'a.cu': ('cpp', '__global__ void add(float* a) {}\n'),
    'a.swift': ('swift', 'func greet() {}\n'),
    'a.kt': ('kotlin', 'fun greet() {}\n'),
    'a.scala': ('scala', 'object Greeter { def greet(): Unit = {} }\n'),
    'a.dart': ('dart', 'void greet() {}\n'),
    'A.svelte': ('svelte', '<script>\n  function greet() {}\n</script>\n<button on:click={greet}>hi</button>\n'),
    'A.vue': ('vue', '<template><div /></template>\n<script setup>\nfunction greet() {}\n</script>\n'),
    'A.astro': ('astro', '---\nfunction greet() {}\n---\n<div>{greet()}</div>\n'),
    'a.liquid': ('liquid', "{% render 'card' %}\n{% assign title = 'x' %}\n"),
    'Greet.pas': ('pascal', 'unit Greet;\ninterface\nprocedure Hello;\nimplementation\nprocedure Hello;\nbegin\nend;\nend.\n'),
    'a.lua': ('lua', 'function greet() end\n'),
    'a.R': ('r', 'greet <- function() {}\n'),
    'a.luau': ('luau', 'function greet() end\n'),
    'Greeter.cfc': ('cfml', 'component {\n  function greet() {}\n}\n'),
    'greet.cbl': ('cobol', COBOL),
    'a.vb': ('vbnet', 'Public Class Greeter\n  Public Sub Greet()\n  End Sub\nEnd Class\n'),
    'greet.erl': ('erlang', '-module(greet).\n-export([hi/0]).\nhi() -> ok.\n'),
    'a.sol': ('solidity', 'pragma solidity ^0.8.0;\ncontract Greeter { function greet() public {} }\n'),
    'a.tf': ('terraform', 'resource "aws_s3_bucket" "b" {\n  bucket = "x"\n}\n'),
    'a.nix': ('nix', '{ greet = x: x; }\n'),
}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    ap.add_argument('--parity', action='store_true')
    a = ap.parse_args()
    work = Path(tempfile.mkdtemp(prefix='cg-probe-'))
    try:
        for name, (_, source) in SAMPLES.items():
            (work / name).write_text(source, encoding='utf8', newline='\n')
        run = subprocess.run([str(bench.rust_bin()), 'init', '.'], cwd=work, capture_output=True, text=True)
        if run.returncode != 0:
            print(run.stdout, run.stderr)
            return 2
        db = sqlite3.connect(work / '.codegraphV' / 'codegraph.db')
        languages = dict(db.execute('select path, language from files'))
        symbols = dict(db.execute("select file_path, count(*) from nodes where kind != 'file' group by file_path"))
        db.close()
        failed = 0
        for name, (language, _) in SAMPLES.items():
            got_lang, count = languages.get(name), symbols.get(name, 0)
            ok = got_lang == language and count > 0
            failed += not ok
            print(f"{'ok  ' if ok else 'FAIL'} {name:14} {str(got_lang):11} {count} symbol(s)")
        print(f'{len(SAMPLES) - failed}/{len(SAMPLES)} languages indexed with symbols')
        if failed:
            return 1
        if a.parity:
            shutil.rmtree(work / '.codegraphV')
            return subprocess.run([sys.executable, str(HERE / 'lang_parity.py'), str(work)]).returncode
        return 0
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == '__main__':
    sys.exit(main())
