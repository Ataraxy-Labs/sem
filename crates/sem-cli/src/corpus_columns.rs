//! The build's **one** post-graph corpus read, and the columns derived from
//! it (semx-3tb / W1; the design is `sem-core/SINGLE-PASS.md`, this is §3's
//! columnar form and §2's collapse of census passes H, I and L).
//!
//! Before this module the save path read every file's bytes **three** times:
//!
//! | pass | was | read for |
//! |---|---|---|
//! | H | `build_cache.rs`'s `file_fingerprint` re-read | `cache.db`'s `files.content_hash` (hex) |
//! | I | `shared_cache::refresh_file_import_entries`'s own re-read | `file_imports` rows |
//! | L | `write_query_index`'s re-read | the index's `content_hash` (u64) + `TRIGRAM` bytes |
//!
//! Three reads, two xxh3 computations of the same bytes, and a whole-corpus
//! `HashMap<String, Vec<u8>>` held live through the index build (1.5 GB on
//! linux) — all of it derivable from one visit. Two facts collapse it:
//!
//! 1. **The two "different" hashes are one number.**
//!    `shared_cache::file_content_hash` is
//!    `format!("{:016x}", xxh3_64(bytes))` (`sem-core/src/utils/hash.rs:9`)
//!    and `parser::incremental::content_hash` is
//!    `Xxh3::new().update(bytes).digest()` (`incremental.rs:440`) — the same
//!    xxh3-64 of the same bytes, one rendered hex. So `hash` below is
//!    computed once and serves both artifacts; `hex16` is the injection into
//!    `cache.db`'s column. (Both this file's predecessor comment and
//!    semx-ccg's commit message asserted they were distinct hashes. They are
//!    not; `SINGLE-PASS.md` §1.3 has the derivation, and
//!    `hash_encoding_identity` in `build_cache.rs`'s tests witnesses it.)
//! 2. **Trigrams and import scans are folds over those same bytes.** By the
//!    fold-fusion invariant (`⟨cata f, cata g⟩ = cata ⟨f,g⟩`) they are components
//!    of one walk, so they run inside the read closure — and the bytes are
//!    dropped *there*, never collected (`SINGLE-PASS.md` §1.1 S2: the byte
//!    string is the fused walk's intermediate, not a column).
//!
//! Behaviour is preserved file-for-file, including the two asymmetries the
//! old three-read shape had and which are easy to lose in a fusion:
//!
//! * `cache.db`'s `files` row wants only *readable bytes* (pass H used
//!   `std::fs::read`), while the index fingerprint and `TRIGRAM` want
//!   *readable UTF-8* (pass L used `read_to_string`, so a non-UTF-8 file was
//!   silently absent from both). [`FileColumns::utf8`] carries exactly that
//!   distinction, so a non-UTF-8 file still gets its `cache.db` row and still
//!   contributes nothing to the index.
//! * Manifest files (`shared_cache::is_manifest_file_name`) are excluded from
//!   all three, and keep being excluded here — `refresh_manifest_entries`
//!   owns them, on its own handful of paths.

use std::collections::HashMap;
use std::path::Path;

use rayon::prelude::*;
use sem_core::index::FileTrigrams;
use sem_mcp::cache as shared_cache;

/// One file's columns, all derived from a single visit to its bytes.
///
/// `⊕` (`SINGLE-PASS.md` §1.1 S3) is disjoint-key union over `path`: the
/// closure that builds a row reads nothing but that file, which is what makes
/// the read coordination-free and the row order irrelevant to correctness
/// (it is still `files` order — rayon's `collect` preserves it — because two
/// consumers, the `files` insert and the `file_imports` refresh, are
/// order-sensitive by SQL statement order and the bit-identical gate covers
/// their output).
pub(crate) struct FileColumns {
    pub path: String,
    pub mtime_secs: i64,
    pub mtime_nanos: i64,
    /// xxh3-64 of the raw bytes — `cache.db`'s hex column *and* the index's
    /// `FileFingerprint.content_hash`, one number (see the module doc).
    pub hash: u64,
    /// The bytes were valid UTF-8, i.e. this file is index-eligible. False
    /// files still carry a `cache.db` row; they carry no fingerprint and no
    /// trigrams, exactly as before the fusion.
    pub utf8: bool,
    /// Empty when `!utf8`.
    pub trigrams: FileTrigrams,
    /// Resolved JS/TS import targets. Empty when `!utf8`, when the file is
    /// not JS/TS, or when nothing resolved — all three were indistinguishable
    /// before the fusion too (`Some(vec![])`).
    pub imports: Vec<String>,
}

impl FileColumns {
    /// `cache.db`'s `files.content_hash` rendering — the injection
    /// `hex₁₆ : u64 ↣ String` (`SINGLE-PASS.md` §1.3).
    pub fn hash_hex(&self) -> String {
        format!("{:016x}", self.hash)
    }
}

