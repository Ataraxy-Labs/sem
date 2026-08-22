//! The text tier: `sem grep <pattern>` — literal and basic-regex search
//! served from `TRIGRAM` postings when the pattern makes that possible.
//!
//! Pipeline, matching Google Code Search / zoekt's shape (
//! prior art):
//!
//! ```text
//! pattern -> required trigram query (DNF: OR of AND-sets, §`derive`)
//!         -> candidate file indices (postings intersection/union, §`candidates`)
//!         -> verify: the real regex matcher, on candidates' CURRENT bytes only
//!         -> rg-compatible file:line:text rows
//! ```
//!
//! The pattern is always compiled as a regex (`regex::bytes::Regex`), same
//! default rg uses without `-F` — a literal pattern is simply a regex with no
//! metacharacters, so there is exactly one code path, not a literal/regex
//! fork. What *is* forked is trigram derivation: patterns whose required
//! trigrams can't be established (see `CandidateOrigin`) fall back to
//! `FullScan`, an honest rg-equivalent worst case, never an incorrect answer.
//!
//! **Freshness.** `verify` reads each candidate file's bytes fresh from disk
//! on every call — there is no cached content anywhere in this module, so a
//! *matched* line is always current by construction ("text freshness is
//! simpler than entity freshness: just scan the current bytes", the change's
//! own framing).
//!
//! **Membership**: `search` runs `complete::complete_check`
//! concurrently with trigram candidate derivation (`rayon::join`) — the same
//! parallel-stat sweep `commands::query`'s verbs use. A brand-new file joins
//! the candidate set unconditionally (there is no trigram data for it yet,
//! so it cannot be prefiltered — it goes straight to `verify`, same as any
//! `Stopped`-trigram candidate); a deleted file is dropped from the
//! candidate set before `verify` wastes a read on it. This closes the direct
//! analogue of membership gap for the text tier. What is **still** out
//! of scope, and unclosed by this change: a file *edited* since the index was
//! built such that it newly contains a required trigram it didn't contain
//! before is not discovered by the membership sweep (its mtime changed, but
//! it isn't new or deleted, so neither `FILES`-existence nor
//! `DIRS`-membership catches it) — postings for an existing, modified file
//! are only as fresh as the last `write_query_index`. Detecting *content*
//! drift on every already-known file would mean stat-ing (or re-deriving
//! trigrams for) the whole corpus per query, which is exactly the
//! corpus-proportional cost spent 85% of the query budget removing — so
//! this module still does not attempt that half, and says so here rather
//! than silently.

use std::collections::BTreeSet;
use std::path::Path;

use regex::bytes::{Regex, RegexBuilder};

use super::complete::complete_check;
use super::format::trigram as wire;
use super::{QueryIndex, TrigramPosting};

/// One matched line, rg's `file:line:text` shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepHit {
    pub file: String,
    /// 1-based, matching rg.
    pub line: usize,
    pub text: String,
}

/// How the candidate file set was derived — surfaced (not hidden inside a
/// single `Vec<GrepHit>`) so a caller can report the honest worst case
/// rather than presenting every query as if it got the trigram fast path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateOrigin {
    /// At least one alternative's required-trigram AND-set resolved to a
    /// bounded set via postings intersection.
    Trigram,
    /// The pattern yielded no usable trigram query at all: case-insensitive
    /// search, the tier is absent, a literal run under 3 bytes, or a
    /// top-level alternative with no mandatory literal content. Candidate
    /// set = every indexed file, i.e. the rg-equivalent scan.
    FullScan,
    /// Every top-level alternative required a trigram that provably occurs
    /// in zero files. No candidates, no scan — the pattern cannot match
    /// anything in this corpus.
    NoCandidates,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct GrepOptions {
    pub case_insensitive: bool,
}

#[derive(Debug)]
pub struct GrepReport {
    pub hits: Vec<GrepHit>,
    pub candidate_files: usize,
    pub total_files: usize,
    pub origin: CandidateOrigin,
}

