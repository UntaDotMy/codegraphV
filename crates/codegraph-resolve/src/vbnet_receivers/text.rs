//! VB.NET source reading: declared types, what one statement binds a name to, and a file's
//! lines as code. Port of the first third of `src/resolution/vbnet-receivers.ts`.

use crate::ctx::{js_lower, Ctx};
use crate::name_matcher::util::{memo_map, MemoMap};
use crate::types::UnresolvedRef;
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::js_trim;
use codegraph_types::NodeKind;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

/// A VB.NET declared type: its simple name (no namespace, no type arguments) and whether it is an array.
#[derive(Clone, Debug, Default)]
pub struct VbType {
    pub name: String,
    pub array: bool,
    /// The namespaces or types written before the name (`API.Base` in `API.Base.UserDataBase`), lowercased.
    pub qualifier: Option<Vec<String>>,
    /// Where the name is written, which decides the namespaces it is looked up in.
    pub file: Option<String>,
    pub line: Option<u32>,
    /// A supertype named in an `Implements` statement: its members are not the type's own.
    pub implemented: bool,
    /// The type arguments written with it: `SiteSettings` in `Checker(Of SiteSettings)`.
    pub args: Option<Vec<VbType>>,
}

impl VbType {
    pub fn new(name: impl Into<String>, array: bool) -> VbType {
        VbType { name: name.into(), array, ..Default::default() }
    }

    /// A bare name written at `file:line`.
    pub fn written_at(name: impl Into<String>, file: &str, line: u32) -> VbType {
        VbType { name: name.into(), file: Some(file.to_string()), line: Some(line), ..Default::default() }
    }
}

/// What a statement says a local is: its type, the call whose result it holds, or a binding that names no type.
#[derive(Clone, Debug)]
pub enum VbBinding {
    Type(VbType),
    Call { receiver: Option<String>, member: String },
    Each(String),
    Unknown,
}

/// The type of what a `For Each` over a value of type `t` binds: an array's or a .NET collection's element type.
pub fn element_type(t: &VbType) -> Option<VbType> {
    if t.array {
        return Some(VbType { array: false, ..t.clone() });
    }
    let args = t.args.as_ref()?;
    if args.len() == 1
        && re!(r"^(?:List|IList|IEnumerable|ICollection|IReadOnlyList|IReadOnlyCollection|HashSet|SortedSet|Queue|Stack|LinkedList|ObservableCollection|Collection|ReadOnlyCollection|BindingList|ConcurrentBag|ConcurrentQueue|ConcurrentStack|BlockingCollection)$", "i")
            .is_match(&t.name)
        && args[0].name != "?"
    {
        return Some(args[0].clone());
    }
    None
}

/// VB.NET's built-in types, by keyword and by .NET name, keyed to the keyword.
pub fn builtin_keyword(lower: &str) -> Option<&'static str> {
    Some(match lower {
        "boolean" => "boolean",
        "byte" => "byte",
        "char" => "char",
        "date" => "date",
        "decimal" => "decimal",
        "double" => "double",
        "integer" => "integer",
        "long" => "long",
        "object" => "object",
        "sbyte" => "sbyte",
        "short" => "short",
        "single" => "single",
        "string" => "string",
        "uinteger" => "uinteger",
        "ulong" => "ulong",
        "ushort" => "ushort",
        "int16" => "short",
        "int32" => "integer",
        "int64" => "long",
        "uint16" => "ushort",
        "uint32" => "uinteger",
        "uint64" => "ulong",
        "datetime" => "date",
        _ => return None,
    })
}

/// The type a type character declares: `Dim aStr$`, `For i% = 0 …`.
fn type_char_name(c: char) -> &'static str {
    match c {
        '$' => "String",
        '%' => "Integer",
        '&' => "Long",
        '!' => "Single",
        '#' => "Double",
        _ => "Decimal",
    }
}

