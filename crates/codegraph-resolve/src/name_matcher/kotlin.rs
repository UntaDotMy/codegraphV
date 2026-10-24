//! Kotlin scope rules: which member a receiver-less call reaches (the enclosing classes, their
//! supertypes, the `with` / `apply` / lambda-with-receiver scopes around it), which stdlib-named
//! method a chain can mean, and which top-level declarations a file can see.

use super::cpp_text::split_cpp_top_level;
use super::lang_shape::{is_method_owner_kind, RefShape};
use super::members::{class_head_supertypes, is_member_class_kind};
use super::method::shares_receiver_word;
use super::rust_go_shape::rust_go_receiver_name;
use super::std_methods::KOTLIN_STD_METHODS;
use super::util::*;
use crate::ctx::Ctx;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_extract::strip_comments::{blank_string_contents, strip_comments_for_regex};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::{Language, Node, NodeKind};
use std::collections::{HashSet, VecDeque};
use std::rc::Rc;

/// The line a reference is on: the cached lines, else the file split on `\n`.
fn line_or_raw(ctx: &Ctx, file: &str, line: u32) -> Option<String> {
    line_at(ctx, file, line).or_else(|| {
        let idx = (line as usize).checked_sub(1)?;
        ctx.read_file(file).and_then(|s| s.split('\n').nth(idx).map(String::from))
    })
}

/// The last `::` / `.` separated segment of an owner path.
fn last_owner_segment(path: &str) -> &str {
    let by_colons = path.rfind("::").map(|i| i + 2);
    let by_dot = path.rfind('.').map(|i| i + 1);
    &path[by_colons.into_iter().chain(by_dot).max().unwrap_or(0)..]
}

/// The name a Kotlin call is written on (`xs.map { }` → `xs`, `a.b().c()` → `c`'s receiver), or
/// the left operand of an infix call.
pub fn kotlin_chain_receiver(r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let line = line_or_raw(ctx, &r.file_path, r.line)?;
    let name = r.reference_name.as_str();
    let mut start: Option<usize> = if starts_with_at(&line, name, r.column) { Some(col_byte(&line, r.column)) } else { None };
    if start.is_none() {
        start = Re::cached(&format!(r"(?<![\w$]){}\s*[({{]", escape_regex(name)), "").find(&line).map(|m| m.start());
    }
    let Some(start) = start else {
        // An infix call — `a shl 2`, `x until y` — has the receiver on its left.
        let infix = infix_call(name).captures(&line)?.get(2)?;
        return Some(rust_go_receiver_name(line[..infix.start()].trim_end_matches(codegraph_types::jscompat::is_js_space)));
    };
    let before = &line[..start];
    if !re!(r"\.\s*$").is_match(before) {
        return None;
    }
    let before = re!(r"\?\s*\.\s*$").replace_first(before, |_| String::new());
    let before = re!(r"!!\s*$").replace_first(&before, |_| String::new());
    Some(rust_go_receiver_name(&before))
}

