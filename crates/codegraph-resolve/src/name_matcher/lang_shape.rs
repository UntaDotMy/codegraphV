//! The per-language shape of a reference and the candidate filters it implies.
//!
//! `matchByExactName` and `matchFuzzy` each narrow a same-named pool with a long conjunction of
//! language-specific tests. The shape of the reference (where its receiver sits, which scope it
//! is written in) is computed once per reference; every language with a scope rule contributes a
//! [`RefShape`] from its own module, so the two matchers cannot drift apart.

use super::calls::{can_name_in_type_position, fits_python_call_shape, is_dot_net_type_ref, python_call_shape, PythonCallShape};
use crate::ctx::Ctx;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::{Language, NodeKind};

/// A language's view of one reference: which same-named candidates it can reach, and how it
/// orders the ones that survive.
pub trait RefShape {
    /// Does the shape rule this candidate out?
    fn excludes(&self, n: &NodeRef, r: &UnresolvedRef, ctx: &Ctx) -> bool;
    /// The pool `matchByExactName` chooses from, ordered most-local first, when this language ranks its own members.
    fn rank_exact(&self, _pool: &[NodeRef], _r: &UnresolvedRef, _ctx: &Ctx) -> Option<Vec<NodeRef>> {
        None
    }
    /// The same for `matchFuzzy`.
    fn rank_fuzzy(&self, _pool: &[NodeRef], _r: &UnresolvedRef, _ctx: &Ctx) -> Option<Vec<NodeRef>> {
        None
    }
}

/// Several shapes of one reference (a Kotlin call is both a stdlib chain and a bare call): it is
/// ruled out when any of them rules it out, and the first shape that ranks the pool decides.
pub struct AllOf(Vec<Box<dyn RefShape>>);

impl AllOf {
    pub fn of(mut shapes: Vec<Box<dyn RefShape>>) -> Option<Box<dyn RefShape>> {
        match shapes.len() {
            0 => None,
            1 => shapes.pop(),
            _ => Some(Box::new(AllOf(shapes))),
        }
    }
}

impl RefShape for AllOf {
    fn excludes(&self, n: &NodeRef, r: &UnresolvedRef, ctx: &Ctx) -> bool {
        self.0.iter().any(|s| s.excludes(n, r, ctx))
    }

    fn rank_exact(&self, pool: &[NodeRef], r: &UnresolvedRef, ctx: &Ctx) -> Option<Vec<NodeRef>> {
        self.0.iter().find_map(|s| s.rank_exact(pool, r, ctx))
    }

    fn rank_fuzzy(&self, pool: &[NodeRef], r: &UnresolvedRef, ctx: &Ctx) -> Option<Vec<NodeRef>> {
        self.0.iter().find_map(|s| s.rank_fuzzy(pool, r, ctx))
    }
}

pub struct LangShape {
    pub type_ref: bool,
    /// A bare Rust name: the scope test, its per-reference half computed once.
    pub rust_scope: Option<super::rust::scope::RustRefScope>,
    pub python: Option<PythonCallShape>,
    pub lang: Option<Box<dyn RefShape>>,
}

impl LangShape {
    pub fn compute(r: &UnresolvedRef, ctx: &Ctx) -> LangShape {
        LangShape {
            type_ref: is_dot_net_type_ref(r, ctx),
            rust_scope: (r.language == Language::Rust && re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name))
                .then(|| super::rust::scope::RustRefScope::of(r, ctx)),
            python: python_call_shape(r, ctx),
            lang: super::pending::language_shape(r, ctx),
        }
    }

    /// Tests both matchers share: does the shape rule this candidate out?
    pub fn excludes(&self, n: &NodeRef, r: &UnresolvedRef, ctx: &Ctx) -> bool {
        (self.type_ref && !can_name_in_type_position(n))
            || self.rust_scope.as_ref().is_some_and(|s| !s.admits(n, r, ctx))
            || self.python.as_ref().is_some_and(|shape| !fits_python_call_shape(n, shape, r, ctx))
            || self.lang.as_ref().is_some_and(|l| l.excludes(n, r, ctx))
            || super::pending::is_hidden_by_language(n, r, ctx)
    }

    pub fn rank_exact(&self, pool: &[NodeRef], r: &UnresolvedRef, ctx: &Ctx) -> Option<Vec<NodeRef>> {
        self.lang.as_ref().and_then(|l| l.rank_exact(pool, r, ctx))
    }

    pub fn rank_fuzzy(&self, pool: &[NodeRef], r: &UnresolvedRef, ctx: &Ctx) -> Option<Vec<NodeRef>> {
        self.lang.as_ref().and_then(|l| l.rank_fuzzy(pool, r, ctx))
    }
}

/// `n.kind` is a method owner (`class`, `struct`, `union`, `interface`, a Scala `object`).
pub fn is_method_owner_kind(n: &codegraph_types::Node) -> bool {
    matches!(n.kind, NodeKind::Class | NodeKind::Struct | NodeKind::Union | NodeKind::Interface) || (n.language == Language::Scala && n.kind == NodeKind::Module)
}
