//! A call that means a member of the class it is written in — a receiver-less `render()` in Java /
//! Scala, or `this.render()` / `self.render()` in TypeScript / JavaScript / Python — binds by the
//! language's scoping, not by which same-named method sits nearest (`matchEnclosingScopeMember`,
//! upstream 25776014).

use super::same_language_family;
use super::util::{is_calls, line_at};
use crate::ctx::Ctx;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::{Language, NodeKind};
use std::collections::{HashMap, HashSet};

/// Languages where a receiver-less call inside a class body means `this.m()`, and whose scoping
/// filters in `match_by_exact_name` can still leave the caller's own member and a sibling class's
/// side by side (Scala keeps every same-file member; Java keeps every type a same-named nested
/// type extends). Kotlin, C#, Swift, Dart and Ruby filter a bare call to what is in reach, and C++
/// has its own class lookup.
fn is_implicit_this_language(l: Language) -> bool {
    matches!(l, Language::Java | Language::Scala)
}

/// Languages whose extractor drops a `this.`/`self.` receiver but where a bare call never means a
/// member.
fn is_explicit_this_language(l: Language) -> bool {
    matches!(l, Language::Python | Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx)
}

fn is_member_owner_kind(k: NodeKind) -> bool {
    matches!(k, NodeKind::Class | NodeKind::Struct | NodeKind::Interface | NodeKind::Trait | NodeKind::Protocol | NodeKind::Enum | NodeKind::Union | NodeKind::Module)
}

fn is_callable(n: &NodeRef) -> bool {
    matches!(n.kind, NodeKind::Method | NodeKind::Function)
}

#[derive(Clone, Copy, PartialEq)]
enum Receiver {
    Bare,
    SelfRef,
    Super,
}

/// How a call names its receiver, read back from the call site's line: the extractor emits
/// `render()`, `this.render()`, `self.render()` and `super.render()` all as `render`. `None` when
/// the line can't be read or the receiver is something else.
fn call_receiver_kind(r: &UnresolvedRef, ctx: &Ctx) -> Option<Receiver> {
    let line = line_at(ctx, &r.file_path, r.line)?;
    // The ref's column is the call expression's start, except in Dart, where it is the argument
    // list right after the name: take the first occurrence at or after the column, else the last
    // one before it.
    let name = Re::cached(&format!(r"(?<![\w$]){}(?![\w$])", escape_regex(&r.reference_name)), "");
    let mut at = None;
    for m in name.captures_iter(&line) {
        let m = m.get(0).unwrap();
        at = Some((m.start(), m.utf16_start()));
        if m.utf16_start() >= r.column as usize {
            break;
        }
    }
    let before = &line[..at?.0];
    // Only the languages' own words: Java's extractor drops a `parent.` or `cls.` receiver too,
    // and there it is a variable (protobuf's `parent.getFullName()` on the EnumDescriptor an enum
    // value belongs to).
    if re!(r"(?:^|[^\w$.])super(?:\s*\([^()]*\)|<[^<>]*>)?\s*\.\s*$").is_match(before) {
        return Some(Receiver::Super);
    }
    if re!(r"(?:^|[^\w$.])(?:this|self)\s*\??\.\s*$").is_match(before) {
        return Some(Receiver::SelfRef);
    }
    if r.language == Language::Python && re!(r"(?:^|[^\w$.])cls\s*\.\s*$").is_match(before) {
        return Some(Receiver::SelfRef);
    }
    if re!(r"(?:\.|->|::)\s*$").is_match(before) {
        return None;
    }
    Some(Receiver::Bare)
}

/// A call that means a member of the class it is written in binds by the language's scoping, not
/// by which same-named method sits nearest: the caller's own class first, then what it inherits,
/// then (for a bare call) the classes it is nested in. Walks the caller's qualified-name scopes
/// innermost-out and returns the first candidate declared directly in one (a local function of an
/// enclosing method counts, same file only); a `super.` call skips its own class. `None` — the
/// existing ranking — when nothing in scope declares the name.
pub fn match_enclosing_scope_member(r: &UnresolvedRef, candidates: &[NodeRef], ctx: &Ctx) -> Option<NodeRef> {
    if !is_calls(r) || r.reference_name.contains(['.', ':']) {
        return None;
    }
    let implicit = is_implicit_this_language(r.language);
    if !implicit && !is_explicit_this_language(r.language) {
        return None;
    }
    let caller = ctx.node_by_id(&r.from_node_id).filter(|c| !c.qualified_name.is_empty())?;
    let receiver = call_receiver_kind(r, ctx).filter(|&k| k != Receiver::Bare || implicit)?;
    Walk::new(r, candidates, ctx, receiver).run(&caller)
}