/// `name` as an infix call: preceded by an operand and spaces, followed by spaces and an operand.
/// (The original is a variable-length lookbehind; the operand is consumed here and group 2 is the name.)
fn infix_call(name: &str) -> std::sync::Arc<Re> {
    Re::cached(&format!(r#"([\w)\]"'}}]\s+)({})\s+(?=[^\s=])"#, escape_regex(name)), "")
}

const KOTLIN_BITWISE_INFIX: &[&str] = &["and", "or", "xor", "shl", "shr", "ushr", "inv"];
const KOTLIN_NUMBER_TYPES: &[&str] = &["Byte", "Short", "Int", "Long", "UByte", "UShort", "UInt", "ULong", "Char"];

/// A bitwise infix (`flags and mask`) on a number type: never a project function named `and`.
pub fn is_kotlin_number_bitwise(n: &Node, r: &UnresolvedRef) -> bool {
    if r.language != Language::Kotlin || !KOTLIN_BITWISE_INFIX.contains(&n.name.as_str()) {
        return false;
    }
    match n.qualified_name.rfind("::") {
        Some(cut) if cut > 0 => KOTLIN_NUMBER_TYPES.contains(&last_owner_segment(&n.qualified_name[..cut])),
        _ => false,
    }
}

/// Is the call written without a receiver (`run(...)`, not `x.run(...)` / `X::run`)?
pub fn is_receiver_less_kotlin_call(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let Some(line) = line_or_raw(ctx, &r.file_path, r.line) else { return false };
    let name = r.reference_name.as_str();
    let at_column = starts_with_at(&line, name, r.column) && !char_before(&line, col_byte(&line, r.column)).is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    let mut start: Option<usize> = if at_column { Some(col_byte(&line, r.column)) } else { None };
    if start.is_none() {
        start = Re::cached(&format!(r"(?<![\w$]){}\s*[({{<]", escape_regex(name)), "").find(&line).map(|m| m.start());
    }
    start.is_some_and(|s| !re!(r"[.:]\s*$").is_match(&line[..s]))
}

/// `require(x > 0)` / `check(ok) { … }` / `assert(a == b)`: the stdlib preconditions, told apart
/// from a project method of the same name by a boolean argument or a trailing message lambda.
fn is_kotlin_precondition_call(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if !re!(r"^(?:require|check|assert)$").is_match(&r.reference_name) {
        return false;
    }
    let Some(line) = line_or_raw(ctx, &r.file_path, r.line) else { return false };
    if line.is_empty() {
        return false;
    }
    let Some(at) = Re::cached(&format!(r"(?<![\w.]){}\s*\(", r.reference_name), "").find(&line) else { return false };
    let mut depth = 0i32;
    let mut args = String::new();
    let mut rest = "";
    for (i, ch) in line[at.end()..].char_indices() {
        if ch == '(' {
            depth += 1;
        } else if ch == ')' {
            let was = depth;
            depth -= 1;
            if was == 0 {
                rest = &line[at.end() + i + 1..];
                break;
            }
        }
        args.push(ch);
    }
    re!(r"[<>=!]=|&&|\|\||(?:^|[\s(])!|\s[<>]\s|\bis\b|\bin\b|\btrue\b|\bfalse\b").is_match(&args) || re!(r"^\s*\{").is_match(rest)
}

memo_map!(KotlinReceiverTypesMemo, (), Rc<HashSet<String>>);

/// Every type some Kotlin lambda-with-receiver or extension function is written on, widened by
/// the supertypes of the ones declared in the repo: the scopes a bare call can resolve inside.
fn kotlin_receiver_types(ctx: &Ctx) -> Rc<HashSet<String>> {
    memoized::<KotlinReceiverTypesMemo>(ctx, (), || {
        let mut types: HashSet<String> = HashSet::new();
        let mut order: Vec<String> = Vec::new();
        let mut outside: Vec<String> = Vec::new();
        let note = |set: &mut HashSet<String>, order: &mut Vec<String>, name: String| {
            if set.insert(name.clone()) {
                order.push(name);
            }
        };
        let mut outside_seen: HashSet<String> = HashSet::new();
        for file in ctx.get_all_files() {
            if !re!(r"\.kts?$").is_match(&file) {
                continue;
            }
            let source = strip_comments_for_regex(&ctx.read_file(&file).map(|s| s.to_string()).unwrap_or_default(), "java");
            for m in re!(r"\b([A-Z]\w*)(?:<[^<>()]*(?:<[^<>()]*>[^<>()]*)*>)?\s*\.\s*\(", "g").captures_iter(&source) {
                note(&mut types, &mut order, m.text(1).unwrap_or("").to_string());
            }
            // `fun Foo.bar(` — an extension on a type that may be outside the repo.
            for m in re!(r"\bfun\s+(?:<[^>]*>\s*)?([A-Z]\w*(?:\.[A-Z]\w*)*)(?:<[^<>()]*(?:<[^<>()]*>[^<>()]*)*>)?\??\.[A-Za-z_`][\w`]*\s*\(", "g").captures_iter(&source) {
                for part in m.text(1).unwrap_or("").split('.') {
                    if outside_seen.insert(part.to_string()) {
                        outside.push(part.to_string());
                    }
                }
            }
        }
        for name in outside {
            if !ctx.get_nodes_by_name(&name).iter().any(|n| is_member_class_kind(n.kind) && matches!(n.language, Language::Kotlin | Language::Java)) {
                note(&mut types, &mut order, name);
            }
        }
        let mut queue: VecDeque<String> = order.into_iter().collect();
        while let Some(name) = queue.pop_front() {
            if types.len() >= 5000 {
                break;
            }
            for decl in ctx.get_nodes_by_name(&name).iter() {
                if decl.language != Language::Kotlin || !is_member_class_kind(decl.kind) {
                    continue;
                }
                for sup in class_head_supertypes(decl, ctx) {
                    if types.insert(sup.clone()) {
                        queue.push_back(sup);
                    }
                }
            }
        }
        Rc::new(types)
    })
}

/// The Android framework supertypes a Kotlin class inherits members from without declaring them.
fn kotlin_platform_supers(name: &str) -> &'static [&'static str] {
    match name {
        "AppCompatActivity" => &["FragmentActivity"],
        "FragmentActivity" => &["ComponentActivity"],
        "ComponentActivity" => &["Activity", "LifecycleOwner", "ViewModelStoreOwner", "SavedStateRegistryOwner"],
        "Activity" => &["ContextThemeWrapper", "ComponentCallbacks2"],
        "ContextThemeWrapper" => &["ContextWrapper"],
        "ContextWrapper" => &["Context"],
        "Application" => &["ContextWrapper", "ComponentCallbacks2"],
        "Service" => &["ContextWrapper", "ComponentCallbacks2"],
        "ComponentCallbacks2" => &["ComponentCallbacks"],
        "Fragment" => &["ComponentCallbacks", "LifecycleOwner", "ViewModelStoreOwner", "SavedStateRegistryOwner"],
        "DialogFragment" => &["Fragment"],
        "AppCompatDialogFragment" => &["DialogFragment"],
        "BottomSheetDialogFragment" => &["AppCompatDialogFragment"],
        "AndroidViewModel" => &["ViewModel"],
        _ => &[],
    }
}

memo_map!(KotlinHierarchyMemo, String, Rc<HashSet<String>>);

/// The type names whose members a bare call at the reference can reach: the enclosing classes
/// and their supertypes, the receivers of the `with` / `apply` / DSL-lambda frames around it,
/// and Gradle's `Project` / `Settings` in a build script. `*` stands for "any receiver".
pub fn kotlin_hierarchy_at(r: &UnresolvedRef, ctx: &Ctx) -> Rc<HashSet<String>> {
    memoized::<KotlinHierarchyMemo>(ctx, format!("{}:{}", r.file_path, r.line), || {
        let mut queue: VecDeque<String> = VecDeque::new();
        for n in ctx.get_nodes_in_file(&r.file_path).iter() {
            if n.start_line > r.line || n.end_line < r.line {
                continue;
            }
            if is_member_class_kind(n.kind) {
                queue.push_back(n.name.clone());
            } else if matches!(n.kind, NodeKind::Method | NodeKind::Function) && n.qualified_name.contains("::") {
                let cut = n.qualified_name.rfind("::").unwrap_or(0);
                queue.push_back(last_owner_segment(&n.qualified_name[..cut]).to_string());
            }
        }
        if r.file_path.ends_with(".gradle.kts") {
            queue.push_back(if re!(r"(?:^|/)settings\.gradle\.kts$").is_match(&r.file_path) { "Settings" } else { "Project" }.to_string());
        }
        for frame in kotlin_brace_frames(&r.file_path, ctx).iter() {
            if frame.start <= r.line && frame.end >= r.line {
                queue.extend(frame.names.iter().cloned());
            }
        }
        let mut names: HashSet<String> = HashSet::new();
        while let Some(name) = queue.pop_front() {
            if names.len() >= 60 {
                break;
            }
            if names.contains(&name) {
                continue;
            }
            queue.extend(kotlin_platform_supers(&name).iter().map(|s| s.to_string()));
            for decl in ctx.get_nodes_by_name(&name).iter() {
                if decl.language == Language::Kotlin && is_member_class_kind(decl.kind) {
                    queue.extend(class_head_supertypes(decl, ctx));
                }
            }
            names.insert(name);
        }
        Rc::new(names)
    })
}

/// A `{ … }` block of a Kotlin file that brings the members of some types into scope.
pub struct KotlinFrame {
    pub start: u32,
    pub end: u32,
    pub names: Vec<String>,
}

memo_map!(KotlinFramesMemo, String, Rc<Vec<KotlinFrame>>);

const KOTLIN_BLOCK_WORDS: &[&str] = &[
    "if", "else", "for", "while", "do", "when", "try", "catch", "finally", "init", "get", "set", "constructor", "fun", "class", "object", "interface", "return",
    "by", "lazy", "apply", "run", "also", "let", "with", "use",
];

/// The brace blocks of a file whose receivers are known: a class body (its name and
/// supertypes), a `with(x) { }` / `.apply { }` / `.run { }` block (any receiver), an extension
/// function body (its receiver type), and the lambda argument of a function declared with a
/// receiver-typed last parameter.
pub fn kotlin_brace_frames(file: &str, ctx: &Ctx) -> Rc<Vec<KotlinFrame>> {
    memoized::<KotlinFramesMemo>(ctx, file.to_string(), || {
        let mut frames: Vec<KotlinFrame> = Vec::new();
        let raw = ctx.read_file(file).map(|s| s.to_string()).unwrap_or_default();
        let source = blank_string_contents(&strip_comments_for_regex(&raw, "java"), None);
        let mut stack: Vec<(u32, Option<Vec<String>>)> = Vec::new();
        let mut line = 1u32;
        let mut pending = String::new();
        let mut pending_len = 0usize;
        for ch in source.chars() {
            if ch == '\n' {
                line += 1;
            }
            if ch == '{' {
                let scoped = re!(r"(?:\bwith\s*\([^{}]*\)|\.\s*(?:apply|run)(?:\s*<[^<>]*>)?)\s*$").is_match(&pending);
                let mut names: Option<Vec<String>> = if scoped { Some(vec!["*".to_string()]) } else { kotlin_head_names(&pending) };
                if !scoped && names.as_ref().map_or(true, |n| n.is_empty()) {
                    let call = re!(r"(?:^|[^\w$])([a-z_]\w*)\s*(?:<[^<>{}]*>)?\s*(?:\([^(){}]*\))?\s*$").captures(&pending).and_then(|c| c.text(1).map(String::from));
                    let receiver = call.filter(|c| !KOTLIN_BLOCK_WORDS.contains(&c.as_str())).and_then(|c| kotlin_lambda_receiver(&c, ctx));
                    if let Some(receiver) = receiver {
                        names = Some(vec![receiver]);
                    }
                }
                stack.push((line, names));
                pending.clear();
                pending_len = 0;
            } else if ch == '}' {
                if let Some((start, Some(names))) = stack.pop() {
                    if !names.is_empty() {
                        frames.push(KotlinFrame { start, end: line, names });
                    }
                }
                pending.clear();
                pending_len = 0;
            } else if ch == ';' {
                pending.clear();
                pending_len = 0;
            } else if pending_len < 600 {
                pending.push(ch);
                pending_len += ch.len_utf16();
            } else {
                // Keep the last 300 UTF-16 units, as the original's `slice(300)` does.
                let units: Vec<u16> = pending.encode_utf16().collect();
                pending = String::from_utf16_lossy(&units[300.min(units.len())..]);
                pending.push(ch);
                pending_len = units.len().saturating_sub(300) + ch.len_utf16();
            }
        }
        Rc::new(frames)
    })
}

memo_map!(KotlinLambdaReceiverMemo, String, Option<String>);

/// The receiver type of the last (lambda) parameter of the one Kotlin function named `name`:
/// `fun buildThing(init: Builder.() -> Unit)` → `Builder`, through a `typealias` if need be.
pub fn kotlin_lambda_receiver(name: &str, ctx: &Ctx) -> Option<String> {
    memoized::<KotlinLambdaReceiverMemo>(ctx, name.to_string(), || {
        let mut found: Vec<String> = Vec::new();
        for f in ctx.get_nodes_by_name(name).iter() {
            if f.language != Language::Kotlin || !matches!(f.kind, NodeKind::Function | NodeKind::Method) {
                continue;
            }
            let lines = lines_of(ctx, &f.file_path);
            let head = js_slice(&lines, f.start_line as i64 - 1, f.start_line as i64 + 11).join(" ");
            let Some(open) = Re::cached(&format!(r"\b{}\s*\(", escape_regex(name)), "").find(&head).map(|m| m.start()) else { continue };
            let Some(paren) = head[open..].find('(').map(|p| open + p) else { continue };
            let mut depth = 0i32;
            let mut end: Option<usize> = None;
            for (i, ch) in head[paren..].char_indices() {
                if ch == '(' {
                    depth += 1;
                } else if ch == ')' {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(paren + i);
                        break;
                    }
                }
            }
            let Some(end) = end else { continue };
            let params = split_cpp_top_level(&head[paren + 1..end]);
            let ty = params.last().and_then(|last| {
                let last = re!(r"^\s*(?:noinline|crossinline)\s+").replace_first(last, |_| String::new());
                re!(r":\s*([\s\S]+?)(?:\s*=\s*[^=>][\s\S]*)?$").captures(&last).and_then(|c| c.text(1).map(String::from))
            });
            if let Some(receiver) = ty.and_then(|t| receiver_of_lambda_type(&t, 0, ctx)) {
                if !found.contains(&receiver) {
                    found.push(receiver);
                }
            }
        }
        if found.len() == 1 {
            found.pop()
        } else {
            None
        }
    })
}

