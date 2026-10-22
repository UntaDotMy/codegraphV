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
pub const BRANCH_GUARD_LANGUAGES: &[&str] = &[
    "typescript",
    "tsx",
    "javascript",
    "jsx",
    "swift",
    "python",
    "java",
    "kotlin",
    "csharp",
    "go",
    "c",
    "cpp",
    "objc",
];

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
        "swift" => Some(&SWIFT_RULES),
        "python" => Some(&PYTHON_RULES),
        "java" => Some(&JAVA_RULES),
        "kotlin" => Some(&KOTLIN_RULES),
        "csharp" => Some(&CSHARP_RULES),
        "go" => Some(&GO_RULES),
        "c" | "cpp" | "objc" => Some(&C_RULES),
        _ => None,
    }
}

fn named_children<'t>(node: Node<'t>) -> Vec<Node<'t>> {
    (0..node.named_child_count())
        .filter_map(|i| node.named_child(i))
        .collect()
}

fn index_of(parent: Node<'_>, child: Node<'_>) -> Option<usize> {
    (0..parent.child_count()).find(|&i| parent.child(i) == Some(child))
}

fn has_child_kind(node: Node<'_>, kind: &str) -> bool {
    (0..node.child_count()).any(|i| node.child(i).is_some_and(|c| c.kind() == kind))
}

fn first_named_kind<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    named_children(node).into_iter().find(|n| n.kind() == kind)
}

/// A preceding `if` with no else whose body always leaves: an early-return guard.
fn early_if(
    out: &mut Vec<BranchGuard>,
    statement: Node<'_>,
    cond: Option<Node<'_>>,
    text: String,
    body: Option<Node<'_>>,
    negated: bool,
    source: &str,
) {
    push(
        out,
        guard(
            GuardForm::Guard,
            cond,
            negated,
            Some(text),
            Some(statement),
            exit_kind_or(body, true, source),
            source,
        ),
    );
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

// -------------------------------------------------------------------- Swift --

fn swift_always_exits(node: Option<Node<'_>>) -> bool {
    let Some(node) = node else {
        return false;
    };
    if node.kind() == "control_transfer_statement" {
        return true;
    }
    if node.kind() == "statements" {
        return swift_always_exits(last_named(node));
    }
    false
}

/// For a Swift `if`: is `child` after the `else` keyword?
fn after_else(parent: Node<'_>, child: Node<'_>) -> bool {
    let mut seen_else = false;
    for i in 0..parent.child_count() {
        let Some(c) = parent.child(i) else {
            continue;
        };
        if c == child {
            return seen_else;
        }
        if c.kind() == "else" {
            seen_else = true;
        }
    }
    false
}

/// All `condition` fields of a Swift `if`/`guard`, joined — `if let x, y > 0`.
fn swift_conditions<'t>(node: Node<'t>, source: &str) -> (Option<Node<'t>>, String) {
    let parts: Vec<Node<'_>> = (0..node.child_count())
        .filter(|&i| node.field_name_for_child(i) == Some("condition"))
        .filter_map(|i| node.child(i))
        .collect();
    let Some(&first) = parts.first() else {
        return (None, String::new());
    };
    let last = *parts.last().unwrap();
    let text = collapse(&source[first.start_byte()..last.end_byte()]);
    if re!(r"^(?:let|var|case|try|await)$").is_match(&text) {
        return (None, String::new());
    }
    (Some(first), cut(&text, MAX_TEXT))
}

struct SwiftRules;
static SWIFT_RULES: SwiftRules = SwiftRules;