/// The state one scope walk shares: the candidates keyed by the type that declares each (a
/// ubiquitous name — `get`, `init` — can bring thousands, and the walk asks about several scopes
/// and supertypes).
struct Walk<'a> {
    r: &'a UnresolvedRef,
    ctx: &'a Ctx,
    receiver: Receiver,
    by_owner: HashMap<String, Vec<NodeRef>>,
    by_owner_name: HashMap<String, Vec<NodeRef>>,
}

impl<'a> Walk<'a> {
    fn new(r: &'a UnresolvedRef, candidates: &[NodeRef], ctx: &'a Ctx, receiver: Receiver) -> Self {
        let name = r.reference_name.as_str();
        let mut by_owner: HashMap<String, Vec<NodeRef>> = HashMap::new();
        let mut by_owner_name: HashMap<String, Vec<NodeRef>> = HashMap::new();
        for c in candidates {
            let Some(sep) = c.qualified_name.rfind("::").filter(|&s| s > 0) else { continue };
            if &c.qualified_name[sep + 2..] != name {
                continue;
            }
            let owner = &c.qualified_name[..sep];
            let simple = owner.rfind("::").map_or(owner, |cut| &owner[cut + 2..]);
            by_owner.entry(owner.to_string()).or_default().push(c.clone());
            by_owner_name.entry(simple.to_string()).or_default().push(c.clone());
        }
        Walk { r, ctx, receiver, by_owner, by_owner_name }
    }

    fn name(&self) -> &str {
        &self.r.reference_name
    }

    /// Whatever a scope declares under the name shadows the scopes outside it — a Scala `val`
    /// applied like a function, a nested class constructed by a bare call — but a method is the
    /// likelier target when both exist.
    fn callable_first(mut found: Vec<NodeRef>) -> Vec<NodeRef> {
        found.sort_by_key(|n| !is_callable(n));
        found
    }

    /// `Owner::Owner` is a constructor: a bare `Owner(...)` builds a new instance, it does not call
    /// an inherited or enclosing member.
    fn members(&self, owner: &str) -> Vec<NodeRef> {
        let name = self.name();
        if owner == name || owner.ends_with(&format!("::{name}")) {
            return Vec::new();
        }
        Self::callable_first(self.by_owner.get(owner).cloned().unwrap_or_default())
    }

    fn types_named(&self, qn: &str) -> Vec<NodeRef> {
        self.ctx
            .get_nodes_by_qualified_name(qn)
            .iter()
            .filter(|n| is_member_owner_kind(n.kind) && same_language_family(n.language, self.r.language))
            .cloned()
            .collect()
    }

    /// One member of `owner`: the caller's file first; another file only when a single type in the
    /// index carries that qualified name.
    fn pick(&self, owner: &str, is_type: bool) -> Option<NodeRef> {
        let found = self.members(owner);
        let first = found.first()?;
        if let Some(local) = found.iter().find(|c| c.file_path == self.r.file_path) {
            return Some(local.clone());
        }
        (is_type && self.types_named(owner).len() == 1).then(|| first.clone())
    }

    /// The declarations of `scope` that enclose the call, else every one.
    fn enclosing_decls(&self, decls: &[NodeRef]) -> Vec<NodeRef> {
        decls.iter().filter(|n| n.file_path == self.r.file_path && n.start_line <= self.r.line && n.end_line >= self.r.line).cloned().collect()
    }

    /// The declaration of `scope` the call sits in, else the only one.
    fn decl_of(&self, scope: &str) -> Option<NodeRef> {
        let decls = self.types_named(scope);
        if let Some(enclosing) = self.enclosing_decls(&decls).pop() {
            return Some(enclosing);
        }
        (decls.len() == 1).then(|| decls[0].clone())
    }

    /// The text from the declaration's line up to the body's `{`, over at most 30 lines.
    fn header_text(&self, decl: &NodeRef) -> Option<String> {
        let lines = self.ctx.get_file_lines(&decl.file_path)?;
        let mut header = String::new();
        let from = (decl.start_line as usize).saturating_sub(1);
        for line in lines.iter().take(decl.start_line as usize + 30).skip(from) {
            let brace = line.find('{');
            header.push(' ');
            header.push_str(brace.map_or(line.as_str(), |b| &line[..b]));
            if brace.is_some() {
                break;
            }
        }
        Some(header)
    }

