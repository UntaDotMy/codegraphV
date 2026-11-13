//! Upstream 47a9c7d6: reference order must not change a cached binding decision.
use codegraph_core::CodeGraph;
use codegraph_resolve::{
    name_matcher::match_function_ref, types::UnresolvedRef, ReferenceResolver,
};
use codegraph_types::{Language, ReferenceKind};
#[test]
fn local_binding_answers_depend_on_the_reference_line_in_both_orders() {
    let dir = tempfile::tempdir().unwrap();
    let source = "def wrap(fn):\n    return fn\n\ndef outer(flag):\n    def view():\n        return 1\n    before = wrap(view)\n    view = wrap(view)\n    after = wrap(view)\n    return before, after\n\ndef related():\n    def Fan():\n        return 1\n    first = wrap(\n        Fan, through=\"Invitation\"\n    )\n    second = wrap(Fan)\n    return first, second\n";
    std::fs::write(dir.path().join("views.py"), source).unwrap();
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let nodes = cg.get_nodes_by_file("views.py").unwrap();
    let outer = nodes.iter().find(|n| n.name == "outer").unwrap();
    let view = nodes.iter().find(|n| n.name == "view").unwrap();
    for order in [[7, 9, 8], [9, 8, 7]] {
        let resolver = ReferenceResolver::new(dir.path().to_str().unwrap(), cg.store().clone());
        resolver.initialize();
        for line in order {
            let reference = UnresolvedRef::new(
                &outer.id,
                "view",
                ReferenceKind::FunctionRef,
                line,
                13,
                "views.py",
                Language::Python,
            );
            let got = match_function_ref(&reference, resolver.context()).map(|r| r.target_node_id);
            let want = (line != 9).then(|| view.id.clone());
            assert_eq!(got, want, "order {order:?}, line {line}");
        }
    }
    let resolver = ReferenceResolver::new(dir.path().to_str().unwrap(), cg.store().clone());
    resolver.initialize();
    let related = nodes.iter().find(|n| n.name == "related").unwrap();
    let fan = nodes.iter().find(|n| n.name == "Fan").unwrap();
    let reference = UnresolvedRef::new(
        &related.id,
        "Fan",
        ReferenceKind::FunctionRef,
        18,
        18,
        "views.py",
        Language::Python,
    );
    assert_eq!(
        match_function_ref(&reference, resolver.context())
            .unwrap()
            .target_node_id,
        fan.id
    );
    cg.close();
}
