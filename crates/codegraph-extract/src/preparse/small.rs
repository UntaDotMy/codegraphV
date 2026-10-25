//! The small `preParse` hooks: C# conditional compilation, Kotlin syntax newer than
//! the grammar, Swift protocol-composition continuations, Flow's type syntax.
//! Ports of `blankCsharpPreprocessorDirectives` (languages/csharp.ts),
//! `preParseKotlin` (kotlin.ts), `joinSwiftCompositionContinuations` (swift.ts) and
//! `blankFlowSyntax` (typescript.ts). All run on the ASCII shadow.

use super::jsre::Re;
use std::sync::LazyLock;

fn spaces(n: usize) -> String {
    " ".repeat(n)
}

// ---------------------------------------------------------------------------
// C#
// ---------------------------------------------------------------------------

static CSHARP_DIRECTIVE: LazyLock<Re> = LazyLock::new(|| Re::multiline(r"^([ \t]*)#[ \t]*(if|elif|else|endif)\b[^\n]*"));

/// Blank `#if` / `#elif` / `#else` / `#endif` lines, keeping the guarded code.
pub(crate) fn blank_csharp_preprocessor_directives(source: &str) -> String {
    if !source.contains('#') {
        return source.to_string();
    }
    CSHARP_DIRECTIVE.replace_all(source, |c| {
        let whole = c.get(0).unwrap().as_str();
        let indent = c.get(1).unwrap().as_str();
        Some(format!("{indent}{}", spaces(whole.len() - indent.len())))
    })
}

// ---------------------------------------------------------------------------
// Kotlin
// ---------------------------------------------------------------------------