fn receiver_of_lambda_type(ty: &str, depth: u32, ctx: &Ctx) -> Option<String> {
    if let Some(direct) = re!(r"^\s*(?:suspend\s+)?([A-Z]\w*)(?:<[^<>]*(?:<[^<>]*>[^<>]*)*>)?\s*\.\s*\(").captures(ty).and_then(|c| c.text(1).map(String::from)) {
        return Some(direct);
    }
    let alias = re!(r"^\s*([A-Z]\w*)\b").captures(ty).and_then(|c| c.text(1).map(String::from))?;
    if depth > 2 {
        return None;
    }
    for decl in ctx.get_nodes_by_name(&alias).iter() {
        if decl.language != Language::Kotlin || decl.kind != NodeKind::TypeAlias {
            continue;
        }
        let lines = lines_of(ctx, &decl.file_path);
        let text = (decl.start_line as usize).checked_sub(1).and_then(|i| lines.get(i)).cloned().unwrap_or_default();
        if let Some(rhs) = re!(r"=\s*(.+)$").captures(&text).and_then(|c| c.text(1).map(String::from)) {
            return receiver_of_lambda_type(&rhs, depth + 1, ctx);
        }
    }
    None
}

/// The types a brace block's head puts in scope: a class / interface / object (its own name
/// and its supertypes), or an extension function (its receiver type).
fn kotlin_head_names(head: &str) -> Option<Vec<String>> {
    let mut depth = 0i32;
    let mut flat = String::new();
    for ch in head.chars() {
        if ch == '<' || ch == '(' {
            depth += 1;
        } else if ch == '>' || ch == ')' {
            depth = (depth - 1).max(0);
        } else if depth == 0 {
            flat.push(ch);
        }
    }
    if let Some(decl) = re!(r"\b(?:class|interface|object)\b(?:\s+([A-Za-z_]\w*))?([^=]*)$").captures(&flat) {
        let tail = decl.text(2).unwrap_or("");
        let supers = re!(r":\s*([\s\S]*)$").captures(tail).and_then(|c| c.text(1).map(String::from)).unwrap_or_default();
        let supers = re!(r"\bwhere\b[\s\S]*$").replace_first(&supers, |_| String::new());
        let mut names: Vec<String> = re!(r"([A-Z]\w*)\s*(?=,|$|\bby\b)", "g").captures_iter(&supers).filter_map(|m| m.text(1).map(String::from)).collect();
        if let Some(own) = decl.text(1).filter(|s| !s.is_empty()) {
            names.insert(0, own.to_string());
        }
        return Some(names);
    }
    re!(r"\bfun\s+(?:<[^>]*>\s*)?([A-Z]\w*)(?:<[^>]*>)?\??\.[A-Za-z_]\w*\s*\(").captures(head).and_then(|c| c.text(1).map(|s| vec![s.to_string()]))
}

