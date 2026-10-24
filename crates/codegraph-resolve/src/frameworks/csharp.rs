//! C# framework resolver: ASP.NET Core, ASP.NET MVC and common C# patterns. Port of
//! `src/resolution/frameworks/csharp.ts` (`aspnetResolver`).

use super::name_heuristic::{pick_by_name_and_kind, NamePickOptions};
use super::now_ms;
use crate::ctx::Ctx;
use crate::types::{FrameworkExtractionResult, FrameworkResolver, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_types::jscompat::{js_trim, utf16_byte_index, utf16_len, utf16_prefix};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};

pub struct AspnetResolver;

const LANGUAGES: [Language; 1] = [Language::CSharp];
// A Razor page's `@model CheckoutModel` names the PageModel in the code-behind beside it
// (`Checkout.cshtml.cs`); these rules look there first.
const RESOLVE_LANGUAGES: [Language; 2] = [Language::CSharp, Language::Razor];

// Directory patterns
const CONTROLLER_DIRS: [&str; 1] = ["/Controllers/"];
const SERVICE_DIRS: [&str; 3] = ["/Services/", "/Service/", "/Application/"];
const REPO_DIRS: [&str; 4] = ["/Repositories/", "/Repository/", "/Data/", "/Infrastructure/"];
const MODEL_DIRS: [&str; 5] = ["/Models/", "/Model/", "/Entities/", "/Entity/", "/Domain/"];
const VIEWMODEL_DIRS: [&str; 4] = ["/ViewModels/", "/ViewModel/", "/DTOs/", "/Dto/"];

const CLASS_KINDS: [NodeKind; 1] = [NodeKind::Class];
const SERVICE_KINDS: [NodeKind; 2] = [NodeKind::Class, NodeKind::Interface];

impl FrameworkResolver for AspnetResolver {
    fn name(&self) -> &str {
        "aspnet"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    fn resolve_languages(&self) -> Option<&[Language]> {
        Some(&RESOLVE_LANGUAGES)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        // Check for .csproj files with ASP.NET references
        let all_files = ctx.get_all_files();
        for file in &all_files {
            if file.ends_with(".csproj") {
                if let Some(content) = ctx.read_file(file).filter(|c| !c.is_empty()) {
                    if content.contains("Microsoft.AspNetCore") || content.contains("Microsoft.NET.Sdk.Web") || content.contains("System.Web.Mvc") {
                        return true;
                    }
                }
            }
        }

        // Check for Program.cs with WebApplication
        if let Some(program_cs) = ctx.read_file("Program.cs").filter(|c| !c.is_empty()) {
            if program_cs.contains("WebApplication") || program_cs.contains("CreateHostBuilder") || program_cs.contains("UseStartup") {
                return true;
            }
        }

        // Check for Startup.cs (ASP.NET Core signature)
        if ctx.file_exists("Startup.cs") {
            return true;
        }

        // ASP.NET signatures in controller/entrypoint SOURCE — covers feature-folder apps with no
        // `/Controllers/` dir and a subdir `Program.cs` that the root-only checks above miss (e.g.
        // realworld: Features/*/FooController.cs). `.csproj` often isn't in the indexed source set,
        // so source-scan is the reliable signal. Endpoint-group apps (`Endpoints/TodoItems.cs :
        // IEndpointGroup`) have none of those names: their files, and the extension that maps
        // them, count.
        for file in &all_files {
            if !re!(r"(?:Controller|Program|Startup|Endpoints?|Extensions)\.cs$").is_match(file) && !re!(r"(?:^|/)Endpoints/[^/]+\.cs$").is_match(file) {
                continue;
            }
            let Some(c) = ctx.read_file(file).filter(|c| !c.is_empty()) else { continue };
            if re!(r"\[(?:ApiController|Route|Http(?:Get|Post|Put|Patch|Delete))\b").is_match(&c)
                || c.contains("ControllerBase")
                || c.contains(": Controller")
                || c.contains("MapControllers")
                || c.contains("WebApplication")
                || c.contains("Microsoft.AspNetCore")
                || c.contains("IEndpointGroup")
                || c.contains("EndpointGroupBase")
                || c.contains("RouteGroupBuilder")
            {
                return true;
            }
        }
        false
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        let name = r.reference_name.as_str();

        // Pattern 1: Controller references
        if name.ends_with("Controller") {
            if let Some(result) = resolve_by_name_and_kind(r, &CLASS_KINDS, &CONTROLLER_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.85, ResolvedBy::Framework));
            }
        }

