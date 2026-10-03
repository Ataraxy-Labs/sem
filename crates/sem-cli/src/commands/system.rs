//! `sem system`: gather what a system runs with (locked dependencies, the
//! standard library, database schema, config and routes, service contracts,
//! a runtime trace) into one graph, and report how much of it is knowable.

use std::path::{Path, PathBuf};

use clap::Subcommand;
use serde_json::{json, Value};

use sem_core::system::locate::{self, RootKind};
use sem_core::system::lockfiles;
use sem_core::system::models::Models;
use sem_core::system::world::{self, Input, LAYERS};

#[derive(Subcommand, Debug)]
pub enum SystemCmd {
    /// Exact dependency versions from every lockfile, and (with --root) whether each is installed
    Deps {
        #[arg(default_value = ".")]
        path: String,
        /// Installed-source roots: KIND=DIR with KIND one of py, npm, go, cargo, pystd, gostd, ruststd
        #[arg(long = "root", num_args = 1..)]
        roots: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// The ecosystem install commands (scripts disabled) that fetch the locked sources; run them in a sandbox
    Fetch {
        #[arg(default_value = ".")]
        path: String,
    },
    /// Build the layered whole-system graph and write summary.json, sites.jsonl, edges.jsonl, world.json
    Build {
        #[arg(default_value = ".")]
        path: String,
        /// Output directory
        #[arg(long)]
        out: PathBuf,
        /// Installed-source roots: KIND=DIR (py, go, cargo, pystd, gostd, ruststd; npm is read by the TS front-end)
        #[arg(long = "root", num_args = 1..)]
        roots: Vec<String>,
        /// TypeScript front-end site files: BASE,DEPS (see scripts/system/ts-sites.mjs)
        #[arg(long)]
        ts_sites: Option<String>,
        /// SQL schema dumps (e.g. pg_dump --schema-only from a live DB)
        #[arg(long = "schema-dump", num_args = 1..)]
        schema_dumps: Vec<PathBuf>,
        /// Observed runtime edges (JSONL, world-relative paths; see scripts/system/)
        #[arg(long)]
        trace: Option<PathBuf>,
        /// Extra boundary models (TOML, the models.toml format)
        #[arg(long)]
        models: Option<PathBuf>,
        /// Skip the value-level data-flow engine (sem dataflow) per layer
        #[arg(long)]
        no_dataflow: bool,
        #[arg(long)]
        json: bool,
    },
}

fn parse_roots(roots: &[String]) -> Result<Vec<(RootKind, PathBuf)>, String> {
    roots
        .iter()
        .map(|r| {
            let (k, d) = r.split_once('=').ok_or_else(|| format!("--root {r}: expected KIND=DIR"))?;
            let kind = RootKind::parse(k).ok_or_else(|| format!("--root {r}: unknown kind {k}"))?;
            let dir = PathBuf::from(d);
            if !dir.is_dir() {
                return Err(format!("--root {r}: not a directory"));
            }
            Ok((kind, dir))
        })
        .collect()
}

fn fetch_command(lockfile: &str) -> Option<&'static str> {
    let name = lockfile.rsplit('/').next().unwrap_or(lockfile);
    Some(match name {
        "package-lock.json" => "npm ci --ignore-scripts --no-audit --no-fund",
        "pnpm-lock.yaml" => "pnpm install --frozen-lockfile --ignore-scripts",
        "yarn.lock" => "yarn install --frozen-lockfile --ignore-scripts",
        "bun.lock" => "bun install --frozen-lockfile --ignore-scripts",
        "uv.lock" => "uv sync --frozen --no-build --all-extras",
        "poetry.lock" => "poetry install --no-root",
        "go.sum" => "go mod download",
        "Cargo.lock" => "cargo fetch --locked",
        n if n.starts_with("requirements") => "pip install --only-binary=:all: -r",
        _ => return None,
    })
}

