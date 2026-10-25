//! CICS framework resolver (COBOL). Port of `src/resolution/frameworks/cics.ts`.
//!
//! Resolves the pseudo-conversational transaction hop: a program ends with `EXEC CICS RETURN
//! TRANSID('CB00')` (or START), and CICS re-invokes the program that OWNS transaction CB00 on the
//! next attention key. The transaction→program mapping lives in the CICS CSD, which is never in the
//! repo — but by near-universal convention each program declares its own transaction id as a
//! working-storage constant:
//!
//! ```text
//! 05 WS-TRANID    PIC X(04) VALUE 'CB00'.
//! ```
//!
//! The COBOL extractor emits `cics-transid:CB00` call references for literal (or same-file-
//! dereferenced) TRANSID options; this resolver maps the id to the program module whose TRAN*-named
//! data item declares that VALUE. No match (an id owned by a program outside the repo) stays
//! unresolved.

use crate::ctx::Ctx;
use crate::types::{FrameworkResolver, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::Re;
use codegraph_types::{Language, NodeKind};
use std::cell::RefCell;
use std::collections::HashMap;

const TRANSID_REF_PREFIX: &str = "cics-transid:";

pub struct CicsResolver;

const LANGUAGES: [Language; 1] = [Language::Cobol];

/// transaction id → owning program module, built once per resolution context. Memoised so a
/// per-ref scan of every data node can't go quadratic on copybook-heavy repos.
#[derive(Default)]
struct TransidIndex(RefCell<Option<std::rc::Rc<HashMap<String, String>>>>);

fn build_index(ctx: &Ctx) -> HashMap<String, String> {
    let mut index: HashMap<String, String> = HashMap::new();
    // Data items that name a transaction id by convention.
    let tranid_name = Re::cached("TRAN", "i");
    let value_literal = Re::cached(r#"\bVALUE\s+['"]([A-Za-z0-9$#@]{1,4})['"]"#, "i");
    let mut data_nodes = Vec::new();
    for kind in [NodeKind::Variable, NodeKind::Field, NodeKind::Constant] {
        data_nodes.extend(ctx.get_nodes_by_kind(kind).iter().cloned());
    }
    for node in &data_nodes {
        if node.language != Language::Cobol {
            continue;
        }
        if !tranid_name.is_match(&node.name) {
            continue;
        }
        let Some(value) = node.signature.as_deref().and_then(|s| value_literal.captures(s)).and_then(|c| c.text(1).filter(|t| !t.is_empty()).map(String::from)) else {
            continue;
        };
        let tx = value.to_uppercase();
        if index.contains_key(&tx) {
            continue; // first declaration wins; collisions are rare and ambiguous
        }
        let module_node = ctx.get_nodes_in_file(&node.file_path).iter().find(|n| n.kind == NodeKind::Module && n.language == Language::Cobol).cloned();
        if let Some(module_node) = module_node {
            index.insert(tx, module_node.id.clone());
        }
    }
    index
}

impl FrameworkResolver for CicsResolver {
    fn name(&self) -> &str {
        "cics"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        // Any indexed COBOL program qualifies — the resolver only ever acts on cics-transid:
        // references, which only the COBOL extractor emits.
        ctx.get_nodes_by_kind(NodeKind::Module).iter().any(|n| n.language == Language::Cobol)
    }

    // cics-transid:XXXX matches no symbol name — opt it past the name-exists pre-filter so it
    // reaches resolve().
    fn claims_reference(&self, name: &str) -> bool {
        name.starts_with(TRANSID_REF_PREFIX)
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        let tx = r.reference_name.strip_prefix(TRANSID_REF_PREFIX)?.to_uppercase();

        let memo = ctx.memo::<TransidIndex>();
        let cached = memo.0.borrow().clone();
        let index = match cached {
            Some(index) => index,
            None => {
                let index = std::rc::Rc::new(build_index(ctx));
                *memo.0.borrow_mut() = Some(std::rc::Rc::clone(&index));
                index
            }
        };

        let target_node_id = index.get(&tx)?;
        Some(ResolvedRef::new(r, target_node_id.clone(), 0.85, ResolvedBy::Framework))
    }
}
