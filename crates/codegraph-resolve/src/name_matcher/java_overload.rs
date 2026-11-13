//! A Java call's arguments pick its overload (upstream be06c5e2, 6aa358cc), and a lambda's
//! parameter is typed by the method it is passed to.
//!
//! [`java_overloads_fitting`] keeps the overloads of the call's count, reads each argument's static
//! type where its text says (a literal, a cast, a `new`, `this`, a local or parameter, a field of the
//! class or one it inherits, `x.f` and `Type.FIELD`, an array element, a call on one of those, a
//! lambda or method reference), and scores it against each parameter as Java does: the same type,
//! widening before boxing, a project type's supertypes, variable arity last. An overload the
//! compiler would refuse goes, and a private one goes from another file.

use super::cpp_text::split_cpp_top_level;
use super::java::java_ancestor_names;
use super::java_receiver::{
    is_java_type_nameable, java_static_field, java_type_name, java_type_ref, jvm_type_decls, resolve_on_java_type, JavaTypeRef,
};
use super::js_store::enclosing_scope_start_line;
use super::members::angle_arguments;
use super::reach::is_anonymous_class;
use super::util::{lines_of, memo_map, memoized};
use crate::ctx::Ctx;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::{Language, Node, NodeKind, Visibility};
use std::collections::BTreeSet;
use std::rc::Rc;

/// A parameter of a Java method: its type (generics and package dropped, array markers kept), its
/// type as declared, and whether it is `...`.
#[derive(Clone, Debug)]
pub struct JavaParameter {
    pub ty: String,
    pub written: String,
    pub varargs: bool,
}

const JAVA_PRIMITIVE_TYPES: &[&str] = &["boolean", "byte", "char", "short", "int", "long", "float", "double"];
const JAVA_STRING_SUPERTYPES: &[&str] = &["CharSequence", "Comparable", "Serializable", "Constable", "ConstantDesc"];
const JAVA_STATEMENT_WORDS: &[&str] = &["return", "throw", "new", "else", "case", "yield", "assert", "instanceof", "final"];
/// What javaArgumentType says of a method reference, and of a lambda with its parameter count after it.
const JAVA_FUNCTION_VALUE: &str = "->";

fn is_primitive(t: &str) -> bool {
    JAVA_PRIMITIVE_TYPES.contains(&t)
}

fn boxed(p: &str) -> Option<&'static str> {
    Some(match p {
        "boolean" => "Boolean",
        "byte" => "Byte",
        "char" => "Character",
        "short" => "Short",
        "int" => "Integer",
        "long" => "Long",
        "float" => "Float",
        "double" => "Double",
        _ => return None,
    })
}

fn widened(p: &str) -> &'static [&'static str] {
    match p {
        "byte" => &["short", "int", "long", "float", "double"],
        "short" | "char" => &["int", "long", "float", "double"],
        "int" => &["long", "float", "double"],
        "long" => &["float", "double"],
        "float" => &["double"],
        _ => &[],
    }
}

/// The JDK's functional interfaces with a type argument per lambda parameter.
fn functional_parameters(name: &str) -> Option<&'static [usize]> {
    Some(match name {
        "Consumer" | "Predicate" | "Function" | "UnaryOperator" | "ToIntFunction" | "ToLongFunction" | "ToDoubleFunction" | "ObjIntConsumer"
        | "ObjLongConsumer" | "ObjDoubleConsumer" => &[0],
        "BinaryOperator" | "Comparator" => &[0, 0],
        "BiConsumer" | "BiFunction" | "BiPredicate" => &[0, 1],
        _ => return None,
    })
}

fn is_functional_type(t: &str) -> bool {
    re!(r"^(?:Runnable|Callable|Comparator|(?:Bi|Boolean|Double|Int|Long|Obj(?:Double|Int|Long)|To(?:Double|Int|Long)(?:Bi)?|(?:Double|Int|Long)To(?:Double|Int|Long))?(?:Consumer|Function|Predicate|Supplier)|(?:Double|Int|Long)?(?:Binary|Unary)Operator)$").is_match(t)
}