/// The corpus's columns: one row per non-manifest file that could be stat'd
/// and read, in `files` order.
pub(crate) struct CorpusColumns {
    pub rows: Vec<FileColumns>,
}

impl CorpusColumns {
    /// The single read. One `std::fs::read` per file, in parallel, folded
    /// into every column the save path needs; the bytes never leave the
    /// closure.
    ///
    /// `all_files` is the candidate set JS/TS import resolution matches
    /// against — the same `all_files` argument
    /// `shared_cache::refresh_file_import_entries` took, with the same
    /// manifest filter, so the resolved targets are identical.
    pub fn read(root: &Path, files: &[String], all_files: &[String]) -> Self {
        // Built once per corpus, not once per file — semx-ccg's O(1)
        // membership fix, carried across unchanged.
        let candidate_files = sem_core::parser::ImportCandidates::new(
            all_files
                .iter()
                .filter(|file| !shared_cache::is_manifest_file_name(file))
                .map(String::as_str),
        );

        let rows: Vec<FileColumns> = files
            .par_iter()
            .filter(|file| !shared_cache::is_manifest_file_name(file))
            .filter_map(|file| {
                let full = root.join(file);
                let (mtime_secs, mtime_nanos) = shared_cache::file_mtime_parts(&full)?;
                let bytes = std::fs::read(&full).ok()?;
                let hash = sem_core::parser::incremental::content_hash_bytes(&bytes);
                let text = std::str::from_utf8(&bytes).ok();
                let trigrams = match text {
                    Some(_) => FileTrigrams::extract(&bytes),
                    None => FileTrigrams::default(),
                };
                let imports = match text {
                    Some(text) => sem_core::parser::js_ts_import_source_files_from_set(
                        file,
                        text,
                        &candidate_files,
                    ),
                    None => Vec::new(),
                };
                Some(FileColumns {
                    path: file.clone(),
                    mtime_secs,
                    mtime_nanos,
                    hash,
                    utf8: text.is_some(),
                    trigrams,
                    imports,
                })
            })
            .collect();

        Self { rows }
    }

    /// The index's `FILES` column: UTF-8 rows only, matching what pass L's
    /// `read_to_string` used to admit.
    pub fn fingerprints(&self) -> Vec<sem_core::index::FileFingerprint> {
        self.rows
            .iter()
            .filter(|row| row.utf8)
            .map(|row| sem_core::index::FileFingerprint {
                path: row.path.clone(),
                mtime_secs: row.mtime_secs,
                mtime_nanos: row.mtime_nanos as u32,
                content_hash: row.hash,
            })
            .collect()
    }

    /// The index's `TRIGRAM` column, keyed the way the writer looks it up.
    /// Consumes the rows' sets — nothing else reads them, and cloning a
    /// corpus of hash sets is exactly the copy this wave exists to delete.
    pub fn into_trigrams(self) -> HashMap<String, FileTrigrams> {
        self.rows
            .into_iter()
            .filter(|row| row.utf8)
            .map(|row| (row.path, row.trigrams))
            .collect()
    }

