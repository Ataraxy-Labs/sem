//! Shared generators and law-checkers for the algebraic-extraction law
//! suites (`laws_extraction.rs`, `laws_diff.rs`).
//!
//! Arbitraries are small synthetic programs in TypeScript, Python and Rust,
//! plus JSON documents and plain-text (fallback-plugin) files. Programs are
//! rendered from an abstract `Program` value so that laws can be stated over
//! *edits to the abstract value* (modify one function's literal, permute the
//! functions, touch the preamble) and witnessed over the rendered bytes at
//! the public boundary (`ParserRegistry::extract_entities`,
//! `compute_semantic_diff`).
//!
//! All law checks are `Result`-returning functions so every law can carry a
//! positive control: a deliberately-corrupted artifact must turn the checker
//! RED (see the `control_*` tests in each suite). A law whose checker cannot
//! be made to fail is vacuous and is not claimed.

#![allow(dead_code)]

use proptest::prelude::*;
use sem_core::git::types::{FileChange, FileStatus};
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::differ::{compute_semantic_diff, DiffResult};
use sem_core::parser::plugins::create_default_registry;
use sem_core::parser::registry::ParserRegistry;

// ---------------------------------------------------------------------------
// Languages and rendering
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Ts,
    Py,
    Rs,
    Json,
}

pub const CODE_LANGS: [Lang; 3] = [Lang::Ts, Lang::Py, Lang::Rs];

impl Lang {
    pub fn path(self) -> &'static str {
        match self {
            Lang::Ts => "gen/mod_a.ts",
            Lang::Py => "gen/mod_a.py",
            Lang::Rs => "gen/mod_a.rs",
            Lang::Json => "gen/config.json",
        }
    }

    /// A second, distinct path for two-file (locality) laws.
    pub fn path_b(self) -> &'static str {
        match self {
            Lang::Ts => "gen/mod_b.ts",
            Lang::Py => "gen/mod_b.py",
            Lang::Rs => "gen/mod_b.rs",
            Lang::Json => "gen/other.json",
        }
    }
}

/// An abstract program: function `f{i}` carries literal `rets[i]`.
#[derive(Debug, Clone)]
pub struct Program {
    pub lang: Lang,
    pub rets: Vec<u32>,
    pub preamble: bool,
    pub with_class: bool,
}

pub fn preamble_text(lang: Lang) -> &'static str {
    match lang {
        Lang::Ts => "import { helper } from \"./helper\";\n\n",
        Lang::Py => "import os\n\n",
        Lang::Rs => "use std::fmt::Debug;\n\n",
        Lang::Json => "",
    }
}

pub fn preamble_text_alt(lang: Lang) -> &'static str {
    match lang {
        Lang::Ts => "import { helper2 } from \"./helper2\";\n\n",
        Lang::Py => "import sys\n\n",
        Lang::Rs => "use std::fmt::Display;\n\n",
        Lang::Json => "",
    }
}

pub fn render_fn(lang: Lang, i: usize, ret: u32) -> String {
    match lang {
        Lang::Ts => format!(
            "function f{i}(x: number): number {{\n  const w = {ret};\n  return x + w;\n}}\n\n"
        ),
        Lang::Py => format!("def f{i}(x):\n    w = {ret}\n    return x + w\n\n\n"),
        Lang::Rs => {
            format!("fn f{i}(x: u32) -> u32 {{\n    let w = {ret};\n    x + w\n}}\n\n")
        }
        Lang::Json => unreachable!("render_fn is for code languages"),
    }
}

/// Same function with a comment inserted into the body (formatting-only
/// change relative to `render_fn`, under the structural-hash congruence).
pub fn render_fn_commented(lang: Lang, i: usize, ret: u32) -> String {
    match lang {
        Lang::Ts => format!(
            "function f{i}(x: number): number {{\n  // note\n  const w = {ret};\n  return x + w;\n}}\n\n"
        ),
        Lang::Py => {
            format!("def f{i}(x):\n    # note\n    w = {ret}\n    return x + w\n\n\n")
        }
        Lang::Rs => format!(
            "fn f{i}(x: u32) -> u32 {{\n    // note\n    let w = {ret};\n    x + w\n}}\n\n"
        ),
        Lang::Json => unreachable!(),
    }
}

