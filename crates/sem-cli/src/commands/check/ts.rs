//! TypeScript: the project's own compiler, its verdict exactly.
//!
//! Backends:
//! - `tsc` (the project's `typescript` through its API, `helpers/ts.cjs`):
//!   incremental with early cutoff by public interface when a state is
//!   available and nothing forces the full check; full otherwise.
//! - `tsgo` (`@typescript/native-preview`): always the full check — at native
//!   speed the full check is the fast path. Chosen by `auto` only when the
//!   project itself uses tsgo (a package.json script runs it, or
//!   `.sem/check.json` says so); otherwise its verdict is tsgo's, not the
//!   project's tsc's, and the result says so.

use std::path::Path;

use serde_json::{json, Value};

use super::util::{self, Ran};
use super::{Ctx, Mode, Outcome, Verdict};

const HELPER: &str = include_str!("helpers/ts.cjs");

fn project(ctx: &Ctx) -> String {
    ctx.args
        .project
        .clone()
        .or_else(|| ctx.cfg("ts").get("project").and_then(Value::as_str).map(String::from))
        .unwrap_or_else(|| "tsconfig.json".to_string())
}

/// Does a package.json script run tsgo?
fn project_uses_tsgo(root: &Path) -> bool {
    let Ok(t) = std::fs::read_to_string(root.join("package.json")) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<Value>(&t) else {
        return false;
    };
    v["scripts"]
        .as_object()
        .is_some_and(|s| s.values().any(|c| c.as_str().is_some_and(|c| c.split(|ch: char| !ch.is_alphanumeric()).any(|w| w == "tsgo"))))
}

pub(crate) fn run(ctx: &Ctx) -> Outcome {
    let project = project(ctx);
    if !ctx.root.join(&project).exists() {
        return Outcome::undecided("ts", "tsc", format!("no {project} in {}", ctx.root.display()));
    }
    let configured = ctx.cfg("ts").get("backend").and_then(Value::as_str).map(String::from);
    let backend = match ctx.args.ts_backend.as_str() {
        "auto" => configured.unwrap_or_else(|| {
            if util::node_bin(&ctx.root, "tsgo").is_some() && project_uses_tsgo(&ctx.root) {
                "tsgo".into()
            } else {
                "tsc".into()
            }
        }),
        b => b.to_string(),
    };
    match backend.as_str() {
        "tsgo" => tsgo(ctx, &project),
        "tsc" => tsc(ctx, &project),
        b => Outcome::undecided("ts", b, format!("unknown TypeScript backend `{b}` (auto, tsc, tsgo)")),
    }
}

fn tsc(ctx: &Ctx, project: &str) -> Outcome {
    let mut o = Outcome::new("ts", "tsc");
    let helper = match util::helper(ctx.store.as_ref().map(|s| s.dir.as_path()), "ts", HELPER) {
        Ok(p) => p,
        Err(e) => return Outcome::undecided("ts", "tsc", format!("could not write the TypeScript helper: {e}")),
    };
    let scratch = match ctx.scratch() {
        Ok(s) => s,
        Err(e) => return Outcome::undecided("ts", "tsc", format!("no scratch directory: {e}")),
    };
    let fp = util::fingerprint(&["ts", "tsc", project]);
    let out = scratch.path("result.json");
    let state_out = scratch.path("state.json.gz");
    let mut cmd = std::process::Command::new(util::node());
    cmd.arg(&helper)
        .arg("--project")
        .arg(project)
        .arg("--out")
        .arg(&out)
        .arg("--state-out")
        .arg(&state_out)
        .current_dir(&ctx.root);
    if ctx.full {
        cmd.arg("--full");
    } else if let Some((from, p)) = ctx.state_candidates("ts", &fp, true).into_iter().next() {
        cmd.arg("--state-in").arg(p);
        o.state_from = Some(from);
    }
    let ran = match util::run(cmd, ctx.timeout) {
        Ok(r) => r,
        Err(e) => return Outcome::undecided("ts", "tsc", e),
    };
    let r: Value = match std::fs::read_to_string(&out).ok().and_then(|t| serde_json::from_str(&t).ok()) {
        Some(v) => v,
        None => {
            let mut u = Outcome::undecided("ts", "tsc", "the TypeScript helper produced no result");
            u.diagnostics = ran.tail(40);
            return u;
        }
    };
    if let Some(e) = r["error"].as_str() {
        return Outcome::undecided("ts", "tsc", e);
    }
    o.tool_version = r["tsVersion"].as_str().map(String::from);
    o.mode = if r["mode"] == "incremental" { Mode::Incremental } else { Mode::Full };
    o.reasons = strings(&r["reasons"]);
    o.rechecked = strings(&r["recheck"]);
    o.diagnostics = strings(&r["diagnostics"]);
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
        if !ctx.args.no_cache && state_out.exists() && ctx.save_state("ts", &fp, &state_out).is_some() {
            o.state_out = Some(d.to_string());
        }
    }
    o.extra = json!({
        "backend": "tsc-api",
        "project": project,
        "node": r["node"],
        "programFiles": r["programFiles"],
        "externalFiles": r["externalFiles"],
        "interfaceChanged": r["interfaceChanged"],
        "timings": r["timings"],
    });
    o
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

