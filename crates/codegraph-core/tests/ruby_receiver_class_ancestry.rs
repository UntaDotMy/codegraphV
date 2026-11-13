//! A Ruby call follows a class's real ancestors, in Ruby's order (upstream 087de4ca, #3169).
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;
use std::path::Path;

fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (relative, content) in files {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

fn files() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "lib/mapper.rb",
            r#"module ActionDispatch
  module Routing
    class Mapper
      module HttpHelpers
        def get(*paths)
          paths
        end
      end

      module Base
        def match(*paths)
          paths
        end
      end

      include Base
      include HttpHelpers
    end
  end
end
"#,
        ),
        (
            "test/routes_test.rb",
            r#"class RoutesTest
  attr_reader :mapper

  def setup
    @mapper = ActionDispatch::Routing::Mapper.new
  end

  def test_get
    mapper.get "/hello"
    mapper.match "/hello"
  end
end
"#,
        ),
        (
            "lib/journey.rb",
            r#"module ActionDispatch
  module Journey
    class Client
      def get(path)
        path
      end

      def match(path)
        path
      end
    end

    module Nodes
      class Node
        def name
          "node"
        end
      end

      class Base < Node
      end
    end
  end
end
"#,
        ),
        (
            "lib/active_record.rb",
            r#"module ActiveSupport
  module Concern
  end
end

module ActiveRecord
  module Persistence
    extend ActiveSupport::Concern

    module ClassMethods
      def create(attributes)
        new
      end
    end

    def save
      true
    end

    def save!
      true
    end
  end

  module Reflection
    def self.create(macro, name)
      macro
    end
  end

  module Transactions
    extend ActiveSupport::Concern

    def save
      super
    end
  end

  module Querying
    def count_by_sql(sql)
      sql
    end
  end

  module AttributeMethods
    extend ActiveSupport::Concern

    module ClassMethods
      def has_attribute?(attr_name)
        true
      end
    end

    def has_attribute?(attr_name)
      false
    end
  end

  module QueryCache
    module ClassMethods
      def cache
        yield
      end
    end
  end

  class Base
    extend QueryCache::ClassMethods
    extend Querying
    include Persistence
    include Reflection
    include Transactions
    include AttributeMethods
  end
end
"#,
        ),
        (
            "test/models.rb",
            r#"class Human < ActiveRecord::Base
end

class Company < ActiveRecord::Base
end

class Holder
  class Inner
    include ActiveRecord::Transactions
  end
end
"#,
        ),
        (
            "test/base_test.rb",
            r#"class BaseTest
  def test_name
    human = Human.new
    human.name
  end

  def test_named_receiver(human)
    human.name
  end

  def test_has_attribute
    company = Company.new
    company.has_attribute?("id")
  end

  def test_mixed_in_last(company)
    company.save
  end

  def test_only_mixin(company)
    company.save!
  end

  def test_class_method
    Company.create(name: "x")
    Company.count_by_sql("x")
  end

  def test_nested_mixin(holder)
    holder.save
  end
end
"#,
        ),
        (
            "lib/jar.rb",
            r#"module ActionDispatch
  class Cookies
    module ChainedCookieJars
      def signed
        self
      end
    end

    class CookieJar
      include ChainedCookieJars

      def self.build(request, cookies)
        jar = new(request)
        jar.update(cookies)
        jar
      end

      def delete(name)
        name
      end
    end
  end

  module TestProcess
    def cookies
      @cookie_jar ||= Cookies::CookieJar.build(@request, @request.cookies)
    end

    def session
      @request.session
    end

    def fresh_jar
      Cookies::CookieJar.new(@request)
    end

    def open_for(request)
      Cookies::CookieJar.new(request)
    end
  end
end

class TestCase
  include ActionDispatch::TestProcess
end

class Mailbox
  def delete(name)
    name
  end
end
"#,
        ),
        (
            "test/cookies_test.rb",
            r#"class CookiesTest < TestCase
  class Session
    def clear
    end
  end

  def test_signed
    cookies.signed[:favorite]
    cookies.delete(:favorite)
  end

  def test_shadowed(cookies)
    cookies.delete(:favorite)
  end

  def test_block
    [].each { |cookies| cookies.delete(:favorite) }
  end

  def test_assigned
    cookies = {}
    cookies.delete(:favorite)
  end

  def test_unmade
    session.clear
  end

  def test_fresh
    fresh_jar.delete(:favorite)
  end

  def test_argument
    open_for.delete(:favorite)
  end
end
"#,
        ),
        (
            "lib/cookies.rb",
            r#"module Sinatra
  module Cookies
    class Jar
      def empty?
        true
      end
    end
  end
end
"#,
        ),
        (
            "lib/tossing.rb",
            r#"class CookieTossing
  def bad_cookies
    @bad_cookies ||= []
  end

  def accepts?
    bad_cookies.empty?
  end
end
"#,
        ),
        (
            "lib/server.rb",
            r#"module IntegrationHelper
  class BaseServer
    def installed?
      true
    end
  end
end
"#,
        ),
        (
            "lib/record.rb",
            r#"module Records
  module ActiveRecord
    module Persistence
      def new_record?
        true
      end
    end
  end
end
"#,
        ),
        (
            "lib/checks.rb",
            r#"class Checks
  def run(server, record)
    server.installed?
    record.new_record?
  end
end
"#,
        ),
    ]
}

