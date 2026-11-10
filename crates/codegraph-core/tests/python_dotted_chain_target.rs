//! A Python call through an imported class links the chain's last member.
//! A chain that leaves the class links nothing (upstream 206449b2, #3163).
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;
use std::path::Path;

const RESPONSE: &str = r#"class HttpResponse:
    @property
    def content(self):
        return b""

    @staticmethod
    def get_default():
        return None

    class Inner:
        objects = None

        def run(self):
            return 1

    def plain(self):
        return 2
"#;

fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (relative, content) in files {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

fn targets_of(body: &str) -> Vec<String> {
    let root = tempfile::tempdir().unwrap();
    let use_py = format!("from response import HttpResponse\n\n\ndef caller(self):\n    {body}\n");
    write_files(root.path(), &[("response.py", RESPONSE), ("use.py", use_py.as_str())]);
    let cg = CodeGraph::init_and_index(root.path().to_str().unwrap(), None).unwrap();
    let nodes = cg.store().get_all_nodes().unwrap();
    let caller = nodes
        .iter()
        .find(|n| n.name == "caller" && n.kind == NodeKind::Function)
        .expect("caller");
    let mut out = Vec::new();
    for edge in cg.get_outgoing_edges(&caller.id).unwrap() {
        if !matches!(edge.kind, EdgeKind::Calls | EdgeKind::Instantiates) {
            continue;
        }
        let Some(target) = cg.get_node(&edge.target).unwrap() else { continue };
        let kind = match edge.kind {
            EdgeKind::Calls => "calls",
            EdgeKind::Instantiates => "instantiates",
            _ => continue,
        };
        out.push(format!("{kind} {}", target.qualified_name));
    }
    cg.close();
    out.sort();
    out
}

#[test]
fn a_chain_that_leaves_the_class_links_nothing() {
    for body in [
        "HttpResponse.content.fset(self, 1)",
        "HttpResponse.get_default.cache_clear()",
        "HttpResponse.Inner.objects.create(x=1)",
        "HttpResponse.Inner.objects.defer(\"x\")[0]",
    ] {
        assert_eq!(targets_of(body), Vec::<String>::new(), "{body}");
    }
}

#[test]
fn a_chain_that_stays_on_the_class_links_its_last_member() {
    assert_eq!(targets_of("HttpResponse.plain(self)"), vec!["calls HttpResponse::plain".to_string()]);
    assert_eq!(targets_of("HttpResponse.get_default()"), vec!["calls HttpResponse::get_default".to_string()]);
    assert_eq!(targets_of("HttpResponse.Inner()"), vec!["instantiates HttpResponse::Inner".to_string()]);
    assert_eq!(targets_of("HttpResponse.Inner.run(self)"), vec!["calls HttpResponse::Inner::run".to_string()]);
}
