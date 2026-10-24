//! The facade's segment seeding, including plural folding and live-node verification.
use codegraph_db::{Result, Store};
use codegraph_search::{segment_lookup_variants, split_identifier_segments};
use codegraph_types::{jscompat::utf16_len, NodeKind, SegmentMatch};
use indexmap::{IndexMap, IndexSet};
use std::collections::HashSet;

pub const SEGMENT_RARITY_CEILING: usize = 25;

pub fn get_segment_matches(
    store: &Store,
    words: &[String],
    limit: usize,
) -> Result<Vec<SegmentMatch>> {
    if words.is_empty() {
        return Ok(Vec::new());
    }
    let mut variants: IndexMap<String, String> = IndexMap::new();
    for word in words {
        for variant in segment_lookup_variants(word) {
            variants.entry(variant).or_insert_with(|| word.clone());
        }
    }
    let pairs: Vec<_> = variants
        .iter()
        .map(|(s, w)| (s.clone(), w.clone()))
        .collect();
    let mut candidates = Vec::new();
    for (name, _) in store.get_segment_co_occurrence(&pairs, 2, 24)? {
        let segments: HashSet<_> = split_identifier_segments(&name).into_iter().collect();
        let matched: IndexSet<_> = variants
            .iter()
            .filter(|(v, _)| segments.contains(v.as_str()))
            .map(|(_, w)| w.clone())
            .collect();
        if matched.len() >= 2 {
            candidates.push((name, matched));
        }
    }
    if candidates.is_empty() {
        let single: Vec<_> = variants
            .iter()
            .filter(|(_, w)| utf16_len(w) >= 5)
            .map(|(v, _)| v.clone())
            .collect();
        let mut counts: Vec<_> = store
            .get_segment_name_counts(&single)?
            .into_iter()
            .filter(|(_, n)| *n >= 2 && *n <= SEGMENT_RARITY_CEILING)
            .collect();
        counts.sort_by_key(|(_, n)| *n);
        for (variant, _) in counts.into_iter().take(2) {
            for name in store.get_names_for_segment(&variant, 12)? {
                if split_identifier_segments(&name).len() >= 2 {
                    candidates.push((name, IndexSet::from([variants[&variant].clone()])));
                }
            }
        }
    }
    candidates.sort_by(|(an, aw), (bn, bw)| {
        bw.len()
            .cmp(&aw.len())
            .then(utf16_len(an).cmp(&utf16_len(bn)))
    });
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (name, matched) in candidates {
        if out.len() >= limit {
            break;
        }
        if !seen.insert(name.clone()) {
            continue;
        }
        let nodes = store.get_nodes_by_name(&name)?;
        let Some(rep) = nodes
            .into_iter()
            .find(|n| !matches!(n.kind, NodeKind::File | NodeKind::Import))
        else {
            continue;
        };
        let mut matched_words: Vec<_> = matched.into_iter().collect();
        matched_words.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        out.push(SegmentMatch {
            name,
            kind: rep.kind,
            file_path: rep.file_path,
            start_line: rep.start_line,
            matched_words,
        });
    }
    Ok(out)
}