/// VB.NET's conversion functions and the built-in type each returns.
fn conversion_type(lower: &str) -> Option<&'static str> {
    Some(match lower {
        "cbool" => "Boolean",
        "cbyte" => "Byte",
        "cchar" => "Char",
        "cdate" => "Date",
        "cdbl" => "Double",
        "cdec" => "Decimal",
        "cint" => "Integer",
        "clng" => "Long",
        "cobj" => "Object",
        "csbyte" => "SByte",
        "cshort" => "Short",
        "csng" => "Single",
        "cstr" => "String",
        "cuint" => "UInteger",
        "culng" => "ULong",
        "cushort" => "UShort",
        _ => return None,
    })
}

/// VB.NET type nodes a member can belong to (a `Module` is indexed as a class).
pub const VB_TYPE_KINDS: [NodeKind; 3] = [NodeKind::Class, NodeKind::Struct, NodeKind::Interface];
pub const VB_VALUE_KINDS: [NodeKind; 4] = [NodeKind::Field, NodeKind::Property, NodeKind::Constant, NodeKind::Variable];
pub const VB_MEMBER_KINDS: [NodeKind; 6] =
    [NodeKind::Method, NodeKind::Property, NodeKind::Field, NodeKind::EnumMember, NodeKind::Constant, NodeKind::Variable];
pub const VB_TYPE_LIKE_KINDS: [NodeKind; 5] = [NodeKind::Class, NodeKind::Struct, NodeKind::Interface, NodeKind::Enum, NodeKind::TypeAlias];
/// The types a value is read through: those a shared call is made on, and an `Enum`.
pub const VB_READ_TYPE_KINDS: [NodeKind; 4] = [NodeKind::Class, NodeKind::Struct, NodeKind::Interface, NodeKind::Enum];

/// A type's key for comparing two of them: the built-in keyword or the lowercased name, `[]` for an array.
pub fn type_key(t: &VbType) -> String {
    let lower = js_lower(&t.name);
    let base = builtin_keyword(&lower).map(String::from).unwrap_or(lower);
    if t.array {
        format!("{base}[]")
    } else {
        base
    }
}

pub fn is_builtin(t: &VbType) -> bool {
    !t.array && builtin_keyword(&js_lower(&t.name)).is_some()
}

/// The index of the `)` closing the `(` at `open`, or none.
pub fn close_paren(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    for (k, b) in bytes.iter().enumerate().skip(open) {
        if *b == b'(' {
            depth += 1;
        } else if *b == b')' {
            depth -= 1;
            if depth == 0 {
                return Some(k);
            }
        }
    }
    None
}

/// The top-level items of the parenthesized list opening at `open`, or none when it does not close.
pub fn split_args(text: &str, open: usize) -> Option<Vec<String>> {
    let close = close_paren(text, open)?;
    let mut args: Vec<String> = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in text[open + 1..close].chars() {
        if ch == '(' || ch == '{' {
            depth += 1;
        } else if ch == ')' || ch == '}' {
            depth -= 1;
        }
        if ch == ',' && depth == 0 {
            args.push(std::mem::take(&mut cur));
        } else {
            cur.push(ch);
        }
    }
    args.push(cur);
    Some(args.into_iter().map(|a| js_trim(&a).to_string()).collect())
}

