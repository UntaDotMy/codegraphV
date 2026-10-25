//! Ruby on Rails resolver. Port of `src/resolution/frameworks/ruby.ts`.

use super::now_ms;
use crate::ctx::Ctx;
use crate::types::{FrameworkExtractionResult, FrameworkResolver, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_types::jscompat::{is_js_space, js_trim, utf16_len};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use std::collections::HashSet;

const LANGUAGES: [Language; 1] = [Language::Ruby];

fn line_of(text: &str, index: usize) -> u32 {
    text[..index].matches('\n').count() as u32 + 1
}

/// A `resources` line's `only:` / `except:` action list, written any way Rails accepts it:
/// `[:index, :show]`, `:show`, `%i[new create index]`, `%w(index show)`, `"show"`, or the older
/// `:only => [...]`. `None` when the option is absent. maybe's `only: %i[new create index]` was
/// not read, so every resource drew all seven routes, four of them to actions that do not exist.
fn rails_action_option(tail: &str, key: &str) -> Option<HashSet<String>> {
    let pattern = format!(r##"(?:\b{key}:|:{key}\s*=>)\s*(?:\[([^\]]*)\]|%[iIwW]\s*[\[(]([^\])]*)[\])]|:(\w+)|["'](\w+)["'])"##);
    let re = Re::cached(&pattern, "");
    let m = re.captures(tail)?;
    if let Some(list) = m.text(1) {
        return Some(
            list.split(',')
                .map(|v| {
                    let v = js_trim(v);
                    let v = v.strip_prefix(':').unwrap_or(v);
                    strip_quotes(v).to_string()
                })
                .filter(|v| !v.is_empty())
                .collect(),
        );
    }
    if let Some(words) = m.text(2) {
        return Some(js_trim(words).split(is_js_space).filter(|v| !v.is_empty()).map(String::from).collect());
    }
    Some(HashSet::from([m.text(3).or(m.text(4)).unwrap_or("").to_string()]))
}

/// `s.replace(/^["']|["']$/g, '')`
fn strip_quotes(s: &str) -> &str {
    let s = s.strip_prefix(['"', '\'']).unwrap_or(s);
    s.strip_suffix(['"', '\'']).unwrap_or(s)
}

pub struct RailsResolver;

impl FrameworkResolver for RailsResolver {
    fn name(&self) -> &str {
        "rails"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    // `controller#action` route refs name no declared symbol, so resolveOne's pre-filter would
    // drop them before resolve() runs. Claim them (like the django `_iterable_class` hook) so
    // they reach Pattern 0.
    fn claims_reference(&self, name: &str) -> bool {
        re!(r"^[\w/]+#\w+$").is_match(name)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        // Check for Gemfile with rails
        if ctx.read_file("Gemfile").is_some_and(|g| !g.is_empty() && re!(r#"\bgem\s+["'](?:rails|railties)["']"#).is_match(&g)) {
            return true;
        }

        // Check for config/application.rb (Rails signature)
        if ctx.file_exists("config/application.rb") {
            return true;
        }

        // Check for typical Rails directory structure — or a Rails engine's routes file anywhere
        // (solidus keeps each engine's under `backend/config/`).
        ctx.file_exists("app/controllers/application_controller.rb")
            || ctx.file_exists("config/routes.rb")
            || ctx.get_all_files().iter().any(|f| re!(r"(?:^|/)config/routes\.rb$").is_match(f))
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        let name = r.reference_name.as_str();
        // Pattern 0: route action `controller#action` (from RESTful `resources` or an explicit
        // route) → the action method in that controller. Precise — avoids the bare-`action`
        // ambiguity (every controller has an `index`/`show`).
        if let Some(ca) = re!(r"^([\w/]+)#(\w+)$").captures(name) {
            let result = resolve_controller_action(ca.text(1).unwrap_or(""), ca.text(2).unwrap_or(""), ctx);
            return result.map(|id| ResolvedRef::new(r, id, 0.85, ResolvedBy::Framework));
        }

        // Pattern 1: Model references (ActiveRecord)
        if re!(r"^[A-Z][a-zA-Z]+$").is_match(name) {
            if let Some(result) = resolve_model(name, ctx) {
                return Some(ResolvedRef::new(r, result, 0.8, ResolvedBy::Framework));
            }
        }

        // Pattern 2: Controller references
        if name.ends_with("Controller") {
            if let Some(result) = resolve_controller(name, ctx) {
                return Some(ResolvedRef::new(r, result, 0.85, ResolvedBy::Framework));
            }
        }

        // Pattern 3: Helper references
        if name.ends_with("Helper") {
            if let Some(result) = resolve_helper(name, ctx) {
                return Some(ResolvedRef::new(r, result, 0.8, ResolvedBy::Framework));
            }
        }

        // Pattern 4: Service/Job references
        if name.ends_with("Service") || name.ends_with("Job") {
            if let Some(result) = resolve_service(name, ctx) {
                return Some(ResolvedRef::new(r, result, 0.8, ResolvedBy::Framework));
            }
        }

        None
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<FrameworkExtractionResult> {
        if !file_path.ends_with(".rb") {
            return Some(FrameworkExtractionResult::default());
        }
        let mut nodes: Vec<Node> = Vec::new();
        let mut references: Vec<UnresolvedRef> = Vec::new();
        let now = now_ms();
        let safe = codegraph_text::strip_comments_for_regex(content, "ruby");
        let safe: &str = &safe;

        // A routes file is read with its blocks: `namespace`, `scope`, nested `resources … do`,
        // `member` / `collection`.
        if re!(r"\broutes\.draw\b").is_match(safe) || re!(r"(?:^|/)config/routes/").is_match(file_path) {
            return Some(extract_scoped_rails_routes(file_path, safe));
        }

        // get/post/put/patch/delete/match '/path', to: 'controller#action'
        // Also: get '/path' => 'controller#action'
        let route_regex = re!(r#"\b(get|post|put|patch|delete|match)\s+['"]([^'"]+)['"]\s*(?:,\s*to:\s*|=>\s*)['"]([^#'"]+)#([^'"]+)['"]"#, "g");
        for m in route_regex.captures_iter(safe) {
            let Some(whole) = m.get(0) else { continue };
            let route_path = m.text(2).unwrap_or("");
            let (ctrl, action) = (m.text(3).unwrap_or(""), m.text(4).unwrap_or(""));
            let line = line_of(safe, whole.start());
            let upper = m.text(1).unwrap_or("").to_uppercase();
            let route_node = Node::new(
                format!("route:{file_path}:{line}:{upper}:{route_path}"),
                NodeKind::Route,
                format!("{upper} {route_path}"),
                format!("{file_path}::route:{route_path}"),
                file_path,
                Language::Ruby,
                line,
                line,
                0,
                utf16_len(whole.as_str()) as u32,
                now,
            );
            let id = route_node.id.clone();
            nodes.push(route_node);

            // precise controller#action, not bare action
            references.push(UnresolvedRef::new(id, format!("{ctrl}#{action}"), EdgeKind::References, line, 0, file_path, Language::Ruby));
        }

        // RESTful resources: `resources :articles` / `resource :user` (the dominant Rails
        // routing) generate a controller action per REST verb. The old resolver only saw
        // explicit `get '/x' => 'c#a'` routes, so resource-routed apps had ZERO route nodes.
        // Expand each into its actions → `controller#action` refs.
        let res_regex = re!(r"\b(resources?)\s+:(\w+)([^\n]*)", "g");
        for m in res_regex.captures_iter(safe) {
            let Some(whole) = m.get(0) else { continue };
            let plural = m.text(1) == Some("resources");
            let res_name = m.text(2).unwrap_or("");
            let tail = m.text(3).unwrap_or("");
            let mut actions: Vec<&str> = if plural { PLURAL_ACTIONS.to_vec() } else { SINGULAR_ACTIONS.to_vec() };
            let only = rails_action_option(tail, "only");
            let except = rails_action_option(tail, "except");
            if let Some(only) = only {
                actions.retain(|a| only.contains(*a));
            } else if let Some(except) = except {
                actions.retain(|a| !except.contains(*a));
            }
            // `resources :articles` → ArticlesController; `resource :user` → UsersController.
            let ctrl = if plural { res_name.to_string() } else { pluralize(res_name) };
            let line = line_of(safe, whole.start());
            for action in actions {
                let (method, path) = restful_route(action, res_name);
                let route_node = Node::new(
                    format!("route:{file_path}:{line}:{method}:{ctrl}#{action}"),
                    NodeKind::Route,
                    format!("{method} {path}"),
                    format!("{file_path}::route:{ctrl}#{action}"),
                    file_path,
                    Language::Ruby,
                    line,
                    line,
                    0,
                    utf16_len(whole.as_str()) as u32,
                    now,
                );
                let id = route_node.id.clone();
                nodes.push(route_node);
                references.push(UnresolvedRef::new(id, format!("{ctrl}#{action}"), EdgeKind::References, line, 0, file_path, Language::Ruby));
            }
        }

        Some(FrameworkExtractionResult { nodes, references })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FrameKind {
    Namespace,
    Scope,
    Resource,
    Member,
    Collection,
    Block,
}

#[derive(Clone)]
struct RailsFrame {
    kind: FrameKind,
    /// Path prefix routes written inside this frame get.
    path: String,
    /// Controller module prefix (`admin/`).
    module: String,
    /// For a resource: its controller, collection path and member path.
    controller: Option<String>,
    collection_path: Option<String>,
    member_path: Option<String>,
}

impl RailsFrame {
    fn new(kind: FrameKind, path: String, module: String) -> Self {
        RailsFrame { kind, path, module, controller: None, collection_path: None, member_path: None }
    }
}

/// `join(...parts)`: `('/' + parts.join('/')).replace(/\/+/g, '/').replace(/(.)\/$/, '$1')`
fn join(parts: &[&str]) -> String {
    let joined = format!("/{}", parts.join("/"));
    let mut collapsed = String::with_capacity(joined.len());
    for ch in joined.chars() {
        if ch == '/' && collapsed.ends_with('/') {
            continue;
        }
        collapsed.push(ch);
    }
    if collapsed.ends_with('/') {
        let before = &collapsed[..collapsed.len() - 1];
        if before.chars().last().is_some_and(|c| !matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')) {
            return before.to_string();
        }
    }
    collapsed
}

/// `s.replace(/^:/, '').replace(/^["']|["']$/g, '')`
fn sym(s: &str) -> String {
    strip_quotes(s.strip_prefix(':').unwrap_or(s)).to_string()
}

fn option(tail: &str, key: &str) -> Option<String> {
    let re = Re::cached(&format!(r##"\b{key}:\s*(:\w+|["'][^"']*["'])"##), "");
    re.captures(tail).and_then(|m| m.text(1).map(String::from))
}

/// A Rails routes file with its nesting: `namespace :admin` prefixes paths and controllers
/// (`/admin/zones` → `admin/zones#index`), `scope` its path and module, a `resources :products
/// do` block nests its children under `/products/:product_id`, and `member` / `collection` blocks
/// add `/products/:id/preview` / `/products/search` actions. Each `do`, and each `if` /
/// `unless` / `case` / `begin` line, opens a frame an `end` closes.
fn extract_scoped_rails_routes(file_path: &str, safe: &str) -> FrameworkExtractionResult {
    let mut nodes: Vec<Node> = Vec::new();
    let mut references: Vec<UnresolvedRef> = Vec::new();
    let now = now_ms();
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack: Vec<RailsFrame> = vec![RailsFrame::new(FrameKind::Block, String::new(), String::new())];

    let mut emit = |method: &str, path: &str, target: &str, line: u32, nodes: &mut Vec<Node>, references: &mut Vec<UnresolvedRef>| {
        let id = format!("route:{file_path}:{line}:{method}:{path}:{target}");
        if !seen.insert(id.clone()) {
            return;
        }
        nodes.push(Node::new(
            id.clone(),
            NodeKind::Route,
            format!("{method} {path}"),
            format!("{file_path}::route:{target}"),
            file_path,
            Language::Ruby,
            line,
            line,
            0,
            0,
            now,
        ));
        references.push(UnresolvedRef::new(id, target, EdgeKind::References, line, 0, file_path, Language::Ruby));
    };

    let opens_re = re!(r"\bdo\s*(?:\|[^|]*\|)?\s*$");
    let lines: Vec<&str> = safe.split('\n').collect();
    let mut i = 0usize;
    while i < lines.len() {
        let line = i as u32 + 1;
        let mut text = js_trim(lines[i]).to_string();
        if text.is_empty() {
            i += 1;
            continue;
        }
        // A statement whose `[` / `(` continues on the next lines (`only: [\n :a,\n :b ]`).
        let open = |t: &str| t.chars().filter(|c| matches!(c, '[' | '(')).count() as i64 - t.chars().filter(|c| matches!(c, ']' | ')')).count() as i64;
        while open(&text) > 0 && i + 1 < lines.len() && !opens_re.is_match(&text) {
            i += 1;
            text.push(' ');
            text.push_str(js_trim(lines[i]));
        }
        i += 1;
        let text: &str = &text;
        if re!(r"^end\b").is_match(text) {
            if stack.len() > 1 {
                stack.pop();
            }
            continue;
        }
        let opens = opens_re.is_match(text);
        let frame = stack.last().cloned().unwrap();
        let mut pushed: Option<RailsFrame> = None;
        if let Some(m) = re!(r#"^namespace\s+(:\w+|["'][\w/-]+["'])(.*)$"#).captures(text) {
            let name = sym(m.text(1).unwrap_or(""));
            let custom = option(m.text(2).unwrap_or(""), "path");
            let path = match &custom {
                Some(c) => sym(c),
                None => name.clone(),
            };
            pushed = Some(RailsFrame::new(FrameKind::Namespace, join(&[&frame.path, &path]), format!("{}{}/", frame.module, name)));
        } else if let Some(m) = re!(r"^scope\b\s*\(?\s*(.*?)\)?\s*(?:do\b.*)?$").captures(text).filter(|_| opens) {
            let args = m.text(1).unwrap_or("");
            let lead = re!(r#"^(["'])([^"']*)\1"#).captures(args).and_then(|l| l.text(2).map(String::from));
            let path_opt = option(args, "path");
            let module_opt = option(args, "module");
            let path = lead.unwrap_or_else(|| path_opt.map(|p| sym(&p)).unwrap_or_default());
            pushed = Some(RailsFrame::new(
                FrameKind::Scope,
                join(&[&frame.path, &path]),
                match module_opt {
                    Some(m) => format!("{}{}/", frame.module, sym(&m)),
                    None => frame.module.clone(),
                },
            ));
        } else if let Some(m) = re!(r"^(resources?)\s+:(\w+)(.*)$").captures(text) {
            let plural = m.text(1) == Some("resources");
            let name = m.text(2).unwrap_or("");
            let tail = m.text(3).unwrap_or("");
            let segment = option(tail, "path").map(|p| sym(&p)).unwrap_or_else(|| name.to_string());
            let controller = format!(
                "{}{}",
                frame.module,
                match option(tail, "controller") {
                    Some(c) => sym(&c),
                    None if plural => name.to_string(),
                    None => pluralize(name),
                }
            );
            let collection_path = join(&[&frame.path, &segment]);
            let member_path = if plural { join(&[&collection_path, ":id"]) } else { collection_path.clone() };
            let mut actions: Vec<&str> = if plural { PLURAL_ACTIONS.to_vec() } else { SINGULAR_ACTIONS.to_vec() };
            let only = rails_action_option(tail, "only");
            let except = rails_action_option(tail, "except");
            if let Some(only) = only {
                actions.retain(|a| only.contains(*a));
            } else if let Some(except) = except {
                actions.retain(|a| !except.contains(*a));
            }
            for action in actions {
                let (method, _) = restful_route(action, "");
                let path = match action {
                    "index" | "create" => collection_path.clone(),
                    "new" => join(&[&collection_path, "new"]),
                    "edit" => join(&[&member_path, "edit"]),
                    _ => member_path.clone(),
                };
                emit(method, &path, &format!("{controller}#{action}"), line, &mut nodes, &mut references);
            }
            if opens {
                let nested = if plural { join(&[&collection_path, &format!(":{}_id", singularize(name))]) } else { collection_path.clone() };
                pushed = Some(RailsFrame {
                    kind: FrameKind::Resource,
                    path: nested,
                    module: frame.module.clone(),
                    controller: Some(controller),
                    collection_path: Some(collection_path),
                    member_path: Some(member_path),
                });
            }
        } else if opens && re!(r"^member\b").is_match(text) {
            pushed = Some(RailsFrame { kind: FrameKind::Member, ..frame.clone() });
        } else if opens && re!(r"^collection\b").is_match(text) {
            pushed = Some(RailsFrame { kind: FrameKind::Collection, ..frame.clone() });
        } else if let Some(m) = re!(r#"^root\s*\(?\s*(?:to:\s*)?["']([\w/]+)#(\w+)["']"#).captures(text) {
            let path = join(&[&frame.path]);
            let path = if path.is_empty() { "/".to_string() } else { path };
            emit("GET", &path, &format!("{}{}#{}", frame.module, m.text(1).unwrap_or(""), m.text(2).unwrap_or("")), line, &mut nodes, &mut references);
        } else if let Some(m) = re!(r#"^(get|post|put|patch|delete|match)\s*\(?\s*(:\w+|["'][^"']*["'])(.*)$"#).captures(text) {
            let upper = m.text(1).unwrap_or("").to_uppercase();
            let method = if upper == "MATCH" { "ANY".to_string() } else { upper };
            let raw = sym(m.text(2).unwrap_or(""));
            let tail = m.text(3).unwrap_or("");
            let to = re!(r#"(?:\bto:\s*|=>\s*)["']([\w/]+)#(\w+)["']"#).captures(tail);
            let resource = stack.iter().rev().find(|f| matches!(f.kind, FrameKind::Resource | FrameKind::Member | FrameKind::Collection));
            if let Some(to) = to {
                let base = match frame.kind {
                    FrameKind::Member => frame.member_path.as_deref().unwrap_or(""),
                    FrameKind::Collection => frame.collection_path.as_deref().unwrap_or(""),
                    _ => frame.path.as_str(),
                };
                emit(&method, &join(&[base, &raw]), &format!("{}{}#{}", frame.module, to.text(1).unwrap_or(""), to.text(2).unwrap_or("")), line, &mut nodes, &mut references);
            } else if let Some(resource) = resource.filter(|r| r.controller.as_deref().is_some_and(|c| !c.is_empty())) {
                if re!(r"^[\w-]+$").is_match(&raw) {
                    let action = sym(option(tail, "action").as_deref().unwrap_or(&raw)).replace('-', "_");
                    let base = if frame.kind == FrameKind::Collection {
                        resource.collection_path.as_deref().unwrap_or("")
                    } else {
                        resource.member_path.as_deref().unwrap_or("")
                    };
                    emit(&method, &join(&[base, &raw]), &format!("{}#{}", resource.controller.as_deref().unwrap_or(""), action), line, &mut nodes, &mut references);
                }
            }
        }
        if let Some(pushed) = pushed {
            if opens {
                stack.push(pushed);
            }
        } else if opens || re!(r"^(?:if|unless|case|begin|while|until)\b").is_match(text) {
            stack.push(RailsFrame { kind: FrameKind::Block, ..frame });
        }
    }
    FrameworkExtractionResult { nodes, references }
}

/// Naive singularize for a resource's nested `:x_id` segment.
fn singularize(w: &str) -> String {
    if let Some(stem) = w.strip_suffix("ies") {
        return format!("{stem}y");
    }
    if w.ends_with("ss") || w.ends_with("us") {
        return w.to_string();
    }
    if (w.ends_with('x') || w.ends_with("ch") || w.ends_with("sh") || w.ends_with("ses")) && w.ends_with("es") {
        return w[..w.len() - 2].to_string();
    }
    w.strip_suffix('s').unwrap_or(w).to_string()
}

// RESTful action → HTTP verb + path. `resources` gets all seven; a singular `resource` omits
// `index`.
fn restful_route(action: &str, r: &str) -> (&'static str, String) {
    match action {
        "index" => ("GET", format!("/{r}")),
        "create" => ("POST", format!("/{r}")),
        "new" => ("GET", format!("/{r}/new")),
        "show" => ("GET", format!("/{r}/:id")),
        "edit" => ("GET", format!("/{r}/:id/edit")),
        "update" => ("PATCH", format!("/{r}/:id")),
        _ => ("DELETE", format!("/{r}/:id")),
    }
}
const PLURAL_ACTIONS: [&str; 7] = ["index", "create", "new", "show", "edit", "update", "destroy"];
const SINGULAR_ACTIONS: [&str; 6] = ["create", "new", "show", "edit", "update", "destroy"];

/// Naive ActiveSupport-style pluralize — covers the common resource names.
fn pluralize(w: &str) -> String {
    let mut rev = w.chars().rev();
    if rev.next() == Some('y') && rev.next().is_some_and(|c| !matches!(c, 'a' | 'e' | 'i' | 'o' | 'u')) {
        return format!("{}ies", &w[..w.len() - 1]);
    }
    if w.ends_with('s') || w.ends_with('x') || w.ends_with('z') || w.ends_with("ch") || w.ends_with("sh") {
        return format!("{w}es");
    }
    format!("{w}s")
}

/// snake_case → CamelCase (`user_profiles` → `UserProfiles`).
fn camelize(s: &str) -> String {
    s.split('_')
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// `name.replace(/([A-Z])/g, '_$1').toLowerCase().slice(1)`
fn snake_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for ch in name.chars() {
        if ch.is_ascii_uppercase() {
            out.push('_');
        }
        out.push(ch);
    }
    let lower = out.to_lowercase();
    codegraph_types::jscompat::utf16_from(&lower, 1).to_string()
}

fn is_action(n: &Node, action: &str) -> bool {
    matches!(n.kind, NodeKind::Method | NodeKind::Function) && n.name == action
}

/// Resolve a `controller#action` route ref to the action method in that controller.
fn resolve_controller_action(ctrl_path: &str, action: &str, ctx: &Ctx) -> Option<String> {
    // Rails convention: `articles` → app/controllers/articles_controller.rb.
    let direct = format!("app/controllers/{ctrl_path}_controller.rb");
    if ctx.file_exists(&direct) {
        if let Some(m) = ctx.get_nodes_in_file(&direct).iter().find(|n| is_action(n, action)) {
            return Some(m.id.clone());
        }
    }
    // Fall back: controller class by name, then the action method in its file — only the ones
    // whose path ends with the route's module path when there are any (an engine's
    // `spree/admin/zones_controller.rb` for `admin/zones`).
    let cls = format!("{}Controller", camelize(ctrl_path.rsplit('/').next().unwrap_or("")));
    let suffix = format!("/{ctrl_path}_controller.rb");
    let all: Vec<_> = ctx.get_nodes_by_name(&cls).iter().filter(|n| n.kind == NodeKind::Class).cloned().collect();
    // A controller of the route's own module that inherits the action is not another module's
    // same-named controller that defines it.
    let own: Vec<_> = if ctrl_path.contains('/') { all.iter().filter(|n| format!("/{}", n.file_path).ends_with(&suffix)).cloned().collect() } else { Vec::new() };
    let classes = if own.is_empty() { &all } else { &own };
    for ctrl in classes {
        if let Some(m) = ctx.get_nodes_in_file(&ctrl.file_path).iter().find(|n| is_action(n, action)) {
            return Some(m.id.clone());
        }
    }
    None
}

fn resolve_in_conventional_files(name: &str, paths: &[String], kind: NodeKind, ctx: &Ctx) -> Option<String> {
    for path in paths {
        if ctx.file_exists(path) {
            if let Some(node) = ctx.get_nodes_in_file(path).iter().find(|n| n.kind == kind && n.name == name) {
                return Some(node.id.clone());
            }
        }
    }
    None
}

fn resolve_model(name: &str, ctx: &Ctx) -> Option<String> {
    // Try direct file path lookup first (Rails convention: CamelCase -> snake_case.rb)
    let snake = snake_name(name);
    let paths = [format!("app/models/{snake}.rb"), format!("app/models/concerns/{snake}.rb")];
    if let Some(id) = resolve_in_conventional_files(name, &paths, NodeKind::Class, ctx) {
        return Some(id);
    }

    // Fall back to name-based lookup
    ctx.get_nodes_by_name(name).iter().find(|n| n.kind == NodeKind::Class && n.file_path.contains("app/models/")).map(|n| n.id.clone())
}

fn resolve_controller(name: &str, ctx: &Ctx) -> Option<String> {
    // Try direct file path lookup first
    let snake = snake_name(name);
    let paths = [format!("app/controllers/{snake}.rb"), format!("app/controllers/api/{snake}.rb"), format!("app/controllers/api/v1/{snake}.rb")];
    if let Some(id) = resolve_in_conventional_files(name, &paths, NodeKind::Class, ctx) {
        return Some(id);
    }

    // Fall back to name-based lookup
    ctx.get_nodes_by_name(name).iter().find(|n| n.kind == NodeKind::Class && n.file_path.contains("controllers/")).map(|n| n.id.clone())
}

fn resolve_helper(name: &str, ctx: &Ctx) -> Option<String> {
    let paths = [format!("app/helpers/{}.rb", snake_name(name))];
    resolve_in_conventional_files(name, &paths, NodeKind::Module, ctx)
}

fn resolve_service(name: &str, ctx: &Ctx) -> Option<String> {
    let snake = snake_name(name);
    let paths = [format!("app/services/{snake}.rb"), format!("app/jobs/{snake}.rb"), format!("app/workers/{snake}.rb")];
    resolve_in_conventional_files(name, &paths, NodeKind::Class, ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resources_expand_with_only() {
        let src = "Rails.application.routes.draw do\n  resources :articles, only: %i[index show]\nend\n";
        let result = RailsResolver.extract("config/routes.rb", src).unwrap();
        let names: Vec<&str> = result.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["GET /articles", "GET /articles/:id"]);
        let refs: Vec<&str> = result.references.iter().map(|r| r.reference_name.as_str()).collect();
        assert_eq!(refs, vec!["articles#index", "articles#show"]);
    }

    #[test]
    fn namespaces_and_members_nest() {
        let src = "Rails.application.routes.draw do\n  namespace :admin do\n    resources :products do\n      member do\n        get :preview\n      end\n    end\n  end\n  root 'home#index'\nend\n";
        let result = RailsResolver.extract("config/routes.rb", src).unwrap();
        let names: Vec<(&str, &str)> = result.nodes.iter().map(|n| (n.name.as_str(), n.qualified_name.as_str())).collect();
        assert!(names.contains(&("GET /admin/products/:id/preview", "config/routes.rb::route:admin/products#preview")));
        assert!(names.contains(&("GET /", "config/routes.rb::route:home#index")));
    }

    #[test]
    fn helpers_inflect_like_the_ts() {
        assert_eq!(pluralize("category"), "categories");
        assert_eq!(pluralize("day"), "days");
        assert_eq!(pluralize("box"), "boxes");
        assert_eq!(singularize("categories"), "category");
        assert_eq!(singularize("buses"), "bus");
        assert_eq!(camelize("user_profiles"), "UserProfiles");
        assert_eq!(snake_name("UserProfile"), "user_profile");
        assert_eq!(join(&["/a/", "b"]), "/a/b");
        assert_eq!(join(&[""]), "/");
    }
}
