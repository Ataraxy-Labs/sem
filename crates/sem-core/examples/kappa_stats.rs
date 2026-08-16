//! Throwaway analysis tool for the kappa spike (semx-n8c). NOT part of the
//! deliverable -- used once to gather KAPPA.md's collision numbers on real
//! corpora, then deleted. Not registered anywhere else; safe to remove.
//!
//! Usage: cargo run --release --example kappa_stats -- <repo_root> [label]
//!
//! Walks the repo, extracts entities, and reports:
//!  - how many entities have Some(kappa) vs None
//!  - how many DISTINCT kappa values exist, and how many of those are shared
//!    by more than one entity
//!  - for kappa groups that span more than one DISTINCT structural_hash,
//!    prints a sample so a human can eyeball whether they're genuine
//!    formatting-variants/duplicated code (good) or false merges (bad)

use rayon::prelude::*;
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::plugins::create_default_registry;
use sem_core::parser::registry::ParserRegistry;
use sem_core::utils::scan::{is_default_excluded, is_probably_binary_path};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn make_registry(root: &Path) -> ParserRegistry {
    let mut registry = create_default_registry();
    registry.load_semrc(root);
    registry.load_gitattributes(root);
    registry
}

fn walk_files(root: &Path, registry: &ParserRegistry) -> Vec<String> {
    let mut files = Vec::new();
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true);
    for entry in builder.build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let rel = match path.strip_prefix(root) {
            Ok(p) => p.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        if is_default_excluded(&rel) || is_probably_binary_path(&rel) {
            continue;
        }
        if registry.get_explicit_plugin(&rel).is_none() {
            continue;
        }
        files.push(rel);
    }
    files.sort();
    files
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: kappa_stats <repo_root> [label] [sample_n]");
        std::process::exit(1);
    }
    let root = PathBuf::from(&args[1]).canonicalize().expect("bad root");
    let label = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| root.file_name().unwrap().to_string_lossy().to_string());
    let sample_n: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(40);

    let registry = make_registry(&root);
    let file_paths = walk_files(&root, &registry);

    let contents: Vec<(String, String)> = file_paths
        .par_iter()
        .filter_map(|p| {
            std::fs::read_to_string(root.join(p))
                .ok()
                .map(|c| (p.clone(), c))
        })
        .collect();

    let all_entities: Vec<SemanticEntity> = contents
        .par_iter()
        .flat_map(|(p, c)| {
            registry
                .extract_entities_with_tree(p, c)
                .map(|(ents, _tree)| ents)
                .unwrap_or_default()
        })
        .collect();

    let total = all_entities.len();
    let with_kappa = all_entities.iter().filter(|e| e.kappa.is_some()).count();
    println!(
        "SUMMARY label={label} files={} total_entities={total} with_kappa={with_kappa} \
         without_kappa={}",
        contents.len(),
        total - with_kappa
    );

    // Group by kappa, among entities that have one.
    let mut by_kappa: HashMap<&str, Vec<&SemanticEntity>> = HashMap::new();
    for e in &all_entities {
        if let Some(k) = e.kappa.as_deref() {
            by_kappa.entry(k).or_default().push(e);
        }
    }
    let distinct_kappa = by_kappa.len();
    let shared_groups: Vec<(&str, &Vec<&SemanticEntity>)> = by_kappa
        .iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(k, v)| (*k, v))
        .collect();
    println!(
        "KAPPA_GROUPS label={label} distinct_kappa={distinct_kappa} groups_with_gt1_entity={}",
        shared_groups.len()
    );

    // Among shared-kappa groups, how many span >1 DISTINCT structural_hash?
    // That's the interesting "collision" set: same kappa, different
    // structural_hash -- either a genuine formatting-variant / duplicated
    // logic (good) or a false merge (bad).
    let mut interesting: Vec<(&str, &Vec<&SemanticEntity>)> = shared_groups
        .iter()
        .filter(|(_, v)| {
            let mut struct_hashes: Vec<&str> = v
                .iter()
                .filter_map(|e| e.structural_hash.as_deref())
                .collect();
            struct_hashes.sort_unstable();
            struct_hashes.dedup();
            struct_hashes.len() > 1
        })
        .cloned()
        .collect();
    interesting.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));

    println!(
        "KAPPA_COLLISIONS label={label} groups_spanning_multiple_structural_hash={} \
         entities_in_those_groups={}",
        interesting.len(),
        interesting.iter().map(|(_, v)| v.len()).sum::<usize>()
    );

    println!("---- sample of up to {sample_n} collision groups ----");
    for (k, v) in interesting.iter().take(sample_n) {
        let mut struct_hashes: Vec<&str> = v
            .iter()
            .filter_map(|e| e.structural_hash.as_deref())
            .collect();
        struct_hashes.sort_unstable();
        struct_hashes.dedup();
        println!(
            "kappa={k} n_entities={} n_distinct_structural_hash={}",
            v.len(),
            struct_hashes.len()
        );
        for e in v.iter().take(6) {
            let snippet: String = e.content.chars().take(90).collect();
            let snippet = snippet.replace('\n', "\\n");
            println!(
                "    {} :: {} `{}` struct_hash={} | {snippet}",
                e.file_path,
                e.entity_type,
                e.name,
                e.structural_hash.as_deref().unwrap_or("<none>"),
            );
        }
        println!();
    }
}