    /// The `file_imports` column, keyed by importing file.
    pub fn imports_by_file(&self) -> HashMap<&str, Vec<String>> {
        self.rows
            .iter()
            .map(|row| (row.path.as_str(), row.imports.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **L-COLUMNS-FUSE** (`SINGLE-PASS.md` §6, W1-F1 — the fold-fusion
    /// witness for the fused corpus read)
    ///
    /// ```text
    /// ∀ corpus.  CorpusColumns::read(corpus)
    ///          = ⟨ fingerprint_of_each , trigrams_of_each , imports_of_each ⟩
    /// ```
    ///
    /// where each right-hand component is computed by an **independent
    /// re-read** through the very primitives the three deleted passes used:
    /// `shared_cache::file_fingerprint` (pass H, still live — the
    /// incremental save path calls it), `read_to_string` +
    /// `incremental::content_hash` + `FileTrigrams::extract` (pass L), and
    /// `read_to_string` + `js_ts_import_source_files_from_set` (pass I).
    /// The specification side is production code, not a transcription of the
    /// implementation.
    ///
    /// NON-VACUITY: the fixture is asserted to exercise every branch that
    /// distinguishes the three passes — a non-UTF-8 file (in `cache.db`, not
    /// in the index), a manifest file (in none of them), a file too short to
    /// have a trigram, and a JS/TS file with a *resolvable* import.
    /// POSITIVE CONTROL: after mutating one file on disk, the recomputed
    /// columns must differ from the ones captured before — so the equality
    /// above cannot be satisfied by both sides reading nothing.
    #[test]
    fn columns_equal_the_three_passes_they_replace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).expect("mkdir");
        std::fs::write(
            root.join("src/a.ts"),
            "import { b } from './b';\nexport const a = 1;\n",
        )
        .expect("write");
        std::fs::write(root.join("src/b.ts"), "export const b = 2;\n").expect("write");
        std::fs::write(root.join("src/tiny.ts"), "x").expect("write");
        std::fs::write(root.join("src/blob.bin"), [0xff_u8, 0xfe, 0x00, 0x41]).expect("write");
        std::fs::write(root.join(".semrc"), "{}\n").expect("write");
        std::fs::write(root.join("src/plain.py"), "def f():\n    return 1\n").expect("write");

        let files: Vec<String> = vec![
            "src/a.ts".into(),
            "src/b.ts".into(),
            "src/tiny.ts".into(),
            "src/blob.bin".into(),
            ".semrc".into(),
            "src/plain.py".into(),
        ];

        let columns = CorpusColumns::read(root, &files, &files);

        // --- the specification side: three independent re-reads ------------
        let candidates = sem_core::parser::ImportCandidates::new(
            files
                .iter()
                .filter(|f| !shared_cache::is_manifest_file_name(f))
                .map(String::as_str),
        );
        let mut spec_files: Vec<(String, i64, i64, String)> = Vec::new();
        let mut spec_fingerprints: Vec<(String, u64)> = Vec::new();
        let mut spec_trigrams: HashMap<String, FileTrigrams> = HashMap::new();
        let mut spec_imports: HashMap<String, Vec<String>> = HashMap::new();
        for file in &files {
            if shared_cache::is_manifest_file_name(file) {
                continue;
            }
            let full = root.join(file);
            if let Some((secs, nanos, hex)) = shared_cache::file_fingerprint(&full) {
                spec_files.push((file.clone(), secs, nanos, hex));
            }
            if let Ok(text) = std::fs::read_to_string(&full) {
                spec_fingerprints.push((
                    file.clone(),
                    sem_core::parser::incremental::content_hash(&text),
                ));
                spec_trigrams.insert(file.clone(), FileTrigrams::extract(text.as_bytes()));
                spec_imports.insert(
                    file.clone(),
                    sem_core::parser::js_ts_import_source_files_from_set(file, &text, &candidates),
                );
            }
        }

        // --- non-vacuity: the fixture really hits every branch -------------
        assert!(
            columns.rows.iter().any(|r| !r.utf8),
            "non-vacuity: fixture must contain a non-UTF-8 file"
        );
        assert!(
            columns.rows.iter().all(|r| r.path != ".semrc"),
            "non-vacuity: manifest files must be excluded"
        );
        assert!(
            columns
                .rows
                .iter()
                .any(|r| r.path == "src/tiny.ts" && r.trigrams.is_empty()),
            "non-vacuity: fixture must contain a sub-trigram-length file"
        );
        assert_eq!(
            spec_imports.get("src/a.ts").map(Vec::as_slice),
            Some(["src/b.ts".to_string()].as_slice()),
            "non-vacuity: fixture must contain a resolvable JS/TS import"
        );

        // --- the invariant ---------------------------------------------------
        let got_files: Vec<(String, i64, i64, String)> = columns
            .rows
            .iter()
            .map(|r| (r.path.clone(), r.mtime_secs, r.mtime_nanos, r.hash_hex()))
            .collect();
        assert_eq!(
            got_files, spec_files,
            "L-COLUMNS-FUSE: cache.db files column"
        );

        let got_fingerprints: Vec<(String, u64)> = columns
            .fingerprints()
            .into_iter()
            .map(|f| (f.path, f.content_hash))
            .collect();
        assert_eq!(
            got_fingerprints, spec_fingerprints,
            "L-COLUMNS-FUSE: index FILES column"
        );

        let got_imports: HashMap<String, Vec<String>> = columns
            .imports_by_file()
            .into_iter()
            .map(|(path, imports)| (path.to_string(), imports))
            .collect();
        for (path, expected) in &spec_imports {
            assert_eq!(
                got_imports.get(path),
                Some(expected),
                "L-COLUMNS-FUSE: file_imports column for {path}"
            );
        }

        let got_trigrams = columns.into_trigrams();
        let mut spec_paths: Vec<&String> = spec_trigrams.keys().collect();
        let mut got_paths: Vec<&String> = got_trigrams.keys().collect();
        spec_paths.sort();
        got_paths.sort();
        assert_eq!(got_paths, spec_paths, "L-COLUMNS-FUSE: TRIGRAM key set");

        // --- positive control ----------------------------------------------
        std::fs::write(root.join("src/b.ts"), "export const b = 3;\n").expect("write");
        let after = CorpusColumns::read(root, &files, &files);
        let before_hash = got_files
            .iter()
            .find(|(p, ..)| p == "src/b.ts")
            .map(|(_, _, _, h)| h.clone());
        let after_hash = after
            .rows
            .iter()
            .find(|r| r.path == "src/b.ts")
            .map(|r| r.hash_hex());
        assert_ne!(
            before_hash, after_hash,
            "positive control: a changed file must change its column"
        );
    }
}
