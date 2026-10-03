//! `sem promises`: laws kept in `.sem/promises/*.json` (the `topology check`
//! format), each a promise about the codebase that a deterministic check
//! verifies. "Done" means `sem promises check` exits 0.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Args, Subcommand};
use serde_json::{json, Value};

use super::topology::{check, Common, Ctx};
use sem_core::topology::glob;

#[derive(Subcommand, Debug)]
pub enum PromisesCmd {
    /// Check promises: one KEPT / BROKEN line each, then violations; exit 1 if any is broken
    Check(Sel),
    /// Each promise's id, kept/broken and violation count (always exits 0)
    Status(Sel),
}

#[derive(Args, Debug)]
pub struct Sel {
    /// Only report violations in these files (paths relative to cwd, or absolute)
    #[arg(long, num_args = 1.., conflicts_with = "since")]
    changed: Vec<String>,
    /// Only report violations in files changed since this git ref (committed, uncommitted or untracked)
    #[arg(long)]
    since: Option<String>,
    /// Only these promise ids (globs, e.g. `jsx-no-logic/*`); an id matching nothing is an error
    #[arg(long, num_args = 1..)]
    only: Vec<String>,
    /// Output as JSON
    #[arg(long)]
    json: bool,
}

/// Promise files: `<root>/.sem/promises/*.json`, sorted.
pub fn discover(root: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(root.join(".sem/promises"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    v.sort();
    v
}

/// Every law of every promise file; an id-less law is `<file stem>#<n>`.
fn load(files: &[PathBuf]) -> Result<Vec<Value>, Box<dyn std::error::Error>> {
    let mut laws = Vec::new();
    for f in files {
        let spec: Value = serde_json::from_str(&std::fs::read_to_string(f)?).map_err(|e| format!("{}: {e}", f.display()))?;
        let stem = f.file_stem().unwrap_or_default().to_string_lossy();
        for (n, mut law) in spec["laws"].as_array().cloned().unwrap_or_default().into_iter().enumerate() {
            if law["id"].as_str().is_none() {
                law["id"] = json!(format!("{stem}#{}", n + 1));
            }
            laws.push(law);
        }
    }
    Ok(laws)
}

/// Repo-relative files changed since `git_ref`, including untracked ones.
fn changed_since(root: &Path, git_ref: &str) -> Result<BTreeSet<String>, String> {
    let git = |args: &[&str]| -> Result<Vec<String>, String> {
        let o = Command::new("git").arg("-C").arg(root).args(args).output().map_err(|e| e.to_string())?;
        if !o.status.success() {
            return Err(String::from_utf8_lossy(&o.stderr).trim().to_string());
        }
        Ok(String::from_utf8_lossy(&o.stdout).split('\0').filter(|s| !s.is_empty()).map(str::to_string).collect())
    };
    let mut set: BTreeSet<String> = git(&["diff", "--name-only", "-z", git_ref, "--"])?.into_iter().collect();
    set.extend(git(&["ls-files", "--others", "--exclude-standard", "-z"])?);
    Ok(set)
}

pub fn run(cmd: PromisesCmd) -> Result<(), Box<dyn std::error::Error>> {
    let (sel, status) = match cmd {
        PromisesCmd::Check(s) => (s, false),
        PromisesCmd::Status(s) => (s, true),
    };
    let cwd = std::env::current_dir()?;
    let root = super::repo_root_or_cwd(&cwd.to_string_lossy());
    let files = discover(&root);
    let mut laws = load(&files)?;
    if !sel.only.is_empty() {
        for pat in &sel.only {
            if !laws.iter().any(|l| glob::matches(pat, l["id"].as_str().unwrap_or(""))) {
                return Err(format!("no promise matches `{pat}` in {}/.sem/promises", root.display()).into());
            }
        }
        laws.retain(|l| sel.only.iter().any(|p| glob::matches(p, l["id"].as_str().unwrap_or(""))));
    }
    let scope: Option<BTreeSet<String>> = match (&sel.since, sel.changed.is_empty()) {
        (Some(r), _) => Some(changed_since(&root, r)?),
        (None, false) => Some(sel.changed.iter().map(|p| super::normalize_repo_relative_path(&cwd, &root, p)).collect()),
        (None, true) => None,
    };
    let ctx = Ctx::new(Common::at(&root.to_string_lossy()));
    let mut results = check(&ctx, &laws, scope.as_ref())?;
    let broken = results.iter().filter(|r| r["kept"] == false).count();
    if status {
        for r in &mut results {
            r.as_object_mut().map(|o| o.remove("details"));
        }
    }
    if sel.json {
        println!("{}", serde_json::to_string_pretty(&json!({ "promises": results, "broken": broken }))?);
    } else {
        if files.is_empty() {
            eprintln!("no promises: add laws to {}/.sem/promises/<name>.json", root.display());
        }
        for r in &results {
            let head = if r["kept"] == true { "KEPT".to_string() } else { format!("BROKEN {}", r["violations"]) };
            let promise = r["promise"].as_str().map(|p| format!("  {p}")).unwrap_or_default();
            println!("{head} {}{promise}", r["id"].as_str().unwrap_or(""));
            for d in r["details"].as_array().into_iter().flatten() {
                println!("  {}", detail_line(d));
            }
        }
    }
    if broken > 0 && !status {
        std::process::exit(1);
    }
    Ok(())
}

/// `file:line:col  capture  snippet` for code-shape hits; `file:line  specifier`
/// for imports; the raw violation for graph laws.
fn detail_line(d: &Value) -> String {
    let s = |k: &str| d[k].as_str().map(str::to_string).unwrap_or_else(|| d[k].to_string());
    match (d.get("file"), d.get("capture")) {
        (Some(_), Some(_)) => format!("{}:{}:{}  {}  {}", s("file"), d["line"], d["col"], s("capture"), s("text")),
        (Some(_), None) => format!("{}:{}  {}", s("file"), d["line"], s("specifier")),
        _ => d.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{discover, load};

    #[test]
    fn discovers_json_promise_files_and_names_idless_laws() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".sem/promises");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("b.json"), r#"{"laws":[{"id":"b/one"},{"forbid":{}}]}"#).unwrap();
        std::fs::write(p.join("a.json"), r#"{"laws":[{"id":"a/one"}]}"#).unwrap();
        std::fs::write(p.join("notes.md"), "not a promise").unwrap();
        let files = discover(dir.path());
        assert_eq!(files.iter().map(|f| f.file_name().unwrap().to_str().unwrap()).collect::<Vec<_>>(), ["a.json", "b.json"]);
        let ids: Vec<String> = load(&files).unwrap().iter().map(|l| l["id"].as_str().unwrap().to_string()).collect();
        assert_eq!(ids, ["a/one", "b/one", "b#2"]);
        assert!(discover(&dir.path().join("missing")).is_empty());
    }
}
