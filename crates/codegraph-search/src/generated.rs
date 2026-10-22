//! Generated-file detection for symbol-disambiguation down-ranking.
//!
//! Port of `src/extraction/generated-detection.ts` (it lives here, in the
//! leaf crate, because the store's ranking queries and the indexer both need it
//! and neither may depend on the other).
//!
//! When a query like "Send" matches 17 symbols across protobuf scaffolding,
//! test mocks, and the hand-written implementation, the FTS ranker often
//! surfaces the generated stubs first because their names are identical to the
//! implementation's name. Generated stubs frequently have no body to trace
//! from, so the agent ends up reading source anyway.
//!
//! This is a relevance hint consulted at disambiguation time, NOT a hard filter
//! — generated nodes are still in the graph and remain reachable; they just
//! rank LAST when there's a real implementation with the same name.
//!
//! Two signals, deliberately separate:
//!
//! 1. [`is_generated_file`] — PATH only, pure and cheap. Most generated files
//!    follow the `<basename>.<tool>.<ext>` convention (`.pb.go`, `.g.dart`,
//!    `_pb2.py`).
//! 2. [`has_generated_header`] — CONTENT banner in the file's head. Go's own
//!    convention is a content marker, not a filename one, so a generated
//!    `payroll.go` beside hand-written use-cases is invisible to (1) — that is
//!    issue #1500. Evaluated ONCE at index time and persisted on the file
//!    record as `files.generated`.

use codegraph_types::jscompat::{utf16_len, utf16_prefix, JS_SPACE_CLASS};
use regex::Regex;
use std::sync::LazyLock;

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| panic!("bad regex {pattern}: {e}"))
}

static GENERATED_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // Go — protobuf / gRPC / pulsar
        r"\.pb\.go$",
        r"\.pulsar\.go$",
        r"_grpc\.pb\.go$",
        // Go — mockgen output. Default emits `mock_<src>.go`; many projects
        // (cosmos-sdk uses `expected_*_mocks.go`) rename to `*_mock.go` /
        // `*_mocks.go`.
        r"_mock\.go$",
        r"_mocks\.go$",
        r"^mock_[^/]+\.go$",
        // TypeScript / JavaScript — common codegen suffixes (Apollo / GraphQL
        // codegen, Prisma, Hasura, ts-proto, gRPC-web, swagger-codegen).
        r"\.generated\.[jt]sx?$",
        r"\.gen\.[jt]sx?$",
        r"\.pb\.[jt]s$",
        r"_pb\.[jt]s$",
        r"_grpc_pb\.[jt]s$",
        // Minified bundles vendored into a repo. Their single-letter symbols
        // make name-based edges pure noise.
        r"[.-]min\.m?js$",
        // Python — protobuf / gRPC / openapi-codegen
        r"_pb2(_grpc)?\.py$",
        r"_pb2\.pyi$",
        // C++ — protobuf
        r"\.pb\.(cc|h)$",
        // C# — protobuf / gRPC
        r"\.g\.cs$",
        r"Grpc\.cs$",
        // Java — protobuf / gRPC
        r"OuterClass\.java$",
        r"Grpc\.java$",
        // Swift — protobuf
        r"\.pb\.swift$",
        // Dart — build_runner / freezed / json_serializable / chopper
        r"\.g\.dart$",
        r"\.freezed\.dart$",
        r"\.pb\.dart$",
        r"\.pbgrpc\.dart$",
        r"\.chopper\.dart$",
        // Rust — in-tree generated files often use `*.generated.rs`.
        r"\.generated\.rs$",
    ]
    .iter()
    .map(|p| re(p))
    .collect()
});

/// Whether `file_path` looks like a tool-generated source file based on its
/// filename. Path-only — does not read content. The result is a relevance hint
/// for disambiguation, not a hard claim.
pub fn is_generated_file(file_path: &str) -> bool {
    GENERATED_PATTERNS.iter().any(|p| p.is_match(file_path))
}

// =============================================================================
// Content-header detection (#1500)
// =============================================================================

