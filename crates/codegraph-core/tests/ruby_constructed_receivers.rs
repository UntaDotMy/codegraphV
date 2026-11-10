//! Upstream 3a4cd3bf: a Ruby call whose name ends in `!` or `?` links that method, and a
//! call on what `Const.new(...)` made links an instance method of the class constant lookup
//! finds. A class the project does not declare, a mixin read off a call, and a `?` name on a
//! value receiver stay unlinked.
use codegraph_core::CodeGraph;
use codegraph_types::EdgeKind;
use std::fs;

fn index(files: &[(&str, &str)]) -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    for (rel, source) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

fn fixture() -> (tempfile::TempDir, CodeGraph) {
    index(&[
        (
            "app/writer.rb",
            "class CacheWriter\n  def self.create(key)\n    new\n  end\n\n  def self.create!(key)\n    new\n  end\n\n  def write_cache(key)\n    key\n  end\n\n  def flush!\n    nil\n  end\n\n  def ready?\n    true\n  end\nend\n\nclass OtherWriter\n  def write_cache(key)\n    key\n  end\n\n  def flush!\n    nil\n  end\n\n  def ready?\n    false\n  end\nend\n",
        ),
        (
            "app/billing.rb",
            "module Billing\n  class Card\n    def charge!(amount)\n      amount\n    end\n  end\n\n  class Ledger < Card\n    def post\n      Card.new.charge!(1)\n    end\n  end\nend\n\nmodule Admin\n  class Card\n    def charge!(amount)\n      amount\n    end\n  end\nend\n",
        ),
        (
            "app/counter.rb",
            "class Counter\n  def bang\n    writer = CacheWriter.new\n    writer.flush!\n  end\n\n  def question\n    writer = CacheWriter.new\n    writer.ready?\n  end\n\n  def chained(key)\n    CacheWriter.new.write_cache(key)\n  end\n\n  def chained_args(key)\n    CacheWriter.new(key).write_cache(key)\n  end\n\n  def chained_bang\n    CacheWriter.new(1).flush!\n  end\n\n  def namespaced\n    Billing::Ledger.new.charge!(2)\n  end\n\n  def top_level\n    ::CacheWriter.new.ready?\n  end\n\n  def class_bang\n    CacheWriter.create!(1)\n  end\n\n  def outside(env)\n    Request.new(env).preferred_type\n  end\nend\n",
        ),
        (
            "lib/rack/show_exceptions.rb",
            "module Rack\n  module Protection\n    class ShowExceptions\n      def call(env)\n        Request.new(env).preferred_type(\"text/html\")\n      end\n    end\n  end\nend\n",
        ),
        (
            "lib/sinatra/request.rb",
            "module Sinatra\n  class Request\n    def preferred_type(*types)\n      types.first\n    end\n  end\nend\n",
        ),
        (
            "app/url_helper.rb",
            "class RouteSet\n  def url_for(options)\n    options\n  end\n\n  def url_helpers\n    Module.new\n  end\nend\n\nclass UrlForHelper\n  include RouteSet.new.url_helpers\nend\n\nclass UrlForTest\n  def test_url\n    UrlForHelper.new.url_for(1)\n  end\nend\n",
        ),
        (
            "lib/options.rb",
            "class InheritableOptions\n  def key?(key)\n    true\n  end\nend\n\nclass FlashHash\n  def key?(name)\n    false\n  end\nend\n\nclass Store\n  def merged(options)\n    options.key?(:namespace)\n  end\nend\n",
        ),
        (
            "app/twin.rb",
            "class Twin\n  def self.build\n    1\n  end\n\n  def build\n    2\n  end\nend\n\nclass TwinUser\n  def run\n    Twin.new.build\n    Twin.build\n  end\nend\n",
        ),
    ])
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
fn links_bang_and_question_methods_on_a_typed_local() {
    let (_dir, cg) = fixture();
    assert_eq!(calls_at(&cg, "app/counter.rb", 4), vec!["CacheWriter::flush!:14"]);
    assert_eq!(calls_at(&cg, "app/counter.rb", 9), vec!["CacheWriter::ready?:18"]);
    cg.close();
}

#[test]
fn links_a_bang_class_method() {
    let (_dir, cg) = fixture();
    assert_eq!(calls_at(&cg, "app/counter.rb", 33), vec!["CacheWriter::create!:6"]);
    cg.close();
}

#[test]
fn links_calls_on_what_new_made_by_constant_lookup() {
    let (_dir, cg) = fixture();
    assert_eq!(calls_at(&cg, "app/counter.rb", 13), vec!["CacheWriter::write_cache:10"]);
    assert_eq!(calls_at(&cg, "app/counter.rb", 17), vec!["CacheWriter::write_cache:10"]);
    assert_eq!(calls_at(&cg, "app/counter.rb", 21), vec!["CacheWriter::flush!:14"]);
    assert_eq!(calls_at(&cg, "app/counter.rb", 29), vec!["CacheWriter::ready?:18"]);
    assert_eq!(calls_at(&cg, "app/billing.rb", 10), vec!["Billing::Card::charge!:3"]);
    assert_eq!(calls_at(&cg, "app/counter.rb", 25), vec!["Billing::Card::charge!:3"]);
    assert_eq!(calls_at(&cg, "app/twin.rb", 13), vec!["Twin::build:6"]);
    assert_eq!(calls_at(&cg, "app/twin.rb", 14), vec!["Twin::build:2"]);
    cg.close();
}

#[test]
fn leaves_undeclared_classes_misread_mixins_and_predicate_guesses_unlinked() {
    let (_dir, cg) = fixture();
    assert_eq!(calls_at(&cg, "app/counter.rb", 37), Vec::<String>::new());
    assert_eq!(calls_at(&cg, "lib/rack/show_exceptions.rb", 5), Vec::<String>::new());
    assert_eq!(calls_at(&cg, "app/url_helper.rb", 17), Vec::<String>::new());
    assert_eq!(calls_at(&cg, "lib/options.rb", 15), Vec::<String>::new());
    cg.close();
}