impl RuleSet for SwiftRules {
    fn boundaries(&self) -> &'static [&'static str] {
        &[
            "function_declaration",
            "init_declaration",
            "deinit_declaration",
            "class_declaration",
            "protocol_declaration",
            "computed_property",
            "source_file",
        ]
    }
    fn inline_functions(&self) -> &'static [&'static str] {
        &["lambda_literal"]
    }
    fn binding_parents(&self) -> &'static [&'static str] {
        &["property_declaration", "assignment"]
    }
    fn blocks(&self) -> &'static [&'static str] {
        &["statements", "function_body"]
    }

    fn enclosing(&self, parent: Node<'_>, child: Node<'_>, source: &str, out: &mut Vec<BranchGuard>) {
        match parent.kind() {
            "if_statement" => {
                if index_of(parent, child)
                    .and_then(|i| parent.field_name_for_child(i))
                    == Some("condition")
                {
                    return;
                }
                let (cond, text) = swift_conditions(parent, source);
                let negated = after_else(parent, child);
                push(
                    out,
                    guard(
                        if negated { GuardForm::Else } else { GuardForm::If },
                        cond,
                        negated,
                        Some(text),
                        None,
                        None,
                        source,
                    ),
                );
            }
            "guard_statement" => {
                if index_of(parent, child)
                    .and_then(|i| parent.field_name_for_child(i))
                    == Some("condition")
                {
                    return;
                }
                let (cond, text) = swift_conditions(parent, source);
                push(
                    out,
                    guard(GuardForm::Else, cond, true, Some(text), None, None, source),
                );
            }
            "ternary_expression" => {
                let cond = parent.child_by_field_name("condition");
                if is_field(parent, "if_true", child) {
                    push(out, guard(GuardForm::Ternary, cond, false, None, None, None, source));
                } else if is_field(parent, "if_false", child) {
                    push(out, guard(GuardForm::Ternary, cond, true, None, None, None, source));
                }
            }
            "switch_entry" => {
                let stmt = parent.parent();
                let subject = cond_text(stmt.and_then(|s| s.child_by_field_name("expr")), source);
                let pattern = first_named_kind(parent, "switch_pattern");
                if pattern == Some(child) {
                    return;
                }
                let is_default = has_child_kind(parent, "default_keyword");
                let value = pattern.map(|p| cond_text(Some(p), source)).unwrap_or_default();
                let text = if is_default {
                    if subject.is_empty() {
                        "default".to_owned()
                    } else {
                        format!("{subject}: default")
                    }
                } else if subject.is_empty() {
                    value
                } else {
                    format!("{subject} == {value}")
                };
                push(
                    out,
                    guard(
                        GuardForm::Case,
                        pattern.or_else(|| stmt.and_then(|s| s.child_by_field_name("expr"))),
                        false,
                        Some(text),
                        stmt,
                        None,
                        source,
                    ),
                );
            }
            "catch_block" => push(
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
            ),
            _ => {}
        }
    }

    fn early_exits(&self, parent: Node<'_>, child: Node<'_>, source: &str, out: &mut Vec<BranchGuard>) {
        for s in preceding_siblings(parent, child).into_iter().rev() {
            if s.kind() == "guard_statement" {
                let (cond, text) = swift_conditions(s, source);
                let body = first_named_kind(s, "statements");
                early_if(out, s, cond, text, body, false, source);
            } else if s.kind() == "if_statement" && !has_child_kind(s, "else") {
                let body = first_named_kind(s, "statements");
                if !swift_always_exits(body) {
                    continue;
                }
                let (cond, text) = swift_conditions(s, source);
                early_if(out, s, cond, text, body, true, source);
            }
        }
    }
}

// ------------------------------------------------------------------- Python --

const PY_EXITS: &[&str] = &[
    "return_statement",
    "raise_statement",
    "break_statement",
    "continue_statement",
];

fn py_always_exits(node: Option<Node<'_>>) -> bool {
    let Some(node) = node else {
        return false;
    };
    if PY_EXITS.contains(&node.kind()) {
        return true;
    }
    if node.kind() == "block" {
        return py_always_exits(last_named(node));
    }
    false
}

fn py_operator<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    if let Some(field) = node.child_by_field_name("operator") {
        return text(field, source);
    }
    for i in 0..node.child_count() {
        let Some(c) = node.child(i) else {
            continue;
        };
        if !c.is_named() {
            let t = text(c, source);
            if t == "and" || t == "or" {
                return t;
            }
        }
    }
    ""
}

struct PythonRules;
static PYTHON_RULES: PythonRules = PythonRules;