/// The type written at `text[at]` — `List(Of Foo)`, `String()`, `Integer?`, `Global.A.B` — by its
/// last name; none when no type starts there.
pub fn read_type(text: &str, at: usize) -> Option<VbType> {
    let tail = text.get(at..)?;
    let head = re!(r"^\s*([A-Za-z_][\w.]*)").captures(tail)?;
    let mut i = at + head.get(0)?.end();
    let mut args: Option<Vec<VbType>> = None;
    if re!(r"^\s*\(\s*Of\b", "i").is_match(&text[i..]) {
        let open = i + text[i..].find('(')?;
        let close = close_paren(text, open)?;
        args = Some(
            split_args(text, open)
                .unwrap_or_default()
                .iter()
                .map(|a| {
                    let stripped = re!(r"^Of\s+", "i").replace_first(a, |_| String::new());
                    read_type(&stripped, 0).unwrap_or_else(|| VbType::new("?", false))
                })
                .collect(),
        );
        i = close + 1;
    }
    let mut segments: Vec<&str> = head.text(1)?.split('.').collect();
    let name = segments.pop()?;
    if !re!(r"^[A-Za-z_]\w*$").is_match(name) || re!(r"^(?:New|Of|As|In|Out|From|With)$", "i").is_match(name) {
        return None;
    }
    let qualifier: Vec<String> = segments
        .iter()
        .map(|s| js_lower(s))
        .enumerate()
        .filter(|(k, s)| !(*k == 0 && s == "global"))
        .map(|(_, s)| s)
        .collect();
    Some(VbType {
        name: name.to_string(),
        array: re!(r"^\s*\??\s*\(\s*,*\s*\)").is_match(&text[i..]),
        qualifier: if qualifier.is_empty() { None } else { Some(qualifier) },
        args,
        ..Default::default()
    })
}

/// `t` — and the type arguments written inside it — written at `file:line`.
pub fn sited(t: Option<VbType>, file: &str, line: u32) -> Option<VbType> {
    let t = t?;
    let args = t.args.map(|args| args.into_iter().filter_map(|a| sited(Some(a), file, line)).collect());
    Some(VbType { file: Some(file.to_string()), line: Some(line), args, ..t })
}

/// A VB.NET line's code: its comment dropped and every string literal emptied to `""`.
pub fn vb_code(line: &str) -> String {
    // Most lines have neither: the line itself.
    if !line.contains(['"', '\'', '\u{2018}', '\u{2019}', '\r']) {
        return line.to_string();
    }
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '"' {
            let mut j = i + 1;
            while j < chars.len() {
                if chars[j] != '"' {
                    j += 1;
                    continue;
                }
                if chars.get(j + 1) == Some(&'"') {
                    j += 1;
                } else {
                    break;
                }
                j += 1;
            }
            out.push_str("\"\"");
            i = j + 1;
            continue;
        }
        if ch == '\'' || ch == '\u{2018}' || ch == '\u{2019}' || ch == '\r' {
            break;
        }
        out.push(ch);
        i += 1;
    }
    out
}

memo_map!(VbCodeLines, String, Rc<Vec<String>>);

/// A file's lines as code (see `vb_code`), read once per file.
pub fn code_lines(file: &str, ctx: &Ctx) -> Rc<Vec<String>> {
    let memo = ctx.memo::<VbCodeLines>();
    if let Some(hit) = memo.lookup(&file.to_string()) {
        return hit;
    }
    let lines: Vec<String> = ctx.get_file_lines(file).map(|l| l.iter().map(|s| vb_code(s)).collect()).unwrap_or_default();
    let lines = Rc::new(lines);
    memo.store(file.to_string(), Rc::clone(&lines));
    lines
}

/// `codeLines(file).slice(from, to).join(' ')` with JavaScript's clamping.
pub fn code_slice(file: &str, from: i64, to: i64, ctx: &Ctx) -> String {
    let lines = code_lines(file, ctx);
    let from = from.max(0) as usize;
    let to = (to.max(0) as usize).min(lines.len());
    if from >= to {
        return String::new();
    }
    lines[from..to].join(" ")
}

/// The patterns that read one name's bindings.
pub struct VbPatterns {
    /// The name standing where a binding names it — not as `name.Member` nor `name(args)`, which only use it.
    declared: Arc<Re>,
    /// `name As T`, `name() As T`, `name As New T`: a variable, parameter, loop or catch variable.
    as_clause: Arc<Re>,
    /// `Dim name, other As T`: a declarator sharing the next one's type.
    listed: Arc<Re>,
    /// `Dim name$`, `ByVal name%`, `For i% = …`.
    type_char: Arc<Re>,
    /// `Dim name = …`: the value's type.
    inferred: Arc<Re>,
    /// `For Each name In …`: the collection's element type, when it says it.
    for_each: Arc<Re>,
    /// A binding that names no type: `For name = …`, `Function(name)`, `From name In`, `Catch name`.
    loose: Arc<Re>,
}

