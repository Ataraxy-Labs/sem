//! C and C++: every translation unit in `compile_commands.json` compiles
//! (`-fsyntax-only` with its own flags), rechecking only the units a change
//! can reach.
//!
//! A unit's diagnostics depend only on its source, the files it includes
//! (transitively), and its command. So the incremental verdict recompiles the
//! units whose include closure mentions a changed file, plus units whose
//! command names one (`-include`, response files), plus new units, and
//! carries every other unit's errors over from the state recorded at the base
//! tree. It runs every unit when there is no such state, the compile database
//! or a compiler's version changed. Headers generated outside the repository
//! are not tracked: regenerate them, then run with `--full`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use serde_json::{json, Value};

use super::deps;
use super::tree;
use super::util;
use super::{Ctx, Mode, Outcome, Verdict};

const C_EXTS: &[&str] = &["c", "cc", "cpp", "cxx", "c++", "m", "mm", "h", "hh", "hpp", "hxx", "h++", "inc", "ipp", "tpp", "inl", "def", "cppm", "ixx"];

#[derive(Clone)]
struct Unit {
    /// repo-relative source path
    file: String,
    /// the file plus its command: one file can be compiled several ways
    key: String,
    dir: PathBuf,
    args: Vec<String>,
}

fn db_path(ctx: &Ctx) -> Option<PathBuf> {
    if let Some(p) = ctx.cfg("cpp")["compileCommands"].as_str() {
        return Some(ctx.root.join(p));
    }
    ["compile_commands.json", "build/compile_commands.json"].iter().map(|p| ctx.root.join(p)).find(|p| p.exists())
}

pub(crate) fn detect(root: &Path, config: &Value) -> bool {
    config.pointer("/cpp/compileCommands").is_some() || root.join("compile_commands.json").exists() || root.join("build/compile_commands.json").exists()
}

/// POSIX-shell word splitting, enough for compile commands.
fn split(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut chars = cmd.chars();
    let (mut sq, mut dq) = (false, false);
    while let Some(c) = chars.next() {
        match c {
            '\'' if !dq => {
                sq = !sq;
                have = true;
            }
            '"' if !sq => {
                dq = !dq;
                have = true;
            }
            '\\' if !sq => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    have = true;
                }
            }
            c if c.is_whitespace() && !sq && !dq => {
                if have {
                    out.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            c => {
                cur.push(c);
                have = true;
            }
        }
    }
    if have {
        out.push(cur);
    }
    out
}

