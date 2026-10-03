//! `sem impact --diff <range>`: the impact of a whole change instead of one
//! entity.
//!
//! The range is git's: `A..B` and `A...B` compare two commits; a single ref
//! compares that ref to the working tree (`git diff <ref>`), so
//! `sem impact --diff HEAD` is the impact of the uncommitted change.
//!
//! - With `--tests` in a JS/TS workspace, the answer is the module graph's
//!   affected-test selection over the changed files (`sem graph --modules
//!   affected-tests`), in that command's JSON shape.
//! - Otherwise every changed entity that exists in the working tree gets the
//!   ordinary `sem impact` report (one JSON object per line with `--json`,
//!   each in `sem impact`'s own shape).

use std::path::Path;

use sem_core::git::bridge::GitBridge;
use sem_core::git::types::DiffScope;
use sem_core::model::change::ChangeType;
use sem_core::parser::differ::compute_semantic_diff;

use super::impact::ImpactMode;

/// Changed entities reported one by one; the rest are counted.
const MAX_ENTITIES: usize = 200;

pub struct ImpactDiffOptions {
    pub cwd: String,
    pub range: String,
    pub mode: ImpactMode,
    pub json: bool,
    pub file_exts: Vec<String>,
    pub depth: usize,
    pub no_cache: bool,
    pub no_default_excludes: bool,
}

const JS_TS: [&str; 8] = [".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"];

fn scope_of(root: &Path, range: &str) -> Result<DiffScope, Box<dyn std::error::Error>> {
    if range.contains("..") {
        let (from, to) = super::certify::resolve_range(root, range)?;
        Ok(DiffScope::Range { from, to })
    } else {
        Ok(DiffScope::RefToWorking {
            refspec: range.to_string(),
        })
    }
}

pub fn impact_diff_command(opts: ImpactDiffOptions) -> Result<(), Box<dyn std::error::Error>> {
    let root = super::repo_root_or_cwd(&opts.cwd);
    let scope = scope_of(&root, &opts.range)?;
    let bridge = GitBridge::open(&root)?;
    let file_changes = bridge.get_changed_files(&scope, &[])?;

    if matches!(opts.mode, ImpactMode::Tests) {
        let changed: Vec<String> = file_changes.iter().map(|f| f.file_path.clone()).collect();
        let touches_js = changed.iter().any(|f| JS_TS.iter().any(|e| f.ends_with(e)));
        if touches_js && super::topology::is_js_workspace(&root.to_string_lossy()) {
            return super::topology::print_affected_tests(&root.to_string_lossy(), changed);
        }
    }

    let registry = super::create_registry(&root.to_string_lossy());
    let diff = compute_semantic_diff(&file_changes, &registry, None, None);
    let mut ids: Vec<String> = Vec::new();
    for c in &diff.changes {
        if matches!(c.change_type, ChangeType::Deleted) || ids.contains(&c.entity_id) {
            continue;
        }
        ids.push(c.entity_id.clone());
    }
    if ids.is_empty() {
        if !opts.json {
            println!("No changed entities in {}.", opts.range);
        }
        return Ok(());
    }
    if ids.len() > MAX_ENTITIES {
        eprintln!(
            "sem impact --diff: {} changed entities; reporting the first {MAX_ENTITIES}",
            ids.len()
        );
        ids.truncate(MAX_ENTITIES);
    }
    // One `sem impact --entity-id` per entity, as its own process: an entity
    // the working tree no longer has is refused by that run alone.
    let exe = std::env::current_exe()?;
    let mut missing = 0usize;
    for id in ids {
        let mut cmd = std::process::Command::new(&exe);
        cmd.current_dir(&root).args([
            "impact",
            "--entity-id",
            &id,
            "--depth",
            &opts.depth.to_string(),
        ]);
        match opts.mode {
            ImpactMode::Tests => {
                cmd.arg("--tests");
            }
            ImpactMode::Deps => {
                cmd.arg("--deps");
            }
            ImpactMode::Dependents => {
                cmd.arg("--dependents");
            }
            ImpactMode::All => {}
        }
        if opts.json {
            cmd.arg("--json");
        }
        if !opts.file_exts.is_empty() {
            cmd.arg("--file-exts").args(&opts.file_exts);
        }
        if opts.no_cache {
            cmd.arg("--no-cache");
        }
        if opts.no_default_excludes {
            cmd.arg("--no-default-excludes");
        }
        if !cmd.status()?.success() {
            missing += 1;
        }
    }
    if missing > 0 {
        eprintln!("sem impact --diff: {missing} changed entit{} could not be analyzed in the working tree", if missing == 1 { "y" } else { "ies" });
    }
    Ok(())
}
