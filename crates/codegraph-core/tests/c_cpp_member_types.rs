//! A C or C++ class, struct, union, or alias references the types its members are declared
//! with (upstream a0174cee). Impact on a type includes what holds it. A function, macro, or
//! variable of that name is not a target.
use codegraph_core::{CodeGraph, IndexOptions};
use codegraph_types::{EdgeKind, Node, NodeKind};
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

fn rel(path: &str) -> String {
    path.replace('\\', "/")
}

fn nodes_named<'a>(cg: &'a CodeGraph, name: &str) -> Vec<Node> {
    cg.get_nodes_by_name(name)
        .unwrap()
        .into_iter()
        .filter(|n| n.kind != NodeKind::File && n.kind != NodeKind::Import)
        .collect()
}

fn named(cg: &CodeGraph, name: &str, kind: Option<NodeKind>, file: Option<&str>) -> Node {
    let found: Vec<Node> = nodes_named(cg, name)
        .into_iter()
        .filter(|n| kind.is_none_or(|k| n.kind == k) && file.is_none_or(|f| rel(&n.file_path).ends_with(f)))
        .collect();
    assert_eq!(found.len(), 1, "{kind:?} {name} {file:?}: {found:#?}");
    found.into_iter().next().unwrap()
}

fn referenced(cg: &CodeGraph, holder: &Node) -> Vec<String> {
    let mut labels: Vec<String> = cg
        .get_outgoing_edges(&holder.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::References)
        .map(|e| {
            let target = cg.get_node(&e.target).unwrap().unwrap();
            format!("{} {} @{}", target.kind, target.qualified_name, rel(&target.file_path))
        })
        .collect();
    labels.sort();
    labels.dedup();
    labels
}

fn impact_names(cg: &CodeGraph, node: &Node, depth: u32) -> Vec<String> {
    let mut names: Vec<String> = cg
        .get_impact_radius(&node.id, depth)
        .unwrap()
        .nodes
        .values()
        .filter(|n| n.id != node.id)
        .map(|n| n.name.clone())
        .collect();
    names.sort();
    names.dedup();
    names
}

