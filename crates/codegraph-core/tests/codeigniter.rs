//! CodeIgniter 3: routes by convention and from routes.php, and the calls that go through the
//! properties `$this->load` fills in (upstream 002dd4d5; port of codeigniter.test.ts).
use codegraph_core::{CodeGraph, IndexOptions};
use codegraph_graph::named_symbol_flow::{resolve_named_symbol_flow, NamedSymbolFlowOptions};
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;
use std::path::Path;

fn write(dir: &Path, rel: &str, content: &str) {
    let full = dir.join(rel);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, content).unwrap();
}

fn method(cg: &CodeGraph, name: &str, file: &str) -> String {
    cg.get_nodes_by_kind(NodeKind::Method)
        .unwrap()
        .into_iter()
        .find(|n| n.name == name && n.file_path == file)
        .unwrap_or_else(|| panic!("no {name} in {file}"))
        .id
}

fn calls(cg: &CodeGraph, name: &str, file: &str) -> Vec<String> {
    let mut out: Vec<String> = cg.get_callees(&method(cg, name, file), 1).unwrap().into_iter().map(|c| format!("{} @{}", c.node.qualified_name, c.node.file_path)).collect();
    out.sort();
    out
}

fn route_target(cg: &CodeGraph, name: &str) -> Option<String> {
    let route = cg.get_nodes_by_kind(NodeKind::Route).unwrap().into_iter().find(|n| n.name == name).unwrap_or_else(|| panic!("no route {name}"));
    cg.get_outgoing_edges(&route.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::References)
        .find_map(|e| cg.get_node(&e.target).unwrap().map(|n| n.qualified_name))
}

