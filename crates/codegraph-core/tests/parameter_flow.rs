//! A parameter's class from its call sites (upstream 483a0988; port of parameter-flow.test.ts).
//! `table.foreign_key` in rails' `def add_to(table)` went to whatever class is called `Table`,
//! while the one caller of `add_to` passes `self`, a `TableDefinition`.
//!
//! Exact or nothing: a call site that can't be typed, two that disagree, or anything of a shape
//! that could be a call the graph never linked leaves the call exactly as resolution answered it.
//! And a sync gives what a fresh index gives when a caller's file changes what the callee's calls
//! mean. Each fixture is indexed for real.
use codegraph_core::{CodeGraph, IndexOptions};
use codegraph_types::{Edge, EdgeKind, NodeKind};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

const REFERENCE: &str = "class Reference
  def add_to(table)
    table.column(:a)
    table.index(:a)
  end
end
";

const TABLES: &str = "class TableDefinition
  def references(name)
    Reference.new.add_to(self)
  end

  def column(name)
    name
  end

  def index(name)
    name
  end
end

class OtherDefinition
  def column(name)
    name
  end

  def index(name)
    name
  end
end
";

/// What a guess by the receiver's word finds.
const GUESS: &str = "class Table
  def column(name)
    name
  end

  def index(name)
    name
  end
end
";

