//! `this.field.method()` in TypeScript/JavaScript: read the field's declared type from the class
//! body (annotation, `new`, `inject(...)`, wrapper factories), then resolve the method on it —
//! through the class's own bases and across imports.

use super::exact::prefer_call_site_file;
use super::infer::{imported_type_source as _imported_type_source, ts_imported_type_file};
use super::members::{declared_supertypes, ts_class_head};
use super::method::resolve_method_on_type;
use super::object_literal::{resolve_object_literal_member, this_scope_caller};
use super::util::*;
use super::same_language_family;
use crate::ambient_modules::is_merged_ambient_declaration;
use crate::ctx::Ctx;
use crate::types::{NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::js_trim;
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

const TS_BASE_WALK_DEPTH: u32 = 10;

#[derive(Clone, Debug, PartialEq)]
pub struct TsFieldDecl {
    pub value_type: bool,
    pub ty: String,
    pub array: bool,
}

#[derive(Clone, Debug)]
struct TsFieldWrapper {
    holder: Option<String>,
    method: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum TsInjector {
    Inject,
    CreateInstance,
}

#[derive(Clone, Debug)]
struct TsFieldOnLine {
    decl: TsFieldDecl,
    via: Option<TsFieldWrapper>,
    injector: Option<TsInjector>,
    open: usize,
}

struct TsClassDecl {
    lines: Vec<String>,
    lookups: Cell<u32>,
    lines_by_token: RefCell<Option<HashMap<String, Vec<usize>>>>,
}

type ClassDecls = Bounded<Option<Rc<TsClassDecl>>, 32>;

memo_map!(FieldDeclMemo, String, Option<TsFieldDecl>);
memo_map!(WrapperMemo, String, bool);
memo_map!(CreateInstanceMemo, String, bool);
memo_map!(NamedClassMemo, String, Option<NodeRef>);
memo_map!(NamedTypeMemo, String, TsNamed);
memo_map!(AmbientFileMemo, String, Option<String>);

#[derive(Clone)]
pub enum TsNamed {
    Node(NodeRef),
    Outside,
    None,
}

fn ts_class_decl(cls: &Node, ctx: &Ctx) -> Option<Rc<TsClassDecl>> {
    ctx.memo::<ClassDecls>().get_or(&cls.id, false, || {
        let source = ctx.read_file(&cls.file_path)?;
        let from = (cls.start_line as usize).saturating_sub(1);
        let lines: Vec<String> = source
            .split('\n')
            .skip(from)
            .take((cls.end_line as usize).saturating_sub(from))
            .map(|raw| re!(r"/\*.*?\*/", "g").replace_all_str(&re!(r"//.*$").replace_first(raw, |_| String::new()), ""))
            .collect();
        Some(Rc::new(TsClassDecl { lines, lookups: Cell::new(0), lines_by_token: RefCell::new(None) }))
    })
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '#'
}

fn ts_array_suffix(rest: &str) -> bool {
    re!(r"^\s*(?:<[^;=(){}<>]*(?:<[^;=(){}<>]*>[^;=(){}<>]*)*>)?\s*\[\s*\]").is_match(rest)
}

/// One field-declaration shape, in the order they are tried.
struct Shape {
    re: std::sync::Arc<Re>,
    value_type: bool,
    wrapped: bool,
    injected: bool,
}

fn shape_from(re: std::sync::Arc<Re>, value_type: bool, wrapped: bool, injected: bool) -> Shape {
    Shape { re, value_type, wrapped, injected }
}

/// The five declaration shapes, with `name` standing for the field (a token pattern when sticky).
fn field_shapes(name: &str, flags: &str) -> Vec<Shape> {
    let start = r"(?<![\w$#])";
    let mk = |tail: &str, value_type: bool, wrapped: bool, injected: bool| shape_from(Re::cached(&format!("{start}{name}{tail}"), flags), value_type, wrapped, injected);
    vec![
        mk(r"\b\s*[?!]?\s*:\s*(?:readonly\s+)?typeof\s+([A-Za-z_$][\w.$]*)", true, false, false),
        mk(r"\b\s*[?!]?\s*:\s*(?:readonly\s+)?([A-Za-z_$][\w.$]*)", false, false, false),
        mk(r"\b\s*=\s*new\s+([A-Za-z_$][\w.$]*)", false, false, false),
        mk(r"\b\s*=\s*this\.(?:([\w$#]+)\.)?([\w$#]+)\(\s*(?:new\s+|((?:this\.)?(?:[\w$#]+\.)*createInstance)\s*\(\s*)([A-Za-z_$][\w.$]*)", false, true, false),
        mk(r"\b\s*=\s*(inject|(?:this\.)?(?:[\w$#]+\.)*createInstance)\s*(?:<\s*([A-Z][\w.$]*)[^;=(){}]*>)?\s*\(\s*([A-Z][\w.$]*)", false, false, true),
    ]
}

thread_local! {
    static TOKEN_SHAPES: Vec<Shape> = field_shapes(r"[\w$#]+", "y");
    static FIELD_SHAPES: RefCell<HashMap<String, Rc<Vec<Shape>>>> = RefCell::new(HashMap::new());
}

fn shape_match(shape: &Shape, caps: &codegraph_jsre::Caps, line: &str) -> Option<TsFieldOnLine> {
    let g = caps.get(0)?;
    let ty = if shape.wrapped {
        caps.text(4)
    } else if shape.injected {
        caps.text(2).or(caps.text(3))
    } else {
        caps.text(1)
    }
    .filter(|t| !t.is_empty())?;
    let mut decl = TsFieldOnLine {
        decl: TsFieldDecl { value_type: shape.value_type, ty: ty.to_string(), array: ts_array_suffix(&line[g.end()..]) },
        via: None,
        injector: None,
        open: 0,
    };
    if shape.wrapped {
        decl.via = Some(TsFieldWrapper { holder: caps.text(1).map(|s| s.to_string()), method: caps.text(2).unwrap_or("").to_string() });
        if caps.text(3).is_some_and(|s| !s.is_empty()) {
            decl.injector = Some(TsInjector::CreateInstance);
        }
    } else if shape.injected {
        decl.injector = Some(if caps.text(1) == Some("inject") { TsInjector::Inject } else { TsInjector::CreateInstance });
    }
    if shape.wrapped || shape.injected {
        decl.open = g.start() + g.as_str().find('(').unwrap_or(0);
    }
    Some(decl)
}

/// The declaration of `field` on `line`, when one of the shapes matches at a whole-token occurrence.
fn ts_field_on_line(line: &str, field: &str) -> Option<TsFieldOnLine> {
    let mut at: Vec<usize> = Vec::new();
    let mut from = 0;
    while let Some(rel) = line[from..].find(field) {
        let i = from + rel;
        let end = i + field.len();
        let before_is_token = line[..i].chars().next_back().is_some_and(is_token_char);
        let after_is_token = line[end..].chars().next().is_some_and(is_token_char);
        if !(before_is_token || after_is_token) {
            at.push(i);
        }
        from = i + line[i..].chars().next().map_or(1, |c| c.len_utf8());
    }
    if at.is_empty() {
        return None;
    }
    TOKEN_SHAPES.with(|shapes| {
        for shape in shapes {
            for &i in &at {
                if let Some(caps) = shape.re.captures_at(line, i) {
                    if let Some(d) = shape_match(shape, &caps, line) {
                        return Some(d);
                    }
                }
            }
        }
        None
    })
}

fn class_lines_with_token(decl: &TsClassDecl, token: &str) -> Vec<usize> {
    if decl.lines_by_token.borrow().is_none() {
        let mut index: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, line) in decl.lines.iter().enumerate() {
            for m in re!(r"[\w$#]+", "g").captures_iter(line) {
                let rows = index.entry(m.get(0).unwrap().as_str().to_string()).or_default();
                if rows.last() != Some(&i) {
                    rows.push(i);
                }
            }
        }
        *decl.lines_by_token.borrow_mut() = Some(index);
    }
    decl.lines_by_token.borrow().as_ref().and_then(|m| m.get(token).cloned()).unwrap_or_default()
}

/// The declared type of `field` in `cls`'s body.
pub fn ts_field_declaration(cls: &Node, field: &str, ctx: &Ctx) -> Option<TsFieldDecl> {
    let key = format!("{}\0{}", cls.id, field);
    let memo = ctx.memo::<FieldDeclMemo>();
    if let Some(hit) = memo.lookup(&key) {
        return hit;
    }
    memo.store(key.clone(), None);
    let mut found: Option<TsFieldDecl> = None;
    if let Some(decl) = ts_class_decl(cls, ctx) {
        if re!(r"^[\w$#]+$").is_match(field) {
            decl.lookups.set(decl.lookups.get() + 1);
            let rows: Option<Vec<usize>> = if decl.lookups.get() > 1 { Some(class_lines_with_token(&decl, field)) } else { None };
            let count = rows.as_ref().map_or(decl.lines.len(), |r| r.len());
            let mut k = 0;
            while k < count && found.is_none() {
                let row = rows.as_ref().map_or(k, |r| r[k]);
                let line = &decl.lines[row];
                if line.contains(field) {
                    found = ts_field_read(ts_field_on_line(line, field), cls, &decl.lines, row, ctx);
                }
                k += 1;
            }
        } else {
            let shapes = FIELD_SHAPES.with(|m| {
                let mut m = m.borrow_mut();
                if m.len() >= 4096 {
                    m.clear();
                }
                m.entry(field.to_string()).or_insert_with(|| Rc::new(field_shapes(&escape_regex(field), ""))).clone()
            });
            'scan: for row in 0..decl.lines.len() {
                let line = &decl.lines[row];
                if !line.contains(field) {
                    continue;
                }
                for shape in shapes.iter() {
                    let Some(caps) = shape.re.captures(line) else { continue };
                    let Some(on_line) = shape_match(shape, &caps, line) else { continue };
                    found = ts_field_read(Some(on_line), cls, &decl.lines, row, ctx);
                    if found.is_some() {
                        break 'scan;
                    }
                    continue 'scan;
                }
            }
        }
    }
    memo.store(key, found.clone());
    found
}

