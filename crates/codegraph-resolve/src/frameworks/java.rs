//! Spring (Java/Kotlin) resolver. Port of `src/resolution/frameworks/java.ts`.

use super::name_heuristic::{pick_by_name_and_kind, NamePickOptions};
use super::now_ms;
use crate::ctx::Ctx;
use crate::types::{FrameworkExtractionResult, FrameworkResolver, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_types::jscompat::{js_trim, utf16_len, utf16_prefix};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use std::collections::HashMap;

const LANGUAGES: [Language; 4] = [Language::Java, Language::Kotlin, Language::Yaml, Language::Properties];

// Directory patterns
const SERVICE_DIRS: [&str; 2] = ["/service/", "/services/"];
const REPO_DIRS: [&str; 2] = ["/repository/", "/repositories/"];
const CONTROLLER_DIRS: [&str; 2] = ["/controller/", "/controllers/"];
const ENTITY_DIRS: [&str; 5] = ["/entity/", "/entities/", "/model/", "/models/", "/domain/"];
const COMPONENT_DIRS: [&str; 3] = ["/component/", "/components/", "/config/"];

const CLASS_KINDS: [NodeKind; 1] = [NodeKind::Class];
const SERVICE_KINDS: [NodeKind; 2] = [NodeKind::Class, NodeKind::Interface];

fn line_of(text: &str, index: usize) -> u32 {
    text[..index].matches('\n').count() as u32 + 1
}

pub struct SpringResolver;

impl FrameworkResolver for SpringResolver {
    fn name(&self) -> &str {
        "spring"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    fn claims_reference(&self, name: &str) -> bool {
        // `@ConfigurationProperties(prefix="app.cache")` emits a reference whose name carries the
        // `:prefix` sentinel — there's no declared symbol with that exact spelling, so the
        // resolver's name-existence pre-filter would drop it. Opt those through.
        name.ends_with(":prefix")
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        // Check for pom.xml / build.gradle / build.gradle.kts with Spring
        for file in ["pom.xml", "build.gradle", "build.gradle.kts"] {
            if ctx.read_file(file).is_some_and(|c| !c.is_empty() && (c.contains("spring-boot") || c.contains("springframework"))) {
                return true;
            }
        }

        // Check for Spring annotations in Java files
        for file in ctx.get_all_files() {
            if file.ends_with(".java") {
                if let Some(content) = ctx.read_file(&file).filter(|c| !c.is_empty()) {
                    if content.contains("@SpringBootApplication")
                        || content.contains("@RestController")
                        || content.contains("@Service")
                        || content.contains("@Repository")
                    {
                        return true;
                    }
                }
            }
        }

        false
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        let name = r.reference_name.as_str();
        // Spring config-key references — `@Value("${key}")` (single leaf) and
        // `@ConfigurationProperties(prefix="X")` (entire subtree, marked with the `:prefix` suffix
        // in extractSpringValueBindings). Lookup goes through Spring's relaxed binding
        // (kebab/camel/snake → canonical lowercase).
        if let Some(prefix) = name.strip_suffix(":prefix") {
            let canon_prefix = canonical_config_key(prefix);
            // Prefer an exact prefix match (one node = the prefix subtree). Without node-level
            // subtree representation we map to the closest matching key.
            let candidates: Vec<_> = ctx
                .get_nodes_by_kind(NodeKind::Constant)
                .iter()
                .filter(|n| matches!(n.language, Language::Yaml | Language::Properties) && canonical_config_key(&n.qualified_name).starts_with(&canon_prefix))
                .cloned()
                .collect();
            // Pick the SHORTEST canonical name — it's the closest binding point
            // (`app.cache` over `app.cache.name.user-token` for prefix=`app.cache`).
            let mut best = candidates.first()?;
            for b in &candidates[1..] {
                if canonical_len(&best.qualified_name) > canonical_len(&b.qualified_name) {
                    best = b;
                }
            }
            return Some(ResolvedRef::new(r, best.id.clone(), 0.85, ResolvedBy::Framework));
        }
        // Spring config-key resolution: `@Value("${a.b.c}")` and `@ConfigurationProperties`. Gate
        // on the `references` kind — those bindings are emitted as `references` by
        // extractSpringValueBindings, whereas the far more numerous method-call refs
        // (`list.add()`, `builder.build()`, every `receiver.method()`) are `calls`. A config key
        // is NEVER a `calls` ref, so this loses no resolution. Without the gate, every dotted
        // `calls` ref fell into the uncached getNodesByKind('constant') scan below — an
        // O(dotted-calls × constant-nodes) cost that dominated resolution and made a full index
        // take ~1h on large Java/Kotlin (Spring) monorepos (#1180). The old
        // `split('.').length >= 2` heuristic couldn't separate keys from calls; the kind check
        // does it exactly.
        let jvm = matches!(r.language, Language::Java | Language::Kotlin);
        if r.is_kind(EdgeKind::References) && jvm && name.contains('.') && !name.contains("::") {
            let canon_ref = canonical_config_key(name);
            let candidates: Vec<_> = ctx
                .get_nodes_by_kind(NodeKind::Constant)
                .iter()
                .filter(|n| n.kind == NodeKind::Constant && matches!(n.language, Language::Yaml | Language::Properties) && canonical_config_key(&n.qualified_name) == canon_ref)
                .cloned()
                .collect();
            if candidates.len() == 1 {
                return Some(ResolvedRef::new(r, candidates[0].id.clone(), 0.9, ResolvedBy::Framework));
            }
            if candidates.len() > 1 {
                // Multiple profile-specific files (application-dev.yml + application-prod.yml) can
                // define the same key. Prefer the one with the shortest profile suffix (the base
                // `application.yml` wins over profile variants when both exist), then by
                // alphabetical path so the pick is deterministic across reindexes.
                let score = |n: &Node| {
                    let base = n.file_path.rsplit('/').next().unwrap_or("");
                    let is_base = Re::cached(r"^(application|bootstrap)\.(yml|yaml|properties)$", "i").is_match(base);
                    (if is_base { 0 } else { 1 }) * 1000 + utf16_len(base)
                };
                let mut best = &candidates[0];
                for b in &candidates[1..] {
                    if score(best) > score(b) {
                        best = b;
                    }
                }
                return Some(ResolvedRef::new(r, best.id.clone(), 0.75, ResolvedBy::Framework));
            }
        }

        // The DI/convention patterns below (1–5) are Spring-specific heuristics: they must only
        // fire on Java/Kotlin refs, and never on inheritance refs. Without these gates a Scala
        // `extends ExtCustomer` (Spring detected via a sibling Java module) was hijacked by
        // Pattern 4's bare-name fallback to an unrelated same-named test class —
        // `extends`/`implements` must resolve via imports/name matching, not directory
        // heuristics.
        if !jvm {
            return None;
        }
        if r.is_kind(EdgeKind::Extends) || r.is_kind(EdgeKind::Implements) {
            return None;
        }

        // Pattern 1: Service references (dependency injection)
        if name.ends_with("Service") {
            if let Some(result) = resolve_by_name_and_kind(r, &SERVICE_KINDS, &SERVICE_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.85, ResolvedBy::Framework));
            }
        }

        // Pattern 2: Repository references
        if name.ends_with("Repository") {
            if let Some(result) = resolve_by_name_and_kind(r, &SERVICE_KINDS, &REPO_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.85, ResolvedBy::Framework));
            }
        }

        // Pattern 3: Controller references
        if name.ends_with("Controller") {
            if let Some(result) = resolve_by_name_and_kind(r, &CLASS_KINDS, &CONTROLLER_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.85, ResolvedBy::Framework));
            }
        }

        // Pattern 4: Entity/Model references
        if re!(r"^[A-Z][a-zA-Z]+$").is_match(name) {
            if let Some(result) = resolve_by_name_and_kind(r, &CLASS_KINDS, &ENTITY_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.7, ResolvedBy::Framework));
            }
        }

        // Pattern 5: Component references
        if name.ends_with("Component") || name.ends_with("Config") {
            if let Some(result) = resolve_by_name_and_kind(r, &CLASS_KINDS, &COMPONENT_DIRS, ctx) {
                return Some(ResolvedRef::new(r, result, 0.8, ResolvedBy::Framework));
            }
        }

        None
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<FrameworkExtractionResult> {
        // Spring config files (application.yml / application.properties / bootstrap.yml +
        // per-profile variants) are extracted on the framework path, not in the language
        // extractor, so the keys become first-class nodes a `@Value("${k}")` reference can
        // resolve to.
        if is_spring_config_file(file_path) {
            return Some(extract_spring_config(file_path, content));
        }
        // Spring Boot is used from both Java and Kotlin (identical @GetMapping etc.
        // annotations); the difference is method syntax — Kotlin `fun name(...)` vs Java
        // `public X name(...)` — handled in the method regex below.
        if !file_path.ends_with(".java") && !file_path.ends_with(".kt") {
            return Some(FrameworkExtractionResult::default());
        }
        let mut nodes: Vec<Node> = Vec::new();
        let mut references: Vec<UnresolvedRef> = Vec::new();
        let now = now_ms();
        let lang = if file_path.ends_with(".kt") { Language::Kotlin } else { Language::Java };
        let safe = codegraph_text::strip_comments_for_regex(content, "java");
        let safe: &str = &safe;
        let mut consts: HashMap<String, String> = HashMap::new();
        for m in re!(r#"\bstatic\s+final\s+String\s+(\w+)\s*=\s*"([^"]*)"\s*;"#, "g").captures_iter(safe) {
            consts.insert(m.text(1).unwrap_or("").to_string(), m.text(2).unwrap_or("").to_string());
        }

        // Class-level @RequestMapping prefix (an @RequestMapping whose tail leads to a `class`).
        // Joined onto each method's path — and, crucially, NOT treated as a route itself (the old
        // regex did, creating one bogus class route and missing every BARE method mapping like
        // `@PostMapping` with the path on the class).
        let cls = re!(r"@RequestMapping\s*\(([^)]*)\)\s*(?:@[\w.]+(?:\([^)]*\))?\s*)*(?:public\s+|final\s+|abstract\s+|open\s+|data\s+|sealed\s+)*class\b").captures(safe);
        let class_prefixes = match &cls {
            Some(c) => parse_mapping_paths(c.text(1).unwrap_or(""), &consts),
            None => vec![String::new()],
        };

        let method_re = re!(r"\bfun\s+(\w+)\s*\(|\b(?:public|private|protected)\s+[^;{=]*?\s+(\w+)\s*\(");

        // Verb-specific method mappings — always method-level, BARE or with a path.
        let mapping_regex = re!(r"@(GetMapping|PostMapping|PutMapping|PatchMapping|DeleteMapping)\b\s*(\([^)]*\))?", "g");
        for m in mapping_regex.captures_iter(safe) {
            let Some(whole) = m.get(0) else { continue };
            let method = match m.text(1).unwrap_or("") {
                "GetMapping" => "GET",
                "PostMapping" => "POST",
                "PutMapping" => "PUT",
                "PatchMapping" => "PATCH",
                _ => "DELETE",
            };
            let paths = parse_mapping_paths(&strip_parens(m.text(2).unwrap_or("")), &consts);
            let line = line_of(safe, whole.start());
            // Method it decorates: first declared method after (skip stacked annotations; Java
            // puts the return type before the name). Bounded so we don't grab a far one.
            let tail = utf16_prefix(&safe[whole.end()..], 600);
            let method_match = method_re.captures(tail);
            let handler = method_match.as_ref().and_then(|mm| mm.text(1).or(mm.text(2))).map(String::from);
            for route_path in class_prefixes.iter().flat_map(|prefix| paths.iter().map(move |sub| join_path(prefix, sub))) {
                let route_node = Node::new(
                    format!("route:{file_path}:{line}:{method}:{route_path}"),
                    NodeKind::Route,
                    format!("{method} {route_path}"),
                    format!("{file_path}::route:{route_path}"),
                    file_path,
                    lang,
                    line,
                    line,
                    0,
                    utf16_len(whole.as_str()) as u32,
                    now,
                );
                let id = route_node.id.clone();
                nodes.push(route_node);

                if let Some(handler) = &handler {
                    references.push(UnresolvedRef::new(id, handler.clone(), EdgeKind::References, line, 0, file_path, lang));
                }
            }
        }

        // Method-level @RequestMapping (older style: `@RequestMapping(value="/x",
        // method=RequestMethod.GET)` on a method). The class-level @RequestMapping is the prefix
        // (handled above) — skip it here so it isn't double-counted.
        let req_re = re!(r"@RequestMapping\b\s*(\([^)]*\))?", "g");
        let class_after = re!(r"^\s*(?:@[\w.]+(?:\([^)]*\))?\s*)*(?:public\s+|final\s+|abstract\s+|open\s+|data\s+|sealed\s+)*class\b");
        for m in req_re.captures_iter(safe) {
            let Some(whole) = m.get(0) else { continue };
            let args = strip_parens(m.text(1).unwrap_or(""));
            let after = utf16_prefix(&safe[whole.end()..], 600);
            if class_after.is_match(after) {
                continue; // class-level prefix
            }
            let Some(method_match) = method_re.captures(after) else { continue };
            let handler = method_match.text(1).or(method_match.text(2)).unwrap_or("").to_string();
            let method = match re!(r"method\s*=\s*[{\[]?\s*(?:RequestMethod\.)?(\w+)").captures(&args) {
                Some(verb) => verb.text(1).unwrap_or("").to_uppercase(),
                None => "ANY".to_string(),
            };
            let paths = parse_mapping_paths(&args, &consts);
            let line = line_of(safe, whole.start());
            for route_path in class_prefixes.iter().flat_map(|prefix| paths.iter().map(move |sub| join_path(prefix, sub))) {
                let route_node = Node::new(
                    format!("route:{file_path}:{line}:{method}:{route_path}"),
                    NodeKind::Route,
                    format!("{method} {route_path}"),
                    format!("{file_path}::route:{route_path}"),
                    file_path,
                    lang,
                    line,
                    line,
                    0,
                    utf16_len(whole.as_str()) as u32,
                    now,
                );
                let id = route_node.id.clone();
                nodes.push(route_node);
                references.push(UnresolvedRef::new(id, handler.clone(), EdgeKind::References, line, 0, file_path, lang));
            }
        }

        // @Value("${key}") and @ConfigurationProperties(prefix="...") — bind Spring config-key
        // references in Java/Kotlin source. The reference target is the corresponding
        // YAML/properties leaf-key node emitted by extractSpringConfig; springResolver.resolve
        // looks it up with relaxed binding (kebab/camel/snake collapse).
        extract_spring_value_bindings(file_path, safe, lang, now, &mut nodes, &mut references);

        Some(FrameworkExtractionResult { nodes, references })
    }
}

