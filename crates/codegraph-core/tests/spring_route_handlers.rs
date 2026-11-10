//! Upstream 3d19da2f (851): a Spring route links the method its mapping annotation is written on,
//! including a package-private method and each overload under a tall annotation stack. Ported
//! from `__tests__/spring-route-handlers.test.ts`.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::collections::BTreeMap;
use std::fs;

const POM: &str = "<project><dependencies><dependency><groupId>org.springframework.boot</groupId><artifactId>spring-boot-starter-web</artifactId></dependency></dependencies></project>\n";

const ARTICLES: &str = "package com.example;\n\nimport java.util.List;\nimport org.springframework.http.ResponseEntity;\nimport org.springframework.web.bind.annotation.*;\n\n@RestController\n@RequestMapping(\"/api/articles\")\nclass ArticleController {\n\n    @GetMapping\n    List<String> list() {\n        return List.of();\n    }\n\n    @GetMapping(\"/{slug}\")\n    String get(@PathVariable String slug) {\n        return slug;\n    }\n\n    @PostMapping\n    void create(@RequestBody String body) {\n    }\n\n    @DeleteMapping(\"/{slug}\")\n    public void delete(@PathVariable String slug) {\n    }\n\n    @RequestMapping(value = \"/feed\", method = RequestMethod.GET)\n    ResponseEntity<String> feed() {\n        return null;\n    }\n}\n";

const SEARCH: &str = "package com.example;\n\nimport org.springframework.web.bind.annotation.*;\n\n@RestController\npublic class SearchController {\n    @Operation(summary = \"Find all\")\n    @ApiResponses({\n        @ApiResponse(responseCode = \"200\"),\n        @ApiResponse(responseCode = \"404\")\n    })\n    @GetMapping(\"/find\")\n    public String find() {\n        return \"all\";\n    }\n    @GetMapping(\"/find/{id}\")\n    public String find(@PathVariable int id) {\n        return \"one\";\n    }\n}\n";

const OWNERS: &str = "package com.example\n\nimport org.springframework.web.bind.annotation.*\n\n@RestController\n@RequestMapping(\"/owners\")\nclass OwnerController {\n    @GetMapping(\"/{ownerId}\")\n    @ResponseBody\n    fun showOwner(@PathVariable ownerId: Int): String = \"owner\"\n\n    @GetMapping(\"/find\")\n    fun <T> find(): List<T> = emptyList()\n\n    @PostMapping\n    internal fun create(@RequestBody body: String): String {\n        return body\n    }\n}\n";

fn line_of(source: &str, needle: &str) -> u32 {
    source.lines().position(|line| line.contains(needle)).expect(needle) as u32 + 1
}

fn handlers(cg: &CodeGraph, file: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for route in cg.get_nodes_by_file(file).unwrap().into_iter().filter(|n| n.kind == NodeKind::Route) {
        let targets: Vec<String> = cg
            .get_outgoing_edges(&route.id)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == EdgeKind::References)
            .filter_map(|e| cg.get_node(&e.target).unwrap())
            .map(|t| format!("{}:{}@{}", t.kind.as_str(), t.name, t.start_line))
            .collect();
        out.insert(route.name, if targets.is_empty() { "-".into() } else { targets.join(",") });
    }
    out
}

fn index(files: &[(&str, &str)]) -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    for (rel, source) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

#[test]
fn every_route_of_a_package_private_controller_binds_its_own_method() {
    let articles = "src/main/java/com/example/ArticleController.java";
    let (_dir, cg) = index(&[("pom.xml", POM), (articles, ARTICLES)]);
    let got = handlers(&cg, articles);
    let want = BTreeMap::from([
        ("GET /api/articles".to_string(), format!("method:list@{}", line_of(ARTICLES, "List<String> list()") - 1)),
        ("GET /api/articles/{slug}".to_string(), format!("method:get@{}", line_of(ARTICLES, "@GetMapping(\"/{slug}\")"))),
        ("POST /api/articles".to_string(), format!("method:create@{}", line_of(ARTICLES, "@PostMapping"))),
        ("DELETE /api/articles/{slug}".to_string(), format!("method:delete@{}", line_of(ARTICLES, "@DeleteMapping"))),
        ("GET /api/articles/feed".to_string(), format!("method:feed@{}", line_of(ARTICLES, "@RequestMapping(value = \"/feed\""))),
    ]);
    assert_eq!(got, want);
    cg.close();
}

#[test]
fn each_overload_binds_the_route_written_on_it() {
    let search = "src/main/java/com/example/SearchController.java";
    let (_dir, cg) = index(&[("pom.xml", POM), (search, SEARCH)]);
    let got = handlers(&cg, search);
    let want = BTreeMap::from([
        ("GET /find".to_string(), format!("method:find@{}", line_of(SEARCH, "@Operation"))),
        ("GET /find/{id}".to_string(), format!("method:find@{}", line_of(SEARCH, "@GetMapping(\"/find/{id}\")"))),
    ]);
    assert_eq!(got, want);
    cg.close();
}

#[test]
fn kotlin_handlers_bind_including_generic_and_internal_ones() {
    let owners = "src/main/kotlin/com/example/OwnerController.kt";
    let (_dir, cg) = index(&[("pom.xml", POM), (owners, OWNERS)]);
    let got = handlers(&cg, owners);
    let want = BTreeMap::from([
        ("GET /owners/{ownerId}".to_string(), format!("method:showOwner@{}", line_of(OWNERS, "@GetMapping(\"/{ownerId}\")"))),
        ("GET /owners/find".to_string(), format!("method:find@{}", line_of(OWNERS, "@GetMapping(\"/find\")"))),
        ("POST /owners".to_string(), format!("method:create@{}", line_of(OWNERS, "@PostMapping"))),
    ]);
    assert_eq!(got, want);
    cg.close();
}
