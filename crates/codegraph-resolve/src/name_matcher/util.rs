//! Helpers every name-matcher region shares: line/column access in JavaScript string units,
//! language-family predicates, the context memo pattern, path proximity and candidate scoring.

use crate::ctx::Ctx;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_types::jscompat::{utf16_byte_index, utf16_from, utf16_prefix};
use codegraph_types::{EdgeKind, Language, Node, NodeKind, ReferenceKind};
use std::rc::Rc;

/// A memo keyed by `K` that lives on the resolution context. Each memo is its own type so
/// `Ctx::memo` can tell them apart; `memo_map!` declares one.
pub trait MemoMap: Default + 'static {
    type K: Clone;
    type V: Clone;
    fn lookup(&self, key: &Self::K) -> Option<Self::V>;
    fn store(&self, key: Self::K, value: Self::V);
}

macro_rules! memo_map {
    ($(#[$meta:meta])* $name:ident, $k:ty, $v:ty) => {
        $(#[$meta])*
        #[derive(Default)]
        pub struct $name(std::cell::RefCell<std::collections::HashMap<$k, $v>>);
        impl $crate::name_matcher::util::MemoMap for $name {
            type K = $k;
            type V = $v;
            fn lookup(&self, key: &$k) -> Option<$v> {
                self.0.borrow().get(key).cloned()
            }
            fn store(&self, key: $k, value: $v) {
                self.0.borrow_mut().insert(key, value);
            }
        }
    };
}
pub(crate) use memo_map;

/// Look `key` up in the context's `M` memo, computing and storing it on a miss.
pub fn memoized<M: MemoMap>(ctx: &Ctx, key: M::K, compute: impl FnOnce() -> M::V) -> M::V {
    let memo = ctx.memo::<M>();
    if let Some(hit) = memo.lookup(&key) {
        return hit;
    }
    let value = compute();
    memo.store(key, value.clone());
    value
}

// -- Lines and columns ---------------------------------------------------------------------

/// `context.getFileLines?.(file)?.[line - 1]` (1-based line).
pub fn line_at(ctx: &Ctx, file: &str, line: u32) -> Option<String> {
    let idx = (line as usize).checked_sub(1)?;
    ctx.get_file_lines(file).and_then(|l| l.get(idx).cloned())
}

/// The file's lines, or none (`getFileLines?.(f) ?? readFile(f)?.split(...) ?? []`).
pub fn lines_of(ctx: &Ctx, file: &str) -> Rc<Vec<String>> {
    ctx.get_file_lines(file).unwrap_or_default()
}

/// `line.slice(col)` for a UTF-16 column.
pub fn after_col(line: &str, col: u32) -> &str {
    utf16_from(line, col as usize)
}

/// `line.slice(0, col)` for a UTF-16 column.
pub fn before_col(line: &str, col: u32) -> &str {
    utf16_prefix(line, col as usize)
}

/// `line.startsWith(needle, col)`.
pub fn starts_with_at(line: &str, needle: &str, col: u32) -> bool {
    after_col(line, col).starts_with(needle)
}

/// The byte offset of UTF-16 column `col`, clamped to the line.
pub fn col_byte(line: &str, col: u32) -> usize {
    utf16_byte_index(line, col as usize)
}

/// `s.slice(from, to)` for byte offsets: clamped to the string and rounded down to character
/// boundaries (the offsets come from scans of ASCII punctuation, so this only matters for a
/// fixed-width window that lands inside an identifier or string).
pub fn slice_bytes(s: &str, from: usize, to: usize) -> &str {
    let floor = |mut i: usize| {
        i = i.min(s.len());
        while !s.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    let (a, b) = (floor(from), floor(to));
    if a >= b {
        ""
    } else {
        &s[a..b]
    }
}

/// `line[col]` as a char, for a UTF-16 column.
/// `items.slice(from, to)`: indexes clamp to the array, and negative ones count from its end.
pub fn js_slice<T>(items: &[T], from: i64, to: i64) -> &[T] {
    let len = items.len() as i64;
    let resolve = |i: i64| if i < 0 { (len + i).max(0) } else { i.min(len) };
    let (a, b) = (resolve(from), resolve(to));
    if a >= b {
        &items[0..0]
    } else {
        &items[a as usize..b as usize]
    }
}

pub fn char_at_col(line: &str, col: u32) -> Option<char> {
    after_col(line, col).chars().next()
}

/// The char just before byte offset `at`.
pub fn char_before(s: &str, at: usize) -> Option<char> {
    s[..at.min(s.len())].chars().next_back()
}

// -- Language families --------------------------------------------------------------------

/// `JS_FAMILY` in name-matcher.ts.
pub fn is_js_family(l: Language) -> bool {
    matches!(
        l,
        Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx | Language::Vue | Language::Svelte | Language::Astro
    )
}

/// `JS_TS`.
pub fn is_js_ts(l: Language) -> bool {
    matches!(l, Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx)
}

/// `ESM_FAMILY`.
pub fn is_esm_family(l: Language) -> bool {
    matches!(l, Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx | Language::ArkTs)
}

/// `ES_IMPORT_LANGUAGES`.
pub fn is_es_import_language(l: Language) -> bool {
    is_esm_family(l) || matches!(l, Language::Vue | Language::Svelte | Language::Astro)
}

/// `OBJECT_LITERAL_LANGUAGES`.
pub fn is_object_literal_language(l: Language) -> bool {
    is_esm_family(l) || matches!(l, Language::Vue | Language::Svelte | Language::Astro)
}

/// `TYPE_MEMBER_KINDS`.
pub fn is_type_member_kind(k: NodeKind) -> bool {
    matches!(k, NodeKind::Method | NodeKind::Property | NodeKind::Field | NodeKind::EnumMember)
}

pub fn is_callable_kind(k: NodeKind) -> bool {
    matches!(k, NodeKind::Function | NodeKind::Method)
}

pub fn is_c_family(l: Language) -> bool {
    matches!(l, Language::C | Language::Cpp)
}

pub fn is_lua(l: Language) -> bool {
    matches!(l, Language::Lua | Language::Luau)
}

pub fn is_cfml(l: Language) -> bool {
    matches!(l, Language::Cfml | Language::CfScript)
}

pub fn is_calls(r: &UnresolvedRef) -> bool {
    r.reference_kind == ReferenceKind::Edge(EdgeKind::Calls)
}

pub fn is_kind(r: &UnresolvedRef, kind: EdgeKind) -> bool {
    r.reference_kind == ReferenceKind::Edge(kind)
}

/// `ref.referenceKind === 'references' || 'function_ref'`.
pub fn is_value_ref(r: &UnresolvedRef) -> bool {
    r.reference_kind == ReferenceKind::Edge(EdgeKind::References) || r.reference_kind == ReferenceKind::FunctionRef
}

// -- Ranges -------------------------------------------------------------------------------

pub fn range_within(inner: &Node, outer: &Node) -> bool {
    let (ie, oe) = (inner.end_line, outer.end_line);
    if inner.start_line < outer.start_line || ie > oe {
        return false;
    }
    if inner.start_line == outer.start_line && inner.start_column < outer.start_column {
        return false;
    }
    if ie == oe && inner.end_column > outer.end_column {
        return false;
    }
    true
}

pub fn same_range(a: &Node, b: &Node) -> bool {
    a.start_line == b.start_line && a.start_column == b.start_column && a.end_line == b.end_line && a.end_column == b.end_column
}

/// The last `::` segment of a qualified name.
pub fn last_qualified_segment(qualified_name: &str) -> &str {
    match qualified_name.rfind("::") {
        Some(cut) => &qualified_name[cut + 2..],
        None => qualified_name,
    }
}

/// The part before the last `::`, or `""`.
pub fn qualified_owner(qualified_name: &str) -> &str {
    match qualified_name.rfind("::") {
        Some(cut) => &qualified_name[..cut],
        None => "",
    }
}

// -- Scoring ------------------------------------------------------------------------------

fn path_proximity_from_dirs(dirs: &[&str], file_path2: &str) -> i64 {
    let mut dir2: Vec<&str> = file_path2.split('/').collect();
    dir2.pop();
    let limit = dirs.len().min(dir2.len());
    let mut shared = 0i64;
    for i in 0..limit {
        if dirs[i] == dir2[i] {
            shared += 1;
        } else {
            break;
        }
    }
    (shared * 15).min(80)
}

/// How near two files are in the directory tree (shared leading directories, capped).
pub fn compute_path_proximity(file_path1: &str, file_path2: &str) -> i64 {
    let mut dir1: Vec<&str> = file_path1.split('/').collect();
    dir1.pop();
    path_proximity_from_dirs(&dir1, file_path2)
}

/// The best-scoring candidate for `r`: same file, near directories, same language, a kind that
/// suits the reference, exported, near the line.
pub fn find_best_match(r: &UnresolvedRef, candidates: &[NodeRef], ctx: &Ctx) -> Option<NodeRef> {
    let mut best_score = -1.0f64;
    let mut best: Option<NodeRef> = None;
    let mut tied: Vec<NodeRef> = Vec::new();
    let mut ref_dirs: Vec<&str> = r.file_path.split('/').collect();
    ref_dirs.pop();
    let has_same_language = candidates.iter().any(|c| c.language == r.language);
    for candidate in candidates {
        if has_same_language && candidate.language != r.language {
            continue;
        }
        let mut score = 0.0f64;
        if candidate.file_path == r.file_path {
            score += 100.0;
        }
        score += path_proximity_from_dirs(&ref_dirs, &candidate.file_path) as f64;
        if r.language == Language::VbNet
            && candidate.language == Language::VbNet
            && crate::vbnet_receivers::same_vb_project(&candidate.file_path, &r.file_path, ctx)
        {
            score += 80.0;
        }
        if candidate.language == r.language {
            score += 50.0;
        } else {
            score -= 80.0;
        }
        if is_calls(r) && matches!(candidate.kind, NodeKind::Function | NodeKind::Method) {
            score += 25.0;
        }
        if is_kind(r, EdgeKind::Instantiates)
            && matches!(candidate.kind, NodeKind::Class | NodeKind::Struct | NodeKind::Union | NodeKind::Interface)
        {
            score += 25.0;
        }
        if is_kind(r, EdgeKind::Decorates) {
            if matches!(candidate.kind, NodeKind::Function | NodeKind::Method) {
                score += 25.0;
            } else if matches!(candidate.kind, NodeKind::Class | NodeKind::Interface) {
                score += 15.0;
            }
        }
        if candidate.is_exported == Some(true) {
            score += 10.0;
        }
        if candidate.file_path == r.file_path && candidate.start_line != 0 {
            let distance = (candidate.start_line as f64 - r.line as f64).abs();
            score += (20.0 - distance / 10.0).max(0.0);
        }
        if score > best_score {
            best_score = score;
            best = Some(candidate.clone());
            tied = vec![candidate.clone()];
        } else if score == best_score {
            tied.push(candidate.clone());
        }
    }
    // Proximity is capped, so on a deep tree two copies of one symbol in sibling modules can tie;
    // the caller's own copy, if uniquely closest, wins.
    if tied.len() > 1 {
        if let Some(copy) = closest_copy(r, &tied) {
            return Some(copy);
        }
    }
    best
}

/// Leading directory segments two file paths share, uncapped.
fn shared_dir_depth(file_path1: &str, file_path2: &str) -> usize {
    let mut a: Vec<&str> = file_path1.split('/').collect();
    let mut b: Vec<&str> = file_path2.split('/').collect();
    a.pop();
    b.pop();
    a.iter().zip(&b).take_while(|(x, y)| x == y).count()
}

fn file_basename(file_path: &str) -> &str {
    file_path.rsplit('/').next().unwrap_or(file_path)
}

/// When every candidate is a copy of one symbol — same qualified name, kind and file basename, as
/// when a module is duplicated under another source root (`rc2/flymod/src/…` vs
/// `rcpro2/flymod/src/…`) — the copy whose file shares the longest directory prefix with the
/// reference, i.e. the one in the caller's own module. `None` when the candidates are not all
/// copies or no copy is uniquely closest, so callers keep their existing choice.
pub fn closest_copy(r: &UnresolvedRef, candidates: &[NodeRef]) -> Option<NodeRef> {
    let first = candidates.first().filter(|_| candidates.len() >= 2)?;
    let base = file_basename(&first.file_path);
    let mut best: Option<&NodeRef> = None;
    let mut best_depth = None;
    let mut unique = false;
    for c in candidates {
        if c.qualified_name != first.qualified_name || c.kind != first.kind || file_basename(&c.file_path) != base {
            return None;
        }
        let depth = shared_dir_depth(&r.file_path, &c.file_path);
        match best_depth {
            Some(d) if depth < d => {}
            Some(d) if depth == d => unique = false,
            _ => {
                best_depth = Some(depth);
                best = Some(c);
                unique = true;
            }
        }
    }
    best.filter(|_| unique).cloned()
}

/// `/^[A-Za-z_$][\w$]*$/` — a plain identifier (JS flavor).
pub fn is_js_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$'),
        _ => false,
    }
}

/// `/^[A-Za-z_]\w*$/`.
pub fn is_plain_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => chars.all(|c| c.is_ascii_alphanumeric() || c == '_'),
        _ => false,
    }
}