/// Whether a Kotlin/Java method found by name is one a receiver-less call at the reference can
/// reach: a member of an enclosing class, its supertypes or a scope-function receiver, one of
/// the types lambdas-with-receiver and extensions are written on, or an imported object's.
pub fn is_kotlin_member_reachable(n: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    // A `.kts` script's top level has no class: a method needs an owner that is not a project type.
    if r.file_path.ends_with(".kts") {
        if !matches!(n.kind, NodeKind::Method | NodeKind::Field | NodeKind::Property) {
            return true;
        }
        let owner = n.qualified_name.rfind("::").filter(|c| *c > 0).map(|c| last_owner_segment(&n.qualified_name[..c])).unwrap_or("");
        return !owner.is_empty() && !ctx.get_nodes_by_name(owner).iter().any(|c| is_method_owner_kind(c) || c.kind == NodeKind::Module);
    }
    if n.kind != NodeKind::Method || !matches!(n.language, Language::Kotlin | Language::Java) {
        return true;
    }
    if is_kotlin_precondition_call(r, ctx) {
        return false;
    }
    let Some(cut) = n.qualified_name.rfind("::").filter(|c| *c > 0) else { return true };
    let mut path: Vec<&str> = n.qualified_name[..cut].split("::").flat_map(|s| s.split('.')).collect();
    let mut owner = path.pop().unwrap_or("").to_string();
    let companion = owner == "Companion" && !path.is_empty();
    if companion {
        owner = path.pop().unwrap_or("").to_string();
    }
    // A constructor-like call: the class's own name.
    if n.name == owner {
        return true;
    }
    let hierarchy = kotlin_hierarchy_at(r, ctx);
    if hierarchy.contains("*") || kotlin_receiver_types(ctx).contains(&owner) || hierarchy.contains(&owner) {
        return true;
    }
    // A member of an `object` brought in by name: `import pkg.Obj.member` / `import pkg.Obj.*`.
    let pkg = kotlin_file_scope(&n.file_path, ctx).pkg.clone();
    let object_path = format!("{}{}", if pkg.is_empty() { owner.clone() } else { format!("{pkg}.{owner}") }, if companion { ".Companion" } else { "" });
    let here = kotlin_file_scope(&r.file_path, ctx);
    here.imports.contains(&format!("{object_path}.{}", n.name)) || here.stars.contains(&object_path)
}

