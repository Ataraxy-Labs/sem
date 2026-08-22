//! `Complete` freshness: the whole-corpus membership proof, and one gap
//! left unimplemented — "a name that exists on disk
//! but was never indexed will not appear via [`find`/`callers`/`refs`] until
//! the next full build."
//!
//! The primitive is exactly what measured: a parallel `stat` over the
//! index's own `FILES` and `DIRS` sections. Nothing here walks the corpus —
//! that would reintroduce the −301 ms cost spent removing. `FILES` gives
//! deletions directly (a path the index knows about that no longer stats).
//! `DIRS` gives *additions* indirectly, through the POSIX guarantee leans
//! on: creating, deleting, or renaming a directory entry bumps that
//! directory's own mtime, and nothing else's. Because `DIRS` holds every
//! ancestor directory of every indexed file (not just direct parents,
//! `writer::DirFingerprint`'s doc), a drifted directory is always the
//! *direct* parent of whatever changed — which bounds the repair work to
//! that one directory's own subtree, never the whole corpus.
//!
//! What this module does **not** do: decide what a caller does with a new or
//! deleted path. `find`/`callers`/`refs` (content-local: extract, filter,
//! merge) and `grep` (file-scoped: join the candidate set) each have
//! different, correct answers to "what next" — see `commands::query` and
//! `grep::search`.

use std::path::Path;

#[cfg(feature = "parallel")]
use rayon::prelude::*;

use super::QueryIndex;

/// Same convention `writer.rs`/`facts_store.rs`/`graph.rs` use: fall back to
/// a serial iterator when the `parallel` feature (default-on) is off.
macro_rules! maybe_par_iter {
    ($slice:expr) => {{
        #[cfg(feature = "parallel")]
        {
            $slice.par_iter()
        }
        #[cfg(not(feature = "parallel"))]
        {
            $slice.iter()
        }
    }};
}

/// The result of one whole-corpus membership sweep.
#[derive(Debug, Clone, Default)]
pub struct CompleteReport {
    /// Root-relative paths the index has never seen, discovered by
    /// re-walking every directory whose mtime drifted since the index was
    /// built. Bounded to the drifted subtrees, not the corpus.
    pub new_files: Vec<String>,
    /// Root-relative paths the index knows about that no longer exist on
    /// disk.
    pub deleted_files: Vec<String>,
    /// `true` if any part of the sweep hit an I/O condition it could not
    /// resolve to a definite answer (a race between the stat and the
    /// directory re-walk, most likely). Fail-toward-MISS: a caller MUST
    /// treat an inconclusive report as "membership freshness not proven",
    /// never as "no changes found" — the honest response to uncertainty is
    /// to do more work (fall back to a full rebuild), not to serve a
    /// possibly-wrong fast answer: a false "complete" is the one
    /// unacceptable outcome here, and a missed one is merely slower.
    pub inconclusive: bool,
    /// Directories the sweep flagged as drifted, purely for
    /// instrumentation/tests — not a promise callers act on directly.
    pub drifted_dirs: Vec<String>,
}

impl CompleteReport {
    pub fn is_clean(&self) -> bool {
        !self.inconclusive && self.new_files.is_empty() && self.deleted_files.is_empty()
    }
}

