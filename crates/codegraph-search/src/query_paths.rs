//! File and line references in explore queries — port of `query-paths.ts`.
//! The index and optional callbacks are the only authorities: no filesystem or
//! database access happens here, and prose such as `and/or` stays prose.

use codegraph_jsre::re;
use codegraph_types::jscompat::{is_js_space, js_trim, utf16_len};
use indexmap::{IndexMap, IndexSet};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryPathExtraction {
    pub stripped_query: String,
    pub pinned_files: Vec<String>,
    pub unresolved_path_spans: Vec<String>,
    pub line_anchors: Vec<QueryLineAnchor>,
    pub set_aside_matches: Vec<QuerySetAsideMatch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct QueryLineAnchor {
    pub file: String,
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuerySetAsideMatch {
    pub span: String,
    pub files: Vec<String>,
}

pub type SymbolFiles<'a> = &'a dyn Fn(&str) -> Vec<String>;

#[derive(Default)]
pub struct QueryPathOptions<'a> {
    pub max_pins: Option<usize>,
    pub max_matches_per_span: Option<usize>,
    pub exists_on_disk: Option<&'a dyn Fn(&str) -> bool>,
    pub symbol_files: Option<SymbolFiles<'a>>,
}

pub fn query_might_contain_paths(query: &str) -> bool {
    re!(r"[/\\]").is_match(query)
        || re!(r#"\.[A-Za-z][A-Za-z0-9]{0,7}(?=[\s,;:)\]'"`]|$)"#).is_match(query)
        || re!(r"(?:^|[^-\w])[A-Za-z0-9]+(?:-[A-Za-z0-9]+)+(?=[^-\w]|$)").is_match(query)
}

fn dotted_basename(s: &str) -> bool {
    re!(r"^[^\s/\\]+\.[A-Za-z][A-Za-z0-9]{0,7}$").is_match(s)
}
fn kebab_basename(s: &str) -> bool {
    re!(r"^[A-Za-z0-9]+(?:-[A-Za-z0-9]+)+$").is_match(s)
}

fn line_span(a: Option<&str>, b: Option<&str>) -> Option<(u32, u32)> {
    let start: u32 = a?.parse().ok()?;
    let end: u32 = b.map(str::parse).transpose().ok()?.unwrap_or(start);
    if start == 0 || end == 0 || start > 1_000_000 || end > 1_000_000 {
        return None;
    }
    Some((start.min(end), start.max(end)))
}

fn strip_line_punctuation(s: &str) -> &str {
    s.trim_start_matches(['(', '\'', '"', '`', '['])
        .trim_end_matches([')', '\'', '"', '`', ']', '.', ',', ';', ':', '!', '?'])
}

fn strip_wrapping(mut s: &str) -> (&str, Option<(u32, u32)>) {
    while let Some(first) = s.chars().next() {
        let strip = "'\"`<".contains(first)
            || (first == '(' && !s.contains(')'))
            || (first == '[' && !s.contains(']'))
            || (first == '{' && !s.contains('}'));
        if !strip {
            break;
        }
        s = &s[first.len_utf8()..];
    }
    while let Some(last) = s.chars().next_back() {
        let strip = "'\"`>.,;!?".contains(last)
            || (last == ')' && !s.contains('('))
            || (last == ']' && !s.contains('['))
            || (last == '}' && !s.contains('{'));
        if !strip {
            break;
        }
        s = &s[..s.len() - last.len_utf8()];
    }
    let Some(c) = re!(r"(?::(\d+)(?:-(\d+))?|#L(\d+)(?:-L?(\d+))?)$").captures(s) else {
        return (s, None);
    };
    let span = if c.text(1).is_some() {
        line_span(c.text(1), c.text(2))
    } else {
        line_span(c.text(3), c.text(4))
    };
    (&s[..c.get(0).unwrap().start()], span)
}

fn normalize_span(s: &str) -> String {
    let s = s.replace('\\', "/");
    let mut s = s.as_str();
    while let Some(tail) = s.strip_prefix("./") {
        s = tail;
    }
    re!(r"/{2,}")
        .replace_all_str(s, "/")
        .trim_end_matches('/')
        .to_owned()
}

fn clearly_path_shaped(s: &str) -> bool {
    s.rsplit_once('/')
        .is_some_and(|(head, tail)| !head.is_empty() && dotted_basename(tail))
}

fn resolve_span(
    s: &str,
    index: &IndexMap<String, String>,
    max_matches: usize,
) -> (Vec<String>, bool) {
    if let Some(exact) = index.get(s) {
        return (vec![exact.clone()], false);
    }
    let segments: Vec<_> = s.split('/').filter(|s| !s.is_empty()).collect();
    for drop in 0..segments.len().min(8) {
        let suffix = segments[drop..].join("/");
        if suffix.is_empty() {
            break;
        }
        let with_slash = format!("/{suffix}");
        let mut matches = Vec::new();
        for (lower, original) in index {
            if lower == &suffix || lower.ends_with(&with_slash) {
                matches.push(original.clone());
                if matches.len() > max_matches {
                    return (Vec::new(), true);
                }
            }
        }
        if !matches.is_empty() {
            return (matches, false);
        }
    }
    (Vec::new(), false)
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn query_symbol_tokens(tokens: &[&str], paths: &[String]) -> Vec<String> {
    let basenames: HashSet<_> = paths.iter().map(|p| basename(p).to_lowercase()).collect();
    let mut out = IndexSet::new();
    for raw in tokens {
        for part in re!(r"[,()[\]{}]+").split(raw) {
            let t = part
                .trim_start_matches(['\'', '"', '`', '<'])
                .trim_end_matches(['\'', '"', '`', '>', '.', ',', ';', ':', '!', '?']);
            if utf16_len(t) < 3 || !re!(r"^[A-Za-z_$][\w$]*(?:(?:::|\.)[\w$]+)*$").is_match(t) {
                continue;
            }
            if !re!(r"[._$]|::|[a-z][A-Z]|^[A-Z]").is_match(t)
                || basenames.contains(&t.to_lowercase())
            {
                continue;
            }
            out.insert(t.to_owned());
            if out.len() >= 16 {
                return out.into_iter().collect();
            }
        }
    }
    out.into_iter().collect()
}

struct Narrowing<'a> {
    symbol_files: Option<SymbolFiles<'a>>,
    tokens: Option<Vec<String>>,
    definers: HashMap<String, HashSet<String>>,
    max_matches: usize,
    set_aside: Vec<QuerySetAsideMatch>,
}

impl Narrowing<'_> {
    fn pinnable(
        &mut self,
        span: &str,
        matches: Vec<String>,
        tokens: &[&str],
        paths: &[String],
    ) -> Option<Vec<String>> {
        if matches.len() > 1 {
            if let Some(symbol_files) = self.symbol_files {
                let symbols = self
                    .tokens
                    .get_or_insert_with(|| query_symbol_tokens(tokens, paths));
                let candidates: HashSet<_> = matches.iter().collect();
                let mut definers = HashSet::new();
                for symbol in symbols {
                    for file in self
                        .definers
                        .entry(symbol.clone())
                        .or_insert_with(|| symbol_files(symbol).into_iter().collect())
                        .iter()
                    {
                        if candidates.contains(file) {
                            definers.insert(file.clone());
                        }
                    }
                }
                if !definers.is_empty()
                    && definers.len() != candidates.len()
                    && definers.len() <= self.max_matches
                {
                    self.set_aside.push(QuerySetAsideMatch {
                        span: span.to_owned(),
                        files: matches
                            .iter()
                            .filter(|m| !definers.contains(*m))
                            .cloned()
                            .collect(),
                    });
                    return Some(
                        matches
                            .into_iter()
                            .filter(|m| definers.contains(m))
                            .collect(),
                    );
                }
            }
        }
        if matches.len() > self.max_matches {
            None
        } else {
            Some(matches)
        }
    }
}

pub fn extract_query_paths(
    query: &str,
    paths: &[String],
    opts: &QueryPathOptions<'_>,
) -> QueryPathExtraction {
    let mut out = QueryPathExtraction {
        stripped_query: query.to_owned(),
        pinned_files: Vec::new(),
        unresolved_path_spans: Vec::new(),
        line_anchors: Vec::new(),
        set_aside_matches: Vec::new(),
    };
    if js_trim(query).is_empty() || paths.is_empty() {
        return out;
    }
    let max_pins = opts.max_pins.unwrap_or(8).max(1);
    let max_matches = opts.max_matches_per_span.unwrap_or(3).max(1);
    let mut index = IndexMap::new();
    for p in paths {
        index.insert(p.to_lowercase(), p.clone());
    }
    let tokens: Vec<_> = query.split(is_js_space).filter(|s| !s.is_empty()).collect();
    let mut consumed = HashSet::new();
    let mut pinned_seen = HashSet::new();
    let mut single_file_at = IndexMap::<usize, String>::new();
    let mut examined = 0;
    let mut narrowing = Narrowing {
        symbol_files: opts.symbol_files,
        tokens: None,
        definers: HashMap::new(),
        max_matches,
        set_aside: Vec::new(),
    };
    let collect_limit = if opts.symbol_files.is_some() {
        usize::MAX
    } else {
        max_matches
    };
    for (i, token) in tokens.iter().enumerate() {
        if out.pinned_files.len() >= max_pins || examined >= 8 {
            break;
        }
        let (stripped, lines) = strip_wrapping(token);
        if utf16_len(stripped) < 4
            || (!stripped.contains(['/', '\\']) && !dotted_basename(stripped))
        {
            continue;
        }
        let normalized = normalize_span(stripped);
        if normalized.is_empty() {
            continue;
        }
        examined += 1;
        let (matches, resolved_ambiguous) =
            resolve_span(&normalized.to_lowercase(), &index, collect_limit);
        let had_matches = !matches.is_empty();
        let pinnable = if had_matches {
            narrowing.pinnable(&normalized, matches, &tokens, paths)
        } else {
            None
        };
        let ambiguous = resolved_ambiguous || (had_matches && pinnable.is_none());
        let matches = pinnable.unwrap_or_default();
        if !matches.is_empty() {
            consumed.insert(i);
            pin_matches(
                &matches,
                i,
                lines,
                max_pins,
                &mut out,
                &mut pinned_seen,
                &mut single_file_at,
            );
        } else if ambiguous
            || clearly_path_shaped(&normalized)
            || (normalized.contains('/')
                && opts
                    .exists_on_disk
                    .is_some_and(|exists| exists(&normalized)))
        {
            consumed.insert(i);
            if out.unresolved_path_spans.len() < 4 {
                out.unresolved_path_spans.push(normalized);
            }
        }
    }
    let mut stems: Option<HashMap<String, Vec<String>>> = None;
    for (i, token) in tokens.iter().enumerate() {
        if out.pinned_files.len() >= max_pins {
            break;
        }
        if consumed.contains(&i) {
            continue;
        }
        let (stripped, lines) = strip_wrapping(token);
        if utf16_len(stripped) < 4 || !kebab_basename(stripped) {
            continue;
        }
        let stems = stems.get_or_insert_with(|| {
            let mut map = HashMap::<String, Vec<String>>::new();
            for p in paths {
                let name = basename(p);
                if !name.contains('-') {
                    continue;
                }
                let stem = re!(r"\.[A-Za-z][A-Za-z0-9]{0,7}$")
                    .replace_first(name, |_| String::new())
                    .to_lowercase();
                if !stem.is_empty() {
                    map.entry(stem).or_default().push(p.clone());
                }
            }
            map
        });
        let Some(matches) = stems.get(&stripped.to_lowercase()) else {
            continue;
        };
        let Some(matches) = narrowing.pinnable(stripped, matches.clone(), &tokens, paths) else {
            continue;
        };
        consumed.insert(i);
        pin_matches(
            &matches,
            i,
            lines,
            max_pins,
            &mut out,
            &mut pinned_seen,
            &mut single_file_at,
        );
    }
    if !single_file_at.is_empty() {
        for (i, token) in tokens.iter().enumerate() {
            if consumed.contains(&i) {
                continue;
            }
            let token = strip_line_punctuation(token);
            let after_word = i > 0
                && !consumed.contains(&(i - 1))
                && re!(r"^lines?$", "i").is_match(strip_line_punctuation(tokens[i - 1]));
            let after_path = i > 0 && single_file_at.contains_key(&(i - 1));
            let Some(c) = re!(r"^(L?)(\d+)(?:(?:-|–|\.\.)L?(\d+))?$").captures(token) else {
                continue;
            };
            if c.text(1) == Some("") && !after_word && !after_path {
                continue;
            }
            let mut span = line_span(c.text(2), c.text(3));
            let mut used = vec![i];
            if span.is_some()
                && c.text(3).is_none()
                && i + 2 < tokens.len()
                && re!(r"^(?:to|through|thru)$", "i")
                    .is_match(strip_line_punctuation(tokens[i + 1]))
            {
                if let Some(tail) = re!(r"^(L?)(\d+)(?:(?:-|–|\.\.)L?(\d+))?$")
                    .captures(strip_line_punctuation(tokens[i + 2]))
                {
                    if tail.text(3).is_none() {
                        span = line_span(c.text(2), tail.text(2));
                        used.extend([i + 1, i + 2]);
                    }
                }
            }
            let Some((start, end)) = span else {
                continue;
            };
            // Stable min preserves the first path token on an equal-distance tie.
            let (_, file) = single_file_at
                .iter()
                .min_by_key(|(idx, _)| idx.abs_diff(i))
                .unwrap();
            out.line_anchors.push(QueryLineAnchor {
                file: file.clone(),
                start,
                end,
            });
            consumed.extend(used);
            if after_word {
                consumed.insert(i - 1);
            }
        }
    }
    if consumed.is_empty() {
        return out;
    }
    out.stripped_query = tokens
        .iter()
        .enumerate()
        .filter(|(i, _)| !consumed.contains(i))
        .map(|(_, t)| *t)
        .collect::<Vec<_>>()
        .join(" ");
    let mut seen = HashSet::new();
    out.line_anchors.retain(|a| seen.insert(a.clone()));
    for mut aside in narrowing.set_aside {
        aside.files.retain(|f| !pinned_seen.contains(f));
        if !aside.files.is_empty() {
            out.set_aside_matches.push(aside);
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn pin_matches(
    matches: &[String],
    i: usize,
    lines: Option<(u32, u32)>,
    max_pins: usize,
    out: &mut QueryPathExtraction,
    seen: &mut HashSet<String>,
    single: &mut IndexMap<usize, String>,
) {
    for m in matches {
        if seen.contains(m) || out.pinned_files.len() >= max_pins {
            continue;
        }
        seen.insert(m.clone());
        out.pinned_files.push(m.clone());
    }
    if matches.len() == 1 && seen.contains(&matches[0]) {
        single.insert(i, matches[0].clone());
        if let Some((start, end)) = lines {
            out.line_anchors.push(QueryLineAnchor {
                file: matches[0].clone(),
                start,
                end,
            });
        }
    }
}
