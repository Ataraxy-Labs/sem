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

pub fn orient_command(opts: OrientOptions) {
    if query_terms(&opts.query).is_empty() {
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

    let hits = orient(&all_entities, &graph, &opts.query, opts.limit);

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
