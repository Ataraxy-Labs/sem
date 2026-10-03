//! Lint: ESLint (incremental when provably exact, `helpers/eslint.cjs`) or
//! Biome (full: it lints a whole project in about the time it takes to start).
//!
//! ESLint options come from `.sem/check.json` `lint` ({"args": [...]} in the
//! eslint CLI's own words, or "patterns"/"extensions"/"maxWarnings"), else from
//! the package.json script that runs eslint (`--ext`, `--max-warnings`,
//! `--quiet`, `-c`, `--no-eslintrc`, `--report-unused-disable-directives`,
//! paths). A script with any other eslint flag is run as is, in full.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{json, Value};

use super::util;
use super::{Ctx, Mode, Outcome, Verdict};

const HELPER: &str = include_str!("helpers/eslint.cjs");

const ESLINT_CONFIGS: &[&str] = &[
    ".eslintrc",
    ".eslintrc.js",
    ".eslintrc.cjs",
    ".eslintrc.json",
    ".eslintrc.yaml",
    ".eslintrc.yml",
    "eslint.config.js",
    "eslint.config.mjs",
    "eslint.config.cjs",
    "eslint.config.ts",
    "eslint.config.mts",
    "eslint.config.cts",
];

fn package_json(root: &Path) -> Value {
    std::fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

fn has_eslint(root: &Path) -> bool {
    util::node_package(root, "eslint").is_some()
        && (ESLINT_CONFIGS.iter().any(|f| root.join(f).exists()) || package_json(root).get("eslintConfig").is_some())
}

fn has_biome(root: &Path) -> bool {
    (root.join("biome.json").exists() || root.join("biome.jsonc").exists()) && util::node_bin(root, "biome").is_some()
}

pub(crate) fn detect(root: &Path) -> bool {
    has_eslint(root) || has_biome(root)
}

/// The eslint invocation of the project's lint script, as argument tokens.
fn script_args(root: &Path) -> Option<(String, Vec<String>)> {
    let pj = package_json(root);
    let scripts = pj["scripts"].as_object()?;
    let mut names: Vec<&String> = scripts.keys().collect();
    names.sort_by_key(|n| (n.as_str() != "lint", !n.contains("lint"), n.to_string()));
    for n in names {
        let cmd = scripts[n].as_str().unwrap_or("");
        let toks = shell_words(cmd);
        if let Some(i) = toks.iter().position(|t| t == "eslint" || t.ends_with("/eslint")) {
            let rest: Vec<String> = toks[i + 1..]
                .iter()
                .take_while(|t| !matches!(t.as_str(), "&&" | "||" | ";" | "|"))
                .cloned()
                .collect();
            return Some((n.clone(), rest));
        }
    }
    None
}

fn shell_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// eslint CLI args -> helper options, or the first flag we do not model.
fn parse_args(args: &[String]) -> Result<Value, String> {
    let mut o = json!({ "patterns": [], "extensions": [] });
    let mut i = 0;
    let take = |i: &mut usize, a: &str| -> Result<String, String> {
        if let Some((_, v)) = a.split_once('=') {
            return Ok(v.to_string());
        }
        *i += 1;
        args.get(*i).cloned().ok_or_else(|| format!("{a} needs a value"))
    };
    while i < args.len() {
        let a = args[i].as_str();
        let flag = a.split('=').next().unwrap_or(a);
        match flag {
            "--ext" => {
                let v = take(&mut i, a)?;
                for e in v.split(',').filter(|e| !e.is_empty()) {
                    let e = if e.starts_with('.') { e.to_string() } else { format!(".{e}") };
                    o["extensions"].as_array_mut().unwrap().push(json!(e));
                }
            }
            "--max-warnings" => {
                let v = take(&mut i, a)?;
                o["maxWarnings"] = json!(v.parse::<i64>().map_err(|_| format!("--max-warnings {v}"))?);
            }
            "-c" | "--config" => o["configFile"] = json!(take(&mut i, a)?),
            "--no-eslintrc" => o["noEslintrc"] = json!(true),
            "--quiet" => o["quiet"] = json!(true),
            "--report-unused-disable-directives" => o["reportUnusedDisableDirectives"] = json!("error"),
            "--cache" | "--color" | "--no-color" => {}
            "--cache-location" | "--cache-strategy" | "--format" | "-f" | "--output-file" | "-o" => {
                take(&mut i, a)?;
            }
            _ if a.starts_with('-') => return Err(format!("eslint flag {a}")),
            _ => o["patterns"].as_array_mut().unwrap().push(json!(a)),
        }
        i += 1;
    }
    Ok(o)
}

pub(crate) fn run(ctx: &Ctx) -> Outcome {
    let cfg = ctx.cfg("lint");
    let tool = cfg.get("tool").and_then(Value::as_str).map(String::from).unwrap_or_else(|| {
        if has_eslint(&ctx.root) {
            "eslint".into()
        } else if has_biome(&ctx.root) {
            "biome".into()
        } else {
            "eslint".into()
        }
    });
    if let Some(cmd) = cfg.get("command").and_then(Value::as_str) {
        return full_command(ctx, &tool, cmd, "lint.command: the configured lint command runs in full");
    }
    match tool.as_str() {
        "biome" => {
            let bin = util::node_bin(&ctx.root, "biome").map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| "biome".into());
            full_command(
                ctx,
                "biome",
                &format!("'{bin}' lint ."),
                "biome: full (it lints the whole project in well under a second; its project rules see across files)",
            )
        }
        "eslint" => eslint(ctx, &cfg),
        t => Outcome::undecided("lint", t, format!("unknown lint tool `{t}` (eslint, biome)")),
    }
}