/// The unit's own command, made to check syntax and types only.
fn syntax_only(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if matches!(a.as_str(), "-o" | "-MF" | "-MT" | "-MQ" | "-MJ") {
            i += 2;
            continue;
        }
        if matches!(a.as_str(), "-c" | "-MD" | "-MMD" | "-M" | "-MM" | "-MP") {
            i += 1;
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    out.push("-fsyntax-only".into());
    out
}

fn load(ctx: &Ctx, db: &Path) -> Result<(Vec<Unit>, String), String> {
    let text = std::fs::read_to_string(db).map_err(|e| format!("{}: {e}", db.display()))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", db.display()))?;
    let root = ctx.root.canonicalize().unwrap_or_else(|_| ctx.root.clone());
    let mut units = Vec::new();
    for e in v.as_array().into_iter().flatten() {
        let dir = PathBuf::from(e["directory"].as_str().unwrap_or("."));
        let dir = if dir.is_absolute() { dir } else { db.parent().unwrap_or(&ctx.root).join(dir) };
        let args: Vec<String> = match e["arguments"].as_array() {
            Some(a) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
            None => split(e["command"].as_str().unwrap_or("")),
        };
        let Some(f) = e["file"].as_str() else { continue };
        let abs = if Path::new(f).is_absolute() { PathBuf::from(f) } else { dir.join(f) };
        let abs = abs.canonicalize().unwrap_or(abs);
        let Ok(rel) = abs.strip_prefix(&root) else { continue };
        if args.is_empty() {
            continue;
        }
        let file = rel.to_string_lossy().replace('\\', "/");
        let key = format!("{file}#{}", util::fingerprint(&args.iter().map(String::as_str).collect::<Vec<_>>()));
        units.push(Unit { file, key, dir, args });
    }
    Ok((units, util::digest(&text)))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct State {
    schema: String,
    version: Option<String>,
    db: String,
    /// every unit (file#command) -> its errors (empty: compiled cleanly)
    errors: BTreeMap<String, Vec<String>>,
}

fn compile(ctx: &Ctx, u: &Unit) -> Result<Vec<String>, String> {
    let a = syntax_only(&u.args);
    let mut c = std::process::Command::new(&a[0]);
    c.args(&a[1..]).current_dir(&u.dir);
    let r = util::run(c, ctx.timeout)?;
    if r.ok() {
        return Ok(Vec::new());
    }
    let mut errs: Vec<String> = r.stderr.lines().filter(|l| l.contains("error:")).map(String::from).collect();
    if errs.is_empty() {
        errs = r.tail(5);
    }
    Ok(errs)
}

fn compiler_versions(units: &[Unit]) -> Option<String> {
    let mut seen = BTreeSet::new();
    for u in units {
        seen.insert(u.args[0].clone());
    }
    let v: Vec<String> = seen
        .iter()
        .map(|c| {
            std::process::Command::new(c)
                .arg("--version")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("").to_string())
                .unwrap_or_else(|| format!("{c}: not runnable"))
        })
        .collect();
    (!v.is_empty()).then(|| v.join("; "))
}

pub(crate) fn run(ctx: &Ctx) -> Outcome {
    let Some(db) = db_path(ctx) else {
        return Outcome::undecided("cpp", "clang", "no compile_commands.json (set cpp.compileCommands in .sem/check.json)");
    };
    let (units, db_digest) = match load(ctx, &db) {
        Ok(x) => x,
        Err(e) => return Outcome::undecided("cpp", "clang", e),
    };
    let tool = units.first().map(|u| u.args[0].rsplit('/').next().unwrap_or("cc").to_string()).unwrap_or_else(|| "cc".into());
    let mut o = Outcome::new("cpp", &tool);
    o.tool_version = compiler_versions(&units);
    let fp = util::fingerprint(&["cpp", &db.to_string_lossy()]);

    let mut reasons = Vec::new();
    let mut base: Option<(State, String, String)> = None;
    if ctx.full {
        reasons.push("full: requested".to_string());
    } else {
        match ctx.state_candidates("cpp", &fp, false).into_iter().next() {
            Some((from, p)) => match std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str::<State>(&t).ok().map(|s| (s, util::digest(&t)))) {
                Some((s, d)) => base = Some((s, from, d)),
                None => reasons.push("no-state: the base's cpp state could not be read".into()),
            },
            None => reasons.push("no-state: no cpp verdict recorded at the base tree".into()),
        }
    }
    let mut affected: BTreeSet<String> = BTreeSet::new();
    if let Some((st, _, _)) = &base {
        if st.version != o.tool_version {
            reasons.push(format!("toolchain: {:?} -> {:?}", st.version, o.tool_version));
        }
        if st.db != db_digest {
            reasons.push("compile database: compile_commands.json changed since the base".into());
        }
        let files = ctx.head.as_ref().ok_or_else(|| "no tree id for the working tree".to_string()).and_then(|h| tree::files(&ctx.root, &h.tree));
        match (&ctx.changed, files) {
            (None, _) => reasons.push("changes unknown: no base tree to compare with".into()),
            (_, Err(e)) => reasons.push(e),
            (Some(ch), Ok(files)) => {
                let ch: Vec<(char, String)> = ch.iter().map(|c| (c.status, c.path.clone())).collect();
                let mut cfiles: BTreeSet<String> =
                    files.into_iter().filter(|f| C_EXTS.contains(&f.rsplit_once('.').map(|(_, e)| e).unwrap_or(""))).collect();
                cfiles.extend(units.iter().map(|u| u.file.clone()));
                let cfiles: Vec<String> = cfiles.into_iter().collect();
                let reach = deps::closure(&ctx.root, &cfiles, &ch, &deps::C);
                let leaves: Vec<String> = ch.iter().map(|(_, p)| p.rsplit('/').next().unwrap_or(p).to_string()).collect();
                for u in &units {
                    if reach.contains(&u.file) || !st.errors.contains_key(&u.key) || u.args.iter().any(|a| leaves.iter().any(|l| a.contains(l.as_str()))) {
                        affected.insert(u.key.clone());
                    }
                }
            }
        }
    }

    let run_units: Vec<&Unit> = if reasons.is_empty() { units.iter().filter(|u| affected.contains(&u.key)).collect() } else { units.iter().collect() };
    let fresh: Vec<(String, Result<Vec<String>, String>)> = run_units.par_iter().map(|u| (u.key.clone(), compile(ctx, u))).collect();
    let mut errors: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if reasons.is_empty() {
        let (st, from, digest) = base.as_ref().expect("incremental implies a base state");
        o.mode = Mode::Incremental;
        o.state_from = Some(from.clone());
        o.state_in = Some(digest.clone());
        o.reasons.push(if affected.is_empty() {
            "no unit can include what changed: the base's results carry".into()
        } else {
            "recompiled: units whose includes or command can reach a changed file; other units' results carry from the base".into()
        });
        let live: BTreeSet<&String> = units.iter().map(|u| &u.key).collect();
        errors.extend(st.errors.iter().filter(|(f, e)| !e.is_empty() && live.contains(f) && !affected.contains(*f)).map(|(f, e)| (f.clone(), e.clone())));
    } else {
        o.mode = Mode::Full;
        o.reasons = reasons;
    }
    o.rechecked = run_units.iter().map(|u| u.file.clone()).collect();
    for (f, r) in fresh {
        match r {
            Ok(e) if e.is_empty() => {}
            Ok(e) => {
                errors.insert(f, e);
            }
            Err(e) => {
                o.verdict = Verdict::Undecided;
                o.diagnostics.push(format!("{}: {e}", f.split('#').next().unwrap_or(&f)));
                o.extra = json!({ "units": units.len() });
                return o;
            }
        }
    }
    // a header error repeats once per unit that includes it: report it once
    let mut seen = BTreeSet::new();
    o.diagnostics = errors.values().flatten().filter(|d| seen.insert(d.to_string())).cloned().collect();
    o.errors = o.diagnostics.len();
    o.verdict = if errors.is_empty() { Verdict::Pass } else { Verdict::Fail };
    o.extra = json!({ "units": units.len(), "compileCommands": db.to_string_lossy() });
    if !ctx.args.no_cache {
        let mut all = BTreeMap::new();
        for u in &units {
            all.insert(u.key.clone(), errors.get(&u.key).cloned().unwrap_or_default());
        }
        let st = State { schema: "sem-check-cpp/1".into(), version: o.tool_version.clone(), db: db_digest, errors: all };
        if let (Ok(scratch), Ok(text)) = (ctx.scratch(), serde_json::to_string(&st)) {
            let p = scratch.path("state.json");
            if std::fs::write(&p, &text).is_ok() && ctx.save_state("cpp", &fp, &p).is_some() {
                o.state_out = Some(util::digest(&text));
            }
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_like_a_shell() {
        assert_eq!(split(r#"cc -DX="a b" -I'inc dir' -c a.c"#), vec!["cc", "-DX=a b", "-Iinc dir", "-c", "a.c"]);
    }

    #[test]
    fn syntax_only_drops_outputs() {
        let a: Vec<String> = ["cc", "-O2", "-MD", "-MF", "a.d", "-c", "a.c", "-o", "a.o"].iter().map(|s| s.to_string()).collect();
        assert_eq!(syntax_only(&a), vec!["cc", "-O2", "a.c", "-fsyntax-only"]);
    }
}
