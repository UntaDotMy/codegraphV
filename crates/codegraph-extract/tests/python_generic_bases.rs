//! Upstream 3fca009e: only the value of a single subscript is a named base.
use codegraph_extract::extract_source;
use codegraph_types::{EdgeKind, Language, ReferenceKind};

#[test]
fn generic_bases_keep_qualified_names_and_reject_computed_bases() {
    let source = "class Leaf(Base[str], pkg.Pair[str, dict[str, int]]): pass\nclass Computed(make()[str]): pass\nclass Twice(Base[str][int]): pass\nclass Keyword(metaclass=Base): pass\n";
    for source in [source.to_owned(), source.replace('\n', "\r\n")] {
        let result = extract_source("main.py", &source, Language::Python).unwrap();
        let bases: Vec<_> = result.unresolved_references.iter()
            .filter(|r| r.reference_kind == ReferenceKind::Edge(EdgeKind::Extends))
            .map(|r| (r.reference_name.as_str(), r.line)).collect();
        assert_eq!(bases, [("Base", 1), ("pkg.Pair", 1)]);
    }
}