fn eslint(ctx: &Ctx, cfg: &Value) -> Outcome {
    let (source, opts) = if let Some(a) = cfg.get("args").and_then(Value::as_array) {
        let args: Vec<String> = a.iter().filter_map(|x| x.as_str().map(String::from)).collect();
        (".sem/check.json lint.args".to_string(), parse_args(&args))
    } else if cfg.get("patterns").is_some() || cfg.get("extensions").is_some() || cfg.get("maxWarnings").is_some() {
        (".sem/check.json lint".to_string(), Ok(cfg.clone()))
    } else if let Some((name, args)) = script_args(&ctx.root) {
        match parse_args(&args) {
            Ok(o) => (format!("package.json script `{name}`"), Ok(o)),
            Err(why) => {
                let script = format!("npm run --silent {name}");
                return full_command(ctx, "eslint", &script, &format!("the `{name}` script uses {why}, which sem check does not model: ran the script, in full"));
            }
        }
    } else {
        ("defaults".to_string(), Ok(json!({ "patterns": ["."] })))
    };
    let mut opts = match opts {
        Ok(o) => o,
        Err(e) => return Outcome::undecided("lint", "eslint", format!("lint options: {e}")),
    };
    // importers, for import-resolving rules
    let g = super::super::topology::import_graph(&ctx.root.to_string_lossy());
    let mut importers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (from, to) in &g.edges {
        importers.entry(g.nodes[*to].0.clone()).or_default().insert(g.nodes[*from].0.clone());
    }
    opts["importers"] = json!(importers);
    opts["unresolved"] = json!(g.unresolved.iter().map(|(a, b)| json!([a, b])).collect::<Vec<_>>());

    let mut o = Outcome::new("lint", "eslint");
    let helper = match util::helper(ctx.store.as_ref().map(|s| s.dir.as_path()), "eslint", HELPER) {
        Ok(p) => p,
        Err(e) => return Outcome::undecided("lint", "eslint", e),
    };
    let scratch = match ctx.scratch() {
        Ok(s) => s,
        Err(e) => return Outcome::undecided("lint", "eslint", e.to_string()),
    };
    let opts_path = scratch.path("opts.json");
    if let Err(e) = std::fs::write(&opts_path, opts.to_string()) {
        return Outcome::undecided("lint", "eslint", e.to_string());
    }
    let fp = util::fingerprint(&["lint", "eslint", &source, &serde_json::to_string(&strip(&opts)).unwrap_or_default()]);
    let out = scratch.path("result.json");
    let state_out = scratch.path("state");
    let mut cmd = std::process::Command::new(util::node());
    cmd.arg(&helper).arg("--opts").arg(&opts_path).arg("--out").arg(&out).arg("--state-out").arg(&state_out).current_dir(&ctx.root);
    if ctx.full {
        cmd.arg("--full");
    } else if let Some((from, p)) = ctx.state_candidates("lint", &fp, true).into_iter().next() {
        cmd.arg("--state-in").arg(p);
        o.state_from = Some(from);
    }
    let ran = match util::run(cmd, ctx.timeout) {
        Ok(r) => r,
        Err(e) => return Outcome::undecided("lint", "eslint", e),
    };
    let r: Value = match std::fs::read_to_string(&out).ok().and_then(|t| serde_json::from_str(&t).ok()) {
        Some(v) => v,
        None => {
            let mut u = Outcome::undecided("lint", "eslint", "the ESLint helper produced no result");
            u.diagnostics = ran.tail(40);
            return u;
        }
    };
    if let Some(e) = r["error"].as_str() {
        return Outcome::undecided("lint", "eslint", e);
    }
    o.tool_version = r["eslintVersion"].as_str().map(String::from);
    o.mode = if r["mode"] == "incremental" { Mode::Incremental } else { Mode::Full };
    o.reasons = strs(&r["reasons"]);
    o.rechecked = strs(&r["relinted"]);
    o.diagnostics = strs(&r["diagnostics"]);
    o.errors = r["errors"].as_u64().unwrap_or(0) as usize;
    o.verdict = match r["verdict"].as_str() {
        Some("pass") => Verdict::Pass,
        Some("fail") => Verdict::Fail,
        _ => Verdict::Undecided,
    };
    o.state_in = r["stateIn"].as_str().map(String::from);
    if o.state_in.is_none() {
        o.state_from = None;
    }
    if let Some(d) = r["stateOut"].as_str() {
        if !ctx.args.no_cache && ctx.save_state("lint", &fp, &state_out).is_some() {
            o.state_out = Some(d.to_string());
        }
    }
    o.extra = json!({
        "options": source, "configSystem": r["configSystem"], "lintedFiles": r["lintedFiles"],
        "warnings": r["warnings"], "maxWarnings": r["maxWarnings"], "timings": r["timings"],
    });
    o
}