/// How much of a file's head to consider "the header". Generous enough for a
/// build-tag block + an Apache-2.0 license preamble (~15 lines) sitting above
/// the banner, tight enough that a `"// Code generated ... DO NOT EDIT."`
/// string constant in the *body* of a code generator's own source can't
/// masquerade as a banner.
const HEADER_SCAN_CHARS: usize = 8192;
const HEADER_SCAN_LINES: usize = 60;

/// Cheap pre-filter run on the header of EVERY indexed file. Every marker below
/// contains the stem "generat", so one unanchored scan rejects ~all
/// hand-written source before any line splitting happens.
static GENERATED_STEM: LazyLock<Regex> = LazyLock::new(|| re("(?i)generat"));

/// Line-comment leaders across the languages we index. A banner must sit on a
/// comment line (or inside an open block comment, tracked below): generators
/// always emit theirs as a comment, and requiring it rules out string literals
/// and identifiers that merely contain the words.
static COMMENT_LEADER: LazyLock<Regex> = LazyLock::new(|| {
    re(&format!(
        r#"(?i)^[{JS_SPACE_CLASS}]*(?://|/\*+|\*+/?|#+|--+|<!--|%+|;+|'|!|\(\*|\{{-|"""|'''|=begin|<#|@rem(?-u:\b)|rem(?-u:\b))"#
    ))
});

struct BlockDelim {
    open: &'static str,
    close: &'static str,
}

/// Openers/closers for block comments, so a banner on an unprefixed line inside
/// `/* … */` (or `<!-- … -->`, or a Python module docstring) still counts.
/// Deliberately naive — it only runs over a file's first few dozen lines.
const BLOCK_DELIMS: [BlockDelim; 6] = [
    BlockDelim { open: "/*", close: "*/" },
    BlockDelim { open: "<!--", close: "-->" },
    BlockDelim { open: "\"\"\"", close: "\"\"\"" },
    BlockDelim { open: "'''", close: "'''" },
    BlockDelim { open: "=begin", close: "=end" },
    BlockDelim { open: "<#", close: "#>" },
];

/// A character a JavaScript `.` matches (anything but a line terminator).
const DOT: &str = r"[^\n\r\u{2028}\u{2029}]";

/// The banners themselves. Each is a real convention emitted by a widely-used
/// generator; the list is precision-first, because a false positive silently
/// demotes hand-written code in every ranking path.
static GENERATED_CONTENT_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    let b = r"(?-u:\b)";
    [
        // Go's codified convention — `^// Code generated .* DO NOT EDIT\.$`,
        // honored by gofmt, golangci-lint and GitHub linguist. Emitted by
        // protoc-gen-go, mockgen, sqlc, ent, wire, stringer, and in-house
        // generators like the FKIT CRUD in #1500.
        format!(r"(?i){b}code generated{b}{DOT}{{0,200}}?{b}do not edit{b}"),
        // protoc's Java/C#/Python banner, ANTLR, Dagger, FlatBuffers,
        // rust-bindgen, Xcode asset catalogs, Bazel rules.
        format!(
            r"(?i){b}(?:automatically |auto[- ]?)?generated (?:by|from|with){b}{DOT}{{0,200}}?{b}do not (?:edit|modify|change){b}"
        ),
        // The `@generated` marker: the JS/TS ecosystem's convention (Relay,
        // GraphQL codegen, protobuf-es/Buf, Meta's `@generated SignedSource`).
        // Guarded against `foo@generated` and `@@generated`.
        format!(r"(?:^|[^\p{{L}}\p{{N}}_@])@generated{b}"),
        // .NET's `<auto-generated>` / `<auto-generated />` doc tag.
        format!(r"(?i)<auto-?generated[{JS_SPACE_CLASS}]*/?>"),
        // swagger-codegen / OpenAPI Generator, Thrift, FlatBuffers. "by" is
        // required — bare "automatically generated" appears in hand-written prose.
        format!(r"(?i){b}(?:automatically generated|auto[- ]?generated|autogenerated) by{b}"),
        // The "run this command to regenerate" shape: Cloudflare Wrangler
        // ("Generated by Wrangler by running `wrangler types`"). Bare
        // "generated by" is ordinary prose, so the reproduction instruction is
        // the discriminator: TWO separate "by" clauses.
        format!(r"(?i){b}generated by[{JS_SPACE_CLASS}]+[^{JS_SPACE_CLASS}]{DOT}{{0,80}}?{b}by running{b}"),
        // Self-declaring in-house banners that name no tool.
        format!(r"(?i){b}this (?:file|class|code|module) (?:is|was) (?:auto[- ]?)?generated{b}"),
        // The reverse ordering: "DO NOT EDIT — this is a generated file".
        format!(
            r"(?i){b}do not (?:edit|modify){b}{DOT}{{0,120}}?{b}(?:auto[- ]?generated|generated file|generated code){b}"
        ),
    ]
    .iter()
    .map(|p| re(p))
    .collect()
});