pub fn run(cmd: SystemCmd) -> Result<(), String> {
    match cmd {
        SystemCmd::Deps { path, roots, json } => {
            let root = Path::new(&path);
            let deps = lockfiles::locked_deps(root);
            let roots = parse_roots(&roots)?;
            let located = locate::locate(&deps, &roots);
            if json {
                println!("{}", serde_json::to_string_pretty(&located).unwrap());
                return Ok(());
            }
            let mut per: std::collections::BTreeMap<(String, String), (usize, usize)> = Default::default();
            for l in &located {
                let e = per.entry((l.dep.ecosystem.as_str().into(), l.dep.lockfile.clone())).or_default();
                e.0 += 1;
                if l.dir.is_some() {
                    e.1 += 1;
                }
            }
            for ((eco, lf), (n, found)) in per {
                if roots.is_empty() {
                    println!("{eco:6} {n:6} locked   {lf}");
                } else {
                    println!("{eco:6} {n:6} locked  {found:6} installed   {lf}");
                }
            }
            Ok(())
        }
        SystemCmd::Fetch { path } => {
            let root = Path::new(&path);
            for lf in lockfiles::find_lockfiles(root) {
                let rel = lf.strip_prefix(root).unwrap_or(&lf).to_string_lossy().to_string();
                let dir = rel.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_else(|| ".".into());
                if let Some(c) = fetch_command(&rel) {
                    let cmd = if c.ends_with(" -r") { format!("{c} {}", rel.rsplit('/').next().unwrap()) } else { c.to_string() };
                    println!("{}", json!({"dir": dir, "lockfile": rel, "cmd": cmd}));
                }
            }
            Ok(())
        }
        SystemCmd::Build { path, out, roots, ts_sites, schema_dumps, trace, models, no_dataflow, json: as_json } => {
            let root = std::fs::canonicalize(&path).map_err(|e| format!("{path}: {e}"))?;
            let extra = match &models {
                Some(p) => Some(std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?),
                None => None,
            };
            let models = Models::load(extra.as_deref())?;
            let ts_sites = match ts_sites {
                Some(s) => {
                    let (a, b) = s.split_once(',').ok_or("--ts-sites: expected BASE,DEPS")?;
                    Some((PathBuf::from(a), PathBuf::from(b)))
                }
                None => None,
            };
            let input = Input {
                root: root.clone(),
                dep_roots: parse_roots(&roots)?,
                ts_sites,
                schema_dumps,
                trace,
                models,
                out: out.clone(),
                dataflow: if no_dataflow {
                    None
                } else {
                    let df_root = root.clone();
                    let df_models = super::arch_diff::load_models(&[&root], &[]).map_err(|e| e.to_string())?;
                    Some(std::sync::Arc::new(move |files: &[String], ents: &[sem_core::model::entity::SemanticEntity]| {
                        // stop the engine itself a little before the layer's budget, so a
                        // slow layer returns a partial (`incomplete`) answer, not a timeout
                        let budget = world::dataflow_budget().mul_f64(0.9);
                        let limits = sem_core::dataflow::Limits { deadline: Some(std::time::Instant::now() + budget), max_rss_bytes: None };
                        let a = super::arch_diff::analyze_tree(&df_root, files, ents, &df_models, limits, None);
                        // compact JSON (no per-function transitive facts, no paths), plus the
                        // escapes' sources, which the layer summary counts per source site
                        let mut j = a.to_json_with(sem_core::dataflow::JsonDetail::Compact);
                        j["escapes"] = serde_json::Value::Array(a.escapes_json(false));
                        j
                    }))
                },
            };
            let registry = super::create_registry(&root.to_string_lossy());
            let s = world::run(&input, &registry)?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&s).unwrap());
                return Ok(());
            }
            println!("{:<10} {:>8} {:>8} {:>9} {:>10} {:>8} {:>9} {:>9}", "layer", "calls", "unk%", "boundary", "unk%", "comb%", "paths", "recall");
            for (i, l) in s.layers.iter().enumerate() {
                let pct = |v: &Value| v.as_f64().map(|x| format!("{:.1}", 100.0 * x)).unwrap_or_else(|| "-".into());
                println!(
                    "{:<10} {:>8} {:>8} {:>9} {:>10} {:>8} {:>9} {:>9}",
                    LAYERS[i],
                    l["call_sites"].to_string(),
                    pct(&l["call_unknown_rate"]),
                    l["boundary_sites"].to_string(),
                    pct(&l["boundary_unknown_rate"]),
                    pct(&l["combined_unknown_rate"]),
                    l["paths"]["pairs"].to_string(),
                    pct(&l["recall"]["repo_repo"]),
                );
            }
            println!("wrote {}", out.display());
            Ok(())
        }
    }
}
