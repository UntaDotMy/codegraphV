//! Angular templates: what a component renders and where it links. Port of
//! `src/resolution/angular-template-synthesizer.ts`.
//!
//! An Angular component's markup is a template — a `templateUrl` file beside it, or an inline
//! `template:` string — that the index never parses, so four things a reader relies on were missing
//! from the graph:
//!
//! - **The component tree.** `<app-article-list>` in a template renders `ArticleListComponent`: a
//!   `calls` edge from the parent class to the child class (`synthesizedBy: 'angular-template'`).
//! - **`routerLink`.** `routerLink="/login"`, `[routerLink]="['/article', article.slug]"` and a bound
//!   property that holds a route constant each become a `navigates` edge to the route it names.
//! - **Event bindings.** `(click)="toggleFavorite()"` is the only caller a handler method has: a
//!   `calls` edge from the component to its own method (`angular-event`) carrying the binding as its
//!   `trigger`.
//! - **Bindings.** `[name]="icon()"`, `{{ label() }}`, `*ngIf="isOpen()"` and `@if (loading()) {` call
//!   the component's own members at every render (`angular-binding`, the binding as `via`), WITHOUT a
//!   trigger. Reading a member without calling it counts only when the member is method-kind: a
//!   getter runs, so it is called; a method handed to a child is a function reference; a field a call
//!   filled is a plain `references`. A property read links nothing.
//!
//! A child is matched by its element selector, in the same app first; a selector two components share
//! there is left unmatched. A relative link and a destination no route serves draw nothing.
//!
//! Offsets are bytes; line numbers count `\n`s, so nothing depends on UTF-16 positions.

use super::common::*;
use crate::ctx::Ctx;
use crate::frameworks::angular_router::{angular_destination, angular_route_table, angular_routes_for, names_somewhere, static_string, template_path_for};
use crate::frameworks::expo_router::{app_root_for, to_href, HrefLiteral, HOLE};
use crate::frameworks::nextjs::destinations_for_href;
use crate::frameworks::object_literal::{match_bracket, read_fields, skip_string, slice, tail};
use crate::frameworks::package_deps::depends_on;
use crate::meta;
use crate::types::NodeRef;
use codegraph_jsre::Re;
use codegraph_search::is_test_path;
use codegraph_types::jscompat::{is_js_space, js_trim, utf16_byte_index, utf16_len, utf16_prefix};
use codegraph_types::{Edge, EdgeKind, Node, NodeKind};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

/// Links a single component may carry before it is a navigation menu rather than a decision.
const MAX_LINKS_PER_COMPONENT: usize = 24;
/// Children a single template may render before the rest are left out.
const MAX_CHILDREN_PER_COMPONENT: usize = 40;

const ELEMENT_SELECTOR: &str = r"^[a-zA-Z][\w-]*$";
/// `routerLink="…"` (plain) and `[routerLink]="…"` (bound), either quote.
const ROUTER_LINK: &str = r#"(\[routerLink\]|\brouterLink)\s*=\s*(?:"([^"]*)"|'([^']*)')"#;
/// `(click)="save()"` — an event binding, not `[(ngModel)]`'s two-way half.
const EVENT_BINDING: &str = r#"(?<!\[)\(([A-Za-z][\w.-]*)\)\s*=\s*(?:"([^"]*)"|'([^']*)')"#;
/// A call a template statement makes on its component: `save()`, `this.toggle(item)` — not `form.reset()`.
const OWN_CALL: &str = r"(?<![\w$.])(?:this\s*\.\s*)?([A-Za-z_$][\w$]*)\s*\(";
/// A `routerLink:` field of an object literal written in a class.
const ROUTER_LINK_FIELD: &str = r"(?<![\w$.])routerLink\s*:\s*";
/// An opening tag's name.
const TAG: &str = r"<([a-zA-Z][\w-]*)(?=[\s/>])";
/// `[name]="…"`, `[attr.aria-label]="…"`, `[(ngModel)]="…"`: a binding whose value is an expression.
const PROPERTY_BINDING: &str = r#"\[(\([A-Za-z][\w.-]*\)|[A-Za-z@][\w.@-]*)\]\s*=\s*(?:"([^"]*)"|'([^']*)')"#;
/// `*ngIf="…"`, `*ngFor="let item of items; trackBy: trackById"`: a structural directive, in its microsyntax.
const STRUCTURAL_DIRECTIVE: &str = r#"(?<![\w$*-])\*([A-Za-z][\w.-]*)\s*=\s*(?:"([^"]*)"|'([^']*)')"#;
/// `{{ … }}`, in text or inside an attribute's value.
const INTERPOLATION: &str = r"\{\{([\s\S]*?)\}\}";
/// A control-flow block that takes an expression: `@if (…) {`, `@else if (…)`, `@for (…)`, …
const CONTROL_FLOW: &str = r"(?<![\w$@])@(if|else\s+if|for|switch|case|defer)\s*\(";
/// `@let total = price() * quantity();` — a template's own constant.
const LET_DECLARATION: &str = r"(?<![\w$@])@let\s+([A-Za-z_$][\w$]*)\s*=";
/// A template reference variable — `#nameInput`, `ref-nameInput` — which the whole template sees.
const REFERENCE_VARIABLE: &str = r"(?<=\s)(?:#|ref-)([A-Za-z_$][\w$]*)(?=[\s=/>])";
/// `let-row`, `let-i="index"` on an `<ng-template>`: a local of what it renders.
const TEMPLATE_INPUT: &str = r"(?<=\s)let-([A-Za-z_$][\w$]*)(?=[\s=/>])";
const HTML_COMMENT: &str = r"<!--[\s\S]*?-->";
/// A name an expression writes: `this.` may lead it; a member of something else (`form.reset`) is not one.
const EXPRESSION_NAME: &str = r"(?<![\w$.])(?:this\s*\.\s*)?([A-Za-z_$][\w$]*)";

