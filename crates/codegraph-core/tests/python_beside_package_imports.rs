//! A Python `from` import of names a module beside the script defines, in a folder
//! that holds an `__init__.py`, links that module (upstream 0576044b, #3155).
//! A module a project root provides still wins. A stdlib name, a module that
//! imports its own name, a plain import, a missing name, and a package above the
//! file stay unlinked. Sync follows an edit of that module.
use codegraph_core::{CodeGraph, IndexOptions};
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

fn touching(edges: &[String], file: &str) -> Vec<String> {
    edges.iter().filter(|e| e.contains(file)).cloned().collect()
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

const PYSIM: &str = "\
class HashTable:
    def size(self):
        return 0


def run(n):
    return n


kLimit = 3
";

#[test]
fn binds_the_module_beside_a_script_in_a_package_folder() {
    let (_root, cg) = index(&[
        ("tools/an/__init__.py", ""),
        ("tools/an/pysim.py", PYSIM),
        (
            "tools/an/pysim_test.py",
            "from pysim import HashTable, kLimit, run\n\n\ndef test_all():\n    table = HashTable()\n    return run(kLimit)\n",
        ),
    ]);
    let got = touching(&edges_of(&cg), "pysim_test.py");
    assert!(
        got.iter().any(|e| e == "calls test_all@tools/an/pysim_test.py -> run@tools/an/pysim.py"),
        "{got:?}"
    );
    for name in ["HashTable", "kLimit", "run", "pysim.py"] {
        assert!(
            got.iter().any(|e| e.starts_with("imports ") && e.contains(&format!("{name}@tools/an/pysim.py"))),
            "{name} missing in {got:?}"
        );
    }
    cg.close();

    let (_root, cg) = index(&[
        ("utils.py", "def load(n):\n    return \"top\"\n"),
        ("pkg/__init__.py", ""),
        ("pkg/utils.py", "def load(n):\n    return \"beside\"\n"),
        ("pkg/mod.py", "from utils import load\n\n\ndef go():\n    return load(1)\n"),
    ]);
    let got = edges_of(&cg);
    assert!(got.iter().any(|e| e == "calls go@pkg/mod.py -> load@utils.py"), "{got:?}");
    assert!(!got.iter().any(|e| e.contains("load@pkg/utils.py")), "{got:?}");
    cg.close();

    let (_root, cg) = index(&[
        ("pkg/__init__.py", ""),
        ("pkg/yaml.py", "def dump(x):\n    return x\n"),
        ("pkg/mod.py", "from yaml import dump, safe_load\n\n\ndef go():\n    return safe_load(dump(1))\n"),
    ]);
    assert!(touching(&edges_of(&cg), "pkg/mod.py").is_empty());
    cg.close();

    let (_root, cg) = index(&[
        ("pkg/__init__.py", ""),
        (
            "pkg/client.py",
            "from client import Client\n\n\nclass Client(Client):\n    pass\n\n\ndef make():\n    return Client()\n",
        ),
    ]);
    assert!(touching(&edges_of(&cg), "pkg/client.py").iter().all(|e| !e.contains("imports ")), "{:?}", edges_of(&cg));
    cg.close();

    let (_root, cg) = index(&[
        ("pkg/__init__.py", ""),
        ("pkg/yaml.py", "import yaml\n\n\ndef load(text):\n    return yaml.safe_load(text)\n"),
        ("pkg/mod.py", "from yaml import load\n\n\ndef go():\n    return load(\"a: 1\")\n"),
    ]);
    assert!(touching(&edges_of(&cg), "pkg/mod.py").is_empty());
    cg.close();

    for (module, importer) in [("pkg/html.py", "pkg/text.py"), ("tools/html.py", "tools/sub/text.py")] {
        let other = if module.starts_with("pkg/") { "pkg/other.py" } else { "tools/other.py" };
        let init = if module.starts_with("pkg/") { vec![("pkg/__init__.py", "")] } else { vec![] };
        let mut files = init;
        files.push((other, ""));
        files.push((module, "def escape(text):\n    return text\n"));
        files.push((importer, "from html import escape\n\n\ndef go():\n    return escape(\"a\")\n"));
        let (_root, cg) = index(&files);
        assert!(touching(&edges_of(&cg), importer).is_empty(), "{importer}");
        cg.close();
    }

    let (_root, cg) = index(&[
        ("pkg/__init__.py", ""),
        ("pkg/sub/__init__.py", ""),
        ("pkg/sub/helper.py", "def assist(n):\n    return n\n"),
        ("pkg/run.py", "from sub.helper import assist\n\n\ndef go():\n    return assist(1)\n"),
    ]);
    let got = edges_of(&cg);
    assert!(
        got.iter().any(|e| e == "calls go@pkg/run.py -> assist@pkg/sub/helper.py"),
        "{got:?}"
    );
    cg.close();

    let (_root, cg) = index(&[
        ("pkg/__init__.py", ""),
        ("pkg/helper.py", "def assist(n):\n    return n\n"),
        ("pkg/mod.py", "import helper\n\n\ndef go():\n    return helper.assist(1)\n"),
    ]);
    assert!(touching(&edges_of(&cg), "pkg/mod.py").is_empty());
    cg.close();

    let (_root, cg) = index(&[
        ("pkg/__init__.py", ""),
        ("pkg/helper.py", "def assist(n):\n    return n\n"),
        ("pkg/sub/__init__.py", ""),
        ("pkg/sub/mod.py", "from helper import assist\n\n\ndef go():\n    return assist(1)\n"),
    ]);
    assert!(touching(&edges_of(&cg), "pkg/sub/mod.py").is_empty());
    cg.close();
}

#[test]
fn sync_follows_an_edit_of_the_module_beside_the_script() {
    let importer = "from helper import assist\n\n\ndef go():\n    return assist(1)\n";
    let root = tempfile::tempdir().unwrap();
    write_files(
        root.path(),
        &[("pkg/__init__.py", ""), ("pkg/run.py", importer), ("pkg/helper.py", "def other():\n    return 0\n")],
    );
    let mut cg = CodeGraph::init_and_index(root.path().to_str().unwrap(), None).unwrap();
    assert!(!edges_of(&cg).iter().any(|e| e.contains("assist")));

    write_files(root.path(), &[("pkg/helper.py", "def assist(n):\n    return n\n")]);
    cg.sync(&IndexOptions::default(), None).unwrap();
    let synced = edges_of(&cg);
    assert_eq!(synced, fresh_edges(root.path()));
    assert!(synced.iter().any(|e| e == "calls go@pkg/run.py -> assist@pkg/helper.py"));

    write_files(root.path(), &[("pkg/helper.py", "def other():\n    return 0\n")]);
    cg.sync(&IndexOptions::default(), None).unwrap();
    let synced = edges_of(&cg);
    assert_eq!(synced, fresh_edges(root.path()));
    assert!(!synced.iter().any(|e| e.contains("assist")));
    cg.close();

    let root = tempfile::tempdir().unwrap();
    write_files(root.path(), &[("pkg/__init__.py", ""), ("pkg/run.py", importer)]);
    let mut cg = CodeGraph::init_and_index(root.path().to_str().unwrap(), None).unwrap();
    write_files(root.path(), &[("pkg/helper.py", "def assist(n):\n    return n\n")]);
    cg.sync(&IndexOptions::default(), None).unwrap();
    let synced = edges_of(&cg);
    assert_eq!(synced, fresh_edges(root.path()));
    assert!(synced.iter().any(|e| e == "calls go@pkg/run.py -> assist@pkg/helper.py"));
    fs::remove_file(root.path().join("pkg/helper.py")).unwrap();
    cg.sync(&IndexOptions::default(), None).unwrap();
    let synced = edges_of(&cg);
    assert_eq!(synced, fresh_edges(root.path()));
    assert!(!synced.iter().any(|e| e.contains("assist")));
    cg.close();

    for (line, module_file, other_file) in [
        ("from sub.helper import assist", "pkg/sub/helper.py", "pkg/sub/__init__.py"),
        ("from sub import assist", "pkg/sub/__init__.py", "pkg/sub/mod.py"),
        ("from lib.helper import assist", "tools/lib/helper.py", "tools/lib/other.py"),
        ("from x.lib import assist", "tools/x/lib/__init__.py", "tools/x/lib/other.py"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let run = format!("{line}\n\n\ndef go():\n    return assist(1)\n");
        let run_path = if line.contains("lib.helper") || line.contains("x.lib") { "tools/sub/run.py" } else { "pkg/run.py" };
        let mut files: Vec<(&str, &str)> = vec![(run_path, run.as_str()), (other_file, ""), (module_file, "def other():\n    return 0\n")];
        if run_path.starts_with("pkg/") {
            files.push(("pkg/__init__.py", ""));
        }
        write_files(root.path(), &files);
        let mut cg = CodeGraph::init_and_index(root.path().to_str().unwrap(), None).unwrap();
        assert!(!edges_of(&cg).iter().any(|e| e.contains("assist")), "{line}");

        write_files(root.path(), &[(module_file, "def assist(n):\n    return n\n")]);
        cg.sync(&IndexOptions::default(), None).unwrap();
        let synced = edges_of(&cg);
        assert_eq!(synced, fresh_edges(root.path()), "{line}");
        assert!(
            synced.iter().any(|e| e == &format!("calls go@{run_path} -> assist@{module_file}")),
            "{line}: {synced:?}"
        );

        write_files(root.path(), &[(module_file, "def other():\n    return 0\n")]);
        cg.sync(&IndexOptions::default(), None).unwrap();
        let synced = edges_of(&cg);
        assert_eq!(synced, fresh_edges(root.path()), "{line}");
        assert!(!synced.iter().any(|e| e.contains("assist")), "{line}");
        cg.close();
    }

    // A relative import already resolves inside the package. An importer in
    // another folder, a TypeScript import, and a function of the module's name
    // do not take the script-folder link. The graph after the edit matches a
    // fresh index.
    let root = tempfile::tempdir().unwrap();
    write_files(
        root.path(),
        &[
            ("pkg/__init__.py", ""),
            ("pkg/helper.py", "def assist(n):\n    return n\n"),
            ("pkg/run.py", "from .helper import assist\n\n\ndef go():\n    return assist(1)\n"),
            ("other/run.py", "from helper import assist\n\n\ndef go():\n    return assist(1)\n"),
            ("pkg/widget.ts", "import { assist } from 'helper';\nexport const go = assist;\n"),
            ("pkg/named.py", "def helper():\n    return 0\n"),
        ],
    );
    let mut cg = CodeGraph::init_and_index(root.path().to_str().unwrap(), None).unwrap();
    write_files(root.path(), &[("pkg/helper.py", "def assist(n):\n    return n\n\n\ndef more():\n    return 2\n")]);
    cg.sync(&IndexOptions::default(), None).unwrap();
    let synced = edges_of(&cg);
    assert_eq!(synced, fresh_edges(root.path()));
    assert!(synced.iter().any(|e| e == "calls go@pkg/run.py -> assist@pkg/helper.py"), "{synced:?}");
    assert!(!synced.iter().any(|e| e.contains("other/run.py") && e.contains("assist@pkg/helper.py")), "{synced:?}");
    cg.close();

    // `from types import ModuleType` names a standard-library module, so the
    // `types.py` beside the script is not that module. The relative import of
    // `Thing` still links, and the edit matches a fresh index.
    let root = tempfile::tempdir().unwrap();
    write_files(
        root.path(),
        &[
            ("pkg/__init__.py", ""),
            ("pkg/types.py", "class Thing:\n    pass\n"),
            ("pkg/run.py", "from types import ModuleType\nfrom .types import Thing\n\n\ndef go():\n    return Thing()\n"),
        ],
    );
    let mut cg = CodeGraph::init_and_index(root.path().to_str().unwrap(), None).unwrap();
    write_files(root.path(), &[("pkg/types.py", "class Thing:\n    pass\n\n\nclass More:\n    pass\n")]);
    cg.sync(&IndexOptions::default(), None).unwrap();
    let synced = edges_of(&cg);
    assert_eq!(synced, fresh_edges(root.path()));
    assert!(synced.iter().any(|e| e.contains("Thing@pkg/types.py")), "{synced:?}");
    assert!(!synced.iter().any(|e| e.contains("ModuleType@pkg/types.py")), "{synced:?}");
    cg.close();
}