fn strip_dims(t: &str) -> &str {
    let mut s = t;
    while let Some(rest) = s.strip_suffix("[]") {
        s = rest;
    }
    s
}

/// `javaBareType`: no type arguments or package, array markers kept.
pub fn java_bare_type(written: &str) -> String {
    let mut ty = written.to_string();
    loop {
        let next = re!(r"<[^<>]*>", "g").replace_all_str(&ty, "");
        if next == ty {
            break;
        }
        ty = next;
    }
    let ty: String = ty.chars().filter(|c| !c.is_whitespace()).collect();
    let base = strip_dims(&ty);
    let dims = &ty[base.len()..];
    let simple = base.rfind('.').map_or(base, |d| &base[d + 1..]);
    format!("{simple}{dims}")
}

/// `javaParameters`: what a Java method's signature (`Node (String html)`) declares.
pub fn java_parameters(signature: Option<&str>) -> Option<Vec<JavaParameter>> {
    let signature = signature?;
    let open = signature.find('(')?;
    let close = signature.rfind(')')?;
    if close < open {
        return None;
    }
    let list = re!(r"/\*[\s\S]*?\*/", "g").replace_all_str(&signature[open + 1..close], " ");
    let list = list.trim();
    if list.is_empty() {
        return Some(Vec::new());
    }
    let mut params = Vec::new();
    for written in split_cpp_top_level(list) {
        let plain = re!(r"@[\w.]+(?:\([^()]*\))?", "g").replace_all_str(&written, " ");
        let plain = re!(r"\bfinal\b", "g").replace_all_str(&plain, " ");
        let plain = plain.trim();
        let m = re!(r"^([\s\S]+?)(\.\.\.)?\s+[\w$]+((?:\s*\[\s*\])*)$")
            .captures(plain)
            .or_else(|| re!(r"^([\s\S]+?)(\.\.\.)\s*[\w$]+()$").captures(plain))?;
        let head = m.text(1).unwrap_or("");
        params.push(JavaParameter {
            ty: java_bare_type(&format!("{head}{}", m.text(3).unwrap_or(""))),
            written: head.to_string(),
            varargs: m.text(2).is_some(),
        });
    }
    Some(params)
}

memo_map!(JavaParametersMemo, String, Option<Rc<Vec<JavaParameter>>>);

fn java_overload_parameters(n: &Node, ctx: &Ctx) -> Option<Rc<Vec<JavaParameter>>> {
    memoized::<JavaParametersMemo>(ctx, n.id.clone(), || {
        if n.language == Language::Java {
            java_parameters(n.signature.as_deref()).map(Rc::new)
        } else {
            None
        }
    })
}

fn is_java_visible_type_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Class | NodeKind::Interface | NodeKind::Enum)
}

/// The type classes around `r`, the tightest first.
fn types_around(r: &UnresolvedRef, ctx: &Ctx) -> Vec<NodeRef> {
    let mut around: Vec<NodeRef> = ctx
        .get_nodes_in_file(&r.file_path)
        .iter()
        .filter(|n| is_java_visible_type_kind(n.kind) && n.start_line <= r.line && n.end_line >= r.line)
        .cloned()
        .collect();
    around.sort_by(|a, b| b.start_line.cmp(&a.start_line));
    around
}