thread_local! {
    static PATTERNS: RefCell<HashMap<String, Rc<VbPatterns>>> = RefCell::new(HashMap::new());
}

fn patterns_for(name: &str) -> Rc<VbPatterns> {
    let key = js_lower(name);
    if let Some(hit) = PATTERNS.with(|p| p.borrow().get(&key).cloned()) {
        return hit;
    }
    let r = escape_regex(&key);
    let patterns = Rc::new(VbPatterns {
        declared: Re::cached(&format!(r"(?<![\w.]){r}(?![\w.]|\s*\.|\s*\((?![\s\d,]*\)))"), "i"),
        as_clause: Re::cached(&format!(r"(?<![\w.]){r}(\s*\([\s\d,]*\))?\s*\??\s+As\s+(New\s+)?"), "gi"),
        listed: Re::cached(&format!(r"\b(?:Dim|Static)\s+(?:[A-Za-z_]\w*\s*,\s*)*{r}\s*,[\w\s,]*?\bAs\s+"), "i"),
        type_char: Re::cached(
            &format!(
                r"(?:\b(?:Dim|Static|Const|ByVal|ByRef|Optional|ParamArray|Each|For|Using|Private|Public|Friend|Protected|Shared|ReadOnly|WithEvents)\s+|[,(]\s*){r}([$%&!#@])(\s*\([\s\d,]*\))?(?![\w$%&!#@])"
            ),
            "i",
        ),
        inferred: Re::cached(&format!(r"\b(?:Dim|Static|Const|Using)\s+{r}\s*=(?!=)"), "i"),
        for_each: Re::cached(&format!(r"\bFor\s+Each\s+{r}\s+In\s+(.+)$"), "i"),
        loose: Re::cached(
            &format!(
                r"\bFor\s+(?:Each\s+)?{r}\b|\b(?:From|Aggregate)\s+{r}\s+In\b|\bLet\s+{r}\s*=|\bCatch\s+{r}\b|\b(?:Function|Sub)\s*\((?:[^()]*,)?\s*(?:ByVal\s+|ByRef\s+)?{r}\s*[,)]"
            ),
            "i",
        ),
    });
    PATTERNS.with(|p| p.borrow_mut().insert(key, Rc::clone(&patterns)));
    patterns
}

