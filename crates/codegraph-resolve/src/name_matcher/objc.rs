//! Objective-C scope rules: how a call is written (a C call, a `[self …]` / `[super …]` send, a
//! send to some other receiver), which class hierarchy a message sent to `self` can reach, and
//! what type an instance variable or property receiver has.

use super::lang_shape::RefShape;
use super::util::*;
use crate::ctx::Ctx;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::{Language, Node, NodeKind};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::{Arc, LazyLock};

fn is_objc_member_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Method | NodeKind::Property | NodeKind::Field)
}

/// A C-level declaration of an Objective-C file is private to its translation unit.
pub fn is_objc_file_local_elsewhere(n: &Node, r: &UnresolvedRef) -> bool {
    n.language == Language::ObjC
        && n.file_path != r.file_path
        && matches!(n.kind, NodeKind::Struct | NodeKind::Union | NodeKind::Enum | NodeKind::TypeAlias | NodeKind::Variable | NodeKind::Constant)
        && re!(r"\.(c|cc|cpp|cxx|c\+\+|m|mm)$", "i").is_match(&n.file_path)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjcCallShape {
    CCall,
    SelfSend,
    SuperSend,
}

/// How the reference's selector is written on its line: `name(…)`, `[self name…]`, `[super name…]`.
pub fn objc_call_shape(r: &UnresolvedRef, ctx: &Ctx) -> Option<ObjcCallShape> {
    let line = line_at(ctx, &r.file_path, r.line).or_else(|| {
        let idx = (r.line as usize).checked_sub(1)?;
        ctx.read_file(&r.file_path).and_then(|s| s.split('\n').nth(idx).map(String::from))
    })?;
    let name = r.reference_name.split(':').next().unwrap_or("");
    if name.is_empty() {
        return None;
    }
    let at = Re::cached(&format!(r"(?<![\w.]){}\b", escape_regex(name)), "g");
    let mut shape: Option<ObjcCallShape> = None;
    for m in at.captures_iter(&line) {
        let Some(whole) = m.get(0) else { continue };
        let before = &line[..whole.start()];
        let after = &line[whole.start() + name.len()..];
        if re!(r"^\s*\(").is_match(after) && !re!(r"\[\s*[\w.]+\s+$").is_match(before) {
            shape.get_or_insert(ObjcCallShape::CCall);
        } else if re!(r"\[\s*super\s+$").is_match(before) {
            return Some(ObjcCallShape::SuperSend);
        } else if re!(r"\[\s*(?:self|\[\s*self\s+class\s*\])\s+$").is_match(before) {
            return Some(ObjcCallShape::SelfSend);
        }
    }
    shape
}

/// The class the reference is inside (or the owner of the method inside), then its superclasses,
/// nearest first.
pub fn objc_hierarchy_at(r: &UnresolvedRef, ctx: &Ctx) -> Option<Vec<String>> {
    let in_file = ctx.get_nodes_in_file(&r.file_path);
    let mut classes: Vec<&NodeRef> = in_file.iter().filter(|c| c.kind == NodeKind::Class && c.start_line <= r.line && c.end_line >= r.line).collect();
    classes.sort_by_key(|c| c.end_line.saturating_sub(c.start_line));
    let here = match classes.first() {
        Some(c) => c.name.clone(),
        None => {
            let method = in_file.iter().find(|c| c.kind == NodeKind::Method && c.start_line <= r.line && c.end_line >= r.line && c.qualified_name.contains("::"))?;
            method.qualified_name[..method.qualified_name.rfind("::")?].to_string()
        }
    };
    if here.is_empty() {
        return None;
    }
    let mut seen: Vec<String> = Vec::new();
    let mut queue: VecDeque<String> = VecDeque::from([here]);
    while let Some(name) = queue.pop_front() {
        if seen.len() >= 30 {
            break;
        }
        if seen.contains(&name) {
            continue;
        }
        queue.extend(objc_supertypes_of(&name, ctx).iter().cloned());
        seen.push(name);
    }
    Some(seen)
}

/// Whether a candidate is one a `[self name]` (or, with `to_super`, `[super name]`) message can reach.
pub fn is_objc_self_send_target(n: &Node, r: &UnresolvedRef, ctx: &Ctx, to_super: bool) -> bool {
    let hierarchy = objc_hierarchy_at(r, ctx);
    if is_objc_member_kind(n.kind) {
        let Some(cut) = n.qualified_name.rfind("::") else { return true };
        let Some(hierarchy) = hierarchy else { return true };
        let owner = &n.qualified_name[..cut];
        return hierarchy.iter().any(|h| h == owner) && !(to_super && hierarchy.first().is_some_and(|f| f == owner));
    }
    if n.file_path == r.file_path {
        return !to_super;
    }
    let Some(hierarchy) = hierarchy else { return true };
    let file = n.file_path.rsplit('/').next().unwrap_or("");
    let base = re!(r"\.\w+$").replace_first(file, |_| String::new());
    hierarchy.iter().any(|h| *h == base) && !(to_super && hierarchy.first().is_some_and(|f| *f == base))
}

/// The platform class hierarchy a project subclass inherits members through.
pub fn objc_system_super(name: &str) -> Option<&'static str> {
    Some(match name {
        "UIResponder" => "NSObject",
        "UIView" | "UIViewController" => "UIResponder",
        "UIWindow" => "UIView",
        "UIControl" => "UIView",
        "UIButton" | "UITextField" | "UISwitch" | "UISlider" | "UISegmentedControl" | "UIStepper" | "UIPageControl" | "UIDatePicker" | "UIRefreshControl" => "UIControl",
        "UIStackView" | "UINavigationBar" | "UIToolbar" | "UITabBar" | "UISearchBar" | "UIVisualEffectView" | "UIActivityIndicatorView" | "UIProgressView"
        | "UIPickerView" | "UITableViewHeaderFooterView" => "UIView",
        "UITableViewController" | "UICollectionViewController" | "UINavigationController" | "UITabBarController" | "UIPageViewController" | "UISplitViewController"
        | "UIAlertController" | "UIHostingController" => "UIViewController",
        "UIImageView" | "UILabel" | "UIScrollView" => "UIView",
        "UITableView" | "UICollectionView" | "UITextView" => "UIScrollView",
        "UITableViewCell" | "UICollectionReusableView" => "UIView",
        "UICollectionViewCell" => "UICollectionReusableView",
        "MKAnnotationView" | "MKMapView" => "UIView",
        "NSResponder" => "NSObject",
        "NSView" | "NSViewController" | "NSWindow" => "NSResponder",
        "NSControl" => "NSView",
        "NSImageView" | "NSButton" | "NSTextField" | "NSTableView" => "NSControl",
        _ => return None,
    })
}