/// `javaLambdaParameterType`: the type of a Java lambda's parameter `name`, read off the method the
/// lambda is passed to: jsoup's `assertAdoptionRecovery(new HtmlTreeBuilder(), tb -> {…})` hands
/// `tb` to a `Consumer<HtmlTreeBuilder>`. Only for a receiver-less call to methods of the calling
/// file, which must agree on a JDK functional interface with a type argument of its own.
pub fn java_lambda_parameter_type(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    if !re!(r"^[A-Za-z_$][\w$]*$").is_match(name) {
        return None;
    }
    let lines = lines_of(ctx, &r.file_path);
    let start = (enclosing_scope_start_line(r, ctx) as usize).saturating_sub(1);
    let n = escape_regex(name);
    let head = Re::cached(&format!(r"(?<![\w$.])(?:{n}|\(\s*((?:[\w$]+\s*,\s*)*){n}(?:\s*,\s*[\w$]+)*\s*\))\s*->"), "");
    let top = (r.line as usize).min(lines.len());
    let mut i = top;
    while i > start {
        i -= 1;
        let line = &lines[i];
        if !line.contains("->") || !line.contains(name) {
            continue;
        }
        let Some(m) = head.captures(line) else { continue };
        let whole = m.get(0)?;
        // Back from the lambda to the `(` of the call it is an argument of.
        let text: Vec<char> = lines[start..=i].join("\n").chars().collect();
        let line_chars = line.chars().count();
        let mut at = (text.len() - line_chars + line[..whole.start()].chars().count()) as isize;
        let mut index = 0usize;
        let mut depth = 0i64;
        loop {
            at -= 1;
            if at < 0 {
                break;
            }
            let c = text[at as usize];
            if matches!(c, ')' | ']' | '}') {
                depth += 1;
            } else if matches!(c, '(' | '[' | '{') {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            } else if c == ',' && depth == 0 {
                index += 1;
            }
        }
        if at < 0 || text[at as usize] != '(' {
            return None;
        }
        let before: String = text[..at as usize].iter().collect();
        let called = re!(r"(?:^|[^\w$.])(?:this\s*\.\s*)?([A-Za-z_$][\w$]*)\s*(?:<[^<>()]*>)?\s*$").captures(&before)?.text(1)?.to_string();
        let position = m.text(1).map_or(0, |s| s.matches(',').count());
        let mut found: BTreeSet<String> = BTreeSet::new();
        for method in ctx.get_nodes_in_file(&r.file_path).iter() {
            if method.kind != NodeKind::Method || method.name != called {
                continue;
            }
            let Some(written) = java_parameters(method.signature.as_deref()).and_then(|ps| ps.get(index).map(|p| p.written.clone())) else {
                continue;
            };
            let functional = re!(r"^(?:[\w$]+\.)*([A-Z][\w$]*)\s*<").captures(&written).and_then(|c| c.text(1).map(|s| s.to_string()));
            let Some(type_argument) = functional.as_deref().and_then(functional_parameters).and_then(|ps| ps.get(position).copied()) else {
                continue;
            };
            let argument = angle_arguments(&written).get(type_argument).map(|a| re!(r"^\?\s*(?:super|extends)\s+").replace_all_str(a, ""));
            match argument {
                Some(a) if !a.is_empty() && a != "?" && !re!(r"^[A-Z][A-Z0-9]{0,2}$").is_match(&a) => {
                    found.insert(a);
                }
                _ => return None,
            }
        }
        return if found.len() == 1 { found.into_iter().next() } else { None };
    }
    None
}

/// `javaLocalDeclaredType`: the nearest declaration of a local or parameter above `r`; `var` as written.
fn java_local_declared_type(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let lines = lines_of(ctx, &r.file_path);
    let start = enclosing_scope_start_line(r, ctx) as usize;
    let declared = Re::cached(
        &format!(
            r"((?:[A-Za-z_$][\w$]*\.)*[A-Za-z_$][\w$]*(?:\s*<[^;=(){{}}]*>)?(?:\s*\[\s*\])*)(\s*\.\.\.)?\s+{}((?:\s*\[\s*\])*)\s*(?:[=;,):]|$)",
            escape_regex(name)
        ),
        "",
    );
    let low = start.saturating_sub(1);
    let mut i = (r.line as usize).min(lines.len());
    while i > low {
        i -= 1;
        let line = &lines[i];
        if !line.contains(name) || re!(r"^\s*(?://|\*|/\*)").is_match(line) {
            continue;
        }
        let Some(m) = declared.captures(line) else { continue };
        let head = m.text(1).unwrap_or("");
        if head == "var" {
            return Some("var".to_string());
        }
        if JAVA_STATEMENT_WORDS.contains(&head) {
            continue;
        }
        let ty = java_bare_type(&format!("{head}{}{}", if m.text(2).is_some() { "[]" } else { "" }, m.text(3).unwrap_or("")));
        if ty.starts_with(|c: char| c.is_ascii_uppercase()) || is_primitive(strip_dims(&ty)) {
            return Some(ty);
        }
    }
    None
}

