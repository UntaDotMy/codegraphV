//! One description of a dynamic hop for explore, node trails and call paths.

use codegraph_types::{Edge, Provenance};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SynthesizedHop {
    pub summary: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registered_at: Option<String>,
}

pub fn describe_synthesized_hop(edge: Option<&Edge>) -> Option<SynthesizedHop> {
    let edge = edge?;
    if edge.provenance != Some(Provenance::Heuristic) {
        return None;
    }
    let m = edge.metadata.as_ref()?;
    let by = m.get("synthesizedBy")?.as_str()?;
    let get = |k: &str| m.get(k).unwrap_or(&Value::Null);
    let string = |k: &str| js_string(get(k));
    let quoted = |k: &str, otherwise: &str| {
        if truthy(get(k)) {
            format!("`{}`", string(k))
        } else {
            otherwise.to_owned()
        }
    };
    let mut detail = String::new();
    let summary = match by {
        "callback" => format!("callback via {}", quoted("via", "a registrar")),
        "http-client" => {
            let method = m
                .get("method")
                .filter(|v| !v.is_null())
                .map(js_string)
                .unwrap_or_else(|| "GET".into());
            let href = m
                .get("href")
                .filter(|v| !v.is_null())
                .map(js_string)
                .unwrap_or_default();
            detail = " — the client's call onto its own route".into();
            format!(
                "HTTP {}",
                codegraph_types::jscompat::js_trim(&format!("{method} {href}"))
            )
        }
        "queue-job" => {
            if truthy(get("queue")) {
                detail = format!(" on `{}`", string("queue"));
            }
            format!("queue job {}", quoted("event", "a job"))
        }
        "event-bus" => {
            let what = if get("channel") == "socket" {
                "socket message"
            } else {
                "bus event"
            };
            detail = match get("tier").as_str() {
                Some("client→server") => " → server",
                Some("server→client") => " → client",
                _ => "",
            }
            .into();
            format!("{what} {}", quoted("event", "an event"))
        }
        "event-emitter" => format!("event {}", quoted("event", "an event")),
        "react-render" => "React re-render via setState".into(),
        "jsx-render" => format!(
            "renders {}",
            if truthy(get("via")) {
                format!("<{}>", string("via"))
            } else {
                "a child component".into()
            }
        ),
        "vue-handler" => format!(
            "Vue {} handler",
            if truthy(get("event")) {
                format!("@{}", string("event"))
            } else {
                "a template event".into()
            }
        ),
        "interface-impl" => match get("promotedInto").as_str() {
            Some(promoted) => format!("interface → method promoted into {promoted}"),
            None => "interface → impl".into(),
        },
        "closure-collection" => format!("runs {} handlers", quoted("field", "a collection")),
        "fn-pointer-dispatch" => format!(
            "fn-pointer {}",
            if truthy(get("via")) {
                string("via")
            } else {
                String::new()
            }
        ),
        "goframe-route" => format!(
            "GoFrame route {}",
            if truthy(get("route")) {
                string("route")
            } else {
                String::new()
            }
        ),
        "go-func-field" => format!(
            "func field {}{}",
            quoted("field", "a func field"),
            if truthy(get("via")) {
                format!(" (set from `{}`)", string("via"))
            } else {
                String::new()
            }
        ),
        _ => by.replace('-', " "),
    };
    Some(SynthesizedHop {
        summary,
        detail,
        registered_at: m
            .get("registeredAt")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

// Metadata comes from JSON, but the oracle explicitly applies String(value).
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(|v| {
                if v.is_null() {
                    String::new()
                } else {
                    js_string(v)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
        _ => v.to_string(),
    }
}
