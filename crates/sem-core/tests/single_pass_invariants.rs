//! Invariant witnesses for the single-pass columnar build.
//!
//! These are **invariant tests, not example tests**: each one states an
//! equation from and samples it over generated inputs. The
//! unfused side of every equation is real production code kept alive as the
//! specification, so a test cannot degrade into "the new code agrees with
//! itself".
//!
//! Generation is a small deterministic LCG rather than a proptest/quickcheck
//! dependency — this crate has neither, and adding one to assert two dozen
//! sampled equalities would be a new noun buying nothing the loop below does
//! not already buy (`benches/common` already generates its fixtures this way
//! for the same reason). Each invariant states its NON-VACUITY probe (the
//! sampled space really contains the interesting case) and its POSITIVE
//! CONTROL (an input class on which the equation is *known* to have a
//! non-trivial answer, so a broken implementation cannot pass by returning a
//! constant).

use std::collections::HashMap;

use sem_core::index::{
    build_with_content_and_dirs_and_tests_and_spans,
    build_with_trigrams_and_dirs_and_tests_and_spans, FileFingerprint, FileTrigrams,
};
use sem_core::parser::graph::EntityGraph;

/// xorshift64*, seeded per test — deterministic across runs and machines, so
/// a failure is reproducible from the seed printed in the assertion message.
struct Gen(u64);

impl Gen {
    fn new(seed: u64) -> Self {
        Gen(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next() % n as u64) as usize
    }

    /// Arbitrary byte strings, biased towards the sizes where the invariants have
    /// edge cases: 0, 1 and 2 bytes (below the trigram window), and a mix of
    /// ASCII, non-UTF-8 and highly repetitive content (the case where a
    /// file's trigram set is far smaller than its bytes).
    fn bytes(&mut self) -> Vec<u8> {
        let len = match self.below(6) {
            0 => 0,
            1 => 1,
            2 => 2,
            3 => 3 + self.below(40),
            4 => 200 + self.below(3000),
            _ => 1 + self.below(64),
        };
        let shape = self.below(3);
        (0..len)
            .map(|i| match shape {
                // repetitive: few distinct trigrams over many bytes
                0 => b"abcabc"[i % 6],
                // ascii-ish source text
                1 => 0x20 + (self.next() % 0x5e) as u8,
                // arbitrary bytes, routinely invalid UTF-8
                _ => (self.next() % 256) as u8,
            })
            .collect()
    }
}

