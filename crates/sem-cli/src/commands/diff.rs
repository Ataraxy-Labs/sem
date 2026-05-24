use std::io::Read;
use std::path::Path;
use std::process;
use std::time::Instant;

use sem_core::git::bridge::GitBridge;
use sem_core::git::jj::maybe_resolve_ref;
use sem_core::git::types::{DiffScope, FileChange};
use sem_core::parser::differ::compute_semantic_diff;

use crate::formatters::{
    json::format_json, markdown::format_markdown, plain::format_plain, terminal::format_terminal,
};
use crate::stats::SemLifetimeStats;

pub struct DiffOptions {
    pub cwd: String,
    pub format: OutputFormat,
    pub staged: bool,
    pub commit: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub stdin: bool,
    pub patch: bool,
    pub verbose: bool,
    pub profile: bool,
    pub file_exts: Vec<String>,
    pub args: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    Terminal,
    Plain,
    Json,
    #[value(alias = "md")]
    Markdown,
}

/// Parsed result of git-diff-style positional arguments
struct ParsedArgs {
    /// The resolved diff scope (None = auto-detect)
    scope: Option<ParsedScope>,
    /// Pathspecs for filtering (after --)
    pathspecs: Vec<String>,
}

enum ParsedScope {
    /// Two files to compare directly
    FileCompare(String, String),
    /// A single ref compared to working tree
    RefToWorking(String),
    /// A range between two refs
    Range(String, String),
    /// A merge-base range (ref1...ref2)
    MergeBaseRange(String, String),
}

/// Split args on "--" separator into (refs_or_files, pathspecs)
fn split_on_separator(args: Vec<String>) -> (Vec<String>, Vec<String>) {
    if let Some(pos) = args.iter().position(|a| a == "--") {
        let mut args = args;
        let pathspecs = args.split_off(pos + 1);
        args.pop(); // remove the "--"
        (args, pathspecs)
    } else {
        (args, vec![])
    }
}

/// Simple glob matching for pathspecs. Supports `*` (any chars except `/`),
/// `**` (any chars including `/`), and `?` (one non-`/` char).
fn glob_match(pattern: &str, path: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = path.chars().collect();
    glob_match_inner(&pat, &text)
}

fn glob_match_inner(pat: &[char], text: &[char]) -> bool {
    if pat.is_empty() {
        return text.is_empty();
    }

    // Handle ** (matches any chars including /)
    if pat.len() >= 2 && pat[0] == '*' && pat[1] == '*' {
        let rest = if pat.len() > 2 && pat[2] == '/' {
            &pat[3..]
        } else {
            &pat[2..]
        };
        // Try matching ** against 0..n chars
        for i in 0..=text.len() {
            if glob_match_inner(rest, &text[i..]) {
                return true;
            }
        }
        return false;
    }

    // Handle * (matches any chars except /)
    if pat[0] == '*' {
        for i in 0..=text.len() {
            if i > 0 && text[i - 1] == '/' {
                break;
            }
            if glob_match_inner(&pat[1..], &text[i..]) {
                return true;
            }
        }
        return false;
    }

    // Handle ? (matches one non-/ char)
    if pat[0] == '?' {
        return !text.is_empty() && text[0] != '/' && glob_match_inner(&pat[1..], &text[1..]);
    }

    // Literal character
    !text.is_empty() && pat[0] == text[0] && glob_match_inner(&pat[1..], &text[1..])
}

/// Check if a file path matches a pathspec (supports prefix matching and basic globs).
fn path_matches_spec(file_path: &str, spec: &str) -> bool {
    if spec.contains('*') || spec.contains('?') || spec.contains('[') {
        glob_match(spec, file_path)
    } else if spec.ends_with('/') {
        file_path.starts_with(spec.trim_end_matches('/'))
    } else {
        file_path == spec || file_path.starts_with(&format!("{spec}/"))
    }
}