    /// The declaration's header — its text up to the body's `{`. The declaration that encloses the
    /// call when there is one (a Scala `object Cookies` beside a `trait Cookies`), else every
    /// declaration of that name (a C++ class whose member is defined out of line).
    fn type_header(&self, scope: &str) -> Option<String> {
        let decls = self.types_named(scope);
        let enclosing = self.enclosing_decls(&decls);
        let chosen: &[NodeRef] = match enclosing.split_last() {
            Some((last, _)) => std::slice::from_ref(last),
            None => &decls,
        };
        let mut header = String::new();
        for decl in chosen {
            header.push_str(&self.header_text(decl)?);
        }
        (!decls.is_empty()).then_some(header)
    }

    /// Where `word` first appears in `header` as a whole identifier; `None` when it doesn't.
    fn word_position(header: &str, word: &str) -> Option<usize> {
        Re::cached(&format!(r"(?<![\w$]){}(?![\w$])", escape_regex(word)), "").find(header).map(|m| m.start())
    }

    /// A declaration's supertypes through its own resolved extends/implements edges, in the order
    /// its header names them.
    fn supertypes_of(&self, decl: &NodeRef) -> Vec<NodeRef> {
        let mut supers: Vec<NodeRef> = self.ctx.get_supertype_nodes(&decl.id).iter().cloned().collect();
        if supers.len() < 2 {
            return supers;
        }
        let header = self.header_text(decl).unwrap_or_default();
        supers.sort_by_key(|s| Self::word_position(&header, &s.name).unwrap_or(usize::MAX));
        supers
    }

    /// Inherited members through those edges, breadth-first: the members of the very types the
    /// declaration extends, where a walk by name reaches every type of that name (Django's
    /// per-backend `DatabaseSchemaEditor`s differ in what they override).
    fn inherited_through(&self, decl: &NodeRef) -> Option<NodeRef> {
        let name = self.name();
        let mut seen: HashSet<String> = HashSet::from([decl.id.clone()]);
        let mut frontier = vec![decl.clone()];
        // An interface's or an abstract declaration answers only when no class further up
        // implements the member: SystemUI's `RotationContextButton extends ContextualButton
        // implements RotationButton` runs ButtonDispatcher's `getCurrentView`, not
        // RotationButton's.
        let mut declared: Option<NodeRef> = None;
        for _ in 0..4 {
            if frontier.is_empty() {
                break;
            }
            let mut next: Vec<NodeRef> = Vec::new();
            for t in &frontier {
                for s in self.supertypes_of(t) {
                    if !seen.insert(s.id.clone()) {
                        continue;
                    }
                    next.push(s.clone());
                    if s.name == name {
                        continue;
                    }
                    let abstract_only = matches!(s.kind, NodeKind::Interface | NodeKind::Protocol);
                    let found = Self::callable_first(
                        self.by_owner
                            .get(&s.qualified_name)
                            .into_iter()
                            .flatten()
                            .filter(|c| c.file_path == s.file_path && !(self.receiver == Receiver::Super && (c.is_abstract == Some(true) || abstract_only)))
                            .cloned()
                            .collect(),
                    );
                    let Some(first) = found.first() else { continue };
                    if !abstract_only && first.is_abstract != Some(true) {
                        return Some(first.clone());
                    }
                    declared.get_or_insert_with(|| first.clone());
                }
            }
            frontier = next;
        }
        declared
    }

    fn is_interface_member(&self, member: &NodeRef) -> bool {
        member.qualified_name.rfind("::").filter(|&s| s > 0).is_some_and(|sep| {
            self.ctx.get_nodes_by_qualified_name(&member.qualified_name[..sep]).iter().any(|n| matches!(n.kind, NodeKind::Interface | NodeKind::Protocol))
        })
    }

