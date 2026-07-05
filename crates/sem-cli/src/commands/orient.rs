//! `sem orient <query>` — structural code search. Finds the entities most
//! relevant to a query so an agent (or human) dropped into an unfamiliar
//! codebase can locate the right function/class without knowing its name.
//!
//! The ranking lives in `sem_core::parser::orient` (shared with the
//! `sem_entities` MCP tool's query mode); this is the CLI/IO wrapper.

use std::path::Path;

use colored::Colorize;
use sem_core::git::bridge::GitBridge;
use sem_core::parser::orient::{orient, query_terms, OrientHit};
use serde::Serialize;

pub struct OrientOptions {
    pub cwd: String,
    pub query: String,
    pub limit: usize,
    pub json: bool,
    pub file_exts: Vec<String>,
    pub no_cache: bool,
    pub no_default_excludes: bool,
    /// Token budget for a packed briefing: after ranking, pack the top hits'
    /// bodies plus immediate neighbors into this budget (0 = off).
    pub pack: usize,
}

#[derive(Serialize)]
struct OrientHitJson {
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

impl From<&OrientHit> for OrientHitJson {
    fn from(h: &OrientHit) -> Self {
        OrientHitJson {
            name: h.name.clone(),
            entity_type: h.entity_type.clone(),
            file: h.file_path.clone(),
            start_line: h.start_line,
            signature: h.signature.clone(),
            dependencies: h.dependencies,
            dependents: h.dependents,
            score: h.score,
        }
    }
}

/// Resident-server fast path: ranked hits from the warm graph in
/// milliseconds. Text output only; --json and custom scopes stay local. A
/// miss auto-spawns a resident server for next time.
fn try_sidecar_orient(opts: &OrientOptions) -> bool {
    if opts.json || opts.no_cache || opts.no_default_excludes || !opts.file_exts.is_empty() {
        return false;
    }
    let Ok(git) = GitBridge::open(Path::new(&opts.cwd)) else {
        return false;
    };
    let root = git.repo_root().to_path_buf();
    if root.join(".semignore").exists() {
        return false;
    }
    let request = serde_json::json!({
        "op": "orient",
        "query": opts.query,
        "limit": opts.limit,
    });
    let Some(response) = super::sidecar::query(&root, &request) else {
        return false;
    };
    let Some(text) = response.get("text").and_then(|v| v.as_str()) else {
        return false;
    };
    print!("{text}");
    true
}

/// Code-ish terms from free task text: identifiers, flags, dotted/underscored
/// names, and quoted tokens — the vocabulary that survives into code bodies.
/// Generic English is dropped so convergence counting stays meaningful.
fn salient_terms(text: &str) -> Vec<String> {
    use std::collections::BTreeSet;
    // GitHub issues bury the signal under environment dumps ("Output of
    // xr.show_versions()", pip freeze) folded into <details> blocks; their
    // library names and platform triples look code-ish, so cut them out
    // before extraction.
    let mut cleaned = String::with_capacity(text.len());
    let mut depth = 0usize;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with("<details") {
            depth += 1;
            continue;
        }
        if l.starts_with("</details") {
            depth = depth.saturating_sub(1);
            continue;
        }
        if depth == 0 {
            cleaned.push_str(line);
            cleaned.push('\n');
        }
    }
    let text = cleaned.as_str();
    let mut out: BTreeSet<String> = BTreeSet::new();
    for raw in text.split(|c: char| c.is_whitespace() || "()[]{}<>,;!?".contains(c)) {
        let t = raw.trim_matches(|c: char| "`'\"*:=#".contains(c));
        if t.len() < 4 || t.len() > 40 {
            continue;
        }
        let codeish = t.contains('_')
            || t.contains('-')
            || (t.contains('.') && !t.ends_with('.'))
            || t.chars().any(|c| c.is_uppercase()) && t.chars().any(|c| c.is_lowercase());
        if codeish {
            let term = t.trim_start_matches('-').to_lowercase();
            // Attribute accesses arrive glued to their receiver ("d2.loc",
            // "self.config.recursive"): the receiver is caller-local noise,
            // but each ".attr" suffix is spelled identically in the library's
            // own source. Emit those as terms too; IDF keeps common ones tame.
            if term.contains('.') {
                for comp in term.split('.').skip(1) {
                    if comp.len() >= 3 && comp.chars().all(|c| c.is_alphanumeric() || c == '_') {
                        out.insert(format!(".{comp}"));
                    }
                }
            }
            out.insert(term);
        }
    }
    // Most issues are plain English ("require a non-empty name for
    // Blueprints"), and their signal lives in ordinary words, not code-ish
    // tokens. Keeping only code-ish tokens throws that signal away and leaves
    // orient matching noise. So always fold in the distinctive plain words too
    // (>= 4 chars, non-stopword); IDF in the ranker keeps common ones like
    // "name" tame while rare ones like "blueprint"/"empty" carry the match.
    for t in query_terms(text) {
        if t.len() >= 4 {
            out.insert(t);
        }
    }
    out.into_iter().take(12).collect()
}