/// Whether a receiver (`Foo`, `self.delegate`, `obj.child`) names a class that is, or derives
/// from, the method's owner.
pub fn objc_receiver_reaches(receiver: &str, method: &Node, ctx: &Ctx) -> bool {
    let Some(cut) = method.qualified_name.rfind("::") else { return false };
    let owner = &method.qualified_name[..cut];
    let mut types: Vec<String> = Vec::new();
    if re!(r"^[A-Z]\w*$").is_match(receiver) {
        if ctx.get_nodes_by_name(receiver).iter().any(|n| n.kind == NodeKind::Class && n.language == Language::ObjC) {
            types = vec![receiver.to_string()];
        }
    } else if receiver.contains('.') {
        types = objc_property_types(receiver.rsplit('.').next().unwrap_or(""), ctx);
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = types.into_iter().collect();
    while let Some(name) = queue.pop_front() {
        if seen.len() >= 40 {
            break;
        }
        if !seen.insert(name.clone()) {
            continue;
        }
        if name == owner {
            return true;
        }
        queue.extend(objc_supertypes_of(&name, ctx).iter().cloned());
    }
    false
}

/// The declared class of every `@property` of this name.
fn objc_property_types(name: &str, ctx: &Ctx) -> Vec<String> {
    let mut types: Vec<String> = Vec::new();
    let decl = Re::cached(&format!(r"@property\s*(?:\([^)]*\)\s*)?([A-Z]\w*)\s*(?:<[^;]*>\s*)?\*\s*(?:_Nullable\s+|_Nonnull\s+)?{}\b", escape_regex(name)), "");
    for n in ctx.get_nodes_by_name(name).iter() {
        if n.kind != NodeKind::Property || n.language != Language::ObjC {
            continue;
        }
        let line = line_at(ctx, &n.file_path, n.start_line).or_else(|| ctx.read_file(&n.file_path).and_then(|s| s.split('\n').nth(n.start_line as usize - 1).map(String::from))).unwrap_or_default();
        if let Some(t) = decl.captures(&line).and_then(|c| c.text(1).map(String::from)) {
            if !types.contains(&t) {
                types.push(t);
            }
        }
    }
    types
}

/// What the `@interface` / `@implementation` bodies of one class declare.
#[derive(Default)]
pub struct ObjcClassMembers {
    ivars: HashMap<String, String>,
    own_ivars: HashMap<String, String>,
    properties: HashMap<String, (String, bool)>,
    synthesized: HashMap<String, String>,
}

memo_map!(ObjcMembersMemo, String, Rc<HashMap<String, Rc<ObjcClassMembers>>>);
memo_map!(ObjcClassFilesMemo, String, Rc<Vec<String>>);
memo_map!(ObjcSupersMemo, String, Rc<Vec<String>>);

const OBJC_MEMBER_TYPE: &str = r"(?:(?:__strong|__weak|__unsafe_unretained|__block|const|volatile|IBOutlet|IBInspectable)\s+)*([A-Z]\w*)\s*(?:<[^;{}]*?>\s*|[A-Z][A-Z0-9_]*\s*\([^;{}]*?\)\s*)?\*";

static OBJC_IVAR_RE: LazyLock<Arc<Re>> = LazyLock::new(|| {
    Re::cached(&format!(r"^\s*{OBJC_MEMBER_TYPE}\s*((?:(?:_Nullable|_Nonnull|const)\s+)*\w+(?:\s*,\s*\*\s*(?:(?:_Nullable|_Nonnull|const)\s+)*\w+)*)\s*;"), "")
});

static OBJC_PROPERTY_RE: LazyLock<Arc<Re>> = LazyLock::new(|| {
    Re::cached(
        &format!(r"^\s*@property\b\s*(?:[A-Z][A-Z0-9_]+\s*)*(?:\([^)]*\)\s*)?(?:(?:nullable|nonnull|null_unspecified|__kindof)\s+)*{OBJC_MEMBER_TYPE}\s*(?:(?:_Nullable|_Nonnull|const)\s+)*(\w+)"),
        "",
    )
});

#[derive(PartialEq, Clone, Copy)]
enum MemberState {
    Head,
    Ivars,
    Body,
}

fn count_chars(text: &str, set: &[char]) -> i64 {
    text.chars().filter(|c| set.contains(c)).count() as i64
}

/// Every class a file's `@interface` / `@implementation` / category blocks declare members of.
fn objc_file_members(file: &str, ctx: &Ctx) -> Rc<HashMap<String, Rc<ObjcClassMembers>>> {
    memoized::<ObjcMembersMemo>(ctx, file.to_string(), || {
        let lines = lines_of(ctx, file);
        let mut tables: HashMap<String, ObjcClassMembers> = HashMap::new();
        let mut current: Option<String> = None;
        let mut in_interface = false;
        let mut category: Option<String> = None;
        let mut state = MemberState::Body;
        let mut open = 0i64;
        let mut depth = 0i64;
        let mut property: Option<String> = None;
        for raw in lines.iter() {
            let mut line = re!(r"//.*$").replace_first(raw, |_| String::new());
            let head: Option<(String, bool, Option<String>, usize)> =
                re!(r"^\s*(?:[A-Z][A-Z0-9_]*(?:\([^)]*\))?\s+)*@(interface|implementation)\s+(\w+)\s*(?:<[^>]*>\s*|[A-Z][A-Z0-9_]*\([^)]*\)\s*)?(?:\(\s*(\w*)\s*\))?")
                    .captures(&line)
                    .map(|h| (h.text(2).unwrap_or("").to_string(), h.text(1) == Some("interface"), h.text(3).map(String::from), h.get(0).map_or(0, |m| m.end())));
            let had_head = head.is_some();
            if let Some((cls, interface, cat, end)) = head {
                tables.entry(cls.clone()).or_default();
                current = Some(cls);
                in_interface = interface;
                category = cat;
                state = MemberState::Head;
                open = 0;
                property = None;
                line = line[end..].to_string();
            } else if re!(r"^\s*@end\b").is_match(&line) {
                current = None;
                continue;
            }
            let Some(cls) = current.clone() else { continue };
            let members = tables.get_mut(&cls).expect("current class has a table");
            if state == MemberState::Head {
                let in_head = had_head || open > 0 || re!(r"^\s*[<:]").is_match(&line);
                open += count_chars(&line, &['<', '(']) - count_chars(&line, &['>', ')']);
                if let Some(brace) = line.find('{') {
                    state = MemberState::Ivars;
                    depth = 0;
                    line = line[brace..].to_string();
                } else if in_head || open > 0 || re!(r"^\s*(?:$|/\*|\*|#)").is_match(&line) {
                    continue;
                } else {
                    state = MemberState::Body;
                }
            }
            if state == MemberState::Ivars {
                depth += count_chars(&line, &['{']) - count_chars(&line, &['}']);
                if depth <= 0 {
                    state = MemberState::Body;
                }
                let inner = re!(r"^[^{]*\{").replace_first(&line, |_| String::new());
                let inner = re!(r"\}.*$").replace_first(&inner, |_| String::new());
                if let Some(ivar) = OBJC_IVAR_RE.captures(&inner) {
                    let ivars = if in_interface && category.is_none() { &mut members.ivars } else { &mut members.own_ivars };
                    for name in ivar.text(2).unwrap_or("").split(',') {
                        let bare = re!(r"\*|_Nullable|_Nonnull|\bconst\b", "g").replace_all_str(name, "");
                        let bare = bare.trim_matches(codegraph_types::jscompat::is_js_space);
                        if re!(r"^\w+$").is_match(bare) && !ivars.contains_key(bare) {
                            ivars.insert(bare.to_string(), ivar.text(1).unwrap_or("").to_string());
                        }
                    }
                }
                continue;
            }
            if !in_interface {
                if let Some(synthesize) = re!(r"^\s*@synthesize\s+([^;]+);").captures(&line) {
                    for item in synthesize.text(1).unwrap_or("").split(',') {
                        let mut parts = item.split('=').map(|s| s.trim_matches(codegraph_types::jscompat::is_js_space));
                        let name = parts.next().unwrap_or("");
                        let ivar = parts.next();
                        if !name.is_empty() && re!(r"^\w+$").is_match(ivar.unwrap_or(name)) {
                            members.synthesized.insert(ivar.unwrap_or(name).to_string(), name.to_string());
                        }
                    }
                }
                continue;
            }
            if property.is_none() && !re!(r"^\s*@property\b").is_match(&line) {
                continue;
            }
            let joined = match property.take() {
                None => line.clone(),
                Some(p) => format!("{p} {line}"),
            };
            if !joined.contains(';') && utf16_len_of(&joined) < 400 {
                property = Some(joined);
                continue;
            }
            let declared = OBJC_PROPERTY_RE.captures(&joined);
            if let Some(declared) = &declared {
                let name = declared.text(2).unwrap_or("").to_string();
                members.properties.entry(name).or_insert_with(|| (declared.text(1).unwrap_or("").to_string(), category.as_deref().is_some_and(|c| !c.is_empty())));
            }
        }
        Rc::new(tables.into_iter().map(|(k, v)| (k, Rc::new(v))).collect())
    })
}

fn utf16_len_of(s: &str) -> usize {
    codegraph_jsre::utf16_len(s)
}

/// The files that declare class `name`, the reference's own file first; when the class has
/// several primary `@interface`s, only those beside the reference.
fn objc_class_files(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Rc<Vec<String>> {
    let dir = &r.file_path[..r.file_path.rfind('/').map_or(0, |i| i + 1)];
    memoized::<ObjcClassFilesMemo>(ctx, format!("{name}\0{dir}"), || {
        let decls: Vec<NodeRef> = ctx.get_nodes_by_name(name).iter().filter(|n| n.kind == NodeKind::Class && n.language == Language::ObjC).cloned().collect();
        let own = Re::cached(&format!(r"@interface\s+{}\b(?!\s*\()", escape_regex(name)), "");
        let mut primary: HashSet<&str> = HashSet::new();
        for decl in &decls {
            let lines = lines_of(ctx, &decl.file_path);
            if js_slice(&lines, decl.start_line as i64 - 1, decl.end_line as i64).iter().any(|l| own.is_match(l)) {
                primary.insert(decl.file_path.as_str());
            }
        }
        let in_dir = |file: &str| file.starts_with(dir) && !file[dir.len()..].contains('/');
        let mut seen: HashSet<&str> = HashSet::new();
        let mut files: Vec<String> = decls.iter().map(|n| n.file_path.as_str()).filter(|f| seen.insert(f)).filter(|f| primary.len() < 2 || in_dir(f)).map(String::from).collect();
        files.sort_by_key(|f| std::cmp::Reverse(*f == r.file_path));
        Rc::new(files)
    })
}

/// The class an instance variable, a property (`self.name`) or a `_name` ivar is declared as.
pub fn objc_member_receiver_type(receiver: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let via_self = receiver.starts_with("self.");
    let name = if via_self { &receiver[5..] } else { receiver };
    if !re!(r"^[A-Za-z_]\w*$").is_match(name) || name == "self" || name == "super" {
        return None;
    }
    let skip = if via_self { !ctx.get_nodes_by_name(name).iter().any(|n| n.kind == NodeKind::Property && n.language == Language::ObjC) } else { re!(r"^[A-Z]").is_match(name) };
    if skip {
        return None;
    }
    let hierarchy = objc_hierarchy_at(r, ctx)?;
    // One level per class of the hierarchy: every table that declares that class.
    let levels: Vec<Vec<Rc<ObjcClassMembers>>> = hierarchy
        .iter()
        .map(|cls| objc_class_files(cls, r, ctx).iter().filter_map(|file| objc_file_members(file, ctx).get(cls).cloned()).collect())
        .collect();
    let property_type = |key: &str, own: bool| -> Option<String> {
        let scope = if own { &levels[..levels.len().min(1)] } else { &levels[..] };
        for level in scope {
            for t in level {
                if let Some((ty, category)) = t.properties.get(key) {
                    if !(own && *category) {
                        return Some(ty.clone());
                    }
                }
            }
        }
        None
    };
    if via_self {
        return property_type(name, false);
    }
    for (i, level) in levels.iter().enumerate() {
        for t in level {
            let ty = t.ivars.get(name).or_else(|| if i == 0 { t.own_ivars.get(name) } else { None });
            if let Some(ty) = ty {
                return Some(ty.clone());
            }
        }
        if i > 0 {
            continue;
        }
        let synthesized = level.iter().find_map(|t| t.synthesized.get(name));
        let ty = match synthesized {
            Some(prop) => property_type(prop, false),
            None if name.starts_with('_') => property_type(&name[1..], true),
            None => None,
        };
        if ty.is_some() {
            return ty;
        }
    }
    None
}

/// A class's superclass: the `@interface Name : Super` of its declaration, or the platform table's.
pub fn objc_supertypes_of(name: &str, ctx: &Ctx) -> Rc<Vec<String>> {
    memoized::<ObjcSupersMemo>(ctx, name.to_string(), || {
        let mut supers: Vec<String> = Vec::new();
        for decl in ctx.get_nodes_by_name(name).iter() {
            if decl.kind != NodeKind::Class || decl.language != Language::ObjC {
                continue;
            }
            let lines = lines_of(ctx, &decl.file_path);
            let line = js_slice(&lines, decl.start_line as i64 - 1, decl.end_line as i64).iter().find(|l| l.contains("@interface")).cloned().unwrap_or_default();
            if let Some(sup) = re!(r"@interface\s+\w+\s*:\s*(\w+)").captures(&line).and_then(|c| c.text(1).map(String::from)) {
                if !supers.contains(&sup) {
                    supers.push(sup);
                }
            }
        }
        if supers.is_empty() {
            if let Some(system) = objc_system_super(name) {
                supers.push(system.to_string());
            }
        }
        Rc::new(supers)
    })
}

/// The Objective-C contribution to [`super::lang_shape::LangShape`].
pub struct ObjcShape(ObjcCallShape);

impl ObjcShape {
    pub fn of(r: &UnresolvedRef, ctx: &Ctx) -> Option<ObjcShape> {
        if r.language != Language::ObjC || !is_calls(r) || !re!(r"^[A-Za-z_]\w*:*(?:\w+:)*$").is_match(&r.reference_name) {
            return None;
        }
        objc_call_shape(r, ctx).map(ObjcShape)
    }
}

impl RefShape for ObjcShape {
    fn excludes(&self, n: &NodeRef, r: &UnresolvedRef, ctx: &Ctx) -> bool {
        match self.0 {
            ObjcCallShape::CCall => is_objc_member_kind(n.kind),
            ObjcCallShape::SelfSend => !is_objc_self_send_target(n, r, ctx, false),
            ObjcCallShape::SuperSend => !is_objc_self_send_target(n, r, ctx, true),
        }
    }
}