fn rx(pattern: &str) -> std::sync::Arc<Re> {
    Re::cached(pattern, "g")
}

fn line_of_offset(text: &str, at: usize) -> u32 {
    let end = at.min(text.len());
    1 + text.as_bytes()[..end].iter().filter(|b| **b == b'\n').count() as u32
}

pub struct AngularTemplate {
    pub file: String,
    pub text: String,
    pub first_line: u32,
    pub inline: bool,
}

pub struct AngularComponent {
    pub node: NodeRef,
    pub file: String,
    pub selectors: Vec<String>,
    pub template: Option<AngularTemplate>,
}

/// Every `@Component` class a file declares, with its selectors and template.
fn components_in(file: &str, content: &str, nodes: &[NodeRef], ctx: &Ctx) -> Vec<AngularComponent> {
    let mut out = Vec::new();
    let safe = codegraph_text::strip_comments_for_regex(content, "typescript");
    let decorator = Re::cached(r"@Component\s*\(", "g");
    let class_re = Re::cached(r"^\s*(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)", "");
    let element = Re::cached(ELEMENT_SELECTOR, "");
    let mut pos = 0;
    while let Some(m) = decorator.captures_at(&safe, pos) {
        let whole = m.get(0).unwrap();
        pos = whole.end();
        let paren = whole.end() - 1;
        let Some(close) = match_bracket(&safe, paren) else { continue };
        pos = close;
        let Some(open) = safe[paren..].find('{').map(|i| i + paren) else { continue };
        if open > close {
            continue;
        }
        let Some(obj_end) = match_bracket(&safe, open) else { continue };
        let Some(cls) = class_re.captures(utf16_prefix(tail(&safe, close + 1), 400)) else { continue };
        let cls_name = cls.text(1).unwrap_or("");
        let Some(node) = nodes.iter().find(|n| n.kind == NodeKind::Class && n.name == cls_name) else { continue };
        let fields = read_fields(&safe, open, obj_end);
        let selector = fields.get("selector").map(|f| f.text.as_str()).filter(|t| !t.is_empty()).and_then(static_string);
        let selectors: Vec<String> =
            selector.unwrap_or_default().split(',').map(|s| js_trim(s).to_string()).filter(|s| element.is_match(s)).collect();

        let mut template: Option<AngularTemplate> = None;
        let template_url = fields.get("templateUrl").map(|f| f.text.as_str()).filter(|t| !t.is_empty()).and_then(static_string).filter(|s| !s.is_empty());
        if let Some(template_url) = template_url {
            let template_file = template_path_for(file, &template_url);
            if let Some(text) = ctx.read_file(&template_file).filter(|t| !t.is_empty()) {
                template = Some(AngularTemplate { file: template_file, text: text.to_string(), first_line: 1, inline: false });
            }
        } else if let Some(inline) = fields.get("template") {
            // The template string's body, from the original source (stripping comments must not
            // touch a template's text), at its own offset. `safe` and `content` agree in UTF-16
            // units, not bytes, so the offset crosses over by units.
            let units = utf16_len(&safe[..inline.at.min(safe.len())]) + "template".len();
            let from = utf16_byte_index(content, units);
            let quote = js_trim(&inline.text).chars().next().unwrap_or('`');
            let quote_at = content[from..].find(quote).map(|i| i + from);
            let end = quote_at.and_then(|q| skip_string(content, q));
            if let (Some(q), Some(end)) = (quote_at, end) {
                if end > q {
                    template = Some(AngularTemplate { file: file.to_string(), text: content[q + 1..end].to_string(), first_line: line_of_offset(content, q + 1), inline: true });
                }
            }
        }
        out.push(AngularComponent { node: node.clone(), file: file.to_string(), selectors, template });
    }
    out
}

/// Every component in the project, read once per resolution state.
pub fn angular_components(ctx: &Ctx) -> Vec<AngularComponent> {
    let mut components = Vec::new();
    let ts = Re::cached(r"\.[cm]?ts$", "");
    for file in ctx.get_all_files() {
        if !ts.is_match(&file) || file.ends_with(".d.ts") || is_test_path(&file) {
            continue;
        }
        if !ctx.file_contains(&file, "@Component") {
            continue;
        }
        let Some(content) = ctx.read_file(&file).filter(|c| !c.is_empty()) else { continue };
        let nodes = ctx.get_nodes_in_file(&file);
        components.extend(components_in(&file, &content, &nodes, ctx));
    }
    components
}

// =============================================================================
// Reading a template
// =============================================================================

