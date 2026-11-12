//! A Kotlin chain reads through a holder of its type argument: `uiState.value` on a
//! `StateFlow<UiState>` is a `UiState` (upstream 41587d38; fixture from kotlin-value-holder-chains.test.ts).
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

const FILES: &[(&str, &str)] = &[
    ("src/Vm.kt", r#"package demo

import kotlinx.coroutines.flow.StateFlow

sealed interface UiState {
    fun shouldKeep(): Boolean = true
}

class Other {
    fun shouldKeep(): Boolean = false
    fun left(): Int = 1
    fun right(): Int = 2
}

class Decoy {
    fun left(): Int = 3
    fun right(): Int = 4
}

class Vm(val uiState: StateFlow<UiState>) {
    val pair: Pair<Other, Decoy> = Pair(Other(), Decoy())
    val lazily: Lazy<Other> = lazy { Other() }
    val box: Library<Other> = Library()
}
"#),
    ("src/Main.kt", r#"package demo

class Main(val viewModel: Vm, val flow: kotlinx.coroutines.flow.MutableStateFlow<Other>) {
    fun fromParameter() {
        viewModel.uiState.value.shouldKeep()
    }

    fun first() {
        viewModel.pair.first.left()
    }

    fun second() {
        viewModel.pair.second.right()
    }

    fun lazily() {
        viewModel.lazily.value.left()
    }

    fun ownProperty() {
        flow.value.left()
    }

    fun library() {
        viewModel.box.value.left()
    }

    fun notAMember() {
        viewModel.pair.third.left()
    }
}
"#),
];

#[test]
fn a_kotlin_chain_reads_through_a_holder_of_its_type_argument() {
    let dir = tempfile::tempdir().unwrap();
    for (rel, text) in FILES {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let methods = cg.get_nodes_by_kind(NodeKind::Method).unwrap();
    let calls = |name: &str| -> Vec<String> {
        let from = methods.iter().find(|n| n.name == name && n.file_path == "src/Main.kt").unwrap_or_else(|| panic!("no method {name}"));
        let mut out: Vec<String> = cg
            .get_outgoing_edges(&from.id)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .filter_map(|e| cg.get_node(&e.target).unwrap())
            .map(|n| n.qualified_name)
            .collect();
        out.sort();
        out
    };
    assert_eq!(calls("fromParameter"), ["demo::UiState::shouldKeep"]);
    assert_eq!(calls("ownProperty"), ["demo::Other::left"]);
    assert_eq!(calls("first"), ["demo::Other::left"]);
    assert_eq!(calls("second"), ["demo::Decoy::right"]);
    assert_eq!(calls("lazily"), ["demo::Other::left"]);
    assert_eq!(calls("library"), Vec::<String>::new());
    assert_eq!(calls("notAMember"), Vec::<String>::new());
    cg.close();
}