/// A field declared through a wrapper or injector counts only when the call really hands back the instance.
fn ts_field_read(on_line: Option<TsFieldOnLine>, cls: &Node, lines: &[String], row: usize, ctx: &Ctx) -> Option<TsFieldDecl> {
    let on_line = on_line?;
    if on_line.via.is_none() && on_line.injector.is_none() {
        return Some(on_line.decl);
    }
    if !ts_call_ends_statement(lines, row, on_line.open) {
        return None;
    }
    if let Some(via) = &on_line.via {
        if !ts_wrapper_returns_argument(cls, via, ctx) {
            return None;
        }
    }
    if let Some(inj) = on_line.injector {
        if !ts_injects_instance(cls, inj, ctx) {
            return None;
        }
    }
    Some(on_line.decl)
}

fn ts_injects_instance(cls: &Node, injector: TsInjector, ctx: &Ctx) -> bool {
    if injector == TsInjector::Inject {
        return ctx.get_import_mappings(&cls.file_path, cls.language).iter().any(|i| i.local_name == "inject" && i.source == "@angular/core");
    }
    memoized::<CreateInstanceMemo>(ctx, String::new(), || {
        ctx.get_nodes_by_name("createInstance").iter().any(|n| {
            if !matches!(n.kind, NodeKind::Method | NodeKind::Function) || !is_esm_family(n.language) {
                return false;
            }
            let lines = lines_of(ctx, &n.file_path);
            let from = (n.start_line as usize - 1).min(lines.len());
            let to = (n.start_line as usize + 2).min(lines.len()).max(from);
            re!(r"\bInstanceType\s*<").is_match(&lines[from..to].join(" "))
        })
    })
}