/// tsc-format output (`--pretty false`) as diagnostics: a line that does not
/// start with whitespace opens one; indented lines continue its message chain.
pub(crate) fn parse_tsc_output(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(last) = out.last_mut() {
                last.push('\n');
                last.push_str(line);
                continue;
            }
        }
        out.push(line.to_string());
    }
    out.sort();
    out
}

fn is_error(d: &str) -> bool {
    let first = d.lines().next().unwrap_or("");
    first.contains(": error TS") || first.starts_with("error TS")
}

fn tsgo(ctx: &Ctx, project: &str) -> Outcome {
    let bin = util::node_bin(&ctx.root, "tsgo")
        .map(|p| p.to_string_lossy().to_string())
        .or_else(|| std::env::var("SEM_CHECK_TSGO").ok())
        .unwrap_or_else(|| "tsgo".to_string());
    let mut o = Outcome::new("ts", "tsgo");
    o.mode = Mode::Full;
    let own = util::node_bin(&ctx.root, "tsgo").is_some() && project_uses_tsgo(&ctx.root);
    o.reasons.push("tsgo: the full check at native speed".into());
    if !own {
        o.reasons.push(
            "tsgo is not this project's own type checker: the verdict is tsgo's, which can differ from the project's pinned typescript"
                .into(),
        );
    }
    let version = std::process::Command::new(&bin)
        .arg("--version")
        .current_dir(&ctx.root)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().trim_start_matches("Version ").to_string());
    if version.is_none() {
        return Outcome::undecided("ts", "tsgo", format!("tsgo not found ({bin}); install @typescript/native-preview"));
    }
    o.tool_version = version;
    let scratch = match ctx.scratch() {
        Ok(s) => s,
        Err(e) => return Outcome::undecided("ts", "tsgo", format!("no scratch directory: {e}")),
    };
    // never write outputs into the tree: emit (if the config emits) goes to scratch
    let mut cmd = std::process::Command::new(&bin);
    cmd.args(["-p", project, "--pretty", "false", "--outDir"])
        .arg(scratch.path("out"))
        .arg("--tsBuildInfoFile")
        .arg(scratch.path("tsbuildinfo"))
        .current_dir(&ctx.root);
    let ran: Ran = match util::run(cmd, ctx.timeout) {
        Ok(r) => r,
        Err(e) => return Outcome::undecided("ts", "tsgo", e),
    };
    o.diagnostics = parse_tsc_output(&ran.stdout);
    o.errors = o.diagnostics.iter().filter(|d| is_error(d)).count();
    o.verdict = if o.errors > 0 {
        Verdict::Fail
    } else if ran.ok() {
        Verdict::Pass
    } else {
        o.diagnostics.extend(ran.tail(20));
        Verdict::Undecided
    };
    o.extra = json!({ "backend": "tsgo", "project": project, "binary": bin, "exitCode": ran.status.code() });
    o
}

#[cfg(test)]
mod tests {
    use super::parse_tsc_output;

    #[test]
    fn continuation_lines_join_their_diagnostic() {
        let out = "b.ts(1,1): error TS2322: Type 'x'.\n  Detail.\na.ts(2,3): error TS1: y\n";
        assert_eq!(
            parse_tsc_output(out),
            vec!["a.ts(2,3): error TS1: y".to_string(), "b.ts(1,1): error TS2322: Type 'x'.\n  Detail.".to_string()]
        );
    }
}
