//! CodeIgniter 3 framework resolver. Port of `src/resolution/frameworks/codeigniter.ts`
//! (upstream 002dd4d5).
//!
//! A request reaches a controller by convention more often than through
//! `application/config/routes.php`: every public method of a class in `application/controllers/`
//! answers at `/<folder>/<class>/<method>`, so both kinds become `route` nodes bound to the method
//! that serves them. HMVC keeps the same layout per module (`modules/<module>/controllers/`), with
//! the module as the first segment, and `/<module>/<method>` for the controller named after it.
//!
//! From there the flow goes through properties nothing declares: `$this->load->model('users_model')`
//! puts a `Users_model` on the controller, and `$this->users_model->find($id)` (or
//! `$this->CI->users_model->find($id)` in a library) calls it. A model is read by its class name
//! whatever the case, as PHP and the loader read it. `$this->load->view('users/list')` links to
//! the view's file, where the calls a template makes are recorded.

use super::now_ms;
use crate::ctx::Ctx;
use crate::types::{FrameworkExtractionResult, FrameworkResolver, NodeList, NodeRef, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

pub struct CodeigniterResolver;

const LANGUAGES: [Language; 1] = [Language::Php];

/// A handler reference: `ci:method:<file>|<method>` from a controller, and
/// `ci:route:<app root>|<uri>:<word>` from `routes.php`. The word is what a sync looks a failed
/// reference up by: a name the controller it waits for defines.
const METHOD_REF: &str = "ci:method:";
const ROUTE_REF: &str = "ci:route:";

/// Levels of `extends` a method is looked for through.
const MAX_ANCESTORS: usize = 6;

fn routes_file_re() -> &'static Re {
    re!(r"(?:^|/)application/(?:modules/[^/]+/)?config/(?:[^/]+/)?routes\.php$")
}
fn controller_file_re() -> &'static Re {
    re!(r"^(?:.*?/)?(?:application/controllers|modules/([^/]+)/controllers)/(.+)\.php$")
}
/// Where the loader finds what `$this->load` puts on a property.
fn loadable_file_re() -> &'static Re {
    re!(r"(?:^|/)(?:models|libraries)/")
}
/// The module a file of an HMVC project belongs to.
fn module_of_re() -> &'static Re {
    re!(r"(?:^|/)modules/([^/]+)/")
}
/// `$this->x_model->find()`, as extraction names it; through `get_instance()` in a library:
/// `$this->CI->x_model`, `$CI->x_model`.
fn property_call_re() -> &'static Re {
    re!(r"^(?:this->(?:_?ci->)?|_?ci->)([A-Za-z_]\w*)\.([A-Za-z_]\w*)$", "i")
}

#[derive(Default)]
struct ClassTable {
    /// What the table was built from: the context hands out one list until the graph changes.
    source: Option<NodeList>,
    /// Lower-cased class name → the models and libraries of that name.
    loadable: HashMap<String, Vec<NodeRef>>,
    /// The lower-cased URI a controller answers at, with no leading slash → its classes, one per app.
    controllers: HashMap<String, Vec<NodeRef>>,
    /// File → property → the class a load in that file gave that name to.
    aliases: HashMap<String, Rc<HashMap<String, String>>>,
}

#[derive(Default)]
struct ClassTableMemo(RefCell<ClassTable>);

fn with_class_table<T>(ctx: &Ctx, f: impl FnOnce(&mut ClassTable) -> T) -> T {
    let all = ctx.get_nodes_by_kind(NodeKind::Class);
    let memo = ctx.memo::<ClassTableMemo>();
    let mut table = memo.0.borrow_mut();
    if table.source.as_ref().is_none_or(|s| !Rc::ptr_eq(s, &all)) {
        *table = ClassTable::default();
        for node in all.iter() {
            if node.language != Language::Php {
                continue;
            }
            if let Some(url) = controller_url(&node.file_path) {
                table.controllers.entry(url[1..].to_string()).or_default().push(node.clone());
            } else if loadable_file_re().is_match(&node.file_path) {
                table.loadable.entry(node.name.to_lowercase()).or_default().push(node.clone());
            }
        }
        table.source = Some(all);
    }
    f(&mut table)
}