/// Of several same-named members, the ones lexically in scope (the hierarchy at the call).
pub fn lexical_kotlin_members(candidates: &[NodeRef], r: &UnresolvedRef, ctx: &Ctx) -> Vec<NodeRef> {
    if candidates.len() < 2 {
        return candidates.to_vec();
    }
    let hierarchy = kotlin_hierarchy_at(r, ctx);
    if hierarchy.contains("*") {
        return candidates.to_vec();
    }
    let lexical: Vec<NodeRef> = candidates
        .iter()
        .filter(|n| {
            if n.kind != NodeKind::Method {
                return false;
            }
            let cut = n.qualified_name.rfind("::").unwrap_or(0);
            let mut path: Vec<&str> = n.qualified_name[..cut].split("::").flat_map(|s| s.split('.')).collect();
            let mut owner = path.pop().unwrap_or("");
            if owner == "Companion" {
                owner = path.pop().unwrap_or("");
            }
            hierarchy.contains(owner)
        })
        .cloned()
        .collect();
    if lexical.is_empty() {
        candidates.to_vec()
    } else {
        lexical
    }
}

/// A method named like a stdlib one (`map`, `let`, `first`) written on a receiver is the
/// project's only when the receiver is `this` or shares a word with the owner.
pub fn is_kotlin_std_chain_target(n: &Node, receiver: &str) -> bool {
    if !matches!(n.kind, NodeKind::Method | NodeKind::Function) {
        return true;
    }
    receiver == "this" || (!receiver.is_empty() && shares_receiver_word(receiver, &n.qualified_name))
}