/// What one line of code says `name` is, or none when it doesn't bind the name.
pub fn binding_on(code: &str, name: &str) -> Option<VbBinding> {
    // What every binding of a name has on its line: a declaring keyword, or a type character.
    if !re!(r"\b(?:As|Dim|Static|Const|For|Each|Function|Sub|Catch|Using|From|Let|Aggregate)\b|\w[$%&!#@]", "i").is_match(code) {
        return None;
    }
    let p = patterns_for(name);
    if !p.declared.is_match(code) {
        return None;
    }
    for m in p.as_clause.captures_iter(code) {
        let whole = m.get(0)?;
        // A declaration keyword right before a name: the name is a member being declared, not a variable.
        if re!(r"\b(?:Function|Sub|Property|Event|Operator|Declare|Delegate|Class|Structure|Module|Interface|Enum|Namespace)\s+$", "i")
            .is_match(&code[..whole.start()])
        {
            continue;
        }
        let Some(ty) = read_type(code, whole.end()) else { return Some(VbBinding::Unknown) };
        // `As New T(…)`: the parentheses are the constructor's.
        return Some(VbBinding::Type(if m.text(2).is_some() {
            VbType { array: false, ..ty }
        } else {
            let array = ty.array || m.text(1).is_some();
            VbType { array, ..ty }
        }));
    }
    if let Some(listed) = p.listed.captures(code) {
        let end = listed.get(0)?.end();
        return Some(match read_type(code, end) {
            Some(t) => VbBinding::Type(t),
            None => VbBinding::Unknown,
        });
    }
    if let Some(type_char) = p.type_char.captures(code) {
        let c = type_char.text(1).and_then(|s| s.chars().next()).unwrap_or('$');
        return Some(VbBinding::Type(VbType::new(type_char_name(c), type_char.text(2).is_some())));
    }
    // An assignment (`x = New Channel`) says nothing: the declared type
    // (`Dim x As IYouTubeMediaContainer`) is what the call binds to.
    if let Some(inferred) = p.inferred.captures(code) {
        return Some(value_binding(&code[inferred.get(0)?.end()..]));
    }
    // `For Each c In controls.OfType(Of ButtonLabel)`: the loop's elements are
    // that type; `For Each user In users`: the elements of what `users` is.
    if let Some(each) = p.for_each.captures(code) {
        let collection = js_trim(each.text(1).unwrap_or("")).to_string();
        let elements = re!(r"\.\s*(?:OfType|Cast)\s*\(\s*Of\s+", "i").captures(&collection);
        let ty = elements.and_then(|e| {
            let at = e.get(0)?.end();
            if re!(r"^[^()]*\)\s*(?:\(\s*\))?\s*$").is_match(&collection[at..]) {
                read_type(&collection, at)
            } else {
                None
            }
        });
        if let Some(t) = ty {
            return Some(VbBinding::Type(t));
        }
        return Some(if re!(r"^[A-Za-z_]\w*$").is_match(&collection) { VbBinding::Each(collection) } else { VbBinding::Unknown });
    }
    if p.loose.is_match(code) {
        Some(VbBinding::Unknown)
    } else {
        None
    }
}

/// What a local initialized with `expr` (`Dim x = expr`) is.
pub fn value_binding(expr: &str) -> VbBinding {
    let e = js_trim(re!(r"\s:(?!=)").split(expr).first().copied().unwrap_or("")).to_string();
    if let Some(created) = re!(r"^New\s+", "i").captures(&e) {
        let at = created.get(0).map(|m| m.end()).unwrap_or(0);
        return match read_type(&e, at) {
            Some(t) => VbBinding::Type(VbType { array: false, ..t }),
            None => VbBinding::Unknown,
        };
    }
    if let Some(cast) = re!(r"^(?:DirectCast|TryCast|CType)\s*\(", "i").captures(&e) {
        let open = cast.get(0).map(|m| m.end()).unwrap_or(1) - 1;
        let args = split_args(&e, open);
        let ty = args.as_ref().filter(|a| a.len() == 2).and_then(|a| read_type(&a[1], 0));
        return match ty {
            Some(t) => VbBinding::Type(t),
            None => VbBinding::Unknown,
        };
    }
    if re!(r#"^\$?"""#).is_match(&e) {
        return VbBinding::Type(VbType::new("String", false));
    }
    if let Some(conversion) = re!(r"^(C[A-Za-z]+)\s*\(").captures(&e) {
        if let Some(converted) = conversion.text(1).and_then(|c| conversion_type(&js_lower(c))) {
            return VbBinding::Type(VbType::new(converted, false));
        }
    }
    // `obj.Member(…)`, `Member(…)`, `Me.Member`: a call or a read as the whole value.
    if let Some(call) = re!(r"^(?:([A-Za-z_]\w*)\s*\.\s*)?([A-Za-z_]\w*)\s*").captures(&e) {
        let mut rest: String = e[call.get(0).map(|m| m.end()).unwrap_or(0)..].to_string();
        if rest.starts_with('(') {
            rest = match close_paren(&rest, 0) {
                None => "unclosed".to_string(),
                Some(c) => js_trim(&rest[c + 1..]).to_string(),
            };
        }
        if rest.is_empty() {
            return VbBinding::Call { receiver: call.text(1).map(String::from), member: call.text(2).unwrap_or("").to_string() };
        }
    }
    VbBinding::Unknown
}

