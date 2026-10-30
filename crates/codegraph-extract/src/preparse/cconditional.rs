//! `blankCUnbalancedConditionalBranches` (languages/c-cpp.ts, upstream cbd9817f).

use super::cmask::{mask_cpp_comments_and_literals, trim};
use super::conly::{blank_keep_cr, strip_cr};
use super::jsre::Re;
use std::sync::LazyLock;

static COND_OPEN: LazyLock<Re> = LazyLock::new(|| Re::new(r"^[ \t]*#[ \t]*(if|ifdef|ifndef)\b[ \t]*(.*)$"));
static COND_NEXT: LazyLock<Re> = LazyLock::new(|| Re::new(r"^[ \t]*#[ \t]*(elif|else)\b"));
static COND_ELSE: LazyLock<Re> = LazyLock::new(|| Re::new(r"^[ \t]*#[ \t]*else\b"));
static COND_ENDIF: LazyLock<Re> = LazyLock::new(|| Re::new(r"^[ \t]*#[ \t]*endif\b"));
static DIRECTIVE: LazyLock<Re> = LazyLock::new(|| Re::new(r"^[ \t]*#"));
static SPLICE: LazyLock<Re> = LazyLock::new(|| Re::new(r"\\\s*$"));
static LITERAL_ZERO: LazyLock<Re> = LazyLock::new(|| Re::new(r"^0\b"));
static LITERAL_ONE: LazyLock<Re> = LazyLock::new(|| Re::new(r"^1\b"));
static STARTS_ELSE: LazyLock<Re> = LazyLock::new(|| Re::new(r"^else\b"));
static EXPRESSION_START: LazyLock<Re> = LazyLock::new(|| Re::new(r"^(\|\||&&|,|\)|:|\?)"));
static EXPRESSION_END: LazyLock<Re> = LazyLock::new(|| Re::new(r"(\|\||&&|,|\(|\?|:)$"));
static BARE_IF_HEADER: LazyLock<Re> = LazyLock::new(|| Re::new(r"^(else[ \t]+)?if[ \t]*\("));
static ENDS_WITH_PAREN: LazyLock<Re> = LazyLock::new(|| Re::new(r"\)$"));

fn blank_line(lines: &mut [String], masked: &mut [String], k: usize) {
    lines[k] = blank_keep_cr(&lines[k]);
    masked[k] = blank_keep_cr(&masked[k]);
}

/// An open `#if` group: the directive lines that bound its branches.
struct Group {
    /// Line indices of the `#if`, each `#elif` / `#else`, and (once closed) the `#endif`.
    marks: Vec<usize>,
    /// `#if 0`.
    dead: bool,
    /// `#if 0` or `#if 1`.
    literal: bool,
    /// A feature test (`#ifdef X`, `#if defined(X)`, `#if X`), whose `#else` is the default build.
    positive: bool,
    /// Index into `marks` of the `#else`, if any.
    else_at: Option<usize>,
}

