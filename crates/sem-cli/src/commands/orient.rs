//! `sem orient <query>` — find the entities most relevant to a query, so an
//! agent (or human) dropped into an unfamiliar codebase can locate the right
//! function/class without already knowing its name.
//!
//! Two-pass ranking, mirroring the cloud `orient` endpoint:
//!   1. Lexical score over entity name (subtoken + prefix + substring), file
//!      path, and the signature line.
//!   2. Re-rank the strongest lexical candidates by graph centrality, so a
//!      well-named-but-trivial helper loses to a central, widely-used entity.
//!
//! This is the structural-discovery counterpart to grep: grep finds text, this
//! finds the entity and reports how connected it is.

use std::collections::HashSet;
use std::path::Path;

use colored::Colorize;
use sem_core::git::bridge::GitBridge;
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::graph::EntityGraph;
use serde::Serialize;

pub struct OrientOptions {
    pub cwd: String,
    pub query: String,
    pub limit: usize,
    pub json: bool,
    pub file_exts: Vec<String>,
    pub no_cache: bool,
    pub no_default_excludes: bool,
}

const STOPWORDS: &[&str] = &[
    "the", "a", "an", "to", "for", "of", "in", "on", "and", "or", "is", "it", "add", "fix", "make",
    "with", "this", "that", "how", "where", "what", "when", "find", "get", "does", "we", "my",
];

/// Split a query into meaningful lowercase terms (drops stopwords and very
/// short tokens).
fn query_terms(query: &str) -> Vec<String> {
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
    // Body-aware: the signature line (first line of the entity) often carries
    // the intent words (parameters, return type).
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
            score += 3.0; // exact name-subtoken hit
        } else if name_tokens.iter().any(|t| token_prefix_match(t, term)) {
            score += 2.5; // stem/prefix hit
        } else if name_lower.contains(term.as_str()) {
            score += 2.0; // substring of the name
        }
        if path_lower.contains(term.as_str()) {
            score += 1.0; // appears in the file path
        }
        if sig_tokens.contains(term) {
            score += 1.5; // appears in the signature
        }
    }
    score
}

#[derive(Serialize)]
struct OrientHit {
    name: String,
    #[serde(rename = "type")]
    entity_type: String,
    file: String,
    start_line: usize,
    signature: String,
    dependencies: usize,
    dependents: usize,
    score: f64,
}

/// Rank entities by lexical relevance, then re-rank the top candidates by graph
/// centrality. Pure and testable; the command wrapper handles IO.
fn rank<'a>(
    entities: &'a [SemanticEntity],
    graph: &EntityGraph,
    terms: &[String],
    limit: usize,
) -> Vec<(f64, &'a SemanticEntity)> {
    let mut scored: Vec<(f64, &SemanticEntity)> = entities
        .iter()
        .filter_map(|e| {
            let s = lexical_score(e, terms);
            (s > 0.0).then_some((s, e))
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    // Re-rank only the strongest lexical candidates by centrality.
    let cap = (limit * 4).max(20);
    scored.truncate(cap);
    let mut hits: Vec<(f64, &SemanticEntity)> = scored
        .into_iter()
        .map(|(lexical, e)| {
            let deps = graph.get_dependencies(&e.id).len();
            let dependents = graph.get_dependents(&e.id).len();
            // Saturating centrality boost so a few hot entities don't dominate.
            let centrality = ((deps + dependents) as f64 + 1.0).ln();
            (lexical * 10.0 + centrality, e)
        })
        .collect();
    hits.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(limit);
    hits
}

pub fn orient_command(opts: OrientOptions) {
    let terms = query_terms(&opts.query);
    if terms.is_empty() {
        eprintln!(
            "{} query has no searchable terms (drop stopwords / use words of 3+ chars)",
            "error:".red().bold()
        );
        std::process::exit(2);
    }

    let root = match GitBridge::open(Path::new(&opts.cwd)) {
        Ok(git) => git.repo_root().to_path_buf(),
        Err(_) => Path::new(&opts.cwd).to_path_buf(),
    };
    let root = root.as_path();
    let registry = super::create_registry(&root.to_string_lossy());
    let ext_filter = super::graph::normalize_exts(&opts.file_exts);
    let source_scope =
        super::graph::cache_source_scope(root, &ext_filter, opts.no_default_excludes);
    let file_paths = super::graph::find_supported_files_with_options(
        root,
        &registry,
        &ext_filter,
        opts.no_default_excludes,
    );
    let prog = crate::progress::Progress::start("Building entity graph");
    let (graph, all_entities) =
        super::graph::get_or_build_graph(root, &file_paths, &registry, opts.no_cache, source_scope);
    prog.done(&format!(
        "{} entities, {} files",
        super::graph::fmt_count(graph.entities.len()),
        super::graph::fmt_count(file_paths.len())
    ));

    let ranked = rank(&all_entities, &graph, &terms, opts.limit);

    if opts.json {
        let hits: Vec<OrientHit> = ranked
            .iter()
            .map(|(score, e)| OrientHit {
                name: e.name.clone(),
                entity_type: e.entity_type.clone(),
                file: e.file_path.clone(),
                start_line: e.start_line,
                signature: e.content.lines().next().unwrap_or("").trim().to_string(),
                dependencies: graph.get_dependencies(&e.id).len(),
                dependents: graph.get_dependents(&e.id).len(),
                score: *score,
            })
            .collect();
        match serde_json::to_string_pretty(&hits) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("{} {e}", "error:".red().bold());
                std::process::exit(1);
            }
        }
        return;
    }

    if ranked.is_empty() {
        println!(
            "{} no entities matched {}",
            "orient:".yellow().bold(),
            opts.query.bold()
        );
        return;
    }

    println!(
        "{} {}\n",
        "orient:".green().bold(),
        opts.query.bold()
    );
    for (_score, e) in &ranked {
        let loc = format!("{}:{}", e.file_path, e.start_line);
        let dependents = graph.get_dependents(&e.id).len();
        let sig = e.content.lines().next().unwrap_or("").trim();
        println!(
            "  {} {}  {}",
            format!("{:<9}", e.entity_type).dimmed(),
            e.name.bold(),
            loc.dimmed(),
        );
        if !sig.is_empty() {
            println!("    {}", sig.dimmed());
        }
        if dependents > 0 {
            println!("    {}", format!("{dependents} dependents").cyan());
        }
    }
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
        assert!(!token_prefix_match("cat", "category")); // shared < 4
    }
}
