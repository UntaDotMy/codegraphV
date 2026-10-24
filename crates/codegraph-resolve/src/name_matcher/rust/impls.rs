//! Rust impl headers, the std types a project can shadow, and the enclosing `impl`/`trait` of a
//! reference: what `Self` means where a reference is written.

use super::uses::*;
use crate::ctx::Ctx;
use crate::name_matcher::util::{lines_of, memo_map, memoized, after_col};
use crate::types::UnresolvedRef;
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::js_trim;
use codegraph_types::{Language, Node, NodeKind};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::LazyLock;

pub const RUST_STD_TYPES: &[&str] = &[
    "String", "str", "Vec", "VecDeque", "HashMap", "HashSet", "BTreeMap", "BTreeSet", "BinaryHeap", "LinkedList", "Box", "Rc", "Arc",
    "Weak", "Cow", "Cell", "RefCell", "Mutex", "RwLock", "Option", "Result", "PathBuf", "Path", "OsString", "OsStr", "CString", "CStr",
    "NonNull", "Pin", "Duration", "Instant", "SystemTime", "PhantomData", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16",
    "i32", "i64", "i128", "isize", "f32", "f64", "bool", "char",
];

pub fn is_rust_std_type(name: &str) -> bool {
    RUST_STD_TYPES.contains(&name)
}

pub const RUST_DECLARED_TYPE_KINDS: [NodeKind; 5] =
    [NodeKind::Struct, NodeKind::Enum, NodeKind::Union, NodeKind::TypeAlias, NodeKind::Trait];
pub const RUST_TRAIT_KIND: [NodeKind; 1] = [NodeKind::Trait];
pub const RUST_OWNER_KINDS: [NodeKind; 3] = [NodeKind::Struct, NodeKind::Enum, NodeKind::Union];

pub fn is_rust_declared_type_kind(k: NodeKind) -> bool {
    RUST_DECLARED_TYPE_KINDS.contains(&k)
}

fn name_sets(lists: &[(&str, String)]) -> HashMap<String, HashSet<String>> {
    lists.iter().map(|(t, names)| (t.to_string(), names.split(' ').map(String::from).collect())).collect()
}

const RUST_RC_INHERENT: &str = "new new_cyclic new_uninit new_uninit_slice new_zeroed pin try_new try_unwrap into_inner unwrap_or_clone \
into_raw as_ptr from_raw increment_strong_count decrement_strong_count downgrade weak_count strong_count get_mut make_mut ptr_eq \
downcast assume_init";
const RUST_PIN_METHODS: &str = "as_ref as_mut as_deref_mut set get_ref get_mut get_unchecked_mut into_ref map_unchecked map_unchecked_mut";
const RUST_POINTER_TRAIT_METHODS: &str = "clone clone_from as_ref borrow deref fmt eq ne partial_cmp lt le gt ge cmp max min clamp hash \
into try_into to_string";

