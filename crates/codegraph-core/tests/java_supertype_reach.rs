//! What a Java (and C#) type inherits does not depend on resolve order or on a namesake nested
//! elsewhere (upstream fab65e9d; fixture from java-supertype-reach.test.ts).
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, Node, NodeKind};
use std::fs;

const FILES: &[(&str, &str)] = &[
    ("src/o/L0.java", r#"package o;

public class L0 extends L1 {
}
"#),
    ("src/o/L1.java", r#"package o;

public class L1 extends L2 {
}
"#),
    ("src/o/L2.java", r#"package o;

public class L2 extends L3 {
}
"#),
    ("src/o/L3.java", r#"package o;

public class L3 extends L4 {
}
"#),
    ("src/o/L4.java", r#"package o;

public class L4 extends L5 {
}
"#),
    ("src/o/L5.java", r#"package o;

public class L5 extends L6 {
}
"#),
    ("src/o/L6.java", r#"package o;

public class L6 implements Writer {
}
"#),
    ("src/o/Writer.java", r#"package o;

public interface Writer {
    interface Reader<V> {
    }
}
"#),
    ("src/o/Deep.java", r#"package o;

public class Deep extends L0 {
    Reader<String> farUp() { return null; }
}
"#),
    ("src/o/Near.java", r#"package o;

public class Near extends L3 {
    Reader<String> inherited() { return null; }
}
"#),
    ("src/o/Own.java", r#"package o;

public class Own {
    public interface Scanner {
    }
    Object make(Theirs theirs) {
        Theirs.Scanner other = theirs.scanner();
        return new Scanner() {
        };
    }
}
"#),
    ("src/o/Theirs.java", r#"package o;

public class Theirs {
    public interface Scanner {
    }
    Scanner scanner() { return null; }
}
"#),
    ("src/o/Holder.java", r#"package o;

public class Holder {
    public interface Bucket {
        interface Reader<B> {
        }
    }
}
"#),
    ("src/o/Written.java", r#"package o;

import o.Holder.Bucket;

public class Written extends L6 {
    java.io.Reader throughPackage() { return null; }
    Bucket.Reader<String> throughType() { return null; }
}
"#),
    ("server/src/main/java/org/es/search/aggregations/bucket/geogrid/GeoGrid.java", r#"package org.es.search.aggregations.bucket.geogrid;

public interface GeoGrid {
}
"#),
    ("server/src/main/java/org/es/search/aggregations/bucket/geogrid/InternalGeoGrid.java", r#"package org.es.search.aggregations.bucket.geogrid;

public abstract class InternalGeoGrid<B> extends Base<
    InternalGeoGrid<B>,
    B> implements GeoGrid {
}
"#),
    ("server/src/main/java/org/es/search/aggregations/bucket/geogrid/Base.java", r#"package org.es.search.aggregations.bucket.geogrid;

public abstract class Base<A, B> {
}
"#),
    ("server/src/main/java/org/es/index/mapper/blockloader/BlockLoaderFunctionConfig.java", r#"package org.es.index.mapper.blockloader;

public sealed interface BlockLoaderFunctionConfig {
    record GeoGrid(int precision) implements BlockLoaderFunctionConfig {
    }
}
"#),
    ("cs/Types.cs", r#"namespace O
{
    public class CL6 : CWriter { }
    public class CL5 : CL6 { }
    public class CL4 : CL5 { }
    public class CL3 : CL4 { }
    public class CL2 : CL3 { }
    public class CL1 : CL2 { }
    public class CL0 : CL1 { }
    public interface CWriter
    {
        public interface CReader<V> { }
    }
}
"#),
    ("cs/ADeep.cs", r#"namespace O
{
    public class ADeep : CL0
    {
        CReader<string> FarUp() { return null; }
    }
}
"#),
    ("cs/Near.cs", r#"namespace O
{
    public class Near : CL3
    {
        CReader<string> Inherited() { return null; }
    }
}
"#),
    ("src/c/Template.java", r#"package c;

public class Template {
    public String lifecycle() { return ""; }
}
"#),
    ("src/d/FailureStore.java", r#"package d;

public class FailureStore {
    public record Template(String lifecycle, int enabled) {
    }
}
"#),
    ("src/e/Use.java", r#"package e;

import c.Template;
import java.util.Optional;

public class Use {
    Object viaImport(Template t) {
        return Optional.ofNullable(t).map(Template::lifecycle);
    }
}
"#),
    ("src/p/Outer.java", r#"package p;

public class Outer {
    public static class Helper {
    }
}
"#),
    ("src/q/Sub.java", r#"package q;

import p.Outer;

public class Sub extends Outer {
    Helper helper() { return null; }
}
"#),
];

fn targets(cg: &CodeGraph, from_id: &str, kinds: &[EdgeKind]) -> Vec<Node> {
    cg.get_outgoing_edges(from_id)
        .unwrap()
        .into_iter()
        .filter(|e| kinds.contains(&e.kind))
        .filter_map(|e| cg.get_node(&e.target).unwrap())
        .collect()
}

#[test]
fn what_a_java_type_inherits_does_not_depend_on_resolve_order_or_a_namesake() {
    let dir = tempfile::tempdir().unwrap();
    for (rel, text) in FILES {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let methods = cg.get_nodes_by_kind(NodeKind::Method).unwrap();
    let referenced = |name: &str| -> Vec<String> {
        let from = methods.iter().find(|n| n.name == name).unwrap_or_else(|| panic!("no method {name}"));
        targets(&cg, &from.id, &[EdgeKind::References]).into_iter().map(|n| n.qualified_name).collect()
    };
    let mut failed: Vec<String> = Vec::new();
    let mut eq = |label: &str, got: Vec<String>, want: &[&str]| {
        if got != want.iter().map(|s| s.to_string()).collect::<Vec<_>>() {
            failed.push(format!("{label}: got {got:?}, want {want:?}"));
        }
    };
    eq("inherited", referenced("inherited"), &["o::Writer::Reader"]);
    eq("farUp", referenced("farUp"), &["o::Writer::Reader"]);
    eq("throughPackage", referenced("throughPackage"), &[]);
    eq("C# FarUp", referenced("FarUp"), &["O::CWriter::CReader"]);
    eq("C# Inherited", referenced("Inherited"), &["O::CWriter::CReader"]);
    eq("helper", referenced("helper"), &["p::Outer::Helper"]);

    let classes = cg.get_nodes_by_kind(NodeKind::Class).unwrap();
    let anon = classes.iter().find(|n| n.name.starts_with("<Scanner$anon@")).expect("anonymous Scanner");
    let implemented: Vec<String> = targets(&cg, &anon.id, &[EdgeKind::Implements, EdgeKind::Extends]).into_iter().map(|n| n.qualified_name).collect();
    eq("anon Scanner", implemented, &["o::Own::Scanner"]);
    let internal = classes.iter().find(|n| n.name == "InternalGeoGrid").expect("InternalGeoGrid");
    let geo: Vec<String> = targets(&cg, &internal.id, &[EdgeKind::Implements]).into_iter().map(|n| format!("{} {}", n.kind.as_str(), n.qualified_name)).collect();
    eq("InternalGeoGrid", geo, &["interface org.es.search.aggregations.bucket.geogrid::GeoGrid"]);

    let through_type = referenced("throughType");
    if !through_type.contains(&"o::Holder::Bucket::Reader".to_string()) || through_type.contains(&"o::Writer::Reader".to_string()) {
        failed.push(format!("throughType: {through_type:?}"));
    }
    let via_import = referenced("viaImport");
    if !via_import.contains(&"c::Template::lifecycle".to_string()) {
        failed.push(format!("viaImport: {via_import:?}"));
    }
    assert!(failed.is_empty(), "{} case(s) failed:\n{}", failed.len(), failed.join("\n"));
    cg.close();
}
