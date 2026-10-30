//! A call that means a member of the class it is written in — a receiver-less `render()` in Java /
//! Scala, or `this.render()` / `self.render()` in TypeScript / JavaScript / Python — binds to that
//! class's member (then what it inherits, then an enclosing class), not to whichever same-named
//! method is declared nearest the call (upstream 25776014). Each fixture puts the caller's own
//! `render` far above the call and a sibling class's `render` right below it, which is what the
//! same-file line-distance term used to pick. C++ has its own class lookup; its cases pin that it
//! keeps the same answer.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

fn filler(prefix: &str) -> String {
    (0..40).map(|i| format!("{prefix} filler {i}")).collect::<Vec<_>>().join("\n")
}

/// `qualifiedName@file` of every `calls` target of the method or function `from`, in a project of
/// the given files.
fn callees_in(files: &[(&str, String)], from: &str) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    for (relative, text) in files {
        let path = dir.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let caller = [NodeKind::Method, NodeKind::Function]
        .into_iter()
        .flat_map(|kind| cg.get_nodes_by_kind(kind).unwrap())
        .find(|n| n.qualified_name == from)
        .unwrap_or_else(|| panic!("no caller {from}"));
    cg.get_outgoing_edges(&caller.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Calls)
        .map(|e| match cg.get_node(&e.target).unwrap() {
            Some(target) => format!("{}@{}", target.qualified_name, target.file_path),
            None => "?".to_string(),
        })
        .collect()
}

fn qualified(callees: &[String]) -> Vec<String> {
    callees.iter().map(|c| c.split('@').next().unwrap().to_lowercase()).collect()
}

fn callees(file: &str, source: String, from: &str) -> Vec<String> {
    callees_in(&[(file, source)], from)
}

fn assert_own_class(lang: &str, file: &str, source: String) {
    let out = qualified(&callees(file, source, "A::show"));
    assert!(out.contains(&"a::render".to_string()), "{lang}: {out:?}");
    assert!(!out.contains(&"b::render".to_string()), "{lang}: {out:?}");
}

#[test]
fn scala_binds_to_the_callers_own_class_not_the_nearer_sibling() {
    let source = format!(
        "class A {{\n  def render(): String = \"a\"\n{}\n  def show(): String = render()\n}}\nclass B {{\n  def render(): String = \"b\"\n}}\n",
        filler("  //")
    );
    assert_own_class("Scala", "A.scala", source);
}

#[test]
fn cpp_inline_members_keep_the_callers_own_class() {
    let source = format!(
        "class A {{\npublic:\n  int render() {{ return 1; }}\n{}\n  int show() {{ return render(); }}\n}};\nclass B {{\npublic:\n  int render() {{ return 2; }}\n}};\n",
        filler("  //")
    );
    assert_own_class("C++ (inline members)", "a.cpp", source);
}

#[test]
fn cpp_out_of_line_members_keep_the_callers_own_class() {
    let source = format!(
        "class A {{ public: int render(); int show(); }};\nclass B {{ public: int render(); }};\nint A::render() {{ return 1; }}\n{}\nint A::show() {{ return render(); }}\nint B::render() {{ return 2; }}\n",
        filler("//")
    );
    assert_own_class("C++ (out-of-line members)", "b.cpp", source);
}

#[test]
fn python_self_call_binds_to_the_callers_own_class() {
    let source = format!(
        "class A:\n    def render(self):\n        return \"a\"\n{}\n    def show(self):\n        return self.render()\nclass B:\n    def render(self):\n        return \"b\"\n",
        filler("    #")
    );
    assert_own_class("Python (self.)", "a.py", source);
}

#[test]
fn typescript_this_call_binds_to_the_callers_own_class() {
    let source = format!(
        "export class A {{\n  render() {{ return \"a\"; }}\n{}\n  show() {{ return this.render(); }}\n}}\nexport class B {{\n  render() {{ return \"b\"; }}\n}}\n",
        filler("  //")
    );
    assert_own_class("TypeScript (this.)", "a.ts", source);
}

#[test]
fn java_super_call_skips_the_overriding_method_itself() {
    let source = format!(
        "class Base {{\n  String render() {{ return \"base\"; }}\n}}\n{}\nclass Kid extends Base {{\n  String render() {{ return super.render(); }}\n}}\n",
        filler("//")
    );
    let out = callees("S.java", source, "Kid::render");
    assert_eq!(qualified(&out), ["base::render"], "{out:?}");
}