static RUST_STD_INHERENT: LazyLock<HashMap<String, HashSet<String>>> = LazyLock::new(|| {
    name_sets(&[
        ("Arc", RUST_RC_INHERENT.to_string()),
        ("Rc", RUST_RC_INHERENT.to_string()),
        ("Weak", "new upgrade strong_count weak_count as_ptr into_raw from_raw ptr_eq".to_string()),
        (
            "Box",
            "new new_uninit new_uninit_slice new_zeroed pin try_new from_raw into_raw from_non_null into_non_null leak into_pin \
into_boxed_slice into_inner as_ptr as_mut_ptr downcast assume_init write"
                .to_string(),
        ),
        (
            "NonNull",
            "new new_unchecked dangling as_ptr as_ref as_mut cast add sub offset byte_add byte_sub byte_offset offset_from read \
read_unaligned read_volatile write write_bytes write_unaligned write_volatile replace swap copy_to copy_from \
copy_to_nonoverlapping copy_from_nonoverlapping drop_in_place from_ref from_mut slice_from_raw_parts len addr with_addr \
map_addr align_offset is_aligned"
                .to_string(),
        ),
        ("Pin", format!("new new_unchecked into_inner into_inner_unchecked static_ref static_mut {RUST_PIN_METHODS}")),
        ("Cell", "new set swap replace into_inner get update as_ptr get_mut from_mut take".to_string()),
        ("RefCell", "new into_inner replace replace_with swap borrow try_borrow borrow_mut try_borrow_mut as_ptr get_mut take".to_string()),
        ("Mutex", "new lock try_lock is_poisoned clear_poison into_inner get_mut".to_string()),
        ("RwLock", "new read try_read write try_write is_poisoned clear_poison into_inner get_mut".to_string()),
        (
            "Vec",
            "new with_capacity from_raw_parts capacity reserve reserve_exact try_reserve try_reserve_exact shrink_to_fit shrink_to \
into_boxed_slice truncate as_slice as_mut_slice as_ptr as_mut_ptr set_len swap_remove insert remove retain retain_mut \
dedup_by_key dedup_by dedup push pop pop_if append drain clear len is_empty split_off resize_with resize leak \
spare_capacity_mut extend_from_slice extend_from_within splice extract_if into_flattened"
                .to_string(),
        ),
        (
            "String",
            "new with_capacity from_utf8 from_utf8_lossy from_utf8_unchecked from_utf16 from_utf16_lossy from_raw_parts into_bytes \
as_str as_mut_str push_str extend_from_within capacity reserve reserve_exact try_reserve try_reserve_exact shrink_to_fit \
shrink_to push as_bytes truncate pop remove retain insert insert_str as_mut_vec len is_empty split_off clear drain \
replace_range into_boxed_str leak"
                .to_string(),
        ),
        (
            "Option",
            "is_some is_some_and is_none is_none_or as_ref as_mut as_pin_ref as_pin_mut as_slice as_mut_slice expect unwrap \
unwrap_or unwrap_or_else unwrap_or_default unwrap_unchecked map inspect map_or map_or_else ok_or ok_or_else as_deref \
as_deref_mut iter iter_mut and and_then filter or or_else xor insert get_or_insert get_or_insert_default \
get_or_insert_with take take_if replace zip unzip copied cloned transpose flatten"
                .to_string(),
        ),
        (
            "Result",
            "is_ok is_ok_and is_err is_err_and ok err as_ref as_mut map map_or map_or_else map_err inspect inspect_err as_deref \
as_deref_mut iter iter_mut expect unwrap unwrap_or_default expect_err unwrap_err into_ok into_err and and_then or or_else \
unwrap_or unwrap_or_else unwrap_unchecked unwrap_err_unchecked copied cloned transpose flatten"
                .to_string(),
        ),
    ])
});

static RUST_WRAPPER_OWN_METHODS: LazyLock<HashMap<String, HashSet<String>>> = LazyLock::new(|| {
    name_sets(&[
        ("Pin", format!("{RUST_PIN_METHODS} deref deref_mut fmt eq ne partial_cmp lt le gt ge cmp max min clamp hash into try_into to_string")),
        ("Box", format!("{RUST_POINTER_TRAIT_METHODS} as_mut borrow_mut deref_mut")),
        ("Rc", RUST_POINTER_TRAIT_METHODS.to_string()),
        ("Arc", RUST_POINTER_TRAIT_METHODS.to_string()),
        ("ManuallyDrop", format!("{RUST_POINTER_TRAIT_METHODS} deref_mut")),
        ("Cow", format!("{RUST_POINTER_TRAIT_METHODS} into_owned to_mut is_borrowed is_owned")),
        ("MutexGuard", "deref deref_mut fmt".to_string()),
        ("RwLockReadGuard", "deref fmt".to_string()),
        ("RwLockWriteGuard", "deref deref_mut fmt".to_string()),
        ("Ref", "deref fmt".to_string()),
        ("RefMut", "deref deref_mut fmt".to_string()),
    ])
});

/// Whether std's inherent methods on `owner` include `method`.
pub fn rust_std_inherent_has(owner: &str, method: &str) -> bool {
    RUST_STD_INHERENT.get(owner).is_some_and(|s| s.contains(method))
}

