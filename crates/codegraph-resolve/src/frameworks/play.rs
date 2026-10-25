//! Play Framework (Scala/Java) resolver. Port of `src/resolution/frameworks/play.ts`.
//!
//! Play declares HTTP routes in a dedicated `conf/routes` file (and included `conf/*.routes`),
//! Rails-style:
//!
//! ```text
//! GET   /computers        controllers.Application.list(p: Int ?= 0)
//! POST  /computers        controllers.Application.save
//! GET   /assets/*file     controllers.Assets.versioned(path = "/public", file: Asset)
//! ```
//!
//! The file is extensionless, so the file walk only indexes it because `is_play_routes_file` opts
//! it in; it's processed through the no-grammar path and this resolver extracts the routes. Each
//! route references its handler as `Controller.method` (the package prefix is dropped), resolved to
//! the action method in the controller class.

use super::now_ms;
use crate::ctx::Ctx;
use crate::name_matcher::reference::{is_java_outside_import, retarget_self_overload};
use crate::types::NodeRef;
use crate::types::{FrameworkExtractionResult, FrameworkResolver, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_extract::languages::is_play_routes_file;
use codegraph_jsre::re;
use codegraph_types::jscompat::js_trim;
use codegraph_types::{EdgeKind, Language, Node, NodeKind};

pub struct PlayResolver;

// `yaml` so this resolver runs on conf/routes (detectLanguage maps it to yaml); `scala`/`java` so
// it's active in Play projects of either language.
const LANGUAGES: [Language; 3] = [Language::Scala, Language::Java, Language::Yaml];

fn qualified_action_re() -> &'static codegraph_jsre::Re {
    re!(r"^([A-Za-z_]\w*)\.([A-Za-z_]\w*)$")
}

impl FrameworkResolver for PlayResolver {
    fn name(&self) -> &str {
        "play"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        if let Some(build_sbt) = ctx.read_file("build.sbt").filter(|s| !s.is_empty()) {
            if codegraph_jsre::Re::cached(r#"playframework|"play"|sbt-plugin|PlayScala|PlayJava"#, "i").is_match(&build_sbt) {
                return true;
            }
        }
        if ctx.file_exists("conf/routes") {
            return true;
        }
        if ctx.file_exists("conf/application.conf") {
            return true;
        }
        // A repository of Play projects (playframework/play-samples) keeps each one's
        // `conf/routes` in its own directory.
        ctx.get_all_files().iter().any(|f| is_play_routes_file(f))
    }