impl RuleSet for PythonRules {
    fn boundaries(&self) -> &'static [&'static str] {
        &["function_definition", "class_definition", "module"]
    }
    fn inline_functions(&self) -> &'static [&'static str] {
        &["lambda"]
    }
    fn binding_parents(&self) -> &'static [&'static str] {
        &["assignment", "augmented_assignment"]
    }
    fn blocks(&self) -> &'static [&'static str] {
        &["block", "module"]
    }

    fn enclosing(&self, parent: Node<'_>, child: Node<'_>, source: &str, out: &mut Vec<BranchGuard>) {
        match parent.kind() {
            "if_statement" => {
                let cond = parent.child_by_field_name("condition");
                if is_field(parent, "consequence", child) {
                    push(out, guard(GuardForm::If, cond, false, None, None, None, source));
                } else if child.kind() == "elif_clause" || child.kind() == "else_clause" {
                    push(out, guard(GuardForm::Else, cond, true, None, None, None, source));
                    for s in preceding_siblings(parent, child) {
                        if s.kind() == "elif_clause" {
                            push(
                                out,
                                guard(
                                    GuardForm::Else,
                                    s.child_by_field_name("condition"),
                                    true,
                                    None,
                                    None,
                                    None,
                                    source,
                                ),
                            );
                        }
                    }
                }
            }
            "elif_clause" => {
                if is_field(parent, "consequence", child) {
                    push(
                        out,
                        guard(
                            GuardForm::If,
                            parent.child_by_field_name("condition"),
                            false,
                            None,
                            None,
                            None,
                            source,
                        ),
                    );
                }
            }
            "conditional_expression" => {
                let kids = named_children(parent);
                if kids.len() < 3 {
                    return;
                }
                if child == kids[0] {
                    push(out, guard(GuardForm::Ternary, Some(kids[1]), false, None, None, None, source));
                } else if child == kids[2] {
                    push(out, guard(GuardForm::Ternary, Some(kids[1]), true, None, None, None, source));
                }
            }
            "case_clause" => {
                if !is_field(parent, "consequence", child) {
                    return;
                }
                let stmt = parent.parent().and_then(|p| p.parent());
                let subject = cond_text(stmt.and_then(|s| s.child_by_field_name("subject")), source);
                let pattern = first_named_kind(parent, "case_pattern");
                let value = pattern.map(|p| cond_text(Some(p), source)).unwrap_or_default();
                let is_default = value == "_" || value.is_empty();
                let text = if is_default {
                    if subject.is_empty() {
                        "default".to_owned()
                    } else {
                        format!("{subject}: default")
                    }
                } else if subject.is_empty() {
                    value
                } else {
                    format!("{subject} == {value}")
                };
                push(
                    out,
                    guard(GuardForm::Case, pattern, false, Some(text), stmt, None, source),
                );
            }
            "boolean_operator" => {
                if !is_field(parent, "right", child) {
                    return;
                }
                let op = py_operator(parent, source);
                let left = parent.child_by_field_name("left");
                match op {
                    "and" => push(out, guard(GuardForm::And, left, false, None, None, None, source)),
                    "or" => push(out, guard(GuardForm::Or, left, true, None, None, None, source)),
                    _ => {}
                }
            }
            "except_clause" | "except_group_clause" => {
                if child.kind() == "block" {
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
        for s in preceding_siblings(parent, child).into_iter().rev() {
            if s.kind() != "if_statement" {
                continue;
            }
            if named_children(s)
                .iter()
                .any(|n| n.kind() == "elif_clause" || n.kind() == "else_clause")
            {
                continue;
            }
            let body = s.child_by_field_name("consequence");
            if !py_always_exits(body) {
                continue;
            }
            let cond = s.child_by_field_name("condition");
            early_if(out, s, cond, cond_text(cond, source), body, true, source);
        }
    }
}

// --------------------------------------------------------------------- Java --

const JAVA_EXITS: &[&str] = &[
    "return_statement",
    "throw_statement",
    "break_statement",
    "continue_statement",
    "yield_statement",
];

fn java_always_exits(node: Option<Node<'_>>) -> bool {
    let Some(node) = node else {
        return false;
    };
    if JAVA_EXITS.contains(&node.kind()) {
        return true;
    }
    if node.kind() == "block" {
        return java_always_exits(last_named(node));
    }
    false
}

fn java_case_text(labels: &[Node<'_>], subject: &str, source: &str) -> String {
    let values: Vec<String> = labels
        .iter()
        .map(|label| {
            named_children(*label)
                .iter()
                .map(|n| cond_text(Some(*n), source))
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|v| !v.is_empty())
        .collect();
    if values.is_empty() {
        return if subject.is_empty() {
            "default".to_owned()
        } else {
            format!("{subject}: default")
        };
    }
    let value = values.join(", ");
    if subject.is_empty() {
        value
    } else {
        format!("{subject} == {value}")
    }
}

struct JavaRules;
static JAVA_RULES: JavaRules = JavaRules;

impl RuleSet for JavaRules {
    fn boundaries(&self) -> &'static [&'static str] {
        &[
            "method_declaration",
            "constructor_declaration",
            "class_declaration",
            "class_body",
            "interface_declaration",
            "enum_declaration",
            "record_declaration",
            "program",
        ]
    }
    fn inline_functions(&self) -> &'static [&'static str] {
        &["lambda_expression"]
    }
    fn binding_parents(&self) -> &'static [&'static str] {
        &["variable_declarator", "assignment_expression", "field_declaration"]
    }
    fn blocks(&self) -> &'static [&'static str] {
        &["block", "switch_block_statement_group", "program", "constructor_body"]
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
            "switch_block_statement_group" | "switch_rule" => {
                if child.kind() == "switch_label" {
                    return;
                }
                let stmt = parent.parent().and_then(|p| p.parent());
                let subject = cond_text(stmt.and_then(|s| s.child_by_field_name("condition")), source);
                let labels: Vec<Node<'_>> = named_children(parent)
                    .into_iter()
                    .filter(|n| n.kind() == "switch_label")
                    .collect();
                let text = java_case_text(&labels, &subject, source);
                push(
                    out,
                    guard(
                        GuardForm::Case,
                        labels.first().copied(),
                        false,
                        Some(text),
                        stmt,
                        None,
                        source,
                    ),
                );
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
                if is_field(parent, "body", child) {
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
        for s in preceding_siblings(parent, child).into_iter().rev() {
            if s.kind() != "if_statement" || s.child_by_field_name("alternative").is_some() {
                continue;
            }
            let body = s.child_by_field_name("consequence");
            if !java_always_exits(body) {
                continue;
            }
            let cond = s.child_by_field_name("condition");
            early_if(out, s, cond, cond_text(cond, source), body, true, source);
        }
    }
}

// ------------------------------------------------------------------- Kotlin --

fn kt_always_exits(node: Option<Node<'_>>) -> bool {
    let Some(node) = node else {
        return false;
    };
    if node.kind() == "jump_expression" {
        return true;
    }
    if node.kind() == "control_structure_body" || node.kind() == "statements" {
        return kt_always_exits(last_named(node));
    }
    false
}

fn kt_arms(if_expr: Node<'_>) -> Vec<Node<'_>> {
    named_children(if_expr)
        .into_iter()
        .filter(|n| n.kind() == "control_structure_body")
        .collect()
}

fn strip_outer_parens(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('(') && s.ends_with(')') {
        s[1..s.len() - 1].to_owned()
    } else {
        s.to_owned()
    }
}

struct KotlinRules;
static KOTLIN_RULES: KotlinRules = KotlinRules;

impl RuleSet for KotlinRules {
    fn boundaries(&self) -> &'static [&'static str] {
        &[
            "function_declaration",
            "secondary_constructor",
            "class_declaration",
            "class_body",
            "object_declaration",
            "getter",
            "setter",
            "source_file",
        ]
    }
    fn inline_functions(&self) -> &'static [&'static str] {
        &["lambda_literal", "anonymous_function"]
    }
    fn binding_parents(&self) -> &'static [&'static str] {
        &["property_declaration", "assignment"]
    }
    fn blocks(&self) -> &'static [&'static str] {
        &["statements", "function_body", "source_file"]
    }

    fn enclosing(&self, parent: Node<'_>, child: Node<'_>, source: &str, out: &mut Vec<BranchGuard>) {
        match parent.kind() {
            "if_expression" => {
                let kids = named_children(parent);
                let cond = kids.first().copied();
                if cond == Some(child) {
                    return;
                }
                let arms = kt_arms(parent);
                if arms.first() == Some(&child) {
                    push(out, guard(GuardForm::If, cond, false, None, None, None, source));
                } else if arms.get(1) == Some(&child) {
                    push(out, guard(GuardForm::Else, cond, true, None, None, None, source));
                }
            }
            "when_entry" => {
                if child.kind() != "control_structure_body" {
                    return;
                }
                let when = parent.parent();
                let subject = when
                    .map(|w| {
                        strip_outer_parens(&cond_text(first_named_kind(w, "when_subject"), source))
                    })
                    .unwrap_or_default();
                let conds: Vec<Node<'_>> = named_children(parent)
                    .into_iter()
                    .filter(|n| n.kind() == "when_condition")
                    .collect();
                if conds.is_empty() {
                    let text = if subject.is_empty() {
                        "else".to_owned()
                    } else {
                        format!("{subject}: else")
                    };
                    push(out, guard(GuardForm::Case, None, false, Some(text), when, None, source));
                } else {
                    let value = conds
                        .iter()
                        .map(|c| cond_text(Some(*c), source))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let text = if subject.is_empty() {
                        value
                    } else {
                        format!("{subject} == {value}")
                    };
                    push(
                        out,
                        guard(
                            GuardForm::Case,
                            conds.first().copied(),
                            false,
                            Some(text),
                            when,
                            None,
                            source,
                        ),
                    );
                }
            }
            "conjunction_expression" | "disjunction_expression" => {
                let kids = named_children(parent);
                if kids.len() < 2 || *kids.last().unwrap() != child {
                    return;
                }
                let left = kids[0];
                if parent.kind() == "conjunction_expression" {
                    push(out, guard(GuardForm::And, Some(left), false, None, None, None, source));
                } else {
                    push(out, guard(GuardForm::Or, Some(left), true, None, None, None, source));
                }
            }
            "catch_block" => {
                if child.kind() == "statements" {
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
        for s in preceding_siblings(parent, child).into_iter().rev() {
            if s.kind() != "if_expression" {
                continue;
            }
            let arms = kt_arms(s);
            if arms.len() <= 1 {
                continue;
            }
            let body = arms.first().copied();
            if !kt_always_exits(body) {
                continue;
            }
            let cond = named_children(s).first().copied();
            early_if(out, s, cond, cond_text(cond, source), body, true, source);
        }
    }
}

// ----------------------------------------------------------------------- C# --

const CS_EXITS: &[&str] = &[
    "return_statement",
    "throw_statement",
    "break_statement",
    "continue_statement",
];

fn cs_always_exits(node: Option<Node<'_>>) -> bool {
    let Some(node) = node else {
        return false;
    };
    if CS_EXITS.contains(&node.kind()) {
        return true;
    }
    if node.kind() == "block" {
        return cs_always_exits(last_named(node));
    }
    false
}

fn is_cs_switch_label(node: Node<'_>) -> bool {
    node.kind().ends_with("pattern") || node.kind().ends_with("switch_label")
}

struct CSharpRules;
static CSHARP_RULES: CSharpRules = CSharpRules;

impl RuleSet for CSharpRules {
    fn boundaries(&self) -> &'static [&'static str] {
        &[
            "method_declaration",
            "constructor_declaration",
            "local_function_statement",
            "class_declaration",
            "struct_declaration",
            "record_declaration",
            "interface_declaration",
            "declaration_list",
            "property_declaration",
            "accessor_declaration",
            "compilation_unit",
        ]
    }
    fn inline_functions(&self) -> &'static [&'static str] {
        &["lambda_expression", "anonymous_method_expression"]
    }
    fn binding_parents(&self) -> &'static [&'static str] {
        &["variable_declarator", "assignment_expression", "equals_value_clause"]
    }
    fn blocks(&self) -> &'static [&'static str] {
        &["block", "switch_section", "compilation_unit"]
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
            "conditional_expression" => {
                let cond = parent.child_by_field_name("condition");
                if is_field(parent, "consequence", child) {
                    push(out, guard(GuardForm::Ternary, cond, false, None, None, None, source));
                } else if is_field(parent, "alternative", child) {
                    push(out, guard(GuardForm::Ternary, cond, true, None, None, None, source));
                }
            }
            "switch_section" => {
                if is_cs_switch_label(child) {
                    return;
                }
                let stmt = parent.parent().and_then(|p| p.parent());
                let subject = cond_text(stmt.and_then(|s| s.child_by_field_name("value")), source);
                let labels: Vec<Node<'_>> = named_children(parent)
                    .into_iter()
                    .filter(|n| is_cs_switch_label(*n))
                    .collect();
                let value = labels
                    .iter()
                    .map(|l| cond_text(Some(*l), source))
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ");
                let text = if value.is_empty() {
                    if subject.is_empty() {
                        "default".to_owned()
                    } else {
                        format!("{subject}: default")
                    }
                } else if subject.is_empty() {
                    value
                } else {
                    format!("{subject} == {value}")
                };
                push(
                    out,
                    guard(
                        GuardForm::Case,
                        labels.first().copied(),
                        false,
                        Some(text),
                        stmt,
                        None,
                        source,
                    ),
                );
            }
            "switch_expression_arm" => {
                if !is_field(parent, "expression", child) {
                    return;
                }
                let stmt = parent.parent();
                let subject = cond_text(stmt.and_then(|s| s.child_by_field_name("value")), source);
                let pattern = parent.child_by_field_name("pattern");
                let value = cond_text(pattern, source);
                let text = if value == "_" || value.is_empty() {
                    if subject.is_empty() {
                        "default".to_owned()
                    } else {
                        format!("{subject}: default")
                    }
                } else if subject.is_empty() {
                    value
                } else {
                    format!("{subject} == {value}")
                };
                push(
                    out,
                    guard(GuardForm::Case, pattern, false, Some(text), stmt, None, source),
                );
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
                if is_field(parent, "body", child) {
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
        for s in preceding_siblings(parent, child).into_iter().rev() {
            if s.kind() != "if_statement" || s.child_by_field_name("alternative").is_some() {
                continue;
            }
            let body = s.child_by_field_name("consequence");
            if !cs_always_exits(body) {
                continue;
            }
            let cond = s.child_by_field_name("condition");
            early_if(out, s, cond, cond_text(cond, source), body, true, source);
        }
    }
}

// ----------------------------------------------------------------------- Go --

const GO_EXITS: &[&str] = &[
    "return_statement",
    "break_statement",
    "continue_statement",
    "goto_statement",
];

fn call_function_text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    let Some(call) = node.named_child(0) else {
        return "";
    };
    if call.kind() != "call_expression" {
        return "";
    }
    call.child_by_field_name("function")
        .map(|f| text(f, source))
        .unwrap_or("")
}