/// Whether the std wrapper `wrapper` has a method of its own named `method`.
pub fn rust_wrapper_has_own(wrapper: &str, method: &str) -> bool {
    RUST_WRAPPER_OWN_METHODS.get(wrapper).is_some_and(|s| s.contains(method))
}

/// `Pin`/`Box`/`Rc`/`Arc` when `method` takes `self: Pin<&mut Self>` (and the like).
pub fn rust_self_wrapper(method: &Node) -> Option<String> {
    re!(r"^\(\s*(?:mut\s+)?self\s*:\s*(?:&\s*(?:'\w+\s+)?(?:mut\s+)?)?(?:(?:std|core|alloc)\s*::\s*\w+\s*::\s*)?(Pin|Box|Rc|Arc)\s*<")
        .captures(method.signature.as_deref().unwrap_or(""))
        .and_then(|c| c.text(1).map(String::from))
}

// -- Impl headers --------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct RustImplHeader {
    /// The trait implemented, `""` for an inherent impl (never produced here).
    pub trait_: String,
    pub trait_args: Vec<String>,
    pub self_: String,
    pub self_args: Vec<String>,
    pub self_text: String,
    pub params: Vec<String>,
}

fn head_of(part: &str) -> String {
    let stripped = re!(r"^(?:&\s*(?:'\w+\s+)?(?:mut\s+)?|dyn\s+)+").replace_first(part, |_| String::new());
    re!(r"([A-Za-z_]\w*)\s*(?:<|$)")
        .captures(js_trim(&stripped))
        .and_then(|c| c.text(1).map(String::from))
        .unwrap_or_default()
}

fn args_of(part: &str) -> Vec<String> {
    let Some(open) = part.find('<') else { return Vec::new() };
    re!(r"(?<!['\w])([A-Za-z_]\w*)", "g")
        .captures_iter(&part[open + 1..])
        .map(|m| m.text(1).unwrap_or("").to_string())
        .filter(|id| id != "mut" && id != "dyn")
        .collect()
}

/// `impl<…> Trait<…> for Type<…>`: `None` for an inherent impl.
pub fn parse_rust_impl_header(text: &str) -> Option<RustImplHeader> {
    let first = re!(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?impl\b").replace_first(text, |_| String::new());
    let second = re!(r"\{[\s\S]*$").replace_first(&first, |_| String::new());
    let third = re!(r"\bwhere\b[\s\S]*$").replace_first(&second, |_| String::new());
    let mut rest = js_trim(&third).to_string();
    let mut params: Vec<String> = Vec::new();
    if rest.starts_with('<') {
        let bytes = rest.as_bytes();
        let mut depth = 0i32;
        let mut i = 0usize;
        let mut from = 1usize;
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'<' {
                depth += 1;
            } else if c == b'>' {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            } else if c == b',' && depth == 1 {
                params.push(rest[from..i].to_string());
                from = i + 1;
            }
            i += 1;
        }
        params.push(rest[from.min(i)..i].to_string());
        rest = js_trim(rest.get(i + 1..).unwrap_or("")).to_string();
    }
    let mut depth = 0i32;
    let mut split: Option<(usize, usize)> = None;
    let mut selfs_at = 0usize;
    for (i, c) in rest.char_indices() {
        if split.is_some() {
            break;
        }
        if c == '<' {
            depth += 1;
        } else if c == '>' {
            depth -= 1;
        } else if depth == 0 && codegraph_types::jscompat::is_js_space(c) {
            if let Some(m) = re!(r"^for\s").find(&rest[i + c.len_utf8()..]) {
                split = Some((i, c.len_utf8()));
                selfs_at = i + c.len_utf8() + m.end();
            }
        }
    }
    let (split, _) = split?;
    let trait_part_raw = js_trim(&rest[..split]).to_string();
    let trait_part = trait_part_raw.strip_prefix('!').unwrap_or(&trait_part_raw).to_string();
    let open = trait_part.find('<');
    let self_part = rest[selfs_at..].to_string();
    let self_trimmed = re!(r"^(?:&\s*(?:'\w+\s+)?(?:mut\s+)?|dyn\s+)+").replace_first(js_trim(&self_part), |_| String::new());
    let self_text = {
        let a = re!(r"\{[\s\S]*$").replace_first(&self_part, |_| String::new());
        let b = re!(r"\bwhere\b[\s\S]*$").replace_first(&a, |_| String::new());
        js_trim(&b).to_string()
    };
    Some(RustImplHeader {
        trait_: head_of(match open {
            None => &trait_part,
            Some(o) => &trait_part[..o],
        }),
        trait_args: args_of(&trait_part),
        self_: head_of(&self_part),
        self_args: args_of(&self_trimmed),
        self_text,
        params: params
            .iter()
            .filter_map(|p| re!(r"^\s*(?:const\s+)?([A-Za-z_]\w*)").captures(p).and_then(|c| c.text(1).map(String::from)))
            .collect(),
    })
}

