//! Checkers for any language: Go (vet + test, scoped to affected packages
//! when exact), Cargo (full: cargo's own fingerprints already rebuild only
//! what changed), and arbitrary full commands from `.sem/check.json`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{json, Value};

use super::util;
use super::{Ctx, Mode, Outcome, Verdict};

fn strs(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// Run `steps` (shell commands) in order; the verdict is pass iff all exit 0.
fn run_steps(ctx: &Ctx, o: &mut Outcome, steps: &[String]) {
    let mut ran_steps = Vec::new();
    for s in steps {
        match util::run(util::sh(&ctx.root, s), ctx.timeout) {
            Ok(r) => {
                ran_steps.push(json!({ "command": s, "exitCode": r.status.code() }));
                if !r.ok() {
                    o.verdict = Verdict::Fail;
                    o.errors += 1;
                    o.diagnostics.push(format!("`{s}` exited {}:", r.status.code().map_or("on a signal".into(), |c| c.to_string())));
                    o.diagnostics.extend(r.tail(200));
                }
            }
            Err(e) => {
                o.verdict = Verdict::Undecided;
                o.diagnostics.push(e);
                o.extra["steps"] = json!(ran_steps);
                return;
            }
        }
    }
    if o.verdict != Verdict::Fail {
        o.verdict = Verdict::Pass;
    }
    o.extra["steps"] = json!(ran_steps);
}

pub(crate) fn commands(ctx: &Ctx) -> Outcome {
    let list = ctx.config.get("commands").and_then(Value::as_array).cloned().unwrap_or_default();
    if list.is_empty() {
        return Outcome::undecided("cmd", "sh", "no \"commands\" in .sem/check.json");
    }
    let mut o = Outcome::new("cmd", "sh");
    o.mode = Mode::Full;
    o.reasons.push("configured commands run in full".into());
    let steps: Vec<String> = list
        .iter()
        .filter_map(|c| c.as_str().map(String::from).or_else(|| c.get("run").and_then(Value::as_str).map(String::from)))
        .collect();
    o.extra = json!({});
    run_steps(ctx, &mut o, &steps);
    o
}

pub(crate) fn cargo(ctx: &Ctx) -> Outcome {
    let cfg = ctx.cfg("cargo");
    let mut o = Outcome::new("cargo", "cargo");
    o.tool_version = std::process::Command::new("cargo")
        .arg("--version")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    o.mode = Mode::Full;
    o.reasons.push(
        "cargo: full workspace check — cargo's own fingerprints recompile only crates a change reaches; checking a subset with -p would change feature unification, so its verdict could differ"
            .into(),
    );
    let steps = {
        let s = strs(&cfg["commands"]);
        if s.is_empty() {
            vec!["cargo check --workspace --all-targets --quiet --message-format short".to_string()]
        } else {
            s
        }
    };
    o.extra = json!({});
    run_steps(ctx, &mut o, &steps);
    o
}

#[derive(serde::Serialize, serde::Deserialize)]
struct GoState {
    schema: String,
    version: Option<String>,
    pass: bool,
    /// import path -> module packages importing it (incl. tests)
    importers: BTreeMap<String, Vec<String>>,
    /// package dir (repo-relative) -> import path
    dirs: BTreeMap<String, String>,
}

struct GoPkgs {
    importers: BTreeMap<String, Vec<String>>,
    dirs: BTreeMap<String, String>,
}

fn go_list(ctx: &Ctx) -> Result<GoPkgs, String> {
    let mut c = std::process::Command::new("go");
    c.args(["list", "-e", "-f", "{{.ImportPath}}\t{{.Dir}}\t{{join .Imports \" \"}} {{join .TestImports \" \"}} {{join .XTestImports \" \"}}", "./..."])
        .current_dir(&ctx.root);
    let r = util::run(c, ctx.timeout)?;
    if !r.ok() {
        return Err(format!("go list failed: {}", r.tail(5).join(" ")));
    }
    let mut importers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut dirs = BTreeMap::new();
    let mut own = BTreeSet::new();
    let mut rows = Vec::new();
    for line in r.stdout.lines() {
        let mut it = line.split('\t');
        let (Some(ip), Some(dir), imps) = (it.next(), it.next(), it.next().unwrap_or("")) else { continue };
        let rel = Path::new(dir)
            .strip_prefix(&ctx.root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        dirs.insert(rel, ip.to_string());
        own.insert(ip.to_string());
        rows.push((ip.to_string(), imps.split_whitespace().map(String::from).collect::<Vec<_>>()));
    }
    for (ip, imps) in rows {
        for i in imps {
            if own.contains(&i) && i != ip {
                importers.entry(i).or_default().insert(ip.clone());
            }
        }
    }
    Ok(GoPkgs { importers: importers.into_iter().map(|(k, v)| (k, v.into_iter().collect())).collect(), dirs })
}

pub(crate) fn go(ctx: &Ctx) -> Outcome {
    let cfg = ctx.cfg("go");
    let mut o = Outcome::new("go", "go");
    o.tool_version = std::process::Command::new("go")
        .arg("version")
        .current_dir(&ctx.root)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let with_tests = cfg.get("test").and_then(Value::as_bool).unwrap_or(true);
    let fp = util::fingerprint(&["go", &with_tests.to_string()]);
    let pkgs = match go_list(ctx) {
        Ok(p) => p,
        Err(e) => return Outcome::undecided("go", "go", e),
    };
    let mut reasons = Vec::new();
    let mut base: Option<(GoState, String)> = None;
    if ctx.full {
        reasons.push("full: requested".to_string());
    } else {
        match ctx.state_candidates("go", &fp, false).into_iter().next() {
            Some((from, p)) => match std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str::<GoState>(&t).ok().map(|s| (s, util::digest(&t)))) {
                Some(s) => {
                    o.state_from = Some(from);
                    base = Some(s);
                }
                None => reasons.push("no-state: the base's go state could not be read".into()),
            },
            None => reasons.push("no-state: no go verdict recorded at the base tree".into()),
        }
    }
    let mut affected: BTreeSet<String> = BTreeSet::new();
    if let Some((st, _)) = &base {
        if !st.pass {
            reasons.push("the base did not pass: its failures may be anywhere".into());
        }
        if st.version != o.tool_version {
            reasons.push(format!("toolchain: {:?} -> {:?}", st.version, o.tool_version));
        }
        match &ctx.changed {
            None => reasons.push("changes unknown: no base tree to compare with".into()),
            Some(changed) => {
                let inert = ["**/*.md", "**/LICENSE*", ".github/**", "docs/**", ".gitignore", ".gitattributes"].map(String::from);
                for c in changed {
                    let p = c.path.as_str();
                    let leaf = p.rsplit('/').next().unwrap_or(p);
                    if matches!(leaf, "go.mod" | "go.sum" | "go.work" | "go.work.sum" | ".go-version") {
                        reasons.push(format!("module input: {p}"));
                        continue;
                    }
                    // the deepest package directory holding the path (code, embeds, testdata)
                    let mut dir = Path::new(p).parent();
                    let mut owner = None;
                    while let Some(d) = dir {
                        let ds = d.to_string_lossy().to_string();
                        if let Some(ip) = pkgs.dirs.get(&ds).or_else(|| st.dirs.get(&ds)) {
                            owner = Some(ip.clone());
                            break;
                        }
                        if ds.is_empty() {
                            break;
                        }
                        dir = d.parent();
                    }
                    match owner {
                        Some(ip) => {
                            affected.insert(ip);
                        }
                        None if util::glob_any(&inert, p) => {}
                        None => reasons.push(format!("unmatched: {p} belongs to no package")),
                    }
                }
                // reverse closure over importers, now and at the base
                let mut stack: Vec<String> = affected.iter().cloned().collect();
                while let Some(x) = stack.pop() {
                    for i in pkgs.importers.get(&x).into_iter().flatten().chain(st.importers.get(&x).into_iter().flatten()) {
                        if affected.insert(i.clone()) {
                            stack.push(i.clone());
                        }
                    }
                }
                // only packages that still exist can be checked
                let live: BTreeSet<&String> = pkgs.dirs.values().collect();
                affected.retain(|p| live.contains(p));
            }
        }
    }
    let steps: Vec<String> = if reasons.is_empty() {
        o.mode = Mode::Incremental;
        o.state_in = base.as_ref().map(|b| b.1.clone());
        o.reasons.push(if affected.is_empty() {
            "no package can be affected: the base's pass carries".into()
        } else {
            "affected packages: changed packages and every package importing them".into()
        });
        o.rechecked = affected.iter().cloned().collect();
        if affected.is_empty() {
            Vec::new()
        } else {
            let list = affected.iter().map(|p| format!("'{p}'")).collect::<Vec<_>>().join(" ");
            let mut v = vec![format!("go vet {list}")];
            if with_tests {
                v.push(format!("go test {list}"));
            }
            v
        }
    } else {
        o.mode = Mode::Full;
        o.reasons = reasons;
        o.rechecked = pkgs.dirs.values().cloned().collect();
        let mut v = vec!["go vet ./...".to_string()];
        if with_tests {
            v.push("go test ./...".to_string());
        }
        v
    };
    o.extra = json!({});
    if steps.is_empty() {
        o.verdict = Verdict::Pass;
    } else {
        run_steps(ctx, &mut o, &steps);
    }
    if o.verdict != Verdict::Undecided && !ctx.args.no_cache {
        let st = GoState {
            schema: "sem-check-go/1".into(),
            version: o.tool_version.clone(),
            pass: o.verdict == Verdict::Pass,
            importers: pkgs.importers,
            dirs: pkgs.dirs,
        };
        if let (Ok(scratch), Ok(text)) = (ctx.scratch(), serde_json::to_string(&st)) {
            let p = scratch.path("state.json");
            if std::fs::write(&p, &text).is_ok() && ctx.save_state("go", &fp, &p).is_some() {
                o.state_out = Some(util::digest(&text));
            }
        }
    }
    o
}