/// Search `idx`'s corpus (files rooted at `root`) for `pattern`. `Err` only
/// for a pattern that fails to compile as a regex — never for "no matches"
/// or "no usable trigram", both of which are represented in `GrepReport`.
///
/// `walk_subtree` is `complete_check`'s corpus-eligibility walker (see its
/// doc) — the caller supplies it because ignore/extension policy lives in
/// `sem-cli`, same reasoning `commands::query`'s call site uses.
pub fn search(
    idx: &QueryIndex,
    root: &Path,
    pattern: &str,
    opts: &GrepOptions,
    walk_subtree: impl Fn(&Path) -> Vec<String> + Sync + Send,
) -> Result<GrepReport, regex::Error> {
    let matcher = RegexBuilder::new(pattern)
        .case_insensitive(opts.case_insensitive)
        .build()?;

    // Trigram candidate derivation and the membership sweep touch disjoint
    // sections (NAMES-adjacent TRIGRAM postings vs. FILES/DIRS), so they run
    // concurrently — same discipline `commands::query::index_answer` uses.
    let ((candidates, origin), complete) = rayon::join(
        || candidate_files(idx, pattern, opts.case_insensitive),
        || complete_check(idx, root, walk_subtree),
    );

    // Unlike `commands::query`'s entity verbs, grep has no cheaper
    // authoritative fallback to bail to on an inconclusive sweep — `verify`
    // already tolerates a missing file (silently skips it), so the worst
    // case here is "this one call might miss a file created in the same
    // instant as a directory-listing race", never a wrong match. Best-effort
    // is the honest trade for a text tier that has no daemon watching it.
    let deleted: BTreeSet<&str> = complete.deleted_files.iter().map(String::as_str).collect();
    let total_files = idx.file_count() + complete.new_files.len();

    let mut hits = Vec::new();
    let mut scanned = 0usize;
    for file_index in &candidates {
        let path = idx.file_path(*file_index);
        if path.is_empty() || deleted.contains(path) {
            continue;
        }
        scanned += 1;
        verify_file(root, path, &matcher, &mut hits);
    }
    // New files carry no trigram data at all (postings are only as fresh as
    // the last build) — they can't be prefiltered, so they join the
    // candidate set unconditionally and go straight to `verify`, same as any
    // `Stopped`-trigram candidate.
    for path in &complete.new_files {
        scanned += 1;
        verify_file(root, path, &matcher, &mut hits);
    }

    Ok(GrepReport {
        hits,
        candidate_files: scanned,
        total_files,
        origin,
    })
}

/// The full-scan fallback path, without going through the index at all —
/// what a repo with no trigram tier (or `SEM_NO_INDEX=1`) falls through to.
/// Kept separate from `search` so a caller can measure the honest worst
/// case even without a built index.
pub fn full_scan(
    root: &Path,
    files: &[String],
    pattern: &str,
    opts: &GrepOptions,
) -> Result<Vec<GrepHit>, regex::Error> {
    let matcher = RegexBuilder::new(pattern)
        .case_insensitive(opts.case_insensitive)
        .build()?;
    let mut hits = Vec::new();
    for path in files {
        verify_file(root, path, &matcher, &mut hits);
    }
    Ok(hits)
}

/// Read `path`'s CURRENT bytes and record every matching line. Missing or
/// unreadable files are silently skipped (deleted-since-index, permission —
/// never a hard error for one file out of a candidate set).
fn verify_file(root: &Path, path: &str, matcher: &Regex, hits: &mut Vec<GrepHit>) {
    let Ok(bytes) = std::fs::read(root.join(path)) else {
        return;
    };
    for (i, mut line) in bytes.split(|&b| b == b'\n').enumerate() {
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        if matcher.is_match(line) {
            hits.push(GrepHit {
                file: path.to_string(),
                line: i + 1,
                text: String::from_utf8_lossy(line).into_owned(),
            });
        }
    }
}

