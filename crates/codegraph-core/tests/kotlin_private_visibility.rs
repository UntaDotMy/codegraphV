//! A Kotlin private property, and the members of a private class, stay in their file (upstream
//! 13f983db). A property was indexed without its visibility modifier, so the file-local rule read
//! every property as public; and a public member of a private class was reachable from other files
//! though nothing outside its file can name it.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind, Visibility};
use std::fs;

fn index(files: &[(&str, &str)]) -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    for (relative, content) in files {
        let path = dir.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

/// `file:name` of everything the function or method called `caller` calls.
fn callees_of(cg: &CodeGraph, caller: &str) -> Vec<String> {
    let node = cg
        .get_nodes_by_name(caller)
        .unwrap()
        .into_iter()
        .find(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method))
        .unwrap_or_else(|| panic!("{caller} is indexed"));
    let mut callees = Vec::new();
    for edge in cg.get_outgoing_edges(&node.id).unwrap() {
        if edge.kind == EdgeKind::Calls {
            let target = cg.get_node(&edge.target).unwrap().expect("target exists");
            callees.push(format!("{}:{}", target.file_path, target.name));
        }
    }
    callees
}

#[test]
fn a_property_records_its_modifier_and_a_call_from_another_file_declines_it() {
    let (_dir, cg) = index(&[
        ("WebRtcTransport.kt", "class WebRtcTransport {\n    @Volatile private var token = \"\"\n    internal val shared = 1\n}\n"),
        (
            "CommandValidator.kt",
            "class CommandValidator {\n    fun validate(command: Map<String, Any>): String? {\n        if (!token(command[\"sessionId\"], 128)) return \"bad\"\n        return null\n    }\n}\n",
        ),
    ]);
    assert!(!callees_of(&cg, "validate").contains(&"WebRtcTransport.kt:token".to_string()));
    let fields: Vec<_> = cg.get_nodes_by_kind(NodeKind::Field).unwrap().into_iter().filter(|n| n.file_path.ends_with("WebRtcTransport.kt")).collect();
    assert_eq!(fields.iter().find(|n| n.name == "token").and_then(|n| n.visibility), Some(Visibility::Private));
    assert_eq!(fields.iter().find(|n| n.name == "shared").and_then(|n| n.visibility), Some(Visibility::Internal));
}

#[test]
fn a_call_declines_a_public_fun_of_another_files_private_class() {
    // `clock`'s type is unknown here, so only the method name picks a target.
    let (_dir, cg) = index(&[
        ("Coordinator.kt", "class Coordinator {\n    private class Clock(var value: Long) { fun now() = value }\n}\n"),
        ("Gimbal.kt", "class Gimbal {\n    fun dispatchSpeed() {\n        val clock = provide()\n        val timestamp = clock.now()\n    }\n}\n"),
    ]);
    assert!(!callees_of(&cg, "dispatchSpeed").contains(&"Coordinator.kt:now".to_string()));
}