#[test]
fn a_holder_references_the_types_its_members_and_aliases_name() {
    let (_dir, cg) = index(&[
        (
            "server.h",
            "\
typedef struct pause_event { int paused_actions; long end; } pause_event;
struct client { int fd; };
struct redisServer {
    pause_event client_pause_per_purpose[3];
    struct client *current_client;
    int port;
};
typedef struct redisObject robj;
typedef struct dict dict;
struct redisObject { int type; };
struct dict { int size; };
struct db { robj *key; dict *keys; struct dict *tagged; };
",
        ),
        (
            "a.h",
            "\
typedef struct ev { int x; } ev;
struct holder {
    ev by_value;
    ev *by_pointer;
    ev by_array[3];
    ev (*by_fn)(int);
    ev *(*by_parameter)(ev e);
};
struct owner { struct holder h; };
#define u32 unsigned int
struct plain_holder {
    u32 mask;
    int n;
    size_t len;
    uint64_t id;
    wchar_t c;
    FILE *out;
};
",
        ),
        ("deps/compat/C99/stdbool.h", "typedef int _Bool;\ntypedef unsigned long long uintmax_t;\n"),
        ("src/flag.h", "struct flag_holder { _Bool flag; uintmax_t widest; };\n"),
        (
            "self.h",
            "\
typedef struct node { struct node *next; node_t *other; } node_t;
typedef struct listNode { struct listNode *prev; } listNode;
struct plain { struct plain *next; };
",
        ),
    ]);

    let server = named(&cg, "redisServer", Some(NodeKind::Struct), Some("server.h"));
    assert_eq!(
        referenced(&cg, &server),
        ["struct client @server.h", "struct pause_event @server.h"]
    );
    assert!(cg.get_outgoing_edges(&server.id).unwrap().iter().all(|e| e.kind != EdgeKind::Extends));
    assert_eq!(impact_names(&cg, &named(&cg, "pause_event", None, Some("server.h")), 2), ["redisServer"]);
    assert_eq!(impact_names(&cg, &named(&cg, "client", None, Some("server.h")), 2), ["redisServer"]);

    let holder = named(&cg, "holder", Some(NodeKind::Struct), Some("a.h"));
    let lines: Vec<u32> = cg
        .get_outgoing_edges(&holder.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::References)
        .map(|e| e.line.expect("member type ref has a line"))
        .collect();
    assert_eq!(lines, [3, 4, 5, 6, 7, 7], "{lines:?}");
    assert_eq!(impact_names(&cg, &named(&cg, "ev", None, Some("a.h")), 2), ["holder", "owner"]);
    assert!(nodes_named(&cg, "by_value").is_empty());
    assert!(cg.get_nodes_by_kind(NodeKind::Field).unwrap().is_empty());

    let db = named(&cg, "db", Some(NodeKind::Struct), Some("server.h"));
    assert_eq!(
        referenced(&cg, &db),
        ["struct dict @server.h", "type_alias robj @server.h"]
    );
    assert_eq!(referenced(&cg, &named(&cg, "robj", Some(NodeKind::TypeAlias), None)), ["struct redisObject @server.h"]);
    assert_eq!(impact_names(&cg, &named(&cg, "redisObject", None, None), 2), ["db", "robj"]);

    assert!(referenced(&cg, &named(&cg, "plain_holder", None, None)).is_empty());
    assert!(referenced(&cg, &named(&cg, "flag_holder", None, None)).is_empty());
    for name in ["node_t", "listNode", "plain"] {
        assert!(referenced(&cg, &named(&cg, name, None, Some("self.h"))).is_empty(), "{name}");
    }
}

