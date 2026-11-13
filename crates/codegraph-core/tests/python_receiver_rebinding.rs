//! Upstream b700a3ee (3046): a parameter or local named like an imported module is not that
//! module. Ported from `__tests__/python-receiver-rebinding.test.ts` (LF; the CRLF writes differ
//! only in line endings).
use codegraph_core::CodeGraph;
use codegraph_types::EdgeKind;
use std::fs;

const FILES: &[(&str, &str)] = &[
    ("pyproject.toml", "[project]\nname = \"pkg\"\n"),
    ("src/pkg/__init__.py", ""),
    (
        "src/pkg/other.py",
        "def update(x=None):\n    return x\n\n\ndef persist(x=None):\n    return x\n\n\ndef get(key=None):\n    return key\n",
    ),
    ("src/pkg/third.py", "def get(key=None):\n    return key\n"),
    (
        "tests/shadow.py",
        "from pkg import other\n\n\ndef real():\n    other.update({\"b\": 2})\n    return other.persist(1)\n\n\ndef by_param(other):\n    other.update({\"a\": 1})\n    return other.persist(2)\n\n\ndef by_with(path):\n    with open(path, \"w\") as other:\n        return other.persist(3)\n\n\ndef by_assign():\n    other = {}\n    other.update({\"c\": 3})\n\n\ndef by_for(items):\n    for other in items:\n        other.persist(4)\n\n\ndef by_except():\n    try:\n        pass\n    except OSError as other:\n        return other.persist(5)\n\n\ndef by_lambda():\n    return lambda other: other.persist(6)\n\n\ndef by_comprehension(items):\n    return [other.persist(7) for other in items]\n\n\ndef by_walrus(load):\n    if (other := load()):\n        return other.persist(8)\n\n\ndef by_closure(other):\n    def inner():\n        return other.persist(9)\n    return inner\n\n\ndef by_capture(v):\n    match v:\n        case other:\n            return other.persist(10)\n\n\ndef by_local_import():\n    from pkg import third as other\n    return other.get(11)\n\n\ndef keyword_argument(run):\n    run(\n        other=1,\n    )\n    return other.persist(12)\n\n\ndef in_a_string():\n    print(\"with x as other\", \"other = 1\")  # other = 2\n    return other.persist(13)\n\n\ndef comprehension_of_others(items):\n    return [x for x in other.persist(items)]\n\n\ndef lambda_beside(run):\n    return run(lambda other: 1, other.persist(14))\n\n\ndef declared_global():\n    global other\n    return other.persist(15)\n\n\ndef keyword_pattern(v):\n    match v:\n        case Point(other=1):\n            return other.persist(16)\n\n\ndef same_module_again():\n    from pkg import other\n    return other.get(17)\n",
    ),
    (
        "tests/leak.py",
        "import pkg.other as other\n\n\ndef local_third():\n    from pkg import third as other\n    return other.get(1)\n\n\ndef untouched():\n    return other.get(2)\n",
    ),
];

fn index() -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    for (rel, content) in FILES {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

fn calls(cg: &CodeGraph, file: &str, name: &str) -> Vec<String> {
    let from = cg
        .get_nodes_by_file(file)
        .unwrap()
        .into_iter()
        .filter(|n| n.name == name && matches!(n.kind.as_str(), "function" | "method"))
        .next()
        .unwrap_or_else(|| panic!("{name} in {file}"));
    let mut out: Vec<String> = cg
        .get_outgoing_edges(&from.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Calls)
        .filter_map(|e| cg.get_node(&e.target).unwrap())
        .filter(|t| t.file_path.starts_with("src/"))
        .map(|t| format!("{}:{}", t.file_path, t.name))
        .collect();
    out.sort();
    out
}

#[test]
fn a_call_where_nothing_nearer_binds_the_name_reaches_the_module() {
    let (_dir, cg) = index();
    assert_eq!(calls(&cg, "tests/shadow.py", "real"), ["src/pkg/other.py:persist", "src/pkg/other.py:update"]);
    cg.close();
}

#[test]
fn a_name_the_function_binds_links_no_module_function() {
    let (_dir, cg) = index();
    for function in ["by_param", "by_with", "by_assign", "by_for", "by_except", "by_lambda", "by_comprehension", "by_walrus", "inner", "by_capture"] {
        assert_eq!(calls(&cg, "tests/shadow.py", function), Vec::<String>::new(), "{function}");
    }
    cg.close();
}

#[test]
fn a_function_local_import_is_that_module_only_in_that_function() {
    let (_dir, cg) = index();
    assert_eq!(calls(&cg, "tests/shadow.py", "by_local_import"), ["src/pkg/third.py:get"]);
    assert_eq!(calls(&cg, "tests/leak.py", "local_third"), ["src/pkg/third.py:get"]);
    assert_eq!(calls(&cg, "tests/leak.py", "untouched"), ["src/pkg/other.py:get"]);
    cg.close();
}

#[test]
fn incidental_words_and_global_declarations_bind_nothing() {
    let (_dir, cg) = index();
    for (function, target) in [
        ("keyword_argument", "src/pkg/other.py:persist"),
        ("in_a_string", "src/pkg/other.py:persist"),
        ("comprehension_of_others", "src/pkg/other.py:persist"),
        ("lambda_beside", "src/pkg/other.py:persist"),
        ("declared_global", "src/pkg/other.py:persist"),
        ("keyword_pattern", "src/pkg/other.py:persist"),
        ("same_module_again", "src/pkg/other.py:get"),
    ] {
        assert_eq!(calls(&cg, "tests/shadow.py", function), [target], "{function}");
    }
    cg.close();
}
