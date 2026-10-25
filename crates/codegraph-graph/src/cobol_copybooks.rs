//! COBOL copybooks a query names — port of `src/graph/cobol-copybooks.ts`.
//!
//! The COBOL extractor records every `COPY X` and `EXEC SQL INCLUDE X` as an
//! `import` node named X sitting on the include statement, and the resolver
//! links X to the indexed copybook file (`X.cpy`, …) with an `imports` edge
//! from the including scope. A query token that names a copybook therefore asks
//! for two things: the copybook's own source, when it is indexed, and every
//! statement that includes it.
//!
//! A member whose source is not in the project (a DB2 DCLGEN member, a
//! compiler-supplied copybook such as SQLCA) has include sites and no file, and
//! saying so is part of the answer. The context builder and explore's renderer
//! share this one derivation so the two cannot drift apart.

use crate::Result;
use codegraph_types::{Edge, EdgeKind, Language, Node, NodeKind};

/// The queries this derivation needs — satisfied by `codegraph_db::Store`.
pub trait CopybookLookup {
    fn has_files_of_language(&self, language: &str) -> Result<bool>;
    fn get_cobol_includes_by_member(&self, member: &str, limit: u32) -> Result<Vec<Node>>;
    fn get_cobol_files_by_stem(&self, member: &str) -> Result<Vec<Node>>;
    fn get_incoming_edges(&self, target_id: &str, kinds: &[EdgeKind]) -> Result<Vec<Edge>>;
}

impl CopybookLookup for codegraph_db::Store {
    fn has_files_of_language(&self, language: &str) -> Result<bool> {
        Ok(codegraph_db::Store::has_files_of_language(self, language)?)
    }
    fn get_cobol_includes_by_member(&self, member: &str, limit: u32) -> Result<Vec<Node>> {
        Ok(codegraph_db::Store::get_cobol_includes_by_member(self, member, limit)?)
    }
    fn get_cobol_files_by_stem(&self, member: &str) -> Result<Vec<Node>> {
        Ok(codegraph_db::Store::get_cobol_files_by_stem(self, member)?)
    }
    fn get_incoming_edges(&self, target_id: &str, kinds: &[EdgeKind]) -> Result<Vec<Edge>> {
        Ok(codegraph_db::Store::get_incoming_edges(self, target_id, kinds)?)
    }
}

/// One copybook member the query names, with what the index knows about it.
#[derive(Debug, Clone, PartialEq)]
pub struct NamedCopybook {
    /// The member as the query spelled it.
    pub member: String,
    /// The indexed copybook file(s) the member resolves to — a `.cpy` file, or
    /// any COBOL file an include actually resolved to. Empty when the member's
    /// source is not indexed.
    pub files: Vec<Node>,
    /// Every `COPY` / `EXEC SQL INCLUDE` statement naming the member, in
    /// file/line order.
    pub includes: Vec<Node>,
}

/// Most include statements gathered per member. A shared copybook in a large
/// estate is included by hundreds of programs; past this the list is a count.
pub const MAX_COPYBOOK_INCLUDES: u32 = 200;

/// Most query tokens looked up — a pasted paragraph must not become 200 probes.
const MAX_MEMBER_TOKENS: usize = 16;

/// A COBOL word: letters, digits, hyphens and the national characters `$ # @`
/// (`CVACT01Y`, `CUST-REC`, `DFH$AID`). Wider than the identifier shapes the
/// generic symbol extraction knows — a copybook name with a digit in it
/// (`CVACT01Y`) was never extracted as a symbol at all.
fn is_word_start(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '$' | '#' | '@')
}

fn is_word_continue(c: char) -> bool {
    is_word_start(c) || c == '-'
}