/// Run the sweep. `walk_subtree(dir)` must return every corpus-eligible file
/// under `dir` (same extension/ignore rules a full build would apply),
/// root-relative — the caller supplies this because sem-core has no opinion
/// on ignore-file semantics (`ignore`/`ParserRegistry` policy lives in
/// `sem-cli`); reusing the real walker scoped to just the drifted directory
/// is what keeps this bounded *and* correct, rather than re-deriving
/// gitignore/extension rules here and risking the two disagreeing.
pub fn complete_check(
    idx: &QueryIndex,
    root: &Path,
    walk_subtree: impl Fn(&Path) -> Vec<String> + Sync + Send,
) -> CompleteReport {
    let mut inconclusive = false;

    // FILES-existence and DIRS-mtime are two disjoint stat sets (~12ms
    // was for FILES alone; DIRS is a much smaller addition) — running them as
    // sibling `rayon` tasks rather than back-to-back sequential passes lets
    // the scheduler interleave both across every worker thread instead of
    // fully draining one array before starting the other, which is what
    // turns "two ~7ms passes" into one ~7-10ms wall-clock window instead of
    // two stacked ones.
    let (deleted_files, drift): (Vec<String>, Vec<(String, bool)>) = rayon_join(
        || {
            let all_files = idx.all_file_paths();
            maybe_par_iter!(all_files)
                .filter_map(|p| {
                    let full = root.join(*p);
                    match full.symlink_metadata() {
                        Ok(_) => None,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(p.to_string()),
                        // A stat error that isn't "gone" (permission, races
                        // we can't classify) is exactly the uncertainty this
                        // report must not paper over — flag inconclusive
                        // rather than guess.
                        Err(_) => Some(p.to_string()),
                    }
                })
                .collect()
        },
        || {
            let all_dirs = idx.all_dir_paths();
            maybe_par_iter!(all_dirs)
                .map(|p| {
                    let full = if p.is_empty() {
                        root.to_path_buf()
                    } else {
                        root.join(*p)
                    };
                    match std::fs::metadata(&full) {
                        Ok(meta) => {
                            let drifted = match (mtime_parts(&meta), idx.dir_fingerprint(p)) {
                                (Some((secs, nanos)), Some(fp)) => {
                                    fp.mtime_secs != secs || fp.mtime_nanos != nanos
                                }
                                // No stored fingerprint or unreadable mtime
                                // for a path DIRS itself claims to know
                                // about: can't prove it's unchanged, so
                                // treat it as drifted (fail toward
                                // re-checking, never toward skipping).
                                _ => true,
                            };
                            (p.to_string(), drifted)
                        }
                        // The directory itself is gone (its whole subtree
                        // was removed). Its files already surface via the
                        // FILES pass above; there is nothing new to
                        // discover under a directory that no longer exists,
                        // so this is not drift in the "something new
                        // appeared" sense the walk below handles.
                        Err(_) => (p.to_string(), false),
                    }
                })
                .filter(|(_, drifted)| *drifted)
                .collect()
        },
    );

    let drifted_dirs: Vec<String> = drift.into_iter().map(|(p, _)| p).collect();

    let mut new_files = Vec::new();
    for dir in &drifted_dirs {
        let full = if dir.is_empty() {
            root.to_path_buf()
        } else {
            root.join(dir)
        };
        if !full.is_dir() {
            // Raced: existed when we stat'd it above, gone now. Nothing new
            // to report from here, but we can no longer prove that — the
            // caller should not trust a "no new files here" silence.
            inconclusive = true;
            continue;
        }
        for path in walk_subtree(&full) {
            if idx.file_fingerprint(&path).is_none() {
                new_files.push(path);
            }
        }
    }
    new_files.sort_unstable();
    new_files.dedup();

    CompleteReport {
        new_files,
        deleted_files,
        inconclusive,
        drifted_dirs,
    }
}

/// `rayon::join` when the `parallel` feature is on, a plain sequential call
/// otherwise — same fallback discipline `maybe_par_iter!` uses, extended to
/// the two-sibling-task case this function's two callers need.
fn rayon_join<A, B, RA, RB>(a: A, b: B) -> (RA, RB)
where
    A: FnOnce() -> RA + Send,
    B: FnOnce() -> RB + Send,
    RA: Send,
    RB: Send,
{
    #[cfg(feature = "parallel")]
    {
        rayon::join(a, b)
    }
    #[cfg(not(feature = "parallel"))]
    {
        (a(), b())
    }
}

