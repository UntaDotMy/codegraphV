use serde_json::{json, Value};
use std::{cell::RefCell, collections::HashMap};
use wasm_bindgen::{prelude::*, JsCast};
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{Element, Event, HtmlInputElement, Request, RequestInit, Response};
type Result<T> = std::result::Result<T, JsValue>;
thread_local! {static STATE:RefCell<State>=RefCell::new(State::default());}
#[derive(Default)]
struct State {
    epoch: u64,
    search_epoch: u64,
    trail: Vec<String>,
}
fn window() -> web_sys::Window {
    web_sys::window().unwrap()
}
fn document() -> web_sys::Document {
    window().document().unwrap()
}
fn by_id(id: &str) -> Element {
    document()
        .get_element_by_id(id)
        .expect("viewer shell element")
}
fn element(tag: &str, class: &str, text: &str) -> Element {
    let node = document().create_element(tag).unwrap();
    if !class.is_empty() {
        node.set_class_name(class);
    }
    if !text.is_empty() {
        node.set_text_content(Some(text));
    }
    node
}
fn add(parent: &Element, tag: &str, class: &str, text: &str) -> Element {
    let node = element(tag, class, text);
    parent.append_child(&node).unwrap();
    node
}
fn attr(el: &Element, key: &str, value: &str) {
    el.set_attribute(key, value).unwrap();
}
fn string(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}
fn count(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}
fn items<'a>(v: &'a Value) -> &'a [Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn link(parent: &Element, label: &str, route: &str) -> Element {
    let link = add(parent, "a", "", label);
    attr(&link, "href", route);
    link
}
fn symbol_route(id: &str) -> String {
    format!("#symbol?id={}", crate::encode(id))
}
fn file_route(file: &str) -> String {
    format!("#file?path={}", crate::encode(file))
}
fn node_link(parent: &Element, node: &Value) -> Element {
    link(
        parent,
        string(&node["name"]),
        &symbol_route(string(&node["id"])),
    )
}
fn row(parent: &Element, node: &Value) {
    let row = add(parent, "div", "row", "");
    add(&row, "span", "kind", string(&node["kind"]));
    node_link(&row, node);
    add(
        &row,
        "span",
        "muted",
        &format!("{}:{}", string(&node["file"]), count(&node["line"])),
    );
}
fn notice(parent: &Element, text: &str, error: bool) {
    add(
        parent,
        "div",
        if error { "notice error" } else { "notice" },
        text,
    );
}
fn navigate(route: &str) {
    let _ = window().location().set_hash(route.trim_start_matches('#'));
}
fn error_text(error: &JsValue) -> String {
    error.as_string().unwrap_or_else(|| format!("{error:?}"))
}
async fn request(path: &str, method: &str, body: Option<Value>) -> Result<Value> {
    let options = RequestInit::new();
    options.set_method(method);
    if let Some(body) = body {
        options.set_body(&JsValue::from_str(&body.to_string()));
    }
    let request = Request::new_with_str_and_init(path, &options)?;
    if method != "GET" {
        request.headers().set("x-codegraph-ui", "1")?;
        request.headers().set("Content-Type", "application/json")?;
    }
    let response: Response = JsFuture::from(window().fetch_with_request(&request))
        .await?
        .dyn_into()?;
    let text = JsFuture::from(response.text()?)
        .await?
        .as_string()
        .unwrap_or_default();
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| JsValue::from_str(&format!("Invalid API response: {e}")))?;
    if !response.ok() {
        return Err(JsValue::from_str(
            value["error"]
                .as_str()
                .unwrap_or("The graph could not answer this request"),
        ));
    }
    Ok(value)
}
async fn get(path: &str) -> Result<Value> {
    request(path, "GET", None).await
}
fn params(hash: &str) -> (&str, HashMap<String, String>) {
    let hash = hash.trim_start_matches('#');
    let (view, query) = hash.split_once('?').unwrap_or((hash, ""));
    let mut params = HashMap::new();
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            params.insert(key.to_owned(), decode(value));
        }
    }
    (if view.is_empty() { "home" } else { view }, params)
}
fn decode(s: &str) -> String {
    let mut out = vec![];
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(hex) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(hex);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
#[wasm_bindgen(start)]
pub fn start() -> Result<()> {
    let app = by_id("app");
    app.set_text_content(None);
    let header = add(&app, "header", "", "");
    link(&header, "CodeGraphV", "#home").set_class_name("brand");
    let project = add(&header, "span", "project", "");
    attr(&project, "id", "project-name");
    let search_input = add(&header, "input", "search", "");
    attr(&search_input, "id", "search");
    attr(&search_input, "placeholder", "Find a symbol or file…");
    attr(&search_input, "aria-label", "Search indexed symbols");
    let palette = add(&header, "div", "palette card", "");
    attr(&palette, "id", "palette");
    let shell = add(&app, "div", "shell", "");
    let nav = add(&shell, "nav", "", "");
    for (name, route) in [
        ("Overview", "#home"),
        ("Module map", "#map"),
        ("Routes", "#routes"),
        ("Flow", "#flow"),
        ("Dead code", "#deadcode"),
        ("Saved trails", "#trails"),
    ] {
        link(&nav, name, route);
    }
    let main = add(&shell, "main", "", "");
    attr(&main, "id", "main");
    let click = Closure::<dyn FnMut(Event)>::new(move |event: Event| {
        let Some(target) = event.target().and_then(|v| v.dyn_into::<Element>().ok()) else {
            return;
        };
        if let Ok(Some(link)) = target.closest("a[href^='#']") {
            event.prevent_default();
            by_id("palette").set_text_content(None);
            if let Some(href) = link.get_attribute("href") {
                navigate(&href);
            }
            return;
        }
        if let Ok(Some(button)) = target.closest("button[data-action]") {
            let action = button.get_attribute("data-action").unwrap_or_default();
            match action.as_str() {
                "flow" => {
                    let value = by_id("flow-query")
                        .dyn_into::<HtmlInputElement>()
                        .unwrap()
                        .value();
                    navigate(&format!("#flow?symbols={}", crate::encode(&value)));
                }
                "save-trail" => save_trail(),
                "delete-trail" => {
                    let id = button.get_attribute("data-id").unwrap_or_default();
                    if window()
                        .confirm_with_message("Delete this saved trail?")
                        .unwrap_or(false)
                    {
                        spawn_local(async move {
                            match request(
                                &format!("/api/trails/{}", crate::encode(&id)),
                                "DELETE",
                                None,
                            )
                            .await
                            {
                                Ok(_) => render(),
                                Err(e) => notice(&by_id("main"), &error_text(&e), true),
                            }
                        });
                    }
                }
                "refresh" => render(),
                _ => {}
            }
        }
    });
    document().add_event_listener_with_callback("click", click.as_ref().unchecked_ref())?;
    click.forget();
    let input = Closure::<dyn FnMut(Event)>::new(move |event: Event| {
        if event
            .target()
            .and_then(|v| v.dyn_into::<Element>().ok())
            .is_some_and(|e| e.id() == "search")
        {
            let query = by_id("search")
                .dyn_into::<HtmlInputElement>()
                .unwrap()
                .value();
            search(query);
        }
    });
    document().add_event_listener_with_callback("input", input.as_ref().unchecked_ref())?;
    input.forget();
    let key =
        Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |event: web_sys::KeyboardEvent| {
            if event.key() == "Escape" {
                by_id("palette").set_text_content(None);
            }
            if event.key() == "Enter" {
                if let Some(input) = event
                    .target()
                    .and_then(|v| v.dyn_into::<HtmlInputElement>().ok())
                {
                    if input.id() == "search" {
                        by_id("palette").set_text_content(None);
                        navigate(&format!("#search?q={}", crate::encode(&input.value())));
                    } else if input.id() == "flow-query" {
                        navigate(&format!("#flow?symbols={}", crate::encode(&input.value())));
                    }
                }
            }
        });
    document().add_event_listener_with_callback("keydown", key.as_ref().unchecked_ref())?;
    key.forget();
    let hash = Closure::<dyn FnMut(Event)>::new(move |_| render());
    window().add_event_listener_with_callback("hashchange", hash.as_ref().unchecked_ref())?;
    hash.forget();
    spawn_local(async {
        match get("/api/stats").await {
            Ok(v) => {
                by_id("project-name").set_text_content(Some(string(&v["project"]["name"])));
            }
            Err(e) => notice(&by_id("main"), &error_text(&e), true),
        }
    });
    render();
    Ok(())
}
fn search(query: String) {
    let epoch = STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.search_epoch += 1;
        s.search_epoch
    });
    if query.trim().is_empty() {
        by_id("palette").set_text_content(None);
        return;
    }
    spawn_local(async move {
        match get(&format!("/api/search?q={}&limit=20", crate::encode(&query))).await {
            Ok(v) => {
                if !STATE.with(|s| s.borrow().search_epoch == epoch) {
                    return;
                }
                let palette = by_id("palette");
                palette.set_text_content(None);
                for n in items(&v["results"]["items"]) {
                    row(&palette, n);
                }
                if items(&v["results"]["items"]).is_empty() {
                    add(&palette, "p", "empty", "No indexed symbols match.");
                }
            }
            Err(e) => {
                by_id("palette").set_text_content(Some(&error_text(&e)));
            }
        }
    });
}
fn render() {
    let epoch = STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.epoch += 1;
        s.epoch
    });
    let main = by_id("main");
    main.set_text_content(None);
    add(&main, "p", "loading", "Reading the graph…");
    let hash = window().location().hash().unwrap_or_default();
    spawn_local(async move {
        if let Err(e) = render_route(&hash, epoch).await {
            if STATE.with(|s| s.borrow().epoch == epoch) {
                let main = by_id("main");
                main.set_text_content(None);
                add(&main, "h1", "", "Could not open this view");
                notice(&main, &error_text(&e), true);
                link(&main, "Back to overview", "#home");
            }
        }
    });
}
fn current(epoch: u64) -> Option<Element> {
    if !STATE.with(|s| s.borrow().epoch == epoch) {
        return None;
    }
    let main = by_id("main");
    main.set_text_content(None);
    Some(main)
}
async fn render_route(hash: &str, epoch: u64) -> Result<()> {
    let (view, params) = params(hash);
    let param = |key: &str| params.get(key).map(String::as_str).unwrap_or("");
    match view {
        "home" => {
            let stats = get("/api/stats").await?;
            let entry = get("/api/entrypoints").await?;
            let Some(main) = current(epoch) else {
                return Ok(());
            };
            add(&main, "h1", "", string(&stats["project"]["name"]));
            add(
                &main,
                "p",
                "subtitle",
                "Explore the code, follow its connections, and see what a change reaches.",
            );
            if stats["index"]["state"] != "complete" {
                notice(
                    &main,
                    &format!(
                        "Index state: {}. Some code may be missing from the graph.",
                        string(&stats["index"]["state"])
                    ),
                    false,
                );
            }
            if stats["index"]["stale"] == true {
                notice(
                    &main,
                    "This index was built with an older extractor. Re-index for current coverage.",
                    false,
                );
            }
            let grid = add(&main, "div", "grid", "");
            for (label, key) in [
                ("Indexed files", "files"),
                ("Symbols", "nodes"),
                ("Connections", "edges"),
            ] {
                let card = add(&grid, "div", "card", "");
                add(
                    &card,
                    "p",
                    "metric",
                    &count(&stats["graph"][key]).to_string(),
                );
                add(&card, "span", "muted", label);
            }
            let grid = add(&main, "div", "grid", "");
            for (title, key) in [
                ("Start reading", "files"),
                ("Most connected", "hubs"),
                ("Tests", "tests"),
            ] {
                let card = add(&grid, "section", "card", "");
                add(&card, "h2", "", title);
                for node in items(&entry[key]["items"]).iter().take(16) {
                    row(&card, node);
                }
                if items(&entry[key]["items"]).is_empty() {
                    add(
                        &card,
                        "p",
                        "empty",
                        "No entry points recorded in this group.",
                    );
                }
            }
        }
        "search" => {
            let value = get(&format!(
                "/api/search?q={}&limit=100",
                crate::encode(param("q"))
            ))
            .await?;
            let Some(main) = current(epoch) else {
                return Ok(());
            };
            add(&main, "h1", "", &format!("Search: {}", param("q")));
            for n in items(&value["results"]["items"]) {
                row(&main, n);
            }
        }
        "symbol" => {
            let id = param("id");
            let value = get(&format!("/api/node/{}", crate::encode(id))).await?;
            let n = &value["node"];
            let source = get(&format!(
                "/api/source?file={}&from={}&to={}",
                crate::encode(string(&n["file"])),
                count(&n["line"]).max(1),
                count(&n["endLine"]).max(1)
            ))
            .await?;
            let Some(main) = current(epoch) else {
                return Ok(());
            };
            STATE.with(|s| {
                let mut s = s.borrow_mut();
                if s.trail.last().map(String::as_str) != Some(id) {
                    s.trail.push(id.to_owned());
                    if s.trail.len() > 64 {
                        s.trail.remove(0);
                    }
                }
            });
            add(&main, "span", "kind", string(&n["kind"]));
            add(&main, "h1", "", string(&n["name"]));
            let bar = add(&main, "div", "toolbar", "");
            link(
                &bar,
                &format!("{}:{}", string(&n["file"]), count(&n["line"])),
                &file_route(string(&n["file"])),
            );
            let save = add(&bar, "button", "", "Save this walk");
            attr(&save, "data-action", "save-trail");
            if value["drift"] == true {
                notice(
                    &main,
                    "This file changed after indexing. Source positions may have moved.",
                    false,
                );
            }
            let columns = add(&main, "div", "columns", "");
            let left = add(&columns, "section", "rail card", "");
            relation_rail(&left, "Called or used by", &value["incoming"]);
            let center = add(&columns, "section", "", "");
            if let Some(sig) = n["signature"].as_str() {
                add(&center, "p", "subtitle", sig);
            }
            code(&center, &source);
            if !items(&value["members"]["items"]).is_empty() {
                add(&center, "h2", "", "Members");
                for member in items(&value["members"]["items"]) {
                    row(&center, member);
                }
            }
            let right = add(&columns, "section", "rail card", "");
            relation_rail(&right, "Calls and uses", &value["outgoing"]);
            add(&main, "h2", "", "Change impact");
            add(
                &main,
                "p",
                "subtitle",
                &format!(
                    "{} symbols across {} files within {} hops.",
                    count(&value["blast"]["withinHops"]),
                    count(&value["blast"]["files"]),
                    count(&value["blast"]["hops"])
                ),
            );
            for item in items(&value["blast"]["topFiles"]) {
                let row = add(&main, "div", "row", "");
                link(
                    &row,
                    string(&item["file"]),
                    &file_route(string(&item["file"])),
                );
                add(
                    &row,
                    "span",
                    "muted",
                    &format!("{} symbols", count(&item["symbols"])),
                );
            }
            if !value["hierarchy"].is_null() {
                add(&main, "h2", "", "Type hierarchy");
                for key in ["ancestors", "descendants"] {
                    for node in items(&value["hierarchy"][key]["items"]) {
                        row(&main, node);
                    }
                }
            }
        }
        "file" | "source" => {
            let file = if view == "file" {
                param("path")
            } else {
                param("file")
            };
            let value = get(&format!("/api/file/{}", crate::encode(file))).await?;
            let source = get(&format!(
                "/api/source?file={}&from={}&ondrift={}",
                crate::encode(file),
                param("from").parse::<usize>().unwrap_or(1),
                if param("current") == "1" {
                    "current"
                } else {
                    "indexed"
                }
            ))
            .await?;
            let Some(main) = current(epoch) else {
                return Ok(());
            };
            add(&main, "span", "kind", string(&value["file"]["language"]));
            add(&main, "h1", "", file);
            let grid = add(&main, "div", "columns", "");
            let outline = add(&grid, "section", "rail card", "");
            add(&outline, "h2", "", "Symbols");
            for node in items(&value["outline"]["items"]) {
                row(&outline, node);
            }
            let content = add(&grid, "section", "", "");
            code(&content, &source);
            let rail = add(&grid, "section", "rail card", "");
            for (title, key) in [("Depends on", "dependencies"), ("Used by", "dependents")] {
                add(&rail, "h2", "", title);
                for f in items(&value[key]) {
                    let r = add(&rail, "div", "row", "");
                    link(&r, string(f), &file_route(string(f)));
                }
            }
        }
        "flow" => {
            let query = param("symbols");
            let data = if query.is_empty() {
                None
            } else {
                Some(get(&format!("/api/flow?symbols={}", crate::encode(query))).await?)
            };
            let Some(main) = current(epoch) else {
                return Ok(());
            };
            add(&main, "h1", "", "Follow a flow");
            add(&main,"p","subtitle","Name the symbols you want to connect. Each hop follows a recorded edge in this index.");
            let toolbar = add(&main, "div", "toolbar", "");
            let input = add(&toolbar, "input", "", "");
            attr(&input, "id", "flow-query");
            attr(&input, "placeholder", "start finish");
            attr(&input, "value", query);
            let button = add(&toolbar, "button", "", "Find flow");
            attr(&button, "data-action", "flow");
            if let Some(data) = data {
                if let Some(reason) = data["reason"].as_str() {
                    notice(&main, reason, false);
                }
                for flow in items(&data["flows"]) {
                    add(&main, "h2", "", string(&flow["label"]));
                    let strip = add(&main, "div", "flow", "");
                    for hop in items(&flow["hops"]) {
                        let card = add(&strip, "section", "card", "");
                        if !hop["edge"].is_null() {
                            add(
                                &card,
                                "p",
                                "flow-label",
                                &format!("→ {}", string(&hop["edge"]["label"])),
                            );
                        }
                        node_link(&card, &hop["node"]);
                        add(
                            &card,
                            "p",
                            "subtitle",
                            &format!(
                                "{}:{}",
                                string(&hop["node"]["file"]),
                                count(&hop["node"]["line"])
                            ),
                        );
                        if !hop["source"].is_null() {
                            code(&card, &hop["source"]);
                        }
                    }
                }
            }
        }
        "routes" => {
            let data = get("/api/routes").await?;
            let Some(main) = current(epoch) else {
                return Ok(());
            };
            add(&main, "h1", "", "Routes");
            add(
                &main,
                "p",
                "subtitle",
                &format!(
                    "{} registered routes, {} resolved handlers.",
                    count(&data["routeCount"]),
                    count(&data["shown"])
                ),
            );
            if data["routed"] == false {
                add(
                    &main,
                    "p",
                    "empty",
                    "No application routes are recorded in this index.",
                );
            }
            for route in items(&data["entries"]) {
                let row = add(&main, "div", "row", "");
                add(&row, "span", "kind", string(&route["method"]));
                link(
                    &row,
                    string(&route["path"]),
                    &symbol_route(
                        route["handlerId"]
                            .as_str()
                            .unwrap_or(string(&route["routeId"])),
                    ),
                );
                add(
                    &row,
                    "span",
                    "muted",
                    &format!(
                        "{} — {}:{}",
                        string(&route["handler"]),
                        string(&route["file"]),
                        count(&route["line"])
                    ),
                );
            }
        }
        "map" | "module" => {
            let data = get("/api/map").await?;
            let Some(main) = current(epoch) else {
                return Ok(());
            };
            add(&main, "h1", "", "Module map");
            add(
                &main,
                "p",
                "subtitle",
                "Direct connections between directories. Select a module to open its files.",
            );
            module_map(&main, &data, param("id"));
        }
        "deadcode" => {
            let data = get("/api/deadcode").await?;
            let Some(main) = current(epoch) else {
                return Ok(());
            };
            add(&main, "h1", "", "Unreferenced code");
            add(&main,"p","subtitle","Candidates with no recorded incoming reference, after excluding framework hooks, exports, tests, and other uncertain cases.");
            add(
                &main,
                "p",
                "muted",
                &format!(
                    "{} findings · {} candidates checked · {} excluded",
                    count(&data["rows"]["total"]),
                    count(&data["candidates"]),
                    count(&data["excludedTotal"])
                ),
            );
            if data["corroborated"] == false {
                notice(
                    &main,
                    "Some source could not be checked. Review each finding before removing code.",
                    false,
                );
            }
            for group in items(&data["groups"]) {
                add(&main, "h2", "", string(&group["file"]));
                for node in items(&group["rows"]) {
                    row(&main, node);
                }
            }
            if items(&data["groups"]).is_empty() {
                add(
                    &main,
                    "p",
                    "empty",
                    "No unreferenced candidates survived the exclusions.",
                );
            }
        }
        "trails" => {
            let data = get("/api/trails").await?;
            let Some(main) = current(epoch) else {
                return Ok(());
            };
            add(&main, "h1", "", "Saved trails");
            add(
                &main,
                "p",
                "subtitle",
                "Walks through the code, saved by symbol identity so they can survive a re-index.",
            );
            if let Some(reason) = data["readOnlyReason"].as_str() {
                notice(&main, reason, false);
            }
            for trail in items(&data["trails"]) {
                let card = add(&main, "section", "card", "");
                add(&card, "h2", "", string(&trail["name"]));
                add(&card, "p", "subtitle", string(&trail["note"]));
                for hop in items(&trail["hops"]) {
                    let r = add(&card, "div", "row", "");
                    if hop["id"].is_null() {
                        add(&r, "span", "", string(&hop["name"]));
                    } else {
                        node_link(&r, hop);
                    }
                    add(&r, "span", "muted", string(&hop["status"]));
                }
                if data["readOnly"] == false {
                    let button = add(&card, "button", "", "Delete trail");
                    attr(&button, "data-action", "delete-trail");
                    attr(&button, "data-id", string(&trail["id"]));
                }
            }
            if items(&data["trails"]).is_empty() {
                add(
                    &main,
                    "p",
                    "empty",
                    "Open a symbol, follow its callers or callees, then choose “Save this walk”.",
                );
            }
        }
        _ => return Err(JsValue::from_str("This view is not available")),
    }
    Ok(())
}
fn relation_rail(parent: &Element, title: &str, list: &Value) {
    add(
        parent,
        "h2",
        "",
        &format!("{title} · {}", count(&list["total"])),
    );
    for rel in items(&list["items"]) {
        let r = add(parent, "div", "row", "");
        node_link(&r, &rel["node"]);
        add(
            &r,
            "span",
            "muted",
            &format!(
                "{}:{}",
                string(&rel["node"]["file"]),
                count(&rel["node"]["line"])
            ),
        );
        if rel["uncertain"] == true {
            add(&r, "span", "kind", "Uncertain match");
        }
        if rel["synthesized"] == true {
            add(&r, "span", "kind", "Inferred connection");
        }
    }
    if items(&list["items"]).is_empty() {
        add(parent, "p", "empty", "No recorded connections.");
    }
}
fn code(parent: &Element, source: &Value) {
    if let Some(reason) = source["reason"].as_str() {
        notice(parent, reason, false);
    }
    if source["drift"] == true && source["lines"].is_null() {
        link(
            parent,
            "View current file",
            &format!(
                "#source?file={}&current=1",
                crate::encode(string(&source["file"]))
            ),
        );
        return;
    }
    let Some(lines) = source["lines"].as_array() else {
        return;
    };
    let from = count(&source["from"]).max(1);
    let pre = add(parent, "pre", "code", "");
    for (i, line) in lines.iter().enumerate() {
        let row = add(&pre, "span", "code-line", "");
        add(&row, "span", "line-number", &(from + i as u64).to_string());
        let text = document().create_text_node(string(line));
        row.append_child(&text).unwrap();
    }
    if source["truncated"] == true {
        link(
            parent,
            "Continue source →",
            &format!(
                "#source?file={}&from={}",
                crate::encode(string(&source["file"])),
                count(&source["to"]) + 1
            ),
        );
    }
}
fn save_trail() {
    let hops = STATE.with(|s| {
        s.borrow()
            .trail
            .iter()
            .enumerate()
            .map(|(i, id)| json!({"id":id,"dir":if i==0{"start"}else{"down"}}))
            .collect::<Vec<_>>()
    });
    if hops.is_empty() {
        return;
    }
    let Some(name) = window()
        .prompt_with_message("Name this code walk")
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
    else {
        return;
    };
    spawn_local(async move {
        match request(
            "/api/trails",
            "POST",
            Some(json!({"name":name,"hops":hops})),
        )
        .await
        {
            Ok(_) => navigate("#trails"),
            Err(e) => notice(&by_id("main"), &error_text(&e), true),
        }
    });
}
fn module_map(main: &Element, data: &Value, selected: &str) {
    let grid = add(main, "div", "grid", "");
    for module in items(&data["modules"]) {
        let card = add(&grid, "section", "card module", "");
        let title = add(&card, "h3", "", "");
        link(
            &title,
            string(&module["label"]),
            &format!("#module?id={}", crate::encode(string(&module["id"]))),
        );
        add(
            &card,
            "p",
            "subtitle",
            &format!(
                "{} files · {} symbols · {} dependent modules",
                count(&module["files"]),
                count(&module["symbols"]),
                count(&module["dependents"]["modules"])
            ),
        );
        if selected == string(&module["id"]) {
            for file in items(&module["fileList"]["items"]) {
                let row = add(&card, "div", "row", "");
                link(&row, string(file), &file_route(string(file)));
            }
        }
    }
    add(main, "h2", "", "Connections");
    for connection in items(&data["links"]) {
        let row = add(main, "div", "row", "");
        link(
            &row,
            string(&connection["source"]),
            &format!(
                "#module?id={}",
                crate::encode(string(&connection["source"]))
            ),
        );
        add(&row, "span", "muted", "→");
        link(
            &row,
            string(&connection["target"]),
            &format!(
                "#module?id={}",
                crate::encode(string(&connection["target"]))
            ),
        );
        add(
            &row,
            "span",
            "muted",
            &format!("{} connections", count(&connection["count"])),
        );
    }
}