/// `javaFieldType`: the type a field, a constant or an enum's constant holds.
fn java_field_type(field: &Node) -> Option<String> {
    if field.kind == NodeKind::EnumMember {
        let parts: Vec<&str> = field.qualified_name.split("::").collect();
        return (parts.len() >= 2).then(|| parts[parts.len() - 2].to_string());
    }
    let sig = field.signature.as_deref()?;
    let cut = match sig.rfind(&field.name) {
        Some(i) => &sig[..i],
        None => sig.char_indices().last().map_or("", |(i, _)| &sig[..i]),
    };
    let ty = java_bare_type(cut);
    (!ty.is_empty()).then_some(ty)
}

/// The declared type of a field of a class around `r`, own or inherited, the tightest class first.
fn java_field_declared_type(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    for cls in types_around(r, ctx) {
        if let Some(field) = java_static_field(cls, name, ctx) {
            return java_field_type(&field);
        }
    }
    None
}

/// A local's or a parameter's type, else a field's. A `var` hides the field and says nothing.
fn java_named_type(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    match java_local_declared_type(name, r, ctx) {
        Some(t) if t == "var" => None,
        Some(t) => Some(t),
        None => java_field_declared_type(name, r, ctx),
    }
}

fn java_receiver_type(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    if re!(r"^[A-Z].*[a-z]").is_match(name) {
        Some(name.to_string())
    } else {
        java_named_type(name, r, ctx)
    }
}