    /// Inherited members, breadth-first over the resolved extends/implements edges, starting from
    /// the caller's own type's supertypes — by name, which is the fallback when the type's own
    /// edges reach nothing.
    fn inherited(&self, first_supers: &[String], type_name: &str) -> Option<NodeRef> {
        let name = self.name();
        let mut frontier = vec![type_name.to_string()];
        let mut seen: HashSet<String> = frontier.iter().cloned().collect();
        for depth in 0..4 {
            if frontier.is_empty() {
                break;
            }
            let mut next: Vec<String> = Vec::new();
            for t in &frontier {
                let supers: Vec<String> = if depth == 0 { first_supers.to_vec() } else { self.ctx.get_supertypes(t, self.r.language).to_vec() };
                for s in supers {
                    if !seen.insert(s.clone()) {
                        continue;
                    }
                    next.push(s.clone());
                    if s == name {
                        continue;
                    }
                    // Supertypes come back as bare names: when the caller's language declares a
                    // type of that name, a same-named type of another language (Scala `Results`
                    // vs Java `play.mvc.Results`) is not it.
                    let Some(owned) = self.by_owner_name.get(&s) else { continue };
                    let own_language = self.ctx.get_nodes_by_name(&s).iter().any(|n| is_member_owner_kind(n.kind) && n.language == self.r.language);
                    let found = Self::callable_first(
                        owned
                            .iter()
                            .filter(|c| {
                                (!own_language || c.language == self.r.language)
                                    // `super.m()` runs a superclass body, never an interface's
                                    // abstract declaration.
                                    && !(self.receiver == Receiver::Super && (c.is_abstract == Some(true) || self.is_interface_member(c)))
                            })
                            .cloned()
                            .collect(),
                    );
                    let Some(first) = found.first() else { continue };
                    // Overloads share a file; copies of a same-named type in several files are a
                    // guess unless one is the caller's.
                    if found.iter().all(|c| c.file_path == first.file_path) {
                        return Some(first.clone());
                    }
                    return found.iter().find(|c| c.file_path == self.r.file_path).cloned();
                }
            }
            frontier = next;
        }
        None
    }

    /// `get_supertypes` answers for every type of that simple name (all of a protobuf file's
    /// `Builder`s at once), so keep the ones this declaration's header actually names, in the order
    /// it names them: the first base that has the member is the one a call reaches (`class A(B, C)`).
    fn direct_supertypes(&self, scope: &str, simple: &str) -> Vec<String> {
        let all = self.ctx.get_supertypes(simple, self.r.language).to_vec();
        if all.is_empty() {
            return all;
        }
        let Some(header) = self.type_header(scope) else { return all };
        let mut named: Vec<(usize, String)> = all.into_iter().filter_map(|t| Self::word_position(&header, &t).map(|at| (at, t))).collect();
        named.sort_by_key(|(at, _)| *at);
        named.into_iter().map(|(_, t)| t).collect()
    }

    fn declares_supertype(&self, scope: &str, simple: &str) -> bool {
        if simple.starts_with('<') {
            return true; // an anonymous class always has one
        }
        let Some(header) = self.type_header(scope) else { return true };
        // `extends` / `implements` / `with`, a C++ `:` base list.
        re!(r"\b(?:extends|implements|with)\b|[\w>)\]]\s*:(?!:)").is_match(&header)
    }

    fn run(&self, caller: &NodeRef) -> Option<NodeRef> {
        let segments: Vec<&str> = caller.qualified_name.split("::").collect();
        // A method's scopes start at its owner; a class caller (a field initializer) is its own scope.
        let start = if is_member_owner_kind(caller.kind) { segments.len() } else { segments.len() - 1 };
        for i in (1..=start).rev() {
            let scope = segments[..i].join("::");
            let is_type = !self.types_named(&scope).is_empty();
            if !is_type {
                // `this.`/`super.` name a class member; only a bare call sees locals.
                if self.receiver != Receiver::Bare {
                    continue;
                }
                if let Some(local) = self.members(&scope).into_iter().find(|c| c.file_path == self.r.file_path && is_callable(c)) {
                    return Some(local);
                }
                continue;
            }
            if self.receiver != Receiver::Super {
                if let Some(own) = self.pick(&scope, true) {
                    return Some(own);
                }
            }
            // The type's own supertypes first; by name when they reach nothing, as when the index
            // resolved an `extends` to a namesake (Minestom's `Player extends LivingEntity` to a
            // nested `EntityStatuses.LivingEntity`).
            let decl = self.decl_of(&scope);
            let supers = self.direct_supertypes(&scope, segments[i - 1]);
            let via_super = decl.and_then(|d| self.inherited_through(&d)).or_else(|| self.inherited(&supers, segments[i - 1]));
            if via_super.is_some() {
                return via_super;
            }
            // `this`/`self`/`super` is the innermost instance; outer classes are reachable only by
            // a bare call.
            if self.receiver != Receiver::Bare {
                return None;
            }
            // A supertype outside the index (an Android `View`, the listener an anonymous class
            // implements) may declare the name, and it would win over the outer class: stop rather
            // than guess past it.
            if supers.is_empty() && self.declares_supertype(&scope, segments[i - 1]) {
                return None;
            }
        }
        None
    }
}
