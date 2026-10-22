//! Edit follow-through, the post-edit half of `sem hook tool`: after the
//! agent's own edit tool (Edit, Write, MultiEdit, Codex apply_patch) changed
//! a file, what that edit left undone, from the entity graph:
//!
//! ```text
//! twins            other overrides / implementations of a changed method
//!                  (same dispatch group) the session has not touched
//! stranded callers call sites of an entity whose signature changed, or that
//!                  was deleted or renamed, outside the edited ranges
//! tests            the tests that reach the changed entities, with a command
//! ```
//!
//! The file's content before the edit comes from the snapshot the PreToolUse
//! half took (`<git dir>/sem-run/snapshots`), else from HEAD. Answers come
//! from the query index; a repo without one gets a cold build only while it
//! fits the budget. The whole analysis has `BUDGET`; what does not fit is
//! skipped, and nothing to say means no output at all.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use regex::Regex;
use sem_core::git::types::{FileChange, FileStatus};
use sem_core::index::{self, QueryIndex};
use sem_core::model::change::{ChangeType, SemanticChange};
use sem_core::parser::graph::RefType;

const BUDGET: Duration = Duration::from_millis(1500);
/// Rows per line (twins, call sites, tests).
const MAX: usize = 5;
const MAX_LINES: usize = 12;
const MAX_SNAPSHOT: u64 = 2 << 20;

/// One file the edit changed: repo-relative path and its content before.
pub(crate) struct Edited {
    pub rel: String,
    pub before: Option<String>,
}

/// `file` (absolute, or relative to `cwd`) relative to `root`, `/`-separated.
pub(crate) fn relative(root: &Path, cwd: &Path, file: &str) -> Option<String> {
    let p = cwd.join(file);
    // a file Write is about to create does not exist yet: resolve its parent
    let p = std::fs::canonicalize(&p).ok().or_else(|| Some(std::fs::canonicalize(p.parent()?).ok()?.join(p.file_name()?)))?;
    let root = std::fs::canonicalize(root).ok()?;
    let rel = p.strip_prefix(&root).ok()?.to_string_lossy().replace('\\', "/");
    (!rel.is_empty() && !rel.starts_with(".git/")).then_some(rel)
}

fn snapshot_path(run: &Path, session: &str, rel: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (session, rel).hash(&mut h);
    run.join("snapshots").join(format!("{:016x}", h.finish()))
}

/// Keep `rel`'s current content (empty for a file that does not exist yet)
/// until the PostToolUse half asks for it.
pub(crate) fn snapshot(run: &Path, session: &str, root: &Path, rel: &str) {
    let full = root.join(rel);
    if std::fs::metadata(&full).is_ok_and(|m| m.len() > MAX_SNAPSHOT) {
        return;
    }
    let text = std::fs::read_to_string(&full).unwrap_or_default();
    let path = snapshot_path(run, session, rel);
    let dir = path.parent().unwrap_or(run);
    let _ = std::fs::create_dir_all(dir);
    // a harness without the PostToolUse half never consumes them
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|age| age > Duration::from_secs(6 * 3600))) {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let _ = std::fs::write(path, text);
}

/// The snapshot of `rel` (consumed), else its content at HEAD.
pub(crate) fn before(run: Option<&Path>, session: &str, root: &Path, rel: &str) -> Option<String> {
    if let Some(path) = run.map(|r| snapshot_path(r, session, rel)) {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let _ = std::fs::remove_file(&path);
            return Some(text);
        }
    }
    let o = std::process::Command::new("git").args(["show", &format!("HEAD:{rel}")]).current_dir(root).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Files an apply_patch body adds or updates.
pub(crate) fn patch_files(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.strip_prefix("*** Update File: ").or(l.strip_prefix("*** Add File: ")))
        .map(|f| f.trim().to_string())
        .collect()
}

/// A definition's signature: its text up to where the body starts, without
/// decorators and attributes, whitespace collapsed.
fn signature(content: &str) -> String {
    let text: String = content
        .lines()
        .skip_while(|l| l.trim_start().starts_with('@') || l.trim_start().starts_with("#["))
        .collect::<Vec<_>>()
        .join("\n");
    let cut = ["{", ":\n", "=>", " where "].iter().filter_map(|m| text.find(m)).min().unwrap_or(text.len().min(400));
    text[..cut].split_whitespace().collect::<Vec<_>>().join(" ")
}

fn callable(entity_type: &str) -> bool {
    entity_type.contains("function") || entity_type.contains("method") || entity_type == "constructor"
}