pub fn render_class(lang: Lang, ret: u32) -> String {
    match lang {
        Lang::Ts => format!(
            "class Box {{\n  value: number = 1;\n  get(): number {{\n    return this.value + {ret};\n  }}\n}}\n"
        ),
        Lang::Py => format!("class Box:\n    def get(self):\n        return {ret}\n"),
        Lang::Rs => format!(
            "struct Box {{\n    v: u32,\n}}\n\nimpl Box {{\n    fn get(&self) -> u32 {{\n        self.v + {ret}\n    }}\n}}\n"
        ),
        Lang::Json => unreachable!(),
    }
}

fn render_json(rets: &[u32]) -> String {
    let mut out = String::from("{\n");
    for (i, ret) in rets.iter().enumerate() {
        let comma = if i + 1 < rets.len() { "," } else { "" };
        out.push_str(&format!("  \"k{i}\": {ret}{comma}\n"));
    }
    out.push_str("}\n");
    out
}

/// Render the program, with functions laid out in `order` (defaults to
/// identity). Each element of `order` is an index into `rets`; the function
/// keeps its name/literal binding, only its position in the file changes.
pub fn render_program_ordered(p: &Program, order: Option<&[usize]>) -> String {
    if p.lang == Lang::Json {
        return render_json(&p.rets);
    }
    let mut out = String::new();
    if p.preamble {
        out.push_str(preamble_text(p.lang));
    }
    let identity: Vec<usize> = (0..p.rets.len()).collect();
    let order = order.unwrap_or(&identity);
    for &i in order {
        out.push_str(&render_fn(p.lang, i, p.rets[i]));
    }
    if p.with_class {
        out.push_str(&render_class(p.lang, 7));
    }
    out
}

pub fn render_program(p: &Program) -> String {
    render_program_ordered(p, None)
}

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

pub fn lang_strategy(include_json: bool) -> BoxedStrategy<Lang> {
    if include_json {
        prop_oneof![
            Just(Lang::Ts),
            Just(Lang::Py),
            Just(Lang::Rs),
            Just(Lang::Json)
        ]
        .boxed()
    } else {
        prop_oneof![Just(Lang::Ts), Just(Lang::Py), Just(Lang::Rs)].boxed()
    }
}

pub fn program_strategy(
    include_json: bool,
    min_fns: usize,
    max_fns: usize,
) -> impl Strategy<Value = Program> {
    (
        lang_strategy(include_json),
        prop::collection::vec(0u32..90, min_fns..=max_fns),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(lang, rets, preamble, with_class)| Program {
            lang,
            rets,
            preamble: preamble && lang != Lang::Json,
            with_class: with_class && lang != Lang::Json,
        })
}

/// Plain-text lines for the fallback (unknown-extension) plugin. Restricted
/// to lowercase words so no shebang/content-detection path can fire.
pub fn plain_lines_strategy(max_lines: usize) -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec("[a-z][a-z0-9 ]{0,30}", 1..=max_lines)
}

// ---------------------------------------------------------------------------
// Boundary helpers
// ---------------------------------------------------------------------------

pub fn registry() -> ParserRegistry {
    create_default_registry()
}

pub fn extract(path: &str, content: &str) -> Vec<SemanticEntity> {
    registry().extract_entities(path, content)
}

pub fn modified_file(path: &str, before: &str, after: &str) -> FileChange {
    FileChange {
        file_path: path.to_string(),
        status: FileStatus::Modified,
        old_file_path: None,
        before_content: Some(before.to_string()),
        after_content: Some(after.to_string()),
    }
}

pub fn renamed_file(old_path: &str, new_path: &str, before: &str, after: &str) -> FileChange {
    FileChange {
        file_path: new_path.to_string(),
        status: FileStatus::Renamed,
        old_file_path: Some(old_path.to_string()),
        before_content: Some(before.to_string()),
        after_content: Some(after.to_string()),
    }
}

pub fn diff(files: &[FileChange]) -> DiffResult {
    let reg = registry();
    compute_semantic_diff(files, &reg, None, None)
}

// ---------------------------------------------------------------------------
// Law checkers (Result-returning so positive controls can turn them RED)
// ---------------------------------------------------------------------------

