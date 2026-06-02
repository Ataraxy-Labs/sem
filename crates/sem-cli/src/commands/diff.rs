use std::collections::HashSet;
use std::io::Read;
use std::path::Path;
use std::process;
use std::time::Instant;

use sem_core::git::bridge::GitBridge;
use sem_core::git::jj::maybe_resolve_ref;
use sem_core::git::types::{DiffScope, FileChange, FileStatus};
use sem_core::model::change::ChangeType;
use sem_core::parser::differ::{collect_binary_file_changes, compute_semantic_diff, DiffResult};
use sem_core::parser::plugins::code::languages::get_language_config;
use sem_core::parser::registry::{detect_ext_from_content, ParserRegistry};

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
    pub no_cosmetics: bool,
    pub label: Option<String>,
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
    FileCompare {
        before: String,
        after: String,
        label: Option<String>,
    },
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
    if spec == "." {
        true
    } else if spec.contains('*') || spec.contains('?') || spec.contains('[') {
        glob_match(spec, file_path)
    } else if spec.ends_with('/') {
        file_path.starts_with(spec.trim_end_matches('/'))
    } else {
        file_path == spec || file_path.starts_with(&format!("{spec}/"))
    }
}

fn file_change_matches_spec(file_change: &FileChange, spec: &str) -> bool {
    path_matches_spec(&file_change.file_path, spec)
        || file_change
            .old_file_path
            .as_ref()
            .is_some_and(|old_path| path_matches_spec(old_path, spec))
}

fn parse_patch_pathspecs(args: Vec<String>) -> Vec<String> {
    let (before_separator, after_separator) = split_on_separator(args);
    // In --patch mode stdin supplies the diff scope, so positional args are filters.
    // If `--` is present, follow git-style pathspec separation and use the right side.
    if after_separator.is_empty() {
        before_separator
    } else {
        after_separator
    }
}

fn parse_output_format(value: &str) -> OutputFormat {
    match value {
        "terminal" => OutputFormat::Terminal,
        "plain" => OutputFormat::Plain,
        "json" => OutputFormat::Json,
        "markdown" | "md" => OutputFormat::Markdown,
        _ => {
            eprintln!(
                "\x1b[31mError: invalid output format '{value}'. Expected terminal, plain, json, markdown, or md.\x1b[0m"
            );
            process::exit(1);
        }
    }
}

fn normalize_trailing_output_format(opts: &mut DiffOptions) {
    let original_args = std::mem::take(&mut opts.args);
    let mut args = Vec::new();
    let mut after_separator = false;
    let mut idx = 0;

    while idx < original_args.len() {
        let arg = &original_args[idx];
        if after_separator {
            args.push(arg.clone());
            idx += 1;
            continue;
        }

        if arg == "--" {
            after_separator = true;
            args.push(arg.clone());
            idx += 1;
            continue;
        }

        if arg == "--json" {
            opts.format = OutputFormat::Json;
            idx += 1;
            continue;
        }

        if let Some(value) = arg.strip_prefix("--format=") {
            opts.format = parse_output_format(value);
            idx += 1;
            continue;
        }

        if arg == "--format" {
            match original_args.get(idx + 1) {
                Some(value) if !value.starts_with("--") => {
                    opts.format = parse_output_format(value);
                    idx += 2;
                    continue;
                }
                Some(value) => {
                    eprintln!("\x1b[31mError: --format requires a value before '{value}'.\x1b[0m");
                    process::exit(1);
                }
                None => {
                    eprintln!("\x1b[31mError: --format requires a value.\x1b[0m");
                    process::exit(1);
                }
            }
        }

        args.push(arg.clone());
        idx += 1;
    }

    opts.args = args;
}

fn bytes_look_binary(bytes: &[u8], complete: bool) -> bool {
    if bytes.iter().any(|byte| *byte == 0) {
        return true;
    }

    match std::str::from_utf8(bytes) {
        Ok(_) => false,
        Err(error) => complete || error.error_len().is_some(),
    }
}