/// The call whose `(` is at byte `open` on `row` is a complete statement (not chained on).
fn ts_call_ends_statement(lines: &[String], row: usize, open: usize) -> bool {
    let mut depth = 0i32;
    let mut r = row;
    let mut from = open;
    while r < lines.len() && r < row + 12 {
        let line = lines[r].as_bytes();
        let mut i = from;
        while i < line.len() {
            let c = line[i];
            if c == b'"' || c == b'\'' || c == b'`' {
                let mut close = i + 1;
                while close < line.len() && line[close] != c {
                    close += if line[close] == b'\\' { 2 } else { 1 };
                }
                if close >= line.len() {
                    return false;
                }
                i = close;
            } else if c == b'(' {
                depth += 1;
            } else if c == b')' {
                depth -= 1;
                if depth == 0 {
                    let rest = js_trim(&lines[r][i + 1..]);
                    if !rest.is_empty() {
                        return rest.starts_with(';');
                    }
                    let next = lines.iter().skip(r + 1).take(3).find(|l| !js_trim(l).is_empty());
                    return match next {
                        None => true,
                        Some(n) => !re!(r"^\s*(?:[.?\[(!,:|&+*/%<>=-]|as\b|satisfies\b)").is_match(n),
                    };
                }
            }
            i += 1;
        }
        r += 1;
        from = 0;
    }
    false
}