fn index() -> (tempfile::TempDir, CodeGraph) {
    let root = tempfile::tempdir().unwrap();
    write_files(root.path(), &files());
    let cg = CodeGraph::init_and_index(root.path().to_str().unwrap(), None).unwrap();
    (root, cg)
}

fn calls_of(cg: &CodeGraph, caller: &str, reference: Option<&str>) -> Vec<String> {
    let nodes = cg.store().get_all_nodes().unwrap();
    let source = nodes
        .iter()
        .find(|n| n.qualified_name == caller && matches!(n.kind, NodeKind::Method | NodeKind::Function))
        .unwrap_or_else(|| panic!("missing {caller}"));
    let mut out = Vec::new();
    for edge in cg.get_outgoing_edges(&source.id).unwrap() {
        if edge.kind != EdgeKind::Calls {
            continue;
        }
        if let Some(want) = reference {
            let got = edge.metadata.as_ref().and_then(|m| m.get("refName")).and_then(|v| v.as_str());
            if got != Some(want) {
                continue;
            }
        }
        let Some(target) = cg.get_node(&edge.target).unwrap() else { continue };
        out.push(target.qualified_name.clone());
    }
    out.sort();
    out
}

#[test]
fn a_receiver_named_after_a_class_follows_real_ancestors() {
    let (_root, cg) = index();
    assert_eq!(
        calls_of(&cg, "RoutesTest::test_get", Some("mapper.get")),
        vec!["ActionDispatch::Routing::Mapper::HttpHelpers::get".to_string()]
    );
    assert_eq!(
        calls_of(&cg, "RoutesTest::test_get", Some("mapper.match")),
        vec!["ActionDispatch::Routing::Mapper::Base::match".to_string()]
    );
    assert_eq!(calls_of(&cg, "BaseTest::test_name", Some("human.name")), Vec::<String>::new());
    assert_eq!(calls_of(&cg, "BaseTest::test_named_receiver", Some("human.name")), Vec::<String>::new());
    assert_eq!(
        calls_of(&cg, "BaseTest::test_mixed_in_last", Some("company.save")),
        vec!["ActiveRecord::Transactions::save".to_string()]
    );
    assert_eq!(
        calls_of(&cg, "BaseTest::test_only_mixin", Some("company.save!")),
        vec!["ActiveRecord::Persistence::save!".to_string()]
    );
    assert_eq!(calls_of(&cg, "BaseTest::test_nested_mixin", Some("holder.save")), Vec::<String>::new());
    cg.close();
}

#[test]
fn a_known_class_answers_with_its_instance_method_and_a_class_with_its_class_method() {
    let (_root, cg) = index();
    assert_eq!(
        calls_of(&cg, "BaseTest::test_has_attribute", Some("company.has_attribute?")),
        vec!["ActiveRecord::AttributeMethods::has_attribute?".to_string()]
    );
    assert_eq!(
        calls_of(&cg, "BaseTest::test_class_method", Some("Company.create")),
        vec!["ActiveRecord::Persistence::ClassMethods::create".to_string()]
    );
    assert_eq!(
        calls_of(&cg, "BaseTest::test_class_method", Some("Company.count_by_sql")),
        vec!["ActiveRecord::Querying::count_by_sql".to_string()]
    );
    cg.close();
}

#[test]
fn a_bare_method_that_hands_back_one_class_types_the_call() {
    let (_root, cg) = index();
    assert_eq!(
        calls_of(&cg, "CookiesTest::test_signed", Some("cookies.signed")),
        vec!["ActionDispatch::Cookies::ChainedCookieJars::signed".to_string()]
    );
    assert_eq!(
        calls_of(&cg, "CookiesTest::test_signed", Some("cookies.delete")),
        vec!["ActionDispatch::Cookies::CookieJar::delete".to_string()]
    );
    assert_eq!(
        calls_of(&cg, "CookiesTest::test_fresh", Some("fresh_jar.delete")),
        vec!["ActionDispatch::Cookies::CookieJar::delete".to_string()]
    );
    assert_eq!(calls_of(&cg, "CookiesTest::test_shadowed", Some("cookies.delete")), Vec::<String>::new());
    assert_eq!(calls_of(&cg, "CookiesTest::test_block", Some("cookies.delete")), Vec::<String>::new());
    assert_eq!(calls_of(&cg, "CookiesTest::test_assigned", Some("cookies.delete")), Vec::<String>::new());
    assert_eq!(calls_of(&cg, "CookiesTest::test_argument", Some("open_for.delete")), Vec::<String>::new());
    assert_eq!(
        calls_of(&cg, "CookiesTest::test_unmade", Some("session.clear")),
        vec!["CookiesTest::Session::clear".to_string()]
    );
    cg.close();
}

#[test]
fn a_core_predicate_is_not_a_method_the_receiver_shares_a_word_with() {
    let (_root, cg) = index();
    assert_eq!(calls_of(&cg, "CookieTossing::accepts?", Some("bad_cookies.empty?")), Vec::<String>::new());
    assert_eq!(
        calls_of(&cg, "Checks::run", Some("server.installed?")),
        vec!["IntegrationHelper::BaseServer::installed?".to_string()]
    );
    assert_eq!(
        calls_of(&cg, "Checks::run", Some("record.new_record?")),
        vec!["Records::ActiveRecord::Persistence::new_record?".to_string()]
    );
    cg.close();
}
