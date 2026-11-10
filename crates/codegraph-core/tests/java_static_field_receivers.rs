//! A Java call on a value a type holds in a static field reaches the method of
//! that field's type (upstream bc9291af, #3055, thanks @mixxer).
//!
//! `Consts.HELPER.go()`, `Holder.SHARED.run()`, `Outer.Inner.NESTED.run()`, an
//! enum constant's `Color.RED.hex()` and a `CREATOR`'s `createFromParcel` are
//! typed by the field's own declaration, read with that field's file's imports:
//! the method an anonymous class declares, else the class a `new X()` builds,
//! else the declared type. A field the type inherits counts. An interface's
//! constants are indexed. A field of a library type, such as
//! `Intent.ACTION_VIEW.equals`, links nothing.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

struct Fixture {
    cg: CodeGraph,
    _root: tempfile::TempDir,
}

fn index() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let files: &[(&str, &str)] = &[
        (
            "r/Consts.java",
            "package r;\npublic interface Consts {\n    int MAX = 10;\n    Helper HELPER = new Helper();\n    String NAME = \"x\";\n}\n",
        ),
        ("r/Helper.java", "package r;\npublic class Helper { public void go() {} }\n"),
        (
            "r/Holder.java",
            "package r;\npublic class Holder {\n    public static final Helper SHARED = new Helper();\n}\n",
        ),
        (
            "r/User.java",
            "package r;\npublic class User {\n    void viaInterface() { Consts.HELPER.go(); }\n    void viaClass() { Holder.SHARED.go(); }\n}\n",
        ),
        (
            "com/lib/ProjectInterpolator.java",
            "package com.lib;\npublic class ProjectInterpolator { public float getInterpolation(float t) { return t; } }\n",
        ),
        (
            "p/ProjectInterpolator.java",
            "package p;\npublic class ProjectInterpolator { public float getInterpolation(float t) { return t; } }\n",
        ),
        (
            "r/ProjectInterpolators.java",
            "package r;\nimport p.ProjectInterpolator;\nclass ProjectInterpolators { static final ProjectInterpolator LOCAL = new ProjectInterpolator(); }\n",
        ),
        (
            "r/Consumer.java",
            "package r;\npublic class Consumer {\n    float projectCall() { return ProjectInterpolators.LOCAL.getInterpolation(1); }\n}\n",
        ),
        ("s/Shapes.java", SHAPES),
        ("s/KObj.kt", "package s\n\nobject KObj {\n    fun go() {}\n}\n\nclass Other {\n    fun equals(x: String): Boolean = true\n}\n"),
    ];
    for (relative, content) in files {
        let path = root.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let cg = CodeGraph::init_and_index(root.path().to_str().unwrap(), None).unwrap();
    Fixture { cg, _root: root }
}

/// The `new Parcelable.Creator` that starts the anonymous class is line 39.
const SHAPES: &str = "\
package s;

import android.os.Parcel;
import android.os.Parcelable;
import android.content.Intent;
import android.view.animation.Interpolators;

class Parent {
    public static final Runner SHARED = new Runner();
}

class Child extends Parent {}