memo_map!(ImplHeaderMemo, String, Option<Rc<RustImplHeader>>);

fn without_line_comment(line: &str) -> String {
    re!(r"//.*$").replace_first(line, |_| String::new())
}

/// The trait-impl header the method at `method` sits in (`None` for an inherent impl).
pub fn rust_trait_impl_header(method: &Node, ctx: &Ctx) -> Option<Rc<RustImplHeader>> {
    memoized::<ImplHeaderMemo>(ctx, method.id.clone(), || {
        let lines = lines_of(ctx, &method.file_path);
        let mut i = method.start_line as i64 - 2;
        while i >= 0 {
            let line = lines.get(i as usize).map(String::as_str).unwrap_or("");
            if re!(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?impl\b").is_match(line) {
                let mut text = without_line_comment(line);
                let mut j = i as usize + 1;
                while !text.contains('{') && j < lines.len().min(i as usize + 8) {
                    text.push(' ');
                    text.push_str(&without_line_comment(lines.get(j).map(String::as_str).unwrap_or("")));
                    j += 1;
                }
                return parse_rust_impl_header(&text).map(Rc::new);
            }
            if re!(r"^(?:pub(?:\([^)]*\))?\s+)?(?:fn|struct|enum|mod|trait|const|static|type)\b").is_match(line) {
                break;
            }
            i -= 1;
        }
        None
    })
}

memo_map!(TraitImplMethodMemo, String, bool);

/// Whether `candidate` is a method of an `impl Trait for Type` block.
pub fn is_rust_trait_impl_method(candidate: &Node, ctx: &Ctx) -> bool {
    if candidate.kind != NodeKind::Method {
        return false;
    }
    memoized::<TraitImplMethodMemo>(ctx, candidate.id.clone(), || {
        let lines = lines_of(ctx, &candidate.file_path);
        let mut i = candidate.start_line as i64 - 2;
        let mut is_trait = false;
        while i >= 0 {
            let line = lines.get(i as usize).map(String::as_str).unwrap_or("");
            if re!(r"^\s*(pub(\([^)]*\))?\s+)?(unsafe\s+)?impl\b").is_match(line) {
                is_trait = re!(r"\sfor\s").is_match(&without_line_comment(line));
                break;
            }
            if re!(r"^(pub(\([^)]*\))?\s+)?(fn|struct|enum|mod|trait|const|static|type)\b").is_match(line) {
                break;
            }
            i -= 1;
        }
        is_trait
    })
}

/// Whether the project declares an item of one of `kinds` called `name` in Rust.
pub fn is_rust_project_named_in(name: &str, kinds: &[NodeKind], ctx: &Ctx) -> bool {
    ctx.get_nodes_by_name(name).iter().any(|n| n.language == Language::Rust && kinds.contains(&n.kind))
}

/// `isRustProjectNamed(name, RUST_DECLARED_TYPE_KINDS, ctx)`.
pub fn is_rust_project_named(name: &str, ctx: &Ctx) -> bool {
    is_rust_project_named_in(name, &RUST_DECLARED_TYPE_KINDS, ctx)
}