fn read_file_compare_content(path: &Path) -> Result<Option<String>, std::io::Error> {
    let bytes = std::fs::read(path)?;
    if bytes_look_binary(&bytes, true) {
        Ok(None)
    } else {
        let content = String::from_utf8(bytes)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        Ok(Some(content))
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
        if pathspecs.is_empty()
            && Path::new(cwd).join(a).exists()
            && Path::new(cwd).join(b).exists()
        {
            // But check if they're also valid git refs — prefer ref interpretation
            // Only fall back to file comparison if neither resolves as a ref
            return ParsedArgs {
                scope: Some(ParsedScope::FileCompare {
                    before: a.clone(),
                    after: b.clone(),
                    label: None,
                }),
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
            scope: Some(ParsedScope::FileCompare {
                before: refs[1].clone(),
                after: refs[4].clone(),
                label: Some(refs[0].clone()),
            }),
            pathspecs,
        };
    }

    eprintln!("\x1b[31mError: too many positional arguments. Use -- to separate pathspecs.\x1b[0m");
    process::exit(1);
}

/// Parse a unified diff (e.g. from `git diff`) into FileChange entries.
/// Uses blob SHAs from `index` lines to retrieve full file contents via `git show`.
#[derive(Debug, PartialEq, Eq)]
enum PatchParseError {
    EmptyInput,
    NoRecognizableHunks,
}

impl PatchParseError {
    fn message(&self) -> &'static str {
        match self {
            PatchParseError::EmptyInput => "no input on stdin (use --patch < file.diff)",
            PatchParseError::NoRecognizableHunks => {
                "no recognizable diff hunks in stdin (expected 'diff --git' headers and '@@ ... @@' hunk markers)"
            }
        }
    }
}

fn is_unified_hunk_header(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("@@ ") else {
        return false;
    };
    let Some((ranges, _context)) = rest.split_once(" @@") else {
        return false;
    };

    let mut parts = ranges.split_whitespace();
    let old_range = parts.next();
    let new_range = parts.next();

    matches!(
        (old_range, new_range, parts.next()),
        (Some(old), Some(new), None)
            if is_hunk_range(old, '-') && is_hunk_range(new, '+')
    )
}

fn is_hunk_range(range: &str, prefix: char) -> bool {
    let Some(rest) = range.strip_prefix(prefix) else {
        return false;
    };
    let mut parts = rest.split(',');
    let Some(start) = parts.next() else {
        return false;
    };
    let count = parts.next();

    parts.next().is_none()
        && !start.is_empty()
        && start.chars().all(|c| c.is_ascii_digit())
        && count.map_or(true, |c| {
            !c.is_empty() && c.chars().all(|ch| ch.is_ascii_digit())
        })
}

fn is_binary_files_marker(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("Binary files ") else {
        return false;
    };
    let Some(paths) = rest.strip_suffix(" differ") else {
        return false;
    };
    let Some((old, new)) = paths.split_once(" and ") else {
        return false;
    };

    (old == "/dev/null" || old.starts_with("a/")) && (new == "/dev/null" || new.starts_with("b/"))
}

fn is_git_binary_payload_line(line: &str) -> bool {
    !line.trim().is_empty()
        && !line.starts_with("@@")
        && !line.starts_with("diff --git ")
        && !line.starts_with("index ")
        && !line.starts_with("--- ")
        && !line.starts_with("+++ ")
}