fn go_always_exits(node: Option<Node<'_>>, source: &str) -> bool {
    let Some(node) = node else {
        return false;
    };
    if GO_EXITS.contains(&node.kind()) {
        return true;
    }
    if node.kind() == "block" {
        return go_always_exits(last_named(node), source);
    }
    if node.kind() == "expression_statement" {
        return matches!(
            call_function_text(node, source),
            "panic" | "os.Exit" | "log.Fatal" | "log.Fatalf" | "log.Fatalln"
        );
    }
    false
}

struct GoRules;
static GO_RULES: GoRules = GoRules;

impl RuleSet for GoRules {
    fn boundaries(&self) -> &'static [&'static str] {
        &["function_declaration", "method_declaration", "source_file"]
    }
    fn inline_functions(&self) -> &'static [&'static str] {
        &["func_literal"]
    }
    fn binding_parents(&self) -> &'static [&'static str] {
        &["short_var_declaration", "var_spec", "assignment_statement", "const_spec"]
    }
    fn blocks(&self) -> &'static [&'static str] {
        &[
            "block",
            "expression_case",
            "default_case",
            "type_case",
            "communication_case",
            "source_file",
        ]
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
            "expression_case" | "type_case" | "communication_case" => {
                let value = parent
                    .child_by_field_name("value")
                    .or_else(|| parent.child_by_field_name("type"))
                    .or_else(|| parent.child_by_field_name("communication"));
                if value == Some(child) {
                    return;
                }
                let stmt = parent.parent();
                let subject = cond_text(stmt.and_then(|s| s.child_by_field_name("value")), source);
                let v = cond_text(value, source);
                let text = if parent.kind() == "communication_case" {
                    v
                } else if subject.is_empty() {
                    v
                } else {
                    format!("{subject} == {v}")
                };
                push(out, guard(GuardForm::Case, value, false, Some(text), stmt, None, source));
            }
            "default_case" => {
                let stmt = parent.parent();
                let subject = cond_text(stmt.and_then(|s| s.child_by_field_name("value")), source);
                let text = if subject.is_empty() {
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
                        Some(text),
                        stmt,
                        None,
                        source,
                    ),
                );
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
            _ => {}
        }
    }

    fn early_exits(&self, parent: Node<'_>, child: Node<'_>, source: &str, out: &mut Vec<BranchGuard>) {
        for s in preceding_siblings(parent, child).into_iter().rev() {
            if s.kind() != "if_statement" || s.child_by_field_name("alternative").is_some() {
                continue;
            }
            let body = s.child_by_field_name("consequence");
            if !go_always_exits(body, source) {
                continue;
            }
            let cond = s.child_by_field_name("condition");
            early_if(out, s, cond, cond_text(cond, source), body, true, source);
        }
    }
}

