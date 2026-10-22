//! Branch guards — the conditions under which a call site runs.
//! Port of `src/graph/branch-guards.ts`.
//!
//! An edge says `handlePress → openObjectDetail`. What a reader wants to know
//! is that it happens **when `isCollected`** and **not while `isUploading`**.
//! Given a file, its language and a call site, this walks from the innermost
//! node at that position up to the enclosing function boundary and records every
//! branch it passes through: `if` / `else` / `else if`, ternary arms, `switch`
//! cases, the right side of `&&` / `||`, a `catch`, and the early exits that
//! precede the site at each statement block.
//!
//! Nothing is stored in the index. Only what the AST states is reported: a
//! language without rules, or a file that will not parse, yields no label rather
//! than a wrong one.
//!
//! This is the JavaScript/TypeScript rule set. The remaining languages
//! (Swift, Python, Java, Kotlin, C#, Go, C/C++) land next; until then
//! [`supports_branch_guards`] is deliberately narrow, so no caller receives a
//! label this port has not proven.

use codegraph_extract::ts::{parse, Node};
use codegraph_extract::grammar_for;
use codegraph_jsre::{re, utf16_len};

// =============================================================================
// Public shape
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardForm {
    /// `if (x)`.
    If,
    /// `else`.
    Else,
    /// A ternary arm.
    Ternary,
    /// A `switch` case.
    Case,
    /// An early-exit guard: `if (x) return`.
    Guard,
    /// The right side of `&&`.
    And,
    /// The right side of `||`.
    Or,
    /// A `catch` arm.
    Catch,
}

/// How an arm leaves the flow: back to the caller, or by an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardExit {
    Return,
    Throw,
    /// A language's other way out (Go's `panic`, C's `exit`).
    Exit,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BranchGuard {
    /// The condition's source, whitespace-collapsed, outer parens dropped, capped.
    pub text: String,
    /// The site runs when the condition is FALSE (an else arm, an early-return
    /// guard, `||`).
    pub negated: bool,
    pub form: GuardForm,
    /// Line of the condition (1-based).
    pub line: u32,
    /// Where the branching construct starts, `line:column` — the identity of the
    /// FORK rather than of the arm. `""` when the walk could not place it.
    pub branch: String,
    /// How the arm the site is IN leaves, when it always does.
    pub arm_exit: Option<GuardExit>,
    /// For an early exit, how the arm that was NOT taken leaves.
    pub exit: Option<GuardExit>,
}

/// Longest condition text kept before it is cut with an ellipsis.
const MAX_TEXT: usize = 80;

/// Files above this size are not parsed for labels.
pub const MAX_PARSE_BYTES: u64 = 256 * 1024;

/// The call site a label is computed for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    pub line: u32,
    /// 0-based; `None` = the first non-blank column of the line.
    pub column: Option<u32>,
    /// The callee's last segment, when known.
    pub callee: Option<String>,
}

pub fn site_key(site: &CallSite) -> String {
    let mut key = format!(
        "{}:{}",
        site.line,
        site.column.map(|c| c.to_string()).unwrap_or_default()
    );
    if let Some(callee) = &site.callee {
        key.push(':');
        key.push_str(callee);
    }
    key
}

/// The label a rail or a flow connector prints: the conditions in execution
/// order, joined with `&&`, each negated one written as `!x`.
pub fn guard_label(guards: &[BranchGuard]) -> String {
    guards
        .iter()
        .map(render_guard)
        .collect::<Vec<_>>()
        .join(" && ")
}

fn render_guard(g: &BranchGuard) -> String {
    if g.form == GuardForm::Catch {
        return g.text.clone();
    }
    if !g.negated {
        return if has_top_level_or(&g.text) {
            format!("({})", g.text)
        } else {
            g.text.clone()
        };
    }
    let bare_bang = re!(r"^!(?![=])").is_match(&g.text);
    if bare_bang {
        let rest = &g.text[1..];
        if is_simple_operand(rest) {
            return rest.to_owned();
        }
    }
    let not_prefix = re!(r"^not\s+").is_match(&g.text);
    if not_prefix {
        let rest = g.text[4..].trim();
        if is_simple_operand(rest) {
            return rest.to_owned();
        }
    }
    if let Some(flipped) = flip_comparison(&g.text) {
        return flipped;
    }
    if is_simple_operand(&g.text) {
        format!("!{}", g.text)
    } else {
        format!("!({})", g.text)
    }
}

