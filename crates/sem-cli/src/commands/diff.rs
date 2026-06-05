use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::process;
use std::time::Instant;

use git2::{ObjectType, Oid, Repository};
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

fn looks_like_pathspec(arg: &str) -> bool {
    arg.starts_with(':') || arg.contains('*') || arg.contains('?') || arg.contains('[')
}

fn pathspec_like_arg_is_valid_rev(
    arg: &str,
    git: &mut Option<Option<GitBridge>>,
    cwd: &str,
) -> bool {
    arg.starts_with(':') && is_valid_git_rev(git, cwd, arg)
}

fn get_git_bridge<'a>(git: &'a mut Option<Option<GitBridge>>, cwd: &str) -> Option<&'a GitBridge> {
    if git.is_none() {
        *git = Some(GitBridge::open(Path::new(cwd)).ok());
    }

    git.as_ref().and_then(|git| git.as_ref())
}

fn is_valid_git_rev(git: &mut Option<Option<GitBridge>>, cwd: &str, arg: &str) -> bool {
    get_git_bridge(git, cwd).is_some_and(|git| git.is_valid_rev(arg))
}

/// Simple glob matching for pathspecs. Supports `*`, `**`, `?`, and bracket classes.
fn glob_match(pattern: &str, path: &str) -> bool {
    if pattern.is_ascii() && path.is_ascii() {
        return glob_match_inner_bytes(pattern.as_bytes(), path.as_bytes());
    }

    let pat: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = path.chars().collect();
    glob_match_inner(&pat, &text)
}

fn posix_bracket_class_matches(name: &str, ch: char) -> Option<bool> {
    match name {
        "alnum" => Some(ch.is_ascii_alphanumeric()),
        "alpha" => Some(ch.is_ascii_alphabetic()),
        "blank" => Some(ch == ' ' || ch == '\t'),
        "cntrl" => Some(ch.is_ascii_control()),
        "digit" => Some(ch.is_ascii_digit()),
        "graph" => Some(ch.is_ascii_graphic()),
        "lower" => Some(ch.is_ascii_lowercase()),
        "print" => Some(ch.is_ascii_graphic() || ch == ' '),
        "punct" => Some(ch.is_ascii_punctuation()),
        "space" => Some(ch.is_ascii_whitespace()),
        "upper" => Some(ch.is_ascii_uppercase()),
        "xdigit" => Some(ch.is_ascii_hexdigit()),
        _ => None,
    }
}

fn match_posix_bracket_class(pat: &[char], ch: char) -> Option<(bool, usize)> {
    if !matches!((pat.first(), pat.get(1)), (Some(&'['), Some(&':'))) {
        return None;
    }

    for idx in 2..pat.len().saturating_sub(1) {
        if pat[idx] == ':' && pat[idx + 1] == ']' {
            let name: String = pat[2..idx].iter().collect();
            return posix_bracket_class_matches(&name, ch).map(|matched| (matched, idx + 2));
        }
    }

    None
}

fn match_bracket_class(pat: &[char], ch: char) -> Option<(bool, usize)> {
    if pat.first() != Some(&'[') {
        return None;
    }

    let mut idx = 1;
    let negated = matches!(pat.get(idx), Some('!' | '^'));
    if negated {
        idx += 1;
    }

    let class_start = idx;
    let mut matched = false;

    while idx < pat.len() {
        if pat[idx] == ']' && idx > class_start {
            return Some((matched != negated, idx + 1));
        }

        if let Some((posix_matched, consumed)) = match_posix_bracket_class(&pat[idx..], ch) {
            matched |= posix_matched;
            idx += consumed;
        } else if idx + 2 < pat.len() && pat[idx + 1] == '-' && pat[idx + 2] != ']' {
            let start = pat[idx];
            let end = pat[idx + 2];
            if start <= ch && ch <= end {
                matched = true;
            }
            idx += 3;
        } else {
            if pat[idx] == ch {
                matched = true;
            }
            idx += 1;
        }
    }

    None
}

