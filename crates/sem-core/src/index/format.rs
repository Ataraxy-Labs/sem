//! The on-disk byte layout.
//!
//! Everything here is little-endian, fixed-width, and read through accessors
//! that take a `&[u8]` — no `repr` tricks, no alignment requirement on the
//! mapping, and no dependency. The file is a wire format that outlives the
//! compiler that wrote it, so its shape is spelled out in bytes rather than
//! inherited from a struct definition.

/// Bumped when any record shape or section meaning below changes. This is the
/// *layout* axis only; extractor semantics ride
/// `facts_store::corpus_identity_salt()` in [`Header::build_salt`].
///
/// `2`: `REFS` targets are no longer bare entity indices — the
/// high `refs::KIND_BITS` bits now carry the edge's `ref_type` (`refs`
/// module below). The record *width* didn't change (still one `u32` per
/// posting), but the *meaning* of those bits did, which is exactly the class
/// of change `format_version` exists to guard: a `1`-tagged image
/// read under this code would have every target's top nibble misread as part
/// of the entity index, corrupting every `refs_of`/`callers_of` answer
/// silently. Bumping the version makes that a clean miss instead (`Header::
/// read` rejects any non-matching version outright, unconditionally, so no
/// migration path is needed — the next build just writes a `2`-tagged image).
///
/// `3`: `EntityRec` grows from 32 to 40 bytes — `start_byte`/
/// `end_byte` appended at offsets 32/36. Unlike `FLAG_ENTITY_TESTS`
/// (`is_test` precedent), there was no reserved padding left
/// to reuse: `EntityRec`'s 32 bytes were exactly full (the `_pad` `u16` that
/// precedent repurposed is `flags` now). A record whose *width* changes is
/// exactly `format_version`'s axis, not a flag's — every offset past
/// `ENTITIES`'s start shifts, so a `2`-tagged image read under `ENTITY_REC_
/// LEN = 40` would misalign every record after the first. Bumping makes that
/// a clean miss (magic/version/salt mismatch ⇒ "index does not exist")
/// instead of silent corruption. Cost, measured on the monster's 454,528-row
/// `ENTITIES` section: 32B → 40B is +8B/entity, +3.64 MB (14.5 MB → 18.2 MB),
/// +11.7% of the ≈31.2 MB base image (table).
pub const FORMAT_VERSION: u32 = 3;

pub const MAGIC: &[u8; 8] = b"SEMIDX01";

/// Set when `EntityRec.id_tail` holds the id with its `"<file_path>::"`
/// prefix removed (the measured-universal case). Clear when some entity
/// violated the prefix rule and every id is stored whole.
pub const FLAG_IDS_ELIDED: u32 = 1 << 0;

/// Set when every `REFS` target's packed high nibble (`refs::kind_of`) holds
/// a trustworthy `ref_type` rather than a zeroed placeholder. Clear only in
/// the fail-safe fallback `refs::MAX_ENTITIES` overflow case (see the `refs`
/// module doc) — a corpus with more entities than the 28-bit target field can
/// address. No corpus measured against this design (largest: the monster's
/// 714,819 entities) is within three orders of magnitude of that cap,
/// so this flag reads `1` on every image built today; it exists so a future
/// reader can tell the degraded case apart from a real "every edge is a
/// `Calls`" corpus rather than silently trusting zeroed kind bits.
pub const FLAG_REFS_TYPED: u32 = 1 << 1;

/// Set when every `EntityRec.flags` word carries a computed
/// [`entity::FLAG_IS_TEST`] bit rather than the zero the field held while it
/// was reserved padding. This is the index's analogue of the
/// SQLite cache's `test_flags_computed` metadata key: the bit's *absence* on
/// an entity is only meaningful if the writer actually classified every
/// entity, and the two build entry points that have the classification on
/// hand (`sem-cli`'s `save_with_test_dirs`/`save_topology`, which already
/// compute it for `entity_flags`) are not the only ones that write an image
/// — `commands::query`'s cold-build path writes one with no `SemanticEntity`
/// bodies in scope and therefore no way to classify. Clear on such an image,
/// which makes every test-shaped reader decline rather than read "no tests"
/// out of an uncomputed field.
///
/// No `FORMAT_VERSION` bump rides with this flag, deliberately and unlike
/// [`FLAG_REFS_TYPED`]: `EntityRec`'s layout is byte-for-byte unchanged (the
/// bits live in the `_pad` `u16` the writer has always written as zero), so
/// a `2`-tagged image built before this flag existed reads correctly under
/// this code — its zeroed field is simply never consulted, because the flag
/// that authorizes consulting it is clear. rule is about *misreads*,
/// and there is none to guard against here.
pub const FLAG_ENTITY_TESTS: u32 = 1 << 2;

