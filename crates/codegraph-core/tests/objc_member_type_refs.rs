//! An Objective-C struct references the types its members are declared with, and an alias
//! references what it names (upstream 79c25b76). Only a type can be named there; a type an
//! `.m` file defines is that file's own, and a class's node in another `.m` yields to its
//! header's.
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

fn holder(cg: &CodeGraph, name: &str, file: Option<&str>) -> Node {
    let mut found: Vec<Node> = cg
        .get_nodes_by_name(name)
        .unwrap()
        .into_iter()
        .filter(|n| {
            matches!(n.kind, NodeKind::Struct | NodeKind::Union | NodeKind::TypeAlias)
                && file.is_none_or(|f| rel(&n.file_path).ends_with(f))
        })
        .collect();
    assert_eq!(found.len(), 1, "{name} {file:?}: {found:#?}");
    found.remove(0)
}

fn referenced(cg: &CodeGraph, name: &str, file: Option<&str>) -> Vec<String> {
    let node = holder(cg, name, file);
    let mut labels: Vec<String> = cg
        .get_outgoing_edges(&node.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::References)
        .map(|e| {
            let target = cg.get_node(&e.target).unwrap().unwrap();
            format!("{} {} @{}", target.kind, target.name, rel(&target.file_path))
        })
        .collect();
    labels.sort();
    labels
}

const TYPES: &str = "\
@interface NSObject
@end
@interface NSString : NSObject
@end
@protocol Delegate
@end
typedef struct pause_event { int paused; } pause_event;
struct client { int fd; };
enum Color { Red, Green };
typedef union { int i; } value;
typedef long MyInt;
";

#[test]
fn links_a_member_declared_by_value_pointer_tag_and_callback() {
    let (_dir, cg) = index(&[
        ("Types.h", TYPES),
        (
            "Server.h",
            "\
#import \"Types.h\"
struct server {
    pause_event pauses[3];
    struct client *current;
    enum Color color;
    value in_union;
    MyInt count;
    NSString *name;
    pause_event (*callback)(MyInt);
    int port;
    char *text;
};
typedef struct server Server;
@interface Controller : NSObject
@end
",
        ),
    ]);
    assert_eq!(
        referenced(&cg, "server", None),
        [
            "class NSString @Types.h",
            "enum Color @Types.h",
            "struct client @Types.h",
            "struct pause_event @Types.h",
            "struct pause_event @Types.h",
            "type_alias MyInt @Types.h",
            "type_alias MyInt @Types.h",
            "union value @Types.h",
        ]
    );
    assert_eq!(referenced(&cg, "Server", None), ["struct server @Server.h"]);
}

#[test]
fn names_no_extends_edge_and_links_no_function_or_variable_of_the_name() {
    let (_dir, cg) = index(&[
        (
            "Types.h",
            &format!("{TYPES}void helper(void);\nint counter;\n"),
        ),
        (
            "Holder.h",
            "\
#import \"Types.h\"
struct holder { helper *a; counter *b; pause_event c; };
@interface H : NSObject
@end
",
        ),
    ]);
    let node = holder(&cg, "holder", None);
    let edges = cg.get_outgoing_edges(&node.id).unwrap();
    assert!(edges.iter().all(|e| e.kind != EdgeKind::Extends));
    let names: Vec<String> = edges
        .iter()
        .filter(|e| e.kind == EdgeKind::References)
        .map(|e| cg.get_node(&e.target).unwrap().unwrap().name)
        .collect();
    assert_eq!(names, ["pause_event"]);
}

#[test]
fn a_type_an_m_file_defines_is_its_own() {
    let (_dir, cg) = index(&[
        ("Types.h", TYPES),
        (
            "One.m",
            "\
#import \"Types.h\"
struct local { int a; };
struct mine { struct local *l; };
@implementation NSString
@end
",
        ),
        ("Two.m", "#import \"Types.h\"\nstruct other { struct local *l; NSString *s; };\n"),
    ]);
    assert_eq!(referenced(&cg, "mine", None), ["struct local @One.m"]);
    assert_eq!(referenced(&cg, "other", None), ["class NSString @Types.h"]);
}

#[test]
fn a_member_type_links_once_its_file_is_added() {
    let dir = tempfile::tempdir().unwrap();
    write_files(
        dir.path(),
        &[("Holder.h", "#import \"Late.h\"\nstruct holder { late_t *l; };\n@interface H\n@end\n")],
    );
    let mut cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    assert!(referenced(&cg, "holder", None).is_empty());

    write_files(
        dir.path(),
        &[("Late.h", "typedef struct late { int x; } late_t;\n@interface Late\n@end\n")],
    );
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(referenced(&cg, "holder", None), ["struct late_t @Late.h"]);
    cg.close();
}

#[test]
fn a_class_is_the_one_in_its_own_header_before_a_category_in_another() {
    let (_dir, cg) = index(&[
        ("Base.h", "@interface NSObject\n@end\n"),
        ("Widget.h", "#import \"Base.h\"\n@interface Widget : NSObject\n@end\n"),
        ("Widget+Extra.h", "#import \"Widget.h\"\n@interface Widget (Extra)\n@end\n"),
        ("Aaa.h", "#import \"Widget.h\"\n@interface Widget (Aaa)\n@end\n"),
        ("Holder.h", "#import \"Widget.h\"\nstruct holder { Widget *w; };\n@interface Holder : NSObject\n@end\n"),
    ]);
    assert_eq!(referenced(&cg, "holder", None), ["class Widget @Widget.h"]);
}