#[test]
fn java_same_named_nested_types_inherit_only_from_what_their_own_header_names() {
    let source = format!(
        "interface AOrBuilder {{ String describe(); }}\ninterface BOrBuilder {{ String describe(); }}\nclass A {{\n  static class Builder implements AOrBuilder {{\n    public String describe() {{ return \"a\"; }}\n  }}\n}}\nclass B {{\n  static class Builder implements BOrBuilder {{\n    String show() {{ return describe(); }}\n    public String describe() {{ return \"b\"; }}\n  }}\n}}\nclass C {{\n  static abstract class Builder implements AOrBuilder {{\n{}\n    String show() {{ return describe(); }}\n  }}\n}}\n",
        filler("    //")
    );
    let out = callees("Msg.java", source, "C::Builder::show");
    assert_eq!(qualified(&out), ["aorbuilder::describe"], "{out:?}");
}

#[test]
fn python_a_base_named_like_types_in_other_files_is_not_taken_for_the_one_another_file_declares() {
    let out = callees_in(
        &[
            ("a/base.py", "class BaseEditor:\n    def execute(self, sql):\n        return sql\n".to_string()),
            ("b/pg.py", "from a.base import BaseEditor\n\nclass DatabaseSchemaEditor(BaseEditor):\n    def execute(self, sql):\n        return super().execute(sql)\n".to_string()),
            ("c/lite.py", "from a.base import BaseEditor\n\nclass DatabaseSchemaEditor(BaseEditor):\n    sql_create = \"x\"\n".to_string()),
            ("d/gis.py", "from c.lite import DatabaseSchemaEditor\n\nclass GisEditor(DatabaseSchemaEditor):\n    def drop(self):\n        return self.execute(\"drop\")\n".to_string()),
        ],
        "GisEditor::drop",
    );
    assert!(!out.contains(&"DatabaseSchemaEditor::execute@b/pg.py".to_string()), "{out:?}");
}

#[test]
fn python_the_first_base_listed_that_has_the_member_is_the_one_self_reaches() {
    let files = [(
        "mixins.py",
        "class Second:\n    def describe(self):\n        return \"second\"\n\nclass First:\n    def describe(self):\n        return \"first\"\n\n\
         class Both(First, Second):\n    def show(self):\n        return self.describe()\n\n\
         class Other(Second, First):\n    def show(self):\n        return self.describe()\n"
            .to_string(),
    )];
    assert_eq!(callees_in(&files, "Both::show"), ["First::describe@mixins.py"]);
    assert_eq!(callees_in(&files, "Other::show"), ["Second::describe@mixins.py"]);
}

#[test]
fn java_a_class_further_up_that_implements_the_member_wins_over_an_interface_that_only_declares_it() {
    let out = callees_in(
        &[
            ("p/Rotation.java", "package p;\ninterface Rotation {\n  String current();\n}\n".to_string()),
            ("p/Dispatcher.java", "package p;\nclass Dispatcher {\n  String current() { return \"d\"; }\n}\n".to_string()),
            ("p/Contextual.java", "package p;\nclass Contextual extends Dispatcher {\n}\n".to_string()),
            ("p/RotationButton.java", "package p;\nclass RotationButton extends Contextual implements Rotation {\n  String show() { return current(); }\n}\n".to_string()),
        ],
        "p::RotationButton::show",
    );
    assert_eq!(out, ["p::Dispatcher::current@p/Dispatcher.java"]);
}

#[test]
fn java_a_variable_named_parent_is_not_super() {
    // The extractor drops the `parent.` receiver, so the call arrives as a bare `getName()`; it
    // must not be read as `super.getName()`.
    let out = callees_in(
        &[
            ("p/Base.java", "package p;\nclass Base {\n  String getName() { return \"base\"; }\n}\n".to_string()),
            (
                "p/Child.java",
                "package p;\nclass Node {\n  String getName() { return \"node\"; }\n}\nclass Child extends Base {\n  Node parent;\n  String getName() { return \"child\"; }\n  String show() { return parent.getName(); }\n}\n".to_string(),
            ),
        ],
        "p::Child::show",
    );
    assert!(!out.contains(&"p::Base::getName@p/Base.java".to_string()), "{out:?}");
}

#[test]
fn cpp_a_call_in_a_namespace_takes_the_overload_its_arguments_fit_not_the_one_in_its_own_file() {
    let out = callees_in(
        &[
            ("core.h", "#pragma once\nnamespace detail {\ntemplate <typename T, typename In, typename Out> Out copy(In b, In e, Out out) { return out; }\n}\n".to_string()),
            (
                "format.h",
                "#pragma once\n#include \"core.h\"\nnamespace detail {\ntemplate <typename T, typename V, typename Out> Out copy(V s, Out out) { return out; }\ntemplate <typename Char, typename Out> Out write_it(Out out, const Char* b, const Char* e) {\n  return copy<Char>(b, e, out);\n}\n}\n".to_string(),
            ),
        ],
        "detail::write_it",
    );
    assert_eq!(out, ["detail::copy@core.h"]);
}