/// `/^\w+$/`.
pub fn is_word_str(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_follow_the_javascript_patterns() {
        assert!(is_js_ident("$el"));
        assert!(is_js_ident("a1"));
        assert!(!is_js_ident("1a"));
        assert!(!is_js_ident("a-b"));
        assert!(is_plain_ident("_x"));
        assert!(!is_plain_ident("$x"));
        assert!(is_word_str("a_1"));
        assert!(!is_word_str(""));
    }

    #[test]
    fn path_proximity_counts_shared_directories_up_to_a_cap() {
        assert_eq!(compute_path_proximity("a/b/c.ts", "a/b/d.ts"), 30);
        assert_eq!(compute_path_proximity("a/b/c.ts", "a/x/d.ts"), 15);
        assert_eq!(compute_path_proximity("a/b/c.ts", "z/d.ts"), 0);
        assert_eq!(compute_path_proximity("a/b/c/d/e/f/g.ts", "a/b/c/d/e/f/h.ts"), 80);
    }

    #[test]
    fn qualified_names_split_on_the_last_separator() {
        assert_eq!(last_qualified_segment("a::b::c"), "c");
        assert_eq!(qualified_owner("a::b::c"), "a::b");
        assert_eq!(qualified_owner("c"), "");
    }
}

// -- Bounded per-context caches ---------------------------------------------------------------