// -------------------------------------------------------------------- C/C++ --

const C_EXITS: &[&str] = &[
    "return_statement",
    "break_statement",
    "continue_statement",
    "goto_statement",
    "throw_statement",
];

fn c_always_exits(node: Option<Node<'_>>, source: &str) -> bool {
    let Some(node) = node else {
        return false;
    };
    if C_EXITS.contains(&node.kind()) {
        return true;
    }
    if node.kind() == "compound_statement" {
        return c_always_exits(last_named(node), source);
    }
    if node.kind() == "expression_statement" {
        return matches!(
            call_function_text(node, source),
            "exit" | "_exit" | "abort" | "longjmp"
        );
    }
    false
}

struct CRules;
static C_RULES: CRules = CRules;

impl RuleSet for CRules {
    fn boundaries(&self) -> &'static [&'static str] {
        &[
            "function_definition",
            "class_specifier",
            "struct_specifier",
            "namespace_definition",
            "translation_unit",
            "field_declaration_list",
        ]
    }
    fn inline_functions(&self) -> &'static [&'static str] {
        &["lambda_expression"]
    }
    fn binding_parents(&self) -> &'static [&'static str] {
        &["init_declarator", "assignment_expression"]
    }
    fn blocks(&self) -> &'static [&'static str] {
        &["compound_statement", "case_statement", "translation_unit"]
    }

    fn enclosing(&self, parent: Node<'_>, child: Node<'_>, source: &str, out: &mut Vec<BranchGuard>) {
        match parent.kind() {
            "if_statement" => {
                let cond = parent.child_by_field_name("condition");
                if is_field(parent, "consequence", child) {
                    push(out, guard(GuardForm::If, cond, false, None, None, None, source));
                } else if is_field(parent, "alternative", child) || child.kind() == "else_clause" {
                    push(out, guard(GuardForm::Else, cond, true, None, None, None, source));
                }
            }
            "conditional_expression" => {
                let cond = parent.child_by_field_name("condition");
                if is_field(parent, "consequence", child) {
                    push(out, guard(GuardForm::Ternary, cond, false, None, None, None, source));
                } else if is_field(parent, "alternative", child) {
                    push(out, guard(GuardForm::Ternary, cond, true, None, None, None, source));
                }
            }
            "case_statement" => {
                let value = parent.child_by_field_name("value");
                if value == Some(child) {
                    return;
                }
                let stmt = parent.parent().and_then(|p| p.parent());
                let subject = cond_text(stmt.and_then(|s| s.child_by_field_name("condition")), source);
                match value {
                    None => {
                        let text = if subject.is_empty() {
                            "default".to_owned()
                        } else {
                            format!("{subject}: default")
                        };
                        push(
                            out,
                            guard(
                                GuardForm::Case,
                                stmt.and_then(|s| s.child_by_field_name("condition")),
                                false,
                                Some(text),
                                stmt,
                                None,
                                source,
                            ),
                        );
                    }
                    Some(value) => {
                        let v = cond_text(Some(value), source);
                        let text = if subject.is_empty() {
                            v
                        } else {
                            format!("{subject} == {v}")
                        };
                        push(
                            out,
                            guard(GuardForm::Case, Some(value), false, Some(text), stmt, None, source),
                        );
                    }
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
                if is_field(parent, "body", child) {
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
        for s in preceding_siblings(parent, child).into_iter().rev() {
            if s.kind() != "if_statement" {
                continue;
            }
            let has_else = s.child_by_field_name("alternative").is_some()
                || named_children(s).iter().any(|n| n.kind() == "else_clause");
            if has_else {
                continue;
            }
            let body = s.child_by_field_name("consequence");
            if !c_always_exits(body, source) {
                continue;
            }
            let cond = s.child_by_field_name("condition");
            early_if(out, s, cond, cond_text(cond, source), body, true, source);
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

    fn label_lang(source: &str, language: &str, line: u32, column: Option<u32>) -> String {
        guard_label(&guards_in_source(source, language, line, column))
    }

    #[test]
    fn swift_if_and_else() {
        let source = [
            "func go(_ x: Bool) {",
            "  if x {",
            "    a()",
            "  } else {",
            "    b()",
            "  }",
            "}",
        ]
        .join("\n");
        assert_eq!(label_lang(&source, "swift", 3, None), "x");
        assert_eq!(label_lang(&source, "swift", 5, None), "!x");
    }

    #[test]
    fn swift_guard_statement_is_a_condition() {
        let source = ["func go(_ x: Bool) {", "  guard x else { return }", "  a()", "}"].join("\n");
        assert_eq!(label_lang(&source, "swift", 3, None), "x");
    }

    #[test]
    fn python_if_elif() {
        let source = [
            "def go(x, y):",
            "    if x:",
            "        a()",
            "    elif y:",
            "        b()",
        ]
        .join("\n");
        assert_eq!(label_lang(&source, "python", 3, None), "x");
        assert_eq!(label_lang(&source, "python", 5, None), "!x && y");
    }

    #[test]
    fn python_conditional_expression() {
        let source = "def go(c, a, b):\n    return a if c else b()\n";
        assert_eq!(label_lang(source, "python", 2, Some(23)), "!c");
    }

    #[test]
    fn java_if_and_logical_operand() {
        let source = [
            "class A {",
            "  void go(boolean x) {",
            "    if (x) {",
            "      a();",
            "    }",
            "  }",
            "}",
        ]
        .join("\n");
        assert_eq!(label_lang(&source, "java", 4, None), "x");

        let source = [
            "class A {",
            "  void go(boolean a, boolean b) {",
            "    a() && b();",
            "  }",
            "}",
        ]
        .join("\n");
        assert_eq!(label_lang(&source, "java", 3, Some(11)), "a()");
    }

    #[test]
    fn kotlin_if_expression() {
        let source = ["fun go(x: Boolean) {", "    if (x) {", "        a()", "    }", "}"].join("\n");
        assert_eq!(label_lang(&source, "kotlin", 3, None), "x");
    }

    #[test]
    fn csharp_if() {
        let source = [
            "class A {",
            "  void Go(bool x) {",
            "    if (x) {",
            "      a();",
            "    }",
            "  }",
            "}",
        ]
        .join("\n");
        assert_eq!(label_lang(&source, "csharp", 4, None), "x");
    }

    #[test]
    fn go_early_return_flips_the_comparison() {
        let source = [
            "package main",
            "func go(err error) {",
            "    if err != nil {",
            "        return",
            "    }",
            "    a()",
            "}",
        ]
        .join("\n");
        assert_eq!(label_lang(&source, "go", 6, None), "err == nil");
    }

    #[test]
    fn c_if() {
        let source = ["void go(int x) {", "  if (x) {", "    a();", "  }", "}"].join("\n");
        assert_eq!(label_lang(&source, "c", 3, None), "x");
    }

    #[test]
    fn every_declared_language_has_rules() {
        for language in BRANCH_GUARD_LANGUAGES {
            assert!(supports_branch_guards(language), "{language}");
        }
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
