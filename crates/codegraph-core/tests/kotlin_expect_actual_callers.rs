//! A Kotlin Multiplatform import reaches what the caller compiles against: common code the
//! `expect`, a platform its `actual`, Java the JVM `actual` (upstream 59bfe6d7; fixture from
//! kotlin-expect-actual-callers.test.ts).
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

const FILES: &[(&str, &str)] = &[
    ("lib/src/commonMain/kotlin/a/Util.kt", r#"package a

expect object Util {
    fun go()
}
"#),
    ("lib/src/androidMain/kotlin/a/Util.kt", r#"package a

actual object Util {
    @JvmStatic
    actual fun go() {}
}
"#),
    ("lib/src/iosMain/kotlin/a/Util.kt", r#"package a

actual object Util {
    actual fun go() {}
}
"#),
    ("lib/src/commonMain/kotlin/t/Timber.kt", r#"package t

expect class Timber {
    class Tree
    companion object {
        fun plant(tree: Tree)
    }
}
"#),
    ("lib/src/androidMain/kotlin/t/Timber.kt", r#"package t

actual class Timber {
    actual class Tree
    actual companion object {
        @JvmStatic
        actual fun plant(tree: Tree) {}
    }
}
"#),
    ("lib/src/commonMain/kotlin/n/OnlyNative.kt", r#"package n

expect object OnlyNative {
    fun go()
}
"#),
    ("lib/src/iosMain/kotlin/n/OnlyNative.kt", r#"package n

actual object OnlyNative {
    actual fun go() {}
}
"#),
    ("lib/src/commonJvmAndroid/kotlin/s/Version.kt", r#"package s

expect object Version {
    fun name(): String
}
"#),
    ("lib/src/androidMain/kotlin/s/Version.android.kt", r#"package s

actual object Version {
    actual fun name() = "android"
}
"#),
    ("lib/src/jvmMain/kotlin/s/Version.jvm.kt", r#"package s

actual object Version {
    actual fun name() = "jvm"
}
"#),
    ("samples/survey/src/main/kotlin/e/Clients.kt", r#"package e

import s.Version

fun jvmSample() = Version.name()
"#),
    ("android-test/src/main/kotlin/e/Screen.kt", r#"package e

import s.Version

fun androidApp() = Version.name()
"#),
    ("lib/src/commonJvmAndroid/kotlin/e/Shared.kt", r#"package e

import s.Version

fun sharedCode() = Version.name()
"#),
    ("app/src/main/java/b/App.java", r#"package b;

import a.Util;
import t.Timber;

class App {
    void run() { Util.go(); }
    void grow() { Timber.plant(new Timber.Tree()); }
}

class Log extends Timber.Tree {
}
"#),
    ("app/src/main/java/b/Native.java", r#"package b;

import n.OnlyNative;

class Native {
    void run() { OnlyNative.go(); }
}
"#),
    ("app/src/main/kotlin/d/Plain.kt", r#"package d

import a.Util

fun plainModule() = Util.go()
"#),
    ("lib/src/commonMain/kotlin/c/Common.kt", r#"package c

import a.Util

fun inCommonMain() = Util.go()
"#),
    ("lib/src/commonTest/kotlin/c/CommonTest.kt", r#"package c

import a.Util

fun inCommonTest() = Util.go()
"#),
    ("lib/src/androidMain/kotlin/c/Android.kt", r#"package c

import a.Util

fun inAndroidMain() = Util.go()
"#),
    ("lib/src/androidUnitTest/kotlin/c/AndroidTest.kt", r#"package c

import a.Util

fun inAndroidUnitTest() = Util.go()
"#),
    ("lib/src/iosMain/kotlin/c/Ios.kt", r#"package c

import a.Util

fun inIosMain() = Util.go()
"#),
    ("lib/src/iosArm64Main/kotlin/c/Arm.kt", r#"package c

import a.Util

fun inIosArm64Main() = Util.go()
"#),
    ("lib/src/nativeMain/kotlin/c/Native.kt", r#"package c

import a.Util

fun inNativeMain() = Util.go()
"#),
];

const COMMON: &str = "lib/src/commonMain/kotlin/a/Util.kt";
const ANDROID: &str = "lib/src/androidMain/kotlin/a/Util.kt";
const IOS: &str = "lib/src/iosMain/kotlin/a/Util.kt";

#[test]
fn a_kotlin_multiplatform_import_reaches_what_the_caller_compiles_against() {
    let dir = tempfile::tempdir().unwrap();
    for (rel, text) in FILES {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let mut callables = cg.get_nodes_by_kind(NodeKind::Function).unwrap();
    callables.extend(cg.get_nodes_by_kind(NodeKind::Method).unwrap());
    let targets = |id: &str, kind: EdgeKind| -> Vec<codegraph_types::Node> {
        cg.get_outgoing_edges(id).unwrap().into_iter().filter(|e| e.kind == kind).filter_map(|e| cg.get_node(&e.target).unwrap()).collect()
    };
    let calls_from = |name: &str, callee: &str| -> Vec<String> {
        let from = callables.iter().find(|n| n.name == name).unwrap_or_else(|| panic!("no {name}"));
        let mut out: Vec<String> = targets(&from.id, EdgeKind::Calls).into_iter().filter(|n| n.name == callee).map(|n| n.file_path).collect();
        out.sort();
        out
    };
    let mut failed: Vec<String> = Vec::new();
    let mut eq = |label: &str, got: Vec<String>, want: &[&str]| {
        if got != want.iter().map(|s| s.to_string()).collect::<Vec<_>>() {
            failed.push(format!("{label}: got {got:?}, want {want:?}"));
        }
    };
    let run = calls_from("run", "go");
    eq("run has android, no common", vec![(run.contains(&ANDROID.to_string()) && !run.contains(&COMMON.to_string())).to_string()], &["true"]);
    eq("grow", calls_from("grow", "plant"), &["lib/src/androidMain/kotlin/t/Timber.kt"]);
    let log = cg.get_nodes_by_kind(NodeKind::Class).unwrap().into_iter().find(|n| n.name == "Log").expect("Log");
    eq("Log extends", targets(&log.id, EdgeKind::Extends).into_iter().map(|n| format!("{} {}", n.name, n.file_path)).collect(), &["Tree lib/src/androidMain/kotlin/t/Timber.kt"]);
    let app = cg.get_nodes_by_file("app/src/main/java/b/App.java").unwrap().into_iter().find(|n| n.kind == NodeKind::Namespace).expect("App namespace");
    let mut imported: Vec<String> = targets(&app.id, EdgeKind::Imports).into_iter().map(|n| n.file_path).collect();
    imported.sort();
    eq("App imports", imported, &[ANDROID, "lib/src/androidMain/kotlin/t/Timber.kt"]);
    let native = cg.get_nodes_by_file("app/src/main/java/b/Native.java").unwrap().into_iter().find(|n| n.name == "run").expect("Native.run");
    let native_calls: Vec<String> = targets(&native.id, EdgeKind::Calls).into_iter().map(|n| n.file_path).collect();
    eq("Native only actual", vec![native_calls.contains(&"lib/src/iosMain/kotlin/n/OnlyNative.kt".to_string()).to_string()], &["true"]);
    eq("plainModule", calls_from("plainModule", "go"), &[ANDROID]);
    eq("inCommonMain", calls_from("inCommonMain", "go"), &[COMMON]);
    eq("inCommonTest", calls_from("inCommonTest", "go"), &[COMMON]);
    eq("inAndroidMain", calls_from("inAndroidMain", "go"), &[ANDROID]);
    eq("inAndroidUnitTest", calls_from("inAndroidUnitTest", "go"), &[ANDROID]);
    eq("inIosMain", calls_from("inIosMain", "go"), &[IOS]);
    eq("jvmSample", calls_from("jvmSample", "name"), &["lib/src/jvmMain/kotlin/s/Version.jvm.kt"]);
    eq("androidApp", calls_from("androidApp", "name"), &["lib/src/androidMain/kotlin/s/Version.android.kt"]);
    eq("sharedCode", calls_from("sharedCode", "name"), &["lib/src/commonJvmAndroid/kotlin/s/Version.kt"]);
    eq("inIosArm64Main", calls_from("inIosArm64Main", "go"), &[IOS]);
    eq("inNativeMain", calls_from("inNativeMain", "go"), &[COMMON]);
    assert!(failed.is_empty(), "{} case(s) failed:\n{}", failed.len(), failed.join("\n"));
    cg.close();
}
