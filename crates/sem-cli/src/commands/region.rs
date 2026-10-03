//! The analysis region of a diff-scoped `arch-diff`: which files of a huge
//! repository are analyzed, so that memory follows the size of the change
//! rather than the size of the repository.
//!
//! The region is the changed files plus every file that mentions, as an
//! identifier token:
//!
//! - **caller** names: a modified, deleted, renamed or moved entity's name.
//!   Every reference the resolver can make to such an entity is by name, so
//!   every static caller, and every competing definition the resolver
//!   weighs, mentions it. Callers resolved through scope and imports are
//!   therefore found as in a whole-tree run when the name is in the region.
//!   A caller attributed by the name-only fallback (receiver type unknown)
//!   can differ, since that fallback weighs the files it was given.
//! - **importer** stems: a changed file's stem (its directory for `index`,
//!   `mod`, `__init__`), for imports that bind a local name of their own
//!   (`import X from './file'`).
//! - **added** names: a new definition can capture references elsewhere.
//! - **callee** names: identifiers used by changed entities that at most
//!   [`CALLEE_MAX_FILES`] files mention (definitions of what the change calls;
//!   keywords and ubiquitous names drop out by count).
//!
//! Groups are admitted smallest first, callers before importers before added
//! names before callees, while the region stays within its byte budget. A
//! name whose files do not fit is recorded in [`Region::unexplored`] with the
//! number of files that mention it: what lies there is reported as unknown,
//! never as absent.
//!
//! The scan reads each file once and keeps, per file, only the seed names it
//! mentions: memory is the region plus that index.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use rayon::prelude::*;

use sem_core::model::change::{ChangeType, SemanticChange};

/// A callee name mentioned in more files than this is not followed: it is a
/// keyword, a builtin or a ubiquitous helper, and its definition is not what
/// makes the change's behavior.
pub(crate) const CALLEE_MAX_FILES: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Role {
    Caller,
    /// The other files of a changed code file's directory (its package):
    /// a package's dependencies are those of all its files.
    Sibling,
    Importer,
    Added,
    Callee,
}

impl Role {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Role::Caller => "callers",
            Role::Sibling => "files of a changed package",
            Role::Importer => "importers",
            Role::Added => "references to an added name",
            Role::Callee => "definitions of a called name",
        }
    }
}

pub(crate) struct Region {
    /// Repo-relative paths analyzed (in either tree).
    pub files: HashSet<String>,
    pub bytes: u64,
    pub repo_files: usize,
    pub repo_bytes: u64,
    /// Names whose mentioning files were not analyzed: (name, role, files
    /// mentioning it outside the region).
    pub unexplored: Vec<(String, Role, usize)>,
    pub budget: u64,
}

impl Region {
    /// Files outside the region that mention `name` (as a caller name),
    /// when its callers were not analyzed.
    pub(crate) fn unexplored_callers(&self, name: &str) -> Option<usize> {
        self.unexplored.iter().find(|(n, r, _)| n == name && *r == Role::Caller).map(|x| x.2)
    }
}