/// `a != b` → `a == b`, `x is None` → `x is not None`; `None` when the text is
/// not one plain comparison.
fn flip_comparison(text: &str) -> Option<String> {
    if re!(r"&&|\|\||\band\b|\bor\b|\?").is_match(text) {
        return None;
    }
    if has_top_level_or(text) {
        return None;
    }
    let m = re!(r"^([^=!<>]+?)\s*(===|!==|==|!=|\bis not\b|\bis\b)\s*([^=!<>]+)$").captures(text)?;
    let op = m.text(2)?;
    let flipped = match op {
        "===" => "!==",
        "!==" => "===",
        "==" => "!=",
        "!=" => "==",
        "is" => "is not",
        "is not" => "is",
        _ => return None,
    };
    Some(format!(
        "{} {} {}",
        m.text(1)?.trim(),
        flipped,
        m.text(3)?.trim()
    ))
}

/// A `||` outside every bracket and string.
fn has_top_level_or(text: &str) -> bool {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if let Some(q) = quote {
            if ch == '\\' {
                i += 2;
                continue;
            }
            if ch == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match ch {
            '\'' | '"' | '`' => quote = Some(ch),
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = (depth - 1).max(0),
            '|' if depth == 0 && chars.get(i + 1) == Some(&'|') => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

fn is_simple_operand(text: &str) -> bool {
    if re!(r"[=<>]").is_match(text) || re!(r"\s(?:&&|\|\||and|or)\s").is_match(text) {
        return false;
    }
    let m = match re!(r"^([\w$.?!]+)(\(.*\))?$", "s").captures(text) {
        Some(m) => m,
        None => return false,
    };
    let Some(call) = m.text(2) else {
        return true;
    };
    if call.is_empty() {
        return true;
    }
    let mut depth = 0i32;
    let len = call.chars().count();
    for (i, ch) in call.chars().enumerate() {
        if ch == '(' {
            depth += 1;
        } else if ch == ')' {
            depth -= 1;
            if depth == 0 && i < len - 1 {
                return false;
            }
        }
    }
    depth == 0
}

/// Languages with walk rules here. Others yield no guards (never a wrong one).
pub fn supports_branch_guards(language: &str) -> bool {
    rules_for(language).is_some()
}

/// The languages with rules here.
pub const BRANCH_GUARD_LANGUAGES: &[&str] = &["typescript", "tsx", "javascript", "jsx"];

// =============================================================================
// Entry points
// =============================================================================

fn first_non_blank_column(source: &str, row: usize) -> usize {
    source
        .split('\n')
        .nth(row)
        .and_then(|line| line.chars().position(|c| !c.is_whitespace()))
        .unwrap_or(0)
}

fn parse_tree<'a>(source: &'a str, language: &str) -> Option<codegraph_extract::ts::Tree> {
    let grammar = grammar_for(language)?;
    codegraph_extract::with_error_tolerance(true, || parse(&grammar, language, source)).ok()
}

/// Guards for many call sites in one file, keyed by [`site_key`]. A language
/// without rules, or a file that cannot be read or parsed, yields an empty map.
pub fn guards_for_file(
    abs_path: &str,
    language: &str,
    sites: &[CallSite],
) -> std::collections::HashMap<String, Vec<BranchGuard>> {
    let mut out = std::collections::HashMap::new();
    if !supports_branch_guards(language) || sites.is_empty() {
        return out;
    }
    let Ok(metadata) = std::fs::metadata(abs_path) else {
        return out;
    };
    if metadata.len() > MAX_PARSE_BYTES {
        return out;
    }
    let Ok(source) = std::fs::read_to_string(abs_path) else {
        return out;
    };
    let Some(tree) = parse_tree(&source, language) else {
        return out;
    };
    let root = tree.root_node();
    for site in sites {
        let key = site_key(site);
        out.entry(key)
            .or_insert_with(|| guards_in_tree(root, &source, language, site.line, site.column));
    }
    out
}

/// [`guards_for_file`] over source text — the test surface.
pub fn guards_in_source(
    source: &str,
    language: &str,
    line: u32,
    column: Option<u32>,
) -> Vec<BranchGuard> {
    if !supports_branch_guards(language) {
        return Vec::new();
    }
    let Some(tree) = parse_tree(source, language) else {
        return Vec::new();
    };
    guards_in_tree(tree.root_node(), source, language, line, column)
}

pub fn guards_in_tree(
    root: Node<'_>,
    source: &str,
    language: &str,
    line: u32,
    column: Option<u32>,
) -> Vec<BranchGuard> {
    if line == 0 {
        return Vec::new();
    }
    let row = (line - 1) as usize;
    let col = column
        .map(|c| c as usize)
        .unwrap_or_else(|| first_non_blank_column(source, row));
    let Some(mut node) = innermost_at(root, row, col) else {
        return Vec::new();
    };
    let Some(rules) = rules_for(language) else {
        return Vec::new();
    };
    let mut found = Vec::new();

    while let Some(parent) = node.parent() {
        if rules.boundaries().contains(&parent.kind()) {
            break;
        }
        if rules.inline_functions().contains(&parent.kind()) {
            let holder = parent.parent().map(|p| p.kind()).unwrap_or("");
            if rules.binding_parents().contains(&holder) {
                break;
            }
            node = parent;
            continue;
        }
        // Anything `enclosing` pushed came from THIS construct, and the arm the
        // site is in is the child the walk came up through: stamp both.
        let before = found.len();
        rules.enclosing(parent, node, source, &mut found);
        for guard in &mut found[before..] {
            if guard.branch.is_empty() {
                guard.branch = branch_key(parent);
            }
            let arm = exit_kind(Some(node), source);
            if arm.is_some() && guard.arm_exit.is_none() {
                guard.arm_exit = arm;
            }
        }
        if rules.blocks().contains(&parent.kind()) {
            rules.early_exits(parent, node, source, &mut found);
        }
        node = parent;
    }
    found.reverse();
    found
}

/// The innermost named node containing (row, col).
fn innermost_at<'t>(root: Node<'t>, row: usize, col: usize) -> Option<Node<'t>> {
    let mut node = root;
    loop {
        let mut next = None;
        for i in 0..node.named_child_count() {
            let Some(child) = node.named_child(i) else {
                continue;
            };
            let start = child.start_position();
            let end = child.end_position();
            let after_start = start.row < row || (start.row == row && start.column <= col);
            let before_end = end.row > row || (end.row == row && end.column > col);
            if after_start && before_end {
                next = Some(child);
                break;
            }
        }
        match next {
            Some(child) => node = child,
            None => return Some(node),
        }
    }
}