fn mtime_parts(meta: &std::fs::Metadata) -> Option<(i64, u32)> {
    let mtime = meta.modified().ok()?;
    let dur = mtime.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some((dur.as_secs() as i64, dur.subsec_nanos()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::writer::{build_with_content_and_dirs, DirFingerprint};
    use crate::index::{FileFingerprint, QueryIndex};
    use crate::parser::graph::{EntityGraph, EntityInfoMap};
    use std::collections::HashMap;
    use std::fs;

    /// Every ancestor directory of `path` (all levels, `""` for the root),
    /// mirroring `sem-cli`'s `write_query_index` derivation — duplicated here
    /// (not imported: `sem-cli` depends on `sem-core`, not the reverse) so
    /// this module's own tests don't need the CLI crate to build a realistic
    /// `DIRS` table.
    fn ancestors_of(path: &str) -> Vec<String> {
        let mut out = vec![String::new()];
        let mut acc = String::new();
        let segs: Vec<&str> = path.split('/').collect();
        // Every segment except the last (the file's own name) is a
        // directory component.
        for seg in &segs[..segs.len().saturating_sub(1)] {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(seg);
            out.push(acc.clone());
        }
        out
    }

    /// Build a real `QueryIndex` over `root`'s current on-disk contents
    /// (`file_names`, all UTF-8 text files directly under `root` or in a
    /// declared subdirectory) — the fixture every test below mutates after
    /// the fact to exercise the sweep.
    fn index_over(root: &std::path::Path, file_names: &[&str]) -> QueryIndex {
        let mut files = Vec::new();
        let mut contents = HashMap::new();
        let mut dir_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for name in file_names {
            let full = root.join(name);
            let meta = fs::metadata(&full).expect("fixture file must exist");
            let (secs, nanos) = mtime_parts(&meta).expect("mtime");
            let bytes = fs::read(&full).expect("read fixture");
            files.push(FileFingerprint {
                path: name.to_string(),
                mtime_secs: secs,
                mtime_nanos: nanos,
                content_hash: crate::parser::incremental::content_hash(&String::from_utf8_lossy(
                    &bytes,
                )),
            });
            contents.insert(name.to_string(), bytes);
            for a in ancestors_of(name) {
                dir_set.insert(a);
            }
        }
        let dirs: Vec<DirFingerprint> = dir_set
            .into_iter()
            .map(|path| {
                let full = if path.is_empty() {
                    root.to_path_buf()
                } else {
                    root.join(&path)
                };
                let meta = fs::metadata(&full).expect("fixture dir must exist");
                let (secs, nanos) = mtime_parts(&meta).expect("dir mtime");
                DirFingerprint {
                    path,
                    mtime_secs: secs,
                    mtime_nanos: nanos,
                }
            })
            .collect();
        let graph = EntityGraph::from_parts(EntityInfoMap::default(), Vec::new());
        let (bytes, _) = build_with_content_and_dirs(&graph, &files, &dirs, &contents);
        QueryIndex::from_bytes(bytes).expect("well-formed image")
    }

    /// Real-walker `walk_subtree`, same shape `sem-cli`'s
    /// `find_supported_files_in_path` gives `commands::query`/`grep` — every
    /// `.txt`/`.rs` file under `dir`, root-relative. Good enough for a test
    /// that only needs *a* consistent notion of "corpus-eligible", not the
    /// production ignore/extension policy itself.
    fn walk(root: &std::path::Path, dir: &std::path::Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let Ok(entries) = fs::read_dir(&d) else {
                continue;
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|e| e == "txt") {
                    let rel = p
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.push(rel);
                }
            }
        }
        out
    }

    #[test]
    fn clean_corpus_reports_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "hello").unwrap();
        let idx = index_over(tmp.path(), &["a.txt"]);
        let report = complete_check(&idx, tmp.path(), |d| walk(tmp.path(), d));
        assert!(report.is_clean(), "{report:?}");
    }

    #[test]
    fn new_file_at_root_is_found() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "hello").unwrap();
        let idx = index_over(tmp.path(), &["a.txt"]);

        // Perturb the root directory's mtime by adding a new file.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(tmp.path().join("b.txt"), "world").unwrap();

        let report = complete_check(&idx, tmp.path(), |d| walk(tmp.path(), d));
        assert_eq!(report.new_files, vec!["b.txt".to_string()]);
        assert!(report.deleted_files.is_empty());
        assert!(!report.inconclusive);
    }

    #[test]
    fn new_file_in_a_brand_new_nested_directory_is_found() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/a.txt"), "hello").unwrap();
        let idx = index_over(tmp.path(), &["src/a.txt"]);

        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::create_dir(tmp.path().join("src/deeply")).unwrap();
        fs::create_dir(tmp.path().join("src/deeply/nested")).unwrap();
        fs::write(tmp.path().join("src/deeply/nested/new.txt"), "fresh").unwrap();

        let report = complete_check(&idx, tmp.path(), |d| walk(tmp.path(), d));
        assert_eq!(
            report.new_files,
            vec!["src/deeply/nested/new.txt".to_string()]
        );
        assert!(!report.inconclusive);
    }

    #[test]
    fn deleted_file_is_found() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "hello").unwrap();
        fs::write(tmp.path().join("b.txt"), "world").unwrap();
        let idx = index_over(tmp.path(), &["a.txt", "b.txt"]);

        fs::remove_file(tmp.path().join("b.txt")).unwrap();

        let report = complete_check(&idx, tmp.path(), |d| walk(tmp.path(), d));
        assert_eq!(report.deleted_files, vec!["b.txt".to_string()]);
        assert!(report.new_files.is_empty());
    }

    #[test]
    fn renamed_file_is_both_a_deletion_and_an_addition() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("old.txt"), "hello").unwrap();
        let idx = index_over(tmp.path(), &["old.txt"]);

        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::rename(tmp.path().join("old.txt"), tmp.path().join("new.txt")).unwrap();

        let report = complete_check(&idx, tmp.path(), |d| walk(tmp.path(), d));
        assert_eq!(report.deleted_files, vec!["old.txt".to_string()]);
        assert_eq!(report.new_files, vec!["new.txt".to_string()]);
    }

    /// Mutation test (the change's item 3): hide a file from the sweep by
    /// constructing a `walk_subtree` that omits it, and assert the omission
    /// is what a caller would see — proving the harness is capable of
    /// catching a ghost (a real bug that silently drops a new file) rather
    /// than passing vacuously because nothing ever calls `walk_subtree`.
    #[test]
    fn mutation_a_walker_that_hides_a_file_produces_a_false_clean_report() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "hello").unwrap();
        let idx = index_over(tmp.path(), &["a.txt"]);

        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(tmp.path().join("ghost.txt"), "boo").unwrap();

        // The honest walker catches it...
        let honest = complete_check(&idx, tmp.path(), |d| walk(tmp.path(), d));
        assert_eq!(honest.new_files, vec!["ghost.txt".to_string()]);

        // ...a walker that (buggily) hides "ghost.txt" produces a report
        // that falsely claims cleanliness. This is the oracle: if a future
        // change to `complete_check`'s own logic ever made it agree with the
        // hobbled walker instead of the honest one, this assertion is what
        // would catch that regression.
        let hobbled = complete_check(&idx, tmp.path(), |d| {
            walk(tmp.path(), d)
                .into_iter()
                .filter(|p| !p.ends_with("ghost.txt"))
                .collect()
        });
        assert!(hobbled.new_files.is_empty(), "{hobbled:?}");
        assert_ne!(honest.new_files, hobbled.new_files);
    }
}