/// Whether the head of `content` carries a recognized machine-generation
/// banner. Bounded to [`HEADER_SCAN_CHARS`] / [`HEADER_SCAN_LINES`], and the
/// marker must sit on a comment line — a generator's own source, which holds
/// the banner as a string constant in its body, is not flagged.
///
/// Called once per file during extraction (content is already in memory), NOT
/// per query: the verdict is persisted on the file record.
pub fn has_generated_header(content: &str) -> bool {
    if content.is_empty() {
        return false;
    }
    let head = utf16_prefix(content, HEADER_SCAN_CHARS);
    // Fast reject for ~every hand-written file: no line splitting, no allocation.
    if !GENERATED_STEM.is_match(head) {
        return false;
    }

    let mut open_block: Option<&BlockDelim> = None;
    for line in head.split('\n').take(HEADER_SCAN_LINES) {
        if (open_block.is_some() || COMMENT_LEADER.is_match(line))
            && GENERATED_CONTENT_PATTERNS.iter().any(|p| p.is_match(line))
        {
            return true;
        }

        // Advance the block-comment state AFTER testing, so the opening line of
        // a `/* Code generated … */` block is itself matched by the leader rule.
        if let Some(block) = open_block {
            if line.contains(block.close) {
                open_block = None;
            }
            continue;
        }
        for delim in &BLOCK_DELIMS {
            let Some(at) = line.find(delim.open) else { continue };
            // Same-line close (`/* … */`, a one-line docstring) leaves no open block.
            if !line[at + delim.open.len()..].contains(delim.close) {
                open_block = Some(delim);
            }
            break;
        }
    }
    false
}

/// The union signal: path convention OR content banner OR minified content.
/// This is what the indexer persists to `files.generated`.
pub fn detect_generated_file(file_path: &str, content: &str) -> bool {
    is_generated_file(file_path) || has_generated_header(content) || is_minified_content(file_path, content)
}

/// Scripts a bundler or minifier writes as a few enormous lines.
static MINIFIABLE_SCRIPT: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\.(?:m?js|cjs)$"));
/// The loader every webpack bundle defines for its modules.
static WEBPACK_RUNTIME: LazyLock<Regex> =
    LazyLock::new(|| re(&format!(r"(?-u:\b)function __webpack_require__[{JS_SPACE_CLASS}]*\(")));
/// A line this long is not written by hand.
const MINIFIED_LINE_CHARS: usize = 1000;