fn ts_wrapper_returns_argument(cls: &Node, via: &TsFieldWrapper, ctx: &Ctx) -> bool {
    let key = format!("{}\0{}\0{}", cls.id, via.holder.as_deref().unwrap_or(""), via.method);
    memoized::<WrapperMemo>(ctx, key, || {
        let at = UnresolvedRef::new(cls.id.clone(), via.method.clone(), EdgeKind::Calls, cls.start_line, cls.start_column, cls.file_path.clone(), cls.language);
        let cls_rc: NodeRef = Rc::new(cls.clone());
        let owner: Option<NodeRef> = match &via.holder {
            Some(holder) => ts_field_through_bases(&cls_rc, holder, &at, ctx, 0)
                .filter(|h| !h.0.value_type && !h.0.array)
                .and_then(|h| ts_named_class(&h.0.ty, &h.1, &at, ctx)),
            None => Some(cls_rc),
        };
        let methods = owner.map(|o| ts_class_methods(&o, &via.method, &at, ctx, 0)).unwrap_or_default();
        !methods.is_empty() && methods.iter().all(|m| ts_returns_its_argument(m, ctx))
    })
}

fn owner_tail(qualified_name: &str) -> Option<&str> {
    let cut = qualified_name.rfind("::").unwrap_or(0);
    re!(r"::|\.").split(&qualified_name[..cut]).into_iter().last()
}

fn ts_class_methods(cls: &NodeRef, name: &str, at: &UnresolvedRef, ctx: &Ctx, depth: u32) -> Vec<NodeRef> {
    let own: Vec<NodeRef> = ctx
        .get_nodes_in_file(&cls.file_path)
        .iter()
        .filter(|n| n.kind == NodeKind::Method && n.name == name && n.start_line >= cls.start_line && n.end_line <= cls.end_line && owner_tail(&n.qualified_name) == Some(cls.name.as_str()))
        .cloned()
        .collect();
    if !own.is_empty() || depth >= TS_BASE_WALK_DEPTH {
        return own;
    }
    for sup in declared_supertypes(cls, ctx).iter() {
        let Some(base) = ts_named_class(sup, cls, at, ctx) else { continue };
        if base.id == cls.id {
            continue;
        }
        let found = ts_class_methods(&base, name, at, ctx, depth + 1);
        if !found.is_empty() {
            return found;
        }
    }
    Vec::new()
}

fn is_ts_member_owner(k: NodeKind) -> bool {
    matches!(k, NodeKind::Class | NodeKind::Interface)
}

fn ts_type_declared_in(file: &str, name: &str, ctx: &Ctx) -> Option<NodeRef> {
    ctx.get_nodes_in_file(file).iter().find(|n| n.name == name && is_ts_member_owner(n.kind)).cloned()
}

/// The `extends`/`implements` names in a TS class head, or none when one of them is not a plain name.
fn ts_head_supertypes(cls: &Node, ctx: &Ctx) -> Option<Vec<String>> {
    let lines = lines_of(ctx, &cls.file_path);
    let head = ts_class_head(&lines, cls);
    let mut out = Vec::new();
    for keyword in ["extends", "implements"] {
        let clause = Re::cached(&format!(r"\b{keyword}\b([\s\S]*?)(?:\b(?:extends|implements)\b|$)"), "").captures(&head).and_then(|c| c.text(1).map(|s| s.to_string()));
        let Some(clause) = clause else { continue };
        for part in clause.split(',') {
            let name = js_trim(part);
            if !re!(r"^[A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*)*$").is_match(name) {
                if keyword == "extends" {
                    return None;
                }
                continue;
            }
            out.push(name.to_string());
        }
    }
    Some(out)
}