fn project_type_ref(ty: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<JavaTypeRef> {
    let written = java_type_name(ty)?;
    java_type_ref(&written, &r.file_path, ctx)
}

/// The type of `x.name` or `Type.name`, where `x` has a project type or is an array.
fn java_field_access_type(receiver: &str, name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let ty = java_receiver_type(receiver, r, ctx)?;
    if ty.ends_with("[]") {
        return (name == "length").then(|| "int".to_string());
    }
    let found = project_type_ref(&ty, r, ctx)?;
    let owners: Vec<NodeRef> = match &found.decl {
        Some(d) => vec![d.clone()],
        None => {
            let tail = found.name.rsplit("::").next().unwrap_or("");
            let suffix = format!("::{}", found.name);
            jvm_type_decls(tail, ctx)
                .into_iter()
                .filter(|d| {
                    d.language == Language::Java
                        && (d.qualified_name == found.name || d.qualified_name.ends_with(&suffix))
                        && is_java_type_nameable(d, &r.file_path, ctx)
                })
                .collect()
        }
    };
    if owners.len() != 1 {
        return None;
    }
    java_static_field(owners[0].clone(), name, ctx).and_then(|f| java_field_type(&f))
}

/// Where the parentheses opened at byte `open` close; `None` when they don't.
fn java_closing_paren(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0i64;
    for (i, &b) in text.as_bytes().iter().enumerate().skip(open) {
        if b == b'(' {
            depth += 1;
        } else if b == b')' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// `javaCalledType`: what `x.m(…)` written as an argument returns, when `x` is a local, a
/// parameter, a field or a type, and `m`'s overloads of that count agree.
fn java_called_type(arg: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let call = re!(r"^(?:this\s*\.\s*)?([A-Za-z_$][\w$]*)\s*\.\s*([A-Za-z_$][\w$]*)\s*\(").captures(arg)?;
    let head_end = call.get(0)?.end();
    let close = java_closing_paren(arg, head_end - 1)?;
    if close != arg.len() - 1 {
        return None;
    }
    let declared = java_receiver_type(call.text(1)?, r, ctx)?;
    let ty = project_type_ref(&declared, r, ctx)?;
    let found = resolve_on_java_type(&ty, call.text(2)?, r, ctx)?;
    let method = ctx.node_by_id(&found.target_node_id)?;
    let list = arg[head_end..close].trim();
    let count = if list.is_empty() { 0 } else { split_cpp_top_level(list).len() };
    let overloads: Vec<NodeRef> = ctx
        .get_nodes_in_file(&method.file_path)
        .iter()
        .filter(|n| {
            n.kind == NodeKind::Method
                && n.qualified_name == method.qualified_name
                && java_parameters(n.signature.as_deref()).is_some_and(|p| p.len() == count)
        })
        .cloned()
        .collect();
    let returned = overloads.first()?.return_type.clone()?;
    overloads.iter().all(|n| n.return_type.as_deref() == Some(returned.as_str())).then(|| java_bare_type(&returned))
}

/// `javaArgumentType`: the static type of a Java argument where its text alone says; a lambda or a
/// method reference is [`JAVA_FUNCTION_VALUE`] (a lambda with its parameter count after it).
fn java_argument_type(arg: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    if let Some(l) = re!(r"^(?:\(([^()]*)\)|[A-Za-z_$][\w$]*)\s*->").captures(arg) {
        let count = match l.text(1) {
            None => 1,
            Some(p) if p.trim().is_empty() => 0,
            Some(p) => split_cpp_top_level(p).len(),
        };
        return Some(format!("{JAVA_FUNCTION_VALUE}{count}"));
    }
    if re!(r"^[\w$.]+(?:<[^()]*>)?(?:\[\])*\s*::\s*[A-Za-z_$][\w$]*$").is_match(arg) {
        return Some(JAVA_FUNCTION_VALUE.to_string());
    }
    let lit = |t: &str| Some(t.to_string());
    if re!(r#"^"(?:[^"\\]|\\.)*"(?:\s*\+[\s\S]*)?$"#).is_match(arg) {
        return lit("String");
    }
    if re!(r"^'(?:[^'\\]|\\.)+'$").is_match(arg) {
        return lit("char");
    }
    if arg == "true" || arg == "false" {
        return lit("boolean");
    }
    if re!(r"^-?(?:0[xX][\da-fA-F_]+|0[bB][01_]+|\d[\d_]*)$").is_match(arg) {
        return lit("int");
    }
    if re!(r"^-?(?:0[xX][\da-fA-F_]+|0[bB][01_]+|\d[\d_]*)[lL]$").is_match(arg) {
        return lit("long");
    }
    if re!(r"^-?(?:\d[\d_]*(?:\.[\d_]*)?|\.\d[\d_]*)(?:[eE][-+]?\d+)?[fF]$").is_match(arg) {
        return lit("float");
    }
    if re!(r"^-?(?:(?:\d[\d_]*\.[\d_]*|\.\d[\d_]*)(?:[eE][-+]?\d+)?[dD]?|\d[\d_]*(?:[eE][-+]?\d+[dD]?|[dD]))$").is_match(arg) {
        return lit("double");
    }
    // (Only when nothing follows it: `new Foo().bar()` is what `bar` returns.)
    if let Some(built) = re!(r"^new\s+([\w$.]+)\s*(?:<[^()]*>)?\s*(\(|\[)").captures(arg) {
        let ty = java_bare_type(built.text(1)?);
        if built.text(2) == Some("[") {
            return Some(format!("{ty}[]"));
        }
        let end = java_closing_paren(arg, built.get(0)?.end() - 1)?;
        return re!(r"^\s*(?:\{[\s\S]*\})?$").is_match(&arg[end + 1..]).then_some(ty);
    }
    if let Some(cast) = re!(r#"^\(\s*([\w$.]+(?:\s*<[^()]*>)?(?:\s*\[\s*\])*)\s*\)\s*[\w$("']"#).captures(arg) {
        return Some(java_bare_type(cast.text(1)?));
    }
    if arg == "this" {
        let around = types_around(r, ctx).into_iter().next()?;
        return (!is_anonymous_class(&around)).then(|| around.name.clone());
    }
    if arg == "null" {
        return None;
    }
    if let Some(own) = re!(r"^this\s*\.\s*([A-Za-z_$][\w$]*)$").captures(arg) {
        return java_field_declared_type(own.text(1)?, r, ctx);
    }
    if let Some(el) = re!(r"^([A-Za-z_$][\w$]*)\s*\[[^\[\]]*\]$").captures(arg) {
        let array = java_named_type(el.text(1)?, r, ctx)?;
        return array.strip_suffix("[]").map(|s| s.to_string());
    }
    if let Some(f) = re!(r"^([A-Za-z_$][\w$]*)\s*\.\s*([A-Za-z_$][\w$]*)$").captures(arg) {
        return java_field_access_type(f.text(1)?, f.text(2)?, r, ctx);
    }
    if re!(r"^[A-Za-z_$][\w$]*$").is_match(arg) {
        java_named_type(arg, r, ctx)
    } else {
        java_called_type(arg, r, ctx)
    }
}

fn java_project_types(name: &str, ctx: &Ctx) -> Vec<NodeRef> {
    jvm_type_decls(strip_dims(name), ctx).into_iter().filter(|n| n.language == Language::Java).collect()
}

/// `javaFunctionFit`: 1 a JDK functional interface taking the lambda's count, 0 what may be one,
/// -1 what can't take one.
fn java_function_fit(arg: &str, param: &str, ctx: &Ctx) -> f64 {
    if is_functional_type(param) {
        let takes = if re!(r"Supplier$|^Runnable$|^Callable$").is_match(param) {
            0
        } else if re!(r"^Bi|BiFunction$|Binary|^Obj|^Comparator$").is_match(param) {
            2
        } else {
            1
        };
        return if arg == JAVA_FUNCTION_VALUE || arg == format!("{JAVA_FUNCTION_VALUE}{takes}") { 1.0 } else { -1.0 };
    }
    if param == "Object" || param == "String" || param.ends_with("[]") || is_primitive(param) {
        return -1.0;
    }
    let decls: Vec<NodeRef> = jvm_type_decls(param, ctx).into_iter().filter(|n| n.language == Language::Java).collect();
    if !decls.is_empty() && decls.iter().all(|n| n.kind != NodeKind::Interface) {
        -1.0
    } else {
        0.0
    }
}

/// `javaArgumentFit`: 2 the same type, 1.5 widening, 1 boxing or a supertype, 0 unknown, -1 refused.
fn java_argument_fit(arg: &str, param: &str, ctx: &Ctx) -> f64 {
    if arg.starts_with(JAVA_FUNCTION_VALUE) {
        return java_function_fit(arg, param, ctx);
    }
    if arg == param {
        return 2.0;
    }
    let type_variable = re!(r"^[A-Z][A-Z0-9]{0,2}(?:\[\])*$");
    if type_variable.is_match(arg) || type_variable.is_match(param) {
        return 0.0;
    }
    // A name the project doesn't declare may be a type parameter (`Item`), which takes anything.
    let refused = |t: &str| if t == "String" || !java_project_types(t, ctx).is_empty() { -1.0 } else { 0.0 };
    // A number or a string is no function.
    let value_refused = |t: &str| if is_functional_type(t) { -1.0 } else { refused(t) };
    let arg_primitive = is_primitive(arg);
    let param_primitive = is_primitive(param);
    if arg_primitive && param_primitive {
        return if widened(arg).contains(&param) { 1.5 } else { -1.0 };
    }
    if arg_primitive {
        return if Some(param) == boxed(arg) || matches!(param, "Object" | "Number" | "Comparable" | "Serializable") { 1.0 } else { value_refused(param) };
    }
    if param_primitive {
        let unboxed = JAVA_PRIMITIVE_TYPES.iter().find(|p| boxed(p) == Some(arg));
        return match unboxed {
            Some(u) if *u == param || widened(u).contains(&param) => 1.0,
            _ => -1.0,
        };
    }
    if param == "Object" {
        return 1.0;
    }
    if arg.ends_with("[]") != param.ends_with("[]") {
        return if param == "Serializable" || param == "Cloneable" {
            1.0
        } else if param.ends_with("[]") {
            -1.0
        } else {
            refused(param)
        };
    }
    if arg == "String" {
        return if JAVA_STRING_SUPERTYPES.contains(&param) { 1.0 } else { value_refused(param) };
    }
    let bare = strip_dims(param);
    if java_project_types(arg, ctx).iter().any(|d| java_ancestor_names(&d.qualified_name, ctx).borrow().contains(bare)) {
        return 1.0;
    }
    refused(param)
}

/// `javaOverloadsFitting`: of one class's overloads of a method, those a call's arguments fit, best
/// first; a private one only from its own file. `None` when a signature can't be read or none fits.
pub fn java_overloads_fitting(overloads: &[NodeRef], written: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<Vec<NodeRef>> {
    let args: Vec<String> = if written.trim().is_empty() { Vec::new() } else { split_cpp_top_level(written) };
    let mut counted: Vec<(NodeRef, Rc<Vec<JavaParameter>>)> = Vec::new();
    for n in overloads {
        let params = java_overload_parameters(n, ctx)?;
        if n.visibility == Some(Visibility::Private) && n.file_path != r.file_path {
            continue;
        }
        let fits = match params.last() {
            Some(last) if last.varargs => args.len() + 1 >= params.len(),
            _ => args.len() == params.len(),
        };
        if fits {
            counted.push((n.clone(), params));
        }
    }
    if counted.len() < 2 {
        return (counted.len() == 1).then(|| vec![counted[0].0.clone()]);
    }
    let types: Vec<Option<String>> = args.iter().map(|a| java_argument_type(a, r, ctx)).collect();
    let mut best = -1.0f64;
    let mut fitting: Vec<NodeRef> = Vec::new();
    for (n, params) in &counted {
        let mut score = 0.0f64;
        for (i, ty) in types.iter().enumerate() {
            if score < 0.0 {
                break;
            }
            let Some(ty) = ty else { continue };
            let param = &params[i.min(params.len() - 1)];
            let whole = i + 1 < params.len() || !param.varargs;
            let fit = if whole {
                java_argument_fit(ty, &param.ty, ctx)
            } else {
                java_argument_fit(ty, &format!("{}[]", param.ty), ctx).max(java_argument_fit(ty, &param.ty, ctx).min(1.0))
            };
            score = if fit < 0.0 { -1.0 } else { score + fit };
        }
        if score < 0.0 {
            continue;
        }
        // Java tries a call without variable arity first, if it can make that call.
        let rank = if params.last().is_some_and(|l| l.varargs) && types.iter().all(|t| t.is_some()) { score - 0.5 } else { score };
        if rank > best {
            best = rank;
            fitting = vec![n.clone()];
        } else if rank == best {
            fitting.push(n.clone());
        }
    }
    (!fitting.is_empty()).then_some(fitting)
}

/// The method a call on a lambda's parameter reaches, typed by [`java_lambda_parameter_type`].
pub fn match_java_lambda_parameter_call(object: &str, method_name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<crate::types::ResolvedRef> {
    let lambda = java_lambda_parameter_type(object, r, ctx)?;
    let ty = project_type_ref(&lambda, r, ctx)?;
    resolve_on_java_type(&ty, method_name, r, ctx)
}