/// `args.replace(/^\(|\)$/g, '')`
fn strip_parens(s: &str) -> String {
    let s = s.strip_prefix('(').unwrap_or(s);
    s.strip_suffix(')').unwrap_or(s).to_string()
}

/// Spring config file patterns: application(-profile)?.{yml,yaml,properties} + bootstrap
/// variants. Matches the basename, not the path, so a project that vendors `application.yml`
/// under `src/main/resources` and one under `src/test/resources` are both picked up.
fn is_spring_config_file(file_path: &str) -> bool {
    let base = file_path.rsplit('/').next().unwrap_or("");
    Re::cached(r"^(application|bootstrap)(-[\w.-]+)?\.(yml|yaml|properties)$", "i").is_match(base)
}

/// Parse a Spring config file (YAML or .properties) and emit one `constant` node per LEAF key,
/// with `qualifiedName` = the dotted path. Leaf keys are what `@Value("${k}")` references hit;
/// intermediate keys aren't bound by Spring's `@Value` (a `@ConfigurationProperties` class binds
/// a SUBTREE, and those references are resolved at lookup time by prefix-suffix matching).
fn extract_spring_config(file_path: &str, content: &str) -> FrameworkExtractionResult {
    let mut nodes: Vec<Node> = Vec::new();
    let is_properties = Re::cached(r"\.properties$", "i").is_match(file_path);
    let lang = if is_properties { Language::Properties } else { Language::Yaml };
    let now = now_ms();

    let mut emit_leaf = |dotted_key: &str, line: u32, value_text: &str| {
        if dotted_key.is_empty() {
            return;
        }
        let mut node = Node::new(
            format!("spring-config:{file_path}:{line}:{dotted_key}"),
            NodeKind::Constant,
            dotted_key.rsplit('.').next().unwrap_or(dotted_key),
            dotted_key,
            file_path,
            lang,
            line,
            line,
            0,
            utf16_len(value_text) as u32,
            now,
        );
        node.signature = Some(dotted_key.to_string());
        // SECURITY (#383): store the KEY only, never the value. Config files routinely hold
        // secrets (DB passwords, API keys, JDBC URLs with embedded credentials), and surfacing
        // the value here pushes it into agent context unbidden (it lands in
        // codegraph_node/explore output via the docstring). The key is all
        // `@Value`/`@ConfigurationProperties` resolution needs; an agent that genuinely needs a
        // value can read the file directly.
        nodes.push(node);
    };

    let line_split = re!(r"\r?\n");
    if is_properties {
        // Properties format: `k1.k2.k3 = value` (or `:` separator, or no value). Lines starting
        // with `#`/`!` are comments. Backslash continuations are valid but rare; we don't try to
        // join them (a continued value is still a value of the same key).
        for (i, raw) in line_split.split(content).into_iter().enumerate() {
            let trimmed = js_trim(raw);
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
                continue;
            }
            let mut sep: Option<usize> = None;
            let mut chars = raw.char_indices().peekable();
            while let Some((j, ch)) = chars.next() {
                if ch == '=' || ch == ':' {
                    sep = Some(j);
                    break;
                }
                if ch == '\\' && chars.peek().is_some() {
                    chars.next();
                }
            }
            let Some(sep) = sep else { continue };
            let key = js_trim(&raw[..sep]);
            let val = js_trim(&raw[sep + 1..]);
            emit_leaf(key, i as u32 + 1, val);
        }
        return FrameworkExtractionResult { nodes, references: Vec::new() };
    }

    // YAML: indent-based. We track a stack of (indent, key) so the dotted path is built by
    // joining ancestor keys with `.`. A leaf is a line with a value on the same line (after `:`).
    // List items, flow-style scalars, and `---` separators are ignored — they don't bind to
    // `@Value` anyway.
    let mut stack: Vec<(usize, String)> = Vec::new();
    for (i, raw) in line_split.split(content).into_iter().enumerate() {
        let trimmed = js_trim(raw);
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed == "---" || trimmed.starts_with("- ") {
            continue;
        }
        let indent = raw.len() - raw.trim_start_matches(['\t', ' ']).len();
        let colon_idx = {
            let mut in_str: Option<char> = None;
            let mut found = None;
            let mut prev: Option<char> = None;
            for (j, ch) in raw.char_indices() {
                if let Some(q) = in_str {
                    if ch == q && prev != Some('\\') {
                        in_str = None;
                    }
                    prev = Some(ch);
                    continue;
                }
                if ch == '"' || ch == '\'' {
                    in_str = Some(ch);
                    prev = Some(ch);
                    continue;
                }
                if ch == ':' {
                    found = Some(j);
                    break;
                }
                prev = Some(ch);
            }
            found
        };
        let Some(colon_idx) = colon_idx else { continue };
        let key = js_trim(&raw[indent..colon_idx]);
        if key.is_empty() {
            continue;
        }
        let after = js_trim(&raw[colon_idx + 1..]);
        while stack.last().is_some_and(|(top_indent, _)| *top_indent >= indent) {
            stack.pop();
        }
        let dotted = stack.iter().map(|(_, k)| k.as_str()).chain(std::iter::once(key)).collect::<Vec<_>>().join(".");
        if after.is_empty() || after.starts_with('#') {
            stack.push((indent, key.to_string()));
        } else {
            // A leaf with an inline value (or a flow-mapping like `{ a: 1 }` — we emit it as a
            // leaf, not as a subtree; precision is fine for `@Value`).
            let val_stripped = {
                let s = after.strip_prefix(['"', '\'']).unwrap_or(after);
                s.strip_suffix(['"', '\'']).unwrap_or(s)
            };
            emit_leaf(&dotted, i as u32 + 1, val_stripped);
        }
    }
    FrameworkExtractionResult { nodes, references: Vec::new() }
}