fn project(dir: &Path) {
    write(dir, "application/config/routes.php", r#"<?php
$route['default_controller'] = 'welcome';
$route['profile/(:num)'] = 'users/show/$1';
$route['reports/(summary_:any)'] = 'admin/reports/$1';
$route['sale'] = 'shop/items/sale';
$route['board'] = 'dashboard';
$route['gone'] = 'nowhere/index';
"#);
    write(dir, "application/controllers/Welcome.php", "<?php\nclass Welcome extends CI_Controller {\n  public function index() {}\n}\n");
    write(dir, "application/controllers/Users.php", r#"<?php
class Users extends MY_Controller {
    public function __construct() {
        parent::__construct();
        $this->load->model('user_model');
        $this->load->model('Order_model');
        $this->load->model('shop/mdl_items', 'items');
        $this->load->library('mailer');
    }
    public function show($id) {
        $user = $this->user_model->find($id);
        $this->user_model->save($user);
        $this->Order_model->find($id);
        $this->items->price($id);
        $this->mailer->welcome($user);
        $this->load->view('users/show', ['user' => $user]);
    }
    public function missing() {
        $this->user_model->no_such_method();
        $this->nothing_loaded->find();
        $this->db->get('users');
    }
}
"#);
    write(dir, "application/controllers/admin/Reports.php", "<?php\nclass Reports extends MY_Controller {\n  public function index() {}\n}\n");
    write(dir, "application/core/MY_Model.php", "<?php\nclass MY_Model extends CI_Model {\n  public function save($row) { return $row; }\n}\n");
    write(dir, "application/models/User_model.php", "<?php\nclass User_model extends MY_Model {\n  public function find($id) { return $id; }\n}\n");
    write(dir, "application/models/Order_model.php", "<?php\nclass Order_model extends MY_Model {\n  public function find($id) { return $id; }\n}\n");
    write(dir, "application/libraries/Mailer.php", r#"<?php
class Mailer {
    protected $CI;
    public function __construct() { $this->CI =& get_instance(); }
    public function welcome($user) {
        $this->CI->user_model->find($user);
        $CI =& get_instance();
        $CI->user_model->save($user);
    }
}
"#);
    write(dir, "application/views/users/show.php", "<?php echo $this->mailer->welcome('x'); ?>\n");
    write(dir, "application/modules/shop/controllers/Items.php", "<?php\nclass Items extends MX_Controller {\n    public function sale() { return $this->mdl_items->price(1); }\n}\n");
    write(dir, "application/modules/shop/models/Mdl_items.php", "<?php\nclass Mdl_Items extends MY_Model {\n  public function price($id) { return $id; }\n}\n");
    write(dir, "application/modules/dashboard/controllers/Dashboard.php", "<?php\nclass Dashboard extends MX_Controller {\n  public function index() {\n    $this->load->view('overview');\n  }\n}\n");
    write(dir, "application/modules/dashboard/views/overview.php", "<p>overview</p>\n");
}

#[test]
fn a_codeigniter_project_indexed() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();

    // A convention route binds its method, and a routes.php one what it names.
    assert_eq!(route_target(&cg, "ANY /users/show").as_deref(), Some("Users::show"));
    assert_eq!(route_target(&cg, "ANY /shop/items/sale").as_deref(), Some("Items::sale"));
    assert_eq!(route_target(&cg, "ANY /dashboard").as_deref(), Some("Dashboard::index"));
    assert_eq!(route_target(&cg, "ANY /").as_deref(), Some("Welcome::index"));
    assert_eq!(route_target(&cg, "ANY /profile/(:num)").as_deref(), Some("Users::show"));
    assert_eq!(route_target(&cg, "ANY /sale").as_deref(), Some("Items::sale"));
    assert_eq!(route_target(&cg, "ANY /board").as_deref(), Some("Dashboard::index"));
    // A variable in the method's place binds the controller; no controller binds nothing.
    assert_eq!(route_target(&cg, "ANY /reports/(summary_:any)").as_deref(), Some("Reports"));
    assert_eq!(route_target(&cg, "ANY /gone"), None);

    let show = calls(&cg, "show", "application/controllers/Users.php");
    for want in [
        "User_model::find @application/models/User_model.php",
        "Order_model::find @application/models/Order_model.php",
        "MY_Model::save @application/core/MY_Model.php",
        "Mailer::welcome @application/libraries/Mailer.php",
        "Mdl_Items::price @application/modules/shop/models/Mdl_items.php",
    ] {
        assert!(show.contains(&want.to_string()), "show is missing {want}: {show:?}");
    }
    assert_eq!(calls(&cg, "sale", "application/modules/shop/controllers/Items.php"), ["Mdl_Items::price @application/modules/shop/models/Mdl_items.php"]);
    assert_eq!(calls(&cg, "welcome", "application/libraries/Mailer.php"), ["MY_Model::save @application/core/MY_Model.php", "User_model::find @application/models/User_model.php"]);
    assert_eq!(calls(&cg, "missing", "application/controllers/Users.php"), Vec::<String>::new());

    // A loaded view links to its file, in the app or the caller's module, and on to what it calls.
    let callees = cg.get_callees(&method(&cg, "show", "application/controllers/Users.php"), 1).unwrap();
    let view = callees.iter().find(|c| c.node.kind == NodeKind::File).expect("show links its view");
    assert_eq!(view.node.file_path, "application/views/users/show.php");
    assert!(cg.get_callees(&view.node.id, 1).unwrap().iter().any(|c| c.node.name == "welcome"));
    let index: Vec<String> = cg
        .get_callees(&method(&cg, "index", "application/modules/dashboard/controllers/Dashboard.php"), 1)
        .unwrap()
        .into_iter()
        .map(|c| c.node.file_path)
        .collect();
    assert_eq!(index, ["application/modules/dashboard/views/overview.php"]);

    // A request connects end to end for a question in the app's own names.
    let store = cg.store().as_ref();
    let flow = resolve_named_symbol_flow(store, store, "Users.show Mailer.welcome User_model.find", &NamedSymbolFlowOptions::default(), None).unwrap();
    let steps: Vec<String> = flow.chains.first().map(|c| c.steps.iter().map(|s| s.node.qualified_name.clone()).collect()).unwrap_or_default();
    assert_eq!(steps, ["Users::show", "Mailer::welcome", "User_model::find"]);
    cg.close();
}

#[test]
fn a_sync_binds_a_route_when_its_controller_arrives_and_a_call_when_its_model_does() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "application/config/routes.php", "<?php\n$route['default_controller'] = 'welcome';\n$route['read/(:num)'] = 'blog/view/$1';\n");
    write(dir.path(), "application/controllers/Welcome.php", "<?php\nclass Welcome extends CI_Controller {\n    public function index() { $this->post_model->latest(); }\n}\n");
    let mut cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let index = |cg: &CodeGraph| -> Vec<String> {
        cg.get_callees(&method(cg, "index", "application/controllers/Welcome.php"), 1).unwrap().into_iter().map(|c| c.node.qualified_name).collect()
    };
    assert_eq!(route_target(&cg, "ANY /read/(:num)"), None);
    assert!(index(&cg).is_empty());

    write(dir.path(), "application/controllers/Blog.php", "<?php\nclass Blog extends CI_Controller {\n  public function view($id) {}\n}\n");
    write(dir.path(), "application/models/Post_model.php", "<?php\nclass Post_model extends CI_Model {\n  public function latest() {}\n}\n");
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(route_target(&cg, "ANY /read/(:num)").as_deref(), Some("Blog::view"));
    assert_eq!(index(&cg), ["Post_model::latest"]);
    cg.close();
}

#[test]
fn another_frameworks_routes_file_is_not_codeigniter() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "app/Config/Routes.php", "<?php\n$routes->get('/', 'Home::index');\n");
    write(dir.path(), "application/config/routes.php", "<?php\nreturn ['home' => 'index'];\n");
    write(dir.path(), "application/controllers/Home.php", "<?php\nclass Home {\n  public function index() {}\n}\n");
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    assert!(cg.get_nodes_by_kind(NodeKind::Route).unwrap().is_empty());
    cg.close();
}
