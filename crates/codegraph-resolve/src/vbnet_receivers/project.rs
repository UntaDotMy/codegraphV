//! VB.NET projects, namespaces and type lookup: which project a file is in, what a type name
//! written at a site means, and a type's supertypes. Port of the second third of
//! `src/resolution/vbnet-receivers.ts`.

use super::text::*;
use crate::ctx::{js_lower, Ctx};
use crate::name_matcher::util::{memo_map, MemoMap};
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::{Language, Node, NodeKind};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::rc::Rc;

/// What the type parameters of a type stand for, by lowercased name.
pub type TypeArgs = IndexMap<String, VbType>;

fn is_vb(n: &Node) -> bool {
    n.language == Language::VbNet
}

fn project_key(file: &str, ctx: &Ctx) -> String {
    project_of(file, ctx).unwrap_or_else(|| "null".to_string())
}

// -- Types around a line, members, modules ---------------------------------------------------------

memo_map!(VbFileTypes, String, Rc<Vec<NodeRef>>);

/// The VB.NET types a line is written inside, innermost first.
pub fn types_around(file: &str, line: u32, ctx: &Ctx) -> Vec<NodeRef> {
    let memo = ctx.memo::<VbFileTypes>();
    let types = match memo.lookup(&file.to_string()) {
        Some(t) => t,
        None => {
            let mut v: Vec<NodeRef> = ctx.get_nodes_in_file(file).iter().filter(|n| is_vb(n) && VB_TYPE_KINDS.contains(&n.kind)).cloned().collect();
            v.sort_by(|a, b| b.start_line.cmp(&a.start_line));
            let v = Rc::new(v);
            memo.store(file.to_string(), Rc::clone(&v));
            v
        }
    };
    types.iter().filter(|n| n.start_line <= line && n.end_line >= line).cloned().collect()
}

/// The project's VB.NET types (classes, modules, structures, interfaces — or `kinds`) named `name`, case aside.
pub fn project_types_named(name: &str, ctx: &Ctx, kinds: &[NodeKind]) -> Vec<NodeRef> {
    ctx.get_nodes_by_lower_name(&js_lower(name)).iter().filter(|n| is_vb(n) && kinds.contains(&n.kind)).cloned().collect()
}

memo_map!(VbFileQns, String, Rc<HashMap<String, Vec<NodeRef>>>);

/// A file's VB.NET nodes by lowercased qualified name, indexed once.
pub fn nodes_by_qualified_name(file: &str, ctx: &Ctx) -> Rc<HashMap<String, Vec<NodeRef>>> {
    let memo = ctx.memo::<VbFileQns>();
    if let Some(hit) = memo.lookup(&file.to_string()) {
        return hit;
    }
    let mut index: HashMap<String, Vec<NodeRef>> = HashMap::new();
    for n in ctx.get_nodes_in_file(file).iter() {
        if !is_vb(n) {
            continue;
        }
        index.entry(js_lower(&n.qualified_name)).or_default().push(Rc::clone(n));
    }
    let index = Rc::new(index);
    memo.store(file.to_string(), Rc::clone(&index));
    index
}

memo_map!(VbMembers, String, Rc<Vec<NodeRef>>);

/// Members named `name` declared directly in `ty` (any partial part of it), case aside. A member
/// is declared inside a part of its type, so it is looked up in those parts' files. The parts are
/// in the type's own project: SCrawler's plugins each declare a `UserData`, whose members are not
/// one another's.
pub fn members_named(ty: &Node, name: &str, ctx: &Ctx) -> Rc<Vec<NodeRef>> {
    let memo = ctx.memo::<VbMembers>();
    let project = project_of(&ty.file_path, ctx);
    let qn = js_lower(&format!("{}::{}", ty.qualified_name, name));
    let key = format!("{}|{}", project.as_deref().unwrap_or("null"), qn);
    if let Some(hit) = memo.lookup(&key) {
        return hit;
    }
    let own = js_lower(&ty.qualified_name);
    let mut files: Vec<String> = vec![ty.file_path.clone()];
    for part in ctx.get_nodes_by_lower_name(&js_lower(&ty.name)).iter() {
        if is_vb(part) && js_lower(&part.qualified_name) == own && project_of(&part.file_path, ctx) == project && !files.contains(&part.file_path) {
            files.push(part.file_path.clone());
        }
    }
    let members: Vec<NodeRef> = files.iter().flat_map(|f| nodes_by_qualified_name(f, ctx).get(&qn).cloned().unwrap_or_default()).collect();
    let members = Rc::new(members);
    memo.store(key, Rc::clone(&members));
    members
}