fn is_rust_project_trait_named(name: &str, ctx: &Ctx) -> bool {
    is_rust_project_named_in(name, &RUST_TRAIT_KIND, ctx)
}

/// Whether the first argument of the call at `r` is a `type` (or a local of it, or `self` of its impl).
pub fn rust_first_arg_is(type_: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let lines = lines_of(ctx, &r.file_path);
    let name = &r.reference_name;
    let cut = name.rfind(':').into_iter().chain(name.rfind('.')).max().map(|i| i + 1).unwrap_or(0);
    let method = &name[cut..];
    let mut parts: Vec<&str> = vec![after_col(lines.get((r.line as usize).wrapping_sub(1)).map(String::as_str).unwrap_or(""), r.column)];
    for l in lines.iter().skip(r.line as usize).take(4) {
        parts.push(l);
    }
    let text = parts.join(" ");
    let Some(open) = Re::cached(&format!(r"\b{}\s*\(", escape_regex(method)), "").find(&text) else { return true };
    let start = open.end();
    let mut depth = 0i32;
    let mut end = start;
    for (off, ch) in text[start..].char_indices() {
        end = start + off;
        if "([{".contains(ch) {
            depth += 1;
        } else if ")]}".contains(ch) {
            if depth == 0 {
                break;
            }
            depth -= 1;
        } else if ch == ',' && depth == 0 {
            break;
        }
        end = start + off + ch.len_utf8();
    }
    let arg = {
        let a = js_trim(&text[start..end]).to_string();
        let a = re!(r"^(?:&\s*(?:mut\s+)?|\*)+").replace_first(&a, |_| String::new());
        re!(r"\.clone\(\)$").replace_first(&a, |_| String::new())
    };
    if Re::cached(&format!(r"^(?:[A-Za-z_]\w*::)*{}\b", escape_regex(type_)), "").is_match(&arg) {
        return true;
    }
    if !re!(r"^[A-Za-z_]\w*$").is_match(&arg) {
        return false;
    }
    let declared =
        Re::cached(&format!(r"\b{arg}\s*:\s*&?\s*(?:'\w+\s+)?(?:mut\s+)?(?:[A-Za-z_]\w*::)*{}\b", escape_regex(type_)), "");
    let built = Re::cached(&format!(r"\blet\s+(?:mut\s+)?{arg}\s*=\s*&?\s*(?:[A-Za-z_]\w*::)*{}\b", escape_regex(type_)), "");
    let mut i = r.line as i64 - 1;
    while i >= 0 {
        let line = lines.get(i as usize).map(String::as_str).unwrap_or("");
        if arg == "self" && re!(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?impl\b").is_match(line) {
            let impl_ = parse_rust_impl_header(&format!("{line} {{"));
            let found = match impl_ {
                Some(h) if !h.self_.is_empty() => Some(h.self_),
                _ => re!(r"impl\b(?:\s*<[^>]*>)?\s*(?:[A-Za-z_]\w*::)*([A-Za-z_]\w*)").captures(line).and_then(|c| c.text(1).map(String::from)),
            };
            return found.as_deref() == Some(type_);
        }
        if arg != "self" && (declared.is_match(line) || built.is_match(line)) {
            return true;
        }
        if arg != "self"
            && re!(r#"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+"[^"]*"\s+)?fn\s"#).is_match(line)
        {
            break;
        }
        i -= 1;
    }
    false
}

/// Whether the trait `trait_` is in scope where `r` is written (`target` is a method of it).
pub fn is_rust_trait_in_scope(trait_: &str, r: &UnresolvedRef, target: &Node, ctx: &Ctx) -> bool {
    if r.file_path == target.file_path {
        return true;
    }
    let traits: Vec<Rc<Node>> =
        ctx.get_nodes_by_name(trait_).iter().filter(|n| n.kind == NodeKind::Trait && n.language == Language::Rust).cloned().collect();
    if traits.iter().any(|t| t.file_path == r.file_path) {
        return true;
    }
    let uses = rust_uses_of(&r.file_path, ctx);
    if uses.names.contains(trait_) {
        return true;
    }
    let crates = rust_project_crates(ctx);
    if uses.globs.iter().any(|g| g == "prelude" || g == "crate" || crates.contains(g)) {
        return true;
    }
    traits.iter().any(|t| rust_glob_covers(&uses, t, r))
}

/// `false` when `target` is a project method that implements a project trait for a std type the
/// call site does not name (so the call is on std, not on the project's impl).
pub fn is_rust_foreign_impl_callable(target: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if target.language != Language::Rust || target.kind != NodeKind::Method {
        return true;
    }
    let owner = match target.qualified_name.rfind("::") {
        None => "",
        Some(cut) => target.qualified_name[..cut].rsplit("::").next().unwrap_or(""),
    };
    if !is_rust_std_type(owner) || is_rust_project_named(owner, ctx) {
        return true;
    }
    let Some(impl_) = rust_trait_impl_header(target, ctx) else { return true };
    if impl_.self_ != owner || impl_.trait_.is_empty() {
        return true;
    }
    if rust_std_inherent_has(owner, &target.name) {
        return false;
    }
    if is_rust_project_trait_named(&impl_.trait_, ctx) {
        return is_rust_trait_in_scope(&impl_.trait_, r, target, ctx);
    }
    match impl_.trait_args.iter().find(|t| is_rust_project_named(t, ctx)) {
        None => true,
        Some(local) => rust_first_arg_is(local, r, ctx),
    }
}

// -- The enclosing impl --------------------------------------------------------------------

/// 1-based line of the `impl`/`trait` header enclosing `line` (0 when none).
pub fn rust_impl_line_at(file_path: &str, line: u32, ctx: &Ctx) -> u32 {
    let lines = lines_of(ctx, file_path);
    let mut i = line as i64 - 1;
    while i >= 0 {
        if let Some(l) = lines.get(i as usize) {
            if re!(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?(?:impl|trait)\b").is_match(l) {
                return i as u32 + 1;
            }
        }
        i -= 1;
    }
    0
}

pub fn rust_enclosing_impl_line(r: &UnresolvedRef, ctx: &Ctx) -> u32 {
    rust_impl_line_at(&r.file_path, r.line, ctx)
}

#[derive(Debug, Clone)]
pub struct RustSelfType {
    pub type_: String,
    pub root: Option<String>,
    pub trait_: Option<String>,
    pub is_trait: bool,
}

/// What `Self` is at `r`: the type of the enclosing impl (or the trait of the enclosing trait).
pub fn rust_self_type(r: &UnresolvedRef, ctx: &Ctx) -> Option<RustSelfType> {
    let header = rust_enclosing_impl_line(r, ctx);
    if header == 0 {
        return None;
    }
    let lines = lines_of(ctx, &r.file_path);
    let mut text = without_line_comment(lines.get(header as usize - 1).map(String::as_str).unwrap_or(""));
    let mut j = header as usize;
    while !text.contains('{') && j < lines.len().min(header as usize + 8) {
        text.push(' ');
        text.push_str(&without_line_comment(lines.get(j).map(String::as_str).unwrap_or("")));
        j += 1;
    }
    if let Some(trait_) =
        re!(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?trait\s+([A-Za-z_]\w*)").captures(&text).and_then(|c| c.text(1).map(String::from))
    {
        return Some(RustSelfType { type_: trait_, root: None, trait_: None, is_trait: true });
    }
    let impl_ = parse_rust_impl_header(&text);
    let captured = if impl_.is_some() {
        re!(r"\sfor\s+(?:&\s*(?:'\w+\s+)?(?:mut\s+)?)?((?:[A-Za-z_]\w*\s*::\s*)*[A-Za-z_]\w*)").captures(&text)
    } else {
        re!(r"\bimpl\b\s*(?:<(?:[^<>]|<[^<>]*>)*>)?\s*((?:[A-Za-z_]\w*\s*::\s*)*[A-Za-z_]\w*)").captures(&text)
    };
    let written: Vec<String> = captured
        .and_then(|c| c.text(1).map(String::from))
        .map(|s| s.split("::").map(|p| js_trim(p).to_string()).collect())
        .unwrap_or_default();
    let type_ = match &impl_ {
        Some(h) => h.self_.clone(),
        None => written.last().cloned().unwrap_or_default(),
    };
    if type_.is_empty() {
        return None;
    }
    Some(RustSelfType {
        type_,
        root: if written.len() > 1 { Some(written[0].clone()) } else { None },
        trait_: impl_.as_ref().map(|h| h.trait_.clone()).filter(|t| !t.is_empty()),
        is_trait: false,
    })
}

/// Whether `Self` names a type outside the project (std, a dependency, or an undeclared std type).
pub fn is_rust_outside_self(self_: &RustSelfType, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if let Some(root) = &self_.root {
        return root == "std" || root == "core" || root == "alloc" || rust_dependency_crates(ctx).contains(root);
    }
    let uses = rust_uses_of(&r.file_path, ctx);
    if uses.external.contains(&self_.type_) && !uses.bound.contains(&self_.type_) {
        return true;
    }
    is_rust_std_type(&self_.type_) && !is_rust_project_named(&self_.type_, ctx)
}

/// The `Self` of a trait method's declared owner: the last `::` segment of the qualified owner.
pub fn rust_owner_of(qualified_name: &str) -> &str {
    match qualified_name.rfind("::") {
        Some(cut) if cut > 0 => qualified_name[..cut].rsplit("::").next().unwrap_or(""),
        _ => "",
    }
}

/// Whether the file's generic parameter list (from the enclosing impl line to the reference) names `name`.
pub fn is_rust_generic_param(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if !re!(r"^[A-Z]\w*$").is_match(name) {
        return false;
    }
    let lines = lines_of(ctx, &r.file_path);
    let from = (rust_enclosing_impl_line(r, ctx) as usize).saturating_sub(1);
    let to = (r.line as usize).min(lines.len());
    let text = if from < to { lines[from..to].join("\n") } else { String::new() };
    Re::cached(&format!(r"<[^<>]*(?:<[^<>]*>[^<>]*)*\b{name}\b\s*(?:[:,>=])"), "").is_match(&text)
}

/// Whether `name` is a type parameter of the enclosing fn or impl.
pub fn is_rust_type_param(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let lines = lines_of(ctx, &r.file_path);
    let header_at = |line: u32| -> String {
        let mut text = String::new();
        let mut j = line as i64 - 1;
        while j >= 0 && (j as usize) < lines.len().min(line as usize + 7) && !re!(r"[{;]").is_match(&text) {
            text.push(' ');
            text.push_str(&without_line_comment(lines.get(j as usize).map(String::as_str).unwrap_or("")));
            j += 1;
        }
        text
    };
    let scope_start = crate::name_matcher::js_store::enclosing_scope_start_line(r, ctx);
    [scope_start, rust_enclosing_impl_line(r, ctx)].into_iter().any(|line| {
        if line == 0 {
            return false;
        }
        let header = header_at(line);
        let Some(open) = re!(r"\b(?:fn\s+[A-Za-z_]\w*|impl)\s*<").find(&header) else { return false };
        let mut depth = 0i32;
        let mut from = open.end();
        let bytes = header.as_bytes();
        let param = |from: usize, i: usize| -> bool {
            Re::cached(&format!(r"^\s*(?:const\s+)?{}\b", escape_regex(name)), "").is_match(&header[from.min(i)..i])
        };
        let mut i = from - 1;
        while i < bytes.len() {
            let ch = bytes[i];
            if ch == b'<' {
                depth += 1;
            } else if ch == b'>' && bytes.get(i.wrapping_sub(1)) != Some(&b'-') {
                depth -= 1;
                if depth == 0 {
                    return param(from, i);
                }
            } else if ch == b',' && depth == 1 {
                if param(from, i) {
                    return true;
                }
                from = i + 1;
            }
            i += 1;
        }
        false
    })
}
