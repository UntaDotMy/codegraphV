//! A `#if` branch that begins with `else` no longer files the rest of the file under the enclosing
//! function (upstream cbd9817f). tree-sitter-c parses every branch of a conditional group as a run
//! of block items, so `} #ifdef X else if (…) { … } #endif` left a K&R-shaped `if (…) { … }` that
//! the grammar reads as a function named `if`, and the functions after it were nested under it.
//! A block macro in capitals in statement position (`ATOMIC_BLOCK(NVIC_PRIO_MAX) {`) was skipped
//! by the statement-macro blank, which matched lowercase iterator macros only.
use codegraph_core::CodeGraph;
use codegraph_types::NodeKind;
use std::fs;

const CURRENT_C: &str = "\
void currentMeterRead(int id, meter_t *meter)
{
    if (id == 1) {
        adcRead(meter);
    }
#ifdef USE_VIRTUAL
    else if (id == 2) {
        virtualRead(meter);
    }
#endif
    else {
        resetMeter(meter);
    }
}

static bool markBusy(void)
{
    ATOMIC_BLOCK(NVIC_PRIO_MAX) {
        busy = true;
    }
    return true;
}

int after(void)
{
    return 1;
}
";

#[test]
fn an_if_branch_beginning_with_else_does_not_file_the_rest_of_the_file_under_the_enclosing_function() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("current.c"), CURRENT_C).unwrap();
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let mut functions: Vec<_> = cg.get_nodes_by_kind(NodeKind::Function).unwrap().into_iter().filter(|n| n.file_path == "current.c").collect();
    functions.sort_by(|a, b| a.name.cmp(&b.name));
    let names: Vec<&str> = functions.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(names, ["after", "currentMeterRead", "markBusy"], "{functions:#?}");
    let by_name = |name: &str| functions.iter().find(|n| n.name == name).unwrap();
    assert_eq!(by_name("currentMeterRead").end_line, 14);
    assert_eq!(by_name("markBusy").qualified_name, "markBusy");
    assert_eq!(by_name("markBusy").end_line, 22);
    assert_eq!(by_name("after").qualified_name, "after");
    assert_eq!(by_name("after").start_line, 24);
}