/// A minified or bundled script that is not NAMED so — a docs site's
/// `bundle.js`, a sample's vendored `app.js`: most of its text sits on lines of
/// a thousand characters or more, and that text is code, dense with `;{}(),`,
/// not one long string literal (a base64 fixture) in an ordinary file. Its
/// one-letter functions (`n`, `t`, `e`) otherwise topped the most-depended-on
/// lists of the projects that vendor it.
pub fn is_minified_content(file_path: &str, content: &str) -> bool {
    // UTF-16 length, as the original measures. Bytes bound it from above, so a
    // short byte length settles the question without counting.
    if content.len() < 4 * MINIFIED_LINE_CHARS
        || !MINIFIABLE_SCRIPT.is_match(file_path)
        || utf16_len(content) < 4 * MINIFIED_LINE_CHARS
    {
        return false;
    }
    // A webpack build carries its module loader, however readable its lines.
    if WEBPACK_RUNTIME.is_match(content) {
        return true;
    }
    let total = utf16_len(content);
    let mut long = 0usize;
    let mut punctuation = 0usize;
    for line in content.split('\n') {
        // UTF-16 length never exceeds the byte length.
        if line.len() < MINIFIED_LINE_CHARS {
            continue;
        }
        let units = utf16_len(line);
        if units >= MINIFIED_LINE_CHARS {
            long += units;
            punctuation += line.bytes().filter(|b| matches!(b, b';' | b'{' | b'}' | b'(' | b')' | b',')).count();
        }
    }
    long as f64 >= total as f64 * 0.5 && punctuation as f64 >= long as f64 * 0.03
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_conventions() {
        for p in [
            "api/cosmos/bank/v1beta1/tx_grpc.pb.go", "x/bank/types/tx.pb.go",
            "api/cosmos/bank/v1beta1/tx.pulsar.go", "x/auth/testutil/expected_keepers_mocks.go",
            "internal/foo_mock.go", "mock_keeper.go", "app/foo.generated.ts", "app/foo.generated.tsx",
            "proto/bar_pb2.py", "proto/bar_pb2_grpc.py", "lib/baz.pb.cc", "lib/baz.pb.h",
            "lib/quux.g.dart", "lib/quux.freezed.dart",
        ] {
            assert!(is_generated_file(p), "{p}");
        }
        for p in [
            "x/bank/keeper/msg_server.go", "x/bank/keeper/send.go", "src/index.ts",
            "src/components/Foo.tsx", "lib/main.dart", "cmd/server/main.go", "app/db.py",
        ] {
            assert!(!is_generated_file(p), "{p}");
        }
    }

    #[test]
    fn banners_are_recognized() {
        let generated = [
            "package payroll\n\n// Code generated by fkit. DO NOT EDIT.\n\nimport \"context\"\n\nfunc CreatePayroll(ctx context.Context) error { return nil }\n",
            "// Code generated by protoc-gen-go. DO NOT EDIT.\n// versions:\n//   protoc-gen-go v1.28.0\n\npackage pb\n",
            "//go:build !windows\n// +build !windows\n\n// Code generated by MockGen. DO NOT EDIT.\npackage mocks\n",
            "// Copyright 2021 The Foo Authors.\n// Licensed under the Apache License, Version 2.0 (the \"License\");\n// you may not use this file except in compliance with the License.\n//\n//     http://www.apache.org/licenses/LICENSE-2.0\n//\n// Unless required by applicable law or agreed to in writing, software\n// distributed under the License is distributed on an \"AS IS\" BASIS.\n\n// Code generated by sqlc. DO NOT EDIT.\n// source: query.sql\n\npackage db\n",
            "// Generated by the protocol buffer compiler.  DO NOT EDIT!\n// source: foo.proto\n\npackage com.example;\n",
            "# -*- coding: utf-8 -*-\n# Generated by the protocol buffer compiler.  DO NOT EDIT!\n# source: foo.proto\n",
            "//------------------------------------------------------------------------------\n// <auto-generated>\n//     This code was generated by a tool.\n// </auto-generated>\n//------------------------------------------------------------------------------\n",
            "// <auto-generated />\nusing System;\n",
            "/**\n * @generated SignedSource<<0123456789abcdef0123456789abcdef>>\n * @flow\n */\n",
            "// @generated by protoc-gen-es v1.2.0 with parameter \"target=ts\"\n// @generated from file foo.proto (package example, syntax proto3)\n",
            "/**\n * Autogenerated by Thrift Compiler (0.14.1)\n *\n * DO NOT EDIT UNLESS YOU ARE SURE THAT YOU KNOW WHAT YOU ARE DOING\n */\n",
            "/*\n * Pet Store API\n *\n * NOTE: This class is auto generated by OpenAPI Generator (https://openapi-generator.tech).\n * Do not edit the class manually.\n */\n",
            "// automatically generated by the FlatBuffers compiler, do not modify\n\npackage MyGame;\n",
            "/* automatically generated by rust-bindgen 0.59.2 */\n\npub const FOO: u32 = 1;\n",
            "// Generated from Expr.g4 by ANTLR 4.9.2 -- DO NOT EDIT\npackage parser;\n",
            "/* eslint-disable */\n// Generated by Wrangler by running `wrangler types` (hash: adcfde101dd7d9077590b6b39d3eaf8d)\n// Runtime types generated with workerd@1.20260708.1 2026-07-12\ndeclare namespace Cloudflare {\n\tinterface Env {}\n}\n",
            "# Generated by ./scripts/schema-gen.py by running `make schema`\n\nfrom typing import Any\n",
            "/*\n   Code generated by ent. DO NOT EDIT.\n*/\npackage ent\n",
            "\"\"\"Generated by the protocol buffer compiler.  DO NOT EDIT!\"\"\"\nimport sys\n",
            "# This file is generated by kustomize. Do not edit.\napiVersion: v1\n",
            "-- Code generated by sqlc. DO NOT EDIT.\nCREATE TABLE foo (id INT);\n",
            "<!-- Autogenerated by docgen. Do not edit. -->\n<html></html>\n",
        ];
        for (i, src) in generated.iter().enumerate() {
            assert!(has_generated_header(src), "case {i} should be flagged: {src:?}");
        }
    }

    #[test]
    fn hand_written_source_is_not_flagged() {
        let hand_written = [
            "package keeper\n\nimport \"context\"\n\n// SendCoins moves coins between accounts.\nfunc (k Keeper) SendCoins(ctx context.Context) error { return nil }\n",
            "// This package generates SQL migrations from the schema.\n// The generated output lives under db/migrations.\npackage gen\n",
            "\"\"\"Report builder.\n\nThe summary table is automatically generated at runtime from the\nrows below; callers should not edit it in place.\n\"\"\"\n",
            "package main\n\n// Package main implements the fkit CRUD generator.\n\nimport \"fmt\"\n\nfunc header() string {\n\treturn \"// Code generated by fkit. DO NOT EDIT.\"\n}\n",
            "// Contact: build@generated.example.com for issues.\npackage main\n",
            "// DO NOT EDIT THIS FILE BY HAND — run `make fmt` instead.\npackage main\n",
            "// The table below is generated by the build at runtime, so the\n// literal values here are only a fallback.\npackage main\n",
            "// The nightly summary is generated by running the ETL job against\n// yesterday's partition.\npackage main\n",
            "",
        ];
        for (i, src) in hand_written.iter().enumerate() {
            assert!(!has_generated_header(src), "case {i} should not be flagged: {src:?}");
        }
    }

    #[test]
    fn only_the_header_window_counts() {
        let filler: Vec<String> = (0..80).map(|i| format!("// filler line {i}")).collect();
        let far = format!("{}\n// Code generated by foo. DO NOT EDIT.\npackage main\n", filler.join("\n"));
        assert!(!has_generated_header(&far));
        let near_filler: Vec<String> = (0..20).map(|i| format!("// filler line {i}")).collect();
        let near = format!("{}\n// Code generated by foo. DO NOT EDIT.\npackage main\n", near_filler.join("\n"));
        assert!(has_generated_header(&near));
    }

    #[test]
    fn executable_code_is_not_a_banner() {
        assert!(!has_generated_header("const banner = \"Code generated by tool. DO NOT EDIT.\";\n"));
    }

    #[test]
    fn the_detector_does_not_classify_its_own_pattern_table() {
        // The banner literals above sit well below the header window of this file.
        let this = include_str!("generated.rs");
        assert!(!has_generated_header(this));
    }

    #[test]
    fn minified_content() {
        let line = "a=1;b=2;".repeat(200); // 1600 chars, punctuation-dense
        let content = format!("{line}\n{line}\n{line}\n");
        assert!(is_minified_content("dist/bundle.js", &content));
        assert!(!is_minified_content("dist/bundle.py", &content));
        let prose = format!("{}\n", "word ".repeat(2000));
        assert!(!is_minified_content("a.js", &prose));
        assert!(is_minified_content("a.js", &format!("{}function __webpack_require__ (x){{}}", "\n".repeat(4001))));
        assert!(detect_generated_file("x.pb.go", ""));
    }
}
