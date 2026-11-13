//! A Python `from` import of names a module in a folder above the script defines
//! links that module (upstream 0f93575c, #3141). The nearest such module wins.
//! A module a project root provides still wins. A plain import, a star import, a
//! missing name, a sibling folder, and a package above the script stay unlinked.
//! An edit that adds or removes those names links or unlinks the importers.
use codegraph_core::CodeGraph;
use codegraph_types::EdgeKind;
use std::fs;
use std::path::Path;

fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (relative, content) in files {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

fn index(files: &[(&str, &str)]) -> (tempfile::TempDir, CodeGraph) {
    let root = tempfile::tempdir().unwrap();
    write_files(root.path(), files);
    let cg = CodeGraph::init_and_index(root.path().to_str().unwrap(), None).unwrap();
    (root, cg)
}

fn edges_of(cg: &CodeGraph) -> Vec<String> {
    let nodes = cg.store().get_all_nodes().unwrap();
    let mut out = Vec::new();
    for node in &nodes {
        if !node.file_path.ends_with(".py") {
            continue;
        }
        for edge in cg.get_outgoing_edges(&node.id).unwrap() {
            let kind = match edge.kind {
                EdgeKind::Calls => "calls",
                EdgeKind::Imports => "imports",
                EdgeKind::Extends => "extends",
                _ => continue,
            };
            let Some(target) = cg.get_node(&edge.target).unwrap() else { continue };
            out.push(format!(
                "{kind} {}@{} -> {}@{}",
                node.qualified_name, node.file_path, target.qualified_name, target.file_path
            ));
        }
    }
    out.sort();
    out.dedup();
    out
}

fn copy_sources(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() == ".codegraphV" {
            continue;
        }
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir_all(&dest).unwrap();
            copy_sources(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}

fn fresh_edges(root: &Path) -> Vec<String> {
    let copy = tempfile::tempdir().unwrap();
    copy_sources(root, copy.path());
    let cg = CodeGraph::init_and_index(copy.path().to_str().unwrap(), None).unwrap();
    let edges = edges_of(&cg);
    cg.close();
    edges
}

const RUNNER: &str = "class TestCase:\n    def run(self):\n        return 1\n\n\ndef generate_vector(n):\n    return [0] * n\n";

#[test]
fn binds_names_from_the_nearest_parent_and_a_root_wins() {
    let (_root, cg) = index(&[
        ("modules/vs/test.py", RUNNER),
        (
            "modules/vs/tests/with_a.py",
            "from test import TestCase, generate_vector\n\n\nclass WithA(TestCase):\n    pass\n\n\ndef go():\n    return generate_vector(3)\n\n\ndef inherited():\n    WithA.run(None)\n",
        ),
    ]);
    let got = edges_of(&cg);
    let mut with_a: Vec<_> = got.into_iter().filter(|e| e.contains("with_a.py")).collect();
    with_a.sort();
    assert_eq!(
        with_a,
        vec![
            "calls go@modules/vs/tests/with_a.py -> generate_vector@modules/vs/test.py".to_string(),
            "calls inherited@modules/vs/tests/with_a.py -> TestCase::run@modules/vs/test.py".to_string(),
            "extends WithA@modules/vs/tests/with_a.py -> TestCase@modules/vs/test.py".to_string(),
            "imports modules/vs/tests/with_a.py@modules/vs/tests/with_a.py -> TestCase@modules/vs/test.py".to_string(),
            "imports modules/vs/tests/with_a.py@modules/vs/tests/with_a.py -> generate_vector@modules/vs/test.py".to_string(),
            "imports modules/vs/tests/with_a.py@modules/vs/tests/with_a.py -> modules/vs/test.py@modules/vs/test.py".to_string(),
        ]
    );
    cg.close();

    let (_root, cg) = index(&[
        ("tools/helper.py", "def assist(n):\n    return n\n"),
        ("tools/sub/helper.py", "def other(n):\n    return n\n"),
        ("tools/sub/deep/er/run.py", "from helper import assist\n\n\ndef go():\n    return assist(1)\n"),
        ("lib/a/common.py", "def shared():\n    return 1\n"),
        ("lib/common.py", "def shared():\n    return 2\n"),
        ("lib/a/b/use.py", "from common import shared\n\n\ndef go2():\n    return shared()\n"),
    ]);
    let got = edges_of(&cg);
    assert!(
        got.iter().any(|e| e == "calls go@tools/sub/deep/er/run.py -> assist@tools/helper.py"),
        "{got:?}"
    );
    assert!(
        got.iter().any(|e| e == "calls go2@lib/a/b/use.py -> shared@lib/a/common.py"),
        "{got:?}"
    );
    assert!(!got.iter().any(|e| e.contains("shared@lib/common.py")), "{got:?}");
    cg.close();

    let (_root, cg) = index(&[
        ("helper.py", "def assist(n):\n    return \"root\"\n"),
        ("tools/helper.py", "def assist(n):\n    return \"above\"\n"),
        ("tools/sub/run.py", "from helper import assist\n\n\ndef go():\n    return assist(1)\n"),
    ]);
    let got = edges_of(&cg);
    assert!(
        got.iter().any(|e| e == "calls go@tools/sub/run.py -> assist@helper.py"),
        "{got:?}"
    );
    assert!(!got.iter().any(|e| e.contains("assist@tools/helper.py")), "{got:?}");
    cg.close();
}

#[test]
fn stays_unlinked_without_every_imported_name() {
    let cases: &[(&str, &[(&str, &str)])] = &[
        (
            "missing-name",
            &[
                ("tools/time.py", "def clock():\n    return 0\n"),
                ("tools/sub/wait.py", "from time import sleep\n\n\ndef nap():\n    sleep(1)\n"),
                ("tools/helper.py", "def assist(n):\n    return n\n"),
                ("tools/sub/partly.py", "from helper import assist, missing\n\n\ndef go():\n    return assist(missing)\n"),
            ],
        ),
        (
            "plain-and-star",
            &[
                ("tools/utils.py", "def assist(n):\n    return n\n"),
                ("tools/sub/plain.py", "import utils\n\n\ndef go():\n    return utils.assist(1)\n"),
                ("tools/helper.py", "def assist(n):\n    return n\n"),
                ("tools/sub/star.py", "from helper import *\n\n\ndef go():\n    return assist(1)\n"),
            ],
        ),
        (
            "sibling-and-package",
            &[
                ("tools/other/helper.py", "def assist(n):\n    return n\n"),
                ("tools/sub/run.py", "from helper import assist\n\n\ndef go():\n    return assist(1)\n"),
                ("pkg/__init__.py", ""),
                ("pkg/shared.py", "def share(n):\n    return n\n"),
                ("pkg/scripts/run.py", "from shared import share\n\n\ndef go2():\n    return share(1)\n"),
            ],
        ),
    ];
    for (label, files) in cases {
        let (_root, cg) = index(files);
        let got = edges_of(&cg);
        // A star import records no names, so `assist()` can still reach the one
        // function of that name. The import itself must stay unresolved.
        let linked: Vec<_> = got
            .iter()
            .filter(|e| {
                if e.contains("star.py") {
                    return e.starts_with("imports ") || e.starts_with("extends ");
                }
                e.contains("wait.py")
                    || e.contains("partly.py")
                    || e.contains("plain.py")
                    || e.contains("tools/sub/run.py")
                    || e.contains("pkg/scripts/run.py")
            })
            .cloned()
            .collect();
        assert!(linked.is_empty(), "{label}: {linked:?}");
        cg.close();
    }
}

#[test]
fn sync_links_and_unlinks_the_parent_module() {
    use codegraph_core::IndexOptions;
    let importer = "from helper import assist\n\n\ndef go():\n    return assist(1)\n";
    let root = tempfile::tempdir().unwrap();
    write_files(root.path(), &[("tools/sub/run.py", importer), ("tools/other.py", "def other():\n    return 0\n")]);
    let mut cg = CodeGraph::init_and_index(root.path().to_str().unwrap(), None).unwrap();
    assert!(!edges_of(&cg).iter().any(|e| e.contains("assist")));

    write_files(root.path(), &[("tools/helper.py", "def assist(n):\n    return n\n")]);
    cg.sync(&IndexOptions::default(), None).unwrap();
    let synced = edges_of(&cg);
    assert_eq!(synced, fresh_edges(root.path()));
    assert!(synced.iter().any(|e| e == "calls go@tools/sub/run.py -> assist@tools/helper.py"));

    fs::remove_file(root.path().join("tools/helper.py")).unwrap();
    cg.sync(&IndexOptions::default(), None).unwrap();
    let synced = edges_of(&cg);
    assert_eq!(synced, fresh_edges(root.path()));
    assert!(!synced.iter().any(|e| e.contains("assist")));
    cg.close();

    let root = tempfile::tempdir().unwrap();
    write_files(
        root.path(),
        &[("tools/sub/run.py", importer), ("tools/helper.py", "def other():\n    return 0\n")],
    );
    let mut cg = CodeGraph::init_and_index(root.path().to_str().unwrap(), None).unwrap();
    assert!(!edges_of(&cg).iter().any(|e| e.contains("assist")));

    write_files(root.path(), &[("tools/helper.py", "def assist(n):\n    return n\n")]);
    cg.sync(&IndexOptions::default(), None).unwrap();
    let synced = edges_of(&cg);
    assert_eq!(synced, fresh_edges(root.path()));
    assert!(synced.iter().any(|e| e == "calls go@tools/sub/run.py -> assist@tools/helper.py"));

    write_files(root.path(), &[("tools/helper.py", "def other():\n    return 0\n")]);
    cg.sync(&IndexOptions::default(), None).unwrap();
    let synced = edges_of(&cg);
    assert_eq!(synced, fresh_edges(root.path()));
    assert!(!synced.iter().any(|e| e.contains("assist")));
    cg.close();
}