// =============================================================================
// Shared rule helpers
// =============================================================================

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.start_byte()..node.end_byte()]
}

fn collapse(text: &str) -> String {
    re!(r"\s+").replace_all_str(text, " ").trim().to_owned()
}

fn cut(text: &str, max: usize) -> String {
    if utf16_len(text) > max {
        format!("{}…", take_utf16(text, max - 1))
    } else {
        text.to_owned()
    }
}

fn take_utf16(s: &str, max: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        let len = c.len_utf16();
        if units + len > max {
            return &s[..i];
        }
        units += len;
    }
    s
}

fn cond_text(node: Option<Node<'_>>, source: &str) -> String {
    let Some(node) = node else {
        return String::new();
    };
    let mut n = node;
    while n.kind() == "parenthesized_expression" && n.named_child_count() == 1 {
        n = n.named_child(0).unwrap();
    }
    let text = collapse(text(n, source));
    // A bare keyword is a grammar's error recovery, never a condition a reader
    // can use.
    if re!(r"^(?:let|var|case|try|await|guard|if|else|some|any)$").is_match(&text) {
        return String::new();
    }
    cut(&text, MAX_TEXT)
}

fn guard(
    form: GuardForm,
    cond: Option<Node<'_>>,
    negated: bool,
    text_override: Option<String>,
    branch: Option<Node<'_>>,
    exit: Option<GuardExit>,
    source: &str,
) -> Option<BranchGuard> {
    let text = text_override.unwrap_or_else(|| cond_text(cond, source));
    if text.is_empty() {
        return None;
    }
    Some(BranchGuard {
        text,
        negated,
        form,
        line: cond.map(|c| c.start_position().row as u32 + 1).unwrap_or(0),
        branch: branch.map(branch_key).unwrap_or_default(),
        arm_exit: None,
        exit,
    })
}

