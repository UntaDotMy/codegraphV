//! Upstream cdfa31be: a Ruby call on an instance variable, `@writer.flush`, links the
//! method of the class the enclosing class's `initialize` constructs for it. A conditional,
//! two classes, `.tap`, a multiple assignment, an unset variable, a class the project does
//! not declare, a class method, and a module method stay unlinked.
use codegraph_core::CodeGraph;
use codegraph_types::EdgeKind;
use std::fs;

const WRITER: &str = "class CacheWriter\n  def flush\n    nil\n  end\n\n  def ready?\n    true\n  end\nend\n\nclass OtherWriter\n  def flush\n    nil\n  end\nend\n\nmodule Billing\n  class Card\n    def charge!(amount)\n      amount\n    end\n  end\n\n  class Ledger < Card\n  end\nend\n";

const HOLDER: &str = "class Holder\n  def initialize(key)\n    @writer = CacheWriter.new\n    @lazy ||= OtherWriter.new(\n      key\n    )\n    @either = key ? CacheWriter.new : OtherWriter.new\n    @twice = CacheWriter.new\n    @twice = OtherWriter.new if key\n    @ledger = Billing::Ledger.new key do |l|\n      l\n    end\n    @tapped = CacheWriter.new.tap { |w| w }\n    @outside = Logger.new($stdout)\n    @a, @b = CacheWriter.new, OtherWriter.new\n  end\n\n  def via_ivar\n    @writer.flush\n    @writer.ready?\n    @lazy.flush\n    @either.flush\n    @twice.flush\n    @ledger.charge!(1)\n    @tapped.flush\n    @outside.info(\"x\")\n    @b.flush\n    @unset.flush\n  end\n\n  def self.klass\n    @writer.flush\n  end\nend\n\nmodule Mixin\n  def mixed\n    @writer.flush\n  end\nend\n";

fn index() -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    for (rel, source) in [("app/writer.rb", WRITER), ("app/holder.rb", HOLDER)] {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

/// `qualifiedName:startLine` of every `calls` edge written on `line` of `file`.
fn calls_at(cg: &CodeGraph, file: &str, line: u32) -> Vec<String> {
    let mut calls: Vec<String> = Vec::new();
    for node in cg.get_nodes_by_file(file).unwrap() {
        for edge in cg.get_outgoing_edges(&node.id).unwrap() {
            if edge.kind == EdgeKind::Calls && edge.line == Some(line) {
                let target = cg.get_node(&edge.target).unwrap().expect("call target");
                calls.push(format!("{}:{}", target.qualified_name, target.start_line));
            }
        }
    }
    calls.sort();
    calls
}

#[test]
fn links_the_method_of_the_class_initialize_constructs() {
    let (_dir, cg) = index();
    assert_eq!(calls_at(&cg, "app/holder.rb", 19), vec!["CacheWriter::flush:2"]);
    assert_eq!(calls_at(&cg, "app/holder.rb", 20), vec!["CacheWriter::ready?:6"]);
    assert_eq!(calls_at(&cg, "app/holder.rb", 21), vec!["OtherWriter::flush:12"]);
    assert_eq!(calls_at(&cg, "app/holder.rb", 24), vec!["Billing::Card::charge!:19"]);
}

#[test]
fn types_nothing_initialize_leaves_ambiguous() {
    let (_dir, cg) = index();
    for line in [22, 23, 25, 27, 28] {
        assert_eq!(calls_at(&cg, "app/holder.rb", line), Vec::<String>::new(), "line {line}");
    }
}

#[test]
fn links_nothing_for_a_class_the_project_does_not_declare() {
    let (_dir, cg) = index();
    assert_eq!(calls_at(&cg, "app/holder.rb", 26), Vec::<String>::new());
}

#[test]
fn leaves_a_class_method_and_a_module_instance_variable_untyped() {
    let (_dir, cg) = index();
    assert_eq!(calls_at(&cg, "app/holder.rb", 32), Vec::<String>::new());
    assert_eq!(calls_at(&cg, "app/holder.rb", 38), Vec::<String>::new());
}

#[test]
fn ambiguous_undeclared_and_outside_class_calls_stay_unlinked() {
    let (_dir, cg) = index();
    for line in [22, 23, 25, 26, 27, 28, 32, 38] {
        assert_eq!(calls_at(&cg, "app/holder.rb", line), Vec::<String>::new(), "line {line}");
    }
}