/// The URL a controller's `index` answers at, or None for a file that isn't one.
fn controller_url(file_path: &str) -> Option<String> {
    let m = controller_file_re().captures(file_path)?;
    let segments: Vec<String> = m.text(2).unwrap_or("").to_lowercase().split('/').map(String::from).collect();
    let Some(module) = m.text(1).filter(|s| !s.is_empty()).map(str::to_lowercase) else {
        return Some(format!("/{}", segments.join("/")));
    };
    if segments.len() == 1 && segments[0] == module {
        Some(format!("/{module}"))
    } else {
        Some(format!("/{module}/{}", segments.join("/")))
    }
}

fn route_node(file_path: &str, line: u32, verb: &str, route_path: &str, now: i64) -> Node {
    Node::new(
        format!("route:{file_path}:{line}:{verb}:{route_path}"),
        NodeKind::Route,
        format!("{verb} {route_path}"),
        format!("{file_path}::route:{verb}:{route_path}"),
        file_path,
        Language::Php,
        line,
        line,
        0,
        0,
        now,
    )
}

fn line_at(safe: &str, index: usize) -> u32 {
    safe[..index].matches('\n').count() as u32 + 1
}

/// Each public method of the class a controller file is named after, as a route.
fn convention_routes(file_path: &str, base_url: &str, safe: &str) -> FrameworkExtractionResult {
    let mut out = FrameworkExtractionResult::default();
    let base = &file_path[file_path.rfind('/').map_or(0, |i| i + 1)..];
    let stem = base.strip_suffix(".php").unwrap_or(base).to_lowercase();
    let class_re = re!(r"^[ \t]*((?:abstract\s+|final\s+)*)class\s+([A-Za-z_]\w*)", "gm");
    let Some(cls) = class_re.captures_iter(safe).find(|c| c.text(2).unwrap_or("").to_lowercase() == stem) else {
        return out;
    };
    if cls.text(1).unwrap_or("").contains("abstract") {
        return out;
    }
    let now = now_ms();
    let mut seen: HashSet<String> = HashSet::new();
    let method_re = re!(r"^[ \t]*((?:(?:public|protected|private|static|final|abstract)\s+)*)function\s+&?\s*([A-Za-z_]\w*)\s*\(", "gm");
    for m in method_re.captures_iter(safe) {
        let name = m.text(2).unwrap_or("");
        let modifiers = m.text(1).unwrap_or("");
        // A leading underscore is CodeIgniter's own way to keep a method off the web.
        if modifiers.contains("private") || modifiers.contains("protected") || name.starts_with('_') {
            continue;
        }
        let lower = name.to_lowercase();
        let route_path = if lower == "index" { base_url.to_string() } else { format!("{base_url}/{lower}") };
        if !seen.insert(route_path.clone()) {
            continue;
        }
        let line = line_at(safe, m.get(0).unwrap().start());
        let route = route_node(file_path, line, "ANY", &route_path, now);
        out.references.push(UnresolvedRef::new(route.id.clone(), format!("{METHOD_REF}{file_path}|{name}"), EdgeKind::References, line, 0, file_path, Language::Php));
        out.nodes.push(route);
    }
    out
}