/// The class/interface a written type names, from `from`'s file: imported, local, or `Outside` the repo.
fn ts_named_type(written: &str, from_file: &str, from_language: Language, from_id: &str, at: &UnresolvedRef, ctx: &Ctx) -> TsNamed {
    let first = written.split('.').next().unwrap_or("");
    let mappings = ctx.get_import_mappings(from_file, from_language);
    let mapping = mappings.iter().find(|i| i.local_name == first);
    let key = format!("{}\0{}{}", from_file, written, if mapping.is_some() { String::new() } else { format!("\0{from_id}") });
    if let Some(hit) = ctx.memo::<NamedTypeMemo>().lookup(&key) {
        return hit;
    }
    let last = written.rsplit('.').next().unwrap_or("");
    let mut r = at.clone();
    r.file_path = from_file.to_string();
    r.language = from_language;
    let found = if let Some(m) = mapping.filter(|m| ctx.is_out_of_repo_import(&m.source, from_file, from_language)) {
        let _ = m;
        match ts_outside_type_declaration(written, last, &r, ctx) {
            Tri::Null => TsNamed::Outside,
            Tri::Undefined => TsNamed::None,
            Tri::Value(file) => ts_type_declared_in(&file, last, ctx).map_or(TsNamed::None, TsNamed::Node),
        }
    } else if mapping.is_some() {
        let mut probe = r.clone();
        probe.reference_name = written.to_string();
        probe.reference_kind = codegraph_types::ReferenceKind::Edge(EdgeKind::References);
        let node = crate::import_resolver::resolve_import(&probe, ctx).and_then(|i| ctx.node_by_id(&i.target_node_id));
        match node {
            Some(n) if !is_merged_ambient_declaration(&n, ctx) => {
                if is_ts_member_owner(n.kind) {
                    TsNamed::Node(n)
                } else {
                    ts_type_declared_in(&n.file_path, &n.name, ctx).map_or(TsNamed::None, TsNamed::Node)
                }
            }
            _ => TsNamed::None,
        }
    } else if !written.contains('.') {
        ctx.get_nodes_in_file(from_file).iter().find(|n| n.id != from_id && n.name == written && is_ts_member_owner(n.kind)).cloned().map_or(TsNamed::None, TsNamed::Node)
    } else {
        TsNamed::None
    };
    ctx.memo::<NamedTypeMemo>().store(key, found.clone());
    found
}

struct UpChain {
    method: Option<NodeRef>,
    complete: bool,
}

/// `name` on `cls` or up its `extends` chain; `complete` says whether every base was readable.
fn ts_method_up_chain(cls: &NodeRef, name: &str, at: &UnresolvedRef, ctx: &Ctx, depth: u32) -> UpChain {
    let members: Vec<NodeRef> = ctx
        .get_nodes_in_file(&cls.file_path)
        .iter()
        .filter(|n| n.name == name && n.start_line >= cls.start_line && n.end_line <= cls.end_line && owner_tail(&n.qualified_name) == Some(cls.name.as_str()))
        .cloned()
        .collect();
    if let Some(own) = members.iter().find(|n| n.kind == NodeKind::Method) {
        return UpChain { method: Some(own.clone()), complete: true };
    }
    if members.iter().any(|n| matches!(n.kind, NodeKind::Property | NodeKind::Field)) {
        return UpChain { method: None, complete: false };
    }
    if depth >= TS_BASE_WALK_DEPTH {
        return UpChain { method: None, complete: false };
    }
    let Some(supers) = ts_head_supertypes(cls, ctx) else { return UpChain { method: None, complete: false } };
    let mut complete = true;
    for written in supers {
        match ts_named_type(&written, &cls.file_path, cls.language, &cls.id, at, ctx) {
            TsNamed::Outside => continue,
            TsNamed::None => {
                complete = false;
                continue;
            }
            TsNamed::Node(base) => {
                if base.id == cls.id {
                    complete = false;
                    continue;
                }
                let up = ts_method_up_chain(&base, name, at, ctx, depth + 1);
                if up.method.is_some() {
                    return up;
                }
                if !up.complete {
                    complete = false;
                }
            }
        }
    }
    UpChain { method: None, complete }
}

/// `typeName.method` when the type is declared in `file` (or imported): the method up its chain, null when provably absent, undefined when unknown.
pub fn ts_known_type_call(type_name: &str, file: &str, method_name: &str, r: &UnresolvedRef, at: &UnresolvedRef, ctx: &Ctx, confidence: f64) -> Tri<ResolvedRef> {
    let mut known = ts_type_declared_in(file, type_name, ctx);
    if known.is_none() {
        if let TsNamed::Node(n) = ts_named_type(type_name, &at.file_path, at.language, "", at, ctx) {
            known = Some(n);
        }
    }
    let Some(known) = known else { return Tri::Undefined };
    let up = ts_method_up_chain(&known, method_name, at, ctx, 0);
    if let Some(m) = up.method {
        return Tri::Value(ResolvedRef::new(r, m.id.clone(), confidence, ResolvedBy::InstanceMethod));
    }
    if up.complete {
        Tri::Null
    } else {
        Tri::Undefined
    }
}