fn branch_key(node: Node<'_>) -> String {
    format!(
        "{}:{}",
        node.start_position().row + 1,
        node.start_position().column
    )
}

fn is_field(parent: Node<'_>, field: &str, child: Node<'_>) -> bool {
    parent
        .child_by_field_name(field)
        .is_some_and(|f| f == child)
}

fn last_named(node: Node<'_>) -> Option<Node<'_>> {
    if node.named_child_count() == 0 {
        None
    } else {
        node.named_child(node.named_child_count() - 1)
    }
}

fn preceding_siblings<'t>(parent: Node<'t>, child: Node<'t>) -> Vec<Node<'t>> {
    let mut out = Vec::new();
    for i in 0..parent.named_child_count() {
        let Some(sibling) = parent.named_child(i) else {
            continue;
        };
        if sibling == child {
            break;
        }
        out.push(sibling);
    }
    out
}

const BLOCKISH: &[&str] = &[
    "statement_block",
    "block",
    "statements",
    "function_body",
    "compound_statement",
    "control_structure_body",
    "else_clause",
    "else_statement",
    "catch_clause",
    "catch_block",
    "except_clause",
    "finally_clause",
];

fn exit_kind(node: Option<Node<'_>>, source: &str) -> Option<GuardExit> {
    let node = node?;
    let kind = node.kind();
    if matches!(kind, "throw_statement" | "raise_statement" | "throw_expression") {
        return Some(GuardExit::Throw);
    }
    if re!(r"^(?:return|break|continue|goto|yield)_statement$").is_match(kind) {
        return Some(GuardExit::Return);
    }
    if matches!(kind, "control_transfer_statement" | "jump_expression") {
        return Some(if re!(r"^\s*throw\b").is_match(text(node, source)) {
            GuardExit::Throw
        } else {
            GuardExit::Return
        });
    }
    if BLOCKISH.contains(&kind) {
        return exit_kind(last_named(node), source);
    }
    None
}

fn exit_kind_or(node: Option<Node<'_>>, exits: bool, source: &str) -> Option<GuardExit> {
    if !exits {
        return None;
    }
    exit_kind(node, source).or(Some(GuardExit::Exit))
}

// =============================================================================
// Language rules
// =============================================================================

trait RuleSet: Sync {
    /// Node types the walk never climbs past: the function the site belongs to.
    fn boundaries(&self) -> &'static [&'static str];
    /// Function-expression types that are boundaries only when named/assigned.
    fn inline_functions(&self) -> &'static [&'static str];
    fn binding_parents(&self) -> &'static [&'static str];
    fn blocks(&self) -> &'static [&'static str];
    /// `parent` encloses `child` (the node the walk came up through).
    fn enclosing(&self, parent: Node<'_>, child: Node<'_>, source: &str, out: &mut Vec<BranchGuard>);
    /// `child` is a statement of block `parent`: record the exits before it.
    fn early_exits(&self, parent: Node<'_>, child: Node<'_>, source: &str, out: &mut Vec<BranchGuard>);
}

fn push(out: &mut Vec<BranchGuard>, guard: Option<BranchGuard>) {
    if let Some(g) = guard {
        out.push(g);
    }
}

fn rules_for(language: &str) -> Option<&'static dyn RuleSet> {
    match language {
        "typescript" | "tsx" | "javascript" | "jsx" => Some(&JS_RULES),
        _ => None,
    }
}

// ----------------------------------------------------------------------- JS --

const JS_EXITS: &[&str] = &[
    "return_statement",
    "throw_statement",
    "break_statement",
    "continue_statement",
];

fn js_always_exits(node: Option<Node<'_>>) -> bool {
    let Some(node) = node else {
        return false;
    };
    if JS_EXITS.contains(&node.kind()) {
        return true;
    }
    if node.kind() == "statement_block" {
        return js_always_exits(last_named(node));
    }
    false
}

struct JsRules;

static JS_RULES: JsRules = JsRules;

