//! A Razor view's inline `<script>` links to the JavaScript it calls (upstream 97492056; port of
//! the extraction.test.ts cases it added and razor-script.test.ts's indexed case).
use codegraph_core::{CodeGraph, IndexOptions};
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;
use std::path::Path;

fn write(dir: &Path, rel: &str, content: &str) {
    let full = dir.join(rel);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, content).unwrap();
}

fn index(files: &[(&str, &str)]) -> (CodeGraph, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    for (rel, content) in files {
        write(dir.path(), rel, content);
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (cg, dir)
}

fn function(cg: &CodeGraph, name: &str) -> String {
    cg.get_nodes_by_kind(NodeKind::Function).unwrap().into_iter().find(|n| n.name == name).unwrap_or_else(|| panic!("no function {name}")).id
}

/// The non-script files of everything that calls or references the named function.
fn dependents(cg: &CodeGraph, name: &str) -> Vec<String> {
    cg.get_impact_radius(&function(cg, name), 1)
        .unwrap()
        .nodes
        .values()
        .map(|n| n.file_path.clone())
        .filter(|p| !p.ends_with(".js") && !p.ends_with(".ts"))
        .collect()
}

fn view_calls(cg: &CodeGraph, view: &str) -> Vec<codegraph_types::Edge> {
    let node = cg.get_nodes_by_name(view).unwrap().into_iter().next().unwrap_or_else(|| panic!("no view {view}"));
    cg.get_outgoing_edges(&node.id).unwrap().into_iter().filter(|e| e.kind == EdgeKind::Calls).collect()
}

#[test]
fn links_what_a_views_inline_script_calls_to_the_javascript_that_defines_it() {
    let (cg, _dir) = index(&[
        ("site.js", "export function showToast(msg) { return msg; }\n"),
        ("Views/Order.cshtml", "@model OrderViewModel\n<div>Order</div>\n<script>\n  import { showToast } from '../site.js';\n  function save() { showToast('saved'); }\n</script>\n"),
    ]);
    assert!(dependents(&cg, "showToast").contains(&"Views/Order.cshtml".to_string()));
    assert_eq!(view_calls(&cg, "Order").first().and_then(|e| e.line), Some(5), "a reference keeps its line in the view");
    cg.close();
}

#[test]
fn reads_a_global_helper_called_from_a_script_that_holds_razor() {
    let cart = [
        "@model CartModel",
        "<script asp-location=\"Footer\">",
        "    var id = @Model.Id;",
        "    var url = '@Url.Action(\"Add\", \"Cart\")';",
        "    var data = @Html.Raw(Json.Serialize(Model.Items));",
        "    @* displayNotice(\"a comment\"); *@",
        "    @if (Model.ShowNotice)",
        "    {",
        "        <text>",
        "        displayNotice(\"@T(\"Added\").Text\");",
        "        </text>",
        "    }",
        "    else",
        "    {",
        "        var hidden = Model.Compute();",
        "    }",
        "    $(function () { AjaxCart.add(url); });",
        "</script>",
    ]
    .join("\n");
    let (cg, _dir) = index(&[
        ("common.js", "function displayNotice(text) { return text; }\nvar AjaxCart = { add: function add(url) { return url; } };\n"),
        ("Views/Cart.cshtml", &cart),
    ]);
    assert!(dependents(&cg, "displayNotice").contains(&"Views/Cart.cshtml".to_string()));
    cg.close();
    // Razor reads as no JavaScript: what the view's script references, straight from extraction.
    let result = codegraph_extract::extract_source("Views/Cart.cshtml", &cart, codegraph_types::Language::Razor).unwrap();
    let js: Vec<&str> = result
        .unresolved_references
        .iter()
        .filter(|r| r.language == Some(codegraph_types::Language::JavaScript))
        .map(|r| r.reference_name.as_str())
        .collect();
    assert!(js.contains(&"displayNotice"), "{js:?}");
    let razor = codegraph_jsre::re!(r"Html|Json|Model|Url\.Action");
    assert!(js.iter().all(|name| !razor.is_match(name)), "{js:?}");
}

#[test]
fn leaves_template_data_and_sourced_blocks_alone_and_no_blocks_own_function_is_another_files() {
    let page = [
        "<script type=\"text/html\" id=\"row\"><div onclick=\"fromTemplate()\"></div></script>",
        "<script type=\"application/json\">{ \"fromTemplate\": 1 }</script>",
        "<script src=\"~/js/fromSrc.js\"></script>",
        "@* <script>fromTemplate();</script> *@",
        "<script>",
        "  function ownName() { return 1; }",
        "  ownName();",
        "</script>",
        "<script>",
        "  ownName();",
        "</script>",
    ]
    .join("\n");
    let (cg, _dir) = index(&[("lib.js", "function fromTemplate() {}\nfunction ownName() {}\nfunction fromSrc() {}\n"), ("Views/Page.cshtml", &page)]);
    for name in ["fromTemplate", "ownName", "fromSrc"] {
        assert!(cg.get_callers(&function(&cg, name), 1).unwrap().is_empty(), "{name}");
    }
    assert!(cg.get_nodes_by_file("Views/Page.cshtml").unwrap().iter().all(|n| n.kind != NodeKind::Function), "a view stays one component node");
    cg.close();
}

#[test]
fn leaves_a_global_two_bundles_define_for_the_page_to_decide() {
    let (cg, _dir) = index(&[
        ("wwwroot/js/admin.common.js", "function setLocation(url) { return url; }\nfunction onlyAdmin() {}\n"),
        ("wwwroot/js/public.common.js", "function setLocation(url) { return url; }\n"),
        ("Views/Page.cshtml", "<script>\n  setLocation('/x');\n  onlyAdmin();\n</script>\n"),
    ]);
    let callees: Vec<String> = view_calls(&cg, "Page").iter().filter_map(|e| cg.get_node(&e.target).unwrap().map(|n| n.name)).collect();
    assert_eq!(callees, ["onlyAdmin"]);
    cg.close();
}

#[test]
fn reads_a_block_as_typescript_when_it_says_so() {
    let (cg, _dir) = index(&[
        ("helper.ts", "export function typedHelper(n: number) { return n; }\n"),
        ("Views/Typed.cshtml", "<script lang=\"ts\">\n  import { typedHelper } from '../helper';\n  const n: number = typedHelper(1);\n</script>\n"),
    ]);
    assert!(dependents(&cg, "typedHelper").contains(&"Views/Typed.cshtml".to_string()));
    cg.close();
}

#[test]
fn keeps_a_views_script_links_through_a_sync_that_rereads_only_the_view() {
    let (mut cg, dir) = index(&[("site.js", "function firstHelper() {}\nfunction secondHelper() {}\n"), ("Views/Page.cshtml", "<script>\n  firstHelper();\n</script>\n")]);
    assert!(dependents(&cg, "firstHelper").contains(&"Views/Page.cshtml".to_string()));
    write(dir.path(), "Views/Page.cshtml", "<script>\n  secondHelper();\n</script>\n");
    cg.sync(&IndexOptions::default(), None).unwrap();
    let callers: Vec<String> = cg.get_callers(&function(&cg, "secondHelper"), 1).unwrap().into_iter().map(|c| c.node.file_path).collect();
    assert_eq!(callers, ["Views/Page.cshtml"]);
    cg.close();
}

#[test]
fn a_view_in_a_project_with_no_javascript_file_still_has_its_script_read() {
    let result = codegraph_extract::extract_source("Views/Page.cshtml", "@{ var x = Compute(); }\n<script>\n  loadPage();\n</script>\n", codegraph_types::Language::Razor).unwrap();
    let refs: Vec<String> = result
        .unresolved_references
        .iter()
        .map(|r| format!("{}:{}", r.language.map_or("?", |l| l.as_str()), r.reference_name))
        .collect();
    assert!(refs.contains(&"javascript:loadPage".to_string()), "{refs:?}");
    assert!(refs.contains(&"razor:Compute".to_string()), "{refs:?}");
}