class Runner { public void run() {} public String getName() { return \"r\"; } }

class FastRunner extends Runner { @Override public void run() {} }

interface Defaults {
    Runner DEFAULT = new FastRunner();
}

class Impl implements Defaults {}

class Outer {
    static class Inner {
        static final Runner NESTED = new Runner();
    }
}

enum Color {
    RED,
    GREEN {
        @Override String hex() { return \"0f0\"; }
    };
    String hex() { return \"f00\"; }
}

class Foo implements Parcelable {
    public static final Parcelable.Creator<Foo> CREATOR = new Parcelable.Creator<Foo>() {
        public Foo createFromParcel(Parcel in) { return new Foo(); }
        public Foo[] newArray(int size) { return new Foo[size]; }
    };
}

class Calls {
    void inherited() { Child.SHARED.run(); }
    void interfaceConstant() { Impl.DEFAULT.run(); }
    void nested() { Outer.Inner.NESTED.run(); }
    void enumConstant() { Color.RED.hex(); }
    void enumBody() { Color.GREEN.hex(); }
    void anonymous(Parcel p) { Foo.CREATOR.createFromParcel(p); }
    boolean library(String a) { return Intent.ACTION_VIEW.equals(a); }
    float libraryField(float t) { return Interpolators.FAST_OUT_SLOW_IN.getInterpolation(t); }
    String javaLang() { return Integer.TYPE.getName(); }
    void kotlinObject() { KObj.INSTANCE.go(); }
}
";

impl Fixture {
    fn constants(&self, file: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .cg
            .get_nodes_by_file(file)
            .unwrap()
            .into_iter()
            .filter(|n| n.kind == NodeKind::Constant)
            .map(|n| n.qualified_name)
            .collect();
        names.sort();
        names
    }

    fn callees(&self, owner: &str, method: &str) -> Vec<String> {
        let mut methods = self.cg.get_nodes_by_kind(NodeKind::Method).unwrap();
        methods.sort_by(|a, b| (&a.file_path, a.start_line).cmp(&(&b.file_path, b.start_line)));
        let suffix = format!("{owner}::{method}");
        let Some(node) = methods
            .iter()
            .find(|n| n.name == method && n.qualified_name.ends_with(&suffix))
        else {
            return vec![format!("<no method {suffix}>")];
        };
        let mut out: Vec<String> = self
            .cg
            .get_outgoing_edges(&node.id)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .filter_map(|e| self.cg.get_node(&e.target).unwrap())
            .map(|n| n.qualified_name)
            .collect();
        out.sort();
        out
    }
}

#[derive(Default)]
struct Checks(Vec<String>);

impl Checks {
    fn is(&mut self, case: &str, actual: Vec<String>, expected: &[&str]) {
        if actual != expected {
            self.0.push(format!(
                "{case}\n    expected {expected:?}\n    actual   {actual:?}"
            ));
        }
    }

    fn finish(self) {
        assert!(
            self.0.is_empty(),
            "{} case(s) failed:\n  {}",
            self.0.len(),
            self.0.join("\n  ")
        );
    }
}

#[test]
fn indexes_an_interface_constant() {
    let f = index();
    let mut c = Checks::default();
    c.is(
        "interface constants",
        f.constants("r/Consts.java"),
        &["r::Consts::HELPER", "r::Consts::MAX", "r::Consts::NAME"],
    );
    c.finish();
}

#[test]
fn calls_the_method_of_the_fields_type() {
    let f = index();
    let mut c = Checks::default();
    c.is(
        "via interface",
        f.callees("User", "viaInterface"),
        &["r::Helper::go"],
    );
    c.is(
        "via class",
        f.callees("User", "viaClass"),
        &["r::Helper::go"],
    );
    c.is(
        "field file import",
        f.callees("Consumer", "projectCall"),
        &["p::ProjectInterpolator::getInterpolation"],
    );
    c.is(
        "inherited field",
        f.callees("Calls", "inherited"),
        &["s::Runner::run"],
    );
    c.is(
        "interface constant subtype",
        f.callees("Calls", "interfaceConstant"),
        &["s::FastRunner::run"],
    );
    c.is(
        "nested type",
        f.callees("Calls", "nested"),
        &["s::Runner::run"],
    );
    c.is(
        "enum constant",
        f.callees("Calls", "enumConstant"),
        &["s::Color::hex"],
    );
    c.is(
        "enum body",
        f.callees("Calls", "enumBody"),
        &["s::Color::GREEN::hex"],
    );
    c.is(
        "anonymous creator",
        f.callees("Calls", "anonymous"),
        &["s::Foo::CREATOR::<Creator$anon@39>::createFromParcel"],
    );
    c.is(
        "kotlin instance",
        f.callees("Calls", "kotlinObject"),
        &["s::KObj::go"],
    );
    c.finish();
}

#[test]
fn library_field_links_nothing() {
    let f = index();
    let mut c = Checks::default();
    c.is("library constant", f.callees("Calls", "library"), &[]);
    c.is("library field", f.callees("Calls", "libraryField"), &[]);
    c.is("java.lang field", f.callees("Calls", "javaLang"), &[]);
    c.finish();
}