/// The 0-based line a call's own declarations start on: its member's first line, which holds a method's parameters.
fn scope_start_line(r: &UnresolvedRef, ctx: &Ctx) -> i64 {
    if let Some(from) = ctx.node_by_id(&r.from_node_id) {
        if from.file_path == r.file_path
            && from.start_line <= r.line
            && from.end_line >= r.line
            && matches!(from.kind, NodeKind::Method | NodeKind::Function | NodeKind::Property | NodeKind::Field)
        {
            return from.start_line as i64 - 1;
        }
    }
    let mut start = -1i64;
    for n in ctx.get_nodes_in_file(&r.file_path).iter() {
        if !matches!(n.kind, NodeKind::Method | NodeKind::Function | NodeKind::Property) {
            continue;
        }
        if n.start_line <= r.line && n.end_line >= r.line && n.start_line as i64 - 1 > start {
            start = n.start_line as i64 - 1;
        }
    }
    if start < 0 {
        r.line as i64 - 1
    } else {
        start
    }
}

memo_map!(VbWordLines, String, Rc<HashMap<String, Vec<usize>>>);

/// The 0-based lines of a file's code each lowercased word appears on, in order: a call's
/// receiver is looked for only on the lines that name it, not on every line back to its
/// method's start.
fn word_lines(file: &str, ctx: &Ctx) -> Rc<HashMap<String, Vec<usize>>> {
    let memo = ctx.memo::<VbWordLines>();
    if let Some(hit) = memo.lookup(&file.to_string()) {
        return hit;
    }
    let mut index: HashMap<String, Vec<usize>> = HashMap::new();
    let lines = code_lines(file, ctx);
    for (i, line) in lines.iter().enumerate() {
        let code = js_lower(line);
        for m in re!(r"[A-Za-z_]\w*", "g").captures_iter(&code) {
            let word = m.text(0).unwrap_or("");
            let at = index.entry(word.to_string()).or_default();
            if at.last() != Some(&i) {
                at.push(i);
            }
        }
    }
    let index = Rc::new(index);
    memo.store(file.to_string(), Rc::clone(&index));
    index
}

memo_map!(VbBindings, String, Rc<Vec<(usize, VbBinding)>>);

/// Every statement of a file that binds `name`, by 0-based line — read once.
fn bindings_of(name: &str, file: &str, ctx: &Ctx) -> Rc<Vec<(usize, VbBinding)>> {
    let memo = ctx.memo::<VbBindings>();
    let key = format!("{file}|{}", js_lower(name));
    if let Some(hit) = memo.lookup(&key) {
        return hit;
    }
    let lines = code_lines(file, ctx);
    let mut found: Vec<(usize, VbBinding)> = Vec::new();
    if let Some(at) = word_lines(file, ctx).get(&js_lower(name)) {
        for &i in at {
            if let Some(binding) = binding_on(&lines[i], name) {
                found.push((i, binding));
            }
        }
    }
    let found = Rc::new(found);
    memo.store(key, Rc::clone(&found));
    found
}

/// The nearest statement above a call (in its member) that binds `name`, and its 1-based line.
pub fn local_binding(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<(VbBinding, u32)> {
    let found = bindings_of(name, &r.file_path, ctx);
    // The last binding at or above the call's line.
    let target = r.line as i64 - 1;
    let after = found.partition_point(|(line, _)| (*line as i64) <= target);
    let (line, binding) = found.get(after.checked_sub(1)?)?;
    if *line as i64 >= scope_start_line(r, ctx) {
        Some((binding.clone(), *line as u32 + 1))
    } else {
        None
    }
}
