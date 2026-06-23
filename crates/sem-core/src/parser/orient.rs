//! Structural code search: rank entities by relevance to a free-text query.
//!
//! Two passes:
//!   1. Lexical score over entity name (camelCase/snake_case subtokens, prefix/
//!      stem match, substring), file path, and the signature line.
//!   2. Re-rank the strongest lexical candidates by graph centrality, so a
//!      central, widely-used entity outranks a trivially-named helper.
//!
//! This is the structural counterpart to text search: grep finds text, orient
//! finds the entity (and reports how connected it is). Shared by the `sem
//! orient` CLI command and the `sem_entities` MCP tool's query mode.

use std::collections::HashSet;

use crate::model::entity::SemanticEntity;
use crate::parser::graph::EntityGraph;

const STOPWORDS: &[&str] = &[
    "the", "a", "an", "to", "for", "of", "in", "on", "and", "or", "is", "it", "add", "fix", "make",
    "with", "this", "that", "how", "where", "what", "when", "find", "get", "does", "we", "my",
];

/// One ranked search result.
#[derive(Debug, Clone)]
pub struct OrientHit {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub file_path: String,
    pub start_line: usize,
    pub signature: String,
    pub dependencies: usize,
    pub dependents: usize,
    pub score: f64,
}

/// Split a query into meaningful lowercase terms (drops stopwords and tokens
/// shorter than 3 chars).
pub fn query_terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(|t| t.to_lowercase())
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        .collect()
}

/// Split an identifier into lowercase subtokens across camelCase and
/// snake_case boundaries: `getUserId` -> [get, user, id].
fn ident_subtokens(name: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if c == '_' || c == '-' || c == '.' {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            tokens.push(std::mem::take(&mut cur));
        }
        cur.push(c.to_ascii_lowercase());
        prev_lower = c.is_lowercase();
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Prefix/stem match so `watch` matches `watcher` and `diff` matches
/// `difference`, requiring a shared prefix of at least 4 chars.
fn token_prefix_match(tok: &str, term: &str) -> bool {
    let shared = tok.len().min(term.len());
    shared >= 4 && (tok.starts_with(term) || term.starts_with(tok))
}

fn lexical_score(e: &SemanticEntity, terms: &[String]) -> f64 {
    let name_lower = e.name.to_lowercase();
    let name_tokens = ident_subtokens(&e.name);
    let path_lower = e.file_path.to_lowercase();
    let mut sig_tokens: HashSet<String> = HashSet::new();
    if let Some(sig) = e.content.lines().next() {
        for word in sig.split(|c: char| !c.is_alphanumeric()) {
            for t in ident_subtokens(word) {
                sig_tokens.insert(t);
            }
        }
    }
    let mut score = 0.0;
    for term in terms {
        if name_tokens.iter().any(|t| t == term) {
            score += 3.0;
        } else if name_tokens.iter().any(|t| token_prefix_match(t, term)) {
            score += 2.5;
        } else if name_lower.contains(term.as_str()) {
            score += 2.0;
        }
        if path_lower.contains(term.as_str()) {
            score += 1.0;
        }
        if sig_tokens.contains(term) {
            score += 1.5;
        }
    }
    score
}

/// Rank `entities` against `query`, returning up to `limit` hits best-first.
/// Returns empty if the query has no searchable terms or nothing matches.
pub fn orient(
    entities: &[SemanticEntity],
    graph: &EntityGraph,
    query: &str,
    limit: usize,
) -> Vec<OrientHit> {
    let terms = query_terms(query);
    if terms.is_empty() {
        return Vec::new();
    }

    let mut scored: Vec<(f64, &SemanticEntity)> = entities
        .iter()
        .filter_map(|e| {
            let s = lexical_score(e, &terms);
            (s > 0.0).then_some((s, e))
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    // Re-rank only the strongest lexical candidates by centrality.
    let cap = (limit * 4).max(20);
    scored.truncate(cap);
    let mut hits: Vec<OrientHit> = scored
        .into_iter()
        .map(|(lexical, e)| {
            let dependencies = graph.get_dependencies(&e.id).len();
            let dependents = graph.get_dependents(&e.id).len();
            let centrality = ((dependencies + dependents) as f64 + 1.0).ln();
            OrientHit {
                id: e.id.clone(),
                name: e.name.clone(),
                entity_type: e.entity_type.clone(),
                file_path: e.file_path.clone(),
                start_line: e.start_line,
                signature: e.content.lines().next().unwrap_or("").trim().to_string(),
                dependencies,
                dependents,
                score: lexical * 10.0 + centrality,
            }
        })
        .collect();
    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(limit);
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_terms_drop_stopwords_and_short() {
        assert_eq!(query_terms("where is the retry logic"), vec!["retry", "logic"]);
    }

    #[test]
    fn subtokens_split_camel_and_snake() {
        assert_eq!(ident_subtokens("getUserId"), vec!["get", "user", "id"]);
        assert_eq!(ident_subtokens("read_file"), vec!["read", "file"]);
    }

    #[test]
    fn prefix_match_handles_stems() {
        assert!(token_prefix_match("watcher", "watch"));
        assert!(token_prefix_match("diff", "difference"));
        assert!(!token_prefix_match("cat", "category"));
    }

    #[test]
    fn empty_query_returns_no_hits() {
        let g = EntityGraph {
            entities: Default::default(),
            edges: Default::default(),
            dependents: Default::default(),
            dependencies: Default::default(),
        };
        assert!(orient(&[], &g, "the a of", 5).is_empty());
    }
}