/// Lens `get` / slice agreement: for every span-bearing entity,
/// `source[start_byte..end_byte] == entity.content`. Returns the number of
/// span-bearing entities so callers can assert a non-vacuity floor.
pub fn check_slice_agreement(src: &str, entities: &[SemanticEntity]) -> Result<usize, String> {
    let mut with_spans = 0usize;
    for e in entities {
        if let (Some(s), Some(t)) = (e.start_byte, e.end_byte) {
            with_spans += 1;
            if s > t || t > src.len() {
                return Err(format!(
                    "entity {} has out-of-bounds span {s}..{t} (len {})",
                    e.id,
                    src.len()
                ));
            }
            let slice = &src[s..t];
            if slice != e.content {
                return Err(format!(
                    "entity {}: source[{s}..{t}] != content\n  slice:   {slice:?}\n  content: {:?}",
                    e.id, e.content
                ));
            }
        }
    }
    Ok(with_spans)
}

/// Splice `replacement` over `src[s..t]`.
pub fn splice(src: &str, s: usize, t: usize, replacement: &str) -> String {
    let mut out = String::with_capacity(src.len() - (t - s) + replacement.len());
    out.push_str(&src[..s]);
    out.push_str(replacement);
    out.push_str(&src[t..]);
    out
}

/// Fingerprint of every entity except those named `exclude_name`, for the
/// frame law: (id, type, name, content, start_line, end_line), sorted.
pub fn frame_fingerprint(
    entities: &[SemanticEntity],
    exclude_name: &str,
) -> Vec<(String, String, String, String, usize, usize)> {
    let mut v: Vec<_> = entities
        .iter()
        .filter(|e| e.name != exclude_name)
        .map(|e| {
            (
                e.id.clone(),
                e.entity_type.clone(),
                e.name.clone(),
                e.content.clone(),
                e.start_line,
                e.end_line,
            )
        })
        .collect();
    v.sort();
    v
}

/// Diff-identity checker: `diff(F, F)` must be observationally empty.
pub fn check_diff_identity(result: &DiffResult) -> Result<(), String> {
    if !result.changes.is_empty() {
        return Err(format!(
            "diff(F, F) produced {} changes: {:?}",
            result.changes.len(),
            result
                .changes
                .iter()
                .map(|c| (c.change_type, c.entity_id.clone()))
                .collect::<Vec<_>>()
        ));
    }
    if result.file_count != 0
        || result.added_count != 0
        || result.modified_count != 0
        || result.deleted_count != 0
        || result.moved_count != 0
        || result.renamed_count != 0
        || result.reordered_count != 0
        || result.orphan_count != 0
    {
        return Err("diff(F, F) has nonzero counters".to_string());
    }
    Ok(())
}

/// Fallback (unknown extension) totality checker: entities must be chunks
/// whose line spans partition `1..=n_lines` in order and whose contents
/// concatenate back to the input lines.
pub fn check_fallback_cover(lines: &[String], entities: &[SemanticEntity]) -> Result<(), String> {
    if lines.is_empty() {
        if entities.is_empty() {
            return Ok(());
        }
        return Err("entities produced for empty input".into());
    }
    if entities.is_empty() {
        return Err("no chunks produced for non-empty input".into());
    }
    let mut expected_start = 1usize;
    let mut rebuilt: Vec<String> = Vec::new();
    for e in entities {
        if e.entity_type != "chunk" {
            return Err(format!(
                "fallback produced non-chunk entity type {:?} — misparse presented as entities",
                e.entity_type
            ));
        }
        if e.start_line != expected_start {
            return Err(format!(
                "chunk starts at line {} but previous chunk ended at {}",
                e.start_line,
                expected_start - 1
            ));
        }
        if e.end_line < e.start_line {
            return Err(format!("chunk has inverted span {}..{}", e.start_line, e.end_line));
        }
        rebuilt.push(e.content.clone());
        expected_start = e.end_line + 1;
    }
    if expected_start != lines.len() + 1 {
        return Err(format!(
            "chunks cover lines 1..={} but input has {} lines",
            expected_start - 1,
            lines.len()
        ));
    }
    let rebuilt = rebuilt.join("\n");
    let original = lines.join("\n");
    if rebuilt != original {
        return Err(format!(
            "chunk contents do not reassemble the input\n  rebuilt: {rebuilt:?}\n  original: {original:?}"
        ));
    }
    Ok(())
}

/// The literal marker of function `i`'s body: `"= {ret}"` appears exactly
/// once in each rendered function (TS `const w = R;`, PY `w = R`, RS
/// `let w = R;`).
pub fn body_marker(ret: u32) -> String {
    format!("= {ret}")
}