/// The command that runs `tests` (`(file, owners, name)`), when the
/// framework is recognisable from the file type and the repo.
fn test_command(root: &Path, tests: &[(String, Vec<String>, String)]) -> Option<String> {
    let ext = tests.first()?.0.rsplit('.').next()?.to_string();
    if !tests.iter().all(|t| t.0.ends_with(&format!(".{ext}"))) {
        return None;
    }
    let files = || {
        let mut f: Vec<&str> = tests.iter().map(|t| t.0.as_str()).collect();
        f.dedup();
        f.join(" ")
    };
    match ext.as_str() {
        "py" => Some(format!(
            "pytest {}",
            tests.iter().map(|(f, o, n)| o.iter().rev().fold(f.clone(), |acc, c| format!("{acc}::{c}")) + "::" + n).collect::<Vec<_>>().join(" ")
        )),
        "rs" => Some(format!("cargo test -- {}", tests.iter().map(|t| t.2.as_str()).collect::<Vec<_>>().join(" "))),
        "go" => {
            let dir = |f: &str| f.rsplit_once('/').map_or(".".to_string(), |(d, _)| format!("./{d}"));
            let d = dir(&tests[0].0);
            tests.iter().all(|t| dir(&t.0) == d).then(|| format!("go test {d} -run '^({})$'", tests.iter().map(|t| t.2.as_str()).collect::<Vec<_>>().join("|")))
        }
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => {
            let pkg = std::fs::read_to_string(root.join("package.json")).unwrap_or_default();
            if pkg.contains("\"vitest\"") {
                Some(format!("npx vitest run {}", files()))
            } else if pkg.contains("\"jest\"") {
                Some(format!("npx jest {}", files()))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The repo's query index; without one, a cold build (which writes it) as
/// long as it finishes before `deadline`.
fn index_by(root: &Path, deadline: Instant) -> Option<QueryIndex> {
    if let Some(idx) = super::query::open_index(root).filter(QueryIndex::has_refs) {
        return Some(idx);
    }
    if std::env::var_os("SEM_NO_INDEX").is_some() {
        return None;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let r = root.to_path_buf();
    std::thread::spawn(move || {
        let ok = std::panic::catch_unwind(|| {
            let registry = super::create_registry(&r.to_string_lossy());
            let files = super::graph::find_supported_files_with_options(&r, &registry, &[], false);
            let scope = super::graph::cache_source_scope(&r, &[], false);
            super::graph::get_or_build_graph(&r, &files, &registry, false, scope);
        });
        let _ = tx.send(ok.is_ok());
    });
    // the index is written by temp file + rename: a build cut off when the
    // hook exits leaves no torn index behind
    rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).ok().filter(|ok| *ok)?;
    super::query::open_index(root).filter(QueryIndex::has_refs)
}

/// Entity ids touched and lines already said this session.
struct Ledger {
    path: Option<PathBuf>,
    touched: HashSet<String>,
    said: HashSet<String>,
}

impl Ledger {
    fn open(run: Option<&Path>, session: &str) -> Ledger {
        let path = run.map(|r| r.join("follow").join(if session.is_empty() { "none".to_string() } else { session.replace(['/', '\\'], "_") }));
        let (mut touched, mut said) = (HashSet::new(), HashSet::new());
        for l in path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default().lines() {
            match l.split_once(' ') {
                Some(("T", id)) => touched.insert(id.to_string()),
                Some(("S", s)) => said.insert(s.to_string()),
                _ => false,
            };
        }
        Ledger { path, touched, said }
    }

    fn append(&self, rows: &[String]) {
        let Some(path) = &self.path else { return };
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(path));
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            for r in rows {
                let _ = writeln!(f, "{r}");
            }
        }
    }
}

/// What the edit to `files` left undone, as compact `sem:` lines (None:
/// nothing to say), plus a note for the coverage log.
pub(crate) fn follow_through(root: &Path, session: &str, run: Option<&Path>, files: &[Edited], started: Instant) -> (Option<String>, String) {
    let deadline = started + BUDGET;
    let over = || Instant::now() >= deadline;
    let registry = super::create_registry(&root.to_string_lossy());
    let mut afters: Vec<(String, String)> = Vec::new();
    let changes_in: Vec<FileChange> = files
        .iter()
        .filter_map(|e| {
            let after = std::fs::read_to_string(root.join(&e.rel)).ok()?;
            afters.push((e.rel.clone(), after.clone()));
            Some(FileChange {
                file_path: e.rel.clone(),
                status: if e.before.is_some() { FileStatus::Modified } else { FileStatus::Added },
                old_file_path: None,
                before_content: e.before.clone(),
                after_content: Some(after),
            })
        })
        .collect();
    let changes: Vec<SemanticChange> = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sem_core::parser::differ::compute_semantic_diff(&changes_in, &registry, None, None).changes))
        .unwrap_or_default()
        .into_iter()
        .filter(|c| callable(&c.entity_type) && !matches!(c.change_type, ChangeType::Added | ChangeType::Reordered | ChangeType::Moved))
        .take(10)
        .collect();
    if changes.is_empty() {
        return (None, "no entity changed".into());
    }
    let mut ledger = Ledger::open(run, session);
    let mut rows: Vec<String> = changes.iter().map(|c| format!("T {}", c.entity_id)).collect();
    ledger.touched.extend(changes.iter().map(|c| c.entity_id.clone()));
    let Some(idx) = index_by(root, deadline - BUDGET / 5) else {
        ledger.append(&rows);
        return (None, "no index in budget".into());
    };
    // spans of the edited files as they are now (the index has the old ones)
    let now_spans: Vec<(String, String, usize, usize)> = afters
        .iter()
        .flat_map(|(rel, text)| registry.extract_entities_brief(rel, text).into_iter().map(move |e| (rel.clone(), e.id, e.start_line, e.end_line)))
        .collect();
    let edited: HashSet<&str> = files.iter().map(|e| e.rel.as_str()).collect();
    let changed_ranges: Vec<(&str, usize, usize)> = changes.iter().map(|c| (c.file_path.as_str(), c.start_line, c.end_line)).collect();
    let display = |at: usize| super::qualified::display_name(&super::qualified::index_owners(&idx, at), idx.entity(at).name());
    let locate = |c: &SemanticChange| -> Option<usize> {
        if let Some(at) = super::query::resolve_by_id_index(&idx, &c.entity_id) {
            return Some(at);
        }
        let name = c.old_entity_name.as_deref().unwrap_or(&c.entity_name);
        let hits: Vec<usize> = idx.lookup(name).iter().filter(|e| e.file_path() == c.file_path && callable(e.entity_type())).map(index::Entity::index).collect();
        (hits.len() == 1).then(|| hits[0])
    };
    let dispatch = |at: usize, out: bool| -> Vec<usize> {
        let name = idx.entity(at).name();
        let row = if out { idx.refs_of_typed(at) } else { idx.callers_of_typed(at) };
        row.into_iter().filter(|(e, k)| *k == RefType::Dispatch && e.name() == name).map(|(e, _)| e.index()).collect()
    };
    let mut lines: Vec<String> = Vec::new();
    let mut walked: Vec<usize> = Vec::new();

    // twins: the other members of each changed method's dispatch group
    for c in changes.iter().filter(|c| c.change_type == ChangeType::Modified) {
        if over() {
            break;
        }
        let Some(at) = locate(c) else { continue };
        walked.push(at);
        let roots = dispatch(at, false);
        let mut twins: Vec<usize> = roots.iter().flat_map(|&r| dispatch(r, true)).chain(dispatch(at, true)).collect();
        let mut seen = HashSet::new();
        twins.retain(|&t| t != at && !roots.contains(&t) && seen.insert(t) && !ledger.touched.contains(&idx.entity(t).id()));
        if twins.is_empty() {
            continue;
        }
        let shown: Vec<String> = twins.iter().take(MAX).map(|&t| format!("{}:{} {}", idx.entity(t).file_path(), idx.entity(t).start_line(), display(t))).collect();
        let more = if twins.len() > MAX { format!(" (+{} more)", twins.len() - MAX) } else { String::new() };
        lines.push(format!("sem: you changed {} ({}). Its twins were not touched: {}{more}", display(at), c.file_path, shown.join(", ")));
    }

    // stranded callers: a new signature, a deletion or a rename
    let mut texts: std::collections::HashMap<String, String> = Default::default();
    for c in &changes {
        if over() {
            break;
        }
        let why = match c.change_type {
            ChangeType::Deleted => "was deleted",
            ChangeType::Renamed => "was renamed",
            ChangeType::Modified if signature(c.before_content.as_deref().unwrap_or("")) != signature(c.after_content.as_deref().unwrap_or("")) => "changed its signature",
            _ => continue,
        };
        let Some(at) = locate(c) else { continue };
        if !walked.contains(&at) {
            walked.push(at);
        }
        let name = idx.entity(at).name().to_string();
        // a call answered with the base method may run this override
        let targets: Vec<usize> = std::iter::once(at).chain(if c.change_type == ChangeType::Modified { dispatch(at, false) } else { Vec::new() }).collect();
        let call = Regex::new(&format!(r"\b{}\s*\(", regex::escape(&name))).ok();
        let mut sites: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        for (caller, _) in targets.iter().flat_map(|&t| idx.callers_of_typed(t)).filter(|(_, k)| matches!(k, RefType::Calls | RefType::Refs)) {
            let (file, id) = (caller.file_path().to_string(), caller.id());
            if !seen.insert(id.clone()) || changes.iter().any(|c| c.entity_id == id) {
                continue;
            }
            let (start, end) = if edited.contains(file.as_str()) {
                match now_spans.iter().find(|s| s.0 == file && s.1 == id) {
                    Some(s) => (s.2, s.3),
                    None => continue, // gone from the edited file
                }
            } else {
                (caller.start_line(), caller.end_line())
            };
            let text = texts.entry(file.clone()).or_insert_with(|| std::fs::read_to_string(root.join(&file)).unwrap_or_default());
            let line = text
                .lines()
                .enumerate()
                .skip(start.saturating_sub(1))
                .take(end + 1 - start.min(end))
                .find(|(_, l)| call.as_ref().is_some_and(|re| re.is_match(l)))
                .map_or(start, |(i, _)| i + 1);
            if changed_ranges.iter().any(|(f, a, b)| *f == file && (*a..=*b).contains(&line)) {
                continue;
            }
            sites.push(format!("{file}:{line}"));
        }
        if sites.is_empty() {
            continue;
        }
        let n = sites.len();
        let more = if n > MAX { format!(" (+{} more)", n - MAX) } else { String::new() };
        let what = if c.change_type == ChangeType::Modified { "the old signature".to_string() } else { format!("`{name}`") };
        lines.push(format!(
            "sem: {} {why}; {n} caller{} still use{} {what}: {}{more}",
            display(at),
            if n == 1 { "" } else { "s" },
            if n == 1 { "s" } else { "" },
            sites.iter().take(MAX).cloned().collect::<Vec<_>>().join(", ")
        ));
    }

    // tests that reach the changed entities
    if !over() && !walked.is_empty() {
        let mut tests: Vec<(String, Vec<String>, String)> = Vec::new();
        let mut total = 0;
        let mut seen = HashSet::new();
        for &at in &walked {
            for (i, _) in super::impact::index_impact_ids(&idx, at, 0, Some(2000)) {
                let e = idx.entity(i);
                let test = if idx.has_test_flags() { e.is_test() } else { sem_core::parser::test_detect::is_test_path(e.file_path()) && e.name().starts_with("test") };
                if !test || !callable(e.entity_type()) || !seen.insert(i) {
                    continue;
                }
                total += 1;
                if tests.len() < MAX {
                    tests.push((e.file_path().to_string(), super::qualified::index_owners(&idx, i).iter().map(|s| s.to_string()).collect(), e.name().to_string()));
                }
            }
        }
        if !tests.is_empty() {
            let shown: Vec<String> = tests.iter().map(|(f, o, n)| format!("{f}::{}", super::qualified::display_name(&o.iter().map(String::as_str).collect::<Vec<_>>(), n))).collect();
            let more = if total > MAX { format!(" (+{} more)", total - MAX) } else { String::new() };
            let run = test_command(root, &tests).map(|c| format!(" (run: {c})")).unwrap_or_default();
            lines.push(format!("sem: tests that reach this change: {}{more}{run}", shown.join(", ")));
        }
    }

    lines.retain(|l| !ledger.said.contains(l));
    lines.truncate(MAX_LINES);
    rows.extend(lines.iter().map(|l| format!("S {l}")));
    ledger.append(&rows);
    ledger.said.extend(lines.iter().cloned());
    let note = format!("{} entities, {} lines, {}ms{}", changes.len(), lines.len(), started.elapsed().as_millis(), if over() { ", over budget" } else { "" });
    ((!lines.is_empty()).then(|| lines.join("\n")), note)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_ignore_bodies_and_decorators() {
        assert_eq!(signature("@abstractmethod\ndef get(self, key):\n    return 1\n"), "def get(self, key)");
        assert_eq!(signature("def get(self, key):\n    return 2\n"), signature("def get(self,  key):\n    pass\n"));
        assert_ne!(signature("def get(self, key):\n"), signature("def get(self, key, default=None):\n"));
        assert_eq!(signature("pub fn f(a: u8) -> u8 {\n    a\n}"), "pub fn f(a: u8) -> u8");
    }

    #[test]
    fn test_commands_follow_the_framework() {
        let root = std::env::temp_dir();
        let py = vec![("tests/test_cache.py".to_string(), vec!["TestCache".to_string()], "test_get".to_string())];
        assert_eq!(test_command(&root, &py).unwrap(), "pytest tests/test_cache.py::TestCache::test_get");
        let go = vec![("pkg/a_test.go".to_string(), vec![], "TestA".to_string()), ("pkg/b_test.go".to_string(), vec![], "TestB".to_string())];
        assert_eq!(test_command(&root, &go).unwrap(), "go test ./pkg -run '^(TestA|TestB)$'");
        assert_eq!(patch_files("*** Begin Patch\n*** Update File: a.py\n@@\n-x\n+y\n*** Add File: b.py\n+z\n"), vec!["a.py", "b.py"]);
    }
}