impl RuleSet for JsRules {
    fn boundaries(&self) -> &'static [&'static str] {
        &[
            "function_declaration",
            "method_definition",
            "generator_function_declaration",
            "class_declaration",
            "class_body",
            "class",
            "program",
        ]
    }

    fn inline_functions(&self) -> &'static [&'static str] {
        &[
            "arrow_function",
            "function_expression",
            "function",
            "generator_function",
        ]
    }

    fn binding_parents(&self) -> &'static [&'static str] {
        &[
            "variable_declarator",
            "assignment_expression",
            "export_statement",
            "public_field_definition",
            "field_definition",
            "lexical_declaration",
        ]
    }

    fn blocks(&self) -> &'static [&'static str] {
        &["statement_block", "program", "switch_case", "switch_default"]
    }

    fn enclosing(&self, parent: Node<'_>, child: Node<'_>, source: &str, out: &mut Vec<BranchGuard>) {
        match parent.kind() {
            "if_statement" => {
                let cond = parent.child_by_field_name("condition");
                if is_field(parent, "consequence", child) {
                    push(out, guard(GuardForm::If, cond, false, None, None, None, source));
                } else if is_field(parent, "alternative", child) {
                    push(out, guard(GuardForm::Else, cond, true, None, None, None, source));
                }
            }
            "ternary_expression" => {
                let cond = parent.child_by_field_name("condition");
                if is_field(parent, "consequence", child) {
                    push(out, guard(GuardForm::Ternary, cond, false, None, None, None, source));
                } else if is_field(parent, "alternative", child) {
                    push(out, guard(GuardForm::Ternary, cond, true, None, None, None, source));
                }
            }
            "switch_case" | "switch_default" => {
                if parent.kind() == "switch_case" && is_field(parent, "value", child) {
                    return;
                }
                let body = parent.parent();
                let stmt = body.and_then(|b| b.parent());
                let subject = cond_text(stmt.and_then(|s| s.child_by_field_name("value")), source);
                if parent.kind() == "switch_default" {
                    let label = if subject.is_empty() {
                        "default".to_owned()
                    } else {
                        format!("{subject}: default")
                    };
                    push(
                        out,
                        guard(
                            GuardForm::Case,
                            stmt.and_then(|s| s.child_by_field_name("value")),
                            false,
                            Some(label),
                            stmt,
                            None,
                            source,
                        ),
                    );
                } else {
                    let value_node = parent.child_by_field_name("value");
                    let value = cond_text(value_node, source);
                    let label = if subject.is_empty() {
                        value
                    } else {
                        format!("{subject} === {value}")
                    };
                    push(
                        out,
                        guard(GuardForm::Case, value_node, false, Some(label), stmt, None, source),
                    );
                }
            }
            "binary_expression" => {
                if !is_field(parent, "right", child) {
                    return;
                }
                let op = parent
                    .child_by_field_name("operator")
                    .map(|n| text(n, source))
                    .unwrap_or("");
                let left = parent.child_by_field_name("left");
                match op {
                    "&&" => push(out, guard(GuardForm::And, left, false, None, None, None, source)),
                    "||" => push(out, guard(GuardForm::Or, left, true, None, None, None, source)),
                    _ => {}
                }
            }
            "catch_clause" => {
                if !is_field(parent, "parameter", child) {
                    push(
                        out,
                        guard(
                            GuardForm::Catch,
                            None,
                            false,
                            Some("on error".to_owned()),
                            None,
                            None,
                            source,
                        ),
                    );
                }
            }
            _ => {}
        }
    }

    fn early_exits(&self, parent: Node<'_>, child: Node<'_>, source: &str, out: &mut Vec<BranchGuard>) {
        // Outer-most last (the list is reversed once at the end): walk the
        // preceding statements backwards so the FIRST guard in the source ends
        // up first in the final order.
        let before = preceding_siblings(parent, child);
        for statement in before.into_iter().rev() {
            if statement.kind() != "if_statement"
                || statement.child_by_field_name("alternative").is_some()
            {
                continue;
            }
            let body = statement.child_by_field_name("consequence");
            if !js_always_exits(body) {
                continue;
            }
            push(
                out,
                guard(
                    GuardForm::Guard,
                    statement.child_by_field_name("condition"),
                    true,
                    None,
                    Some(statement),
                    exit_kind_or(body, true, source),
                    source,
                ),
            );
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn label(source: &str, line: u32, column: Option<u32>) -> String {
        guard_label(&guards_in_source(source, "typescript", line, column))
    }

    #[test]
    fn reads_an_early_return_and_an_if_in_execution_order() {
        let source = [
            "function go() {",
            "  if (isUploading) return;",
            "  if (isCollected) {",
            "    openObjectDetail(item);",
            "  }",
            "}",
        ]
        .join("\n");
        assert_eq!(label(&source, 4, None), "!isUploading && isCollected");
    }

    #[test]
    fn renders_an_else_arm_negated() {
        let source = [
            "function go(x) {",
            "  if (x) {",
            "    a();",
            "  } else {",
            "    b();",
            "  }",
            "}",
        ]
        .join("\n");
        assert_eq!(label(&source, 5, None), "!x");
    }

    #[test]
    fn renders_ternary_arms() {
        let source = "const y = cond ? a() : b();\n";
        assert_eq!(label(source, 1, Some(18)), "cond");
        assert_eq!(label(source, 1, Some(24)), "!cond");
    }

    #[test]
    fn reads_both_sides_of_a_logical_operator() {
        let source = "function go(a, b) {\n  a && b();\n}\n";
        assert_eq!(label(source, 2, Some(7)), "a");
        let source = "function go(a, b) {\n  a || b();\n}\n";
        assert_eq!(label(source, 2, Some(7)), "!a");
    }

    #[test]
    fn reads_switch_cases_with_the_subject() {
        let source = [
            "function go(kind) {",
            "  switch (kind) {",
            "    case 1:",
            "      a();",
            "      break;",
            "    default:",
            "      b();",
            "  }",
            "}",
        ]
        .join("\n");
        assert_eq!(label(&source, 4, None), "kind === 1");
        assert_eq!(label(&source, 7, None), "kind: default");
    }

    #[test]
    fn reads_a_catch_arm() {
        let source = [
            "function go() {",
            "  try {",
            "    a();",
            "  } catch (e) {",
            "    b();",
            "  }",
            "}",
        ]
        .join("\n");
        assert_eq!(label(&source, 5, None), "on error");
    }

    #[test]
    fn a_named_function_expression_is_its_own_boundary() {
        let source = [
            "function outer() {",
            "  if (x) {",
            "    const inner = function () {",
            "      a();",
            "    };",
            "  }",
            "}",
        ]
        .join("\n");
        assert_eq!(label(&source, 4, None), "");
    }

    #[test]
    fn an_inline_arrow_keeps_the_conditions_around_it() {
        let source = [
            "function outer() {",
            "  if (x) {",
            "    button.onPress(() => {",
            "      a();",
            "    });",
            "  }",
            "}",
        ]
        .join("\n");
        assert_eq!(label(&source, 4, None), "x");
    }

    #[test]
    fn unsupported_languages_yield_no_label() {
        assert!(guards_in_source("if x: a()\n", "ruby", 1, None).is_empty());
        assert!(!supports_branch_guards("ruby"));
        assert!(supports_branch_guards("typescript"));
    }

    #[test]
    fn site_keys_match_the_typescript_shape() {
        assert_eq!(
            site_key(&CallSite {
                line: 4,
                column: Some(7),
                callee: Some("save".into())
            }),
            "4:7:save"
        );
        assert_eq!(
            site_key(&CallSite {
                line: 4,
                column: None,
                callee: None
            }),
            "4:"
        );
    }

    #[test]
    fn flips_one_comparison_instead_of_wrapping_it() {
        let guard = BranchGuard {
            text: "err != nil".into(),
            negated: true,
            form: GuardForm::Guard,
            line: 1,
            branch: String::new(),
            arm_exit: None,
            exit: None,
        };
        assert_eq!(render_guard(&guard), "err == nil");
    }

    #[test]
    fn parenthesises_a_negated_disjunction() {
        let guard = BranchGuard {
            text: "a || b".into(),
            negated: true,
            form: GuardForm::Guard,
            line: 1,
            branch: String::new(),
            arm_exit: None,
            exit: None,
        };
        assert_eq!(render_guard(&guard), "!(a || b)");
    }
}