pub const HEADER_LEN: usize = 192;
pub const SECTION_COUNT: usize = 8;

pub const SEC_STRINGS: usize = 0;
pub const SEC_FILES: usize = 1;
pub const SEC_ENTITIES: usize = 2;
pub const SEC_NAMES: usize = 3;
/// Reserved for S2 (refs/callers postings, CSR).
pub const SEC_REFS: usize = 4;
/// Reserved for S3 (trigram postings over file indices).
pub const SEC_TRIGRAM: usize = 5;
pub const SEC_DIRS: usize = 6;
pub const SEC_KINDS: usize = 7;

pub const ENTITY_REC_LEN: usize = 40;
pub const FILE_REC_LEN: usize = 40;
pub const DIR_REC_LEN: usize = 24;
pub const KIND_REC_LEN: usize = 8;
pub const NAME_REC_LEN: usize = 4;

/// `u32::MAX` in `EntityRec.parent` and `FileRec` ranges means "none".
pub const NONE_U32: u32 = u32::MAX;

/// Sections start 8-byte aligned so a future zero-copy cast stays available
/// without a format break.
pub const SECTION_ALIGN: usize = 8;

pub fn align_up(n: usize) -> usize {
    (n + SECTION_ALIGN - 1) & !(SECTION_ALIGN - 1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SectionRef {
    pub offset: u64,
    pub len: u64,
}

impl SectionRef {
    pub const EMPTY: SectionRef = SectionRef { offset: 0, len: 0 };

    /// A zero-length section means "tier absent" — readers fall through
    /// rather than fail, which is what lets S2 and S3 land independently.
    pub fn is_absent(&self) -> bool {
        self.len == 0
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Header {
    pub format_version: u32,
    pub flags: u32,
    pub build_salt: u64,
    pub entity_count: u64,
    pub file_count: u64,
    pub dir_count: u64,
    pub kind_count: u32,
    pub sections: [SectionRef; SECTION_COUNT],
}

impl Header {
    pub fn write(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.format_version.to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&self.build_salt.to_le_bytes());
        out.extend_from_slice(&self.entity_count.to_le_bytes());
        out.extend_from_slice(&self.file_count.to_le_bytes());
        out.extend_from_slice(&self.dir_count.to_le_bytes());
        out.extend_from_slice(&self.kind_count.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        for section in &self.sections {
            out.extend_from_slice(&section.offset.to_le_bytes());
            out.extend_from_slice(&section.len.to_le_bytes());
        }
        // Trailing reserve, so a future scalar field can be added without
        // moving the section table off its fixed offset of 56.
        out.extend_from_slice(&[0u8; HEADER_LEN - 184]);
        debug_assert_eq!(out.len() - start, HEADER_LEN);
    }

    /// `None` for every rejection — bad magic, unknown version, truncation,
    /// or a section that runs past the end of the image. A malformed index is
    /// a clean miss, never an error and never a panic, matching
    /// `FactsStore::load`.
    pub fn read(bytes: &[u8], expected_salt: u64) -> Option<Header> {
        if bytes.len() < HEADER_LEN || &bytes[..8] != MAGIC {
            return None;
        }
        let format_version = u32_at(bytes, 8)?;
        if format_version != FORMAT_VERSION {
            return None;
        }
        let build_salt = u64_at(bytes, 16)?;
        if build_salt != expected_salt {
            return None;
        }
        let mut sections = [SectionRef::EMPTY; SECTION_COUNT];
        for (i, section) in sections.iter_mut().enumerate() {
            let base = 56 + i * 16;
            let offset = u64_at(bytes, base)?;
            let len = u64_at(bytes, base + 8)?;
            let end = offset.checked_add(len)?;
            if end > bytes.len() as u64 {
                return None;
            }
            *section = SectionRef { offset, len };
        }
        Some(Header {
            format_version,
            flags: u32_at(bytes, 12)?,
            build_salt,
            entity_count: u64_at(bytes, 24)?,
            file_count: u64_at(bytes, 32)?,
            dir_count: u64_at(bytes, 40)?,
            kind_count: u32_at(bytes, 48)?,
            sections,
        })
    }
}

pub fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

pub fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

pub fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

pub fn i64_at(bytes: &[u8], at: usize) -> Option<i64> {
    Some(i64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

/// `EntityRec`, 40 bytes:
///
/// ```text
///  0  id_tail_off u32    20  start_line u32
///  4  name_off    u32    24  end_line   u32
///  8  id_tail_len u16    28  parent     u32
/// 10  name_len    u16    32  start_byte u32
/// 12  kind        u16    36  end_byte   u32
/// 14  flags       u16
/// 16  file        u32
/// ```
///
/// `flags` was `_pad` until and is still written as zero by every
/// build that cannot classify test entities; bit 0 ([`entity::FLAG_IS_TEST`])
/// is trustworthy only when the header carries [`FLAG_ENTITY_TESTS`].
///
/// `start_byte`/`end_byte` (`FORMAT_VERSION` 3) are `NONE_U32` when
/// the writer had no byte span to record — `SemanticEntity.start_byte`/
/// `end_byte` are `Option<usize>` and several extractors (`fallback.rs`,
/// `json.rs`'s entity path, cache rehydration) never set them. Unlike
/// `FLAG_ENTITY_TESTS`, this is a per-record signal, not a header-wide one:
/// there is no "computed for none vs computed as none" ambiguity the way
/// `is_test`'s boolean had, because `NONE_U32` (`u32::MAX`) is not a byte
/// offset any real file reaches, so it is unambiguously "absent" — the same
/// convention `parent` already uses for "no parent".
pub mod entity {
    use super::*;

    /// This entity is a test, by `parser::graph::is_test_entity`'s
    /// definition — the same predicate that fills the SQLite cache's
    /// `entity_flags.is_test` column, evaluated once at build time by
    /// whoever hands the writer a classification (see [`FLAG_ENTITY_TESTS`]).
    /// The predicate needs the entity's *body*, which `EntityInfo` (and
    /// therefore this image) does not carry, which is exactly why the answer
    /// is precomputed here instead of derived on the read side.
    pub const FLAG_IS_TEST: u16 = 1 << 0;

    // A wire-format record writer is positional by nature; grouping these into
    // a struct would add a type whose only job is to be destructured here.
    #[allow(clippy::too_many_arguments)]
    pub fn write(
        out: &mut Vec<u8>,
        id_tail: (u32, u16),
        name: (u32, u16),
        kind: u16,
        flags: u16,
        file: u32,
        start_line: u32,
        end_line: u32,
        parent: u32,
        start_byte: u32,
        end_byte: u32,
    ) {
        out.extend_from_slice(&id_tail.0.to_le_bytes());
        out.extend_from_slice(&name.0.to_le_bytes());
        out.extend_from_slice(&id_tail.1.to_le_bytes());
        out.extend_from_slice(&name.1.to_le_bytes());
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&file.to_le_bytes());
        out.extend_from_slice(&start_line.to_le_bytes());
        out.extend_from_slice(&end_line.to_le_bytes());
        out.extend_from_slice(&parent.to_le_bytes());
        out.extend_from_slice(&start_byte.to_le_bytes());
        out.extend_from_slice(&end_byte.to_le_bytes());
    }

    pub fn id_tail(rec: &[u8]) -> Option<(usize, usize)> {
        Some((u32_at(rec, 0)? as usize, u16_at(rec, 8)? as usize))
    }
    pub fn name(rec: &[u8]) -> Option<(usize, usize)> {
        Some((u32_at(rec, 4)? as usize, u16_at(rec, 10)? as usize))
    }
    pub fn kind(rec: &[u8]) -> Option<u16> {
        u16_at(rec, 12)
    }
    pub fn flags(rec: &[u8]) -> Option<u16> {
        u16_at(rec, 14)
    }
    pub fn file(rec: &[u8]) -> Option<u32> {
        u32_at(rec, 16)
    }
    pub fn start_line(rec: &[u8]) -> Option<u32> {
        u32_at(rec, 20)
    }
    pub fn end_line(rec: &[u8]) -> Option<u32> {
        u32_at(rec, 24)
    }
    pub fn parent(rec: &[u8]) -> Option<u32> {
        u32_at(rec, 28)
    }
    /// `NONE_U32` when the writer had no byte span for this entity — see the
    /// module doc. Callers that need "absent" as `Option` should compare
    /// against [`NONE_U32`] the same way [`parent`]'s callers do.
    pub fn start_byte(rec: &[u8]) -> Option<u32> {
        u32_at(rec, 32)
    }
    pub fn end_byte(rec: &[u8]) -> Option<u32> {
        u32_at(rec, 36)
    }
}

/// `FileRec`, 40 bytes:
///
/// ```text
///  0  path_off    u32    24  content_hash u64
///  4  path_len    u32    32  entity_lo    u32
///  8  mtime_secs  i64    36  entity_hi    u32
/// 16  mtime_nanos i32
/// 20  _pad        u32
/// ```
///
/// `entity_lo..entity_hi` is the half-open `ENTITIES` range belonging to this
/// file, which is what makes "entities of this file" a slice rather than a
/// search and what makes the patch protocol a range replacement.
pub mod file {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    pub fn write(
        out: &mut Vec<u8>,
        path: (u32, u32),
        mtime_secs: i64,
        mtime_nanos: i32,
        content_hash: u64,
        entity_lo: u32,
        entity_hi: u32,
    ) {
        out.extend_from_slice(&path.0.to_le_bytes());
        out.extend_from_slice(&path.1.to_le_bytes());
        out.extend_from_slice(&mtime_secs.to_le_bytes());
        out.extend_from_slice(&mtime_nanos.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&content_hash.to_le_bytes());
        out.extend_from_slice(&entity_lo.to_le_bytes());
        out.extend_from_slice(&entity_hi.to_le_bytes());
    }

    pub fn path(rec: &[u8]) -> Option<(usize, usize)> {
        Some((u32_at(rec, 0)? as usize, u32_at(rec, 4)? as usize))
    }
    pub fn mtime_secs(rec: &[u8]) -> Option<i64> {
        i64_at(rec, 8)
    }
    pub fn mtime_nanos(rec: &[u8]) -> Option<u32> {
        u32_at(rec, 16)
    }
    pub fn content_hash(rec: &[u8]) -> Option<u64> {
        u64_at(rec, 24)
    }
    pub fn entity_lo(rec: &[u8]) -> Option<u32> {
        u32_at(rec, 32)
    }
    pub fn entity_hi(rec: &[u8]) -> Option<u32> {
        u32_at(rec, 36)
    }
}

/// `DirRec`, 24 bytes (wired into the `Complete` freshness
/// tier): every distinct ancestor directory of every indexed file, all
/// levels (not just direct parents) plus the repo root (`path` = `""`). The
/// ancestor-chain requirement is what makes directory-mtime drift a sound
/// membership signal at whatever depth a file was actually added — POSIX
/// bumps a directory's mtime only on its own direct-entry create/unlink/
/// rename, so a new leaf 3 levels deep is invisible to a DIRS table that only
/// stored leaf parents.
///
/// ```text
///  0  path_off    u32     8  mtime_secs  i64
///  4  path_len    u32    16  mtime_nanos i32
///                        20  _pad        u32
/// ```
pub mod dir {
    use super::*;

    pub fn write(out: &mut Vec<u8>, path: (u32, u32), mtime_secs: i64, mtime_nanos: i32) {
        out.extend_from_slice(&path.0.to_le_bytes());
        out.extend_from_slice(&path.1.to_le_bytes());
        out.extend_from_slice(&mtime_secs.to_le_bytes());
        out.extend_from_slice(&mtime_nanos.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
    }

    pub fn path(rec: &[u8]) -> Option<(usize, usize)> {
        Some((u32_at(rec, 0)? as usize, u32_at(rec, 4)? as usize))
    }
    pub fn mtime_secs(rec: &[u8]) -> Option<i64> {
        i64_at(rec, 8)
    }
    pub fn mtime_nanos(rec: &[u8]) -> Option<u32> {
        u32_at(rec, 16)
    }
}

/// `TRIGRAM` section (S3): postings over **file** indices, not
/// entity indices — "the text tier can answer without the
/// entity tier being loaded" contract.
///
/// ```text
///  0  trigram_count u32
///  4  posting_total u32
///  8  KEYS     trigram_count x u32   -- packed trigram | STOP_FLAG, sorted by
///                                       the low 24 bits (the trigram value)
///     OFFSETS  (trigram_count+1) x u32  -- row offsets into TARGETS (CSR)
///     TARGETS  posting_total x u32   -- file indices, ascending, deduped
/// ```
///
/// A trigram absent from `KEYS` occurs in **zero** files — a strong, correct
/// "no candidates" signal. A trigram present but flagged `STOP` occurs in so
/// many files that storing its full posting list would blow the section's
/// budget; its row is empty and the reader must treat it as "no
/// filtering information", never as "zero files" — the two are not the same
/// claim and conflating them would turn a budget decision into a correctness
/// bug. See `writer::build_trigram_section` for how the flag gets set and
/// trigram section for the measured stop-list.
pub mod trigram {
    use super::*;

    /// Set on a `KEYS` entry's high bit. The trigram value itself only needs
    /// the low 24 bits (3 bytes), so this costs nothing extra to store.
    pub const STOP_FLAG: u32 = 1 << 31;
    pub const VALUE_MASK: u32 = 0x00FF_FFFF;

    pub fn pack(bytes: [u8; 3]) -> u32 {
        (bytes[0] as u32) << 16 | (bytes[1] as u32) << 8 | bytes[2] as u32
    }

    pub fn unpack(value: u32) -> [u8; 3] {
        let v = value & VALUE_MASK;
        [(v >> 16) as u8, (v >> 8) as u8, v as u8]
    }

    pub fn value_of(entry: u32) -> u32 {
        entry & VALUE_MASK
    }

    pub fn is_stop(entry: u32) -> bool {
        entry & STOP_FLAG != 0
    }

    /// Byte offset of `KEYS[0]` within the section — right after the 8-byte
    /// sub-header, same convention `REFS_SUB_HEADER_LEN` uses.
    pub const SUB_HEADER_LEN: usize = 8;

    pub fn header(section: &[u8]) -> Option<(usize, usize)> {
        Some((u32_at(section, 0)? as usize, u32_at(section, 4)? as usize))
    }

    pub fn offsets_at(trigram_count: usize) -> usize {
        SUB_HEADER_LEN + trigram_count * 4
    }

    /// Byte offset (relative to the section start) of `TARGETS[0]` — public
    /// so a caller with a section offset on hand (e.g. `index_probe`'s
    /// mutation test) can locate a specific posting to corrupt without
    /// duplicating this arithmetic.
    pub fn targets_at(trigram_count: usize) -> usize {
        offsets_at(trigram_count) + (trigram_count + 1) * 4
    }

    /// Expected total section length for a well-formed image — used by the
    /// reader's open-time soundness check, same discipline as
    /// `QueryIndex::refs_section_is_sound`.
    pub fn expected_len(trigram_count: usize, posting_total: usize) -> usize {
        targets_at(trigram_count) + posting_total * 4
    }

    pub fn key(section: &[u8], slot: usize) -> Option<u32> {
        u32_at(section, SUB_HEADER_LEN + slot * 4)
    }

    /// `TARGETS[lo..hi]` is this trigram's posting row (CSR row-offsets, not
    /// byte offsets — one more indirection through `target` below).
    pub fn row(section: &[u8], trigram_count: usize, slot: usize) -> Option<(usize, usize)> {
        let base = offsets_at(trigram_count);
        let lo = u32_at(section, base + slot * 4)? as usize;
        let hi = u32_at(section, base + (slot + 1) * 4)? as usize;
        Some((lo, hi))
    }

    pub fn target(section: &[u8], trigram_count: usize, at: usize) -> Option<u32> {
        u32_at(section, targets_at(trigram_count) + at * 4)
    }
}

/// `REFS` postings pack each edge's `ref_type` into the same `u32` that
/// already carried the bare target entity index (a packed-field
/// extension), rather than adding a parallel `u8` array.
///
/// Measured trade, on the two encodings the change was asked to weigh:
///
/// - **parallel `u8` array** (one extra byte per posting, both directions):
///   on the monster (454,528-entity image, ~196k edges per §S2's own
///   comment in `graph.rs`) that is ≈ +400 KB across fwd+rev, plus a second
///   cache line touched per edge at read time (`targets[i]` and `kinds[i]`
///   live in different arrays) — real, but not the deciding factor.
/// - **bits packed into the existing target `u32`** (this design): zero
///   extra bytes, zero extra reads — `kind_of`/`target_of` are a mask and a
///   shift on the word `csr_row` already loads. The cost is capping the
///   addressable target-index space to `TARGET_BITS` (28) bits instead of the
///   full 32, i.e. `MAX_ENTITIES` (268,435,456) entities per repo.
///
/// The deciding factor: `RefType` (`parser::graph::RefType`) has exactly 3
/// variants today and this field reserves 4 bits (16 values), 5x headroom for
/// future kinds with no format change. Meanwhile the entity-count cap this
/// costs is 375x the largest corpus this design has ever measured (the
/// monster's 714,819 entities, "confirmed by the skeleton" scope) —
/// nowhere near a real constraint. Zero-copy, zero-extra-bytes, and a cap
/// four orders of magnitude above any measured corpus dominates a strictly
/// larger, strictly slower alternative that buys nothing back for it: this is
/// the `semantic-hard-cut-refactor` dominance test (`c1 ⪰ c2` — packing has
/// every guarantee the parallel array has, `Γ`, at strictly lower cost, `σ`)
/// applied to a format choice, not just a code one.
pub mod refs {
    /// Bits of a packed `REFS` target reserved for `ref_type`. `Calls` = 0,
    /// `TypeRef` = 1, `Imports` = 2 (`parser::graph::RefType`'s declaration
    /// order — arbitrary but stable, since nothing outside this format needs
    /// to be equal to it and it is documented here as the one place that
    /// matters). Values 3-15 are reserved for future `RefType` variants.
    pub const KIND_BITS: u32 = 4;
    pub const TARGET_BITS: u32 = 32 - KIND_BITS;
    pub const TARGET_MASK: u32 = (1u32 << TARGET_BITS) - 1;
    /// The largest `entity_count` a typed `REFS` image can address. Above
    /// this, the writer falls back: clear [`super::FLAG_REFS_TYPED`] for the
    /// whole image and pack every target with kind `0`, the same fail-safe
    /// (never a corrupted index, only a less-precise one) `id_tail_of`'s
    /// prefix-rule fallback already establishes.
    pub const MAX_ENTITIES: usize = 1 << TARGET_BITS;
    pub const MAX_KIND: u8 = (1 << KIND_BITS) - 1;

    /// Pack a target entity index and its edge kind into one CSR posting.
    /// `target` must be `<= TARGET_MASK` (the writer never calls this
    /// otherwise — every caller checks `entities.len() <= MAX_ENTITIES`
    /// first) and `kind <= MAX_KIND` (guaranteed: only 3 of 16 values are
    /// ever produced today).
    pub fn pack(target: u32, kind: u8) -> u32 {
        debug_assert!(
            target <= TARGET_MASK,
            "REFS target index exceeds the packed field width"
        );
        debug_assert!(kind <= MAX_KIND, "REFS ref_type exceeds 4 bits");
        (target & TARGET_MASK) | ((kind as u32) << TARGET_BITS)
    }

    /// The entity index half of a packed posting — what every prior
    /// reader needs, and still all `refs_of`/`callers_of` (the untyped
    /// accessors) return.
    pub fn target_of(packed: u32) -> u32 {
        packed & TARGET_MASK
    }

    /// The `ref_type` half, as its raw 4-bit encoding (0/1/2 today). Callers
    /// map this back to `parser::graph::RefType` — kept out of this module so
    /// `format.rs` stays dependency-free: this file only knows about
    /// bytes and bits, never about `parser::graph`.
    pub fn kind_of(packed: u32) -> u8 {
        (packed >> TARGET_BITS) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(salt: u64) -> Header {
        Header {
            format_version: FORMAT_VERSION,
            flags: FLAG_IDS_ELIDED,
            build_salt: salt,
            entity_count: 7,
            file_count: 2,
            dir_count: 1,
            kind_count: 3,
            sections: [SectionRef {
                offset: 192,
                len: 8,
            }; SECTION_COUNT],
        }
    }

    fn image(salt: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        header(salt).write(&mut bytes);
        bytes.extend_from_slice(&[0u8; 8]);
        bytes
    }

    #[test]
    fn header_round_trips() {
        let bytes = image(0xdead_beef);
        let read = Header::read(&bytes, 0xdead_beef).expect("valid header");
        assert_eq!(read.entity_count, 7);
        assert_eq!(read.file_count, 2);
        assert_eq!(read.dir_count, 1);
        assert_eq!(read.kind_count, 3);
        assert_eq!(read.flags, FLAG_IDS_ELIDED);
        assert_eq!(
            read.sections[SEC_ENTITIES],
            SectionRef {
                offset: 192,
                len: 8
            }
        );
    }

    #[test]
    fn header_is_exactly_header_len() {
        let mut bytes = Vec::new();
        header(1).write(&mut bytes);
        assert_eq!(bytes.len(), HEADER_LEN);
    }

    #[test]
    fn salt_mismatch_is_a_clean_miss() {
        let bytes = image(1);
        assert!(Header::read(&bytes, 2).is_none());
    }

    #[test]
    fn bad_magic_is_a_clean_miss() {
        let mut bytes = image(1);
        bytes[0] = b'X';
        assert!(Header::read(&bytes, 1).is_none());
    }

    #[test]
    fn unknown_format_version_is_a_clean_miss() {
        let mut bytes = image(1);
        bytes[8..12].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        assert!(Header::read(&bytes, 1).is_none());
    }

    #[test]
    fn truncated_file_is_a_clean_miss_not_a_panic() {
        let bytes = image(1);
        for cut in 0..bytes.len() {
            assert!(Header::read(&bytes[..cut], 1).is_none(), "cut at {cut}");
        }
    }

    #[test]
    fn garbage_bytes_are_a_clean_miss() {
        let garbage: Vec<u8> = (0..512u32).map(|i| (i * 37) as u8).collect();
        assert!(Header::read(&garbage, 1).is_none());
    }

    #[test]
    fn section_running_past_end_is_a_clean_miss() {
        let mut bytes = image(1);
        bytes[56 + 8..56 + 16].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(Header::read(&bytes, 1).is_none());
    }

    #[test]
    fn entity_rec_round_trips() {
        let mut rec = Vec::new();
        entity::write(
            &mut rec,
            (10, 4),
            (99, 7),
            3,
            entity::FLAG_IS_TEST,
            5,
            100,
            200,
            NONE_U32,
            1000,
            2000,
        );
        assert_eq!(rec.len(), ENTITY_REC_LEN);
        assert_eq!(entity::id_tail(&rec), Some((10, 4)));
        assert_eq!(entity::name(&rec), Some((99, 7)));
        assert_eq!(entity::kind(&rec), Some(3));
        assert_eq!(entity::flags(&rec), Some(entity::FLAG_IS_TEST));
        assert_eq!(entity::file(&rec), Some(5));
        assert_eq!(entity::start_line(&rec), Some(100));
        assert_eq!(entity::end_line(&rec), Some(200));
        assert_eq!(entity::parent(&rec), Some(NONE_U32));
        assert_eq!(entity::start_byte(&rec), Some(1000));
        assert_eq!(entity::end_byte(&rec), Some(2000));
    }

    #[test]
    fn entity_rec_start_byte_end_byte_absent_is_none_u32() {
        let mut rec = Vec::new();
        entity::write(
            &mut rec, (0, 0), (0, 0), 0, 0, 0, 0, 0, NONE_U32, NONE_U32, NONE_U32,
        );
        assert_eq!(entity::start_byte(&rec), Some(NONE_U32));
        assert_eq!(entity::end_byte(&rec), Some(NONE_U32));
    }

    #[test]
    fn file_rec_round_trips() {
        let mut rec = Vec::new();
        file::write(&mut rec, (1, 2), -7, 42, 0xabcd_ef01_2345_6789, 3, 9);
        assert_eq!(rec.len(), FILE_REC_LEN);
        assert_eq!(file::path(&rec), Some((1, 2)));
        assert_eq!(file::mtime_secs(&rec), Some(-7));
        assert_eq!(file::mtime_nanos(&rec), Some(42));
        assert_eq!(file::content_hash(&rec), Some(0xabcd_ef01_2345_6789));
        assert_eq!(file::entity_lo(&rec), Some(3));
        assert_eq!(file::entity_hi(&rec), Some(9));
    }

    #[test]
    fn reserved_sections_read_as_absent() {
        assert!(SectionRef::EMPTY.is_absent());
    }

    #[test]
    fn dir_rec_round_trips() {
        let mut rec = Vec::new();
        dir::write(&mut rec, (5, 3), -11, 77);
        assert_eq!(rec.len(), DIR_REC_LEN);
        assert_eq!(dir::path(&rec), Some((5, 3)));
        assert_eq!(dir::mtime_secs(&rec), Some(-11));
        assert_eq!(dir::mtime_nanos(&rec), Some(77));
    }
}