fn parse_args(args: Vec<String>, cwd: &str) -> ParsedArgs {
    let (refs, pathspecs) = split_on_separator(args);

    if refs.is_empty() {
        return ParsedArgs {
            scope: None,
            pathspecs,
        };
    }

    if refs.len() == 1 {
        let arg = &refs[0];

        // Check for ... (merge-base) syntax first (before ..)
        if let Some((from, to)) = arg.split_once("...") {
            if !from.is_empty() || !to.is_empty() {
                let from = if from.is_empty() { "HEAD" } else { from };
                let to = if to.is_empty() { "HEAD" } else { to };
                return ParsedArgs {
                    scope: Some(ParsedScope::MergeBaseRange(
                        from.to_string(),
                        to.to_string(),
                    )),
                    pathspecs,
                };
            }
        }

        // Check for .. (range) syntax: rev1..rev2, rev1.., ..rev2
        if let Some((from, to)) = arg.split_once("..") {
            if !from.is_empty() || !to.is_empty() {
                let from = if from.is_empty() { "HEAD" } else { from };
                let to = if to.is_empty() { "HEAD" } else { to };
                return ParsedArgs {
                    scope: Some(ParsedScope::Range(from.to_string(), to.to_string())),
                    pathspecs,
                };
            }
        }

        // If it exists as a file or directory on disk, treat as pathspec
        if Path::new(cwd).join(arg).exists() {
            let mut pathspecs = pathspecs;
            pathspecs.push(arg.clone());
            return ParsedArgs {
                scope: None,
                pathspecs,
            };
        }

        // If the arg contains glob meta-characters, treat as pathspec
        if arg.contains('*') || arg.contains('?') || arg.contains('[') {
            let mut pathspecs = pathspecs;
            pathspecs.push(arg.clone());
            return ParsedArgs {
                scope: None,
                pathspecs,
            };
        }

        // Single ref → compare to working tree
        return ParsedArgs {
            scope: Some(ParsedScope::RefToWorking(arg.clone())),
            pathspecs,
        };
    }

    if refs.len() == 2 {
        let a = &refs[0];
        let b = &refs[1];

        // If both exist as files on disk and no pathspecs, treat as file comparison
        if pathspecs.is_empty() && Path::new(cwd).join(a).exists() && Path::new(cwd).join(b).exists() {
            // But check if they're also valid git refs — prefer ref interpretation
            // Only fall back to file comparison if neither resolves as a ref
            return ParsedArgs {
                scope: Some(ParsedScope::FileCompare(a.clone(), b.clone())),
                pathspecs,
            };
        }

        // Two refs → range
        return ParsedArgs {
            scope: Some(ParsedScope::Range(a.clone(), b.clone())),
            pathspecs,
        };
    }

    // Git external diff protocol: path old-file old-hex old-mode new-file new-hex new-mode
    // When sem is set as diff.external, git passes 7 positional args per file.
    if refs.len() == 7 {
        return ParsedArgs {
            scope: Some(ParsedScope::FileCompare(refs[1].clone(), refs[4].clone())),
            pathspecs,
        };
    }

    eprintln!("\x1b[31mError: too many positional arguments. Use -- to separate pathspecs.\x1b[0m");
    process::exit(1);
}