memo_map!(VbModules, String, bool);

/// Whether a VB.NET owner (by qualified name) is a `Module`, whose members are reached without a qualifier.
pub fn is_module(owner_qn: &str, ctx: &Ctx) -> bool {
    let memo = ctx.memo::<VbModules>();
    if let Some(hit) = memo.lookup(&owner_qn.to_string()) {
        return hit;
    }
    // Its own head line or the two after it (attributes may come first); the raw lines, as the
    // whole file need not be read as code for this.
    let module = ctx.get_nodes_by_qualified_name(owner_qn).iter().any(|n| {
        is_vb(n)
            && n.kind == NodeKind::Class
            && ctx.get_file_lines(&n.file_path).is_some_and(|lines| {
                let from = (n.start_line as usize).saturating_sub(1);
                lines
                    .iter()
                    .skip(from)
                    .take(3)
                    .any(|l| re!(r"^\s*(?:<[^>]*>\s*)*(?:(?:Public|Friend|Private|Partial)\s+)*Module\s", "i").is_match(l))
            })
    });
    memo.store(owner_qn.to_string(), module);
    module
}

/// The members named `name` of the project's Modules, which code reaches without a qualifier —
/// the caller's project's first.
pub fn module_members_named(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Vec<NodeRef> {
    let members: Vec<NodeRef> = ctx
        .get_nodes_by_lower_name(&js_lower(name))
        .iter()
        .filter(|n| match n.qualified_name.rfind("::") {
            Some(cut) => is_vb(n) && cut > 0 && is_module(&n.qualified_name[..cut], ctx),
            None => false,
        })
        .cloned()
        .collect();
    prefer_vb_project(members, r, ctx)
}

// -- Supertypes -------------------------------------------------------------------------------------

memo_map!(VbSupers, String, Rc<Vec<VbType>>);

/// The types a VB.NET type's declarations (every `Partial` part) inherit or implement: the
/// `Inherits` / `Implements` statements that open its body, including the `Class X : Inherits Y` form.
pub fn supertypes_of(ty: &Node, ctx: &Ctx) -> Rc<Vec<VbType>> {
    let memo = ctx.memo::<VbSupers>();
    let key = js_lower(&ty.qualified_name);
    if let Some(hit) = memo.lookup(&key) {
        return hit;
    }
    let mut supers: Vec<VbType> = Vec::new();
    let found = ctx.get_nodes_by_qualified_name(&ty.qualified_name);
    let mut parts: Vec<&Node> = found.iter().filter(|n| is_vb(n) && VB_TYPE_KINDS.contains(&n.kind)).map(|n| &**n).collect();
    if parts.is_empty() {
        parts.push(ty);
    }
    for part in parts {
        let lines = code_lines(&part.file_path, ctx);
        let first = (part.start_line as usize).saturating_sub(1);
        let last = lines.len().min(part.start_line as usize + 12).min(part.end_line as usize);
        'scan: for i in first..last {
            for statement in re!(r":(?!=)").split(&lines[i]) {
                let s = codegraph_types::jscompat::js_trim(statement);
                // Blank, an attribute, a directive, or the declaration's own head.
                if s.is_empty()
                    || re!(r"^<.*>$").is_match(s)
                    || s.starts_with('#')
                    || (re!(r"\b(?:Class|Structure|Interface|Module)\s+[A-Za-z_]", "i").is_match(s) && !re!(r"^(?:Inherits|Implements)\b", "i").is_match(s))
                {
                    continue;
                }
                let Some(clause) = re!(r"^(Inherits|Implements)\s+(.+)$", "i").captures(s) else { break 'scan };
                let implements = re!(r"^Implements$", "i").is_match(clause.text(1).unwrap_or(""));
                for sup in split_args(&format!("({})", clause.text(2).unwrap_or("")), 0).unwrap_or_default() {
                    if let Some(t) = sited(read_type(&sup, 0), &part.file_path, part.start_line) {
                        supers.push(if implements { VbType { implemented: true, ..t } } else { t });
                    }
                }
            }
        }
    }
    let supers = Rc::new(supers);
    memo.store(key, Rc::clone(&supers));
    supers
}

/// A type a VB.NET type inherits, with what its type parameters stand for there (`T` → `SiteSettings`).
#[derive(Clone)]
pub struct VbAncestor {
    pub node: NodeRef,
    pub args: TypeArgs,
}

memo_map!(VbAncestries, String, Rc<Vec<VbAncestor>>);

/// `t` replaced by what `args` says its type parameter stands for, else `t` itself.
pub fn substitute_found(t: &VbType, args: &TypeArgs) -> Option<VbType> {
    if t.qualifier.is_some() || t.array {
        return None;
    }
    args.get(&js_lower(&t.name)).cloned()
}

pub fn substitute(t: &VbType, args: &TypeArgs) -> VbType {
    substitute_found(t, args).unwrap_or_else(|| t.clone())
}

/// A VB.NET type and the project types it inherits, nearest first: a class's base classes, an
/// interface's base interfaces — whose members are the type's own. Each carries the type
/// arguments `Inherits Checker(Of SiteSettings)` gives its parameters. An interface a class
/// implements lends it no members: they are reached through the class's own `… Implements
/// IFoo.Bar` ones.
pub fn ancestry(ty: &NodeRef, ctx: &Ctx) -> Rc<Vec<VbAncestor>> {
    let memo = ctx.memo::<VbAncestries>();
    if let Some(hit) = memo.lookup(&ty.id) {
        return hit;
    }
    let mut out: Vec<VbAncestor> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<VbAncestor> = VecDeque::new();
    queue.push_back(VbAncestor { node: Rc::clone(ty), args: TypeArgs::new() });
    while !queue.is_empty() && out.len() < 16 {
        let current = queue.pop_front().unwrap();
        let key = js_lower(&current.node.qualified_name);
        if seen.contains(&key) {
            continue;
        }
        seen.insert(key);
        out.push(current.clone());
        for sup in supertypes_of(&current.node, ctx).iter() {
            if sup.implemented {
                continue;
            }
            let found = types_named_at(sup, ctx, &VB_TYPE_KINDS);
            if found.ambiguous {
                continue;
            }
            let given: Vec<VbType> = sup.args.iter().flatten().map(|a| substitute(a, &current.args)).collect();
            for base in found.owners {
                let params: Vec<String> = type_parameters_of(&base, ctx).keys().cloned().collect();
                let args: TypeArgs = params.into_iter().enumerate().filter_map(|(i, p)| given.get(i).map(|g| (p, g.clone()))).collect();
                queue.push_back(VbAncestor { node: base, args });
            }
        }
    }
    let out = Rc::new(out);
    memo.store(ty.id.clone(), Rc::clone(&out));
    out
}

/// A VB.NET type and the project types it inherits, nearest first (see `ancestry`).
pub fn hierarchy(ty: &NodeRef, ctx: &Ctx) -> Vec<NodeRef> {
    ancestry(ty, ctx).iter().map(|a| Rc::clone(&a.node)).collect()
}

memo_map!(VbTypeParams, String, Rc<IndexMap<String, Option<VbType>>>);

/// The type parameters a declaration's head names (`Class Repo(Of T As Entity)`), each with its constraint or none.
pub fn type_parameters_of(n: &Node, ctx: &Ctx) -> Rc<IndexMap<String, Option<VbType>>> {
    let memo = ctx.memo::<VbTypeParams>();
    if let Some(hit) = memo.lookup(&n.id) {
        return hit;
    }
    let params = Rc::new(read_type_parameters(n, ctx));
    memo.store(n.id.clone(), Rc::clone(&params));
    params
}

fn read_type_parameters(n: &Node, ctx: &Ctx) -> IndexMap<String, Option<VbType>> {
    let mut params: IndexMap<String, Option<VbType>> = IndexMap::new();
    let code = code_slice(&n.file_path, n.start_line as i64 - 1, n.start_line as i64 + 2, ctx);
    let pattern = format!(
        r"\b(?:Class|Structure|Interface|Module|Function|Sub)\s+{}\s*(?=\(\s*Of\b)",
        codegraph_jsre::escape_regex(&n.name)
    );
    let Some(head) = codegraph_jsre::Re::cached(&pattern, "i").captures(&code) else { return params };
    let at = head.get(0).map(|m| m.end()).unwrap_or(0);
    for p in split_args(&code, at).unwrap_or_default() {
        let Some(m) = re!(r"^(?:Of\s+)?(?:In\s+|Out\s+)?([A-Za-z_]\w*)(?:\s+As\s+(.+))?$", "i").captures(&p) else { continue };
        let raw = re!(r"^\{|\}$", "g").replace_all_str(m.text(2).unwrap_or(""), "");
        let constraints: Vec<String> = raw
            .split(',')
            .map(|c| codegraph_types::jscompat::js_trim(c).to_string())
            .filter(|c| !c.is_empty() && !re!(r"^(?:New|Class|Structure)$", "i").is_match(c))
            .collect();
        let value = if constraints.is_empty() { None } else { sited(read_type(&constraints[0], 0), &n.file_path, n.start_line) };
        params.insert(js_lower(m.text(1).unwrap_or("")), value);
    }
    params
}

// -- Projects -----------------------------------------------------------------------------------------

memo_map!(VbProjects, String, Option<String>);

/// The directory of the nearest `.vbproj` at or above `dir` (`""` for the root), or none outside any project.
fn project_of_dir(dir: &str, ctx: &Ctx, memo: &VbProjects) -> Option<String> {
    if let Some(hit) = memo.lookup(&dir.to_string()) {
        return hit;
    }
    let mut found: Option<String> = None;
    if let Ok(entries) = std::fs::read_dir(Path::new(ctx.project_root()).join(dir)) {
        if entries.flatten().any(|e| e.file_name().to_string_lossy().to_ascii_lowercase().ends_with(".vbproj")) {
            found = Some(dir.to_string());
        }
    }
    if found.is_none() && !dir.is_empty() {
        let parent = dir.rfind('/').map(|c| &dir[..c]).unwrap_or("");
        found = project_of_dir(parent, ctx, memo);
    }
    memo.store(dir.to_string(), found.clone());
    found
}

/// The project (the directory of the nearest `.vbproj` above it) a file belongs to, or none.
pub fn project_of(file: &str, ctx: &Ctx) -> Option<String> {
    let memo = ctx.memo::<VbProjects>();
    let dir = file.rfind('/').map(|c| &file[..c]).unwrap_or("");
    project_of_dir(dir, ctx, &memo)
}

/// Whether two files are in the same VB.NET project — false when either is in none.
pub fn same_vb_project(a: &str, b: &str, ctx: &Ctx) -> bool {
    let project = project_of(a, ctx);
    project.is_some() && project == project_of(b, ctx)
}

#[derive(Default)]
pub struct VbProjectInfo {
    /// The root namespace every file of the project declares its namespaces inside, as segments.
    pub root: Vec<String>,
    /// The namespaces the project imports into every file (`<Import Include="…" />`).
    pub imports: Vec<String>,
}

memo_map!(VbProjectInfos, String, Rc<VbProjectInfo>);

/// A file's project's root namespace (`<RootNamespace>`, else the project's name) and
/// project-wide imports, lowercased.
pub fn project_info(file: &str, ctx: &Ctx) -> Rc<VbProjectInfo> {
    let Some(dir) = project_of(file, ctx) else { return Rc::new(VbProjectInfo::default()) };
    let memo = ctx.memo::<VbProjectInfos>();
    if let Some(hit) = memo.lookup(&dir) {
        return hit;
    }
    let mut info = VbProjectInfo::default();
    let abs = Path::new(ctx.project_root()).join(&dir);
    if let Ok(entries) = std::fs::read_dir(&abs) {
        // Directory listings come back sorted, as Node's do.
        let mut names: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        if let Some(project) = names.into_iter().find(|n| n.to_ascii_lowercase().ends_with(".vbproj")) {
            if let Ok(bytes) = std::fs::read(abs.join(&project)) {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let root = match re!(r"<RootNamespace>\s*([\w.]*)\s*<\/RootNamespace>", "i").captures(&text) {
                    Some(c) => c.text(1).unwrap_or("").to_string(),
                    None => project[..project.len() - ".vbproj".len()].to_string(),
                };
                info.root = js_lower(&root).split('.').filter(|s| !s.is_empty()).map(String::from).collect();
                for m in re!(r#"<Import\s+Include\s*=\s*"([\w.]+)""#, "gi").captures_iter(&text) {
                    info.imports.push(js_lower(m.text(1).unwrap_or("")));
                }
            }
        }
    }
    let info = Rc::new(info);
    memo.store(dir, Rc::clone(&info));
    info
}

/// A node's full qualified name as segments: its project's root namespace, then its namespaces and types.
pub fn full_segments(n: &Node, ctx: &Ctx) -> Vec<String> {
    let mut segments: Vec<String> = project_info(&n.file_path, ctx).root.clone();
    for s in js_lower(&n.qualified_name).split("::") {
        segments.extend(s.split('.').map(String::from));
    }
    segments
}

/// All but the last segment: the namespaces and types a name is declared in.
pub fn container_of(n: &Node, ctx: &Ctx) -> Vec<String> {
    let mut segments = full_segments(n, ctx);
    segments.pop();
    segments
}

/// A file's lines before its first declaration: where its `Option` and `Imports` statements are.
fn header_lines(file: &str, ctx: &Ctx) -> Vec<String> {
    let lines = code_lines(file, ctx);
    let end = lines.iter().position(|l| {
        re!(r"^\s*(?:Namespace|Module|Class|Structure|Interface|Enum|Delegate|Public|Friend|Private|Protected|Partial|NotInheritable|MustInherit)\b", "i").is_match(l)
    });
    match end {
        Some(e) => lines[..e].to_vec(),
        None => lines.to_vec(),
    }
}

memo_map!(VbFileImports, String, Rc<Vec<String>>);

/// The namespaces a file imports — its own `Imports` and its project's — lowercased.
pub fn imported_namespaces(file: &str, ctx: &Ctx) -> Rc<Vec<String>> {
    let memo = ctx.memo::<VbFileImports>();
    if let Some(hit) = memo.lookup(&file.to_string()) {
        return hit;
    }
    let mut imports: Vec<String> = project_info(file, ctx).imports.clone();
    for line in header_lines(file, ctx) {
        if let Some(m) = re!(r"^\s*Imports\s+([\w.]+)\s*$", "i").captures(&line) {
            let lower = js_lower(m.text(1).unwrap_or(""));
            imports.push(lower.strip_prefix("global.").map(String::from).unwrap_or(lower));
        }
    }
    let imports = Rc::new(imports);
    memo.store(file.to_string(), Rc::clone(&imports));
    imports
}

memo_map!(VbAliases, String, Rc<HashMap<String, VbType>>);

/// A file's import aliases: `Imports TDJob = SCrawler.DownloadObjects.TDownloader.Job`, by lowercased alias.
pub fn import_aliases(file: &str, ctx: &Ctx) -> Rc<HashMap<String, VbType>> {
    let memo = ctx.memo::<VbAliases>();
    if let Some(hit) = memo.lookup(&file.to_string()) {
        return hit;
    }
    let mut aliases: HashMap<String, VbType> = HashMap::new();
    for line in header_lines(file, ctx) {
        if let Some(m) = re!(r"^\s*Imports\s+([A-Za-z_]\w*)\s*=\s*(.+)$", "i").captures(&line) {
            if let Some(target) = read_type(m.text(2).unwrap_or(""), 0) {
                aliases.insert(js_lower(m.text(1).unwrap_or("")), target);
            }
        }
    }
    let aliases = Rc::new(aliases);
    memo.store(file.to_string(), Rc::clone(&aliases));
    aliases
}

/// `t` with an import alias it is written with — as itself or as its first qualifier — spelled out.
pub fn unalias(t: &VbType, file: &str, ctx: &Ctx) -> VbType {
    let aliases = import_aliases(file, ctx);
    if aliases.is_empty() {
        return t.clone();
    }
    match &t.qualifier {
        None => match aliases.get(&js_lower(&t.name)) {
            Some(target) => VbType { name: target.name.clone(), qualifier: target.qualifier.clone(), ..t.clone() },
            None => t.clone(),
        },
        Some(q) => match aliases.get(&q[0]) {
            Some(target) => {
                let mut qualifier: Vec<String> = target.qualifier.clone().unwrap_or_default();
                qualifier.push(js_lower(&target.name));
                qualifier.extend(q[1..].iter().cloned());
                VbType { qualifier: Some(qualifier), ..t.clone() }
            }
            None => t.clone(),
        },
    }
}

/// Whether `tail` is the end of `full`.
pub fn ends_with(full: &[String], tail: &[String]) -> bool {
    tail.len() <= full.len() && tail.iter().enumerate().all(|(i, s)| full[full.len() - tail.len() + i] == *s)
}

/// The class, structure or interface (not a `Module`) a VB.NET type is nested in, or none.
pub fn enclosing_type(n: &Node, ctx: &Ctx) -> Option<NodeRef> {
    let cut = n.qualified_name.rfind("::")?;
    let parent_qn = &n.qualified_name[..cut];
    let parent = ctx.get_nodes_by_qualified_name(parent_qn).iter().find(|p| is_vb(p) && VB_TYPE_KINDS.contains(&p.kind)).cloned()?;
    if is_module(parent_qn, ctx) {
        None
    } else {
        Some(parent)
    }
}

pub fn is_nested_in_type(n: &Node, ctx: &Ctx) -> bool {
    enclosing_type(n, ctx).is_some()
}

memo_map!(VbBaseNames, String, Rc<HashSet<String>>);

/// The lowercased names of the types around a line and of the classes they inherit, walked by
/// name (a few levels): the types whose nested types code there names bare — `Dim ret As New
/// MenuList` in a `VideoEncoder` subclass.
fn base_type_names_around(file: &str, line: u32, ctx: &Ctx) -> Rc<HashSet<String>> {
    let around = types_around(file, line, ctx);
    let memo = ctx.memo::<VbBaseNames>();
    let key = around.iter().map(|t| t.id.as_str()).collect::<Vec<_>>().join("|");
    if let Some(hit) = memo.lookup(&key) {
        return hit;
    }
    let mut names: HashSet<String> = HashSet::new();
    let mut frontier = around;
    let mut depth = 0;
    while depth < 5 && !frontier.is_empty() {
        let mut next: Vec<NodeRef> = Vec::new();
        for t in &frontier {
            let lower = js_lower(&t.name);
            if names.contains(&lower) {
                continue;
            }
            names.insert(lower);
            for sup in supertypes_of(t, ctx).iter() {
                if !sup.implemented {
                    next.extend(project_types_named(&sup.name, ctx, &VB_TYPE_KINDS));
                }
            }
        }
        frontier = next;
        depth += 1;
    }
    let names = Rc::new(names);
    memo.store(key, Rc::clone(&names));
    names
}

/// Whether a type is one a name written with this qualifier can mean: its namespaces and outer
/// types end with what is written before the name. Designer code's `New System.Drawing.Point(3,
/// 3)` is not staxrip's nested `ButtonEx.SymbolDrawer.Point`; `New API.Base.UserDataBase(…)` is
/// that one.
pub fn is_vb_type_qualified_by(n: &Node, qualifier: &str, file: &str, ctx: &Ctx) -> bool {
    if !is_vb(n) || !VB_TYPE_LIKE_KINDS.contains(&n.kind) {
        return true;
    }
    let Some(written) = read_type(&format!("{qualifier}.{}", n.name), 0) else { return true };
    if written.qualifier.is_none() {
        return true;
    }
    let t = unalias(&written, file, ctx);
    match &t.qualifier {
        None => true,
        Some(q) => ends_with(&container_of(n, ctx), q),
    }
}

/// The project types a type name resolves to at a site, and whether they are different types.
pub struct Named {
    pub owners: Vec<NodeRef>,
    pub ambiguous: bool,
}

/// The project types a type name written at a site means, as VB.NET looks it up: one its written
/// qualifier names, nested in or declared in the namespace of a type around the site — the
/// nearest first — else nested in a class those inherit, else in a namespace the file or project
/// imports, else in the site's own project (a class's nested type only where it is in scope).
/// SCrawler declares a `SiteSettings` in each site's namespace (`API.Pinterest`, `API.Bluesky`,
/// …); a member typed `SiteSettings` in `API.Pinterest.UserData` is Pinterest's. `ambiguous`
/// when what is left are different types. `kinds` widens the types looked for (an `Enum` a value
/// is read through).
pub fn types_named_at(written: &VbType, ctx: &Ctx, kinds: &[NodeKind]) -> Named {
    let t = match &written.file {
        Some(file) => unalias(written, file, ctx),
        None => written.clone(),
    };
    let mut candidates = project_types_named(&t.name, ctx, kinds);
    // `System.Drawing.Color` is not the project's `Color`; `API.Base.UserDataBase` is that one.
    if let Some(q) = &t.qualifier {
        candidates.retain(|c| ends_with(&container_of(c, ctx), q));
    }
    let Some(file) = t.file.clone() else { return Named { owners: candidates, ambiguous: false } };
    if candidates.is_empty() {
        return Named { owners: candidates, ambiguous: false };
    }
    let line = t.line.unwrap_or(0);
    let site = types_around(&file, line, ctx).into_iter().next();
    let site_path: Vec<String> = match &site {
        Some(s) => full_segments(s, ctx),
        None => project_info(&file, ctx).root.clone(),
    };
    let mut tier: Vec<NodeRef> = Vec::new();
    let mut best: i64 = -1;
    for c in &candidates {
        let container = container_of(c, ctx);
        if container.len() > site_path.len() || container.iter().enumerate().any(|(i, s)| *s != site_path[i]) {
            continue;
        }
        if container.len() as i64 > best {
            best = container.len() as i64;
            tier = vec![Rc::clone(c)];
        } else if container.len() as i64 == best {
            tier.push(Rc::clone(c));
        }
    }
    // A type nested in a class the site's types inherit.
    if tier.is_empty() && t.qualifier.is_none() {
        let bases = base_type_names_around(&file, line, ctx);
        tier = candidates.iter().filter(|c| bases.contains(&enclosing_type(c, ctx).map(|e| js_lower(&e.name)).unwrap_or_default())).cloned().collect();
    }
    if tier.is_empty() {
        let imports = imported_namespaces(&file, ctx);
        tier = candidates.iter().filter(|c| imports.contains(&container_of(c, ctx).join("."))).cloned().collect();
    }
    // Else any the project declares in a namespace (the root namespaces and project imports this
    // cannot see) — but not a class's nested type, which is named bare only inside it or a class
    // deriving from it.
    if tier.is_empty() {
        tier = if t.qualifier.is_some() { candidates.clone() } else { candidates.iter().filter(|c| !is_nested_in_type(c, ctx)).cloned().collect() };
    }
    // A project's own declaration over another project's of the same name (staxrip's main app and
    // its AutoCrop tool each declare a `ColorHSL`).
    let own: Vec<NodeRef> = tier.iter().filter(|c| same_vb_project(&c.file_path, &file, ctx)).cloned().collect();
    if !own.is_empty() {
        tier = own;
    }
    // One type's partial parts are one type.
    let distinct: HashSet<String> = tier.iter().map(|c| format!("{}|{}", project_key(&c.file_path, ctx), js_lower(&c.qualified_name))).collect();
    let mut owners: Vec<NodeRef> = tier.iter().filter(|c| c.file_path == file).cloned().collect();
    owners.extend(tier.iter().filter(|c| c.file_path != file).cloned());
    Named { owners, ambiguous: distinct.len() > 1 }
}

// -- Namespaces ---------------------------------------------------------------------------------------

#[derive(Default)]
pub struct VbNamespaces(RefCell<Option<Rc<HashSet<String>>>>);

/// Every namespace the project's VB.NET code is declared in, by its full name (lowercased,
/// dot-joined, root namespace first), and each namespace around one: `acme`, `acme.billing`,
/// `scrawler.api.reddit`.
pub fn known_namespaces(ctx: &Ctx) -> Rc<HashSet<String>> {
    let memo = ctx.memo::<VbNamespaces>();
    if let Some(hit) = memo.0.borrow().as_ref() {
        return Rc::clone(hit);
    }
    let mut known: HashSet<String> = HashSet::new();
    let add = |known: &mut HashSet<String>, segments: &[String]| {
        for i in 1..=segments.len() {
            known.insert(segments[..i].join("."));
        }
    };
    for n in ctx.get_nodes_by_kind(NodeKind::Namespace).iter() {
        if is_vb(n) {
            add(&mut known, &full_segments(n, ctx));
        }
    }
    for file in ctx.get_all_files() {
        if file.to_ascii_lowercase().ends_with(".vb") {
            add(&mut known, &project_info(&file, ctx).root);
        }
    }
    let known = Rc::new(known);
    *memo.0.borrow_mut() = Some(Rc::clone(&known));
    known
}

/// The namespace a line of a file is in, as full segments: its project's root namespace, then the `Namespace` block around it.
fn namespace_around(file: &str, line: u32, ctx: &Ctx) -> Vec<String> {
    let root = project_info(file, ctx).root.clone();
    let around = types_around(file, line, ctx);
    let Some(outer) = around.last() else { return root };
    let Some(cut) = outer.qualified_name.find("::") else { return root };
    let head = js_lower(&outer.qualified_name[..cut]);
    let is_namespace = nodes_by_qualified_name(file, ctx).get(&head).is_some_and(|nodes| nodes.iter().any(|n| n.kind == NodeKind::Namespace));
    if is_namespace {
        let mut full = root;
        full.extend(head.split('.').map(String::from));
        full
    } else {
        root
    }
}

/// The namespace a name written first in a path means where it is written, as full segments — as
/// VB.NET looks one up: nested in a namespace around the read, the innermost first and the root
/// namespaces last, else in one the file or its project imports. None when no namespace of that
/// name is there: `Billing` is Acme's `Billing` in Acme's code, and nothing in a project whose
/// own namespaces don't have one.
pub fn namespace_named(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<Vec<String>> {
    let known = known_namespaces(ctx);
    let key = js_lower(name);
    let around = namespace_around(&r.file_path, r.line, ctx);
    for i in (0..=around.len()).rev() {
        let mut candidate: Vec<String> = around[..i].to_vec();
        candidate.push(key.clone());
        if known.contains(&candidate.join(".")) {
            return Some(candidate);
        }
    }
    for imported in imported_namespaces(&r.file_path, ctx).iter() {
        let mut candidate: Vec<String> = imported.split('.').map(String::from).collect();
        candidate.push(key.clone());
        if known.contains(&candidate.join(".")) {
            return Some(candidate);
        }
    }
    None
}

/// The project types named `name` declared directly in namespace `ns` (full segments): the
/// caller's project's, if it has one — SCrawler's projects share a root namespace — and none when
/// that leaves two different types.
pub fn types_in_namespace(ns: &[String], name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<Vec<NodeRef>> {
    let key = ns.join(".");
    let mut found: Vec<NodeRef> = project_types_named(name, ctx, &VB_READ_TYPE_KINDS).into_iter().filter(|c| container_of(c, ctx).join(".") == key).collect();
    let own: Vec<NodeRef> = found.iter().filter(|c| same_vb_project(&c.file_path, &r.file_path, ctx)).cloned().collect();
    if !own.is_empty() {
        found = own;
    }
    let distinct: HashSet<String> = found.iter().map(|c| format!("{}|{}", project_key(&c.file_path, ctx), js_lower(&c.qualified_name))).collect();
    if distinct.len() > 1 {
        return None;
    }
    let mut out: Vec<NodeRef> = found.iter().filter(|c| c.file_path == r.file_path).cloned().collect();
    out.extend(found.iter().filter(|c| c.file_path != r.file_path).cloned());
    Some(out)
}

/// A member named `name` of a `Module` declared directly in namespace `ns`, which VB.NET reads
/// through the namespace: `Assets.Logo`.
pub fn module_member_in(ns: &[String], name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    let key = ns.join(".");
    module_members_named(name, r, ctx).into_iter().find(|n| {
        (VB_VALUE_KINDS.contains(&n.kind) || n.kind == NodeKind::Method) && {
            let mut segments = full_segments(n, ctx);
            segments.truncate(segments.len().saturating_sub(2));
            segments.join(".") == key
        }
    })
}

/// The call site's own file first, then its project's, the rest after, each in its given order.
pub fn prefer_vb_project(nodes: Vec<NodeRef>, r: &UnresolvedRef, ctx: &Ctx) -> Vec<NodeRef> {
    if nodes.len() < 2 {
        return nodes;
    }
    let mut file: Vec<NodeRef> = Vec::new();
    let mut project: Vec<NodeRef> = Vec::new();
    let mut rest: Vec<NodeRef> = Vec::new();
    for n in nodes {
        if n.file_path == r.file_path {
            file.push(n);
        } else if same_vb_project(&n.file_path, &r.file_path, ctx) {
            project.push(n);
        } else {
            rest.push(n);
        }
    }
    file.extend(project);
    file.extend(rest);
    file
}

/// How many leading directories two files share.
fn shared_dirs(a: &str, b: &str) -> usize {
    let da: Vec<&str> = {
        let mut v: Vec<&str> = a.split('/').collect();
        v.pop();
        v
    };
    let db: Vec<&str> = {
        let mut v: Vec<&str> = b.split('/').collect();
        v.pop();
        v
    };
    let mut i = 0;
    while i < da.len() && i < db.len() && da[i] == db[i] {
        i += 1;
    }
    i
}

/// The one of several equally good guesses a call means: the one in its own file, else in its own
/// project, else in the nearest directory — or none when none of these tells them apart.
pub fn break_vb_tie(tied: &[NodeRef], r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    // One type's overloads (and partial parts) are one guess, not a tie: the first stands for them.
    let mut seen: HashSet<String> = HashSet::new();
    let distinct: Vec<NodeRef> = tied
        .iter()
        .filter(|n| {
            let key = format!("{}|{}", project_key(&n.file_path, ctx), js_lower(&n.qualified_name));
            seen.insert(key)
        })
        .cloned()
        .collect();
    if distinct.len() == 1 {
        return Some(Rc::clone(&distinct[0]));
    }
    let mut pool: Vec<NodeRef> = distinct.iter().filter(|n| n.file_path == r.file_path).cloned().collect();
    if pool.is_empty() {
        let own: Vec<NodeRef> = distinct.iter().filter(|n| same_vb_project(&n.file_path, &r.file_path, ctx)).cloned().collect();
        pool = if own.is_empty() { distinct } else { own };
    }
    if pool.len() == 1 {
        return Some(Rc::clone(&pool[0]));
    }
    let mut best: i64 = -1;
    let mut winners: Vec<NodeRef> = Vec::new();
    for n in &pool {
        let shared = shared_dirs(&r.file_path, &n.file_path) as i64;
        if shared > best {
            best = shared;
            winners = vec![Rc::clone(n)];
        } else if shared == best {
            winners.push(Rc::clone(n));
        }
    }
    if winners.len() == 1 {
        winners.pop()
    } else {
        None
    }
}