/// Resolve `pattern`'s candidate file indices against `idx`'s postings.
/// Never wrong, only ever more or less precise — see the module doc for the
/// soundness argument (a real match always survives to the union; only
/// provably-impossible alternatives are dropped).
fn candidate_files(
    idx: &QueryIndex,
    pattern: &str,
    case_insensitive: bool,
) -> (Vec<u32>, CandidateOrigin) {
    let total = idx.file_count() as u32;
    if case_insensitive || !idx.has_trigram() || has_inline_group_flags(pattern) {
        // Case folding means a stored (case-sensitive) trigram no longer
        // proves anything about what the pattern needs — out of scope for
        // this change's trigram tier.
        // `has_inline_group_flags` covers the same hazard when it's spelled
        // inline (`(?i)...`) rather than via `-i`: an un-parsed `(?x)` could
        // also make our literal-run text not correspond to what the compiled
        // regex actually requires (extended mode strips whitespace/comments
        // the pattern text still contains). Both are soundness risks, not
        // just selectivity ones, so both disable the prefilter rather than
        // trying to parse inline flags — see the out-of-scope list.
        return ((0..total).collect(), CandidateOrigin::FullScan);
    }

    let alternatives = split_top_level(pattern, '|');
    let mut and_sets: Vec<Vec<[u8; 3]>> = Vec::with_capacity(alternatives.len());
    for alt in &alternatives {
        let set = required_trigrams(alt);
        if set.is_empty() {
            // This alternative can match with no mandatory literal evidence
            // at all, so the whole OR is unconstrained by trigrams.
            return ((0..total).collect(), CandidateOrigin::FullScan);
        }
        and_sets.push(set);
    }

    let mut union: BTreeSet<u32> = BTreeSet::new();
    let mut any_bounded = false;
    for set in &and_sets {
        let mut acc: Option<BTreeSet<u32>> = None;
        let mut impossible = false;
        for &trigram in set {
            match idx.trigram_posting(trigram) {
                TrigramPosting::Absent => {
                    impossible = true;
                    break;
                }
                TrigramPosting::Stopped => continue, // no constraint, safe to skip
                TrigramPosting::Present(files) => {
                    let s: BTreeSet<u32> = files.into_iter().collect();
                    acc = Some(match acc {
                        None => s,
                        Some(prev) => prev.intersection(&s).copied().collect(),
                    });
                }
            }
        }
        if impossible {
            continue; // this alternative cannot match any file
        }
        match acc {
            Some(s) => {
                any_bounded = true;
                union.extend(s);
            }
            // Every trigram this alternative required was stop-listed: no
            // filtering information survived, so this alternative alone
            // could match any file. The OR degenerates to "all files".
            None => return ((0..total).collect(), CandidateOrigin::FullScan),
        }
    }

    if !any_bounded {
        return (Vec::new(), CandidateOrigin::NoCandidates);
    }
    (union.into_iter().collect(), CandidateOrigin::Trigram)
}

/// `true` if `pattern` contains an inline flag group — `(?i)`, `(?x)`,
/// `(?ism-U:...)`, etc. — as opposed to a benign non-capturing (`(?:...)`)
/// or named (`(?P<name>...)`/`(?<name>...)`) group. Conservative by design:
/// a false positive here only costs selectivity (an unnecessary full scan);
/// a false negative would be a soundness bug (`(?i)` invalidates
/// case-sensitive trigrams, `(?x)` can make literal whitespace in the
/// pattern text not correspond to what the compiled regex requires) — see
/// the call site and the out-of-scope list.
fn has_inline_group_flags(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'(' && bytes[i + 1] == b'?' {
            match bytes.get(i + 2) {
                Some(b':') | Some(b'P') | Some(b'<') => {}
                Some(_) => return true,
                None => {}
            }
        }
        i += 1;
    }
    false
}

