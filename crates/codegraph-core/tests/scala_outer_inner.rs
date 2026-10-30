//! A Scala `Outer.Inner(args)` call reaches the Scala case class or object it builds (upstream
//! b6379e6d). Scala qualified names carry no package (`Outer::Inner`), so a call written
//! `Outer.Inner(args)` found no type by its written name and fell through to the name strategies;
//! under a return type annotation (`def f(): WebSocket = WebSocket.Accepted(flow)`) the
//! local-declaration reader took `WebSocket` for the receiver's type and handed over an unrelated
//! Java constructor, `Other::Accepted::Accepted`.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

const WEB_SOCKET_JAVA: &str = "package com.acme.http;\npublic abstract class WebSocket {\n  public static WebSocket fromString(String s) { return null; }\n  public static class Accepted {\n    public Accepted(String flow) {}\n  }\n}\n";
// A second Java class sharing the method names, so the receiver-word strategy (not a unique-name
// shortcut) is what decides.
const OTHER_JAVA: &str = "package com.acme.http;\npublic class Other {\n  public static Other fromString(String s) { return null; }\n  public static class Accepted {\n    public Accepted(String flow) {}\n  }\n}\n";

fn index(scala: &[(&str, &str)]) -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    let mut files = vec![("java/com/acme/http/WebSocket.java", WEB_SOCKET_JAVA), ("java/com/acme/http/Other.java", OTHER_JAVA)];
    files.extend_from_slice(scala);
    for (relative, content) in files {
        let path = dir.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

#[derive(Debug, PartialEq)]
struct Edge {
    kind: String,
    qn: String,
    lang: String,
}

fn edge(kind: &str, qn: &str, lang: &str) -> Edge {
    Edge { kind: kind.into(), qn: qn.into(), lang: lang.into() }
}

fn edges_from(cg: &CodeGraph, name: &str) -> Vec<Edge> {
    let Some(from) = cg.get_nodes_by_kind(NodeKind::Method).unwrap().into_iter().find(|n| n.name == name) else { return Vec::new() };
    cg.get_outgoing_edges(&from.id)
        .unwrap()
        .into_iter()
        .filter(|e| !matches!(e.kind, EdgeKind::Contains | EdgeKind::References))
        .map(|e| {
            let target = cg.get_node(&e.target).unwrap().expect("target exists");
            Edge { kind: e.kind.as_str().into(), qn: target.qualified_name, lang: target.language.as_str().into() }
        })
        .collect()
}

#[test]
fn outer_inner_resolves_to_the_scala_case_class_never_the_java_constructor() {
    let (_dir, cg) = index(&[
        ("scala/app/WebSocket.scala", "package app\ntrait WebSocket\nobject WebSocket {\n  final case class Accepted[In, Out](flow: String, sub: Option[String])\n}\n"),
        ("scala/app/Use.scala", "package app\nclass Use {\n  def accept(): Unit = { WebSocket.Accepted(\"f\", None) }\n}\n"),
    ]);
    let out = edges_from(&cg, "accept");
    assert!(out.iter().all(|e| e.lang != "java"), "{out:?}");
    assert!(out.contains(&edge("instantiates", "WebSocket::Accepted", "scala")), "{out:?}");
}

#[test]
fn a_return_type_annotation_read_as_the_receiver_type_does_not_hand_over_a_java_constructor() {
    // `def f(): WebSocket = WebSocket.Accepted(...)`: the local-declaration pattern reads
    // `WebSocket = WebSocket.Accepted` as the receiver's type.
    let (_dir, cg) = index(&[
        ("scala/app/WebSocket.scala", "package app\ntrait WebSocket\nobject WebSocket {\n  final case class Accepted(flow: String) extends WebSocket\n}\n"),
        ("scala/app/Use.scala", "package app\nclass Use {\n  def accept(flow: String): WebSocket = WebSocket.Accepted(flow)\n}\n"),
    ]);
    let out = edges_from(&cg, "accept");
    assert!(out.iter().all(|e| e.lang != "java"), "{out:?}");
    assert!(out.contains(&edge("instantiates", "WebSocket::Accepted", "scala")), "{out:?}");
}

#[test]
fn a_scala_call_to_a_real_java_static_method_still_resolves() {
    let (_dir, cg) = index(&[("scala/app/Use.scala", "package app\nclass Use {\n  def load(): Unit = { WebSocket.fromString(\"y\") }\n}\n")]);
    assert!(edges_from(&cg, "load").contains(&edge("calls", "com.acme.http::WebSocket::fromString", "java")));
}

#[test]
fn outer_inner_resolves_to_a_nested_scala_object_that_defines_apply() {
    // A Scala `object` is indexed as a `module`, not a class.
    let (_dir, cg) = index(&[
        ("scala/app/WebSocket.scala", "package app\nobject WebSocket {\n  object Accepted {\n    def apply(flow: String): String = flow\n  }\n}\n"),
        ("scala/app/Use.scala", "package app\nclass Use {\n  def accept(): Unit = { WebSocket.Accepted(\"f\") }\n}\n"),
    ]);
    let out = edges_from(&cg, "accept");
    assert!(out.iter().all(|e| e.lang != "java"), "{out:?}");
    assert!(out.contains(&edge("calls", "WebSocket::Accepted", "scala")), "{out:?}");
}

#[test]
fn the_case_class_is_preferred_over_its_companion_object_declared_first() {
    let (_dir, cg) = index(&[
        (
            "scala/app/WebSocket.scala",
            "package app\nobject WebSocket {\n  object Accepted {\n    def apply(): Accepted = Accepted(\"default\")\n  }\n  final case class Accepted(flow: String)\n}\n",
        ),
        ("scala/app/Use.scala", "package app\nclass Use {\n  def accept(): Unit = { WebSocket.Accepted(\"f\") }\n}\n"),
    ]);
    let out = edges_from(&cg, "accept");
    assert!(out.iter().all(|e| e.lang != "java"), "{out:?}");
    assert!(out.contains(&edge("instantiates", "WebSocket::Accepted", "scala")), "{out:?}");
    assert!(!out.contains(&edge("calls", "WebSocket::Accepted", "scala")), "{out:?}");
}
