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
    /// Prove every promise can fail: apply its `mutation`, expect it BROKEN
    /// at the mutated file, restore. Exit 1 if any promise has no mutation,
    /// is broken already, or stays kept under its mutation (a vacuous law)
    Verify(VerifyArgs),
}

#[derive(Args, Debug)]
pub struct VerifyArgs {
    /// Only these promise ids (globs)
    #[arg(long, num_args = 1..)]
    only: Vec<String>,
    /// Output as JSON
    #[arg(long)]
    json: bool,
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

/// A law's `mutation`: the smallest edit that must break it.
///   { "file": "<repo-relative>", "append": "<text>" }          add text at the end
///   { "file": "<repo-relative>", "replace": ["<old>", "<new>"] } first occurrence
///   { "file": "<repo-relative>", "create": "<text>" }          a new file
enum Edit {
    Append(String),
    Replace(String, String),
    Create(String),
}

fn parse_mutation(m: &Value) -> Result<(String, Edit), String> {
    let file = m["file"].as_str().ok_or("mutation needs \"file\"")?.to_string();
    let edit = if let Some(t) = m["append"].as_str() {
        Edit::Append(t.to_string())
    } else if let Some(t) = m["create"].as_str() {
        Edit::Create(t.to_string())
    } else if let Some([a, b]) = m["replace"].as_array().map(Vec::as_slice) {
        Edit::Replace(a.as_str().ok_or("replace: strings")?.to_string(), b.as_str().ok_or("replace: strings")?.to_string())
    } else {
        return Err("mutation needs one of \"append\", \"replace\": [old, new], \"create\"".into());
    };
    Ok((file, edit))
}

/// Restores a mutated file's original bytes (or removes a created one) on drop.
struct Restore {
    path: PathBuf,
    original: Option<Vec<u8>>,
}

impl Drop for Restore {
    fn drop(&mut self) {
        match &self.original {
            Some(b) => {
                let _ = std::fs::write(&self.path, b);
            }
            None => {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

fn apply(root: &Path, file: &str, edit: &Edit) -> Result<Restore, String> {
    let path = root.join(file);
    let original = std::fs::read(&path).ok();
    let text = match (edit, &original) {
        (Edit::Create(t), None) => t.clone(),
        (Edit::Create(_), Some(_)) => return Err(format!("create: {file} already exists")),
        (_, None) => return Err(format!("{file}: no such file")),
        (Edit::Append(t), Some(b)) => format!("{}{t}", String::from_utf8_lossy(b)),
        (Edit::Replace(a, n), Some(b)) => {
            let s = String::from_utf8_lossy(b);
            if !s.contains(a.as_str()) {
                return Err(format!("replace: {a:?} not found in {file}"));
            }
            s.replacen(a.as_str(), n, 1)
        }
    };
    if original.is_none() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
    }
    let guard = Restore { path: path.clone(), original };
    std::fs::write(&path, text).map_err(|e| format!("{file}: {e}"))?;
    Ok(guard)
}

fn verify(root: &Path, laws: &[Value], json_out: bool) -> Result<(), Box<dyn std::error::Error>> {
    let fresh = || Ctx::new(Common::at(&root.to_string_lossy()));
    let before = check(&fresh(), laws, None)?;
    let mut rows = Vec::new();
    for (law, base) in laws.iter().zip(&before) {
        let id = law["id"].as_str().unwrap_or("").to_string();
        let verdict: Result<Value, String> = (|| {
            if base["kept"] != true {
                return Err(format!("already broken ({} violations): a mutation cannot show it can fail", base["violations"]));
            }
            let (file, edit) = parse_mutation(law.get("mutation").ok_or("no mutation: add one that must break this law")?)?;
            let _restore = apply(root, &file, &edit)?;
            let r = check(&fresh(), std::slice::from_ref(law), None).map_err(|e| e.to_string())?;
            let set: BTreeSet<String> = [file.clone()].into_iter().collect();
            let at_file = r[0]["details"].as_array().map(|d| d.iter().filter(|v| super::topology::involves(v, &set)).count()).unwrap_or(0);
            if at_file == 0 {
                return Err(format!("stays kept under its mutation of {file} (vacuous as written)"));
            }
            Ok(json!({ "file": file, "violations": at_file }))
        })();
        rows.push(match verdict {
            Ok(v) => json!({ "id": id, "falsifiable": true, "mutation": v }),
            Err(e) => json!({ "id": id, "falsifiable": false, "reason": e }),
        });
    }
    let bad = rows.iter().filter(|r| r["falsifiable"] == false).count();
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "promises": rows, "unfalsifiable": bad }))?);
    } else {
        for r in &rows {
            match r["falsifiable"].as_bool() {
                Some(true) => println!("FALSIFIABLE {}  (mutation of {} -> {} violation(s))", r["id"].as_str().unwrap_or(""), r["mutation"]["file"].as_str().unwrap_or(""), r["mutation"]["violations"]),
                _ => println!("UNVERIFIED {}  {}", r["id"].as_str().unwrap_or(""), r["reason"].as_str().unwrap_or("")),
            }
        }
    }
    if bad > 0 {
        std::process::exit(1);
    }
    Ok(())
}

pub fn run(cmd: PromisesCmd) -> Result<(), Box<dyn std::error::Error>> {
    let (sel, status) = match cmd {
        PromisesCmd::Check(s) => (s, false),
        PromisesCmd::Status(s) => (s, true),
        PromisesCmd::Verify(v) => {
            let cwd = std::env::current_dir()?;
            let root = super::repo_root_or_cwd(&cwd.to_string_lossy());
            let mut laws = load(&discover(&root))?;
            if !v.only.is_empty() {
                laws.retain(|l| v.only.iter().any(|p| glob::matches(p, l["id"].as_str().unwrap_or(""))));
                if laws.is_empty() {
                    return Err(format!("no promise matches {:?}", v.only).into());
                }
            }
            return verify(&root, &laws, v.json);
        }
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
    if d.get("problem").is_some() {
        return format!("{}:{}:{}  {}  (defined at {})  {}", s("file"), d["line"], d["col"], s("problem"), s("target"), s("text"));
    }
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