/// A Kotlin file's package, plain imports and star imports.
pub struct KotlinFileScope {
    pub pkg: String,
    pub imports: HashSet<String>,
    pub stars: HashSet<String>,
}

memo_map!(KotlinFileScopeMemo, String, Rc<KotlinFileScope>);

const KOTLIN_DEFAULT_IMPORTS: &[&str] = &[
    "kotlin", "kotlin.annotation", "kotlin.collections", "kotlin.comparisons", "kotlin.io", "kotlin.ranges", "kotlin.sequences", "kotlin.text", "kotlin.jvm",
    "java.lang", "kotlin.js",
];

pub fn kotlin_file_scope(file: &str, ctx: &Ctx) -> Rc<KotlinFileScope> {
    memoized::<KotlinFileScopeMemo>(ctx, file.to_string(), || {
        let raw = ctx.read_file(file).map(|s| s.to_string()).unwrap_or_default();
        let text = re!(r"/\*[\s\S]*?\*/", "g").replace_all_str(&raw, " ").replace('`', "");
        let pkg = re!(r"^\s*package\s+([\w.]+)", "m").captures(&text).and_then(|c| c.text(1).map(String::from)).unwrap_or_default();
        let mut imports = HashSet::new();
        let mut stars = HashSet::new();
        for m in re!(r"^\s*import\s+([\w.]+?)(\.\*)?(?:\s+as\s+\w+)?\s*;?\s*(?://.*)?$", "gm").captures_iter(&text) {
            let name = m.text(1).unwrap_or("").to_string();
            if m.text(2).is_some_and(|s| !s.is_empty()) {
                stars.insert(name);
            } else {
                imports.insert(name);
            }
        }
        Rc::new(KotlinFileScope { pkg, imports, stars })
    })
}