fn strip(o: &Value) -> Value {
    let mut o = o.clone();
    if let Some(m) = o.as_object_mut() {
        m.remove("importers");
        m.remove("unresolved");
    }
    o
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// Run a lint command in full: the verdict is its exit status.
fn full_command(ctx: &Ctx, tool: &str, cmd: &str, why: &str) -> Outcome {
    let mut o = Outcome::new("lint", tool);
    o.mode = Mode::Full;
    o.reasons.push(why.to_string());
    match util::run(util::sh(&ctx.root, cmd), ctx.timeout) {
        Ok(r) => {
            o.verdict = if r.ok() { Verdict::Pass } else { Verdict::Fail };
            if !r.ok() {
                o.diagnostics = r.tail(200);
                o.errors = 1;
            }
            o.extra = json!({ "command": cmd, "exitCode": r.status.code() });
        }
        Err(e) => o = Outcome::undecided("lint", tool, e),
    }
    o
}

#[cfg(test)]
mod tests {
    use super::{parse_args, shell_words};

    #[test]
    fn eslint_script_args() {
        let toks = shell_words("eslint --max-warnings=0 --ext .js,.ts,.tsx .");
        let o = parse_args(&toks[1..]).unwrap();
        assert_eq!(o["maxWarnings"], 0);
        assert_eq!(o["extensions"], serde_json::json!([".js", ".ts", ".tsx"]));
        assert_eq!(o["patterns"], serde_json::json!(["."]));
        assert!(parse_args(&shell_words("--rulesdir x .")).is_err());
    }
}