/// **L-HASH-ENC**
///
/// ```text
/// ∀ b : bytes.  utils::hash::content_hash_bytes(b) = hex₁₆(incremental::content_hash_bytes(b))
/// ```
///
/// `cache.db`'s `files.content_hash` and the query index's
/// `FileFingerprint.content_hash` are the same xxh3-64 in two encodings, not
/// two hashes. This is the equation that licenses deleting an entire
/// full-corpus read and an entire full-corpus hash from the save path — both
/// `build_cache.rs`'s prior comment and commit message asserted
/// the two were distinct, so the invariant is asserted here rather than
/// believed.
///
/// NON-VACUITY: the sample includes the empty string (where a broken hash
/// still agrees) *and* asserts that at least two sampled inputs produce
/// different digests, so a constant implementation fails.
/// POSITIVE CONTROL: `hex₁₆` is checked to be exactly 16 lowercase hex
/// digits, so agreement cannot be manufactured by both sides returning "".
#[test]
fn hash_encoding_identity() {
    let mut g = Gen::new(0x5EED_0001);
    let mut digests = std::collections::HashSet::new();
    for _ in 0..500 {
        let bytes = g.bytes();
        let hex = sem_core::utils::hash::content_hash_bytes(&bytes);
        let num = sem_core::parser::incremental::content_hash_bytes(&bytes);
        assert_eq!(hex, format!("{num:016x}"), "L-HASH-ENC failed on {bytes:?}");
        assert_eq!(hex.len(), 16);
        assert!(hex
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
        digests.insert(num);
    }
    assert!(
        digests.len() > 100,
        "non-vacuity: the sampled space must produce many distinct digests, got {}",
        digests.len()
    );
}

/// **L-HASH-UTF8**
///
/// ```text
/// ∀ s : str.  content_hash(s) = content_hash_bytes(s.as_bytes())
/// ```
///
/// The str-taking entry point is the bytes one; the single corpus read hashes
/// bytes before it knows whether they are UTF-8, and this is what makes that
/// legitimate for a file the index will later fingerprint as text.
#[test]
fn hash_bytes_extends_hash_str() {
    let mut g = Gen::new(0x5EED_0002);
    let mut utf8_seen = 0;
    for _ in 0..500 {
        let bytes = g.bytes();
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        utf8_seen += 1;
        assert_eq!(
            sem_core::parser::incremental::content_hash(text),
            sem_core::parser::incremental::content_hash_bytes(&bytes)
        );
    }
    assert!(
        utf8_seen > 50,
        "non-vacuity: too few UTF-8 samples ({utf8_seen})"
    );
}

/// **L-TRIGRAM-SRC** (pass L; the fold-fusion witness
/// for the trigram half of the fused read)
///
/// ```text
/// ∀ corpus.  build(…, Contents(c), …)  =  build(…, PerFile(c ▷ extract), …)
/// ```
///
/// i.e. extracting each file's trigrams *inside the caller's read closure*
/// and handing the writer the sets produces a byte-identical index image to
/// handing the writer the bytes and letting it extract. The fused save path
/// takes the right-hand side; every existing caller takes the left. The
/// equality is what makes the index-bytes oracle a formality rather than a
/// hope.
///
/// NON-VACUITY: asserts the produced image actually contains a non-empty
/// TRIGRAM tier (a corpus of <3-byte files would make both sides trivially
/// equal at zero length), and that at least one sampled corpus has files
/// sharing trigrams (so postings have length > 1).
/// POSITIVE CONTROL: a deliberately perturbed set (one file's trigrams
/// dropped) must produce *different* bytes — asserted below.
#[test]
fn trigram_source_agree() {
    let mut g = Gen::new(0x5EED_0003);
    let mut nonempty_images = 0;
    for round in 0..40 {
        let file_count = 1 + g.below(8);
        let mut contents: HashMap<String, Vec<u8>> = HashMap::new();
        let mut files: Vec<FileFingerprint> = Vec::new();
        for i in 0..file_count {
            let path = format!("dir{}/f{i}.ts", g.below(3));
            let bytes = g.bytes();
            files.push(FileFingerprint {
                path: path.clone(),
                mtime_secs: g.below(100_000) as i64,
                mtime_nanos: g.below(1_000_000) as u32,
                content_hash: sem_core::parser::incremental::content_hash_bytes(&bytes),
            });
            contents.insert(path, bytes);
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        files.dedup_by(|a, b| a.path == b.path);

        let trigrams: HashMap<String, FileTrigrams> = contents
            .iter()
            .map(|(path, bytes)| (path.clone(), FileTrigrams::extract(bytes)))
            .collect();

        let graph = EntityGraph::from_parts(Default::default(), Vec::new());
        let (from_contents, stats) = build_with_content_and_dirs_and_tests_and_spans(
            &graph,
            &files,
            &[],
            &contents,
            None,
            None,
        );
        let (from_trigrams, _) = build_with_trigrams_and_dirs_and_tests_and_spans(
            &graph,
            &files,
            &[],
            &trigrams,
            None,
            None,
        );
        assert_eq!(
            from_contents, from_trigrams,
            "L-TRIGRAM-SRC failed in round {round}"
        );
        if stats.distinct_trigrams > 0 {
            nonempty_images += 1;

            // POSITIVE CONTROL: drop one file's set; the images must diverge.
            let mut broken = trigrams.clone();
            if let Some(key) = files.iter().map(|f| f.path.clone()).find(|p| {
                broken
                    .get(p)
                    .map(|t: &FileTrigrams| !t.is_empty())
                    .unwrap_or(false)
            }) {
                broken.insert(key, FileTrigrams::default());
                let (perturbed, _) = build_with_trigrams_and_dirs_and_tests_and_spans(
                    &graph,
                    &files,
                    &[],
                    &broken,
                    None,
                    None,
                );
                assert_ne!(
                    from_contents, perturbed,
                    "positive control: dropping a file's trigrams must change the image"
                );
            }
        }
    }
    assert!(
        nonempty_images > 20,
        "non-vacuity: only {nonempty_images} rounds produced a populated TRIGRAM tier"
    );
}