fn glob_match_inner(pat: &[char], text: &[char]) -> bool {
    if pat.is_empty() {
        return text.is_empty();
    }

    // Handle ** (matches any chars, with **/ advancing on path boundaries)
    if pat.len() >= 2 && pat[0] == '*' && pat[1] == '*' {
        if pat.len() > 2 && pat[2] == '/' {
            let rest = &pat[3..];
            if glob_match_inner(rest, text) {
                return true;
            }
            for i in 0..text.len() {
                if text[i] == '/' && glob_match_inner(rest, &text[i + 1..]) {
                    return true;
                }
            }
            return false;
        }

        let rest = &pat[2..];
        for i in 0..=text.len() {
            if glob_match_inner(rest, &text[i..]) {
                return true;
            }
        }
        return false;
    }

    // Handle * (matches any chars)
    if pat[0] == '*' {
        for i in 0..=text.len() {
            if glob_match_inner(&pat[1..], &text[i..]) {
                return true;
            }
        }
        return false;
    }

    // Handle ? (matches one char)
    if pat[0] == '?' {
        return !text.is_empty() && glob_match_inner(&pat[1..], &text[1..]);
    }

    // Handle bracket character classes.
    if pat[0] == '[' {
        if let Some((class_matches, consumed)) =
            text.first().and_then(|ch| match_bracket_class(pat, *ch))
        {
            return class_matches && glob_match_inner(&pat[consumed..], &text[1..]);
        }
    }

    // Literal character
    !text.is_empty() && pat[0] == text[0] && glob_match_inner(&pat[1..], &text[1..])
}

fn posix_bracket_class_matches_byte(name: &[u8], byte: u8) -> Option<bool> {
    match name {
        b"alnum" => Some(byte.is_ascii_alphanumeric()),
        b"alpha" => Some(byte.is_ascii_alphabetic()),
        b"blank" => Some(byte == b' ' || byte == b'\t'),
        b"cntrl" => Some(byte.is_ascii_control()),
        b"digit" => Some(byte.is_ascii_digit()),
        b"graph" => Some(byte.is_ascii_graphic()),
        b"lower" => Some(byte.is_ascii_lowercase()),
        b"print" => Some(byte.is_ascii_graphic() || byte == b' '),
        b"punct" => Some(byte.is_ascii_punctuation()),
        b"space" => Some(byte.is_ascii_whitespace()),
        b"upper" => Some(byte.is_ascii_uppercase()),
        b"xdigit" => Some(byte.is_ascii_hexdigit()),
        _ => None,
    }
}

fn match_posix_bracket_class_bytes(pat: &[u8], byte: u8) -> Option<(bool, usize)> {
    if !matches!((pat.first(), pat.get(1)), (Some(&b'['), Some(&b':'))) {
        return None;
    }

    for idx in 2..pat.len().saturating_sub(1) {
        if pat[idx] == b':' && pat[idx + 1] == b']' {
            return posix_bracket_class_matches_byte(&pat[2..idx], byte)
                .map(|matched| (matched, idx + 2));
        }
    }

    None
}

fn match_bracket_class_bytes(pat: &[u8], byte: u8) -> Option<(bool, usize)> {
    if pat.first() != Some(&b'[') {
        return None;
    }

    let mut idx = 1;
    let negated = matches!(pat.get(idx), Some(b'!' | b'^'));
    if negated {
        idx += 1;
    }

    let class_start = idx;
    let mut matched = false;

    while idx < pat.len() {
        if pat[idx] == b']' && idx > class_start {
            return Some((matched != negated, idx + 1));
        }

        if let Some((posix_matched, consumed)) = match_posix_bracket_class_bytes(&pat[idx..], byte)
        {
            matched |= posix_matched;
            idx += consumed;
        } else if idx + 2 < pat.len() && pat[idx + 1] == b'-' && pat[idx + 2] != b']' {
            let start = pat[idx];
            let end = pat[idx + 2];
            if start <= byte && byte <= end {
                matched = true;
            }
            idx += 3;
        } else {
            if pat[idx] == byte {
                matched = true;
            }
            idx += 1;
        }
    }

    None
}