/// Parse a unified diff (e.g. from `git diff`) into FileChange entries.
/// Uses blob SHAs from `index` lines to retrieve full file contents via `git show`.
fn parse_unified_diff(patch: &str, cwd: &str) -> Vec<FileChange> {
    use sem_core::git::types::FileStatus;

    struct PatchEntry {
        file_path: String,
        old_file_path: Option<String>,
        status: FileStatus,
        old_sha: Option<String>,
        new_sha: Option<String>,
    }

    let mut entries: Vec<PatchEntry> = Vec::new();
    let mut current: Option<PatchEntry> = None;

    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            // Flush previous entry
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            // Parse "a/path b/path" — the b-side path is after the last " b/"
            let file_path = if let Some(pos) = rest.rfind(" b/") {
                rest[pos + 3..].to_string()
            } else {
                rest.to_string()
            };
            current = Some(PatchEntry {
                file_path,
                old_file_path: None,
                status: FileStatus::Modified,
                old_sha: None,
                new_sha: None,
            });
        } else if let Some(ref mut entry) = current {
            if line.starts_with("new file mode") {
                entry.status = FileStatus::Added;
            } else if line.starts_with("deleted file mode") {
                entry.status = FileStatus::Deleted;
            } else if let Some(rest) = line.strip_prefix("rename from ") {
                entry.old_file_path = Some(rest.to_string());
                entry.status = FileStatus::Renamed;
            } else if let Some(rest) = line.strip_prefix("rename to ") {
                entry.file_path = rest.to_string();
            } else if let Some(rest) = line.strip_prefix("index ") {
                // "index abc123..def456 100644" or "index abc123..def456"
                let shas_part = rest.split_whitespace().next().unwrap_or("");
                if let Some((old, new)) = shas_part.split_once("..") {
                    if old != "0000000" && !old.chars().all(|c| c == '0') {
                        entry.old_sha = Some(old.to_string());
                    }
                    if new != "0000000" && !new.chars().all(|c| c == '0') {
                        entry.new_sha = Some(new.to_string());
                    }
                }
            }
        }
    }
    if let Some(entry) = current.take() {
        entries.push(entry);
    }

    // Resolve blob contents via git show
    let git_show = |sha: &str| -> Option<String> {
        let output = process::Command::new("git")
            .args(["show", sha])
            .current_dir(cwd)
            .output()
            .ok()?;
        if output.status.success() {
            String::from_utf8(output.stdout).ok()
        } else {
            None
        }
    };

    entries
        .into_iter()
        .map(|e| {
            let before_content = e.old_sha.as_deref().and_then(&git_show);
            let mut after_content = e.new_sha.as_deref().and_then(&git_show);

            // Fallback: if git show fails for the new SHA (e.g. unstaged working
            // tree changes where the blob doesn't exist yet), read from disk.
            if after_content.is_none() && e.new_sha.is_some() {
                let file = Path::new(cwd).join(&e.file_path);
                after_content = std::fs::read_to_string(&file).ok();
            }

            if before_content.is_none() && after_content.is_none() {
                eprintln!(
                    "\x1b[33mwarning:\x1b[0m could not resolve contents for \x1b[1m{}\x1b[0m. \
                     Try running from inside the repo, or use \x1b[1m-C /path/to/repo\x1b[0m.",
                    e.file_path
                );
            }

            FileChange {
                file_path: e.file_path,
                old_file_path: e.old_file_path,
                status: e.status,
                before_content,
                after_content,
            }
        })
        .collect()
}