/// `(x: T): T` — a pass-through whose generic head names its parameter's type.
fn ts_returns_its_argument(f: &Node, ctx: &Ctx) -> bool {
    let sig = f.signature.as_deref().map(js_trim).unwrap_or("");
    let Some(m) = re!(r"^\(\s*[\w$]+\s*:\s*([A-Za-z_$][\w$]*)\s*\)\s*:\s*([A-Za-z_$][\w$]*)$").captures(sig) else { return false };
    if m.text(1) != m.text(2) {
        return false;
    }
    let lines = lines_of(ctx, &f.file_path);
    let from = (f.start_line as usize - 1).min(lines.len());
    let to = (f.start_line as usize + 2).min(lines.len()).max(from);
    let head = lines[from..to].join(" ");
    Re::cached(&format!(r"(?<![\w$#]){}\s*<\s*(?:const\s+)?{}\b", escape_regex(&f.name), escape_regex(m.text(1).unwrap_or(""))), "").is_match(&head)
}

pub struct IndexedPropertyType {
    pub ty: String,
    pub array: bool,
    pub file: String,
}

/// `Props["items"]` — the declared type of that property.
pub fn ts_indexed_property_type(indexed: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<IndexedPropertyType> {
    let m = re!(r#"^([A-Za-z_$][\w$]*)\s*\[\s*(['"])([\w$]+)\2\s*\]$"#).captures(indexed)?;
    let owner_name = m.text(1)?;
    let imported = ts_imported_type_file(owner_name, r, ctx);
    let named: Vec<NodeRef> = ctx.get_nodes_by_name(owner_name).iter().cloned().collect();
    let owners: Vec<NodeRef> = prefer_call_site_file(named, imported.as_deref().unwrap_or(&r.file_path))
        .into_iter()
        .filter(|n| matches!(n.kind, NodeKind::Class | NodeKind::Interface | NodeKind::TypeAlias | NodeKind::Component) && same_language_family(n.language, r.language))
        .collect();
    for owner in owners {
        let Some(decl) = ts_field_declaration(&owner, m.text(3)?, ctx) else { continue };
        if decl.value_type || ts_class_type_parameter(&owner, &decl.ty, ctx) {
            return None;
        }
        let at = if owner.file_path == r.file_path {
            r.clone()
        } else {
            let mut a = r.clone();
            a.file_path = owner.file_path.clone();
            a.language = owner.language;
            a
        };
        let file = ts_imported_type_file(&decl.ty, &at, ctx).unwrap_or_else(|| owner.file_path.clone());
        return Some(IndexedPropertyType { ty: decl.ty, array: decl.array, file });
    }
    None
}

fn ts_class_type_parameter(cls: &Node, name: &str, ctx: &Ctx) -> bool {
    let lines = lines_of(ctx, &cls.file_path);
    let from = (cls.start_line as usize - 1).min(lines.len());
    let to = (cls.start_line as usize + 2).min(lines.len()).max(from);
    let joined = lines[from..to].join(" ");
    let head = joined.split('{').next().unwrap_or("");
    let Some(open) = Re::cached(&format!(r"\b{}\s*<", escape_regex(&cls.name)), "").find(head) else { return false };
    let first_of = |s: &str| -> String {
        let t = js_trim(s);
        let t = re!(r"^(?:in|out|const)\s+").replace_first(t, |_| String::new());
        re!(r"[\s=]").split(&t).into_iter().next().unwrap_or("").to_string()
    };
    let chars: Vec<(usize, char)> = head.char_indices().collect();
    let mut depth = 0i32;
    let mut from_b = open.end();
    for (k, &(i, c)) in chars.iter().enumerate() {
        if i < open.end() {
            continue;
        }
        if c == '<' || c == '(' || c == '[' || c == '{' {
            depth += 1;
        } else if c == ')' || c == ']' || c == '}' {
            depth -= 1;
        } else if c == '>' && !(k > 0 && chars[k - 1].1 == '=') {
            if depth == 0 {
                return first_of(&head[from_b..i]) == name;
            }
            depth -= 1;
        } else if c == ',' && depth == 0 {
            if first_of(&head[from_b..i]) == name {
                return true;
            }
            from_b = i + 1;
        }
    }
    false
}

/// `this.field.method(...)` / `this.#field.method(...)`.
pub fn match_ts_this_field_call(field: &str, method_name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    if field.is_empty() || field.contains('.') {
        return None;
    }
    let written = ctx.node_by_id(&r.from_node_id)?;
    let caller = this_scope_caller(&written, ctx);
    let sep = caller.qualified_name.rfind("::").filter(|s| *s > 0)?;
    let owner = caller.qualified_name[..sep].rsplit("::").next().filter(|s| !s.is_empty())?;
    let named: Vec<NodeRef> = ctx.get_nodes_by_name(owner).iter().cloned().collect();
    let owners: Vec<NodeRef> = prefer_call_site_file(named, &r.file_path)
        .into_iter()
        .filter(|n| matches!(n.kind, NodeKind::Class | NodeKind::Component) && same_language_family(n.language, r.language))
        .collect();
    for cls in &owners {
        if cls.file_path != r.file_path {
            continue;
        }
        if let Some((decl, holder)) = ts_field_through_bases(cls, field, r, ctx, 0) {
            return ts_field_call(&decl, Some(&holder), method_name, r, ctx);
        }
    }
    for cls in &owners {
        if cls.file_path == r.file_path {
            continue;
        }
        if let Some(decl) = ts_field_declaration(cls, field, ctx) {
            if ts_type_visible_in(&decl.ty, r, ctx) {
                return ts_field_call(&decl, None, method_name, r, ctx);
            }
        }
    }
    None
}

/// The field's declaration on `cls` or the nearest base declaring it.
fn ts_field_through_bases(cls: &NodeRef, field: &str, r: &UnresolvedRef, ctx: &Ctx, depth: u32) -> Option<(TsFieldDecl, NodeRef)> {
    if let Some(decl) = ts_field_declaration(cls, field, ctx) {
        return Some((decl, cls.clone()));
    }
    if depth >= TS_BASE_WALK_DEPTH {
        return None;
    }
    for name in declared_supertypes(cls, ctx).iter() {
        let Some(base) = ts_named_class(name, cls, r, ctx) else { continue };
        if base.id == cls.id {
            continue;
        }
        if let Some(found) = ts_field_through_bases(&base, field, r, ctx, depth + 1) {
            return Some(found);
        }
    }
    None
}

/// The class `name` names from `from`'s file (imported or declared there).
fn ts_named_class(name: &str, from: &Node, r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    let key = format!("{}\0{}", from.file_path, name);
    if let Some(hit) = ctx.memo::<NamedClassMemo>().lookup(&key) {
        return hit;
    }
    let found = if ctx.get_import_mappings(&from.file_path, from.language).iter().any(|i| i.local_name == name) {
        let mut probe = r.clone();
        probe.file_path = from.file_path.clone();
        probe.language = from.language;
        probe.reference_name = name.to_string();
        probe.reference_kind = codegraph_types::ReferenceKind::Edge(EdgeKind::References);
        crate::import_resolver::resolve_import(&probe, ctx)
            .and_then(|i| ctx.node_by_id(&i.target_node_id))
            .filter(|n| matches!(n.kind, NodeKind::Class | NodeKind::Component) && !is_merged_ambient_declaration(n, ctx))
    } else {
        ctx.get_nodes_in_file(&from.file_path).iter().find(|n| n.kind == NodeKind::Class && n.name == name).cloned()
    };
    ctx.memo::<NamedClassMemo>().store(key, found.clone());
    found
}

/// The file declaring `typeName` in an ambient `declare module 'pkg'` block, for a type imported from outside the repo.
pub fn ts_outside_type_declaration(written: &str, type_name: &str, at: &UnresolvedRef, ctx: &Ctx) -> Tri<String> {
    let Some(head) = re!(r"^[A-Za-z_$][\w$]*").find(js_trim(written)).map(|m| m.as_str().to_string()) else { return Tri::Undefined };
    let mappings = ctx.get_import_mappings(&at.file_path, at.language);
    let Some(binding) = mappings.iter().find(|m| m.local_name == head) else { return Tri::Undefined };
    if !ctx.is_out_of_repo_import(&binding.source, &at.file_path, at.language) {
        return Tri::Undefined;
    }
    let key = format!("{}\0{}", binding.source, type_name);
    let file = memoized::<AmbientFileMemo>(ctx, key, || {
        let pkg = escape_regex(&re!(r"^node:").replace_first(&binding.source, |_| String::new()));
        let declares = Re::cached(&format!(r#"\bdeclare\s+module\s+(['"])(?:node:)?{pkg}\1"#), "");
        for n in ctx.get_nodes_by_name(type_name).iter() {
            if !same_language_family(n.language, at.language) {
                continue;
            }
            if declares.is_match(&ctx.read_file(&n.file_path).map(|s| s.to_string()).unwrap_or_default()) {
                return Some(n.file_path.clone());
            }
        }
        None
    });
    match file {
        Some(f) => Tri::Value(f),
        None => Tri::Null,
    }
}

fn ts_type_visible_in(ty: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if ts_imported_type_file(ty, r, ctx).is_some() {
        return true;
    }
    let head = ty.split('.').next().unwrap_or("");
    ctx.get_nodes_in_file(&r.file_path)
        .iter()
        .any(|n| n.name == head && matches!(n.kind, NodeKind::Class | NodeKind::Interface | NodeKind::TypeAlias | NodeKind::Constant | NodeKind::Variable))
}

/// Resolve `method_name` on the type a field is declared with.
fn ts_field_call(decl: &TsFieldDecl, declaring: Option<&NodeRef>, method_name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    if decl.value_type {
        let holder_name = decl.ty.rsplit('.').next().unwrap_or("");
        let named: Vec<NodeRef> = ctx.get_nodes_by_name(holder_name).iter().cloned().collect();
        let holders = prefer_call_site_file(named, &r.file_path)
            .into_iter()
            .filter(|n| matches!(n.kind, NodeKind::Constant | NodeKind::Variable) && same_language_family(n.language, r.language));
        for holder in holders {
            if let Some(hit) = resolve_object_literal_member(&holder, method_name, r, ctx, 0.85, ResolvedBy::InstanceMethod) {
                return Some(hit);
            }
        }
        return None;
    }
    let type_name = decl.ty.rsplit('.').next().unwrap_or("").to_string();
    if !re!(r"^[A-Z]").is_match(&type_name) {
        return None;
    }
    let same = declaring.map_or(true, |d| d.file_path == r.file_path);
    let at: UnresolvedRef = if same {
        r.clone()
    } else {
        let d = declaring.unwrap();
        let mut a = r.clone();
        a.file_path = d.file_path.clone();
        a.language = d.language;
        a
    };
    let mut imported = ts_imported_type_file(&decl.ty, &at, ctx);
    if imported.is_none() {
        match ts_outside_type_declaration(&decl.ty, &type_name, &at, ctx) {
            Tri::Null => return None,
            Tri::Value(f) => imported = Some(f),
            Tri::Undefined => imported = if same { None } else { Some(at.file_path.clone()) },
        }
    }
    match ts_known_type_call(&type_name, imported.as_deref().unwrap_or(&at.file_path), method_name, r, &at, ctx, 0.85) {
        Tri::Value(v) => return Some(v),
        Tri::Null => return None,
        Tri::Undefined => {}
    }
    let want = format!("{type_name}::{method_name}");
    let suffix = format!("::{want}");
    let declared: Vec<NodeRef> = ctx
        .get_nodes_by_name(method_name)
        .iter()
        .filter(|n| n.kind == NodeKind::Method && same_language_family(n.language, r.language) && (n.qualified_name == want || n.qualified_name.ends_with(&suffix)))
        .cloned()
        .collect();
    if declared.len() > 1 {
        if let Some(own) = imported.as_ref().and_then(|f| declared.iter().find(|n| &n.file_path == f)) {
            return Some(ResolvedRef::new(r, own.id.clone(), 0.85, ResolvedBy::InstanceMethod));
        }
        let call_dirs: Vec<&str> = r.file_path.split('/').collect::<Vec<_>>().split_last().map(|(_, d)| d.to_vec()).unwrap_or_default();
        let shared = |fp: &str| -> usize {
            let dirs: Vec<&str> = fp.split('/').collect::<Vec<_>>().split_last().map(|(_, d)| d.to_vec()).unwrap_or_default();
            dirs.iter().zip(call_dirs.iter()).take_while(|(a, b)| a == b).count()
        };
        let mut sorted = declared.clone();
        sorted.sort_by(|a, b| shared(&b.file_path).cmp(&shared(&a.file_path)).then_with(|| locale_compare(&a.file_path, &b.file_path)));
        return Some(ResolvedRef::new(r, sorted[0].id.clone(), 0.85, ResolvedBy::InstanceMethod));
    }
    resolve_method_on_type(&type_name, method_name, r, ctx, 0.85, ResolvedBy::InstanceMethod, imported.as_deref(), 0)
}

#[allow(dead_code)]
fn unused() -> Option<String> {
    let _ = _imported_type_source;
    None
}