/// Identifier tokens (`[A-Za-z_$][A-Za-z0-9_$]*`) of `src`.
pub(crate) fn idents(src: &[u8], mut f: impl FnMut(&str)) {
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
    let mut i = 0;
    while i < src.len() {
        let c = src[i];
        if c.is_ascii_alphabetic() || c == b'_' || c == b'$' {
            let s = i;
            while i < src.len() && word(src[i]) {
                i += 1;
            }
            // ASCII only, so valid UTF-8
            f(std::str::from_utf8(&src[s..i]).unwrap_or(""));
        } else if c.is_ascii_digit() {
            while i < src.len() && word(src[i]) {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
}

fn stem(path: &str) -> Option<String> {
    let p = Path::new(path);
    let s = p.file_stem()?.to_string_lossy().to_string();
    let s = s.split('.').next().unwrap_or(&s).to_string();
    if matches!(s.as_str(), "index" | "mod" | "__init__" | "lib" | "main") {
        return p.parent()?.file_name().map(|d| d.to_string_lossy().to_string());
    }
    Some(s)
}

/// Source code (not data, markup or config): its names are code names.
pub(crate) fn is_code(path: &str) -> bool {
    sem_core::dataflow::ir::Lang::for_path(path).is_some()
        || [".java", ".kt", ".cs", ".rb", ".php", ".swift", ".c", ".cc", ".cpp", ".h", ".hpp", ".scala"].iter().any(|e| path.ends_with(e))
}

/// Seed names by role, from the semantic diff (code files only).
fn seeds(changes: &[SemanticChange], changed_files: &BTreeSet<String>) -> BTreeMap<String, Role> {
    let mut out: BTreeMap<String, Role> = BTreeMap::new();
    let mut put = |n: &str, r: Role| {
        if !n.is_empty() {
            let e = out.entry(n.to_string()).or_insert(r);
            *e = (*e).min(r);
        }
    };
    let changes: Vec<&SemanticChange> = changes.iter().filter(|c| is_code(&c.file_path)).collect();
    for c in &changes {
        let r = if matches!(c.change_type, ChangeType::Added) { Role::Added } else { Role::Caller };
        // a qualified name (`Type.method`, `Type::f`) is referenced by its last part
        let last = |n: &str| n.rsplit(['.', ':']).next().unwrap_or(n).to_string();
        put(&last(&c.entity_name), r);
        if let Some(o) = &c.old_entity_name {
            put(&last(o), Role::Caller);
        }
    }
    for f in changed_files.iter().filter(|f| is_code(f)) {
        if let Some(s) = stem(f) {
            put(&s, Role::Importer);
        }
    }
    for c in &changes {
        for body in [&c.before_content, &c.after_content].into_iter().flatten() {
            idents(body.as_bytes(), |t| put(t, Role::Callee));
        }
    }
    out
}

/// The region of a change. `trees`: (materialized dir, supported files) of
/// base and head; `changed`: paths the diff touches (either side).
pub(crate) fn select(trees: [(&Path, &[String]); 2], changed: &BTreeSet<String>, changes: &[SemanticChange], budget: u64) -> Region {
    // every path once, read from head when it exists there
    let mut paths: BTreeMap<&str, &Path> = BTreeMap::new();
    for (dir, files) in trees.iter().rev() {
        for f in files.iter() {
            paths.entry(f.as_str()).or_insert(*dir);
        }
    }
    let paths: Vec<(&str, &Path)> = paths.into_iter().collect();
    let seeds = seeds(changes, changed);
    let names: Vec<(&String, &Role)> = seeds.iter().collect();
    let id: HashMap<&str, u32> = names.iter().enumerate().map(|(i, (n, _))| (n.as_str(), i as u32)).collect();
    // per file: its size and the seed names it mentions
    let scanned: Vec<(u64, Vec<u32>)> = paths
        .par_iter()
        .map(|(p, dir)| {
            let Ok(src) = std::fs::read(dir.join(p)) else { return (0, Vec::new()) };
            let mut hit: Vec<u32> = Vec::new();
            idents(&src, |t| {
                if let Some(&i) = id.get(t) {
                    hit.push(i);
                }
            });
            hit.sort_unstable();
            hit.dedup();
            hit.shrink_to_fit();
            (src.len() as u64, hit)
        })
        .collect();
    let repo_bytes: u64 = scanned.iter().map(|x| x.0).sum();
    let mut mentions: Vec<Vec<u32>> = vec![Vec::new(); names.len()];
    for (fi, (_, hit)) in scanned.iter().enumerate() {
        for &n in hit {
            mentions[n as usize].push(fi as u32);
        }
    }
    let mut files: HashSet<String> = HashSet::new();
    let mut bytes = 0u64;
    let mut inside = vec![false; paths.len()];
    for (fi, (p, _)) in paths.iter().enumerate() {
        if changed.contains(*p) {
            inside[fi] = true;
            files.insert(p.to_string());
            bytes += scanned[fi].0;
        }
    }
    // groups: (label, role, files)
    let mut groups: Vec<(String, Role, Vec<u32>)> = names
        .iter()
        .zip(mentions)
        .filter(|((_, r), m)| **r != Role::Callee || m.len() <= CALLEE_MAX_FILES)
        .map(|((n, r), m)| ((*n).clone(), **r, m))
        .collect();
    let dirs: BTreeSet<&str> = changed.iter().filter(|f| is_code(f)).map(|f| f.rsplit_once('/').map_or("", |x| x.0)).collect();
    let mut by_dir: BTreeMap<&str, Vec<u32>> = BTreeMap::new();
    for (fi, (p, _)) in paths.iter().enumerate() {
        let d = p.rsplit_once('/').map_or("", |x| x.0);
        if dirs.contains(d) && is_code(p) {
            by_dir.entry(d).or_default().push(fi as u32);
        }
    }
    groups.extend(by_dir.into_iter().map(|(d, fs)| (format!("{d}/"), Role::Sibling, fs)));
    groups.sort_by(|a, b| (a.1, a.2.len(), &a.0).cmp(&(b.1, b.2.len(), &b.0)));
    let mut unexplored = Vec::new();
    for (label, role, group) in groups {
        let new: Vec<u32> = group.into_iter().filter(|&f| !inside[f as usize]).collect();
        if new.is_empty() {
            continue;
        }
        let add: u64 = new.iter().map(|&f| scanned[f as usize].0).sum();
        if bytes + add > budget {
            unexplored.push((label, role, new.len()));
            continue;
        }
        bytes += add;
        for f in new {
            inside[f as usize] = true;
            files.insert(paths[f as usize].0.to_string());
        }
    }
    Region { files, bytes, repo_files: paths.len(), repo_bytes, unexplored, budget }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_tokens() {
        let mut v = Vec::new();
        idents(b"fn get_user(x: u32) -> $el { a.b_2(1e3, 0x1f) }", |t| v.push(t.to_string()));
        assert_eq!(v, ["fn", "get_user", "x", "u32", "$el", "a", "b_2"]);
    }

    #[test]
    fn stems_of_index_files_are_their_directory() {
        assert_eq!(stem("src/utils/index.ts").as_deref(), Some("utils"));
        assert_eq!(stem("pkg/a/__init__.py").as_deref(), Some("a"));
        assert_eq!(stem("src/foo.test.ts").as_deref(), Some("foo"));
    }
}