#[test]
fn only_a_type_the_file_can_name_is_linked() {
    let (_dir, cg) = index(&[
        (
            "a.h",
            "\
struct holder {
    log *to;
    struct stat st;
    counter hits;
    limit cap;
};
",
        ),
        (
            "a.c",
            "\
#include \"a.h\"
#define limit 10
int stat(const char *path) { return 0; }
void log(const char *m) {}
int counter = 3;
",
        ),
        ("deps/hiredis/dict.h", "typedef struct dictEntry { void *key; } dictEntry;\nstruct vendored { dictEntry *head; };\n"),
        ("src/dict.h", "typedef struct dictEntry dictEntry;\nstruct dict { dictEntry **table; };\n"),
        ("src/dict.c", "#include \"dict.h\"\nstruct dictEntry { void *key; int v; };\nstruct walker { dictEntry *at; };\n"),
        ("src/kvstore.c", "#include \"dict.h\"\nstruct kvstore { dictEntry *rehashing; };\n"),
        ("b.c", "struct holder_b { struct state *s; };\n"),
        ("state.c", "struct state { int a; };\nint f(void) { return 0; }\n"),
        ("nest.h", "struct outer { struct inner { int x; } i; };\nstruct other { struct inner *p; };\n"),
        (
            "unnamed.h",
            "\
typedef struct { int x; } ev;
struct holder_u {
    union { ev *in_anonymous; int i; };
    struct { ev *in_unnamed; } named_member;
};
",
        ),
        (
            "include/types.h",
            "\
namespace other { class Foo {}; class Slice {}; }
namespace ns {
class Foo {};
class Slice {};
template <typename K, typename V> class Map {};
enum class Color { Red };
}
class Global {};
",
        ),
        (
            "src/holder.h",
            "\
#include \"types.h\"
namespace ns {
class Outer {
  struct Nested {};
  enum Mode { A, B };
  Foo bare;
  ns::Foo qualified;
  other::Foo elsewhere;
  ::Global global;
  Nested nested;
  Mode mode;
  Color color;
  const Slice& reference;
  Map<int, Foo*> map;
  std::vector<Slice> items;
  std::unique_ptr<other::Slice> owner;
};
}
",
        ),
        (
            "include/using.h",
            "\
namespace db { class Options {}; class Slice {}; class string {}; }
namespace internal { class Slice {}; }
",
        ),
        ("src/use_a.h", "#include \"using.h\"\nusing db::Slice;\nnamespace d = db;\nclass A { Slice slice; d::Options options; };\n"),
        ("src/use_b.h", "#include \"using.h\"\nusing namespace db;\nclass B { Options options; };\n"),
        ("src/use_c.h", "#include \"using.h\"\n#include <string>\nusing std::string;\nclass C { string text; };\n"),
        (
            "include/port.h",
            "\
namespace db { namespace port { using pthread_key_t = int; class Latch {}; } }
class OuterHash { public: using Hash = int; };
",
        ),
        (
            "src/port_user.h",
            "\
#include \"port.h\"
namespace db {
class PortUser { pthread_key_t key; Hash hash; port::Latch latch; };
}
",
        ),
        (
            "include/t.h",
            "\
class T {};
class Element {};
class value_type {};
template <class T, typename Element = int>
class Box {
  T item;
  Element* pointer;
  typename T::value_type inner;
  Element other[2];
};
template <class T> class Tree {
  class Node { T value; };
  Node* root;
};
",
        ),
        ("include/table.h", "namespace db {\nclass Table {\n private:\n  struct Rep;\n  Rep* rep_;\n};\n}\n"),
        ("src/table.cc", "#include \"table.h\"\nnamespace db {\nstruct Table::Rep { int n; };\n}\n"),
        ("src/other.cc", "#include \"table.h\"\nnamespace other {\nstruct Rep { int m; };\n}\n"),
    ]);

    assert!(referenced(&cg, &named(&cg, "holder", None, Some("a.h"))).is_empty());

    let mine = "struct dictEntry @src/dict.c";
    assert_eq!(referenced(&cg, &named(&cg, "dict", Some(NodeKind::Struct), Some("src/dict.h"))), [mine]);
    assert_eq!(referenced(&cg, &named(&cg, "walker", None, None)), [mine]);
    assert_eq!(referenced(&cg, &named(&cg, "kvstore", None, None)), [mine]);
    assert_eq!(referenced(&cg, &named(&cg, "vendored", None, None)), ["struct dictEntry @deps/hiredis/dict.h"]);
    assert!(referenced(&cg, &named(&cg, "holder_b", None, None)).is_empty());

    assert_eq!(referenced(&cg, &named(&cg, "outer", None, Some("nest.h"))), ["struct outer::inner @nest.h"]);
    assert_eq!(referenced(&cg, &named(&cg, "other", None, Some("nest.h"))), ["struct outer::inner @nest.h"]);
    assert_eq!(referenced(&cg, &named(&cg, "holder_u", None, None)), ["struct ev @unnamed.h"]);
    assert!(impact_names(&cg, &named(&cg, "ev", None, Some("unnamed.h")), 2).contains(&"holder_u".to_string()));

    assert_eq!(
        referenced(&cg, &named(&cg, "Outer", Some(NodeKind::Class), Some("src/holder.h"))),
        [
            "class Global @include/types.h",
            "class ns::Foo @include/types.h",
            "class ns::Map @include/types.h",
            "class ns::Slice @include/types.h",
            "class other::Foo @include/types.h",
            "class other::Slice @include/types.h",
            "enum ns::Color @include/types.h",
            "enum ns::Outer::Mode @src/holder.h",
            "struct ns::Outer::Nested @src/holder.h",
        ]
    );
    assert_eq!(
        referenced(&cg, &named(&cg, "A", Some(NodeKind::Class), Some("src/use_a.h"))),
        ["class db::Options @include/using.h", "class db::Slice @include/using.h"]
    );
    assert_eq!(referenced(&cg, &named(&cg, "B", Some(NodeKind::Class), None)), ["class db::Options @include/using.h"]);
    assert!(referenced(&cg, &named(&cg, "C", Some(NodeKind::Class), None)).is_empty());
    assert_eq!(referenced(&cg, &named(&cg, "PortUser", None, None)), ["class db::port::Latch @include/port.h"]);
    assert!(referenced(&cg, &named(&cg, "Box", Some(NodeKind::Class), None)).is_empty());
    assert!(referenced(&cg, &named(&cg, "Node", Some(NodeKind::Class), None)).is_empty());
    assert_eq!(referenced(&cg, &named(&cg, "Tree", Some(NodeKind::Class), None)), ["class Tree::Node @include/t.h"]);
    assert_eq!(referenced(&cg, &named(&cg, "Table", Some(NodeKind::Class), None)), ["struct db::Table::Rep @src/table.cc"]);
}

