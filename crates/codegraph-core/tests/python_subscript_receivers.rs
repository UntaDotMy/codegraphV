//! A Python call on an element of a collection links to the project's one method of the name,
//! in a module the file has in hand (upstream 9191ab66). The collection's name says nothing of
//! the element's class, so no word scoring and no attribute after the subscript.
use codegraph_core::{CodeGraph, IndexOptions};
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;
use std::path::Path;

fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (rel, content) in files {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }
}

fn index(files: &[(&str, &str)]) -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    write_files(dir.path(), files);
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

fn calls(cg: &CodeGraph, function: &str) -> Vec<(u32, String)> {
    let node = cg
        .get_nodes_by_name(function)
        .unwrap()
        .into_iter()
        .find(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method))
        .unwrap_or_else(|| panic!("{function} not found"));
    let mut reached: Vec<(u32, String)> = cg
        .get_outgoing_edges(&node.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Calls)
        .map(|e| (e.line.unwrap_or(0), cg.get_node(&e.target).unwrap().unwrap().qualified_name))
        .collect();
    reached.sort_by_key(|(line, _)| *line);
    reached
}

const CLASSES: &str = "\
class Log:
    def get_message(self):
        return 1
    def pop(self):
        return 1
    def search(self):
        return 1

class Timeline:
    def replay(self):
        return 1

class Other:
    def replay(self):
        return 1
";

fn at(offset: u32) -> u32 {
    CLASSES.lines().count() as u32 + 1 + offset
}

#[test]
fn links_the_one_method_of_the_name_through_any_subscript() {
    let source = format!(
        "{CLASSES}def f(trigger, stats, key):\n    trigger[\"a\"][0].get_message()\n    stats[key].get_message()\n    stats[key[\"k\"]].get_message()\n    stats[\"a]b\"].get_message()\n"
    );
    let (_dir, cg) = index(&[("a.py", &source)]);
    assert_eq!(
        calls(&cg, "f"),
        [
            (at(1), "Log::get_message".to_string()),
            (at(2), "Log::get_message".to_string()),
            (at(3), "Log::get_message".to_string()),
            (at(4), "Log::get_message".to_string()),
        ]
    );
}

#[test]
fn never_takes_a_method_a_list_or_a_dict_has_or_one_of_several() {
    let source = format!(
        "{CLASSES}def f(trigger):\n    trigger[\"a\"].pop(1)\n    trigger[\"a\"].search()\n    trigger[\"a\"].replay()\n"
    );
    let (_dir, cg) = index(&[("a.py", &source)]);
    assert!(calls(&cg, "f").is_empty());
}

#[test]
fn stops_at_an_attribute_after_the_subscript() {
    let source = format!(
        "{CLASSES}def f(caches):\n    caches[\"opt\"].timeline.replay()\n    caches[\"opt\"].anything.get_message()\n"
    );
    let (_dir, cg) = index(&[("a.py", &source)]);
    assert!(calls(&cg, "f").is_empty());
}

#[test]
fn never_goes_by_the_name_of_the_collection_or_by_a_dunder() {
    let source = "\
class ConfigContext:
    def url(self):
        return 1

class Other:
    def url(self):
        return 1

class Registry:
    def __setitem__(self, k, v):
        pass

def f(context, registry, a):
    context[\"object\"].url()
    registry[\"a\"].__setitem__(1, a)
";
    let (_dir, cg) = index(&[("a.py", source)]);
    assert!(calls(&cg, "f").is_empty());
}

#[test]
fn reads_an_element_of_what_a_call_returns_the_same_way() {
    let source = format!("{CLASSES}def g():\n    return []\ndef f(x):\n    g()[0].get_message()\n");
    let (_dir, cg) = index(&[("a.py", &source)]);
    let mut names: Vec<String> = calls(&cg, "f").into_iter().map(|(_, name)| name).collect();
    names.sort();
    assert_eq!(names, ["Log::get_message", "g"]);
}

#[test]
fn wants_the_method_in_a_module_the_file_has_in_hand() {
    let (_dir, cg) = index(&[
        ("lookups.py", "class CollatedMixin:\n    def process_rhs(self):\n        return 1\n"),
        ("models.py", "class Log:\n    def get_message(self):\n        return 1\n"),
        (
            "use.py",
            "from models import Log\ndef f(children, logs):\n    children[0].process_rhs()\n    logs[0].get_message()\n",
        ),
    ]);
    let reached: Vec<String> = calls(&cg, "f").into_iter().map(|(_, name)| name).collect();
    assert_eq!(reached, ["Log::get_message"]);
}