fn parse_unified_diff(
    patch: &str,
    worktree_root: &Path,
) -> Result<Vec<FileChange>, PatchParseError> {
    use sem_core::git::types::FileStatus;

    struct PatchEntry {
        file_path: String,
        old_file_path: Option<String>,
        status: FileStatus,
        old_sha: Option<String>,
        new_sha: Option<String>,
        has_valid_hunk: bool,
        has_malformed_hunk: bool,
        has_index: bool,
        has_new_file_mode: bool,
        has_deleted_file_mode: bool,
        has_old_mode: bool,
        has_new_mode: bool,
        has_rename_from: bool,
        has_rename_to: bool,
        has_binary_marker: bool,
        has_git_binary_patch: bool,
        awaiting_git_binary_payload: bool,
        has_git_binary_payload: bool,
        has_invalid_git_binary_patch: bool,
    }

    if patch.trim().is_empty() {
        return Err(PatchParseError::EmptyInput);
    }

    let mut entries: Vec<PatchEntry> = Vec::new();
    let mut current: Option<PatchEntry> = None;
    let mut valid_hunk_count = 0usize;
    let mut malformed_hunk_count = 0usize;
    let has_complete_hunkless_metadata = |entry: &PatchEntry| {
        (entry.has_rename_from && entry.has_rename_to)
            || (entry.has_old_mode && entry.has_new_mode)
            || ((entry.has_new_file_mode || entry.has_deleted_file_mode) && entry.has_index)
            || (entry.has_index
                && (entry.has_binary_marker
                    || (entry.has_git_binary_patch
                        && entry.has_git_binary_payload
                        && !entry.awaiting_git_binary_payload
                        && !entry.has_invalid_git_binary_patch)))
    };

    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            // Flush previous entry
            if let Some(entry) = current.take() {
                if entry.has_valid_hunk
                    || (!entry.has_malformed_hunk && has_complete_hunkless_metadata(&entry))
                {
                    entries.push(entry);
                }
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
                has_valid_hunk: false,
                has_malformed_hunk: false,
                has_index: false,
                has_new_file_mode: false,
                has_deleted_file_mode: false,
                has_old_mode: false,
                has_new_mode: false,
                has_rename_from: false,
                has_rename_to: false,
                has_binary_marker: false,
                has_git_binary_patch: false,
                awaiting_git_binary_payload: false,
                has_git_binary_payload: false,
                has_invalid_git_binary_patch: false,
            });
        } else if let Some(ref mut entry) = current {
            let is_git_binary_chunk_header =
                line.starts_with("literal ") || line.starts_with("delta ");

            if entry.awaiting_git_binary_payload {
                if is_git_binary_payload_line(line) && !is_git_binary_chunk_header {
                    entry.awaiting_git_binary_payload = false;
                    entry.has_git_binary_payload = true;
                } else if !line.trim().is_empty() {
                    entry.awaiting_git_binary_payload = false;
                    entry.has_invalid_git_binary_patch = true;
                }
            } else if line.starts_with("new file mode") {
                entry.status = FileStatus::Added;
                entry.has_new_file_mode = true;
            } else if line.starts_with("deleted file mode") {
                entry.status = FileStatus::Deleted;
                entry.has_deleted_file_mode = true;
            } else if line.starts_with("old mode") {
                entry.has_old_mode = true;
            } else if line.starts_with("new mode") {
                entry.has_new_mode = true;
            } else if is_binary_files_marker(line) {
                entry.has_binary_marker = true;
            } else if line.starts_with("GIT binary patch") {
                entry.has_git_binary_patch = true;
            } else if entry.has_git_binary_patch && is_git_binary_chunk_header {
                entry.awaiting_git_binary_payload = true;
            } else if let Some(rest) = line.strip_prefix("rename from ") {
                entry.old_file_path = Some(rest.to_string());
                entry.status = FileStatus::Renamed;
                entry.has_rename_from = true;
            } else if let Some(rest) = line.strip_prefix("rename to ") {
                entry.file_path = rest.to_string();
                entry.has_rename_to = true;
            } else if let Some(rest) = line.strip_prefix("index ") {
                // "index abc123..def456 100644" or "index abc123..def456"
                entry.has_index = true;
                let shas_part = rest.split_whitespace().next().unwrap_or("");
                if let Some((old, new)) = shas_part.split_once("..") {
                    if old != "0000000" && !old.chars().all(|c| c == '0') {
                        entry.old_sha = Some(old.to_string());
                    }
                    if new != "0000000" && !new.chars().all(|c| c == '0') {
                        entry.new_sha = Some(new.to_string());
                    }
                }
            } else if line.starts_with("@@") {
                if is_unified_hunk_header(line) {
                    entry.has_valid_hunk = true;
                    valid_hunk_count += 1;
                } else {
                    malformed_hunk_count += 1;
                    entry.has_malformed_hunk = true;
                    eprintln!(
                        "warning: malformed hunk header in {}: '{}' (expected '@@ -N,M +N,M @@')",
                        entry.file_path, line
                    );
                }
            }
        }
    }
    if let Some(entry) = current.take() {
        if entry.has_valid_hunk
            || (!entry.has_malformed_hunk && has_complete_hunkless_metadata(&entry))
        {
            entries.push(entry);
        }
    }

    // Malformed hunk headers are recognizable diff structure: warn above, then
    // proceed with any other parsed entries. If none parsed, the empty diff is intentional.
    if entries.is_empty() && valid_hunk_count == 0 && malformed_hunk_count == 0 {
        return Err(PatchParseError::NoRecognizableHunks);
    }

    // Resolve blob contents via git show
    let git_show = |sha: &str| -> Option<String> {
        let output = process::Command::new("git")
            .args(["show", sha])
            .current_dir(worktree_root)
            .output()
            .ok()?;
        if output.status.success() {
            String::from_utf8(output.stdout).ok()
        } else {
            None
        }
    };

    Ok(entries
        .into_iter()
        .map(|e| {
            let is_binary = e.has_binary_marker || e.has_git_binary_patch;
            let mut has_binary_content = is_binary;
            let before_content = if is_binary {
                None
            } else {
                e.old_sha.as_deref().and_then(&git_show)
            };
            let mut after_content = if is_binary {
                None
            } else {
                e.new_sha.as_deref().and_then(&git_show)
            };

            // Fallback: if git show fails for the new SHA (e.g. unstaged working
            // tree changes where the blob doesn't exist yet), read from disk.
            if !is_binary && after_content.is_none() && e.new_sha.is_some() {
                let file = worktree_root.join(&e.file_path);
                match read_file_compare_content(&file) {
                    Ok(Some(content)) => after_content = Some(content),
                    Ok(None) => has_binary_content = true,
                    Err(_) => {}
                }
            }

            if !has_binary_content && before_content.is_none() && after_content.is_none() {
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
        .collect())
}

fn file_language_id(file_path: &str, content: &str, registry: &ParserRegistry) -> Option<String> {
    let resolved = registry.resolve_file_path(file_path);
    let detection_path = resolved.as_deref().unwrap_or(file_path);
    let plugin = registry.get_plugin_with_content(detection_path, content)?;

    if plugin.id() != "code" {
        return Some(plugin.id().to_string());
    }

    if let Some(ext) = Path::new(detection_path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| format!(".{}", ext.to_lowercase()))
    {
        if let Some(config) = get_language_config(&ext) {
            return Some(config.id.to_string());
        }
    }

    detect_ext_from_content(content)
        .and_then(|ext| get_language_config(&ext).map(|config| config.id.to_string()))
        .or_else(|| Some(plugin.id().to_string()))
}

fn display_language(language_id: &str) -> String {
    match language_id {
        "bash" => "Bash".to_string(),
        "c" => "C".to_string(),
        "cpp" => "C++".to_string(),
        "csharp" => "C#".to_string(),
        "csv" => "CSV".to_string(),
        "erb" => "ERB".to_string(),
        "hcl" => "HCL".to_string(),
        "html" => "HTML".to_string(),
        "javascript" => "JavaScript".to_string(),
        "json" => "JSON".to_string(),
        "markdown" => "Markdown".to_string(),
        "nix" => "Nix".to_string(),
        "ocaml" => "OCaml".to_string(),
        "php" => "PHP".to_string(),
        "python" => "Python".to_string(),
        "ruby" => "Ruby".to_string(),
        "rust" => "Rust".to_string(),
        "svelte" => "Svelte".to_string(),
        "toml" => "TOML".to_string(),
        "tsx" => "TSX".to_string(),
        "typescript" => "TypeScript".to_string(),
        "vue" => "Vue".to_string(),
        "xml" => "XML".to_string(),
        "yaml" => "YAML".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

fn file_compare_changes(
    source_path: &str,
    target_path: &str,
    source_content: String,
    target_content: String,
    registry: &ParserRegistry,
) -> (Vec<FileChange>, Option<(String, String)>) {
    let source_language = file_language_id(source_path, &source_content, registry);
    let target_language = file_language_id(target_path, &target_content, registry);

    if let (Some(source_language), Some(target_language)) = (&source_language, &target_language) {
        if source_language != target_language {
            // Cross-language inputs cannot share one parser namespace without
            // misattributing one side, so represent them as independent sides.
            return (
                vec![
                    FileChange {
                        file_path: source_path.to_string(),
                        old_file_path: None,
                        status: FileStatus::Deleted,
                        before_content: Some(source_content),
                        after_content: None,
                    },
                    FileChange {
                        file_path: target_path.to_string(),
                        old_file_path: None,
                        status: FileStatus::Added,
                        before_content: None,
                        after_content: Some(target_content),
                    },
                ],
                Some((
                    display_language(source_language),
                    display_language(target_language),
                )),
            );
        }
    }

    (
        vec![FileChange {
            file_path: target_path.to_string(),
            old_file_path: None,
            status: FileStatus::Modified,
            before_content: Some(source_content),
            after_content: Some(target_content),
        }],
        None,
    )
}

pub fn diff_command(mut opts: DiffOptions) {
    let total_start = Instant::now();

    let t0 = Instant::now();
    normalize_trailing_output_format(&mut opts);
    let raw_args = std::mem::take(&mut opts.args);
    let mut parsed = if opts.patch {
        ParsedArgs {
            scope: None,
            pathspecs: parse_patch_pathspecs(raw_args),
        }
    } else {
        parse_args(raw_args, &opts.cwd)
    };
    let patch_worktree_root = if opts.patch {
        Some(super::repo_root_or_cwd(&opts.cwd))
    } else {
        None
    };
    if let Some(root) = patch_worktree_root.as_deref() {
        parsed.pathspecs = parsed
            .pathspecs
            .iter()
            .map(|spec| super::normalize_repo_relative_path(Path::new(&opts.cwd), root, spec))
            .collect();
    }

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
                ParsedScope::FileCompare { .. } => {}
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
    } else if let Some(ParsedScope::FileCompare {
        ref before,
        ref after,
        ref label,
    }) = parsed.scope
    {
        // Compare two arbitrary files: sem diff file1.ts file2.ts
        let path_a = Path::new(&opts.cwd).join(before);
        let path_b = Path::new(&opts.cwd).join(after);

        // If we're in a git repo and both resolve as refs, prefer ref comparison
        if let Ok(git) = GitBridge::open(Path::new(&opts.cwd)) {
            if git.is_valid_rev(before) && git.is_valid_rev(after) {
                let scope = DiffScope::Range {
                    from: before.clone(),
                    to: after.clone(),
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

        let content_a = read_file_compare_content(&path_a).unwrap_or_else(|e| {
            eprintln!("\x1b[31mError reading {}: {e}\x1b[0m", path_a.display());
            process::exit(1);
        });
        let content_b = read_file_compare_content(&path_b).unwrap_or_else(|e| {
            eprintln!("\x1b[31mError reading {}: {e}\x1b[0m", path_b.display());
            process::exit(1);
        });

        let target_label = opts
            .label
            .clone()
            .or_else(|| label.clone())
            .unwrap_or_else(|| after.clone());
        if content_a.is_none() || content_b.is_none() {
            // A None side is binary or otherwise not UTF-8; emit a file-level
            // change so the diff pipeline reports it as a binary change.
            let change = FileChange {
                file_path: target_label,
                old_file_path: None,
                status: FileStatus::Modified,
                before_content: content_a,
                after_content: content_b,
            };
            (vec![change], false)
        } else {
            let registry = super::create_registry(&opts.cwd);
            let (changes, language_mismatch) = file_compare_changes(
                before,
                &target_label,
                content_a.unwrap(),
                content_b.unwrap(),
                &registry,
            );
            if let Some((language_a, language_b)) = language_mismatch {
                eprintln!(
                    "warning: comparing files with different languages: {} ({}) and {} ({}); rendering as delete/add",
                    before, language_a, after, language_b
                );
            }
            (changes, false)
        }
    } else if opts.patch {
        // Read unified diff from stdin and parse it
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .unwrap_or_else(|e| {
                eprintln!("\x1b[31mError reading stdin: {e}\x1b[0m");
                process::exit(1);
            });
        let worktree_root = patch_worktree_root
            .as_deref()
            .expect("patch worktree root is initialized when --patch is set");
        let changes = parse_unified_diff(&input, worktree_root).unwrap_or_else(|e| {
            eprintln!("error: {}", e.message());
            process::exit(1);
        });
        let changes = if parsed.pathspecs.is_empty() {
            changes
        } else {
            changes
                .into_iter()
                .filter(|fc| {
                    parsed
                        .pathspecs
                        .iter()
                        .any(|spec| file_change_matches_spec(fc, spec))
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
        let file_changes = if let Some(ref sha) = opts.commit {
            let scope = DiffScope::Commit { sha: sha.clone() };
            match git.get_changed_files(&scope, &parsed.pathspecs) {
                Ok(files) => files,
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
                Ok(files) => files,
                Err(e) => {
                    eprintln!("\x1b[31mError: {e}\x1b[0m");
                    process::exit(1);
                }
            }
        } else if let Some(ParsedScope::RefToWorking(refspec)) = parsed.scope.as_ref() {
            if opts.staged {
                // git diff --cached <ref> = compare ref to index
                match git.get_staged_files_with_base_ref(refspec, &parsed.pathspecs) {
                    Ok(files) => files,
                    Err(e) => {
                        eprintln!("\x1b[31mError: {e}\x1b[0m");
                        process::exit(1);
                    }
                }
            } else {
                let scope = DiffScope::RefToWorking {
                    refspec: refspec.clone(),
                };
                match git.get_changed_files(&scope, &parsed.pathspecs) {
                    Ok(files) => files,
                    Err(e) => {
                        eprintln!("\x1b[31mError: {e}\x1b[0m");
                        process::exit(1);
                    }
                }
            }
        } else if let Some(ref parsed_scope) = parsed.scope {
            // Use scope from positional args
            let scope = match parsed_scope {
                ParsedScope::RefToWorking(_) => unreachable!(),
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
                ParsedScope::FileCompare { .. } => unreachable!(),
            };
            match git.get_changed_files(&scope, &parsed.pathspecs) {
                Ok(files) => files,
                Err(e) => {
                    eprintln!("\x1b[31mError: {e}\x1b[0m");
                    process::exit(1);
                }
            }
        } else if opts.staged {
            let scope = DiffScope::Staged;
            match git.get_changed_files(&scope, &parsed.pathspecs) {
                Ok(files) => files,
                Err(e) => {
                    eprintln!("\x1b[31mError: {e}\x1b[0m");
                    process::exit(1);
                }
            }
        } else {
            match git.detect_and_get_files(&parsed.pathspecs) {
                Ok((_scope, files)) => files,
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
                        || fc
                            .old_file_path
                            .as_ref()
                            .is_some_and(|old| old.ends_with(ext.as_str()))
                })
            })
            .collect()
    };

    if file_changes.is_empty() {
        match opts.format {
            OutputFormat::Json => {
                println!("{{\"summary\":{{\"fileCount\":0,\"added\":0,\"modified\":0,\"deleted\":0,\"moved\":0,\"renamed\":0,\"reordered\":0,\"binary\":0,\"orphan\":0,\"total\":0}},\"changes\":[],\"binaryChanges\":[]}}");
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
    let binary_changes = collect_binary_file_changes(&file_changes);
    let mut result = compute_semantic_diff(&file_changes, &registry, None, None);
    let parse_diff_ms = t3.elapsed().as_secs_f64() * 1000.0;

    // Filter out cosmetic-only changes when --no-cosmetics is set
    if opts.no_cosmetics {
        retain_non_cosmetic_changes(&mut result);
    }

    // Record lifetime stats (best-effort)
    let _ = SemLifetimeStats::load()
        .record_diff(&result, binary_changes.len())
        .save();

    let t4 = Instant::now();
    let output = match opts.format {
        OutputFormat::Json => format_json(&result, &binary_changes),
        OutputFormat::Markdown => format_markdown(&result, &binary_changes, opts.verbose),
        OutputFormat::Plain => format_plain(&result, &binary_changes),
        OutputFormat::Terminal => format_terminal(&result, &binary_changes, opts.verbose),
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
                + binary_changes.len()
        );
        eprintln!("\x1b[2m─────────────────────────────────────────────\x1b[0m");
    }
}

fn retain_non_cosmetic_changes(result: &mut DiffResult) {
    // A move/rename/reorder that also carries a content change is a compound change:
    // keep it visible under --no-cosmetics, but drop the purely cosmetic content payload.
    for change in &mut result.changes {
        if matches!(
            change.change_type,
            ChangeType::Moved | ChangeType::Renamed | ChangeType::Reordered
        ) && change.has_content_change()
            && change.structural_change == Some(false)
        {
            change.before_content = None;
            change.after_content = None;
        }
    }
    // Drop purely cosmetic changes; compound moves/renames/reorders are preserved above.
    result.changes.retain(|c| {
        c.structural_change != Some(false)
            || matches!(
                c.change_type,
                ChangeType::Moved | ChangeType::Renamed | ChangeType::Reordered
            )
    });
    recalculate_diff_summary(result);
}

fn recalculate_diff_summary(result: &mut DiffResult) {
    // Mirrors compute_semantic_diff: orphan_count is cross-cutting metadata,
    // while retained orphans still contribute to change-type buckets.
    result.file_count = result
        .changes
        .iter()
        .map(|change| change.file_path.as_str())
        .collect::<HashSet<_>>()
        .len();

    let mut added_count = 0;
    let mut modified_count = 0;
    let mut deleted_count = 0;
    let mut moved_count = 0;
    let mut renamed_count = 0;
    let mut reordered_count = 0;
    let mut orphan_count = 0;

    for change in &result.changes {
        if change.entity_type == "orphan" {
            orphan_count += 1;
        }

        match change.change_type {
            ChangeType::Added => added_count += 1,
            ChangeType::Modified => modified_count += 1,
            ChangeType::Deleted => deleted_count += 1,
            ChangeType::Moved => {
                moved_count += 1;
                if change.has_content_change() {
                    modified_count += 1;
                }
            }
            ChangeType::Renamed => {
                renamed_count += 1;
                if change.has_content_change() {
                    modified_count += 1;
                }
            }
            ChangeType::Reordered => {
                reordered_count += 1;
                if change.has_content_change() {
                    modified_count += 1;
                }
            }
        }
    }

    result.added_count = added_count;
    result.modified_count = modified_count;
    result.deleted_count = deleted_count;
    result.moved_count = moved_count;
    result.renamed_count = renamed_count;
    result.reordered_count = reordered_count;
    result.orphan_count = orphan_count;
}

#[cfg(test)]
mod tests {
    use sem_core::model::change::{ChangeType, SemanticChange};
    use serde_json::json;

    use super::*;

    fn change(
        file_path: &str,
        change_type: ChangeType,
        structural_change: Option<bool>,
    ) -> SemanticChange {
        serde_json::from_value(json!({
            "id": format!("{file_path}::{change_type:?}"),
            "entityId": format!("{file_path}::entity"),
            "changeType": change_type,
            "entityType": "function",
            "entityName": "value",
            "entityLine": 1,
            "filePath": file_path,
            "structuralChange": structural_change,
        }))
        .expect("valid SemanticChange fixture")
    }

    fn diff_result(changes: Vec<SemanticChange>) -> DiffResult {
        DiffResult {
            changes,
            file_count: 99,
            added_count: 99,
            modified_count: 99,
            deleted_count: 99,
            moved_count: 99,
            renamed_count: 99,
            reordered_count: 99,
            orphan_count: 99,
            total_entities_before: 0,
            total_entities_after: 0,
        }
    }

    #[test]
    fn no_cosmetics_filter_recomputes_file_count_to_zero() {
        let mut result = diff_result(vec![change("app.py", ChangeType::Modified, Some(false))]);

        retain_non_cosmetic_changes(&mut result);

        assert!(result.changes.is_empty());
        assert_eq!(result.file_count, 0);
        assert_eq!(result.modified_count, 0);
    }

    #[test]
    fn no_cosmetics_filter_recomputes_summary_from_remaining_changes() {
        let mut structural = change("src/lib.rs", ChangeType::Modified, Some(true));
        structural.entity_name = "kept".to_string();
        let cosmetic = change("src/cosmetic.rs", ChangeType::Modified, Some(false));
        let unknown = change("src/lib.rs", ChangeType::Added, None);
        let mut orphan = change("src/orphan.rs", ChangeType::Modified, Some(true));
        orphan.entity_type = "orphan".to_string();

        let mut result = diff_result(vec![structural, cosmetic, unknown, orphan]);

        retain_non_cosmetic_changes(&mut result);

        assert_eq!(result.changes.len(), 3);
        assert_eq!(result.file_count, 2);
        assert_eq!(result.added_count, 1);
        assert_eq!(result.modified_count, 2);
        assert_eq!(result.deleted_count, 0);
        assert_eq!(result.moved_count, 0);
        assert_eq!(result.renamed_count, 0);
        assert_eq!(result.reordered_count, 0);
        assert_eq!(result.orphan_count, 1);
    }

    #[test]
    fn no_cosmetics_filter_drops_cosmetic_orphan_addition() {
        let mut orphan = change("src/orphan.rs", ChangeType::Added, Some(false));
        orphan.entity_type = "orphan".to_string();

        let mut result = diff_result(vec![orphan]);

        retain_non_cosmetic_changes(&mut result);

        assert!(result.changes.is_empty());
        assert_eq!(result.file_count, 0);
        assert_eq!(result.added_count, 0);
        assert_eq!(result.orphan_count, 0);
    }

    #[test]
    fn hunk_header_validation_accepts_git_forms() {
        assert!(is_unified_hunk_header("@@ -1 +1 @@"));
        assert!(is_unified_hunk_header("@@ -1,2 +3,4 @@"));
        assert!(is_unified_hunk_header("@@ -0,0 +1,4 @@ function_name"));

        assert!(!is_unified_hunk_header("@@ NOTAHUNK @@"));
        assert!(!is_unified_hunk_header("@@ -1 +1@@"));
        assert!(!is_unified_hunk_header("@@ -1, +1 @@"));
    }

    #[test]
    fn parse_unified_diff_rejects_empty_input() {
        assert_eq!(
            parse_unified_diff("", Path::new(".")).unwrap_err(),
            PatchParseError::EmptyInput
        );
        assert_eq!(
            parse_unified_diff("\n\t ", Path::new(".")).unwrap_err(),
            PatchParseError::EmptyInput
        );
    }

    #[test]
    fn path_matches_spec_requires_normalized_dot_for_whole_repo() {
        assert!(path_matches_spec("src/lib.rs", "."));
        assert!(!path_matches_spec("src/lib.rs", ""));
    }

    #[test]
    fn parse_unified_diff_rejects_non_diff_input() {
        assert_eq!(
            parse_unified_diff("this is not a diff\n", Path::new(".")).unwrap_err(),
            PatchParseError::NoRecognizableHunks
        );
        assert_eq!(
            parse_unified_diff("diff --git a/a.ts b/a.ts\n", Path::new(".")).unwrap_err(),
            PatchParseError::NoRecognizableHunks
        );
    }

    #[test]
    fn parse_unified_diff_drops_malformed_hunks_before_content_resolution() {
        let patch = "diff --git a/a.ts b/a.ts\n\
                     --- a/a.ts\n\
                     +++ b/a.ts\n\
                     @@ NOTAHUNK @@\n\
                     -foo\n\
                     +bar\n";

        let changes = parse_unified_diff(patch, Path::new(".")).unwrap();
        assert!(changes.is_empty());
    }

    #[test]
    fn parse_unified_diff_keeps_entries_with_valid_hunks() {
        let patch = "diff --git a/a.ts b/a.ts\n\
                     --- a/a.ts\n\
                     +++ b/a.ts\n\
                     @@ -1 +1 @@\n\
                     -foo\n\
                     +bar\n";

        let changes = parse_unified_diff(patch, Path::new(".")).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].file_path, "a.ts");
    }

    #[test]
    fn parse_unified_diff_accepts_hunkless_metadata_patches() {
        let patch = "diff --git a/old.py b/new.py\n\
                     similarity index 100%\n\
                     rename from old.py\n\
                     rename to new.py\n";

        let changes = parse_unified_diff(patch, Path::new(".")).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].old_file_path.as_deref(), Some("old.py"));
        assert_eq!(changes[0].file_path, "new.py");
        assert_eq!(changes[0].status, sem_core::git::types::FileStatus::Renamed);
    }

    #[test]
    fn parse_unified_diff_accepts_complete_git_binary_patches() {
        let patch = "diff --git a/blob.bin b/blob.bin\n\
                     index 1111111..2222222 100644\n\
                     GIT binary patch\n\
                     literal 0\n\
                     HcmV?d00001\n";

        let changes = parse_unified_diff(patch, Path::new(".")).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].file_path, "blob.bin");
    }

    #[test]
    fn parse_unified_diff_accepts_multiple_complete_git_binary_chunks() {
        let patch = "diff --git a/blob.bin b/blob.bin\n\
                     index 1111111..2222222 100644\n\
                     GIT binary patch\n\
                     literal 1\n\
                     abc\n\
                     delta 1\n\
                     def\n";

        let changes = parse_unified_diff(patch, Path::new(".")).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].file_path, "blob.bin");
    }

    #[test]
    fn parse_unified_diff_accepts_complete_binary_files_markers() {
        let patch = "diff --git a/blob.bin b/blob.bin\n\
                     index 1111111..2222222 100644\n\
                     Binary files a/blob.bin and b/blob.bin differ\n";

        let changes = parse_unified_diff(patch, Path::new(".")).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].file_path, "blob.bin");
    }

    #[test]
    fn parse_unified_diff_rejects_incomplete_metadata_patches() {
        for patch in [
            "diff --git a/a.ts b/a.ts\nnew file mode 100644\n",
            "diff --git a/old.py b/new.py\nrename from old.py\n",
            "diff --git a/a.ts b/a.ts\nindex 1111111..2222222 100644\n--- a/a.ts\n+++ b/a.ts\n",
            "diff --git a/blob.bin b/blob.bin\nindex 1111111..2222222 100644\nBinary files differ\n",
            "diff --git a/blob.bin b/blob.bin\nindex 1111111..2222222 100644\nBinary files  and  differ\n",
            "diff --git a/blob.bin b/blob.bin\nindex 1111111..2222222 100644\nBinary files old.bin and new.bin differ\n",
            "diff --git a/blob.bin b/blob.bin\nindex 1111111..2222222 100644\nGIT binary patch\n",
            "diff --git a/blob.bin b/blob.bin\nindex 1111111..2222222 100644\nGIT binary patch\nliteral 1\n",
            "diff --git a/blob.bin b/blob.bin\nindex 1111111..2222222 100644\nGIT binary patch\nliteral 1\n@@ -1 +1 @@\n",
            "diff --git a/blob.bin b/blob.bin\nindex 1111111..2222222 100644\nGIT binary patch\nliteral 1\nabc\ndelta 1\n",
        ] {
            assert_eq!(
                parse_unified_diff(patch, Path::new(".")).unwrap_err(),
                PatchParseError::NoRecognizableHunks
            );
        }
    }

    #[test]
    fn file_compare_binary_detection_handles_nuls_and_invalid_utf8() {
        assert!(bytes_look_binary(b"\0png", true));
        assert!(bytes_look_binary(&[0xff, 0xfe], true));
        assert!(!bytes_look_binary("plain text".as_bytes(), true));
    }

    #[test]
    fn file_compare_binary_detection_ignores_partial_utf8_at_scan_boundary() {
        assert!(!bytes_look_binary(&[0xe2, 0x82], false));
        assert!(bytes_look_binary(&[0xe2, 0x82], true));
    }

    #[test]
    fn read_file_compare_content_preserves_text_side_for_mixed_binary_compare() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "sem-file-compare-test-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).unwrap();

        let text_path = dir.join("text.txt");
        let binary_path = dir.join("image.bin");
        std::fs::write(&text_path, "plain text\n").unwrap();
        std::fs::write(&binary_path, b"\0binary").unwrap();

        assert_eq!(
            read_file_compare_content(&text_path).unwrap().as_deref(),
            Some("plain text\n")
        );
        assert!(read_file_compare_content(&binary_path).unwrap().is_none());

        std::fs::remove_dir_all(dir).unwrap();
    }
}