/// The element a binding at `at` is written on: the last opening tag before it.
fn element_at(text: &str, at: usize) -> Option<String> {
    let end = (at + 1).min(text.len());
    let open = text.as_bytes()[..end].iter().rposition(|b| *b == b'<')?;
    let tag = Re::cached(r"^<([a-zA-Z][\w-]*)", "").captures(utf16_prefix(tail(text, open), 64))?;
    tag.text(1).map(String::from)
}

/// An object field's value text from `at`, up to its `,` or closing brace at depth 0.
fn field_value(text: &str, at: usize) -> Option<String> {
    let b = text.as_bytes();
    let mut i = at;
    while i < b.len() {
        let ch = b[i];
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            i = skip_string(text, i)? + 1;
            continue;
        }
        if ch == b'[' || ch == b'{' || ch == b'(' {
            i = match_bracket(text, i)? + 1;
            continue;
        }
        if matches!(ch, b',' | b'}' | b']' | b')' | b';') {
            break;
        }
        i += 1;
    }
    let value = js_trim(slice(text, at, i));
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// The destination a plain `routerLink` names: `/profile/{{ user.username }}` is `/profile/${…}`.
fn plain_href(value: &str) -> Option<HrefLiteral> {
    let with_holes = rx(r"\{\{[\s\S]*?\}\}").replace_all_str(js_trim(value), &HOLE.to_string());
    if with_holes.starts_with('/') {
        names_somewhere(to_href(Some(with_holes)))
    } else {
        None
    }
}

// =============================================================================
// What a template reads at render
// =============================================================================

/// A name a template expression calls (`icon()`) or only reads (`canSave`, `displayFn`).
pub struct TemplateMemberUse {
    pub name: String,
    /// Offset of the name in the template.
    pub at: usize,
    pub call: bool,
    /// The binding the expression belongs to, as written: `[name]`, `{{ }}`, `*ngIf`, `@if`, `@let`.
    pub via: String,
}

/// One expression, and the scope it is read in. `scope` is the directive or block that evaluates it.
struct TemplateExpression {
    via: String,
    at: usize,
    text: String,
    scope: i32,
    sees_own_locals: bool,
}

/// A name the template declares — a local, never the component's member. Visible from `from` on.
struct TemplateLocal {
    name: String,
    from: usize,
    /// The directive or block that declared it; -1 for a reference variable or a `let-` input.
    scope: i32,
}