/// A small insertion-ordered cache of the last `N` entries, for per-file scans that are large
/// and only revisited while a file's references are being resolved. The oldest entry goes first.
pub struct Bounded<V, const N: usize>(std::cell::RefCell<std::collections::VecDeque<(String, V)>>);

impl<V, const N: usize> Default for Bounded<V, N> {
    fn default() -> Self {
        Bounded(std::cell::RefCell::new(std::collections::VecDeque::new()))
    }
}

impl<V: Clone + 'static, const N: usize> Bounded<V, N> {
    /// The cached entry for `key`, building it on a miss. `touch` moves a hit to the back (the
    /// most recently read goes last).
    pub fn get_or(&self, key: &str, touch: bool, make: impl FnOnce() -> V) -> V {
        let hit = {
            let mut q = self.0.borrow_mut();
            match q.iter().position(|(k, _)| k == key) {
                Some(i) if touch => {
                    let entry = q.remove(i).unwrap();
                    let v = entry.1.clone();
                    q.push_back(entry);
                    Some(v)
                }
                Some(i) => Some(q[i].1.clone()),
                None => None,
            }
        };
        if let Some(v) = hit {
            return v;
        }
        let v = make();
        let mut q = self.0.borrow_mut();
        if q.len() >= N {
            q.pop_front();
        }
        q.push_back((key.to_string(), v.clone()));
        v
    }
}

// -- Locale ordering ----------------------------------------------------------------------------

/// `a.localeCompare(b)`: the root collation order, as Node's ICU gives it.
pub fn locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    use icu_collator::{options::CollatorOptions, Collator, CollatorBorrowed};
    use std::sync::LazyLock;
    static COLLATOR: LazyLock<CollatorBorrowed<'static>> =
        LazyLock::new(|| Collator::try_new(Default::default(), CollatorOptions::default()).expect("compiled root collation data"));
    COLLATOR.compare(a, b)
}

/// `a.localeCompare(b, "en", { numeric: true })`: digit runs compare by value.
pub fn locale_compare_numeric(a: &str, b: &str) -> std::cmp::Ordering {
    use icu_collator::{options::CollatorOptions, preferences::CollationNumericOrdering, Collator, CollatorBorrowed, CollatorPreferences};
    use std::sync::LazyLock;
    static COLLATOR: LazyLock<CollatorBorrowed<'static>> = LazyLock::new(|| {
        let mut prefs = CollatorPreferences::default();
        prefs.numeric_ordering = Some(CollationNumericOrdering::True);
        Collator::try_new(prefs, CollatorOptions::default()).expect("compiled root collation data")
    });
    COLLATOR.compare(a, b)
}