        // Pattern 2: Service references (dependency injection)
        if name.ends_with("Service") || (name.starts_with('I') && utf16_len(name) > 1) {
            if let Some(result) = resolve_by_name_and_kind(r, &SERVICE_KINDS, &SERVICE_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.85, ResolvedBy::Framework));
            }
        }

        // Pattern 3: Repository references
        if name.ends_with("Repository") {
            if let Some(result) = resolve_by_name_and_kind(r, &SERVICE_KINDS, &REPO_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.85, ResolvedBy::Framework));
            }
        }

        // Pattern 4: Model/Entity references
        if re!(r"^[A-Z][a-zA-Z]+$").is_match(name) {
            if let Some(result) = resolve_by_name_and_kind(r, &CLASS_KINDS, &MODEL_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.7, ResolvedBy::Framework));
            }
        }

        // Pattern 5: ViewModel references
        if name.ends_with("ViewModel") || name.ends_with("Dto") {
            if let Some(result) = resolve_by_name_and_kind(r, &CLASS_KINDS, &VIEWMODEL_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.8, ResolvedBy::Framework));
            }
        }

        None
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<FrameworkExtractionResult> {
        if !file_path.ends_with(".cs") {
            return Some(FrameworkExtractionResult::default());
        }
        let mut nodes: Vec<Node> = Vec::new();
        let mut references: Vec<UnresolvedRef> = Vec::new();
        let now = now_ms();
        let safe = codegraph_text::strip_comments_for_regex(content, "csharp");
        let safe: &str = &safe;

        let route_node = |line: u32, method: &str, name_path: &str, qualified_name: String, end_column: u32| {
            Node::new(
                format!("route:{file_path}:{line}:{method}:{name_path}"),
                NodeKind::Route,
                format!("{method} {name_path}"),
                qualified_name,
                file_path,
                Language::CSharp,
                line,
                line,
                0,
                end_column,
                now,
            )
        };
        let line_of = |index: usize| safe[..index].matches('\n').count() as u32 + 1;

        // Class-level [Route("api/[controller]")] prefix — joined onto each action.
        let class_prefix = re!(
            r#"\[Route\s*\(\s*"([^"]+)"[^)]*\)\]\s*(?:\[[^\]]*\]\s*)*(?:public\s+|sealed\s+|abstract\s+|partial\s+)*class\b"#
        )
        .captures(safe)
        .and_then(|c| c.text(1).map(String::from))
        .unwrap_or_default();

        // [HttpGet], [HttpGet("path")], [HttpPost("path", Name="x")] — BARE or with a path. (The
        // old regex required a string, so bare attributes — with the route on the class [Route] —
        // were missed; eShopOnWeb was 24 bare / 2 string.)
        let attr_regex = re!(r#"\[(HttpGet|HttpPost|HttpPut|HttpPatch|HttpDelete)(?:\s*\(\s*"([^"]+)"[^)]*\))?\s*\]"#, "g");
        for m in attr_regex.captures_iter(safe) {
            let Some(whole) = m.get(0) else { continue };
            let verb = m.text(1).unwrap_or("");
            let method = verb.strip_prefix("Http").unwrap_or(verb).to_uppercase();
            let route_path = join_cs_path(&class_prefix, m.text(2).unwrap_or(""));
            let line = line_of(whole.start());

            let node = route_node(line, &method, &route_path, format!("{file_path}::route:{route_path}"), utf16_len(whole.as_str()) as u32);
            let route_id = node.id.clone();
            nodes.push(node);

            // Next method declaration (skip stacked attributes; C# puts the return type before
            // the name). Bounded so we don't grab a far one.
            let tail = utf16_prefix(&safe[whole.end()..], 600);
            if let Some(method_match) = re!(r"(?:public|private|protected|internal)\s+[\w<>,\s\[\]?.]+?\s+(\w+)\s*\(").captures(tail) {
                references.push(UnresolvedRef::new(route_id, method_match.text(1).unwrap_or(""), EdgeKind::References, line, 0, file_path, Language::CSharp));
            }
        }

        // Minimal APIs: app.MapGet("/path", handler)
        let minimal_regex = re!(r#"\.Map(Get|Post|Put|Patch|Delete)\s*\(\s*"([^"]+)"\s*,\s*([^,)]+)"#, "g");
        for m in minimal_regex.captures_iter(safe) {
            let Some(whole) = m.get(0) else { continue };
            // `app.MapGet("api/todos", …)` serves `/api/todos`.
            let route_path = join_cs_path("", m.text(2).unwrap_or(""));
            let method = m.text(1).unwrap_or("").to_uppercase();
            let line = line_of(whole.start());

            let node = route_node(line, &method, &route_path, format!("{file_path}::route:{route_path}"), utf16_len(whole.as_str()) as u32);
            let route_id = node.id.clone();
            nodes.push(node);

            if let Some(handler_name) = extract_csharp_tail_ident(m.text(3).unwrap_or("")) {
                references.push(UnresolvedRef::new(route_id, handler_name, EdgeKind::References, line, 0, file_path, Language::CSharp));
            }
        }

        // Minimal APIs, handler first — the endpoint-group idiom (Jason Taylor's Clean
        // Architecture template and its descendants):
        //
        //   public class TodoItems : IEndpointGroup {
        //     public static void Map(RouteGroupBuilder group) {
        //       group.MapPost(CreateTodoItem);
        //       group.MapPut(UpdateTodoItem, "{id}");
        //
        // The class is the group, the handler is the first argument, the path the optional
        // second. The group's prefix is the app's convention (`$"/api/{groupName}"`, read
        // repo-wide in postExtract) or the class's own `RoutePrefix` literal; here the route is
        // named under the class.
        let group_regex = re!(r#"\.Map(Get|Post|Put|Patch|Delete)\s*\(\s*([A-Za-z_]\w*)\s*(?:,\s*"([^"]*)")?\s*\)"#, "g");
        let route_prefix_literal = re!(r#"\bRoutePrefix\s*(?:=>|=)\s*"([^"]+)""#).captures(safe).and_then(|c| c.text(1).map(String::from));
        for m in group_regex.captures_iter(safe) {
            let Some(whole) = m.get(0) else { continue };
            let method = m.text(1).unwrap_or("").to_uppercase();
            let handler_name = m.text(2).unwrap_or("");
            let sub = m.text(3);
            let line = line_of(whole.start());
            let before = &safe[..whole.start()];
            let Some(class_match) = re!(r"\bclass\s+([A-Za-z_]\w*)", "g").captures_iter(before).last() else { continue };
            let group = class_match.text(1).unwrap_or("");
            let route_path = join_cs_path(&route_prefix_literal.clone().unwrap_or_else(|| format!("/{group}")), sub.unwrap_or(""));
            let node = route_node(
                line,
                &method,
                &route_path,
                format!("{file_path}::group:{group}:{method}:{}", sub.unwrap_or("")),
                utf16_len(whole.as_str()) as u32,
            );
            let route_id = node.id.clone();
            nodes.push(node);
            references.push(UnresolvedRef::new(route_id, handler_name, EdgeKind::References, line, 0, file_path, Language::CSharp));
        }

        // FastEndpoints: an endpoint class (`: Endpoint<TReq, TRes>`, `EndpointWithoutRequest`,
        // `Ep.Req<…>.Res<…>`) declares its verb and path in `Configure()` — `Get("/Contributors")`,
        // `Post(CreateContributorRequest.Route)` — and handles the request in its own
        // `HandleAsync` / `ExecuteAsync`. A constant path (`X.Route`, usually in the request's own
        // file) is read in postExtract; until then the route is named by the expression.
        let endpoint_class = re!(r"\bclass\s+([A-Za-z_]\w*)[^{;]*?:\s*(?:FastEndpoints\.)?(?:Endpoint(?:WithoutRequest|WithoutResponse)?\b|Ep\.)", "g");
        for m in endpoint_class.captures_iter(safe) {
            let Some(whole) = m.get(0) else { continue };
            let class_start = whole.start();
            let body = &safe[class_start..];
            let Some(configure) = re!(r"\bvoid\s+Configure\s*\(\s*\)\s*\{").find(body) else { continue };
            let handler = re!(r"\b(HandleAsync|ExecuteAsync)\s*\(").captures(body).and_then(|c| c.text(1).map(String::from));
            let verb_regex = re!(r"\b(Get|Post|Put|Patch|Delete)\s*\(\s*([^;]*?)\s*\)\s*;", "g");
            let configure_end = utf16_byte_index(body, utf16_len(&body[..configure.start()]) + 3000);
            let mut last_index = configure.start();
            while let Some(verb) = verb_regex.captures_at(body, last_index) {
                let Some(verb_whole) = verb.get(0) else { break };
                if verb_whole.start() >= configure_end {
                    break;
                }
                last_index = verb_whole.end();
                let method = verb.text(1).unwrap_or("").to_uppercase();
                let args = verb.text(2).unwrap_or("");
                // `$"/{nameof(Project)}s"` is `/Projects`.
                let literals: Vec<String> = re!(r#""([^"]+)""#, "g")
                    .captures_iter(args)
                    .map(|l| {
                        let unwrapped = re!(r"\{\s*nameof\s*\(\s*(\w+)\s*\)\s*\}", "g")
                            .replace_all(l.text(1).unwrap_or(""), |c| Some(c.text(1).unwrap_or("").to_string()));
                        join_cs_path("", &unwrapped)
                    })
                    .collect();
                let constant = re!(r"^([A-Za-z_]\w*)\.([A-Za-z_]\w*)$").captures(args);
                // (route name, qualified-name key)
                let targets: Vec<(String, String)> = if !literals.is_empty() {
                    literals.into_iter().map(|p| (p.clone(), p)).collect()
                } else if let Some(c) = &constant {
                    let expr = format!("{}.{}", c.text(1).unwrap_or(""), c.text(2).unwrap_or(""));
                    vec![(expr.clone(), format!("const:{expr}"))]
                } else {
                    Vec::new()
                };
                let line = line_of(class_start + verb_whole.start());
                for (name, key) in targets {
                    let node = route_node(line, &method, &name, format!("{file_path}::fastendpoint:{method}:{key}"), 0);
                    let id = node.id.clone();
                    nodes.push(node);
                    if let Some(handler) = &handler {
                        references.push(UnresolvedRef::new(id, handler.as_str(), EdgeKind::References, line, 0, file_path, Language::CSharp));
                    }
                }
                // A second verb call (`Get(...); Post(...)`) is a second route; stop at
                // Configure's end.
                if re!(r"\n\s*\}\s*\n").is_match(&body[configure.start()..verb_whole.start()]) {
                    break;
                }
            }
        }

        Some(FrameworkExtractionResult { nodes, references })
    }

    /// Route names only the whole repository can give: FastEndpoints constant paths,
    /// endpoint-group prefixes.
    fn post_extract(&self, ctx: &Ctx) -> Option<Vec<Node>> {
        let mut updates = fast_endpoint_constant_routes(ctx);
        updates.extend(endpoint_group_routes(ctx));
        Some(updates)
    }
}

/// A FastEndpoints route named by a constant (`Post(CreateContributorRequest.Route)`) gets the
/// constant's value: `public const string Route = "/Contributors";` in that class, wherever it is
/// declared. Idempotent — the qualified name keeps the expression.
fn fast_endpoint_constant_routes(ctx: &Ctx) -> Vec<Node> {
    let mut updates: Vec<Node> = Vec::new();
    for route in ctx.get_nodes_by_kind(NodeKind::Route).iter() {
        if route.language != Language::CSharp {
            continue;
        }
        let Some(q) = re!(r"::fastendpoint:([A-Z]+):const:([A-Za-z_]\w*)\.([A-Za-z_]\w*)$").captures(&route.qualified_name) else { continue };
        let method = q.text(1).unwrap_or("");
        let owner = q.text(2).unwrap_or("");
        let field = q.text(3).unwrap_or("");
        let mut value: Option<String> = None;
        for decl in ctx.get_nodes_by_name(owner).iter() {
            if decl.language != Language::CSharp || !matches!(decl.kind, NodeKind::Class | NodeKind::Struct) {
                continue;
            }
            let text = ctx.read_file(&decl.file_path).unwrap_or_else(|| "".into());
            let pattern = format!(r#"\b(?:const\s+string|static\s+readonly\s+string|static\s+string)\s+{field}\s*=\s*"([^"]+)""#);
            let declared = Re::cached(&pattern, "");
            let hit = declared.captures(&text).and_then(|m| m.text(1).map(String::from));
            if hit.is_some() {
                value = hit;
                break;
            }
        }
        let Some(value) = value.filter(|v| !v.is_empty()) else { continue };
        let name = format!("{method} {}", join_cs_path("", &value));
        if name != route.name {
            let mut updated = (**route).clone();
            updated.name = name;
            updates.push(updated);
        }
    }
    updates
}

/// The endpoint-group prefix convention, read once from the app: the
/// `MapGroup($"/api/{groupName}")` that registers every `IEndpointGroup` (or `EndpointGroupBase`)
/// under a head — `/api/` — before the class name. A group route extracted as `POST /TodoItems`
/// becomes `POST /api/TodoItems`; a class with its own `RoutePrefix` literal already has its path.
/// Idempotent: `qualifiedName` keeps the group and the sub-path.
fn endpoint_group_routes(ctx: &Ctx) -> Vec<Node> {
    let mut head: Option<String> = None;
    let mut looked = 0;
    for file in ctx.get_all_files() {
        if !file.ends_with(".cs") {
            continue;
        }
        let Some(content) = ctx.read_file(&file).filter(|c| !c.is_empty()) else { continue };
        if !content.contains("MapGroup") {
            continue;
        }
        looked += 1;
        if looked > 400 {
            break;
        }
        let m = re!(r#"\$"([^"{]*)\{\s*(?:groupName|type\.Name|name|prefix)\s*\}""#)
            .captures(&content)
            .or_else(|| re!(r#"MapGroup\(\s*\$"([^"{]*)\{"#).captures(&content));
        if let Some(m) = m {
            head = m.text(1).map(String::from);
            break;
        }
    }
    let head = match head {
        Some(h) if !h.is_empty() && h != "/" => h,
        _ => return Vec::new(),
    };
    let mut updates: Vec<Node> = Vec::new();
    for route in ctx.get_nodes_by_kind(NodeKind::Route).iter() {
        if route.language != Language::CSharp {
            continue;
        }
        let Some(q) = re!(r"::group:([A-Za-z_]\w*):([A-Z]+):(.*)$").captures(&route.qualified_name) else { continue };
        if let Some(content) = ctx.read_file(&route.file_path).filter(|c| !c.is_empty()) {
            if re!(r#"\bRoutePrefix\s*(?:=>|=)\s*""#).is_match(&content) {
                continue;
            }
        }
        let prefix = format!("{}/{}", re!(r"/+$").replace_first(&head, |_| String::new()), q.text(1).unwrap_or(""));
        let name = format!("{} {}", q.text(2).unwrap_or(""), join_cs_path(&prefix, q.text(3).unwrap_or("")));
        if name != route.name {
            let mut updated = (**route).clone();
            updated.name = name;
            updates.push(updated);
        }
    }
    updates
}

/// Join a class-level [Route] prefix and an action's path into one normalized `/path`.
fn join_cs_path(prefix: &str, sub: &str) -> String {
    let parts: Vec<&str> = [prefix, sub].iter().map(|p| p.trim_matches('/')).filter(|p| !p.is_empty()).collect();
    format!("/{}", parts.join("/"))
}

/// Extract last identifier from an expression like `MyService.Handler` or `Handler`.
fn extract_csharp_tail_ident(expr: &str) -> Option<String> {
    let cleaned = re!(r"\s+", "g").replace_all_str(js_trim(expr), "");
    re!(r"(?:\.|^)([A-Za-z_][A-Za-z0-9_]*)$").captures(&cleaned).and_then(|m| m.text(1).map(String::from))
}

/// A framework name heuristic's pick (see `name_heuristic`), preferring these folders.
fn resolve_by_name_and_kind(r: &UnresolvedRef, kinds: &[NodeKind], preferred_dir_patterns: &[&str], ctx: &Ctx) -> Option<String> {
    pick_by_name_and_kind(r, kinds, &|f: &str| preferred_dir_patterns.iter().any(|d| f.contains(d)), ctx, &NamePickOptions::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_join_without_doubled_slashes() {
        assert_eq!(join_cs_path("api/[controller]", ""), "/api/[controller]");
        assert_eq!(join_cs_path("/api/", "/items/"), "/api/items");
        assert_eq!(join_cs_path("", ""), "/");
    }

    #[test]
    fn controller_attributes_and_minimal_apis_become_routes() {
        let src = "[Route(\"api/[controller]\")]\npublic class ItemsController : ControllerBase\n{\n    [HttpGet(\"{id}\")]\n    public IActionResult Get(int id) => Ok();\n\n    [HttpPost]\n    public IActionResult Create() => Ok();\n}\n\napp.MapGet(\"/ping\", Ping.Handler);\n";
        let result = AspnetResolver.extract("Controllers/ItemsController.cs", src).unwrap();
        let names: Vec<&str> = result.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["GET /api/[controller]/{id}", "POST /api/[controller]", "GET /ping"]);
        let refs: Vec<&str> = result.references.iter().map(|r| r.reference_name.as_str()).collect();
        assert_eq!(refs, vec!["Get", "Create", "Handler"]);
    }

    #[test]
    fn endpoint_groups_and_fast_endpoints() {
        let src = "public class TodoItems : IEndpointGroup\n{\n    public static void Map(RouteGroupBuilder group)\n    {\n        group.MapPost(CreateTodoItem);\n        group.MapPut(UpdateTodoItem, \"{id}\");\n    }\n}\n";
        let result = AspnetResolver.extract("Endpoints/TodoItems.cs", src).unwrap();
        let names: Vec<&str> = result.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["POST /TodoItems", "PUT /TodoItems/{id}"]);
        assert_eq!(result.nodes[1].qualified_name, "Endpoints/TodoItems.cs::group:TodoItems:PUT:{id}");

        let fast = "public class Create : Endpoint<Req, Res>\n{\n    public override void Configure()\n    {\n        Post(\"/Contributors\");\n    }\n\n    public override async Task HandleAsync(Req req, CancellationToken ct) {}\n}\n";
        let result = AspnetResolver.extract("Create.cs", fast).unwrap();
        assert_eq!(result.nodes.len(), 1);
        assert_eq!(result.nodes[0].name, "POST /Contributors");
        assert_eq!(result.references[0].reference_name, "HandleAsync");
    }
}