    // The handler is `Controller.method` (a class-qualified action), which names no bare declared
    // symbol, so resolveOne's pre-filter could drop it — claim it.
    fn claims_reference(&self, name: &str) -> bool {
        qualified_action_re().is_match(name)
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        let m = qualified_action_re().captures(&r.reference_name)?;
        let class_name = m.text(1)?;
        let method_name = m.text(2)?;
        let route = is_play_routes_file(&r.file_path);
        // `import org.apache.logging.log4j.core.config.Configurator` makes a Java file's
        // `Configurator` that library's, never a project type of the name.
        if !route && r.language == Language::Java && is_java_outside_import(class_name, r, ctx) {
            return None;
        }
        // A route's controller is a class or a Scala `object` (a companion shares its class's
        // qualified name); a call in code may also name a Java interface or enum, which hold
        // static members of their own.
        let owner_kind = |kind: NodeKind| {
            matches!(kind, NodeKind::Class | NodeKind::Module) || (!route && matches!(kind, NodeKind::Interface | NodeKind::Enum))
        };
        let owners: Vec<NodeRef> = ctx.get_nodes_by_name(class_name).iter().filter(|n| owner_kind(n.kind)).cloned().collect();
        // A call's own language comes first: Play's `RequestAuthority` is both a Scala case class
        // and a Java record, and each language's callers mean their own; the other language's
        // types follow, since Play's Java API calls its Scala classes. A route file has no
        // language of its own.
        let groups: Vec<Vec<&NodeRef>> = if route {
            vec![owners.iter().collect()]
        } else {
            vec![
                owners.iter().filter(|n| n.language == r.language).collect(),
                owners.iter().filter(|n| n.language != r.language).collect(),
            ]
        };
        for group in groups {
            // Only a member of THAT type counts: a file can hold many classes (Java nested
            // classes, a Scala class + its companion + siblings), and the first same-named method
            // in the file is usually another class's.
            let mut members: Vec<NodeRef> = Vec::new();
            for cls in group {
                let member_qn = format!("{}::{method_name}", cls.qualified_name);
                for n in ctx.get_nodes_in_file(&cls.file_path).iter() {
                    if matches!(n.kind, NodeKind::Method | NodeKind::Function)
                        && n.name == method_name
                        && n.qualified_name == member_qn
                        && !members.iter().any(|m| m.id == n.id)
                    {
                        members.push(n.clone());
                    }
                }
            }
            if members.is_empty() {
                continue;
            }
            // Same-named types in several packages (Scala names carry no package): the one in
            // the caller's own file comes first.
            let pick = if route { None } else { members.iter().find(|n| n.file_path == r.file_path) }.unwrap_or(&members[0]);
            let result = ResolvedRef::new(r, pick.id.clone(), 0.9, ResolvedBy::Framework);
            // Overloads: the one the call's arguments fit, as the generic resolver picks it.
            return Some(if route { result } else { retarget_self_overload(result, r, ctx) });
        }
        // An inherited member isn't looked up here: the generic resolver finds it through the
        // class's declared supertypes.
        None
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<FrameworkExtractionResult> {
        if !is_play_routes_file(file_path) {
            return Some(FrameworkExtractionResult::default());
        }
        let mut nodes: Vec<Node> = Vec::new();
        let mut references: Vec<UnresolvedRef> = Vec::new();
        let now = now_ms();

        for (i, raw) in content.split('\n').enumerate() {
            let line = js_trim(raw);
            // Skip comments and `->` route includes (a sub-router mount, not an action).
            if line.is_empty() || line.starts_with('#') || line.starts_with("->") {
                continue;
            }
            let Some(m) = re!(r"^(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\s+(\S+)\s+(.+)$").captures(line) else { continue };
            let method = m.text(1).unwrap_or("");
            let route_path = m.text(2).unwrap_or("");
            let action = m.text(3).unwrap_or("");

            // action: `controllers.Application.list(p: Int ?= 0)` → drop args, keep the last
            // `Controller.method` segment (package prefix is irrelevant for lookup).
            let fqn = js_trim(action.split('(').next().unwrap_or(""));
            let parts: Vec<&str> = fqn.split('.').filter(|p| !p.is_empty()).collect();
            if parts.len() < 2 {
                continue;
            }
            let handler_ref = parts[parts.len() - 2..].join("."); // Application.list

            let line_num = i as u32 + 1;
            let route_node = Node::new(
                format!("route:{file_path}:{line_num}:{method}:{route_path}"),
                NodeKind::Route,
                format!("{method} {route_path}"),
                format!("{file_path}::{method}:{route_path}"),
                file_path,
                Language::Scala,
                line_num,
                line_num,
                0,
                0,
                now,
            );
            references.push(UnresolvedRef::new(route_node.id.clone(), handler_ref, EdgeKind::References, line_num, 0, file_path, Language::Scala));
            nodes.push(route_node);
        }

        Some(FrameworkExtractionResult { nodes, references })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_reference_their_controller_action() {
        let src = "# comment\nGET   /computers        controllers.Application.list(p: Int ?= 0)\nPOST  /computers        controllers.Application.save\n-> /admin admin.Routes\nGET /x Bare\n";
        let result = PlayResolver.extract("conf/routes", src).unwrap();
        let names: Vec<&str> = result.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["GET /computers", "POST /computers"]);
        let refs: Vec<&str> = result.references.iter().map(|r| r.reference_name.as_str()).collect();
        assert_eq!(refs, vec!["Application.list", "Application.save"]);
        assert_eq!(result.nodes[0].id, "route:conf/routes:2:GET:/computers");
    }
}
