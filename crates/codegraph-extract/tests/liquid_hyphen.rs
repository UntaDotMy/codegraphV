//! Upstream a0c6c1b1 (3080): a Liquid variable named with a hyphen is indexed under its whole
//! name, not cut short or dropped.
use codegraph_extract::extract_source;
use codegraph_types::{Language, NodeKind};

#[test]
fn hyphenated_liquid_variables_keep_their_whole_name() {
    let source = "{% assign product-title = product.title %}\n{% capture 'meta-description' %}x{% endcapture %}\n{% liquid\n  assign stock-level = 1\n  capture promo-code\n  endcapture\n%}\n";
    let result = extract_source("snippets/card.liquid", source, Language::Liquid).unwrap();
    let mut names: Vec<_> = result
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Variable)
        .map(|n| n.name.as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["meta-description", "product-title", "promo-code", "stock-level"]);
}
