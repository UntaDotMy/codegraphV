//! A Java read through a qualifier reads that class's constant, not this file's own of the name
//! (upstream c3443661; fixture from java-value-reads.test.ts).
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

const FILES: &[(&str, &str)] = &[
    ("src/p/Other.java", r#"package p;

public class Other {
    public static final Object PARSER = new Object();
}
"#),
    ("src/p/Req.java", r#"package p;

public class Req {
    public static final Object PARSER = new Object();
    public static final Object FIELDS = new Object();

    public static class Inner {
        public static final Object DEEP_ONE = new Object();
    }

    Object foreign() {
        return Other.PARSER;
    }

    Object foreignChain() {
        return Other.PARSER.toString();
    }

    Object bare() {
        return PARSER;
    }

    Object ownClass() {
        return Req.PARSER;
    }

    Object viaThis() {
        return this.PARSER;
    }

    Object bothForms() {
        return Other.PARSER == null ? FIELDS : PARSER;
    }

    Object nested() {
        return Inner.DEEP_ONE;
    }
}
"#),
];

#[test]
fn a_java_read_through_a_qualifier_is_that_classs() {
    let dir = tempfile::tempdir().unwrap();
    for (rel, text) in FILES {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let methods = cg.get_nodes_by_kind(NodeKind::Method).unwrap();
    let reads = |name: &str| -> Vec<String> {
        let from = methods.iter().find(|n| n.name == name).unwrap_or_else(|| panic!("no method {name}"));
        let mut out: Vec<String> = cg
            .get_outgoing_edges(&from.id)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == EdgeKind::References)
            .filter_map(|e| cg.get_node(&e.target).unwrap())
            .filter(|n| n.kind == NodeKind::Constant)
            .map(|n| n.qualified_name)
            .collect();
        out.sort();
        out
    };
    assert_eq!(reads("foreign"), Vec::<String>::new());
    assert_eq!(reads("foreignChain"), Vec::<String>::new());
    assert_eq!(reads("bare"), ["p::Req::PARSER"]);
    assert_eq!(reads("ownClass"), ["p::Req::PARSER"]);
    assert_eq!(reads("viaThis"), ["p::Req::PARSER"]);
    assert_eq!(reads("bothForms"), ["p::Req::FIELDS", "p::Req::PARSER"]);
    assert_eq!(reads("nested"), ["p::Req::Inner::DEEP_ONE"]);
    cg.close();
}