/// The required trigram set for one (already top-level-alternative-split)
/// pattern fragment: the union of every literal run's byte-trigrams. Empty
/// means "no usable evidence" (every literal run was under 3 bytes, or there
/// were none at all). Public so test/probe tooling (`index_probe`'s mutation
/// test) can find a trigram a given pattern actually depends on, without
/// duplicating this logic.
pub fn required_trigrams(fragment: &str) -> Vec<[u8; 3]> {
    let mut set: BTreeSet<[u8; 3]> = BTreeSet::new();
    for run in literal_runs(fragment) {
        let bytes = run.as_bytes();
        if bytes.len() < 3 {
            continue;
        }
        for w in bytes.windows(3) {
            set.insert([w[0], w[1], w[2]]);
        }
    }
    set.into_iter().collect()
}

/// Split `pattern` on `sep` at depth 0 only — `(...)`/`[...]` nesting is
/// respected so `a(b|c)d` splits nowhere (the top-level pattern has one
/// alternative) rather than incorrectly at the `|` inside the group.
/// **Out of scope**: nested alternation inside a group is not given its own
/// trigram treatment — `literal_runs` treats the whole group as an opaque
/// boundary (see its doc), which is sound (never wrongly excludes a match)
/// but less selective than a full regex-AST-based query would be.
fn split_top_level(pattern: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_class = false;
    let mut escaped = false;
    let mut start = 0usize;
    for (i, c) in pattern.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            '(' if !in_class => depth += 1,
            ')' if !in_class => depth -= 1,
            _ if c == sep && !in_class && depth == 0 => {
                parts.push(&pattern[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&pattern[start..]);
    parts
}

/// The mandatory literal substrings of one regex fragment — every char that
/// is guaranteed to be present verbatim whenever the fragment matches.
///
/// This is a conservative, *sound* approximation, not a full regex-AST
/// analysis (that is Code Search's actual approach, and out of scope here): it walks the pattern once,
/// accumulating literal characters, and flushes the current run into the
/// result on any metacharacter. A quantifier (`*`, `+`, `?`, `{n,m}`)
/// additionally **drops the last accumulated character** before flushing,
/// because that character is what the quantifier applies to and is
/// therefore not guaranteed present (`+` technically guarantees at least
/// one occurrence, but this function does not special-case it — dropping is
/// always safe, only ever costs selectivity, never correctness).
///
/// Escapes: `\` followed by a regex metacharacter (`. ^ $ | ( ) [ ] { } * +
/// ?  \`) is treated as that literal character. `\` followed by anything
/// else (`\d`, `\w`, `\s`, `\p{...}`, ...) is treated as an opaque boundary —
/// out of scope, degrades selectivity, never correctness.
fn literal_runs(pattern: &str) -> Vec<String> {
    let mut runs = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                if let Some(&next) = chars.get(i + 1) {
                    if "\\.^$|()[]{}*+?".contains(next) {
                        buf.push(next);
                    } else {
                        flush(&mut buf, &mut runs);
                    }
                    i += 2;
                } else {
                    flush(&mut buf, &mut runs);
                    i += 1;
                }
            }
            '*' | '+' | '?' => {
                buf.pop();
                flush(&mut buf, &mut runs);
                i += 1;
            }
            '{' => {
                buf.pop();
                flush(&mut buf, &mut runs);
                while i < chars.len() && chars[i] != '}' {
                    i += 1;
                }
                i += 1; // consume '}', or step past end — both fine
            }
            '.' | '^' | '$' | ')' => {
                flush(&mut buf, &mut runs);
                i += 1;
            }
            '(' => {
                flush(&mut buf, &mut runs);
                i += 1;
            }
            '[' => {
                flush(&mut buf, &mut runs);
                i += 1;
                // Opaque char class: skip to the matching ']'. A leading
                // '^' (negation) or a leading ']' (literal ']' as the first
                // class member) are the two POSIX-regex quirks; neither
                // changes that the class contributes no mandatory literal
                // content, so both are handled the same way — skip to the
                // next ']' — with the "first char after an optional '^' may
                // itself be ']'" rule respected so we don't stop early.
                if chars.get(i) == Some(&'^') {
                    i += 1;
                }
                if chars.get(i) == Some(&']') {
                    i += 1;
                }
                while i < chars.len() && chars[i] != ']' {
                    i += 1;
                }
                i += 1; // consume ']', or step past end
            }
            _ => {
                buf.push(c);
                i += 1;
            }
        }
    }
    flush(&mut buf, &mut runs);
    runs
}