/// The declaration each `references` edge lands on, by that declaration's start line.
fn target_lines(cg: &CodeGraph, holder: &Node) -> Vec<u32> {
    let mut lines: Vec<u32> = cg
        .get_outgoing_edges(&holder.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::References)
        .map(|e| cg.get_node(&e.target).unwrap().unwrap().start_line)
        .collect();
    lines.sort();
    lines
}

#[test]
fn alias_templates_if_arms_and_function_locals_stay_the_declaration_in_force() {
    let (_dir, cg) = index(&[(
        "include/containers.h",
        "\
namespace db {
template <class K, class V> using UnorderedMap = Other<K, V>;
class Key {};
class Holder { UnorderedMap<int, Key*> map; };
}
",
    )]);
    assert_eq!(
        referenced(&cg, &named(&cg, "Holder", Some(NodeKind::Class), None)),
        [
            "class db::Key @include/containers.h",
            "type_alias db::UnorderedMap @include/containers.h",
        ]
    );

    let (_dir, cg) = index(&[(
        "src/a.h",
        "\
#ifdef WIDE
typedef long width_t;
struct first { width_t w; };
#else
typedef int width_t;
struct second { width_t w; };
#endif
",
    )]);
    assert_eq!(target_lines(&cg, &named(&cg, "first", None, None)), [2]);
    assert_eq!(target_lines(&cg, &named(&cg, "second", None, None)), [5]);

    let (_dir, cg) = index(&[(
        "src/a_test.cc",
        "\
namespace db {
TEST_F(Suite, First) {
  struct Item { int a; };
  struct Box { Item item; };
}
TEST_F(Suite, Second) {
  struct Item { int b; };
  struct Crate { Item item; };
}
}
",
    )]);
    assert_eq!(target_lines(&cg, &named(&cg, "Box", None, None)), [3]);
    assert_eq!(target_lines(&cg, &named(&cg, "Crate", None, None)), [7]);
}

#[test]
fn sync_links_a_type_added_after_the_holder_and_keeps_it_across_an_edit() {
    let dir = tempfile::tempdir().unwrap();
    write_files(dir.path(), &[("src/holder.h", "#include \"ev.h\"\nstruct holder { ev *e; };\n")]);
    let mut cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    assert!(referenced(&cg, &named(&cg, "holder", None, None)).is_empty());

    write_files(dir.path(), &[("src/ev.h", "typedef struct ev { int x; } ev;\n")]);
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(referenced(&cg, &named(&cg, "holder", None, None)), ["struct ev @src/ev.h"]);

    write_files(dir.path(), &[("src/ev.h", "/* moved */\n\ntypedef struct ev { int x; int y; } ev;\n")]);
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(referenced(&cg, &named(&cg, "holder", None, None)), ["struct ev @src/ev.h"]);
    assert_eq!(impact_names(&cg, &named(&cg, "ev", None, None), 2), ["holder"]);
    cg.close();
}