type Files = Vec<(&'static str, String)>;

fn base() -> Files {
    vec![("lib/reference.rb", REFERENCE.to_string()), ("lib/tables.rb", TABLES.to_string()), ("lib/guess.rb", GUESS.to_string())]
}

/// `base()` with these files put in or replaced.
fn with(changes: &[(&'static str, String)]) -> Files {
    let mut files = base();
    for (rel, content) in changes {
        match files.iter_mut().find(|(r, _)| r == rel) {
            Some(slot) => slot.1 = content.clone(),
            None => files.push((rel, content.clone())),
        }
    }
    files
}

fn write(dir: &Path, rel: &str, content: &str) {
    let full = dir.join(rel);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, content).unwrap();
}

fn index(files: &[(&'static str, String)]) -> (CodeGraph, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    for (rel, content) in files {
        write(dir.path(), rel, content);
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (cg, dir)
}

#[allow(dead_code)]
fn sync(cg: &mut CodeGraph) {
    cg.sync(&IndexOptions::default(), None).unwrap();
}

fn meta(e: &Edge, key: &str) -> Option<serde_json::Value> {
    e.metadata.as_ref().and_then(|m| m.get(key)).cloned()
}

fn calls_edges(cg: &CodeGraph, file: &str, method: &str) -> Vec<Edge> {
    let nodes = cg.get_nodes_by_file(file).unwrap();
    let source = nodes.iter().find(|n| n.qualified_name == method && n.kind == NodeKind::Method).unwrap_or_else(|| panic!("{method} in {file}"));
    cg.get_outgoing_edges(&source.id).unwrap().into_iter().filter(|e| e.kind == EdgeKind::Calls).collect()
}

/// The qualified name each `calls` edge out of the method reaches, by the reference that made it.
fn calls_of(cg: &CodeGraph, file: &str, method: &str) -> BTreeMap<String, Vec<String>> {
    let mut calls: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for e in calls_edges(cg, file, method) {
        let name = meta(&e, "refName").and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
        calls.entry(name).or_default().push(cg.get_node(&e.target).unwrap().unwrap().qualified_name);
    }
    for targets in calls.values_mut() {
        targets.sort();
    }
    calls
}

fn add_to_calls(cg: &CodeGraph) -> BTreeMap<String, Vec<String>> {
    calls_of(cg, "lib/reference.rb", "Reference::add_to")
}

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, Vec<String>> {
    pairs.iter().map(|(k, v)| (k.to_string(), vec![v.to_string()])).collect()
}

fn flowed() -> BTreeMap<String, Vec<String>> {
    map(&[("table.column", "TableDefinition::column"), ("table.index", "TableDefinition::index")])
}

fn guessed() -> BTreeMap<String, Vec<String>> {
    map(&[("table.column", "Table::column"), ("table.index", "Table::index")])
}

fn other() -> BTreeMap<String, Vec<String>> {
    map(&[("table.column", "OtherDefinition::column"), ("table.index", "OtherDefinition::index")])
}

fn each(cases: &[(&str, Files)], expected: &BTreeMap<String, Vec<String>>) {
    let mut failures = Vec::new();
    for (shape, files) in cases {
        let (cg, _dir) = index(files);
        let got = add_to_calls(&cg);
        if &got != expected {
            failures.push(format!("{shape}: {got:?}"));
        }
    }
    assert!(failures.is_empty(), "expected {expected:?}\n{}", failures.join("\n"));
}

fn tables_without_site() -> String {
    TABLES.replace("Reference.new.add_to(self)", "name")
}

// ── A parameter takes its class from its call sites ─────────────────────────────

#[test]
fn answers_a_call_on_the_parameter_with_the_method_of_the_class_every_call_passes() {
    let (cg, _dir) = index(&base());
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn beats_a_guess_and_marks_the_edge_as_its_own() {
    let (cg, _dir) = index(&base());
    let edge = calls_edges(&cg, "lib/reference.rb", "Reference::add_to")
        .into_iter()
        .find(|e| meta(e, "refName").as_ref().and_then(|v| v.as_str()) == Some("table.column"))
        .unwrap();
    assert_eq!(meta(&edge, "resolvedBy"), Some(serde_json::json!("instance-method")));
    assert_eq!(meta(&edge, "confidence"), Some(serde_json::json!(0.9)));
    assert_eq!(meta(&edge, "parameterFlow"), Some(serde_json::json!("table")));
}

#[test]
fn types_a_value_of_each_exact_shape() {
    let shapes: [(&str, &str); 10] = [
        ("Const.new", "class A\n  def run\n    Reference.new.add_to(OtherDefinition.new)\n  end\nend\n"),
        ("Const.new with arguments", "class A\n  def run(x)\n    Reference.new.add_to(OtherDefinition.new(x, [1, 2]))\n  end\nend\n"),
        ("a local assigned one", "class A\n  def run\n    table = OtherDefinition.new\n    Reference.new.add_to(table)\n  end\nend\n"),
        ("a typed instance variable", "class A\n  def initialize\n    @t = OtherDefinition.new\n  end\n\n  def run\n    Reference.new.add_to(@t)\n  end\nend\n"),
        ("a parameter that flows", "class A\n  def run\n    go(OtherDefinition.new)\n  end\n\n  def go(t)\n    Reference.new.add_to(t)\n  end\nend\n"),
        ("a call with no parentheses", "class A\n  def run\n    Reference.new.add_to OtherDefinition.new\n  end\nend\n"),
        ("arguments across lines", "class A\n  def run\n    Reference.new.add_to(\n      OtherDefinition.new,\n    )\n  end\nend\n"),
        ("a chain broken across lines", "class A\n  def run\n    Reference\n      .new\n      .add_to(OtherDefinition.new)\n  end\nend\n"),
        ("a receiver typed by a local", "class A\n  def run\n    r = Reference.new\n    r.add_to(OtherDefinition.new)\n  end\nend\n"),
        ("the argument of a modifier line", "class A\n  def run(flag)\n    Reference.new.add_to OtherDefinition.new if flag\n  end\nend\n"),
    ];
    let cases: Vec<(&str, Files)> = shapes
        .iter()
        .map(|(shape, caller)| (*shape, with(&[("lib/tables.rb", tables_without_site()), ("lib/caller.rb", caller.to_string())])))
        .collect();
    each(&cases, &other());
}

#[test]
fn reads_an_argument_among_others() {
    let shapes: [(&str, &str, &str); 7] = [
        ("past a string with a comma", "add_to(name, table)", "add_to(\"a,b\", self)"),
        ("past a call with its own arguments", "add_to(name, table)", "add_to(name.to_s(1, 2), self)"),
        ("past a hash", "add_to(name, table)", "add_to({ a: 1, b: 2 }, self)"),
        ("before keywords", "add_to(table, flag: nil)", "add_to(self, flag: 1)"),
        ("before a block", "add_to(table)", "add_to(self) { |x| x }"),
        ("before a block passed on", "add_to(table, &block)", "add_to(self, &:to_s)"),
        ("with a default the call overrides", "add_to(table = nil)", "add_to(self)"),
    ];
    let cases: Vec<(&str, Files)> = shapes
        .iter()
        .map(|(shape, declared, called)| {
            (*shape, with(&[("lib/reference.rb", REFERENCE.replace("add_to(table)", declared)), ("lib/tables.rb", TABLES.replace("add_to(self)", called))]))
        })
        .collect();
    each(&cases, &flowed());
}

#[test]
fn reads_a_call_on_self() {
    let (cg, _dir) = index(&with(&[
        ("lib/reference.rb", "class Reference\n  def run\n    self.add_to(TableDefinition.new)\n  end\n\n  def add_to(table)\n    table.column(:a)\n    table.index(:a)\n  end\nend\n".to_string()),
        ("lib/tables.rb", tables_without_site()),
    ]));
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn types_a_keyword_parameter_by_its_keyword() {
    let (cg, _dir) = index(&with(&[
        ("lib/reference.rb", REFERENCE.replace("add_to(table)", "add_to(flag, table:)")),
        ("lib/tables.rb", TABLES.replace("add_to(self)", "add_to(name, table: self)")),
    ]));
    assert_eq!(add_to_calls(&cg), flowed());
}

const BUILD_REFERENCE: &str = "class Reference\n  def add_to(table)\n    table.build(:a)\n  end\nend\n";

fn add_to_target(cg: &CodeGraph) -> codegraph_types::Node {
    let edge = calls_edges(cg, "lib/reference.rb", "Reference::add_to").into_iter().next().unwrap();
    cg.get_node(&edge.target).unwrap().unwrap()
}

#[test]
fn reads_a_constant_naming_a_class_as_the_class_and_calls_its_class_method() {
    let (cg, _dir) = index(&[
        ("lib/reference.rb", BUILD_REFERENCE.to_string()),
        ("lib/tables.rb", "class TableDefinition\n  def build(name)\n    name\n  end\n\n  def self.build(name)\n    name\n  end\nend\n\nclass Table\n  def build(name)\n    name\n  end\nend\n".to_string()),
        ("lib/caller.rb", "class A\n  def run\n    Reference.new.add_to(TableDefinition)\n  end\nend\n".to_string()),
    ]);
    let target = add_to_target(&cg);
    assert_eq!(target.qualified_name, "TableDefinition::build");
    assert_eq!(target.is_static, Some(true));
}

#[test]
fn takes_self_in_a_class_method_for_the_class_itself() {
    let (cg, _dir) = index(&[
        ("lib/reference.rb", BUILD_REFERENCE.to_string()),
        ("lib/tables.rb", "class TableDefinition\n  def build(name)\n    name\n  end\n\n  def self.build(name)\n    name\n  end\n\n  def self.run\n    Reference.new.add_to(self)\n  end\nend\n\nclass Table\n  def build(name)\n    name\n  end\nend\n".to_string()),
    ]);
    assert_eq!(add_to_target(&cg).is_static, Some(true));
}

#[test]
fn follows_a_parameter_that_passes_itself_back_by_the_calls_from_outside_the_cycle() {
    let (cg, _dir) = index(&with(&[(
        "lib/reference.rb",
        "class Reference\n  def add_to(table)\n    table.column(:a)\n    table.index(:a)\n    add_to(table)\n  end\nend\n".to_string(),
    )]));
    let calls = add_to_calls(&cg);
    assert_eq!(calls.get("table.column"), flowed().get("table.column"));
    assert_eq!(calls.get("table.index"), flowed().get("table.index"));
}

#[test]
fn reaches_a_method_the_class_has_from_its_ancestors() {
    let tables = "class OtherDefinition\n  def column(name)\n    name\n  end\n\n  def index(name)\n    name\n  end\nend\n\nclass TableDefinition < OtherDefinition\n  def references(name)\n    Reference.new.add_to(self)\n  end\nend\n";
    let (cg, _dir) = index(&with(&[("lib/tables.rb", tables.to_string())]));
    assert_eq!(add_to_calls(&cg), other());
}

#[test]
fn reads_a_method_written_each_way() {
    let shapes: [(&str, &str); 5] = [
        ("one line", "class Reference\n  def add_to(table) = table.column(:a)\nend\n"),
        ("a body after a semicolon", "class Reference\n  def add_to(table); table.column(:a)\n  end\nend\n"),
        ("a modifier before the def", "class Reference\n  protected def add_to(table)\n    table.column(:a)\n  end\nend\n"),
        ("no parentheses", "class Reference\n  def add_to table\n    table.column(:a)\n  end\nend\n"),
        ("a list across lines", "class Reference\n  def add_to(\n    table,\n    other = nil\n  )\n    table.column(:a)\n  end\nend\n"),
    ];
    let mut failures = Vec::new();
    for (shape, reference) in shapes {
        let (cg, _dir) = index(&with(&[("lib/reference.rb", reference.to_string())]));
        let got = add_to_calls(&cg).get("table.column").cloned();
        if got != Some(vec!["TableDefinition::column".to_string()]) {
            failures.push(format!("{shape}: {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn sees_an_assignment_on_the_line_of_the_def() {
    let (cg, _dir) = index(&with(&[(
        "lib/reference.rb",
        "class Reference\n  def add_to(table); table = table.first\n    table.column(:a)\n  end\nend\n".to_string(),
    )]));
    assert_eq!(add_to_calls(&cg).get("table.column"), Some(&vec!["Table::column".to_string()]));
}

#[test]
fn reads_no_comment_as_a_mention() {
    let (cg, _dir) = index(&with(&[("lib/doc.rb", "# Call send(:add_to, table) to attach it.\n#   %i[add_to]\nclass Doc\nend\n".to_string())]));
    assert_eq!(add_to_calls(&cg), flowed());
}

// ── No answer when a call site says nothing ─────────────────────────────────────

#[test]
fn leaves_the_guess_when_another_site_passes_something_untyped() {
    let shapes: [(&str, &str); 10] = [
        ("two classes", "class A\n  def run\n    Reference.new.add_to(OtherDefinition.new)\n  end\nend\n"),
        ("a value of no known class", "class A\n  def run(thing)\n    Reference.new.add_to(thing.table)\n  end\nend\n"),
        ("a parameter nothing types", "class A\n  def run(thing)\n    Reference.new.add_to(thing)\n  end\nend\n"),
        ("a literal", "class A\n  def run\n    Reference.new.add_to(nil)\n  end\nend\n"),
        ("a splat", "class A\n  def run(args)\n    Reference.new.add_to(*args)\n  end\nend\n"),
        ("a block parameter", "class A\n  def run(items)\n    items.each { |t| Reference.new.add_to(t) }\n  end\nend\n"),
        ("a local assigned two classes", "class A\n  def run(flag)\n    t = OtherDefinition.new\n    t = TableDefinition.new if flag\n    Reference.new.add_to(t)\n  end\nend\n"),
        ("a conditional", "class A\n  def run(flag)\n    Reference.new.add_to(flag ? TableDefinition.new : OtherDefinition.new)\n  end\nend\n"),
        ("a constant holding a value", "class A\n  TABLE = OtherDefinition.new\n\n  def run\n    Reference.new.add_to(TABLE)\n  end\nend\n"),
        ("a call on what new made", "class A\n  def run\n    Reference.new.add_to(OtherDefinition.new.tap { |t| t })\n  end\nend\n"),
    ];
    let cases: Vec<(&str, Files)> = shapes.iter().map(|(shape, caller)| (*shape, with(&[("lib/caller.rb", caller.to_string())]))).collect();
    each(&cases, &guessed());
}

#[test]
fn reads_no_place_for_a_parameter_whose_place_depends_on_the_call() {
    let shapes: [(&str, &str, &str); 2] =
        [("past a splat", "add_to(*rest, table)", "add_to(self)"), ("a required one past an optional", "add_to(flag = nil, table)", "add_to(self)")];
    let cases: Vec<(&str, Files)> = shapes
        .iter()
        .map(|(shape, declared, called)| {
            (*shape, with(&[("lib/reference.rb", REFERENCE.replace("add_to(table)", declared)), ("lib/tables.rb", TABLES.replace("add_to(self)", called))]))
        })
        .collect();
    each(&cases, &guessed());
}

#[test]
fn says_nothing_of_self_where_it_is_not_the_class() {
    let shapes: [(&str, &str); 4] = [
        ("in a module, which runs on whatever includes it", "module A\n  def run\n    Reference.new.add_to(self)\n  end\n\n  def column(name)\n    name\n  end\nend\n"),
        ("in a block run on something else", "class A\n  def run(thing)\n    thing.instance_eval { Reference.new.add_to(self) }\n  end\n\n  def column(name)\n    name\n  end\nend\n"),
        ("in a def inside a block of the class", "class A\n  included do\n    def run\n      Reference.new.add_to(self)\n    end\n  end\n\n  def column(name)\n    name\n  end\nend\n"),
        ("in a def of a class made at runtime", "class A\n  Point = Struct.new(:x) do\n    def run\n      Reference.new.add_to(self)\n    end\n  end\n\n  def column(name)\n    name\n  end\nend\n"),
    ];
    let cases: Vec<(&str, Files)> = shapes
        .iter()
        .map(|(shape, caller)| (*shape, with(&[("lib/tables.rb", tables_without_site()), ("lib/caller.rb", caller.to_string())])))
        .collect();
    each(&cases, &guessed());
}

#[test]
fn leaves_the_guess_when_a_site_passes_no_argument() {
    let (cg, _dir) = index(&with(&[
        ("lib/reference.rb", REFERENCE.replace("add_to(table)", "add_to(table = nil)")),
        ("lib/caller.rb", "class A\n  def run\n    Reference.new.add_to\n  end\nend\n".to_string()),
    ]));
    assert_eq!(add_to_calls(&cg), guessed());
}

#[test]
fn leaves_the_guess_when_the_method_rebinds_the_parameter() {
    let shapes: [(&str, &str); 4] = [
        ("a block parameter of the name", "class Reference\n  def add_to(table)\n    [1].each { |table| table.column(:a) }\n    table.index(:a)\n  end\nend\n"),
        ("an assignment", "class Reference\n  def add_to(table)\n    table = table.first\n    table.column(:a)\n    table.index(:a)\n  end\nend\n"),
        ("several assignment", "class Reference\n  def add_to(table)\n    a, table = table\n    table.column(:a)\n    table.index(:a)\n  end\nend\n"),
        ("a rescue", "class Reference\n  def add_to(table)\n    table.column(:a)\n    table.index(:a)\n  rescue StandardError => table\n    nil\n  end\nend\n"),
    ];
    let cases: Vec<(&str, Files)> = shapes.iter().map(|(shape, reference)| (*shape, with(&[("lib/reference.rb", reference.to_string())]))).collect();
    each(&cases, &guessed());
}

fn ivar_site() -> String {
    TABLES.replace("Reference.new.add_to(self)", "Reference.new.add_to(name)").replace(
        "class TableDefinition\n",
        "class Holder\n  def go\n    Reference.new.add_to(@table)\n  end\nend\n\nclass TableDefinition\n",
    )
}

#[test]
fn says_nothing_of_an_instance_variable_typed_in_another_file() {
    let (cg, _dir) = index(&[
        ("lib/reference.rb", REFERENCE.to_string()),
        ("lib/guess.rb", GUESS.to_string()),
        ("lib/tables.rb", ivar_site().replace("Reference.new.add_to(name)", "name")),
        ("lib/holder.rb", "class Holder\n  def initialize\n    @table = TableDefinition.new\n  end\nend\n".to_string()),
    ]);
    assert_eq!(add_to_calls(&cg), guessed());
}

// ── No answer when the project may call the method some way the graph never linked ──

#[test]
fn abstains_on_each_call_the_graph_never_linked() {
    let shapes: [(&str, &str); 19] = [
        ("a call on an untyped receiver", "class A\n  def run(thing, x)\n    thing.add_to(x)\n  end\nend\n"),
        ("a bare call nothing placed", "module A\n  def run(x)\n    add_to(x)\n  end\nend\n"),
        ("a call in a class body, which the graph records nowhere", "class A\n  Reference.new.add_to(self)\nend\n"),
        ("a call in a block of a class body", "class A\n  test \"x\" do\n    Reference.new.add_to(1)\n  end\nend\n"),
        ("a call at the top of a file", "Reference.new.add_to(1)\n"),
        ("a spec", "describe Reference do\n  it \"adds\" do\n    described_class.new.add_to(table)\n  end\nend\n"),
        ("send", "class A\n  def run(x)\n    Reference.new.send(:add_to, x)\n  end\nend\n"),
        ("public_send", "class A\n  def run(x)\n    Reference.new.public_send(:add_to, x)\n  end\nend\n"),
        ("__send__ with a string", "class A\n  def run(x)\n    Reference.new.__send__(\"add_to\", x)\n  end\nend\n"),
        ("method", "class A\n  def run(x)\n    Reference.new.method(:add_to).call(x)\n  end\nend\n"),
        ("a symbol to proc", "class A\n  def run(xs)\n    xs.each(&:add_to)\n  end\nend\n"),
        ("alias", "class Reference\n  alias attach add_to\nend\n"),
        ("alias with symbols", "class Reference\n  alias :attach :add_to\nend\n"),
        ("alias_method", "class Reference\n  alias_method :attach, :add_to\nend\n"),
        ("delegate", "class A\n  delegate :add_to, to: :reference\nend\n"),
        ("a list of symbols", "class A\n  def run(r, x)\n    %i[add_to].each { |m| r.public_send(m, x) }\n  end\nend\n"),
        ("a list of words", "class A\n  def run(r, x)\n    %w(add_to).each { |m| r.public_send(m, x) }\n  end\nend\n"),
        ("a subclass's super", "class Sub < Reference\n  def add_to(table)\n    super\n  end\nend\n"),
        ("a subclass's super with arguments", "class Sub < Reference\n  def add_to(table)\n    super(table)\n  end\nend\n"),
    ];
    let cases: Vec<(&str, Files)> = shapes.iter().map(|(shape, extra)| (*shape, with(&[("lib/extra.rb", extra.to_string())]))).collect();
    each(&cases, &guessed());
}

#[test]
fn counts_a_subclass_override_whose_call_passes_the_same_parameter() {
    let (cg, _dir) = index(&with(&[(
        "lib/sub.rb",
        "class Sub < Reference\n  def add_to(table)\n    table\n  end\n\n  def run\n    Sub.new.add_to(OtherDefinition.new)\n  end\nend\n".to_string(),
    )]));
    assert_eq!(add_to_calls(&cg), guessed());
}

// ── A call that is not a caller of this method ──────────────────────────────────

#[test]
fn tells_calls_of_other_methods_apart() {
    let shapes: [(&str, &str, &str); 3] = [
        ("a typed call of a same-named method of another class", "lib/other.rb", "class Other\n  def add_to(table)\n    table\n  end\n\n  def run\n    Other.new.add_to(OtherDefinition.new)\n  end\nend\n"),
        ("an untyped call, by its arguments", "lib/format.rb", "class Format\n  def add_to(buffer, hash)\n    buffer\n  end\n\n  def run(part, buffer, hash)\n    part.add_to(buffer, hash)\n  end\nend\n"),
        ("a call on a class from outside", "lib/outside.rb", "class A\n  def run(x)\n    Rack::Utils.add_to(x)\n  end\nend\n"),
    ];
    let cases: Vec<(&str, Files)> = shapes.iter().map(|(shape, file, text)| (*shape, with(&[(*file, text.to_string())]))).collect();
    each(&cases, &flowed());
}

// ── What the pass reads of a file ───────────────────────────────────────────────

fn in_add_to(line: &str) -> String {
    REFERENCE.replace("    table.column(:a)\n", &format!("    {line}\n    table.column(:a)\n"))
}

#[test]
fn takes_a_file_over_the_size_limit_for_nothing_as_the_index_does() {
    let mention = "class Huge\n  def run(name)\n    name.send(:add_to, name)\n  end\nend\n";
    // Sized off codegraphV's own limit (10 MiB by default; upstream's is 1 MB).
    let limit = codegraph_types::limits::max_source_file_bytes() as usize;
    let padding = "# a line of padding, to go over the limit\n";
    let huge = format!("{mention}{}", padding.repeat(limit / padding.len() + 1));
    assert!(huge.len() > limit);
    let (cg, _dir) = index(&with(&[("lib/huge.rb", huge)]));
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn counts_a_mention_in_a_file_just_under_the_limit() {
    let mention = "class Big\n  def run(name)\n    name.send(:add_to, name)\n  end\nend\n";
    let big = format!("{mention}{}", "# a line of padding, to go near the limit\n".repeat(20_000));
    let (cg, _dir) = index(&with(&[("lib/big.rb", big)]));
    assert_eq!(add_to_calls(&cg), guessed());
}

#[test]
fn takes_every_word_of_a_line_past_the_cap_as_bound() {
    let (cg, _dir) = index(&with(&[("lib/reference.rb", in_add_to(&format!("puts \"{}\", table", "x".repeat(3_000))))]));
    assert_eq!(add_to_calls(&cg), guessed());
}

#[test]
fn reads_a_line_short_of_the_cap_as_it_always_did() {
    let (cg, _dir) = index(&with(&[("lib/reference.rb", in_add_to(&format!("puts \"{}\", table", "x".repeat(1_000))))]));
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn gets_through_an_assignment_on_a_line_of_a_third_of_a_megabyte() {
    let (cg, _dir) = index(&with(&[("lib/reference.rb", in_add_to(&format!("x = a{}b", " ".repeat(320_000))))]));
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn gets_through_a_rescue_on_a_line_of_two_thirds_of_a_megabyte() {
    let (cg, _dir) = index(&with(&[("lib/reference.rb", in_add_to(&format!("x = \"{}\"", "rescue ".repeat(95_000))))]));
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn gets_through_one_line_full_of_a_name_and_counts_it() {
    let full = format!("puts \"{}\"\n", "add_to ".repeat(140_000));
    let (cg, _dir) = index(&with(&[("lib/full.rb", full)]));
    assert_eq!(add_to_calls(&cg), guessed());
}

#[test]
fn still_reads_a_comment_as_no_mention_when_indented() {
    let (cg, _dir) = index(&with(&[("lib/comments.rb", "# add_to in a comment\n  # add_to, indented\n".to_string())]));
    assert_eq!(add_to_calls(&cg), flowed());
}

// ── A class with no such method ─────────────────────────────────────────────────

const ATTRIBUTES: &str = "class TableDefinition
  attr_reader :other,
              :index

  def references(name)
    Reference.new.add_to(self)
  end

  def column(name)
    name
  end
end
";

fn column_only() -> BTreeMap<String, Vec<String>> {
    map(&[("table.column", "TableDefinition::column")])
}

#[test]
fn rules_out_the_guess_when_the_class_reads_the_name_as_an_attribute() {
    let (cg, _dir) = index(&with(&[("lib/tables.rb", ATTRIBUTES.to_string())]));
    assert_eq!(add_to_calls(&cg), column_only());
}

#[test]
fn rules_out_the_guess_on_a_class_from_outside_the_project() {
    let (cg, _dir) = index(&with(&[(
        "lib/tables.rb",
        "class Caller\n  def run\n    Reference.new.add_to(StringScanner.new(\"x\"))\n  end\nend\n".to_string(),
    )]));
    assert_eq!(add_to_calls(&cg), BTreeMap::new());
}

#[test]
fn leaves_the_guess_when_the_class_may_hand_the_call_on() {
    let shapes: [(&str, &str); 2] = [
        ("it hands the call on", "class TableDefinition\n  extend Forwardable\n  def_delegators :@inner, :index\n\n  def references(name)\n    Reference.new.add_to(self)\n  end\n\n  def column(name)\n    name\n  end\nend\n"),
        ("nothing says why", "class TableDefinition\n  def references(name)\n    Reference.new.add_to(self)\n  end\n\n  def column(name)\n    name\n  end\nend\n"),
    ];
    let cases: Vec<(&str, Files)> = shapes.iter().map(|(shape, tables)| (*shape, with(&[("lib/tables.rb", tables.to_string())]))).collect();
    each(&cases, &map(&[("table.column", "TableDefinition::column"), ("table.index", "Table::index")]));
}

#[test]
fn gives_no_answer_on_a_class_method_of_a_module_level_attribute() {
    let (cg, _dir) = index(&with(&[("lib/tables.rb", ATTRIBUTES.replace("Reference.new.add_to(self)", "Reference.new.add_to(TableDefinition)"))]));
    assert_eq!(add_to_calls(&cg).get("table.index"), Some(&vec!["Table::index".to_string()]));
}

// ── A sync gives what a fresh index gives ───────────────────────────────────────

/// Every edge of an index, by natural key.
fn graph_of(cg: &CodeGraph) -> Vec<String> {
    let mut keys = BTreeMap::new();
    for file in cg.get_files().unwrap() {
        for n in cg.get_nodes_by_file(&file.path).unwrap() {
            keys.insert(n.id.clone(), format!("{:?} {}:{}:{}", n.kind, n.file_path, n.qualified_name, n.start_line));
        }
    }
    let mut edges = Vec::new();
    for id in keys.keys() {
        for e in cg.get_outgoing_edges(id).unwrap() {
            let metadata = serde_json::to_string(&e.metadata.clone().unwrap_or_default()).unwrap();
            edges.push(format!(
                "{:?} {} -> {} @{:?}:{:?} {metadata}",
                e.kind,
                keys[&e.source],
                keys.get(&e.target).cloned().unwrap_or(e.target.clone()),
                e.line,
                e.column
            ));
        }
    }
    edges.sort();
    edges
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(".codegraph") {
            continue;
        }
        let target = to.join(&name);
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir_all(&target).unwrap();
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Index `first`, apply `change` to the files, sync, and compare with a fresh index of the result.
fn converges(first: &[(&'static str, String)], change: impl FnOnce(&Path)) -> (CodeGraph, tempfile::TempDir) {
    let (mut cg, dir) = index(first);
    change(dir.path());
    sync(&mut cg);
    let fresh = tempfile::tempdir().unwrap();
    copy_tree(dir.path(), fresh.path());
    let rebuilt = CodeGraph::init_and_index(fresh.path().to_str().unwrap(), None).unwrap();
    assert_eq!(graph_of(&cg), graph_of(&rebuilt), "a sync and a fresh index differ");
    (cg, dir)
}

fn put(rel: &'static str, content: &str) -> impl FnOnce(&Path) {
    let content = content.to_string();
    move |root: &Path| write(root, rel, &content)
}

fn remove(rel: &'static str) -> impl FnOnce(&Path) {
    move |root: &Path| fs::remove_file(root.join(rel)).unwrap()
}

const OTHER_CALLER: &str = "class A\n  def run\n    Reference.new.add_to(OtherDefinition.new)\n  end\nend\n";

#[test]
fn sync_a_second_call_site_passing_another_class_takes_the_flow_away() {
    let (cg, _dir) = converges(&base(), put("lib/caller.rb", OTHER_CALLER));
    assert_eq!(add_to_calls(&cg), guessed());
}

#[test]
fn sync_removing_the_only_call_site_takes_the_flow_away() {
    let (cg, _dir) = converges(&base(), put("lib/tables.rb", &tables_without_site()));
    assert_eq!(add_to_calls(&cg), guessed());
}

#[test]
fn sync_deleting_the_file_of_the_only_call_site_takes_the_flow_away() {
    let files = with(&[("lib/tables.rb", tables_without_site()), ("lib/caller.rb", OTHER_CALLER.to_string())]);
    let (cg, _dir) = converges(&files, remove("lib/caller.rb"));
    assert_eq!(add_to_calls(&cg), guessed());
}

#[test]
fn sync_changing_what_the_call_site_passes_moves_the_edges() {
    let (cg, _dir) = converges(&base(), put("lib/tables.rb", &TABLES.replace("Reference.new.add_to(self)", "Reference.new.add_to(OtherDefinition.new)")));
    assert_eq!(add_to_calls(&cg), other());
}

#[test]
fn sync_a_call_site_that_starts_passing_a_known_class_brings_the_flow() {
    let start = with(&[("lib/tables.rb", TABLES.replace("Reference.new.add_to(self)", "Reference.new.add_to(name)"))]);
    let (cg, _dir) = converges(&start, put("lib/tables.rb", TABLES));
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn sync_a_mention_in_a_third_file_takes_the_flow_away_and_removing_it_brings_it_back() {
    let (mut cg, dir) = converges(&base(), put("lib/extra.rb", "class A\n  def run(r, x)\n    r.send(:add_to, x)\n  end\nend\n"));
    assert_eq!(add_to_calls(&cg), guessed());
    fs::remove_file(dir.path().join("lib/extra.rb")).unwrap();
    sync(&mut cg);
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn sync_an_edit_of_the_method_itself_keeps_the_flow() {
    let (cg, _dir) = converges(&base(), put("lib/reference.rb", &REFERENCE.replace("table.index(:a)", "table.index(:b)\n    table.column(:b)")));
    assert_eq!(add_to_calls(&cg).get("table.index"), Some(&vec!["TableDefinition::index".to_string()]));
}

#[test]
fn sync_removing_the_method_the_flow_reached_puts_the_call_back_through_resolution() {
    let (cg, _dir) = converges(&base(), put("lib/tables.rb", &TABLES.replacen("  def column(name)\n    name\n  end\n\n", "", 1)));
    assert_eq!(add_to_calls(&cg).get("table.column"), Some(&vec!["Table::column".to_string()]));
}

fn tables_without_index() -> String {
    TABLES.replace("  def index(name)\n    name\n  end\nend\n\nclass OtherDefinition", "end\n\nclass OtherDefinition")
}

fn tables_reading_index() -> String {
    tables_without_index().replace("class TableDefinition\n", "class TableDefinition\n  attr_reader :index\n\n")
}

#[test]
fn sync_a_guess_the_flow_ruled_out_comes_back_when_the_flow_goes() {
    let (mut cg, dir) = converges(&with(&[("lib/tables.rb", tables_reading_index())]), put("lib/caller.rb", OTHER_CALLER));
    assert_eq!(add_to_calls(&cg), guessed());
    fs::remove_file(dir.path().join("lib/caller.rb")).unwrap();
    sync(&mut cg);
    assert_eq!(add_to_calls(&cg), column_only());
}

#[test]
fn sync_a_guess_the_flow_ruled_out_stays_out_across_a_sync_of_something_else() {
    let (cg, _dir) = converges(&with(&[("lib/tables.rb", tables_reading_index())]), put("lib/unrelated.rb", "class Unrelated\nend\n"));
    assert_eq!(add_to_calls(&cg), column_only());
}

#[test]
fn sync_a_call_on_a_parameter_that_had_no_edge_goes_back_to_failed_when_its_flow_goes() {
    let names = |text: &str| text.replace("TableDefinition", "Frame").replace("OtherDefinition", "Panel");
    let files: Files = vec![("lib/reference.rb", REFERENCE.to_string()), ("lib/tables.rb", names(TABLES))];
    let (before, _d) = index(&files);
    assert_eq!(add_to_calls(&before), map(&[("table.column", "Frame::column"), ("table.index", "Frame::index")]));
    let (cg, _dir) = converges(&files, put("lib/caller.rb", "class A\n  def run\n    Reference.new.add_to(Panel.new)\n  end\nend\n"));
    assert_eq!(add_to_calls(&cg), BTreeMap::new());
}

// The upstream "from what the last pass recorded" cases, as convergence checks: codegraphV keeps
// no record and re-runs the whole pass, so only what the graph comes to is compared, not how the
// pass got there (which files it read, whether it started over, the record's contents).

const CALLER: &str = "class Caller
  def run(name)
    name
  end

  def other(name)
    name
  end
end
";

fn with_caller(changes: &[(&'static str, String)]) -> Files {
    let mut all = vec![("lib/caller.rb", CALLER.to_string())];
    all.extend(changes.iter().cloned());
    with(&all)
}

fn caller_running(body: &str) -> String {
    CALLER.replace("def run(name)\n    name", &format!("def run(name)\n    {body}"))
}

#[test]
fn recorded_an_edit_of_an_unrelated_file_leaves_the_flow() {
    let (cg, _dir) = converges(&with_caller(&[]), put("lib/caller.rb", &format!("# a note\n{CALLER}")));
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn recorded_an_edit_that_lands_its_edges_elsewhere_puts_the_flow_back() {
    let (cg, _dir) = converges(&with_caller(&[]), put("lib/reference.rb", &format!("# a note\n{REFERENCE}")));
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn recorded_a_flow_moves_with_the_method_it_reached() {
    let columns = "class TableDefinition\n  def column(name)\n    name\n  end\n\n  def index(name)\n    name\n  end\nend\n";
    let files: Files = vec![
        ("lib/reference.rb", REFERENCE.to_string()),
        ("lib/tables.rb", "class TableDefinition\n  def references(name)\n    Reference.new.add_to(self)\n  end\nend\n".to_string()),
        ("lib/columns.rb", columns.to_string()),
        ("lib/guess.rb", GUESS.to_string()),
    ];
    let (cg, _dir) = converges(&files, put("lib/columns.rb", &format!("# a note\n{columns}")));
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn recorded_a_flow_moves_off_a_method_that_moves_to_another_class_of_the_file() {
    let index_def = "  def index(name)\n    name\n  end\n";
    let column = "class TableDefinition\n  def column(name)\n    name\n  end\n";
    let spare = "class Spare\n  def spare\n    1\n  end\n";
    let files: Files = vec![
        ("lib/reference.rb", REFERENCE.to_string()),
        ("lib/tables.rb", "class TableDefinition\n  def references(name)\n    Reference.new.add_to(self)\n  end\nend\n".to_string()),
        ("lib/columns.rb", format!("{column}\n{index_def}end\n\n{spare}end\n")),
        ("lib/guess.rb", GUESS.to_string()),
    ];
    let (cg, _dir) = converges(&files, put("lib/columns.rb", &format!("{column}end\n\n{spare}\n{index_def}end\n")));
    assert_eq!(add_to_calls(&cg), map(&[("table.column", "TableDefinition::column"), ("table.index", "Table::index")]));
}

#[test]
fn recorded_keeps_a_flow_when_a_second_call_passes_the_same_class() {
    let (cg, _dir) = converges(&with_caller(&[]), put("lib/caller.rb", &caller_running("Reference.new.add_to(TableDefinition.new)")));
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn recorded_takes_a_flow_away_when_a_new_call_passes_something_unknown() {
    let (cg, _dir) = converges(&with_caller(&[]), put("lib/caller.rb", &caller_running("Reference.new.add_to(name)")));
    assert_eq!(add_to_calls(&cg), guessed());
}

#[test]
fn recorded_takes_a_flow_away_and_brings_it_back_when_the_only_mention_comes_and_goes() {
    let mention = caller_running("name.send(:add_to, name)");
    let (cg, _dir) = converges(&with_caller(&[]), put("lib/caller.rb", &mention));
    assert_eq!(add_to_calls(&cg), guessed());
    let (cg, _dir) = converges(&with_caller(&[("lib/caller.rb", mention)]), put("lib/caller.rb", CALLER));
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn recorded_counts_a_mention_in_a_class_body() {
    let in_body = CALLER.replace("class Caller\n", "class Caller\n  alias_method :other_add_to, :add_to\n");
    let (cg, _dir) = converges(&with_caller(&[]), put("lib/caller.rb", &in_body));
    assert_eq!(add_to_calls(&cg), guessed());
    let (cg, _dir) = converges(&with_caller(&[("lib/caller.rb", in_body)]), put("lib/caller.rb", CALLER));
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn recorded_stops_counting_a_mention_when_its_file_goes() {
    let (cg, _dir) = converges(&with_caller(&[("lib/mention.rb", "puts \"add_to\"\n".to_string())]), remove("lib/mention.rb"));
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn recorded_finds_the_flow_of_a_method_that_had_no_call_until_this_one() {
    let (cg, _dir) = converges(&with_caller(&[("lib/tables.rb", tables_without_site())]), put("lib/tables.rb", TABLES));
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn recorded_finds_the_flow_of_a_method_a_file_starts_to_declare() {
    let without = with_caller(&[("lib/reference.rb", "class Reference\n  def other(name)\n    name\n  end\nend\n".to_string())]);
    let (cg, _dir) = converges(&without, put("lib/reference.rb", &REFERENCE.replace("class Reference\n", "class Reference\n  def other(name)\n    name\n  end\n\n")));
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn recorded_finds_the_flow_a_removed_call_was_all_that_stood_in_the_way_of() {
    let (cg, _dir) = converges(&with_caller(&[("lib/caller.rb", caller_running("Reference.new.add_to(name)"))]), put("lib/caller.rb", CALLER));
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn recorded_finds_the_flow_a_call_that_now_fits_no_longer_stood_in_the_way_of() {
    let (cg, _dir) = converges(&with_caller(&[("lib/caller.rb", caller_running("name.add_to(name)"))]), put("lib/caller.rb", &caller_running("name.add_to(name, name)")));
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn recorded_an_edit_of_a_same_named_method_to_super_stops_the_flow() {
    let caller = format!("{CALLER}\nclass Sub < Reference\n  def add_to(table)\n    table\n  end\nend\n");
    let edited = caller.replace("    table\n  end\nend\n", "    super\n  end\nend\n");
    let (cg, _dir) = converges(&with_caller(&[("lib/caller.rb", caller)]), put("lib/caller.rb", &edited));
    assert_eq!(add_to_calls(&cg), guessed());
}

#[test]
fn recorded_follows_a_flow_through_a_method_reached_by_a_chain() {
    let files: Files = vec![
        ("lib/reference.rb", "class Reference\n  def add_to(table)\n    table.column(:a)\n    table.index(:a)\n  end\n\n  def relay(table)\n    add_to(table)\n  end\nend\n".to_string()),
        ("lib/tables.rb", TABLES.replace("Reference.new.add_to(self)", "Reference.new.relay(name)")),
        ("lib/guess.rb", GUESS.to_string()),
    ];
    let (cg, _dir) = converges(&files, put("lib/tables.rb", &TABLES.replace("Reference.new.add_to(self)", "Reference.new.relay(self)")));
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn recorded_follows_a_flow_through_a_function_that_only_passes_it_on() {
    let files: Files = vec![
        ("lib/reference.rb", REFERENCE.to_string()),
        ("lib/relay.rb", "def relay(table)\n  Reference.new.add_to(table)\nend\n".to_string()),
        ("lib/tables.rb", TABLES.replace("Reference.new.add_to(self)", "relay(name)")),
        ("lib/guess.rb", GUESS.to_string()),
    ];
    let (cg, _dir) = converges(&files, put("lib/tables.rb", &TABLES.replace("Reference.new.add_to(self)", "relay(self)")));
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn recorded_sees_a_call_of_a_method_the_sync_just_defined() {
    let part = "class TableDefinition\n  def part\n    1\n  end\nend\n";
    let files = with(&[
        ("lib/reference.rb", "class Reference\n  def add_to(table)\n    table.resize(:a)\n  end\nend\n".to_string()),
        ("lib/part.rb", part.to_string()),
    ]);
    let (cg, _dir) = converges(&files, put("lib/part.rb", &part.replace("  def part", "  def resize(name)\n    name\n  end\n\n  def part")));
    assert_eq!(add_to_calls(&cg), map(&[("table.resize", "TableDefinition::resize")]));
}

#[test]
fn recorded_sees_a_method_the_flow_reaches_appear_on_its_class() {
    let (cg, _dir) = converges(&with(&[("lib/tables.rb", tables_without_index())]), put("lib/tables.rb", TABLES));
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn recorded_rules_out_a_guess_and_takes_that_back_as_the_class_starts_and_stops_reading_the_name() {
    let (cg, _dir) = converges(&with(&[("lib/tables.rb", tables_without_index())]), put("lib/tables.rb", &tables_reading_index()));
    assert_eq!(add_to_calls(&cg), column_only());
    let (cg, _dir) = converges(&with(&[("lib/tables.rb", tables_reading_index())]), put("lib/tables.rb", &tables_without_index()));
    assert_eq!(add_to_calls(&cg), map(&[("table.column", "TableDefinition::column"), ("table.index", "Table::index")]));
}

#[test]
fn recorded_does_the_same_when_another_file_of_the_class_reads_the_name() {
    let part = "class TableDefinition\n  def part\n    1\n  end\nend\n";
    let files = with(&[("lib/tables.rb", tables_without_index()), ("lib/part.rb", part.to_string())]);
    let (cg, _dir) = converges(&files, put("lib/part.rb", &part.replace("  def part", "  attr_reader :index\n\n  def part")));
    assert_eq!(add_to_calls(&cg), column_only());
}

#[test]
fn recorded_keeps_the_flow_across_changes_to_classes_constants_and_ancestry() {
    let cases: Vec<(&str, Files, &'static str, String)> = vec![
        ("a class it reads appears", base(), "lib/new.rb", "class Unseen\nend\n".to_string()),
        ("a constant appears", with_caller(&[]), "lib/caller.rb", format!("LIMIT = 1\n{CALLER}")),
        ("a superclass changes", with_caller(&[("lib/caller.rb", "class A\nend\n\nclass B < A\nend\n".to_string())]), "lib/caller.rb", "class A\nend\n\nclass B\nend\n".to_string()),
        ("a mixin goes", with_caller(&[("lib/caller.rb", "module M\nend\n\nclass B\n  include M\nend\n".to_string())]), "lib/caller.rb", "module M\nend\n\nclass B\nend\n".to_string()),
        (
            "two mixins change places",
            with_caller(&[("lib/caller.rb", "module M\nend\nmodule N\nend\n\nclass B\n  include M\n  include N\nend\n".to_string())]),
            "lib/caller.rb",
            "module M\nend\nmodule N\nend\n\nclass B\n  include N\n  include M\nend\n".to_string(),
        ),
        ("a module becomes a class", with_caller(&[("lib/caller.rb", "module M\nend\n".to_string())]), "lib/caller.rb", "class M\nend\n".to_string()),
    ];
    for (shape, files, rel, content) in cases {
        let (cg, _dir) = converges(&files, put(rel, &content));
        assert_eq!(add_to_calls(&cg), flowed(), "{shape}");
    }
}

const HOLDER_INIT: &str = "  def initialize\n    @table = TableDefinition.new\n  end\n\n";

fn holder_here() -> String {
    ivar_site().replace("Reference.new.add_to(name)", "name").replace("class Holder\n", &format!("class Holder\n{HOLDER_INIT}"))
}

#[test]
fn recorded_follows_an_instance_variable_when_its_initialize_moves_into_another_file() {
    let files: Files = vec![("lib/reference.rb", REFERENCE.to_string()), ("lib/guess.rb", GUESS.to_string()), ("lib/tables.rb", holder_here())];
    let (cg, _dir) = converges(&files, put("lib/holder.rb", &format!("class Holder\n{HOLDER_INIT}end\n")));
    assert_eq!(add_to_calls(&cg), guessed());
}

#[test]
fn recorded_follows_an_instance_variable_when_the_other_initialize_of_its_class_goes() {
    let files: Files = vec![
        ("lib/reference.rb", REFERENCE.to_string()),
        ("lib/guess.rb", GUESS.to_string()),
        ("lib/tables.rb", holder_here()),
        ("lib/holder.rb", format!("class Holder\n{HOLDER_INIT}  def other(name)\n    name\n  end\nend\n")),
    ];
    let (cg, _dir) = converges(&files, put("lib/holder.rb", "class Holder\n  def other(name)\n    name\n  end\nend\n"));
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn recorded_counts_a_flow_that_moves_a_call_for_the_first_time() {
    let tables = "class TableDefinition\n  def references(name)\n    Reference.new.add_to(self)\n  end\n\n  def both(name)\n    name\n  end\n\n  def column(name)\n    name\n  end\nend\n";
    let reference = "class Reference\n  def add_to(table)\n    table.both(:a)\n  end\nend\n";
    let files: Files = vec![
        ("lib/reference.rb", reference.to_string()),
        ("lib/tables.rb", tables.to_string()),
        ("lib/guess.rb", GUESS.to_string()),
        ("lib/caller.rb", caller_running("name.send(:add_to, name)")),
    ];
    let (cg, _dir) = converges(&files, put("lib/reference.rb", &reference.replace("table.both(:a)", "table.both(:a)\n    table.column(:a)")));
    assert_eq!(add_to_calls(&cg).get("table.column"), Some(&vec!["Table::column".to_string()]));
}

#[test]
fn recorded_finds_a_flow_an_ancestor_gave_a_same_named_method_a_say_in() {
    let caller = "class Sub < Object\n  def add_to(table)\n    table\n  end\nend\n\nclass Call\n  def go\n    Sub.new.add_to(name)\n  end\nend\n";
    let (cg, _dir) = converges(&with_caller(&[("lib/caller.rb", caller.to_string())]), put("lib/caller.rb", &caller.replace("class Sub < Object", "class Sub < Reference")));
    assert_eq!(add_to_calls(&cg), guessed());
}

#[test]
fn recorded_a_sync_through_many_files_converges() {
    let many: Vec<&'static str> = (0..70).map(|i| &*Box::leak(format!("lib/many/f{i}.rb").into_boxed_str())).collect();
    let mut files = with_caller(&[]);
    files.extend(many.iter().map(|rel| (*rel, "# one\n".to_string())));
    let (cg, _dir) = converges(&files, |root: &Path| {
        for rel in &many {
            write(root, rel, "# two\n");
        }
    });
    assert_eq!(add_to_calls(&cg), flowed());
}

#[test]
fn a_sync_of_another_language_leaves_the_flow() {
    let (cg, _dir) = converges(&with(&[("lib/app.ts", "export const a = 1;\n".to_string())]), put("lib/app.ts", "export const a = 2;\n"));
    assert_eq!(add_to_calls(&cg), flowed());
}
