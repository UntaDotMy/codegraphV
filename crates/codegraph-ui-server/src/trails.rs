//! Saved walks use symbol identity, with ids only as a lookup hint.
use crate::api::{Error, Result};
use codegraph_core::CodeGraph;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
const DIRECTORY: &str = ".codegraph/ui/trails";
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
fn directory(cg: &CodeGraph) -> Result<PathBuf> {
    codegraph_core::utils::validate_path_within_root(
        cg.project_root(),
        DIRECTORY,
        Default::default(),
    )
    .map(PathBuf::from)
    .ok_or_else(|| Error::refused("Trail directory leaves the indexed project"))
}
fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}
fn io(error: std::io::Error) -> Error {
    Error {
        status: 500,
        code: "internal",
        message: error.to_string(),
    }
}
fn resolve(cg: &CodeGraph, stored: Value) -> Result<Value> {
    let mut result = stored.clone();
    let Some(saved) = stored["hops"].as_array() else {
        return Err(Error::bad("Stored trail has no hops"));
    };
    let mut hops = vec![];
    for hop in saved {
        let by_id = hop["id"]
            .as_str()
            .and_then(|id| cg.get_node(id).ok().flatten())
            .filter(|n| n.qualified_name == hop["qualifiedName"] && n.kind.as_str() == hop["kind"]);
        let candidates = if by_id.is_none() {
            cg.store()
                .get_nodes_by_qualified_name_exact(
                    hop["qualifiedName"].as_str().unwrap_or_default(),
                )?
                .into_iter()
                .filter(|n| n.kind.as_str() == hop["kind"])
                .collect::<Vec<_>>()
        } else {
            vec![]
        };
        let same = candidates.iter().find(|n| n.file_path == hop["file"]);
        let ambiguous = by_id.is_none() && same.is_none() && candidates.len() > 1;
        let node = by_id.as_ref().or(same).or(candidates.first());
        let status = if node.is_none() {
            "missing"
        } else if ambiguous {
            "ambiguous"
        } else if node.is_some_and(|n| {
            n.file_path != hop["file"] || n.start_line as u64 != hop["line"].as_u64().unwrap_or(0)
        }) {
            "moved"
        } else {
            "ok"
        };
        hops.push(json!({"dir":hop["dir"],"name":hop["name"],"qualifiedName":hop["qualifiedName"],"kind":hop["kind"],"savedFile":hop["file"],"savedLine":hop["line"],"status":status,"id":node.map(|n|&n.id),"file":node.map(|n|&n.file_path),"line":node.map(|n|n.start_line),"note":match status{"missing"=>Some("Symbol moved or was renamed"),"ambiguous"=>Some("Several symbols now have this name; showing the closest match"),"moved"=>Some("Symbol moved since this trail was saved"),_=>None}}));
    }
    let mut best = (0, 0);
    let mut current = 0;
    for i in 0..=hops.len() {
        if i < hops.len() && !hops[i]["id"].is_null() {
            current += 1;
        } else {
            if current > best.1 {
                best = (i - current, current);
            }
            current = 0;
        }
    }
    let run = &hops[best.0..best.0 + best.1];
    let encoded = if run.is_empty() {
        None
    } else {
        Some(
            run.iter()
                .enumerate()
                .map(|(i, h)| {
                    format!(
                        "{}{}",
                        if i == 0 {
                            "s"
                        } else if h["dir"] == "up" {
                            "u"
                        } else {
                            "d"
                        },
                        url::form_urlencoded::byte_serialize(h["id"].as_str().unwrap().as_bytes())
                            .collect::<String>()
                    )
                })
                .collect::<Vec<_>>()
                .join(","),
        )
    };
    result["resolved"] = json!(hops.iter().filter(|h| !h["id"].is_null()).count());
    result["intact"] = json!(hops.iter().all(|h| h["status"] == "ok"));
    result["encoded"] = json!(encoded);
    result["openFrom"] = json!(if best.1 == 0 { 0 } else { best.0 + 1 });
    result["openCount"] = json!(best.1);
    result["openId"] = run.last().map(|h| h["id"].clone()).unwrap_or(Value::Null);
    result["hops"] = json!(hops);
    result.as_object_mut().unwrap().remove("version");
    Ok(result)
}
pub fn list(cg: &CodeGraph, read_only: bool) -> Result<Value> {
    let dir = directory(cg)?;
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => Some(e),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(io(e)),
    };
    let mut trails = vec![];
    let mut skipped = 0;
    let mut bounded = false;
    if let Some(entries) = entries {
        for entry in entries {
            let Ok(entry) = entry else {
                skipped += 1;
                continue;
            };
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if trails.len() >= 200 {
                bounded = true;
                break;
            }
            // The same per-file check as a single read: checking only the directory lets
            // `trails/x.json -> /outside/secret.json` escape it.
            let name = entry.file_name().to_string_lossy().into_owned();
            let inside = codegraph_core::utils::validate_path_within_root(
                cg.project_root(),
                &format!("{DIRECTORY}/{name}"),
                Default::default(),
            );
            if path.is_symlink() || inside.is_none() {
                skipped += 1;
                continue;
            }
            let valid = std::fs::metadata(&path)
                .ok()
                .is_some_and(|m| m.len() <= 65536);
            let parsed = if valid {
                std::fs::read(&path)
                    .ok()
                    .and_then(|s| serde_json::from_slice::<Value>(&s).ok())
            } else {
                None
            };
            // An unversioned hand-written trail predates the format marker and reads as v1; a
            // declared unknown version is not guessed at.
            match parsed.filter(|v| (v["version"].is_null() || v["version"] == 1) && v["id"].as_str().is_some_and(valid_id)) {
                Some(v) => match resolve(cg, v) {
                    Ok(v) => trails.push(v),
                    Err(_) => skipped += 1,
                },
                None => skipped += 1,
            }
        }
    }
    trails.sort_by(|a, b| b["updatedAt"].as_str().cmp(&a["updatedAt"].as_str()));
    Ok(
        json!({"trails":trails,"readOnly":read_only,"readOnlyReason":if read_only{Some("Trail writes are disabled by --read-only")}else{None},"directory":DIRECTORY,"skipped":skipped,"bounded":bounded}),
    )
}
pub fn save(cg: &CodeGraph, body: &Value, read_only: bool) -> Result<Value> {
    if read_only {
        return Err(Error::refused("Trail writes are disabled by --read-only"));
    }
    let name = body["name"]
        .as_str()
        .map(str::trim)
        .filter(|n| !n.is_empty() && n.chars().count() <= 120)
        .ok_or_else(|| Error::bad("Trail name must contain 1–120 characters"))?;
    let note = body["note"].as_str().unwrap_or("");
    if note.chars().count() > 4000 {
        return Err(Error::bad("Trail note is too long"));
    }
    let hops = body["hops"]
        .as_array()
        .filter(|h| !h.is_empty() && h.len() <= 64)
        .ok_or_else(|| Error::bad("A trail must have 1–64 hops"))?;
    let mut saved = vec![];
    for (i, h) in hops.iter().enumerate() {
        let id = h["id"]
            .as_str()
            .ok_or_else(|| Error::bad("Each hop needs a symbol id"))?;
        let n = cg
            .get_node(id)?
            .ok_or_else(|| Error::missing("A trail symbol is no longer in the index"))?;
        let dir = if i == 0 {
            "start"
        } else {
            h["dir"].as_str().unwrap_or("down")
        };
        if !matches!(dir, "start" | "up" | "down") {
            return Err(Error::bad("Hop direction must be start, up, or down"));
        }
        saved.push(json!({"dir":dir,"id":n.id,"name":n.name,"qualifiedName":n.qualified_name,"kind":n.kind,"file":n.file_path,"line":n.start_line}));
    }
    let dir = directory(cg)?;
    std::fs::create_dir_all(&dir).map_err(io)?;
    // The directory need not have existed at the first check: resolve it again now that it does,
    // so a parent link cannot turn a new trail directory into an out-of-project write.
    let dir = directory(cg)?;
    let mut slug = String::new();
    for c in name.to_ascii_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let base = slug.trim_matches('-').chars().take(70).collect::<String>();
    let base = if base.is_empty() { "trail" } else { &base };
    let mut id = base.to_owned();
    let mut ordinal = 2;
    while dir.join(format!("{id}.json")).exists() {
        id = format!("{base}-{ordinal}");
        ordinal += 1;
        if ordinal > 1000 {
            return Err(Error::bad("Too many saved trails with this name"));
        }
    }
    let timestamp = now();
    let trail = json!({"version":1,"id":id,"name":name,"note":note,"author":std::env::var("USERNAME").or_else(|_|std::env::var("USER")).unwrap_or_else(|_|"local".into()),"createdAt":timestamp,"updatedAt":timestamp,"hops":saved});
    let path = dir.join(format!("{id}.json"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(io)?;
    use std::io::Write;
    file.write_all(serde_json::to_string_pretty(&trail).unwrap().as_bytes())
        .map_err(io)?;
    file.sync_all().map_err(io)?;
    let mut result = list(cg, read_only)?;
    result["saved"] = json!(id);
    Ok(result)
}
pub fn remove(cg: &CodeGraph, id: &str, read_only: bool) -> Result<Value> {
    if read_only {
        return Err(Error::refused("Trail writes are disabled by --read-only"));
    }
    if !valid_id(id) {
        return Err(Error::bad("Invalid trail id"));
    }
    let dir = directory(cg)?;
    let path = dir.join(format!("{id}.json"));
    if !path.starts_with(Path::new(&dir)) || path.is_symlink() {
        return Err(Error::refused("Invalid trail path"));
    }
    std::fs::remove_file(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::missing("No saved trail with that id")
        } else {
            io(e)
        }
    })?;
    list(cg, read_only)
}
