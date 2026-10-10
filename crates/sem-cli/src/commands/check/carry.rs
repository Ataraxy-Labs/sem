//! The layer every language shares: a whole-project verdict carries over
//! from the base tree when no changed path is one of the checker's inputs.
//!
//! A checker that cannot scope its tool (cargo, Gradle, Maven, dotnet, swift,
//! configured commands) still skips the run entirely when a change cannot
//! reach it: the base passed with the same tool version and configuration,
//! and every changed path is inert (docs, CI config) or a source file of a
//! language this checker does not read. Anything else is an input, so the
//! tool runs in full. The state is valid only at exactly the base tree.

use serde_json::Value;

use super::util;
use super::{Ctx, Mode, Outcome, Verdict};

/// Paths no checker reads.
pub(crate) const INERT: &[&str] = &[
    "**/*.md",
    "**/*.mdx",
    "**/*.rst",
    "**/LICENSE*",
    "**/CHANGELOG*",
    "**/AUTHORS*",
    "**/CODEOWNERS",
    ".github/**",
    ".gitlab-ci.yml",
    ".husky/**",
    ".vscode/**",
    ".idea/**",
    "docs/**",
    ".gitignore",
    ".gitattributes",
    ".editorconfig",
    ".dockerignore",
    ".sem/**",
];

/// Source extensions per language family, used to tell that a changed file
/// belongs to another language's toolchain.
const FAMILIES: &[(&str, &[&str])] = &[
    ("js", &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "vue", "svelte"]),
    ("python", &["py", "pyi", "pyx", "pxd"]),
    ("go", &["go"]),
    ("rust", &["rs"]),
    ("jvm", &["java", "kt", "kts", "scala", "groovy"]),
    ("dotnet", &["cs", "fs", "fsx", "vb", "razor", "cshtml"]),
    ("swift", &["swift"]),
    ("ruby", &["rb", "erb", "rake"]),
    ("php", &["php"]),
    ("dart", &["dart"]),
    ("elixir", &["ex", "exs"]),
];

fn ext(path: &str) -> &str {
    let leaf = path.rsplit('/').next().unwrap_or(path);
    leaf.rsplit_once('.').map(|(_, e)| e).unwrap_or("")
}

/// Is `path` a source file of a family other than `own`? C and C++ sources
/// are never foreign: build systems of every language compile them.
pub(crate) fn foreign(path: &str, own: &[&str]) -> bool {
    let e = ext(path);
    FAMILIES.iter().any(|(fam, exts)| !own.contains(fam) && exts.contains(&e))
}

pub(crate) fn inert(path: &str, extra: &[String]) -> bool {
    INERT.iter().any(|g| util::glob(g, path)) || util::glob_any(extra, path)
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct State {
    pub schema: String,
    pub version: Option<String>,
    pub pass: bool,
}

/// The state `name` recorded at the base tree: (state, where it came from,
/// its digest).
pub(crate) fn base_state(ctx: &Ctx, name: &str, fp: &str) -> Result<(State, String, String), String> {
    let Some((from, p)) = ctx.state_candidates(name, fp, false).into_iter().next() else {
        return Err(format!("no-state: no {name} verdict recorded at the base tree"));
    };
    std::fs::read_to_string(&p)
        .ok()
        .and_then(|t| serde_json::from_str::<State>(&t).ok().map(|s| (s, from, util::digest(&t))))
        .ok_or_else(|| format!("no-state: the base's {name} state could not be read"))
}

/// Which changed paths are inputs, given `is_input`; `None` with reasons
/// when the carry cannot even be considered.
fn inputs_changed(ctx: &Ctx, is_input: &dyn Fn(&str) -> bool) -> Result<Vec<String>, String> {
    match &ctx.changed {
        None => Err("changes unknown: no base tree to compare with".into()),
        Some(c) => Ok(c.iter().filter(|c| is_input(&c.path)).map(|c| c.path.clone()).collect()),
    }
}

/// Try to carry `name`'s base verdict. On success the outcome is complete
/// (pass, incremental, nothing rechecked); otherwise the reasons say why the
/// tool has to run.
pub(crate) fn try_carry(ctx: &Ctx, o: &mut Outcome, fp: &str, is_input: &dyn Fn(&str) -> bool) -> Result<(), Vec<String>> {
    if ctx.full {
        return Err(vec!["full: requested".into()]);
    }
    let (st, from, digest) = base_state(ctx, o.name, fp).map_err(|e| vec![e])?;
    let mut reasons = Vec::new();
    if !st.pass {
        reasons.push("the base did not pass".into());
    }
    if st.version != o.tool_version {
        reasons.push(format!("toolchain: {:?} -> {:?}", st.version, o.tool_version));
    }
    match inputs_changed(ctx, is_input) {
        Err(e) => reasons.push(e),
        Ok(v) => {
            for p in v.iter().take(5) {
                reasons.push(format!("input changed: {p}"));
            }
            if v.len() > 5 {
                reasons.push(format!("... and {} more inputs", v.len() - 5));
            }
        }
    }
    if !reasons.is_empty() {
        return Err(reasons);
    }
    o.mode = Mode::Incremental;
    o.verdict = Verdict::Pass;
    o.state_from = Some(from);
    o.state_in = Some(digest);
    o.reasons.push("no input changed since the base passed: its verdict carries".into());
    Ok(())
}

/// Record the verdict (of a full run, or a carried or scoped pass) as the
/// state of the checked tree.
pub(crate) fn record(ctx: &Ctx, o: &mut Outcome, fp: &str) {
    if o.verdict == Verdict::Undecided || ctx.args.no_cache {
        return;
    }
    let st = State { schema: format!("sem-check-{}/1", o.name), version: o.tool_version.clone(), pass: o.verdict == Verdict::Pass };
    if let (Ok(scratch), Ok(text)) = (ctx.scratch(), serde_json::to_string(&st)) {
        let p = scratch.path("state.json");
        if std::fs::write(&p, &text).is_ok() && ctx.save_state(o.name, fp, &p).is_some() {
            o.state_out = Some(util::digest(&text));
        }
    }
}

/// The default input rule for a checker of the families `own`: everything
/// that is neither inert nor another language's source, plus the user's
/// `inert` globs from the checker's config section.
pub(crate) fn default_inputs(cfg: &Value, own: &'static [&'static str]) -> impl Fn(&str) -> bool {
    let extra: Vec<String> = cfg["inert"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    move |p: &str| !inert(p, &extra) && !foreign(p, own)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_sources_are_other_families_only() {
        assert!(foreign("web/app.tsx", &["rust"]));
        assert!(foreign("tools/gen.py", &["rust"]));
        assert!(!foreign("src/lib.rs", &["rust"]));
        assert!(!foreign("native/zstd.c", &["rust"]));
        assert!(!foreign("proto/api.proto", &["rust"]));
        assert!(!foreign("Cargo.toml", &["rust"]));
        assert!(foreign("src/Main.kt", &["python"]));
    }

    #[test]
    fn docs_and_ci_are_inert() {
        assert!(inert("README.md", &[]));
        assert!(inert(".github/workflows/ci.yml", &[]));
        assert!(!inert("src/main.rs", &[]));
        assert!(inert("site/index.html", &["site/**".to_string()]));
    }
}