/// Append `@Value("${k}")` and `@ConfigurationProperties(prefix=...)` references discovered in
/// `safe` (comments stripped) into the caller's `nodes`/`references` arrays.
fn extract_spring_value_bindings(file_path: &str, safe: &str, lang: Language, now: i64, nodes: &mut Vec<Node>, references: &mut Vec<UnresolvedRef>) {
    let value_re = re!(r#"@Value\s*\(\s*["']\$\{([^}:]+)(?::[^}]*)?\}["']\s*\)"#, "g");
    for m in value_re.captures_iter(safe) {
        let Some(whole) = m.get(0) else { continue };
        let key = js_trim(m.text(1).unwrap_or(""));
        if key.is_empty() {
            continue;
        }
        let line = line_of(safe, whole.start());
        let mut bind_node = Node::new(
            format!("spring-value:{file_path}:{line}:{key}"),
            NodeKind::Constant,
            key,
            format!("{file_path}::@Value:{key}"),
            file_path,
            lang,
            line,
            line,
            0,
            utf16_len(whole.as_str()) as u32,
            now,
        );
        bind_node.signature = Some(format!("@Value(\"{key}\")"));
        let id = bind_node.id.clone();
        nodes.push(bind_node);
        references.push(UnresolvedRef::new(id, key, EdgeKind::References, line, 0, file_path, lang));
    }

    let cp_re = re!(r#"@ConfigurationProperties\s*\(\s*(?:prefix\s*=\s*)?["']([^"']+)["']"#, "g");
    for m in cp_re.captures_iter(safe) {
        let Some(whole) = m.get(0) else { continue };
        let prefix = js_trim(m.text(1).unwrap_or(""));
        if prefix.is_empty() {
            continue;
        }
        let line = line_of(safe, whole.start());
        let mut bind_node = Node::new(
            format!("spring-cp:{file_path}:{line}:{prefix}"),
            NodeKind::Constant,
            prefix,
            format!("{file_path}::@ConfigurationProperties:{prefix}"),
            file_path,
            lang,
            line,
            line,
            0,
            utf16_len(whole.as_str()) as u32,
            now,
        );
        bind_node.signature = Some(format!("@ConfigurationProperties(\"{prefix}\")"));
        let id = bind_node.id.clone();
        nodes.push(bind_node);
        // Mark the reference with a `:prefix` suffix so springResolver.resolve knows to expand it
        // into the SUBTREE rather than a single key.
        references.push(UnresolvedRef::new(id, format!("{prefix}:prefix"), EdgeKind::References, line, 0, file_path, lang));
    }
}

/// Spring's relaxed binding (`cache-list` ↔ `cacheList` ↔ `cache_list` ↔ `CACHE_LIST`) collapses
/// on lowercase + dash/underscore removal. We compare candidate keys to a reference in this
/// canonical form.
fn canonical_config_key(key: &str) -> String {
    key.to_lowercase().replace(['-', '_'], "")
}

fn canonical_len(key: &str) -> usize {
    utf16_len(&canonical_config_key(key))
}

/// All declared paths; [''] for an omitted path, [] for an unresolved one.
fn parse_mapping_paths(args: &str, consts: &HashMap<String, String>) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    let mut has_paths = false;
    // Keep Java/Kotlin arrays and quoted URI variables together when separating attributes.
    // Strings in produces/consumes/etc. are never mapping paths.
    let arg_re = re!(r#"(?:^|,)\s*(?:(\w+)\s*=\s*)?(\{(?:[^"'}]|"[^"]*"|'[^']*')*\}|\[(?:[^"'\]]|"[^"]*"|'[^']*')*\]|"[^"]*"|'[^']*'|[^,]+)"#, "g");
    let value_re = re!(r#"(?:^|,)\s*(?:"([^"]*)"|'([^']*)'|([\w.]+))\s*(?=,|$)"#, "g");
    for arg in arg_re.captures_iter(args) {
        if let Some(name) = arg.text(1).filter(|n| !n.is_empty()) {
            if name != "value" && name != "path" {
                continue;
            }
        }
        has_paths = true;
        let raw = js_trim(arg.text(2).unwrap_or(""));
        let raw = raw.strip_prefix(['{', '[']).unwrap_or(raw);
        let raw = raw.strip_suffix(['}', ']']).unwrap_or(raw);
        let scope = js_trim(raw);
        if scope.is_empty() {
            paths.push(String::new()); // An explicitly empty path array also inherits the prefix.
        }
        for value in value_re.captures_iter(scope) {
            let path = match value.text(1).or(value.text(2)) {
                Some(p) => Some(p.to_string()),
                None => value.text(3).and_then(|ident| consts.get(ident.rsplit('.').next().unwrap_or("")).cloned()),
            };
            if let Some(path) = path {
                paths.push(path);
            }
        }
    }
    if !has_paths {
        return vec![String::new()];
    }
    let mut unique: Vec<String> = Vec::new();
    for p in paths {
        if !unique.contains(&p) {
            unique.push(p);
        }
    }
    unique
}

/// Join a class-level prefix and a method sub-path into one normalized `/path`.
fn join_path(prefix: &str, sub: &str) -> String {
    let parts: Vec<&str> = [prefix, sub].iter().map(|p| p.trim_matches('/')).filter(|p| !p.is_empty()).collect();
    format!("/{}", parts.join("/"))
}

/// A framework name heuristic's pick (see `name_heuristic`), preferring these folders.
fn resolve_by_name_and_kind(r: &UnresolvedRef, kinds: &[NodeKind], preferred_dir_patterns: &[&str], ctx: &Ctx) -> Option<String> {
    pick_by_name_and_kind(r, kinds, &|f: &str| preferred_dir_patterns.iter().any(|d| f.contains(d)), ctx, &NamePickOptions::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_prefix_and_verb_mappings() {
        let src = "@RestController\n@RequestMapping(\"/api\")\npublic class UserController {\n  @GetMapping(\"/users/{id}\")\n  public User get(@PathVariable long id) { return null; }\n  @PostMapping\n  public User create() { return null; }\n  @RequestMapping(value = \"/old\", method = RequestMethod.PUT)\n  public void old() {}\n}\n";
        let result = SpringResolver.extract("UserController.java", src).unwrap();
        let names: Vec<&str> = result.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["GET /api/users/{id}", "POST /api", "PUT /api/old"]);
        let refs: Vec<&str> = result.references.iter().map(|r| r.reference_name.as_str()).collect();
        assert_eq!(refs, vec!["get", "create", "old"]);
    }

    #[test]
    fn config_files_yield_leaf_keys() {
        let yaml = "app:\n  cache:\n    ttl: 30\n    name: \"x\"\nserver:\n  port: 8080\n";
        let result = SpringResolver.extract("src/main/resources/application.yml", yaml).unwrap();
        let keys: Vec<&str> = result.nodes.iter().map(|n| n.qualified_name.as_str()).collect();
        assert_eq!(keys, vec!["app.cache.ttl", "app.cache.name", "server.port"]);
        let props = "a.b = 1\n# c\nd:2\n";
        let result = SpringResolver.extract("application.properties", props).unwrap();
        let keys: Vec<&str> = result.nodes.iter().map(|n| n.qualified_name.as_str()).collect();
        assert_eq!(keys, vec!["a.b", "d"]);
    }

    #[test]
    fn mapping_paths_parse_arrays_and_constants() {
        let mut consts = HashMap::new();
        consts.insert("BASE".to_string(), "/base".to_string());
        assert_eq!(parse_mapping_paths("{\"/a\", \"/b\"}", &consts), vec!["/a", "/b"]);
        assert_eq!(parse_mapping_paths("value = Api.BASE, produces = \"x\"", &consts), vec!["/base"]);
        assert_eq!(parse_mapping_paths("", &consts), vec![""]);
        assert_eq!(join_path("/api/", "/users"), "/api/users");
    }
}