fn glob_match_inner_bytes(pat: &[u8], text: &[u8]) -> bool {
    if pat.is_empty() {
        return text.is_empty();
    }

    if pat.len() >= 2 && pat[0] == b'*' && pat[1] == b'*' {
        if pat.len() > 2 && pat[2] == b'/' {
            let rest = &pat[3..];
            if glob_match_inner_bytes(rest, text) {
                return true;
            }
            for i in 0..text.len() {
                if text[i] == b'/' && glob_match_inner_bytes(rest, &text[i + 1..]) {
                    return true;
                }
            }
            return false;
        }

        let rest = &pat[2..];
        for i in 0..=text.len() {
            if glob_match_inner_bytes(rest, &text[i..]) {
                return true;
            }
        }
        return false;
    }

    if pat[0] == b'*' {
        for i in 0..=text.len() {
            if glob_match_inner_bytes(&pat[1..], &text[i..]) {
                return true;
            }
        }
        return false;
    }

    if pat[0] == b'?' {
        return !text.is_empty() && glob_match_inner_bytes(&pat[1..], &text[1..]);
    }

    if pat[0] == b'[' {
        if let Some((class_matches, consumed)) = text
            .first()
            .and_then(|byte| match_bracket_class_bytes(pat, *byte))
        {
            return class_matches && glob_match_inner_bytes(&pat[consumed..], &text[1..]);
        }
    }

    !text.is_empty() && pat[0] == text[0] && glob_match_inner_bytes(&pat[1..], &text[1..])
}

fn path_has_dir_prefix(file_path: &str, dir: &str) -> bool {
    file_path.starts_with(dir) && file_path.as_bytes().get(dir.len()) == Some(&b'/')
}

fn literal_pathspec_matches(file_path: &str, spec: &str) -> bool {
    if spec == "." {
        true
    } else if spec.ends_with('/') {
        let dir = spec.trim_end_matches('/');
        path_has_dir_prefix(file_path, dir)
    } else {
        file_path == spec || path_has_dir_prefix(file_path, spec)
    }
}