static QUALIFIED_RECEIVER: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"\b(?:[A-Za-z_]\w*(?:<[^<>()\n]*>)?\.)+(?=[A-Za-z_]\w*(?:<[^<>()\n]*>)?\.\()"));
static SPLIT_PRIMARY_CONSTRUCTOR: LazyLock<Re> = LazyLock::new(|| {
    Re::new(
        r"(\bclass\s+[A-Za-z_]\w*(?:\s*<[^<>{}]*(?:<[^<>{}]*>[^<>{}]*)*>)?[ \t]*\r?\n(?:[ \t]*//[^\n]*\n)*[ \t]*)((?:(?:public|private|protected|internal|actual|expect)\s+|@[\w.]+(?:\([^)]*\))?\s+)*constructor)(?=\s*\()",
    )
});
static WHEN_GUARD: LazyLock<Re> =
    LazyLock::new(|| Re::multiline(r#"^([ \t]*!?(?:is|in)[ \t]+(?:(?!->)[^\n])*?[\w>?)\]'"])([ \t]+)if(?=[ \t(])"#));
static MULTI_DOLLAR_STRING: LazyLock<Re> = LazyLock::new(|| Re::new(r#"(?<![\w$"])\$\$+(?=")"#));
static NULLABLE_RECEIVER_FN: LazyLock<Re> = LazyLock::new(|| Re::new(r"([\w>])\?\.\(\)(?=\s*->)"));

fn blank_kotlin_qualified_receivers(source: &str) -> String {
    if !source.contains(".(") {
        return source.to_string();
    }
    QUALIFIED_RECEIVER.replace_all(source, |c| Some(spaces(c.get(0).unwrap().as_str().len())))
}

fn join_kotlin_split_constructors(source: &str) -> String {
    if !source.contains("constructor") {
        return source.to_string();
    }
    SPLIT_PRIMARY_CONSTRUCTOR.replace_all(source, |c| {
        let head = c.get(1).unwrap().as_str();
        let ctor = c.get(2).unwrap().as_str();
        Some(format!("{head}{}", spaces(ctor.len())))
    })
}

fn rewrite_newer_kotlin_syntax(source: &str) -> String {
    let mut out = source.to_string();
    if out.contains(" if") {
        out = WHEN_GUARD.replace_all(&out, |c| {
            let cond = c.get(1).unwrap().as_str();
            let gap = c.get(2).unwrap().as_str();
            Some(format!("{cond},{}", spaces(gap.len() + 1)))
        });
    }
    if out.contains("..<") {
        out = out.replace("..<", ".. ");
    }
    if out.contains("$$") {
        out = MULTI_DOLLAR_STRING.replace_all(&out, |c| Some(spaces(c.get(0).unwrap().as_str().len())));
    }
    if out.contains("?.()") {
        out = NULLABLE_RECEIVER_FN.replace_all(&out, |c| Some(format!("{}.( )", c.get(1).unwrap().as_str())));
    }
    out
}

pub(crate) fn pre_parse_kotlin(source: &str) -> String {
    rewrite_newer_kotlin_syntax(&join_kotlin_split_constructors(&blank_kotlin_qualified_receivers(source)))
}

// ---------------------------------------------------------------------------
// Swift
// ---------------------------------------------------------------------------

static COMPOSITION_HEAD: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"^[ \t]+(?:@[\w.]+(?:\([^)]*\))?\s*)*(?:[a-z]+(?:\([a-z]+\))?\s+)*(?:typealias|let|var)\s"));
static COMPOSITION_CONTINUATION: LazyLock<Re> = LazyLock::new(|| Re::new(r"^([ \t]*)&([ \t]+)(?=\S)"));
static COMPOSITION_ANY: LazyLock<Re> = LazyLock::new(|| Re::new(r"\n[ \t]*&[ \t]"));
static ENDS_ON_TYPE: LazyLock<Re> = LazyLock::new(|| Re::new(r"[\w>)\]?!]$"));

/// Move a protocol composition's line-leading `&` onto the line before it.
pub(crate) fn join_swift_composition_continuations(source: &str) -> String {
    if !COMPOSITION_ANY.is_match(source) {
        return source.to_string();
    }
    let original: Vec<&str> = source.split('\n').collect();
    let mut lines: Vec<String> = original.iter().map(|l| l.to_string()).collect();
    let mut changed = false;
    for i in 1..lines.len() {
        let Some(cont) = COMPOSITION_CONTINUATION.captures(&lines[i]) else { continue };
        let mut head = i - 1;
        while head > 0 && COMPOSITION_CONTINUATION.is_match(original[head]) {
            head -= 1;
        }
        if !COMPOSITION_HEAD.is_match(original[head]) {
            continue;
        }
        let prev = lines[i - 1].clone();
        let (body, cr) = match prev.strip_suffix('\r') {
            Some(b) => (b.to_string(), "\r"),
            None => (prev.clone(), ""),
        };
        // The previous line must end on a type; a trailing comment would swallow the `&`.
        if !ENDS_ON_TYPE.is_match(&body) || body.contains("//") {
            continue;
        }
        let whole_len = cont.get(0).unwrap().end();
        let new_cur = format!("{}{}{}", &cont[1], &cont[2], &lines[i][whole_len..]);
        drop(cont);
        lines[i - 1] = format!("{body}&{cr}");
        lines[i] = new_cur;
        changed = true;
    }
    if changed {
        lines.join("\n")
    } else {
        source.to_string()
    }
}

// ---------------------------------------------------------------------------
// Flow
// ---------------------------------------------------------------------------

static FLOW_OBJ_OPEN: LazyLock<Re> = LazyLock::new(|| Re::new(r"\{\|"));
static FLOW_OBJ_CLOSE: LazyLock<Re> = LazyLock::new(|| Re::new(r"\|\}"));
static FLOW_MAYBE: LazyLock<Re> = LazyLock::new(|| Re::new(r"([:<,]\s*)\?(?=[\w$(\{\[])"));
static FLOW_IMPORT_TYPEOF: LazyLock<Re> = LazyLock::new(|| Re::new(r"\bimport\s+typeof\b"));
static FLOW_OPAQUE: LazyLock<Re> = LazyLock::new(|| Re::new(r"\bopaque(?=\s+type\b)"));
static FLOW_INEXACT: LazyLock<Re> = LazyLock::new(|| Re::new(r"\.\.\.(?=\s*[},])"));

/// Flow's own syntax, blanked to spaces so a Flow file reads as TSX. The caller
/// has already decided the file carries the `@flow` pragma.
pub(crate) fn blank_flow_syntax(source: &str) -> String {
    let s = FLOW_OBJ_OPEN.replace_all(source, |_| Some("{ ".into()));
    let s = FLOW_OBJ_CLOSE.replace_all(&s, |_| Some(" }".into()));
    let s = FLOW_MAYBE.replace_all(&s, |c| Some(format!("{} ", c.get(1).unwrap().as_str())));
    let s = FLOW_IMPORT_TYPEOF.replace_all(&s, |c| {
        let n = c.get(0).unwrap().as_str().len();
        Some(format!("import{}", spaces(n - "import".len())))
    });
    let s = FLOW_OPAQUE.replace_all(&s, |_| Some("      ".into()));
    FLOW_INEXACT.replace_all(&s, |_| Some("   ".into()))
}

/// Whether the file writes `export type *` anywhere: most files don't, and skip the rest.
static EXPORT_TYPE_STAR_HINT: LazyLock<Re> = LazyLock::new(|| Re::new(r"export\s+type\s*\*"));
/// `export type *` where a statement starts (a line's first word, or after `;`, `{`, `}` or the
/// end of a block comment), followed by `from` or `as`.
static EXPORT_TYPE_STAR: LazyLock<Re> =
    LazyLock::new(|| Re::multiline(r"(^|[;{}]|\*/)([ \t]*export\s+)type(?=\s*\*\s*(?:from|as)\b)"));

pub(crate) fn has_export_type_star(source: &str) -> bool {
    EXPORT_TYPE_STAR_HINT.is_match(source)
}

/// TypeScript 5.0's type-only re-export of a whole module, `export type * from './types'` and
/// `export type * as ns from './types'`, with its `type` blanked to spaces so it reads as the
/// `export *` it is: the grammar has no rule for it, and parsed `type` as an error (#2474). Only a
/// statement start is blanked, so a mention in a comment keeps its text.
pub(crate) fn blank_export_type_star(source: &str) -> String {
    if !has_export_type_star(source) {
        return source.to_string();
    }
    EXPORT_TYPE_STAR.replace_all(source, |c| {
        Some(format!("{}{}    ", c.get(1).map_or("", |m| m.as_str()), c.get(2).map_or("", |m| m.as_str())))
    })
}

/// `/\.(?:jsx?|mjs|cjs)$/.test(filePath)`.
pub(crate) fn is_flow_candidate_path(file_path: &str) -> bool {
    [".js", ".jsx", ".mjs", ".cjs"].iter().any(|ext| file_path.ends_with(ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_type_star_blanks_only_statement_starts() {
        let src = "export type * from './a';\nexport type * as ns from './b'; export type * from './c'\n// export type * from './d'\nexport type T = 1;\n";
        let out = blank_export_type_star(src);
        assert_eq!(out.len(), src.len());
        assert_eq!(
            out,
            "export      * from './a';\nexport      * as ns from './b'; export      * from './c'\n// export type * from './d'\nexport type T = 1;\n"
        );
    }

    #[test]
    fn csharp_directives_keep_offsets() {
        let src = "enum E {\n#if X\n  A,\n  #endif\n  B }\n";
        let out = blank_csharp_preprocessor_directives(src);
        assert_eq!(out.len(), src.len());
        assert_eq!(out, "enum E {\n     \n  A,\n        \n  B }\n");
        assert_eq!(blank_csharp_preprocessor_directives("#region x\n"), "#region x\n");
    }

    #[test]
    fn kotlin_newer_syntax() {
        assert_eq!(rewrite_newer_kotlin_syntax("val r = 1..<n"), "val r = 1.. n");
        assert_eq!(
            rewrite_newer_kotlin_syntax("  is H2 if dialect.mode == X ->"),
            "  is H2,   dialect.mode == X ->"
        );
        assert_eq!(rewrite_newer_kotlin_syntax("x: Op<Boolean>?.() -> Unit"), "x: Op<Boolean>.( ) -> Unit");
        assert_eq!(rewrite_newer_kotlin_syntax(r#"val s = $$"a""#), r#"val s =   "a""#);
        assert_eq!(blank_kotlin_qualified_receivers("f: (A.Builder.() -> Unit)"), "f: (  Builder.() -> Unit)");
        assert_eq!(
            join_kotlin_split_constructors("class A\ninternal constructor(x: Int) : B {"),
            "class A\n                    (x: Int) : B {"
        );
    }

    #[test]
    fn swift_composition_moves_the_ampersand_up() {
        let src = "struct S {\n  typealias C = A.Client\n    & B.Client\n}\n";
        let out = join_swift_composition_continuations(src);
        assert_eq!(out, "struct S {\n  typealias C = A.Client&\n     B.Client\n}\n");
        assert_eq!(out.len(), src.len());
    }

    #[test]
    fn flow_syntax_is_blanked() {
        assert_eq!(blank_flow_syntax("type T = {| a: ?string |};"), "type T = {  a:  string  };");
        assert_eq!(blank_flow_syntax("import typeof X from 'y';"), "import        X from 'y';");
        assert_eq!(blank_flow_syntax("opaque type T = number;"), "       type T = number;");
        assert_eq!(blank_flow_syntax("type O = {a: T, ...};"), "type O = {a: T,    };");
    }
}