pub fn orient_command(opts: OrientOptions) {
    if query_terms(&opts.query).is_empty() {
        eprintln!(
            "{} query has no searchable terms (drop stopwords / use words of 3+ chars)",
            "error:".red().bold()
        );
        std::process::exit(2);
    }

    if opts.pack == 0 && try_sidecar_orient(&opts) {
        return;
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

    let hits = orient(&all_entities, &graph, &opts.query, opts.limit);

    // Briefing mode: pack the top hits' bodies plus their immediate
    // neighborhood into the token budget. Designed for prompt-time injection:
    // the task text goes in, the code an agent would otherwise spend its
    // first turns foraging for comes out.
    //
    // Ranking differs from plain orient: issue text vocabulary lives in
    // BODIES ("self.config.recursive"), not entity names, so we extract the
    // code-ish salient terms from the task text, match them against entity
    // contents, and rank by the IDF-weighted sum of converging terms: a term
    // appearing in half the repo ("method", "value") is worth almost nothing,
    // a term appearing in three bodies is worth everything.
    // Name-level orient score breaks ties.
    if opts.pack > 0 {
        let salient = salient_terms(&opts.query);
        // Functions/methods only: class entities contain every method
        // body, so they match every term by sheer size and drown the
        // actual signal. Methods are extracted separately anyway.
        let candidates: Vec<(&sem_core::model::entity::SemanticEntity, String)> = all_entities
            .iter()
            .filter(|e| {
                (e.entity_type == "function" || e.entity_type == "method")
                    && !e.file_path.contains("test")
                    && !e.file_path.starts_with("ci/")
                    && !e.file_path.starts_with("doc")
            })
            // Match terms against the entity's identity (file path + name),
            // not just its body. A distinctive query word like "blueprint"
            // names the file/class where the fix belongs even when the terse
            // constructor's body never repeats it — the case body-only
            // matching misses.
            .map(|e| {
                let hay = format!("{} {} {}", e.content, e.file_path, e.name).to_lowercase();
                (e, hay)
            })
            .collect();
        let n = candidates.len() as f64;
        let idf: Vec<f64> = salient
            .iter()
            .map(|t| {
                let df = candidates.iter().filter(|(_, b)| b.contains(t)).count() as f64;
                ((n + 1.0) / (df + 1.0)).ln()
            })
            .collect();
        let mut scored: Vec<(f64, &sem_core::model::entity::SemanticEntity)> = candidates
            .iter()
            .filter_map(|(e, body)| {
                let term_score: f64 = salient
                    .iter()
                    .zip(&idf)
                    .filter(|(t, _)| body.contains(*t))
                    .map(|(_, w)| w)
                    .sum();
                if term_score <= 0.0 {
                    return None;
                }
                let name_hit = hits.iter().position(|h| h.id == e.id);
                let name_bonus = name_hit.map(|i| (hits.len() - i) as f64).unwrap_or(0.0);
                let centrality =
                    ((graph.get_dependents(&e.id).len() + graph.get_dependencies(&e.id).len())
                        as f64
                        + 1.0)
                        .ln();
                Some((term_score * 3.0 + name_bonus + centrality, *e))
            })
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        // Two slots by body-term convergence, one by entity NAME: when the
        // issue names the surface API (".loc"), the culprit's body often
        // never mentions it — but an entity NAMED like a salient term, plus
        // its 1-hop neighborhood, reaches it. Name must echo a code-ish term
        // so plain-word junk hits can't claim the slot.
        let mut top: Vec<&str> = scored.iter().take(2).map(|(_, e)| e.id.as_str()).collect();
        let name_pick = hits.iter().find(|h| {
            !top.contains(&h.id.as_str())
                && !h.file_path.contains("test")
                && salient.iter().any(|t| {
                    let stem = t.trim_start_matches('.');
                    stem.len() >= 3 && h.name.to_lowercase().contains(stem)
                })
        });
        if let Some(h) = name_pick {
            top.push(h.id.as_str());
        } else if let Some((_, e)) = scored.get(2) {
            top.push(e.id.as_str());
        }
        if top.is_empty() {
            println!("(no entities matched the task text)");
            return;
        }
        let per = opts.pack / top.len();
        println!(
            "⊕ briefing · {} entities packed from task text · budget {}\n",
            top.len(),
            opts.pack
        );
        for id in &top {
            let ctx = sem_core::parser::context::build_context_result_bounded(
                &graph,
                id,
                &all_entities,
                per,
                1,
            );
            for e in &ctx.entries {
                match e.role.as_str() {
                    "target" => {
                        println!("── {} · {} · {}", e.entity_name, e.entity_type, e.file_path);
                        println!("{}\n", e.content);
                    }
                    role => {
                        let sig = e.content.lines().next().unwrap_or("").trim();
                        println!("   {} {} · {} · {}", role, e.entity_name, e.file_path, sig);
                    }
                }
            }
            println!();
        }
        return;
    }

    if opts.json {
        let rows: Vec<OrientHitJson> = hits.iter().map(OrientHitJson::from).collect();
        match serde_json::to_string_pretty(&rows) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("{} {e}", "error:".red().bold());
                std::process::exit(1);
            }
        }
        return;
    }

    if hits.is_empty() {
        println!(
            "{} no entities matched {}",
            "orient:".yellow().bold(),
            opts.query.bold()
        );
        return;
    }

    println!("{} {}\n", "orient:".green().bold(), opts.query.bold());
    for h in &hits {
        let loc = format!("{}:{}", h.file_path, h.start_line);
        println!(
            "  {} {}  {}",
            format!("{:<9}", h.entity_type).dimmed(),
            h.name.bold(),
            loc.dimmed(),
        );
        if !h.signature.is_empty() {
            println!("    {}", h.signature.dimmed());
        }
        if h.dependents > 0 {
            println!("    {}", format!("{} dependents", h.dependents).cyan());
        }
    }
}