/// Each string literal blanked to spaces, so a quoted `'save()'` names nothing and offsets survive.
/// A template literal keeps its `${…}` holes: they are expressions.
fn blank_strings(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < b.len() {
        let ch = b[i];
        if ch == b'"' || ch == b'\'' {
            let close = skip_string(text, i).map_or(text.len(), |e| e + 1);
            out.push_str(&" ".repeat(close - i));
            i = close;
        } else if ch == b'`' {
            out.push(' ');
            i += 1;
            while i < b.len() {
                if b[i] == b'`' {
                    out.push(' ');
                    i += 1;
                    break;
                }
                let close = if b[i] == b'$' && b.get(i + 1) == Some(&b'{') { match_bracket(text, i + 1) } else { None };
                if let Some(close) = close {
                    out.push_str("  ");
                    out.push_str(&blank_strings(&text[i + 2..close]));
                    out.push(' ');
                    i = close + 1;
                    continue;
                }
                // One space per byte, and a backslash takes the character after it.
                let mut step = text[i..].chars().next().map_or(1, char::len_utf8);
                if b[i] == b'\\' {
                    step += text[i + 1..].chars().next().map_or(0, char::len_utf8);
                }
                out.push_str(&" ".repeat(step));
                i += step;
            }
        } else {
            let c = text[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

fn prev_char(text: &str, i: usize) -> Option<(usize, char)> {
    text[..i].char_indices().next_back()
}

fn is_blank_before(text: &str, i: usize) -> bool {
    prev_char(text, i).is_some_and(|(_, c)| is_js_space(c))
}

struct ExprUse {
    name: String,
    at: usize,
    call: bool,
}

/// The names an expression calls or reads, minus a pipe's name and an object literal's keys.
fn expression_uses(expression: &str) -> Vec<ExprUse> {
    let text = blank_strings(expression);
    let mut out = Vec::new();
    for m in rx(EXPRESSION_NAME).captures_iter(&text) {
        let whole = m.get(0).unwrap();
        let name = m.text(1).unwrap_or("");
        let end = whole.end();
        let mut before = whole.start();
        let mut prev: Option<(usize, char)> = None;
        while let Some((i, c)) = prev_char(&text, before) {
            if is_js_space(c) {
                before = i;
                continue;
            }
            prev = Some((i, c));
            break;
        }
        let mut after = end;
        let mut next: Option<char> = None;
        while let Some(c) = text[after..].chars().next() {
            if is_js_space(c) {
                after += c.len_utf8();
                continue;
            }
            next = Some(c);
            break;
        }
        let prev_c = prev.map(|p| p.1);
        // `form .reset()`: a member of something else, however it is spaced.
        if prev_c == Some('.') {
            continue;
        }
        // A pipe's name: `total | currency: code()` — but `a || b` is no pipe.
        if prev_c == Some('|') && prev_char(&text, prev.unwrap().0).map(|p| p.1) != Some('|') {
            continue;
        }
        // An object literal's key: `[ngClass]="{ active: isActive() }"`.
        if (prev_c == Some('{') || prev_c == Some(',')) && next == Some(':') {
            continue;
        }
        out.push(ExprUse { name: name.to_string(), at: end - name.len(), call: next == Some('(') });
    }
    out
}

/// Where the expression starting at `from` ends: a `;` or `,` outside any bracket, or the words
/// `as` / `let` that open the next microsyntax binding.
fn expression_end(text: &str, from: usize) -> usize {
    let b = text.as_bytes();
    let as_let = Re::cached(r"^(?:as|let)(?![\w$])", "");
    let mut depth = 0i32;
    let mut i = from;
    while i < b.len() {
        let ch = b[i];
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            match skip_string(text, i) {
                None => return text.len(),
                Some(end) => {
                    i = end + 1;
                    continue;
                }
            }
        }
        if ch == b'(' || ch == b'[' || ch == b'{' {
            depth += 1;
        } else if ch == b')' || ch == b']' || ch == b'}' {
            depth -= 1;
        } else if depth == 0 && (ch == b';' || ch == b',') {
            return i;
        } else if depth == 0 && is_blank_before(text, i) && as_let.is_match(slice(text, i, i + 4)) {
            return i;
        }
        i += 1;
    }
    text.len()
}

/// `text` split at each `;` outside brackets and strings, with each part's offset.
fn top_level_parts(text: &str) -> Vec<(usize, String)> {
    let b = text.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0i32;
    let mut i = 0;
    while i < b.len() {
        let ch = b[i];
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            match skip_string(text, i) {
                None => break,
                Some(end) => {
                    i = end + 1;
                    continue;
                }
            }
        }
        if ch == b'(' || ch == b'[' || ch == b'{' {
            depth += 1;
        } else if ch == b')' || ch == b']' || ch == b'}' {
            depth -= 1;
        } else if ch == b';' && depth == 0 {
            parts.push((start, slice(text, start, i).to_string()));
            start = i + 1;
        }
        i += 1;
    }
    parts.push((start, tail(text, start).to_string()));
    parts
}

/// `[A-Za-z_$][\w$]*` at `i`, sticky.
fn name_at(text: &str, i: usize) -> Option<&str> {
    let b = text.as_bytes();
    let first = *b.get(i)?;
    if !(first.is_ascii_alphabetic() || first == b'_' || first == b'$') {
        return None;
    }
    let mut j = i + 1;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'$') {
        j += 1;
    }
    Some(&text[i..j])
}

/// A structural directive's microsyntax, read into the expressions it evaluates and the locals it
/// declares: `let item of items; let i = index; trackBy: trackById` evaluates `items` and
/// `trackById` and declares `item` and `i`. A key (`of`, `trackBy:`, `else`) and the context name a
/// binding takes (`index`, `index as i`) belong to the directive, never the component.
pub fn read_microsyntax(text: &str) -> (Vec<(usize, String)>, Vec<String>) {
    let mut expressions: Vec<(usize, String)> = Vec::new();
    let mut locals: Vec<String> = Vec::new();
    let mut i = 0usize;
    let skip = |i: &mut usize, separators: bool| {
        while let Some(c) = text[*i..].chars().next() {
            if is_js_space(c) || (separators && (c == ';' || c == ',')) {
                *i += c.len_utf8();
            } else {
                break;
            }
        }
    };
    let word = |i: &mut usize| -> Option<String> {
        let w = name_at(text, *i)?;
        *i += w.len();
        Some(w.to_string())
    };
    let as_alias = Re::cached(r"^as(?![\w$])", "");
    let mut first = true;
    while i < text.len() {
        skip(&mut i, true);
        if i >= text.len() {
            break;
        }
        let start = i;
        let key = word(&mut i);
        if key.as_deref() == Some("let") && text[i..].chars().next().is_some_and(is_js_space) {
            // `let item`, `let i = index`: a local, and the context name it takes.
            skip(&mut i, false);
            if let Some(local) = word(&mut i) {
                locals.push(local);
            }
            skip(&mut i, false);
            if text.as_bytes().get(i) == Some(&b'=') {
                i += 1;
                skip(&mut i, false);
                word(&mut i);
            }
            first = false;
            continue;
        }
        if first || key.is_none() {
            // The directive's own expression (`*ngIf="cond"`) has no key.
            i = start;
        } else {
            skip(&mut i, false);
            if text.as_bytes().get(i) == Some(&b':') {
                i += 1;
            }
            skip(&mut i, false);
        }
        let end = expression_end(text, i);
        if end > i {
            expressions.push((i, text[i..end].to_string()));
        }
        i = end;
        skip(&mut i, false);
        if as_alias.is_match(slice(text, i, i + 3)) {
            i += 2;
            skip(&mut i, false);
            if let Some(alias) = word(&mut i) {
                locals.push(alias);
            }
        }
        // Nothing read (a stray character): step past it.
        if i == start {
            i += text[i..].chars().next().map_or(1, char::len_utf8);
        }
        first = false;
    }
    (expressions, locals)
}

/// The `;` that ends a `@let` declaration's expression.
fn statement_end(text: &str, from: usize) -> Option<usize> {
    let b = text.as_bytes();
    let mut depth = 0i32;
    let mut i = from;
    while i < b.len() {
        let ch = b[i];
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            i = skip_string(text, i)? + 1;
            continue;
        }
        if ch == b'(' || ch == b'[' || ch == b'{' {
            depth += 1;
        } else if ch == b')' || ch == b']' || ch == b'}' {
            depth -= 1;
            if depth < 0 {
                return None;
            }
        } else if ch == b';' && depth == 0 {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Every name a template's bindings call or read at render — property bindings, interpolations,
/// structural directives, control flow and `@let` — in template order, minus what the template
/// names itself: a pipe, an object key, a microsyntax key, and any local it declares. Event
/// bindings are not here: they run when the user acts, and are read on their own. A commented-out
/// binding reads nothing.
pub fn template_member_uses(template: &str) -> Vec<TemplateMemberUse> {
    // Each commented character blanked, newlines kept.
    let text = rx(HTML_COMMENT).replace_all(template, |c| {
        Some(c.text(0).unwrap_or("").chars().map(|ch| if ch == '\n' { "\n".to_string() } else { " ".repeat(ch.len_utf8()) }).collect::<String>())
    });
    let mut expressions: Vec<TemplateExpression> = Vec::new();
    let mut locals: Vec<TemplateLocal> = Vec::new();
    let mut scope = 0i32;
    let value_at = |end: usize, value: &str| end - 1 - value.len();
    let tag_start = |at: usize| text.as_bytes()[..(at + 1).min(text.len())].iter().rposition(|b| *b == b'<').unwrap_or(0);
    let quoted = |m: &codegraph_jsre::Caps, g: usize| -> String { m.text(g).or(m.text(g + 1)).unwrap_or("").to_string() };

    for m in rx(PROPERTY_BINDING).captures_iter(&text) {
        let whole = m.get(0).unwrap();
        let value = quoted(&m, 2);
        scope += 1;
        expressions.push(TemplateExpression {
            via: format!("[{}]", m.text(1).unwrap_or("")),
            at: value_at(whole.end(), &value),
            text: value,
            scope,
            sees_own_locals: false,
        });
    }
    for m in rx(STRUCTURAL_DIRECTIVE).captures_iter(&text) {
        let whole = m.get(0).unwrap();
        let value = quoted(&m, 2);
        let at = value_at(whole.end(), &value);
        scope += 1;
        let own = scope;
        let (read_expressions, read_locals) = read_microsyntax(&value);
        for (eat, etext) in read_expressions {
            expressions.push(TemplateExpression { via: format!("*{}", m.text(1).unwrap_or("")), at: at + eat, text: etext, scope: own, sees_own_locals: false });
        }
        // The host element and everything inside it see the directive's locals.
        for name in read_locals {
            locals.push(TemplateLocal { name, from: tag_start(whole.start()), scope: own });
        }
    }
    for m in rx(INTERPOLATION).captures_iter(&text) {
        let whole = m.get(0).unwrap();
        scope += 1;
        expressions.push(TemplateExpression { via: "{{ }}".to_string(), at: whole.start() + 2, text: m.text(1).unwrap_or("").to_string(), scope, sees_own_locals: false });
    }
    let whitespace = Re::cached(r"\s+", "g");
    for m in rx(CONTROL_FLOW).captures_iter(&text) {
        let whole = m.get(0).unwrap();
        let index = whole.start();
        let open = whole.end() - 1;
        let Some(close) = match_bracket(&text, open) else { continue };
        let kind = whitespace.replace_all_str(m.text(1).unwrap_or(""), " ");
        let via = format!("@{kind}");
        scope += 1;
        let own = scope;
        let parts: Vec<(usize, String)> = top_level_parts(&text[open + 1..close]).into_iter().map(|(at, t)| (open + 1 + at, t)).collect();
        let mut expr = |at: usize, t: String, sees_own_locals: bool| {
            expressions.push(TemplateExpression { via: via.clone(), at, text: t, scope: own, sees_own_locals });
        };
        if kind == "switch" || kind == "case" {
            for (at, t) in parts {
                expr(at, t, false);
            }
        } else if kind == "defer" {
            // `when cond` is an expression; `on viewport`, `on timer(5s)` are triggers.
            let when_re = Re::cached(r"^\s*(?:(?:prefetch|hydrate)\s+)?when(?![\w$])", "");
            for (at, t) in parts {
                if let Some(when) = when_re.captures(&t) {
                    let len = when.get(0).unwrap().end();
                    expr(at + len, t[len..].to_string(), false);
                }
            }
        } else if kind == "for" {
            // `item of items(); track item.id; let i = $index, odd = $odd`
            let mut iter = parts.into_iter();
            let head = iter.next();
            let loop_re = Re::cached(r"^(\s*)([A-Za-z_$][\w$]*)\s+of(?![\w$])", "");
            if let Some((hat, htext)) = head {
                if let Some(lp) = loop_re.captures(&htext) {
                    let len = lp.get(0).unwrap().end();
                    locals.push(TemplateLocal { name: lp.text(2).unwrap_or("").to_string(), from: index, scope: own });
                    expr(hat + len, htext[len..].to_string(), false);
                }
            }
            let track_re = Re::cached(r"^\s*track(?![\w$])", "");
            let let_re = Re::cached(r"^\s*let(?![\w$])", "");
            let alias_re = Re::cached(r"([A-Za-z_$][\w$]*)\s*=\s*\$?[\w$]+", "g");
            for (pat, ptext) in iter {
                if let Some(track) = track_re.captures(&ptext) {
                    let len = track.get(0).unwrap().end();
                    expr(pat + len, ptext[len..].to_string(), true);
                    continue;
                }
                if let_re.is_match(&ptext) {
                    for alias in alias_re.captures_iter(&ptext) {
                        locals.push(TemplateLocal { name: alias.text(1).unwrap_or("").to_string(), from: index, scope: own });
                    }
                }
            }
        } else {
            // `@if (user(); as user)`: the condition, then the alias its body sees.
            let mut iter = parts.into_iter();
            if let Some((hat, htext)) = iter.next() {
                expr(hat, htext, false);
            }
            let alias_re = Re::cached(r"^\s*as\s+([A-Za-z_$][\w$]*)", "");
            for (_, ptext) in iter {
                if let Some(alias) = alias_re.captures(&ptext) {
                    locals.push(TemplateLocal { name: alias.text(1).unwrap_or("").to_string(), from: index, scope: own });
                }
            }
        }
    }
    for m in rx(LET_DECLARATION).captures_iter(&text) {
        let whole = m.get(0).unwrap();
        let at = whole.end();
        let Some(end) = statement_end(&text, at) else { continue };
        scope += 1;
        let own = scope;
        expressions.push(TemplateExpression { via: "@let".to_string(), at, text: text[at..end].to_string(), scope: own, sees_own_locals: false });
        locals.push(TemplateLocal { name: m.text(1).unwrap_or("").to_string(), from: whole.start(), scope: own });
    }
    for m in rx(REFERENCE_VARIABLE).captures_iter(&text) {
        locals.push(TemplateLocal { name: m.text(1).unwrap_or("").to_string(), from: 0, scope: -1 });
    }
    for m in rx(TEMPLATE_INPUT).captures_iter(&text) {
        locals.push(TemplateLocal { name: m.text(1).unwrap_or("").to_string(), from: tag_start(m.get(0).unwrap().start()), scope: -1 });
    }

    let mut uses: Vec<TemplateMemberUse> = Vec::new();
    for e in &expressions {
        for u in expression_uses(&e.text) {
            let at = e.at + u.at;
            let shadowed = locals.iter().any(|l| l.name == u.name && l.from <= at && (l.scope != e.scope || e.sees_own_locals));
            if !shadowed {
                uses.push(TemplateMemberUse { name: u.name, at, call: u.call, via: e.via.clone() });
            }
        }
    }
    uses.sort_by_key(|u| u.at);
    uses
}

/// What a method-kind member's own declaration says it is: an accessor (`get canSave() {`), a
/// function (`displayFn(p) {`, `onPick = (p) => …`), or a field holding what a call returned
/// (`days = Array.from(…)`, `total = computed(() => …)`) — a method only because a callback is
/// written in its initializer, and as likely plain data.
#[derive(Clone, Copy, PartialEq)]
enum MemberShape {
    Get,
    Set,
    Function,
    Value,
}

fn is_name_edge(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '\'' || c == '"' || c == '`')
}

fn member_shape(content: &str, member: &Node) -> MemberShape {
    let window: Vec<&str> = content.split('\n').skip((member.start_line as usize).saturating_sub(1)).take(5).collect();
    let joined = window.join("\n");
    let head = &joined[utf16_byte_index(&joined, member.start_column as usize)..];
    let accessor_re = Re::cached(r"(?<![\w$])(get|set)\s+$", "");
    let field_re = Re::cached(r"^[?!]?\s*(?::[^=;]*(?:=>[^=;]*)*)?=(?![=>])\s*", "");
    let function_re = Re::cached(r"^(?:function(?![\w$])|[A-Za-z_$][\w$]*\s*=>)", "");
    let arrow_re = Re::cached(r"^\s*(?::[^=]*)?=>", "");
    let async_re = Re::cached(r"^async\s+", "");
    let mut from = 0;
    loop {
        let Some(at) = head[from..].find(member.name.as_str()).map(|i| i + from) else { return MemberShape::Function };
        from = at + member.name.len();
        // The name itself — not a longer name, and not a decorator's `'value'` argument.
        if is_name_edge(head[..at].chars().next_back()) || is_name_edge(head[from..].chars().next()) {
            continue;
        }
        if let Some(accessor) = accessor_re.captures(&head[..at]) {
            return if accessor.text(1) == Some("get") { MemberShape::Get } else { MemberShape::Set };
        }
        // `name = …`, `name: Type = …` (a function type's `=>` is not the `=`).
        let Some(field) = field_re.captures(&head[from..]) else { return MemberShape::Function };
        let value = async_re.replace_all_str(&head[from + field.get(0).unwrap().end()..], "");
        if function_re.is_match(&value) {
            return MemberShape::Function;
        }
        let close = if value.starts_with('(') { match_bracket(&value, 0) } else { None };
        return match close {
            Some(c) if c > 0 && arrow_re.is_match(&value[c + 1..]) => MemberShape::Function,
            _ => MemberShape::Value,
        };
    }
}

/// The member one use links, and how. A call links what it calls: a method, or a signal held in a
/// field (`count = signal(0)` is a property). A name read without a call links only a method-kind
/// member — a getter runs when it is read, so it is called; a function handed over uncalled is a
/// function reference; a field a call filled is a plain reference. Same-named methods resolve as the
/// event bindings' do: the last declared (an overload's body).
fn binding_target(candidates: Option<&Vec<NodeRef>>, call: bool, shape: &dyn Fn(&Node) -> MemberShape) -> Option<(NodeRef, EdgeKind, bool)> {
    let candidates = candidates?;
    let methods: Vec<&NodeRef> = candidates.iter().filter(|n| n.kind == NodeKind::Method).collect();
    if call {
        let target = methods
            .iter()
            .rev()
            .find(|n| matches!(shape(n), MemberShape::Function | MemberShape::Value))
            .map(|n| (*n).clone())
            .or_else(|| methods.first().map(|n| (*n).clone()))
            .or_else(|| candidates.iter().find(|n| n.kind == NodeKind::Property).cloned());
        return target.map(|node| (node, EdgeKind::Calls, false));
    }
    if let Some(getter) = methods.iter().find(|n| shape(n) == MemberShape::Get) {
        return Some(((*getter).clone(), EdgeKind::Calls, false));
    }
    let read = methods.iter().rev().find(|n| shape(n) != MemberShape::Set)?;
    let fn_ref = shape(read) == MemberShape::Function;
    Some(((*read).clone(), EdgeKind::References, fn_ref))
}

// =============================================================================
// The pass
// =============================================================================

pub fn angular_template_edges(ctx: &Ctx) -> Vec<Edge> {
    if !depends_on(ctx, &["@angular/core"]) {
        return Vec::new();
    }
    let components = angular_components(ctx);
    if components.is_empty() {
        return Vec::new();
    }

    // Element selector → the components that answer to it, per app.
    let mut by_selector: IndexMap<&str, Vec<usize>> = IndexMap::new();
    for (i, c) in components.iter().enumerate() {
        for sel in &c.selectors {
            by_selector.entry(sel.as_str()).or_default().push(i);
        }
    }
    let child_for = |tag: &str, parent: &AngularComponent| -> Option<usize> {
        let all = by_selector.get(tag)?;
        if all.is_empty() {
            return None;
        }
        if all.len() == 1 {
            return Some(all[0]);
        }
        let root = app_root_for(&parent.file);
        let near: Vec<usize> = all.iter().copied().filter(|i| components[*i].file.starts_with(&root)).collect();
        if near.len() == 1 {
            Some(near[0])
        } else {
            None
        }
    };

    let table = angular_route_table(ctx);
    let mut edges: Vec<Edge> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let tag_re = rx(TAG);
    let event_re = rx(EVENT_BINDING);
    let own_call_re = rx(OWN_CALL);
    let router_link_re = rx(ROUTER_LINK);
    let router_link_field_re = rx(ROUTER_LINK_FIELD);
    for component in &components {
        let Some(template) = &component.template else { continue };
        let text = template.text.as_str();
        // Where an edge is written: the tag's own line for an inline template; for a template
        // file, the class — the file the edge's source is in.
        let site_line = |at: usize| if template.inline { template.first_line + line_of_offset(text, at) - 1 } else { component.node.start_line };
        let registered_at = |at: usize| format!("{}:{}", template.file, template.first_line + line_of_offset(text, at) - 1);

        let mut children = 0usize;
        for t in tag_re.captures_iter(text) {
            if children >= MAX_CHILDREN_PER_COMPONENT {
                break;
            }
            let tag = t.text(1).unwrap_or("");
            let Some(child) = child_for(tag, component).map(|i| &components[i]) else { continue };
            if child.node.id == component.node.id {
                continue;
            }
            let key = format!("{}>{}", component.node.id, child.node.id);
            if !seen.insert(key) {
                continue;
            }
            children += 1;
            let at = t.get(0).unwrap().start();
            edges.push(calls_edge(
                &component.node.id,
                &child.node.id,
                Some(site_line(at)),
                meta! { "synthesizedBy" => "angular-template", "via" => tag, "registeredAt" => registered_at(at) },
            ));
        }

        // Event bindings: `(click)="toggleFavorite()"` runs the component's own method when the
        // user acts. The binding is the trigger Steps draws — known only from the template, so it
        // rides on the edge.
        let prefix = format!("{}::", component.node.qualified_name);
        let nodes = ctx.get_nodes_in_file(&component.file);
        let own: Vec<NodeRef> =
            nodes.iter().filter(|n| (n.kind == NodeKind::Method || n.kind == NodeKind::Property) && n.qualified_name.starts_with(&prefix)).cloned().collect();
        let members: HashMap<&str, &NodeRef> = own.iter().filter(|n| n.kind == NodeKind::Method).map(|n| (n.name.as_str(), n)).collect();
        // Members this template already links — only the first edge between two nodes survives the merge.
        let mut linked: HashSet<String> = HashSet::new();
        let mut handlers = 0usize;
        for ev in event_re.captures_iter(text) {
            if handlers >= MAX_CHILDREN_PER_COMPONENT {
                break;
            }
            let statement = ev.text(2).or(ev.text(3)).unwrap_or("");
            let event = format!("({})", ev.text(1).unwrap_or(""));
            let ev_at = ev.get(0).unwrap().start();
            let element = element_at(text, ev_at);
            for c in own_call_re.captures_iter(statement) {
                let Some(method) = members.get(c.text(1).unwrap_or("")) else { continue };
                let key = format!("{}>{}>{}", component.node.id, method.id, event);
                if !seen.insert(key) {
                    continue;
                }
                linked.insert(method.id.clone());
                handlers += 1;
                edges.push(calls_edge(
                    &component.node.id,
                    &method.id,
                    Some(site_line(ev_at)),
                    meta! {
                        "synthesizedBy" => "angular-event",
                        "via" => event.as_str(),
                        "registeredAt" => registered_at(ev_at),
                        "trigger" => serde_json::json!({ "kind": "prop", "name": event, "of": element }),
                    },
                ));
            }
        }

        // Bindings: `[name]="icon()"`, `{{ label() }}`, `@if (loading())` read the component every
        // time it renders, so the hop carries no trigger. At most one edge per member, so the
        // class's own size bounds them.
        let declaration: RefCell<Option<String>> = RefCell::new(None);
        let shapes: RefCell<HashMap<String, MemberShape>> = RefCell::new(HashMap::new());
        let shape = |n: &Node| -> MemberShape {
            if let Some(known) = shapes.borrow().get(&n.id) {
                return *known;
            }
            let mut decl = declaration.borrow_mut();
            let content = decl.get_or_insert_with(|| ctx.read_file(&component.file).map(|c| c.to_string()).unwrap_or_default());
            let read = member_shape(content, n);
            shapes.borrow_mut().insert(n.id.clone(), read);
            read
        };
        let mut named: HashMap<&str, Vec<NodeRef>> = HashMap::new();
        for n in &own {
            named.entry(n.name.as_str()).or_default().push(n.clone());
        }
        for u in template_member_uses(text) {
            let Some((node, kind, fn_ref)) = binding_target(named.get(u.name.as_str()), u.call, &shape) else { continue };
            if linked.contains(&node.id) {
                continue;
            }
            linked.insert(node.id.clone());
            let mut md = meta! { "synthesizedBy" => "angular-binding", "via" => u.via.as_str(), "registeredAt" => registered_at(u.at) };
            if fn_ref {
                md.insert("fnRef".to_string(), true.into());
            }
            edges.push(heuristic_edge(&component.node.id, &node.id, kind, Some(site_line(u.at)), md));
        }

        let Some(routes) = angular_routes_for(&table, &component.file) else { continue };
        if routes.exact.is_empty() {
            continue;
        }
        let mut links = 0usize;
        // A tab bar or menu built in the class — `this.tabs = [{ label, routerLink:
        // internalRoutes.home.subRoutes.summary.routerLink }]`, handed to a component whose template
        // binds `[routerLink]="tab.routerLink"` in a loop. The destination is the field this class wrote.
        if let Some(source) = ctx.read_file(&component.file).filter(|s| !s.is_empty()) {
            let body = slice_lines(&source, component.node.start_line, component.node.end_line).unwrap_or_default();
            for f in router_link_field_re.captures_iter(&body) {
                if links >= MAX_LINKS_PER_COMPONENT {
                    break;
                }
                let whole = f.get(0).unwrap();
                let value = field_value(&body, whole.end());
                let href = value.and_then(|v| angular_destination(&v, &component.file, Some(&*component.node), ctx, 0, false));
                let Some(href) = href.filter(|h| h.path.starts_with('/')) else { continue };
                let line = component.node.start_line + line_of_offset(&body, whole.start()) - 1;
                for dest in destinations_for_href(&href, routes) {
                    let key = format!("{}>{}", component.node.id, dest.node.id);
                    if !seen.insert(key) {
                        continue;
                    }
                    links += 1;
                    edges.push(heuristic_edge(
                        &component.node.id,
                        &dest.node.id,
                        EdgeKind::Navigates,
                        Some(line),
                        meta! {
                            "synthesizedBy" => "angular-router-link",
                            "href" => dest.href.display.as_str(),
                            "navMethod" => "routerLink",
                            "registeredAt" => format!("{}:{}", component.file, line),
                        },
                    ));
                }
            }
        }
        for r in router_link_re.captures_iter(text) {
            if links >= MAX_LINKS_PER_COMPONENT {
                break;
            }
            let value = r.text(2).or(r.text(3)).unwrap_or("");
            let href = if r.text(1) == Some("[routerLink]") { angular_destination(value, &component.file, Some(&*component.node), ctx, 0, false) } else { plain_href(value) };
            let Some(href) = href.filter(|h| h.path.starts_with('/')) else { continue };
            let at = r.get(0).unwrap().start();
            for dest in destinations_for_href(&href, routes) {
                let key = format!("{}>{}", component.node.id, dest.node.id);
                if !seen.insert(key) {
                    continue;
                }
                links += 1;
                edges.push(heuristic_edge(
                    &component.node.id,
                    &dest.node.id,
                    EdgeKind::Navigates,
                    Some(site_line(at)),
                    meta! {
                        "synthesizedBy" => "angular-router-link",
                        "href" => dest.href.display.as_str(),
                        "navMethod" => "routerLink",
                        "registeredAt" => registered_at(at),
                        "template" => true,
                    },
                ));
            }
        }
    }
    edges
}