fn is_kotlin_enclosing_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Class
            | NodeKind::Interface
            | NodeKind::Enum
            | NodeKind::Struct
            | NodeKind::Trait
            | NodeKind::Protocol
            | NodeKind::Module
            | NodeKind::Namespace
            | NodeKind::Function
            | NodeKind::Method
    )
}

/// A top-level Kotlin declaration of another file is visible from the same package, a star
/// import of its package, a named import, or the default imports.
pub fn is_kotlin_top_level_visible(n: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if n.language != Language::Kotlin || n.file_path == r.file_path {
        return true;
    }
    let enclosed = ctx
        .get_nodes_in_file(&n.file_path)
        .iter()
        .any(|o| o.id != n.id && is_kotlin_enclosing_kind(o.kind) && o.start_line <= n.start_line && o.end_line >= n.end_line && (o.start_line < n.start_line || o.end_line > n.end_line));
    if enclosed {
        return true;
    }
    let pkg = kotlin_file_scope(&n.file_path, ctx).pkg.clone();
    let here = kotlin_file_scope(&r.file_path, ctx);
    pkg == here.pkg
        || here.stars.contains(&pkg)
        || here.imports.contains(&if pkg.is_empty() { n.name.clone() } else { format!("{pkg}.{}", n.name) })
        || KOTLIN_DEFAULT_IMPORTS.contains(&pkg.as_str())
}

/// The Kotlin contribution to [`super::lang_shape::LangShape`].
pub struct KotlinShape {
    call: bool,
    bare: bool,
    std_chain: Option<String>,
}

impl KotlinShape {
    pub fn of(r: &UnresolvedRef, ctx: &Ctx) -> Option<KotlinShape> {
        if r.language != Language::Kotlin {
            return None;
        }
        let call = is_calls(r) && re!(r"^[A-Za-z_$][\w$]*$").is_match(&r.reference_name);
        let bare = call && is_receiver_less_kotlin_call(r, ctx);
        let std_chain = if is_calls(r) && KOTLIN_STD_METHODS.contains(r.reference_name.as_str()) { kotlin_chain_receiver(r, ctx) } else { None };
        Some(KotlinShape { call, bare, std_chain })
    }
}

impl RefShape for KotlinShape {
    fn excludes(&self, n: &NodeRef, r: &UnresolvedRef, ctx: &Ctx) -> bool {
        self.std_chain.as_deref().is_some_and(|recv| !is_kotlin_std_chain_target(n, recv))
            || (self.call && !is_kotlin_top_level_visible(n, r, ctx))
            || (self.bare && !is_kotlin_member_reachable(n, r, ctx))
            || is_kotlin_number_bitwise(n, r)
    }

    fn rank_exact(&self, pool: &[NodeRef], r: &UnresolvedRef, ctx: &Ctx) -> Option<Vec<NodeRef>> {
        self.bare.then(|| lexical_kotlin_members(pool, r, ctx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_owner_segment_splits_on_both_separators() {
        assert_eq!(last_owner_segment("a::b.C"), "C");
        assert_eq!(last_owner_segment("a.b::C"), "C");
        assert_eq!(last_owner_segment("Plain"), "Plain");
    }

    #[test]
    fn head_names_read_class_supertypes_and_extension_receivers() {
        assert_eq!(kotlin_head_names("class Foo(val x: Int) : Base(), Iface by impl "), Some(vec!["Foo".into(), "Base".into(), "Iface".into()]));
        assert_eq!(kotlin_head_names("fun Bar<T>.baz(x: Int) "), Some(vec!["Bar".into()]));
        assert_eq!(kotlin_head_names("val x = foo "), None);
    }

    #[test]
    fn infix_call_finds_the_name_after_its_operand() {
        let line = "val y = x shl 2";
        let m = infix_call("shl").captures(line).and_then(|c| c.get(2)).expect("infix call found");
        assert_eq!(&line[..m.start()], "val y = x ");
        assert!(infix_call("shl").captures("val y = x shl = 2").is_none());
    }
}