/// Check if a file path matches a pathspec (supports prefix matching and basic globs).
fn path_matches_spec(file_path: &str, spec: &str) -> bool {
    if literal_pathspec_matches(file_path, spec) {
        true
    } else if spec.contains('*') || spec.contains('?') || spec.contains('[') {
        glob_match(spec, file_path)
    } else {
        false
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

enum WorktreePatchContent {
    Text(String),
    Binary,
}

fn read_git_blob_content(repo: &Repository, sha: &str) -> Option<String> {
    let object = repo.revparse_single(sha).ok()?;
    let blob = object.peel_to_blob().ok()?;

    String::from_utf8(blob.content().to_vec()).ok()
}

fn cached_git_blob_content(
    repo: Option<&Repository>,
    blob_cache: &mut HashMap<String, Option<String>>,
    sha: &str,
) -> Option<String> {
    if let Some(content) = blob_cache.get(sha) {
        return content.clone();
    }

    let content = repo.and_then(|repo| read_git_blob_content(repo, sha));
    blob_cache.insert(sha.to_string(), content.clone());
    content
}

fn read_worktree_content_matching_sha(
    worktree_root: &Path,
    file_path: &str,
    sha_prefix: &str,
) -> Option<WorktreePatchContent> {
    let bytes = std::fs::read(worktree_root.join(file_path)).ok()?;
    let oid = Oid::hash_object(ObjectType::Blob, &bytes).ok()?;
    if !oid.to_string().starts_with(sha_prefix) {
        return None;
    }

    if bytes_look_binary(&bytes, true) {
        return Some(WorktreePatchContent::Binary);
    }

    String::from_utf8(bytes)
        .ok()
        .map(WorktreePatchContent::Text)
}

fn parse_args(args: Vec<String>, cwd: &str) -> ParsedArgs {
    let (refs, pathspecs) = split_on_separator(args);
    let mut git = None;

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
            if is_valid_git_rev(&mut git, cwd, arg) {
                eprintln!(
                    "warning: '{arg}' is both a git revision and a path; treating it as a pathspec"
                );
            }
            let mut pathspecs = pathspecs;
            pathspecs.push(arg.clone());
            return ParsedArgs {
                scope: None,
                pathspecs,
            };
        }

        if looks_like_pathspec(arg) && pathspec_like_arg_is_valid_rev(arg, &mut git, cwd) {
            return ParsedArgs {
                scope: Some(ParsedScope::RefToWorking(arg.clone())),
                pathspecs,
            };
        }

        // If the arg contains glob meta-characters, treat as pathspec
        if looks_like_pathspec(arg) {
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
        let b_exists = Path::new(cwd).join(b).exists();
        let b_looks_like_pathspec = b_exists || looks_like_pathspec(b);

        // git diff <ref> <path> treats the second positional argument as a
        // pathspec when only the first argument resolves as a revision.
        if pathspecs.is_empty() && b_looks_like_pathspec {
            let a_is_rev = is_valid_git_rev(&mut git, cwd, a);
            let b_is_rev = is_valid_git_rev(&mut git, cwd, b);
            if a_is_rev && !b_is_rev {
                let mut pathspecs = pathspecs;
                pathspecs.push(b.clone());
                return ParsedArgs {
                    scope: Some(ParsedScope::RefToWorking(a.clone())),
                    pathspecs,
                };
            }
        }

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
/// Uses blob SHAs when possible and hunk bodies when the patch is not tied to local blobs.
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

    let (old, rest) = if paths.trim_start().starts_with('"') {
        let Some((old, rest)) = parse_git_path_token(paths) else {
            return false;
        };
        let Some(rest) = rest.strip_prefix(" and ") else {
            return false;
        };
        (old, rest)
    } else {
        let Some((old, rest)) = paths.split_once(" and ") else {
            return false;
        };
        (Cow::Borrowed(old), rest)
    };
    let Some((new, rest)) = parse_git_path_token(rest) else {
        return false;
    };
    if !rest.is_empty() {
        return false;
    }

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

fn parse_git_path_token(input: &str) -> Option<(Cow<'_, str>, &str)> {
    let input = input.trim_start();
    if let Some(rest) = input.strip_prefix('"') {
        let mut bytes = Vec::new();
        let rest_bytes = rest.as_bytes();
        let mut index = 0usize;

        while index < rest_bytes.len() {
            let byte = rest_bytes[index];
            if byte == b'"' {
                return Some((
                    Cow::Owned(String::from_utf8_lossy(&bytes).into_owned()),
                    &rest[index + 1..],
                ));
            }

            if byte == b'\\' && index + 1 < rest_bytes.len() {
                let next = rest_bytes[index + 1];
                if next.is_ascii_digit() && next < b'8' {
                    let mut value = 0u16;
                    let mut consumed = 0usize;
                    while consumed < 3 && index + 1 + consumed < rest_bytes.len() {
                        let digit = rest_bytes[index + 1 + consumed];
                        if !digit.is_ascii_digit() || digit >= b'8' {
                            break;
                        }
                        value = value * 8 + u16::from(digit - b'0');
                        consumed += 1;
                    }
                    bytes.push(u8::try_from(value).ok()?);
                    index += 1 + consumed;
                    continue;
                }

                bytes.push(match next {
                    b'a' => 0x07,
                    b'b' => 0x08,
                    b'f' => 0x0c,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'v' => 0x0b,
                    other => other,
                });
                index += 2;
                continue;
            }

            bytes.push(byte);
            index += 1;
        }

        None
    } else {
        let end = input.find('\t').unwrap_or(input.len());
        let rest = if end < input.len() {
            &input[end + 1..]
        } else {
            &input[end..]
        };
        Some((Cow::Borrowed(&input[..end]), rest))
    }
}

fn normalize_diff_path(path: &str, side_prefix: &str) -> Option<String> {
    if path == "/dev/null" {
        None
    } else {
        Some(path.strip_prefix(side_prefix).unwrap_or(path).to_string())
    }
}

fn parse_diff_git_paths(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("diff --git ")?;
    if rest.trim_start().starts_with('"') {
        let (old_path, rest) = parse_git_path_token(rest)?;
        let (new_path, _) = parse_git_path_token(rest)?;
        return Some((
            normalize_diff_path(old_path.as_ref(), "a/").unwrap_or_else(|| old_path.into_owned()),
            normalize_diff_path(new_path.as_ref(), "b/").unwrap_or_else(|| new_path.into_owned()),
        ));
    }

    let rest = rest.strip_prefix("a/")?;
    let mut fallback = None;
    let mut search_start = 0usize;
    while let Some(relative_pos) = rest[search_start..].find(" b/") {
        let pos = search_start + relative_pos;
        let old_path = &rest[..pos];
        let new_path = &rest[pos + 3..];
        if old_path == new_path {
            return Some((old_path.to_string(), new_path.to_string()));
        }
        if fallback.is_none() {
            fallback = Some((old_path, new_path));
        }
        search_start = pos + 1;
    }

    let (old_path, new_path) = fallback.unwrap_or((rest, rest));
    Some((old_path.to_string(), new_path.to_string()))
}

fn parse_file_header_path(line: &str, marker: &str, side_prefix: &str) -> Option<Option<String>> {
    let rest = line.strip_prefix(marker)?;
    let (path, _) = parse_git_path_token(rest)?;
    Some(normalize_diff_path(path.as_ref(), side_prefix))
}

fn parse_metadata_path(rest: &str) -> String {
    parse_git_path_token(rest)
        .map(|(path, _)| path.into_owned())
        .unwrap_or_else(|| rest.to_string())
}

fn parse_unified_diff(
    patch: &str,
    worktree_root: &Path,
) -> Result<Vec<FileChange>, PatchParseError> {
    use sem_core::git::types::FileStatus;

    #[derive(Clone, Copy)]
    enum HunkLineSide {
        Before,
        After,
        Both,
    }

    struct PatchEntry {
        file_path: String,
        old_file_path: Option<String>,
        status: FileStatus,
        old_sha: Option<String>,
        new_sha: Option<String>,
        hunk_before_content: String,
        hunk_after_content: String,
        last_hunk_side: Option<HunkLineSide>,
        has_valid_hunk: bool,
        has_hunk_content: bool,
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

    impl PatchEntry {
        fn from_diff_header(line: &str) -> Option<Self> {
            let (_old_path, file_path) = parse_diff_git_paths(line)?;
            Some(Self {
                file_path,
                old_file_path: None,
                status: FileStatus::Modified,
                old_sha: None,
                new_sha: None,
                hunk_before_content: String::new(),
                hunk_after_content: String::new(),
                last_hunk_side: None,
                has_valid_hunk: false,
                has_hunk_content: false,
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
            })
        }

        fn push_hunk_line(&mut self, side: HunkLineSide, content: &str) {
            match side {
                HunkLineSide::Before => {
                    self.hunk_before_content.push_str(content);
                    self.hunk_before_content.push('\n');
                }
                HunkLineSide::After => {
                    self.hunk_after_content.push_str(content);
                    self.hunk_after_content.push('\n');
                }
                HunkLineSide::Both => {
                    self.hunk_before_content.push_str(content);
                    self.hunk_before_content.push('\n');
                    self.hunk_after_content.push_str(content);
                    self.hunk_after_content.push('\n');
                }
            }
            self.last_hunk_side = Some(side);
            self.has_hunk_content = true;
        }

        fn trim_last_hunk_newline(&mut self) {
            match self.last_hunk_side {
                Some(HunkLineSide::Before) => {
                    self.hunk_before_content.pop();
                }
                Some(HunkLineSide::After) => {
                    self.hunk_after_content.pop();
                }
                Some(HunkLineSide::Both) => {
                    self.hunk_before_content.pop();
                    self.hunk_after_content.pop();
                }
                None => {}
            }
        }
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
        if line.starts_with("diff --git ") {
            // Flush previous entry
            if let Some(entry) = current.take() {
                if entry.has_valid_hunk
                    || (!entry.has_malformed_hunk && has_complete_hunkless_metadata(&entry))
                {
                    entries.push(entry);
                }
            }
            current = PatchEntry::from_diff_header(line);
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
            } else if line.starts_with("@@") {
                if is_unified_hunk_header(line) {
                    entry.has_valid_hunk = true;
                    entry.last_hunk_side = None;
                    valid_hunk_count += 1;
                } else {
                    malformed_hunk_count += 1;
                    entry.has_malformed_hunk = true;
                    eprintln!(
                        "warning: malformed hunk header in {}: '{}' (expected '@@ -N,M +N,M @@')",
                        entry.file_path, line
                    );
                }
            } else if entry.has_valid_hunk {
                if line == r"\ No newline at end of file" {
                    entry.trim_last_hunk_newline();
                } else if let Some(content) = line.strip_prefix(' ') {
                    entry.push_hunk_line(HunkLineSide::Both, content);
                } else if let Some(content) = line.strip_prefix('-') {
                    entry.push_hunk_line(HunkLineSide::Before, content);
                } else if let Some(content) = line.strip_prefix('+') {
                    entry.push_hunk_line(HunkLineSide::After, content);
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
            } else if let Some(old_path) = parse_file_header_path(line, "--- ", "a/") {
                if old_path.is_none() {
                    entry.status = FileStatus::Added;
                }
            } else if let Some(new_path) = parse_file_header_path(line, "+++ ", "b/") {
                if let Some(new_path) = new_path {
                    entry.file_path = new_path;
                } else {
                    entry.status = FileStatus::Deleted;
                }
            } else if is_binary_files_marker(line) {
                entry.has_binary_marker = true;
            } else if line.starts_with("GIT binary patch") {
                entry.has_git_binary_patch = true;
            } else if entry.has_git_binary_patch && is_git_binary_chunk_header {
                entry.awaiting_git_binary_payload = true;
            } else if let Some(rest) = line.strip_prefix("rename from ") {
                entry.old_file_path = Some(parse_metadata_path(rest));
                entry.status = FileStatus::Renamed;
                entry.has_rename_from = true;
            } else if let Some(rest) = line.strip_prefix("rename to ") {
                entry.file_path = parse_metadata_path(rest);
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

    let repo = Repository::discover(worktree_root).ok();
    let mut blob_cache: HashMap<String, Option<String>> = HashMap::new();

    Ok(entries
        .into_iter()
        .map(|e| {
            let is_binary = e.has_binary_marker || e.has_git_binary_patch;
            let mut has_binary_content = is_binary;
            let mut before_content = if is_binary {
                None
            } else {
                e.old_sha
                    .as_deref()
                    .and_then(|sha| cached_git_blob_content(repo.as_ref(), &mut blob_cache, sha))
            };
            let mut after_content = if is_binary {
                None
            } else {
                e.new_sha
                    .as_deref()
                    .and_then(|sha| cached_git_blob_content(repo.as_ref(), &mut blob_cache, sha))
            };

            // If git cannot resolve the target blob for a local worktree diff,
            // read the target file from disk when the base blob resolved.
            if !is_binary
                && after_content.is_none()
                && before_content.is_some()
                && e.new_sha.is_some()
            {
                if let Some(content) = e.new_sha.as_deref().and_then(|sha| {
                    read_worktree_content_matching_sha(worktree_root, &e.file_path, sha)
                }) {
                    match content {
                        WorktreePatchContent::Text(content) => after_content = Some(content),
                        WorktreePatchContent::Binary => has_binary_content = true,
                    }
                }
            }

            if !is_binary
                && e.has_hunk_content
                && (before_content.is_none() || after_content.is_none())
            {
                before_content = Some(e.hunk_before_content.clone());
                after_content = Some(e.hunk_after_content.clone());
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
                println!("{}", format_json(&DiffResult::default(), &[]));
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
    fn pathspec_matching_respects_directory_boundaries() {
        assert!(!path_matches_spec("src", "src/"));
        assert!(path_matches_spec("src/a.py", "src/"));
        assert!(path_matches_spec("src/nested/a.py", "src/"));
        assert!(!path_matches_spec("src2/a.py", "src/"));
        assert!(path_matches_spec("src/a.py", "src"));
        assert!(!path_matches_spec("src2/a.py", "src"));
        assert!(path_matches_spec("a[12].py", "a[12].py"));
        assert!(path_matches_spec("a[12].py/nested.py", "a[12].py"));
    }

    #[test]
    fn glob_matching_supports_bracket_classes() {
        assert!(glob_match("a[12].py", "a1.py"));
        assert!(glob_match("a[12].py", "a2.py"));
        assert!(!glob_match("a[12].py", "a3.py"));
        assert!(glob_match("a[1-3].py", "a2.py"));
        assert!(glob_match("a[!3].py", "a2.py"));
        assert!(!glob_match("a[!3].py", "a3.py"));
        assert!(glob_match("a[^3].py", "a2.py"));
        assert!(glob_match("a[[:digit:]].py", "a2.py"));
        assert!(!glob_match("a[[:digit:]].py", "aa.py"));
        assert!(glob_match("a[![:digit:]].py", "aa.py"));
        assert!(!glob_match("a[![:digit:]].py", "a2.py"));
        assert!(glob_match("a[ab].py", "aa.py"));
        assert!(glob_match("a?b.py", "a/b.py"));
        assert!(glob_match("a[/]b.py", "a/b.py"));
        assert!(glob_match("*.py", "src/nested/a.py"));
        assert!(glob_match("**/a.py", "src/nested/a.py"));
        assert!(glob_match("**/a.py", "a.py"));
        assert!(!glob_match("**/a.py", "nested-a.py"));
        assert!(glob_match("a[12.py", "a[12.py"));
    }

    #[test]
    fn quoted_git_path_tokens_reject_octal_overflow() {
        assert_eq!(
            parse_git_path_token(r#""a/\303\274n.py""#)
                .map(|(path, rest)| (path.into_owned(), rest)),
            Some(("a/ün.py".to_string(), ""))
        );
        assert!(parse_git_path_token(r#""a/\777.py""#).is_none());
        assert_eq!(
            parse_git_path_token(r#""a/\9.py""#).map(|(path, rest)| (path.into_owned(), rest)),
            Some(("a/9.py".to_string(), ""))
        );
    }

    #[test]
    fn unquoted_git_path_tokens_consume_tab_metadata() {
        let (path, rest) =
            parse_git_path_token("a/app.py\t2026-01-01").expect("unquoted token parses");

        assert_eq!(path, "a/app.py");
        assert!(matches!(path, std::borrow::Cow::Borrowed(_)));
        assert_eq!(rest, "2026-01-01");
    }

    #[test]
    fn diff_git_paths_use_first_fallback_split_for_renames() {
        assert_eq!(
            parse_diff_git_paths("diff --git a/foo b/bar b/baz"),
            Some(("foo".to_string(), "bar b/baz".to_string()))
        );
    }

    #[test]
    fn binary_files_marker_accepts_quoted_paths_without_trailing_content() {
        assert!(is_binary_files_marker(
            r#"Binary files "a/\303\274n.bin" and "b/\303\274n.bin" differ"#
        ));
        assert!(is_binary_files_marker(
            r#"Binary files /dev/null and "b/\303\274n.bin" differ"#
        ));
        assert!(!is_binary_files_marker(
            r#"Binary files "a/blob.bin" and "b/blob.bin"  differ"#
        ));
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
    fn parse_unified_diff_decodes_quoted_git_paths() {
        let patch = concat!(
            "diff --git \"a/\\303\\274n\\303\\257c\\303\\266d\\303\\251.py\" \"b/\\303\\274n\\303\\257c\\303\\266d\\303\\251.py\"\n",
            "--- \"a/\\303\\274n\\303\\257c\\303\\266d\\303\\251.py\"\n",
            "+++ \"b/\\303\\274n\\303\\257c\\303\\266d\\303\\251.py\"\n",
            "@@ -1,2 +1,2 @@\n",
            " def foo():\n",
            "-    return 1\n",
            "+    return 2\n",
        );

        let changes = parse_unified_diff(patch, Path::new(".")).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].file_path, "ünïcödé.py");
        assert_eq!(
            changes[0].before_content.as_deref(),
            Some("def foo():\n    return 1\n")
        );
        assert_eq!(
            changes[0].after_content.as_deref(),
            Some("def foo():\n    return 2\n")
        );
    }

    #[test]
    fn parse_unified_diff_uses_file_headers_for_paths_containing_b_prefix() {
        let patch = concat!(
            "diff --git a/my b/app.py b/my b/app.py\n",
            "--- a/my b/app.py\n",
            "+++ b/my b/app.py\n",
            "@@ -1,2 +1,2 @@\n",
            " def foo():\n",
            "-    return 1\n",
            "+    return 2\n",
        );

        let changes = parse_unified_diff(patch, Path::new(".")).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].file_path, "my b/app.py");
    }

    #[test]
    fn parse_unified_diff_reconstructs_indexless_hunk_content() {
        let patch = concat!(
            "diff --git a/app.py b/app.py\n",
            "--- a/app.py\n",
            "+++ b/app.py\n",
            "@@ -1,2 +1,2 @@\n",
            " def foo():\n",
            "-    return 1\n",
            "+    return 2\n",
        );

        let changes = parse_unified_diff(patch, Path::new(".")).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes[0].before_content.as_deref(),
            Some("def foo():\n    return 1\n")
        );
        assert_eq!(
            changes[0].after_content.as_deref(),
            Some("def foo():\n    return 2\n")
        );
    }

    #[test]
    fn parse_unified_diff_reads_hunkless_b_prefix_path_from_diff_header() {
        let patch = "diff --git a/my b/app.py b/my b/app.py\n\
                     old mode 100644\n\
                     new mode 100755\n";

        let changes = parse_unified_diff(patch, Path::new(".")).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].file_path, "my b/app.py");
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
    fn parse_unified_diff_accepts_added_quoted_binary_marker() {
        let patch = "diff --git a/ün.bin b/ün.bin\n\
                     new file mode 100644\n\
                     index 0000000..2222222\n\
                     Binary files /dev/null and \"b/\\303\\274n.bin\" differ\n";

        let changes = parse_unified_diff(patch, Path::new(".")).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].file_path, "ün.bin");
        assert_eq!(changes[0].status, sem_core::git::types::FileStatus::Added);
        assert!(changes[0].before_content.is_none());
        assert!(changes[0].after_content.is_none());
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