/// The query's tokens that could name a copybook member. In a longer query a
/// plain lowercase word is skipped: `customer` in a prose question is English,
/// while a member name is written the way the code writes it (`CUSTOMER`,
/// `cvact01y`) — the same rule explore's own symbol seeding applies to bare
/// words. A query that is one word is a name lookup whatever its case
/// (`lgpolicy`, after the file `lgpolicy.cpy`).
pub fn copybook_member_tokens(query: &str) -> Vec<String> {
    // Same shape as the TypeScript `[A-Za-z0-9$#@][A-Za-z0-9$#@-]*`: a word
    // starts only at a start character, so a leading `-` belongs to no token.
    let mut words: Vec<&str> = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in query.char_indices() {
        if is_word_continue(c) {
            if start.is_none() && is_word_start(c) {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            words.push(&query[s..i]);
        }
    }
    if let Some(s) = start {
        words.push(&query[s..]);
    }
    let words: Vec<&str> = words
        .into_iter()
        .map(|w| w.trim_end_matches('-'))
        .filter(|w| !w.is_empty())
        .collect();
    let lone = words.len() == 1;
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for token in words {
        if token.chars().count() < 3
            || !token.chars().any(|c| c.is_ascii_alphabetic())
            || (!lone && token.chars().all(|c| c.is_ascii_lowercase()))
        {
            continue;
        }
        let key = token.to_ascii_uppercase();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push(token.to_owned());
        if out.len() >= MAX_MEMBER_TOKENS {
            break;
        }
    }
    out
}

/// Is this node a COBOL copybook include — the statement `COPY X` /
/// `EXEC SQL INCLUDE X` — rather than an import in any other language?
pub fn is_copybook_include(node: &Node) -> bool {
    node.kind == NodeKind::Import && node.language == Language::Cobol
}

/// The copybooks `query` names. A token qualifies when the index holds a COBOL
/// include of that member or a copybook file with that name. A project without
/// COBOL answers after one index seek, so every other language's queries are
/// untouched.
pub fn find_named_copybooks<L: CopybookLookup>(lookup: &L, query: &str) -> Result<Vec<NamedCopybook>> {
    let tokens = copybook_member_tokens(query);
    if tokens.is_empty() || !lookup.has_files_of_language(Language::Cobol.as_str())? {
        return Ok(Vec::new());
    }
    let mut named = Vec::new();
    for member in tokens {
        let includes = lookup.get_cobol_includes_by_member(&member, MAX_COPYBOOK_INCLUDES)?;
        // A same-named PROGRAM (`COACTUPC.cbl` for the query `COACTUPC`) is not
        // a copybook: only a `.cpy` file, or a file some include resolved to, is.
        let mut files = Vec::new();
        for file in lookup.get_cobol_files_by_stem(&member)? {
            if file.file_path.to_ascii_lowercase().ends_with(".cpy")
                || !lookup.get_incoming_edges(&file.id, &[EdgeKind::Imports])?.is_empty()
            {
                files.push(file);
            }
        }
        if includes.is_empty() && files.is_empty() {
            continue;
        }
        named.push(NamedCopybook {
            member,
            files,
            includes,
        });
    }
    Ok(named)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_cobol_shaped_words_and_skips_plain_lowercase_english() {
        assert_eq!(
            copybook_member_tokens("where is CVACT01Y included from cust-rec and dfh$aid"),
            vec!["CVACT01Y", "cust-rec", "dfh$aid"]
        );
    }

    #[test]
    fn dedupes_case_insensitively_and_drops_short_or_digit_only_tokens() {
        assert_eq!(copybook_member_tokens("SQLCA sqlca IO 0042 Sqlca"), vec!["SQLCA"]);
    }

    #[test]
    fn takes_a_one_word_query_as_a_name_whatever_its_case() {
        assert_eq!(copybook_member_tokens("lgpolicy"), vec!["lgpolicy"]);
        assert!(copybook_member_tokens("the lgpolicy").is_empty());
    }

    #[test]
    fn trims_trailing_hyphens_from_a_word() {
        assert_eq!(copybook_member_tokens("CUST-REC--"), vec!["CUST-REC"]);
    }
}