/// The name the handler's method goes by, or its class's when only that is written.
fn awaited_name(handler: &str) -> String {
    let literal: Vec<&str> = handler.split('/').filter(|s| !s.is_empty() && !s.contains('$')).collect();
    let last = literal.last().copied().unwrap_or(handler);
    if literal.len() > 1 {
        return last.to_string();
    }
    let mut chars = last.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// `$route['pattern'] = 'controller/method'`, verb-scoped or not, and the default controller at `/`.
fn explicit_routes(file_path: &str, safe: &str) -> FrameworkExtractionResult {
    let mut out = FrameworkExtractionResult::default();
    let now = now_ms();
    let root = &file_path[..file_path.find("application/").unwrap_or(0)];
    let route_re = re!(r#"\$route\s*\[\s*['"]([^'"]+)['"]\s*\](?:\s*\[\s*['"]([A-Za-z]+)['"]\s*\])?\s*=\s*['"]([^'"]*)['"]"#, "g");
    for m in route_re.captures_iter(safe) {
        let pattern = m.text(1).unwrap_or("");
        let handler = m.text(3).unwrap_or("");
        if pattern == "404_override" || pattern == "translate_uri_dashes" || handler.is_empty() {
            continue;
        }
        let line = line_at(safe, m.get(0).unwrap().start());
        let route_path = if pattern == "default_controller" { "/".to_string() } else { format!("/{}", pattern.trim_start_matches('/')) };
        let verb = m.text(2).unwrap_or("ANY").to_uppercase();
        let route = route_node(file_path, line, &verb, &route_path, now);
        out.references.push(UnresolvedRef::new(
            route.id.clone(),
            format!("{ROUTE_REF}{root}|{handler}:{}", awaited_name(handler)),
            EdgeKind::References,
            line,
            0,
            file_path,
            Language::Php,
        ));
        out.nodes.push(route);
    }
    out
}

/// A method by name on a class, its own or one it inherits. PHP doesn't mind the case.
fn method_of(cls: &NodeRef, name: &str, ctx: &Ctx) -> Option<NodeRef> {
    let lower = name.to_lowercase();
    let mut seen: HashSet<String> = HashSet::new();
    let mut level: Vec<NodeRef> = vec![cls.clone()];
    let mut depth = 0;
    while depth < MAX_ANCESTORS && !level.is_empty() {
        let mut next: Vec<NodeRef> = Vec::new();
        for ty in &level {
            if !seen.insert(ty.id.clone()) {
                continue;
            }
            let prefix = format!("{}::", ty.qualified_name).to_lowercase();
            let own = ctx
                .get_nodes_in_file(&ty.file_path)
                .iter()
                .find(|n| n.kind == NodeKind::Method && n.name.to_lowercase() == lower && n.qualified_name.to_lowercase().starts_with(&prefix))
                .cloned();
            if own.is_some() {
                return own;
            }
            next.extend(ctx.get_supertype_nodes(&ty.id).iter().filter(|p| p.kind == NodeKind::Class).cloned());
        }
        level = next;
        depth += 1;
    }
    None
}

/// What `controller/method/$1` serves: the first run of segments that names a controller, then
/// the method after it (`index` when none does), and the class when a variable does.
fn route_handler(root: &str, uri: &str, ctx: &Ctx) -> Option<NodeRef> {
    let segments: Vec<&str> = uri.split('/').filter(|s| !s.is_empty()).collect();
    let lower: Vec<String> = segments.iter().map(|s| s.to_lowercase()).collect();
    let cls = with_class_table(ctx, |table| {
        (1..=lower.len()).rev().find_map(|i| {
            let classes = table.controllers.get(&lower[..i].join("/"))?;
            let cls = classes.iter().find(|c| c.file_path.starts_with(root)).unwrap_or(&classes[0]).clone();
            Some((cls, i))
        })
    });
    let (cls, at) = cls?;
    let segment = segments.get(at).copied().unwrap_or("index");
    let method = if segment.contains('$') { None } else { method_of(&cls, segment, ctx) };
    Some(method.unwrap_or(cls))
}

/// The file the `$this->load->view('users/list')` on this line loads: the caller's module's,
/// another module's, or the app's.
fn view_file(r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    let lines = ctx.get_file_lines(&r.file_path)?;
    let line = lines.get((r.line as usize).checked_sub(1)?)?;
    let view = re!(r#"->\s*load\s*->\s*view\s*\(\s*['"]([\w./-]+)['"]"#).captures(line)?.text(1)?.to_string();
    let file = if re!(r"\.\w+$").is_match(&view) { view } else { format!("{view}.php") };
    let fp = r.file_path.as_str();
    let root = &fp[..fp.find("application/").unwrap_or(0)];
    let module = module_of_re().captures(fp);
    let modules = match &module {
        Some(m) => {
            let whole = m.get(0).unwrap();
            fp[..whole.end() - m.text(1).unwrap_or("").len() - 1].to_string()
        }
        None => format!("{root}application/modules/"),
    };
    let mut candidates: Vec<String> = Vec::new();
    if let Some(m) = &module {
        candidates.push(format!("{modules}{}/views/{file}", m.text(1).unwrap_or("")));
    }
    if let Some(slash) = file.find('/').filter(|&s| s > 0) {
        candidates.push(format!("{modules}{}/views/{}", &file[..slash], &file[slash + 1..]));
    }
    candidates.push(format!("{root}application/views/{file}"));
    candidates.iter().find_map(|candidate| ctx.get_nodes_in_file(candidate).iter().find(|n| n.kind == NodeKind::File).cloned())
}

fn aliases_in(file: &str, table: &mut ClassTable, ctx: &Ctx) -> Rc<HashMap<String, String>> {
    if let Some(known) = table.aliases.get(file) {
        return known.clone();
    }
    let mut aliases: HashMap<String, String> = HashMap::new();
    let source = ctx.read_file(file).unwrap_or_else(|| Rc::from(""));
    let model_re = re!(r#"->\s*load\s*->\s*model\s*\(\s*['"]([\w/]+)['"]\s*,\s*['"](\w+)['"]"#, "g");
    let library_re = re!(r#"->\s*load\s*->\s*library\s*\(\s*['"]([\w/]+)['"]\s*,[^,()]*,\s*['"](\w+)['"]"#, "g");
    for re in [model_re, library_re] {
        for m in re.captures_iter(&source) {
            let class = m.text(1).unwrap_or("").rsplit('/').next().unwrap_or("").to_lowercase();
            aliases.insert(m.text(2).unwrap_or("").to_string(), class);
        }
    }
    let aliases = Rc::new(aliases);
    table.aliases.insert(file.to_string(), aliases.clone());
    aliases
}

/// The class a property holds in `file`: by its own name, or by the alias a load there gave it.
fn property_class(property: &str, file: &str, ctx: &Ctx) -> Option<NodeRef> {
    with_class_table(ctx, |table| {
        let lower = property.to_lowercase();
        let key = if table.loadable.contains_key(&lower) { lower } else { aliases_in(file, table, ctx).get(property).cloned().unwrap_or(lower) };
        let classes = table.loadable.get(&key)?;
        if classes.len() == 1 {
            return Some(classes[0].clone());
        }
        // One name in two HMVC modules: the caller's own, and no guess past that.
        let module = module_of_re().captures(file).and_then(|m| m.text(1).map(String::from))?;
        let own: Vec<&NodeRef> = classes
            .iter()
            .filter(|c| module_of_re().captures(&c.file_path).and_then(|m| m.text(1).map(String::from)).as_deref() == Some(module.as_str()))
            .collect();
        if own.len() == 1 {
            Some(own[0].clone())
        } else {
            None
        }
    })
}

impl FrameworkResolver for CodeigniterResolver {
    fn name(&self) -> &str {
        "codeigniter"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    // The handler references are this file's own, and a property call names no declared symbol,
    // so neither would get past the name pre-filter.
    fn claims_reference(&self, name: &str) -> bool {
        name.starts_with("ci:") || property_call_re().is_match(name)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        if ctx.file_exists("system/core/CodeIgniter.php") {
            return true;
        }
        // `system/` is often a Composer package, and `application/` can sit below the repository
        // root. A CodeIgniter routes file names its default controller.
        let Some(routes) = ctx.get_all_files().into_iter().find(|f| f.ends_with("application/config/routes.php")) else { return false };
        ctx.read_file(&routes).is_some_and(|text| re!(r#"\$route\s*\[\s*['"]default_controller['"]\s*\]"#).is_match(&text))
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        let name = r.reference_name.as_str();
        let resolved = |target: Option<NodeRef>, confidence: f64| target.map(|t| ResolvedRef::new(r, t.id.clone(), confidence, ResolvedBy::Framework));

        if let Some(rest) = name.strip_prefix(METHOD_REF) {
            let (file, method) = rest.split_once('|').unwrap_or((rest, ""));
            return resolved(ctx.get_nodes_in_file(file).iter().find(|n| n.kind == NodeKind::Method && n.name == method).cloned(), 0.95);
        }
        if let Some(rest) = name.strip_prefix(ROUTE_REF) {
            let (root, handler) = rest.split_once('|').unwrap_or((rest, ""));
            let uri = handler.rfind(':').map_or(handler, |i| &handler[..i]);
            return resolved(route_handler(root, uri, ctx), 0.9);
        }
        if let Some(call) = property_call_re().captures(name) {
            let (property, method) = (call.text(1).unwrap_or(""), call.text(2).unwrap_or(""));
            if property == "load" && method == "view" {
                return resolved(view_file(r, ctx), 0.9);
            }
            let cls = property_class(property, &r.file_path, ctx);
            return resolved(cls.and_then(|c| method_of(&c, method, ctx)), 0.9);
        }
        None
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<FrameworkExtractionResult> {
        let routes = routes_file_re().is_match(file_path);
        let base_url = if routes { None } else { controller_url(file_path) };
        if !routes && base_url.is_none() {
            return Some(FrameworkExtractionResult::default());
        }
        let safe = codegraph_text::strip_comments_for_regex(content, "php");
        Some(match base_url {
            None => explicit_routes(file_path, &safe),
            Some(base) => convention_routes(file_path, &base, &safe),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route_names(file: &str, source: &str) -> Vec<String> {
        CodeigniterResolver.extract(file, source).unwrap().nodes.into_iter().map(|n| n.name).collect()
    }

    const CONTROLLER: &str = r#"<?php
class Blog extends MY_Controller {
    public function __construct() { parent::__construct(); }
    public function index() {}
    public function view($id) {}
    function comments() {}
    public static function feed() {}
    protected function guard() {}
    private function load_post() {}
    public function _remap() {}
    public function _hidden() {}
}
"#;

    #[test]
    fn takes_every_public_method_and_keeps_underscored_ones_off_the_web() {
        assert_eq!(route_names("application/controllers/Blog.php", CONTROLLER), ["ANY /blog", "ANY /blog/view", "ANY /blog/comments", "ANY /blog/feed"]);
    }

    #[test]
    fn builds_the_url_from_the_folder_and_lower_cases_it() {
        assert!(route_names("application/controllers/admin/Blog.php", CONTROLLER).contains(&"ANY /admin/blog/view".into()));
        assert!(route_names("sub/dir/application/controllers/Blog.php", CONTROLLER).contains(&"ANY /blog/view".into()));
    }

    #[test]
    fn reads_hmvc_modules() {
        assert!(route_names("application/modules/blog/controllers/Blog.php", CONTROLLER).contains(&"ANY /blog/view".into()));
        assert!(route_names("application/modules/shop/controllers/Blog.php", CONTROLLER).contains(&"ANY /shop/blog/view".into()));
        assert!(route_names("bonfire/modules/shop/controllers/Blog.php", CONTROLLER).contains(&"ANY /shop/blog/view".into()));
    }

    #[test]
    fn skips_a_misnamed_class_an_abstract_one_comments_and_other_files() {
        assert!(route_names("application/controllers/Other.php", CONTROLLER).is_empty());
        assert!(route_names("application/controllers/Base.php", "<?php\nabstract class Base extends CI_Controller {\n  public function index() {}\n}\n").is_empty());
        assert_eq!(
            route_names("application/controllers/Blog.php", "<?php\nclass Blog {\n  // public function gone() {}\n  /* public function older() {} */\n  public function kept() {}\n}\n"),
            ["ANY /blog/kept"]
        );
        assert!(route_names("application/models/Blog.php", CONTROLLER).is_empty());
    }

    #[test]
    fn reads_routes_php() {
        let file = "application/config/routes.php";
        assert_eq!(
            route_names(
                file,
                "<?php\n$route['default_controller'] = 'welcome';\n$route['404_override'] = 'errors/missing';\n$route['translate_uri_dashes'] = FALSE;\n$route['product/(:num)'] = 'catalog/lookup/$1';\n$route['api/users']['post'] = 'api/users/create';\n"
            ),
            ["ANY /", "ANY /product/(:num)", "POST /api/users"]
        );
        assert!(route_names(file, "<?php\n// $route['old'] = 'old/index';\n$route['drinks/(:any)'] = function ($type) { return 'catalog/drinks/' . strtolower($type); };\n").is_empty());
        assert_eq!(route_names("application/modules/shop/config/routes.php", "<?php\n$route['shop/sale'] = 'shop/items/sale';\n"), ["ANY /shop/sale"]);
    }

    #[test]
    fn awaited_name_is_the_method_or_the_capitalised_class() {
        assert_eq!(awaited_name("users/show/$1"), "show");
        assert_eq!(awaited_name("dashboard"), "Dashboard");
    }
}
