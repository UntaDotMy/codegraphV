use crate::{
    api::{self, Error, Query},
    trails,
};
use codegraph_core::{CodeGraph, OpenOptions};
use serde_json::{json, Value};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tiny_http::{Header, Request, Response, Server, StatusCode};
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
#[derive(Clone)]
pub struct Options {
    pub root: String,
    pub port: u16,
    pub port_fallback: bool,
    pub viewer: PathBuf,
    pub read_only: bool,
}
pub fn viewer_directory() -> Result<PathBuf> {
    let mut paths = vec![];
    if let Some(path) = std::env::var_os("CODEGRAPHV_VIEWER_PATH") {
        paths.push(PathBuf::from(path));
    }
    let exe = std::env::current_exe()?;
    if let Some(dir) = exe.parent() {
        paths.push(dir.join("viewer"));
    }
    paths.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dist/viewer"));
    paths
        .into_iter()
        .find(|p| p.join("index.html").is_file())
        .ok_or_else(|| {
            "Viewer assets are missing; build the viewer or set CODEGRAPHV_VIEWER_PATH.".into()
        })
}
fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).unwrap()
}
fn host_allowed(host: &str, port: u16) -> bool {
    let Ok(url) = url::Url::parse(&format!("http://{host}")) else {
        return false;
    };
    matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    ) && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.port().is_none_or(|p| p == port)
}
fn origin_allowed(origin: &str, port: u16) -> bool {
    let Ok(url) = url::Url::parse(origin) else {
        return false;
    };
    url.scheme() == "http"
        && matches!(
            url.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
        )
        && url.port_or_known_default() == Some(port)
        && url.username().is_empty()
        && url.password().is_none()
}
pub fn serve(options: Options, stop: &AtomicBool, on_ready: impl FnOnce(u16)) -> Result<()> {
    let mut bound = None;
    let attempts = if options.port_fallback { 20 } else { 1 };
    for offset in 0..attempts {
        let port = options.port.saturating_add(offset);
        match Server::http(("127.0.0.1", port)) {
            Ok(server) => {
                bound = Some(server);
                break;
            }
            Err(e) if offset + 1 == attempts => return Err(e.to_string().into()),
            Err(_) => {}
        }
    }
    let server = bound.unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let mut cg = CodeGraph::open(
        &options.root,
        OpenOptions {
            read_only: true,
            sync: false,
        },
    )?;
    on_ready(port);
    while !stop.load(Ordering::Relaxed) {
        let Some(mut req) = server.recv_timeout(Duration::from_millis(100))? else {
            continue;
        };
        cg.reopen_if_replaced();
        let status = handle(&mut req, &cg, &options, port);
        match status {
            Ok((code, mime, body)) => {
                let head = req.method().as_str() == "HEAD";
                let response = Response::from_data(if head { vec![] } else { body })
                    .with_status_code(StatusCode(code))
                    .with_header(header("Content-Type", &mime))
                    .with_header(header("X-Content-Type-Options", "nosniff"))
                    .with_header(header("Referrer-Policy", "no-referrer"))
                    .with_header(header("Cache-Control", "no-store"));
                let _ = req.respond(response);
            }
            Err(error) => {
                let _ = req.respond(
                    Response::from_string(error.body().to_string())
                        .with_status_code(StatusCode(error.status))
                        .with_header(header("Content-Type", "application/json; charset=utf-8")),
                );
            }
        }
    }
    cg.close();
    Ok(())
}
fn handle(
    req: &mut Request,
    cg: &CodeGraph,
    options: &Options,
    port: u16,
) -> std::result::Result<(u16, String, Vec<u8>), Error> {
    let host = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Host"))
        .map(|h| h.value.as_str())
        .unwrap_or("");
    if !host_allowed(host, port) {
        return Err(Error::refused("Host must name this loopback viewer"));
    }
    if let Some(origin) = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Origin"))
        .map(|h| h.value.as_str())
    {
        if !origin_allowed(origin, port) {
            return Err(Error::refused("Origin must name this loopback viewer"));
        }
    }
    if !req.url().starts_with('/') || req.url().starts_with("//") {
        return Err(Error::bad("Invalid request target"));
    }
    let path = decode_path(req.url().split('?').next().unwrap())?;
    let url = url::Url::parse(&format!("http://127.0.0.1:{}{}", port, req.url()))
        .map_err(|_| Error::bad("Invalid URL"))?;
    let query: Query = url
        .query_pairs()
        .map(|(a, b)| (a.into_owned(), b.into_owned()))
        .collect();
    let method = req.method().as_str();
    if !matches!(method, "GET" | "HEAD" | "POST" | "DELETE") {
        return Err(Error {
            status: 405,
            code: "refused",
            message: "Method is not allowed".into(),
        });
    }
    let write = matches!(method, "POST" | "DELETE");
    if write {
        if !(path == "/api/trails" && method == "POST"
            || path.starts_with("/api/trails/") && method == "DELETE")
        {
            return Err(Error::refused("This API only writes saved trails"));
        }
        if !req
            .headers()
            .iter()
            .any(|h| h.field.equiv("x-codegraph-ui"))
        {
            return Err(Error::refused(
                "A write must carry the x-codegraph-ui header",
            ));
        }
        if options.read_only {
            return Err(Error::refused("Trail writes are disabled by --read-only"));
        }
    }
    if path.starts_with("/api") {
        let value = if path == "/api/trails" && method == "POST" {
            let content_type = req
                .headers()
                .iter()
                .find(|h| h.field.equiv("Content-Type"))
                .map(|h| h.value.as_str())
                .unwrap_or("");
            if !content_type.starts_with("application/json") {
                return Err(Error::bad("Trail writes require application/json"));
            }
            let mut body = vec![];
            req.as_reader()
                .take(65537)
                .read_to_end(&mut body)
                .map_err(|_| Error::bad("Could not read request body"))?;
            if body.len() > 65536 {
                return Err(Error::bad("Trail request is too large"));
            }
            let body: Value = serde_json::from_slice(&body)
                .map_err(|_| Error::bad("Request body must be JSON"))?;
            trails::save(cg, &body, options.read_only)?
        } else if let Some(id) = path
            .strip_prefix("/api/trails/")
            .filter(|_| method == "DELETE")
        {
            trails::remove(cg, id, options.read_only)?
        } else {
            get(cg, &path, &query, options.read_only)?
        };
        return Ok((
            200,
            "application/json; charset=utf-8".into(),
            value.to_string().into_bytes(),
        ));
    }
    let relative = path.trim_start_matches('/');
    let requested = if relative.is_empty() {
        "index.html"
    } else {
        relative
    };
    let root = std::fs::canonicalize(&options.viewer)
        .map_err(|_| Error::missing("Viewer assets are missing"))?;
    let candidate = root.join(requested);
    let asset = std::fs::canonicalize(&candidate)
        .ok()
        .filter(|p| p.starts_with(&root) && p.is_file());
    let asset = asset
        .or_else(|| {
            if !Path::new(requested).extension().is_some() {
                Some(root.join("index.html"))
            } else {
                None
            }
        })
        .ok_or_else(|| Error::missing("Asset not found"))?;
    let mime = mime_guess::from_path(&asset)
        .first_or_octet_stream()
        .to_string();
    let body = std::fs::read(asset).map_err(|_| Error::missing("Asset not found"))?;
    Ok((200, mime, body))
}
fn decode_path(path: &str) -> std::result::Result<String, Error> {
    let mut bytes = vec![];
    let raw = path.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' {
            if i + 2 >= raw.len() {
                return Err(Error::bad("Invalid path encoding"));
            }
            let value = u8::from_str_radix(&path[i + 1..i + 3], 16)
                .map_err(|_| Error::bad("Invalid path encoding"))?;
            bytes.push(value);
            i += 3;
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    let path = String::from_utf8(bytes).map_err(|_| Error::bad("Invalid UTF-8 in path"))?;
    if path.contains(['\0', '\\']) || path.split('/').any(|s| matches!(s, ".." | ".")) {
        return Err(Error::refused("Unsafe request path"));
    }
    Ok(path)
}
pub fn get(cg: &CodeGraph, path: &str, query: &Query, read_only: bool) -> api::Result<Value> {
    match path {
        "/api" | "/api/" => Ok(
            json!({"name":"codegraphV ui","readOnly":read_only,"endpoints":["stats","search","node/<id>","nodes","source","file/<path>","trails"]}),
        ),
        "/api/stats" => api::stats(cg),
        "/api/search" => api::search(cg, query),
        "/api/source" => api::source(cg, query),
        "/api/trails" => trails::list(cg, read_only),
        "/api/routes" => crate::views::routes(cg, query),
        "/api/entrypoints" => crate::views::entrypoints(cg),
        "/api/flow" => crate::views::flow(cg, query),
        "/api/map" => crate::views::map(cg, query),
        "/api/deadcode" => crate::views::deadcode(cg, query),
        "/api/nodes" => {
            let mut items = vec![];
            let mut missing = vec![];
            for (_, id) in query.iter().filter(|(k, _)| k == "id").take(200) {
                if let Some(n) = cg.get_node(id)? {
                    items.push(api::node_ref(&n));
                } else {
                    missing.push(id);
                }
            }
            Ok(json!({"items":items,"missing":missing}))
        }
        path if path.starts_with("/api/node/") => {
            api::node(cg, path.trim_start_matches("/api/node/"))
        }
        path if path.starts_with("/api/file/") => {
            api::file(cg, path.trim_start_matches("/api/file/"))
        }
        path if path.starts_with("/api/filecode/") => {
            crate::views::filecode(cg, path.trim_start_matches("/api/filecode/"))
        }
        _ => Err(Error::missing("API endpoint not found")),
    }
}