fn flush(buf: &mut String, runs: &mut Vec<String>) {
    if !buf.is_empty() {
        runs.push(std::mem::take(buf));
    }
}

// Re-exported for callers that want to pack a literal byte triple without
// going through `required_trigrams` (e.g. tests, `index_probe`'s oracle).
pub fn pack_trigram(bytes: [u8; 3]) -> u32 {
    wire::pack(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_pattern_is_one_run() {
        assert_eq!(literal_runs("createProgram"), vec!["createProgram"]);
    }

    #[test]
    fn inline_flag_groups_are_detected_but_benign_groups_are_not() {
        assert!(has_inline_group_flags("(?i)createProgram"));
        assert!(has_inline_group_flags("foo(?x) bar"));
        assert!(has_inline_group_flags("(?ism-U:foo)"));
        assert!(!has_inline_group_flags("(?:foo|bar)baz"));
        assert!(!has_inline_group_flags("(?P<name>foo)"));
        assert!(!has_inline_group_flags("(?<name>foo)"));
        assert!(!has_inline_group_flags("createProgram"));
    }

    #[test]
    fn quantifier_drops_the_quantified_char() {
        // "foo*" guarantees "fo", not "foo" (the trailing 'o' is optional).
        assert_eq!(literal_runs("foo*bar"), vec!["fo", "bar"]);
        assert_eq!(literal_runs("colou?r"), vec!["colo", "r"]);
    }

    #[test]
    fn dot_and_classes_break_the_run_without_adding_content() {
        assert_eq!(literal_runs("foo.bar"), vec!["foo", "bar"]);
        assert_eq!(literal_runs("foo[0-9]bar"), vec!["foo", "bar"]);
        assert_eq!(literal_runs("foo[^0-9]bar"), vec!["foo", "bar"]);
        assert_eq!(literal_runs("foo[]0-9]bar"), vec!["foo", "bar"]);
    }

    #[test]
    fn escaped_metachar_is_literal() {
        assert_eq!(literal_runs(r"a\.b\*c"), vec!["a.b*c"]);
    }

    #[test]
    fn unrecognized_escape_is_opaque() {
        assert_eq!(literal_runs(r"foo\dbar"), vec!["foo", "bar"]);
    }

    #[test]
    fn top_level_alternation_splits_but_nested_does_not() {
        assert_eq!(split_top_level("foo|bar", '|'), vec!["foo", "bar"]);
        assert_eq!(split_top_level("a(b|c)d", '|'), vec!["a(b|c)d"]);
        assert_eq!(split_top_level("[a|b]", '|'), vec!["[a|b]"]);
    }

    #[test]
    fn required_trigrams_of_a_short_literal_is_empty() {
        assert!(required_trigrams("ab").is_empty());
        assert!(!required_trigrams("abc").is_empty());
    }

    #[test]
    fn required_trigrams_of_quantified_literal_excludes_the_quantified_tail() {
        // "createProgram*" — the required trigrams must not depend on the
        // optional trailing 'm' being present.
        let trigrams = required_trigrams("createProgram*");
        let excludes_tail = !trigrams.contains(&[b'r', b'a', b'm']);
        assert!(excludes_tail);
        assert!(trigrams.contains(&[b'c', b'r', b'e']));
    }
}