/// Open up a `#if … #elif … #else … #endif` group whose branches are not self-contained
/// statements. tree-sitter-c keeps EVERY branch of a conditional group and parses each as a run of
/// block items, so a group is only harmless when each branch is complete on its own. Three shapes
/// in real C are not, and each ends in a phantom function (K&R `if(cond) { … }` reads as an
/// implicit-int function definition named `if`, and the extractor then files it — and every
/// symbol after it — under the enclosing function):
///
///  - a branch that BEGINS with `else` — `} #ifdef X  else if (…) { … } #endif` (a conditional arm
///    of an if-chain);
///  - a branch that ENDS with a bare `if (…)` / `else if (…)` header whose body sits after the
///    `#endif` (the ST HAL's per-device latency tables);
///  - a branch whose braces do not balance — a function signature or a `{` that differs per
///    configuration, with the body shared.
///
/// The group's directive lines are blanked to spaces (newlines and `\r` kept, so offsets
/// survive). When every branch's braces and parentheses balance, all branches stay, read in a row.
/// Otherwise one branch stays verbatim and the rest become spaces too, which is what the
/// preprocessor would hand a compiler for that configuration: the `#else` of a feature test, the
/// first branch of `#ifndef`, the live one of `#if 0` or `#if 1`. Groups whose branches are whole
/// statements — the vast majority — are untouched. This pass edits directive lines on purpose, so
/// it runs AFTER `restore_directive_lines`. Groups are handled innermost-first; an inner group
/// inside a blanked branch simply disappears with it.
pub(crate) fn blank_c_unbalanced_conditional_branches(source: &str) -> String {
    if !source.contains('#') {
        return source.to_string();
    }
    let mut lines: Vec<String> = source.split('\n').map(str::to_string).collect();
    // The same lines with comments and string / char literals as spaces: a `{` in a block comment
    // over several lines (Lua's `** {======` banners) or a `#` line inside one is not code.
    // Blanked along with `lines`, so an outer group reads an inner one as collapsed.
    let mut masked: Vec<String> = mask_cpp_comments_and_literals(source).split('\n').map(str::to_string).collect();
    let mut stack: Vec<Group> = Vec::new();
    // Directive lines, and the lines that continue the directive above them with a `\`.
    let mut directive = vec![false; lines.len()];
    let mut continued = vec![false; lines.len()];
    let mut changed = false;
    let mut continuation = false;
    for i in 0..lines.len() {
        let is_directive = continuation || DIRECTIVE.is_match(strip_cr(&masked[i]));
        directive[i] = is_directive;
        continued[i] = continuation;
        let was_continuation = continuation;
        // Read on the raw line: a `\` that ends a line splices it even inside a comment (a
        // multi-line comment in a `#define` body), and masking would hide it.
        continuation = is_directive && SPLICE.is_match(strip_cr(&lines[i]));
        if !is_directive || was_continuation {
            continue;
        }
        let line = strip_cr(&masked[i]).to_string();
        if let Some(m) = COND_OPEN.captures(&line) {
            let kind = m.get(1).unwrap().as_str().to_string();
            let cond = trim(m.get(2).unwrap().as_str()).to_string();
            drop(m);
            let dead = kind == "if" && LITERAL_ZERO.is_match(&cond);
            // `#if 1` is the other way round from `#if 0`: its first branch is the live one.
            let live = kind == "if" && LITERAL_ONE.is_match(&cond);
            let positive = kind == "ifdef" || (kind == "if" && !dead && !live && !cond.starts_with('!'));
            stack.push(Group { marks: vec![i], dead, literal: dead || live, positive, else_at: None });
            continue;
        }
        let Some(top) = stack.last_mut() else { continue };
        if COND_NEXT.is_match(&line) {
            if COND_ELSE.is_match(&line) {
                top.else_at = Some(top.marks.len());
            }
            top.marks.push(i);
            continue;
        }
        if !COND_ENDIF.is_match(&line) {
            continue;
        }
        let mut top = stack.pop().unwrap();
        top.marks.push(i);
        // Which branch survives: never `#if 0`'s; for a feature test with an `#else`, the `#else`
        // branch — the build without the feature is the one that defines `main`, not `umain` (jq's
        // `#ifdef WIN32`); otherwise the first. A kept branch is verbatim; the others become spaces.
        let keep = if top.dead {
            1
        } else {
            match top.else_at {
                Some(at) if top.positive && at > 0 => at,
                _ => 0,
            }
        };
        if keep >= top.marks.len() - 1 {
            continue; // `#if 0` with nothing to keep
        }
        let mut damaged = false;
        // Whether each branch's braces and parentheses balance on their own.
        let mut balanced = true;
        for b in 0..top.marks.len() - 1 {
            let mut body: Vec<String> = Vec::new();
            for k in top.marks[b] + 1..top.marks[b + 1] {
                if directive[k] {
                    continue; // a `\`-continued directive line
                }
                let c = trim(strip_cr(&masked[k]));
                if !c.is_empty() {
                    body.push(c.to_string());
                }
            }
            let (Some(first), Some(last)) = (body.first(), body.last()) else { continue };
            let mut depth = 0i32;
            let mut parens = 0i32;
            let mut dips = false;
            for l in &body {
                for ch in l.chars() {
                    match ch {
                        '{' => depth += 1,
                        '}' => depth -= 1,
                        '(' => parens += 1,
                        ')' => parens -= 1,
                        _ => {}
                    }
                    if depth < 0 {
                        dips = true; // closes a block it did not open (`} else if (…) {`)
                    }
                }
            }
            if depth != 0 || parens != 0 {
                balanced = false;
            }
            damaged = damaged
                || depth != 0
                || dips
                || STARTS_ELSE.is_match(first)
                // A fragment of an expression: `|| (h4 == NULL)` inside an `if (…)`, `isEmpty(s) ||`
                // before the rest of the condition.
                || EXPRESSION_START.is_match(first)
                || EXPRESSION_END.is_match(last)
                || (BARE_IF_HEADER.is_match(last) && ENDS_WITH_PAREN.is_match(last));
        }
        if !damaged {
            continue;
        }
        // A group on a flag whose branches each balance keeps them all: read in a row they are
        // still C (`} else if (a) {…} else if (b) {…}`, list entries, `|| x || y`), and the parser
        // needed only the directive lines gone. Dropping one would lose real code of a build the
        // project may well use. A branch that opens or closes more than it should, or a literal
        // `#if 0` / `#if 1`, keeps one.
        let keep_all = balanced && !top.literal;
        for b in 0..top.marks.len() - 1 {
            let from = top.marks[b];
            let to = top.marks[b + 1];
            // The directive line (with its `\`-continuations), then — unless this is the kept
            // branch — the branch body. Only its own `\` continuations: a `#define` or `#include`
            // right under it is a line of the branch, kept with a kept branch.
            let mut k = from;
            while k < to && (k == from || continued[k]) {
                blank_line(&mut lines, &mut masked, k);
                k += 1;
            }
            if b == keep || keep_all {
                continue;
            }
            for k in from + 1..to {
                blank_line(&mut lines, &mut masked, k);
            }
        }
        blank_line(&mut lines, &mut masked, i);
        changed = true;
    }
    if changed {
        lines.join("\n")
    } else {
        source.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::blank_c_unbalanced_conditional_branches as open_groups;

    fn lines(s: &str) -> Vec<String> {
        s.split('\n').map(str::to_string).collect()
    }

    fn spaces(s: &str) -> String {
        " ".repeat(s.chars().count())
    }

    #[test]
    fn conditional_group_branches_that_balance_keep_both() {
        let src = [
            "void f(int id)",
            "{",
            "    if (id == 1) {",
            "        a(id);",
            "    }",
            "#if defined(USE_V) || \\",
            "    defined(USE_W)",
            "    else if (id == 2) { // \"{\" in a comment",
            "        b(id);",
            "    }",
            "#else",
            "    else if (id == 3) {",
            "        c(id);",
            "    }",
            "#endif",
            "#ifdef USE_X",
            "    d(id);",
            "#else",
            "    e(id);",
            "#endif",
            "}",
            "#if 0",
            "void dead(void) {",
            "#else",
            "void live(void) {",
            "#endif",
            "}",
        ];
        let joined = src.join("\n");
        let out = open_groups(&joined);
        assert_eq!(out.len(), joined.len());
        let got = lines(&out);
        assert_eq!(got.len(), src.len());
        // The `else`-led group's branches each balance, so both stay, read in a row; only its
        // directive lines, continuation included, become spaces.
        assert_eq!(got[5], spaces(src[5]));
        assert_eq!(got[6], spaces(src[6]));
        assert_eq!(got[7], src[7]);
        assert_eq!(got[8], src[8]);
        assert_eq!(got[10], "     ");
        assert_eq!(got[11], src[11]);
        assert_eq!(got[12], src[12]);
        assert_eq!(got[14], "      ");
        // A balanced group is untouched, directives included.
        assert_eq!(got[15], "#ifdef USE_X");
        assert_eq!(got[16], "    d(id);");
        assert_eq!(got[18], "    e(id);");
        assert_eq!(got[19], "#endif");
        // `#if 0` keeps the live branch instead.
        assert!(!out.contains("dead"));
        assert_eq!(got[24], "void live(void) {");
        // `#if 1` is live as written: its first branch stays, not its `#else`.
        let on = open_groups(
            &["void g(int id) {", "  if (id) { a(); }", "#if 1", "  else if (id == 2) { live(); }", "#else", "  else if (id == 3) { dead(); }", "#endif", "}"].join("\n"),
        );
        assert!(on.contains("else if (id == 2) { live(); }"));
        assert!(!on.contains("dead"));
        // Arms that close the block above and open their own (`} else if (…) {`) balance too:
        // under a flag, both builds' arms stay, so neither one's calls are lost.
        let arms = lines(&open_groups(
            &["void f(int a) {", "  if (a == 1) {", "    one();", "#if ENABLE_X", "  } else if (a == 2) {", "    two();", "#else", "  } else if (a == 3) {", "    three();", "#endif", "  }", "}"]
                .join("\n"),
        ));
        assert_eq!(arms[3], spaces("#if ENABLE_X"));
        assert_eq!(arms[5], "    two();");
        assert_eq!(arms[8], "    three();");
        // A brace inside a block comment is not code: Lua's loadlib.c opens a banner with
        // `** {====` inside a multi-line comment and closes it with `/* }==== */`.
        let lua = [
            "#if defined(LUA_DL_DLOPEN)",
            "static void *ll_load (const char *path) { return dlopen(path); }",
            "#elif defined(LUA_DL_DLL)",
            "/*",
            "** {======================================================",
            "** This is an implementation of loadlib for Windows",
            "** =======================================================",
            "*/",
            "static void *ll_load (const char *path) { return LoadLibraryA(path); }",
            "/* }====================================================== */",
            "#else",
            "static void *ll_load (const char *path) { return NULL; }",
            "#endif",
        ]
        .join("\n");
        assert_eq!(open_groups(&lua), lua);
        // A `\` ending a line inside a block comment still continues the `#define` (jemalloc's
        // tsd.h): the macro body is not code of the group.
        let guarded = [
            "#ifndef GUARD_H",
            "#define GUARD_H",
            "#define O(n)\t\t\t\t\\",
            "static int n##_get(void) {\t\t\\",
            "\t/*\t\t\t\t\t\\",
            "\t * a comment inside the body\t\\",
            "\t */\t\t\t\t\t\\",
            "\treturn 0;\t\t\t\t\\",
            "}",
            "O(a)",
            "#endif",
        ]
        .join("\n");
        assert_eq!(open_groups(&guarded), guarded);
        // No conditional group at all: identity.
        let plain = "int g(void) {\n#define X 1\n    return X;\n}\n";
        assert_eq!(open_groups(plain), plain);
    }

    #[test]
    fn unbalanced_group_keeps_one_branch() {
        // jq's main.c: `umain` for WIN32, `main` otherwise — `main` must survive.
        let heads = ["#ifdef WIN32", "int umain(int argc, char* argv[]) {", "#else /*}*/", "int main(int argc, char* argv[]) {", "#endif", "  return run(argc, argv);", "}"].join("\n");
        let h = lines(&open_groups(&heads));
        assert_eq!(h[1], spaces("int umain(int argc, char* argv[]) {"));
        assert_eq!(h[3], "int main(int argc, char* argv[]) {");
        assert_eq!(h[0], "            ");
        // `#ifndef` is the inverse: its first branch is the default build.
        let inv = heads.replace("#ifdef WIN32", "#ifndef POSIX");
        assert_eq!(lines(&open_groups(&inv))[1], "int umain(int argc, char* argv[]) {");
        // A directive right under the kept branch's own is a line of that branch: only the
        // `#if`/`#else` line itself (and its `\` continuations) goes.
        let with_includes = [
            "#if defined(FEATURE) && \\",
            "    defined(OTHER)",
            "#include \"feature.h\"",
            "int f(int x) {",
            "#else",
            "#include \"plain.h\"",
            "int g(int x) {",
            "#endif",
            "  return x;",
            "}",
        ];
        let inc = lines(&open_groups(&with_includes.join("\n")));
        assert_eq!(inc[1], spaces(with_includes[1]));
        assert_eq!(inc[2], spaces(with_includes[2]));
        assert_eq!(inc[5], "#include \"plain.h\"");
        assert_eq!(inc[6], "int g(int x) {");
    }

    #[test]
    fn expression_fragment_branches_keep_their_only_branch() {
        // A branch that is a fragment of an `if (…)` condition (STM32 HAL), or that opens with
        // `} else if` (betaflight cli.c): directives blanked, the only branch kept, so the
        // statement parses as written for that build.
        let frag = [
            "int check(op_t *h1, op_t *h4)",
            "{",
            "  if ((h1 == NULL)",
            "#if defined(STM32G474xx)",
            "      || (h4 == NULL)",
            "#endif",
            "     )",
            "  {",
            "    return 1;",
            "  }",
            "  if (",
            "#if !defined(USE_FLASH)",
            "      isEmpty(h1) ||",
            "#endif",
            "      strncasecmp(h1, \"rom\", 3) == 0) {",
            "    reboot(1);",
            "#if defined(USE_FLASH)",
            "  } else if (isEmpty(h1)) {",
            "    reboot(2);",
            "#endif",
            "  } else {",
            "    reboot(0);",
            "  }",
            "  return 0;",
            "}",
        ];
        let f = lines(&open_groups(&frag.join("\n")));
        assert_eq!(f[3], spaces("#if defined(STM32G474xx)"));
        assert_eq!(f[4], "      || (h4 == NULL)");
        assert_eq!(f[5], "      ");
        assert_eq!(f[12], "      isEmpty(h1) ||");
        assert_eq!(f[17], "  } else if (isEmpty(h1)) {");
        assert_eq!(f[16], spaces("#if defined(USE_FLASH)"));
    }
}