pub fn diff_command(mut opts: DiffOptions) {
    let total_start = Instant::now();

    let t0 = Instant::now();
    let mut parsed = parse_args(std::mem::take(&mut opts.args), &opts.cwd);

    // Resolve jj revsets to git SHAs if we're in a jj repo
    let root = Path::new(&opts.cwd);
    if sem_core::git::jj::is_jj_repo(root) {
        if let Some(ref mut scope) = parsed.scope {
            match scope {
                ParsedScope::RefToWorking(ref mut r) => {
                    *r = maybe_resolve_ref(r, root);
                }
                ParsedScope::Range(ref mut from, ref mut to) => {
                    *from = maybe_resolve_ref(from, root);
                    *to = maybe_resolve_ref(to, root);
                }
                ParsedScope::MergeBaseRange(ref mut a, ref mut b) => {
                    *a = maybe_resolve_ref(a, root);
                    *b = maybe_resolve_ref(b, root);
                }
                ParsedScope::FileCompare(_, _) => {}
            }
        }
        if let Some(ref mut sha) = opts.commit {
            *sha = maybe_resolve_ref(sha, root);
        }
        if let Some(ref mut from) = opts.from {
            *from = maybe_resolve_ref(from, root);
        }
        if let Some(ref mut to) = opts.to {
            *to = maybe_resolve_ref(to, root);
        }
    }

    let (file_changes, from_stdin) = if opts.stdin {
        // Read FileChange[] from stdin — no git repo needed
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .unwrap_or_else(|e| {
                eprintln!("\x1b[31mError reading stdin: {e}\x1b[0m");
                process::exit(1);
            });
        let changes: Vec<FileChange> = serde_json::from_str(&input).unwrap_or_else(|e| {
            eprintln!("\x1b[31mError parsing stdin JSON: {e}\x1b[0m");
            process::exit(1);
        });
        (changes, true)
    } else if let Some(ParsedScope::FileCompare(ref a, ref b)) = parsed.scope {
        // Compare two arbitrary files: sem diff file1.ts file2.ts
        let path_a = Path::new(&opts.cwd).join(a);
        let path_b = Path::new(&opts.cwd).join(b);

        // If we're in a git repo and both resolve as refs, prefer ref comparison
        if let Ok(git) = GitBridge::open(Path::new(&opts.cwd)) {
            if git.is_valid_rev(a) && git.is_valid_rev(b) {
                let scope = DiffScope::Range {
                    from: a.clone(),
                    to: b.clone(),
                };
                match git.get_changed_files(&scope, &parsed.pathspecs) {
                    Ok(files) => {
                        return run_diff_pipeline(files, false, &opts, &parsed, total_start, t0)
                    }
                    Err(e) => {
                        eprintln!("\x1b[31mError: {e}\x1b[0m");
                        process::exit(1);
                    }
                }
            }
        }

        let content_a = std::fs::read_to_string(&path_a).unwrap_or_else(|e| {
            eprintln!("\x1b[31mError reading {}: {e}\x1b[0m", path_a.display());
            process::exit(1);
        });
        let content_b = std::fs::read_to_string(&path_b).unwrap_or_else(|e| {
            eprintln!("\x1b[31mError reading {}: {e}\x1b[0m", path_b.display());
            process::exit(1);
        });

        let change = FileChange {
            file_path: b.clone(),
            old_file_path: None,
            status: sem_core::git::types::FileStatus::Modified,
            before_content: Some(content_a),
            after_content: Some(content_b),
        };
        (vec![change], false)
    } else if opts.patch {
        // Read unified diff from stdin and parse it
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .unwrap_or_else(|e| {
                eprintln!("\x1b[31mError reading stdin: {e}\x1b[0m");
                process::exit(1);
            });
        let changes = parse_unified_diff(&input, &opts.cwd);
        let changes = if parsed.pathspecs.is_empty() {
            changes
        } else {
            changes
                .into_iter()
                .filter(|fc| {
                    parsed.pathspecs.iter().any(|spec| {
                        path_matches_spec(&fc.file_path, spec)
                    })
                })
                .collect()
        };
        (changes, true)
    } else {
        let git = match GitBridge::open(Path::new(&opts.cwd)) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("\x1b[31mError: {e}\x1b[0m");
                process::exit(1);
            }
        };

        // Determine scope from explicit flags, parsed args, or auto-detect
        let (_scope, file_changes) = if let Some(ref sha) = opts.commit {
            let scope = DiffScope::Commit { sha: sha.clone() };
            match git.get_changed_files(&scope, &parsed.pathspecs) {
                Ok(files) => (scope, files),
                Err(e) => {
                    eprintln!("\x1b[31mError: {e}\x1b[0m");
                    process::exit(1);
                }
            }
        } else if let (Some(ref from), Some(ref to)) = (&opts.from, &opts.to) {
            let scope = DiffScope::Range {
                from: from.clone(),
                to: to.clone(),
            };
            match git.get_changed_files(&scope, &parsed.pathspecs) {
                Ok(files) => (scope, files),
                Err(e) => {
                    eprintln!("\x1b[31mError: {e}\x1b[0m");
                    process::exit(1);
                }
            }
        } else if let Some(ref parsed_scope) = parsed.scope {
            // Use scope from positional args
            let scope = match parsed_scope {
                ParsedScope::RefToWorking(refspec) => {
                    if opts.staged {
                        // git diff --cached <ref> = compare ref to index
                        // We approximate this as Range from ref to HEAD (staged view)
                        // For now, just use the ref as a range base
                        DiffScope::Range {
                            from: refspec.clone(),
                            to: "HEAD".to_string(),
                        }
                    } else {
                        DiffScope::RefToWorking {
                            refspec: refspec.clone(),
                        }
                    }
                }
                ParsedScope::Range(from, to) => DiffScope::Range {
                    from: from.clone(),
                    to: to.clone(),
                },
                ParsedScope::MergeBaseRange(ref1, ref2) => {
                    match git.resolve_merge_base(ref1, ref2) {
                        Ok(base) => DiffScope::Range {
                            from: base,
                            to: ref2.clone(),
                        },
                        Err(e) => {
                            eprintln!("\x1b[31mError resolving merge base: {e}\x1b[0m");
                            process::exit(1);
                        }
                    }
                }
                ParsedScope::FileCompare(_, _) => unreachable!(),
            };
            match git.get_changed_files(&scope, &parsed.pathspecs) {
                Ok(files) => (scope, files),
                Err(e) => {
                    eprintln!("\x1b[31mError: {e}\x1b[0m");
                    process::exit(1);
                }
            }
        } else if opts.staged {
            let scope = DiffScope::Staged;
            match git.get_changed_files(&scope, &parsed.pathspecs) {
                Ok(files) => (scope, files),
                Err(e) => {
                    eprintln!("\x1b[31mError: {e}\x1b[0m");
                    process::exit(1);
                }
            }
        } else {
            match git.detect_and_get_files(&parsed.pathspecs) {
                Ok((scope, files)) => (scope, files),
                Err(e) => {
                    eprintln!("\x1b[31mError: {e}\x1b[0m");
                    process::exit(1);
                }
            }
        };
        (file_changes, false)
    };

    run_diff_pipeline(file_changes, from_stdin, &opts, &parsed, total_start, t0);
}

