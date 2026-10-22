//! MyBatis 3 and iBatis 2 XML mappers, preserving statement identity and includes.
use super::Document;
use crate::source::{now_ms, Utf16Cursor};
use codegraph_jsre::re;
use codegraph_types::{
    jscompat::{js_trim, utf16_prefix},
    node_id, ExtractionResult, Language, Node, NodeKind,
};

/// Mask comments in bytes while keeping a cursor over the original source for
/// UTF-16 IDs. CDATA holds SQL data, so its literal comment markers stay intact.
fn mask_comments(source: &str) -> String {
    let mut out = source.as_bytes().to_vec();
    let mut at = 0;
    while at < source.len() {
        if source.as_bytes()[at..].starts_with(b"<![CDATA[") {
            at = source[at + 9..]
                .find("]]>")
                .map(|p| at + 12 + p)
                .unwrap_or(source.len());
        } else if source.as_bytes()[at..].starts_with(b"<!--") {
            let stop = source[at + 4..]
                .find("-->")
                .map(|p| at + 7 + p)
                .unwrap_or(source.len());
            for b in &mut out[at..stop] {
                if *b != b'\n' {
                    *b = b' ';
                }
            }
            at = stop;
        } else {
            at += 1;
        }
    }
    String::from_utf8(out).expect("whole comment ranges were masked")
}

fn root(source: &str) -> Option<(&str, bool, usize, usize)> {
    for (pattern, close_tag, ibatis) in [
        (re!(r"<mapper\b([^>]*)>"), "</mapper>", false),
        (re!(r"<sqlMap\b([^>]*)>"), "</sqlMap>", true),
    ] {
        let Some(c) = pattern.captures(source) else {
            continue;
        };
        let attrs = c.text(1).unwrap_or("");
        let namespace = re!(r#"\bnamespace\s*=\s*(["'])([^"']+)\1"#)
            .captures(attrs)
            .and_then(|c| c.text(2));
        if namespace.is_none() && !ibatis {
            continue;
        }
        let start = c.get(0).unwrap().end();
        let end = source[start..]
            .find(close_tag)
            .map(|i| start + i)
            .unwrap_or(source.len());
        return Some((namespace.unwrap_or(""), ibatis, start, end));
    }
    None
}

pub fn extract(file: &str, source: &str) -> ExtractionResult {
    let mut doc = Document::new(file, source, Language::Xml);
    let file_id = doc.file_node();
    let masked = mask_comments(source);
    let Some((namespace, ibatis, start, end)) = root(&masked) else {
        return doc.finish();
    };
    let pattern = if ibatis {
        re!(
            r"<(select|insert|update|delete|sql|statement|procedure)\b([^>]*)>([\s\S]*?)</\1\s*>",
            "g"
        )
    } else {
        re!(
            r"<(select|insert|update|delete|sql)\b([^>]*)>([\s\S]*?)</\1\s*>",
            "g"
        )
    };
    let mut offsets = Utf16Cursor::new(source);
    for c in pattern.captures_iter(&masked[start..end]) {
        let verb = c.text(1).unwrap();
        let attrs = c.text(2).unwrap_or("");
        let body = c.text(3).unwrap_or("");
        let Some(id_match) = re!(r#"\bid\s*=\s*(["'])([^"']+)\1"#).captures(attrs) else {
            continue;
        };
        let name = id_match.text(2).unwrap();
        let absolute = start + c.get(0).unwrap().start();
        let (qualified, display) = if !namespace.is_empty() {
            (format!("{namespace}::{name}"), name)
        } else if let Some((owner, name)) = name.rsplit_once('.') {
            (format!("{owner}::{name}"), name)
        } else {
            (name.to_owned(), name)
        };
        // The reference hashes the UTF-16 offset here, not a line number.
        let node_id = node_id(
            file,
            NodeKind::Method,
            &qualified,
            offsets.at(absolute) as u32,
        );
        let mut node = Node::new(
            &node_id,
            NodeKind::Method,
            display,
            qualified,
            file,
            Language::Xml,
            doc.lines.row(absolute) as u32 + 1,
            doc.lines.row(start + c.get(0).unwrap().end()) as u32 + 1,
            0,
            0,
            now_ms(),
        );
        node.signature = Some(signature(verb, attrs));
        let without_tags = re!(r"<[^>]+>", "g").replace_all_str(body, " ");
        let condensed = re!(r"\s+", "g").replace_all_str(&without_tags, " ");
        node.docstring = Some(utf16_prefix(js_trim(&condensed), 200).to_owned());
        doc.contains(&file_id, node);
        for inc in re!(r#"<include\b[^>]*\brefid\s*=\s*(["'])([^"']+)\1"#, "g").captures_iter(body)
        {
            let target = inc.text(2).unwrap();
            let qualified = if let Some((owner, name)) = target.rsplit_once('.') {
                format!("{owner}::{name}")
            } else if !namespace.is_empty() {
                format!("{namespace}::{target}")
            } else {
                target.to_owned()
            };
            let offset = start + c.get(3).unwrap().start() + inc.get(0).unwrap().start();
            doc.reference(
                &node_id,
                qualified,
                doc.lines.row(offset) as u32 + 1,
                0,
                false,
            );
        }
    }
    doc.finish()
}

fn signature(verb: &str, attrs: &str) -> String {
    if verb == "sql" {
        return "<sql>".to_owned();
    }
    let mut out = verb.to_ascii_uppercase();
    for (pattern, label) in [
        (re!(r#"\bparameterType\s*=\s*(["'])([^"']+)\1"#), "param"),
        (re!(r#"\bresultType\s*=\s*(["'])([^"']+)\1"#), "result"),
        (re!(r#"\bdatabaseId\s*=\s*(["'])([^"']+)\1"#), "databaseId"),
    ] {
        if let Some(c) = pattern.captures(attrs) {
            out.push(' ');
            out.push_str(label);
            out.push('=');
            out.push_str(c.text(2).unwrap());
        }
    }
    out
}