fn run_diff_pipeline(
    file_changes: Vec<FileChange>,
    from_stdin: bool,
    opts: &DiffOptions,
    _parsed: &ParsedArgs,
    total_start: Instant,
    t0: Instant,
) {
    let git_diff_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // Filter by file extensions if specified
    let file_changes = if opts.file_exts.is_empty() {
        file_changes
    } else {
        let exts: Vec<String> = opts
            .file_exts
            .iter()
            .map(|e| {
                if e.starts_with('.') {
                    e.clone()
                } else {
                    format!(".{}", e)
                }
            })
            .collect();
        file_changes
            .into_iter()
            .filter(|fc| {
                exts.iter().any(|ext| {
                    fc.file_path.ends_with(ext.as_str())
                        || fc.old_file_path.as_ref().is_some_and(|old| old.ends_with(ext.as_str()))
                })
            })
            .collect()
    };

    if file_changes.is_empty() {
        match opts.format {
            OutputFormat::Json => {
                println!("{{\"summary\":{{\"fileCount\":0,\"added\":0,\"modified\":0,\"deleted\":0,\"moved\":0,\"renamed\":0,\"reordered\":0,\"orphan\":0,\"total\":0}},\"changes\":[]}}");
            }
            _ => {
                println!("\x1b[2mNo semantic changes detected.\x1b[0m");
            }
        }
        return;
    }

    let t2 = Instant::now();
    let registry = super::create_registry(&opts.cwd);
    let registry_ms = t2.elapsed().as_secs_f64() * 1000.0;

    let t3 = Instant::now();
    let result = compute_semantic_diff(&file_changes, &registry, None, None);
    let parse_diff_ms = t3.elapsed().as_secs_f64() * 1000.0;

    // Record lifetime stats (best-effort)
    let _ = SemLifetimeStats::load().record_diff(&result).save();

    let t4 = Instant::now();
    let output = match opts.format {
        OutputFormat::Json => format_json(&result),
        OutputFormat::Markdown => format_markdown(&result, opts.verbose),
        OutputFormat::Plain => format_plain(&result),
        OutputFormat::Terminal => format_terminal(&result, opts.verbose),
    };
    let format_ms = t4.elapsed().as_secs_f64() * 1000.0;

    println!("{output}");

    if opts.profile {
        let total_ms = total_start.elapsed().as_secs_f64() * 1000.0;
        eprintln!();
        eprintln!("\x1b[2m── Profile ──────────────────────────────────\x1b[0m");
        eprintln!(
            "\x1b[2m  input ({})  {git_diff_ms:>8.2}ms\x1b[0m",
            if from_stdin { "stdin" } else { "git" }
        );
        eprintln!("\x1b[2m  registry init        {registry_ms:>8.2}ms\x1b[0m");
        eprintln!("\x1b[2m  parse + match        {parse_diff_ms:>8.2}ms\x1b[0m");
        eprintln!("\x1b[2m  format output        {format_ms:>8.2}ms\x1b[0m");
        eprintln!("\x1b[2m  ─────────────────────────────────────────────\x1b[0m");
        eprintln!("\x1b[2m  total                {total_ms:>8.2}ms\x1b[0m");
        eprintln!(
            "\x1b[2m  files: {}  entities: {}  changes: {}\x1b[0m",
            file_changes.len(),
            result.changes.len(),
            result.added_count
                + result.modified_count
                + result.deleted_count
                + result.moved_count
                + result.renamed_count
                + result.reordered_count
        );
        eprintln!("\x1b[2m─────────────────────────────────────────────\x1b[0m");
    }
}
