mod entity_extractor;
pub mod languages;
#[cfg(feature = "oxc-fastpath")]
pub mod oxc_extractor;

use std::cell::RefCell;
use std::collections::HashMap;

use crate::model::entity::SemanticEntity;
use crate::parser::cache;
use crate::parser::fast_extractor;
use crate::parser::plugin::SemanticParserPlugin;
use crate::utils::hash::{content_hash, structural_hash};
use entity_extractor::extract_entities;
use languages::{get_all_code_extensions, get_language_config};

/// Walk an already-parsed tree and build entities, without re-parsing.
///
/// Exposed so callers that already hold a `Tree` (from
/// [`SemanticParserPlugin::extract_entities_with_tree`] or [`parse_tree`]) can
/// attribute parse cost separately from walk cost. This is the second half of
/// `extract_entities`.
pub use entity_extractor::extract_entities as extract_entities_from_tree;

pub struct CodeParserPlugin;

// Thread-local parser cache: one Parser per language per thread.
// Avoids creating a new Parser for every file during parallel graph builds.
thread_local! {
    static PARSER_CACHE: RefCell<HashMap<&'static str, tree_sitter::Parser>> = RefCell::new(HashMap::new());
}

/// Resolve the tree-sitter language config for a file, by extension first and
/// then by shebang. `None` means "not a code file this build can parse".
pub fn language_config_for_content(
    content: &str,
    file_path: &str,
) -> Option<&'static languages::LanguageConfig> {
    let ext = std::path::Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_lowercase()))
        .unwrap_or_default();

    get_language_config(&ext).or_else(|| {
        detect_ext_from_content(content).and_then(|shebang_ext| get_language_config(&shebang_ext))
    })
}

/// Parse `content` with the thread-local parser for `config`, from scratch.
pub fn parse_tree(
    config: &'static languages::LanguageConfig,
    content: &str,
) -> Option<tree_sitter::Tree> {
    parse_tree_incremental(config, content, None)
}

/// Hard wall-clock ceiling for a single-file parse. Healthy files parse in
/// microseconds to low milliseconds, so this budget is far above the normal
/// case and never fires for healthy input. It exists for the pathological
/// case: tree-sitter's GLR error recovery (`ts_parser__handle_error` ->
/// `ts_parser__do_all_potential_reductions`, and `ts_parser__recover` ->
/// `ts_stack_pop_count`) goes super-linear on large inputs that end up in an
/// error-recovery parse (deliberately malformed compiler-fixture files; or,
/// per, a pathologically deep/adversarial data fixture that a
/// grammar never designed for such depth also drives into error recovery --
/// see `crates/sem-core/, "C# pathology
/// (dotnet-runtime)"). Shared with `scope_resolve.rs`'s pass-2 reparse loop,
/// which established this mechanism first for exactly the
/// TypeScript-fixture shape of this same failure mode; this is the pass-1
/// (initial parse) sibling.
///
/// raised from the original 2s to 10s after this budget
/// started spawning a supervisor thread (see `parse_tree_within_budget`'s doc
/// comment) whose wall-clock timing is scheduler-sensitive under load, not
/// just tree-sitter's own progress-callback cancellation. dotnet-runtime
/// ships 6 files (`hugeexpr1.cs`, `HugeField1/2.cs`, `HugeArray1.cs`,
/// `TestData.g.cs`, and siblings under `src/tests/JIT/jit64/opt/cse/`) that
/// are legitimately slow to parse -- 1.5-2.8s in isolation, genuinely
/// error-recovery-bound generated JIT torture-test fixtures, not a bug to
/// route around -- clustered close enough to the old 2s budget that
/// scheduler jitter under 18-way parallelism made whether any given one
/// finished in time non-deterministic: two in-process builds of the same
/// corpus disagreed on `hugeexpr1.cs`'s fate and produced different edge
/// counts (`incr_probe`'s own `cold-vs-build` and `warm-vs-cold` oracles both
/// caught this). The next corpus file above that cluster is >30x slower
/// (`EncryptedXmlSample4.xml`, ~90s, see `LARGE_FILE_BUDGET_THRESHOLD`'s
/// sibling section) with nothing in between, so 10s clears every known
/// legitimate file with a >=3.5x margin while still bounding the genuinely
/// pathological one to a small fraction of its unbounded cost.
///
/// no longer read from either hot path (pass 1's
/// `extract_entities_with_tree`, pass 2's `scope_resolve.rs` reparse loop) --
/// both now use [`is_pathological_large_file`], a deterministic content-shape
/// predicate, as their sole give-up decision. A hybrid was tried first
/// (predicate ahead of this budget, budget kept as fallback for whatever the
/// predicate didn't flag) and *measured* to still reproduce 's
/// chunk-boundary edge-count nondeterminism on dotnet-runtime -- proof some
/// file other than the one confirmed pathological one was still racing this
/// clock. Left defined, undeleted: still correct machinery, still worth
/// knowing about if a future call site genuinely needs a bounded-wall-clock
/// parse (not a give-up-or-not classification decided ahead of time).
pub const PARSE_TIME_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// Files at or below this size skip the budget supervisor entirely and go
/// through the plain, unconditional [`parse_tree`] -- exactly the code path
/// and behavior pass 1 has always used. See the call site in
/// `CodeParserPlugin::extract_entities_with_tree` for why this gate exists
/// (thread-spawn scale safety, not a correctness requirement of the budget
/// mechanism itself). 128 KiB is comfortably above every healthy source file
/// this document's corpora contain and comfortably below the multi-megabyte
/// generated/fixture files that have actually been observed pathological.
///
/// no longer read from either hot path -- see [`PARSE_TIME_BUDGET`]'s
/// doc comment. Left defined, undeleted: the thread-oversubscription finding
/// this constant's doc comment records is still true and still worth
/// knowing if a future call site wants a bounded-wall-clock parse, and
/// [`parse_tree_within_budget`] itself is kept working (not gutted) for
/// exactly that case.
pub const LARGE_FILE_BUDGET_THRESHOLD: u64 = 128 * 1024;

/// Parse `content` for `config` with a hard wall-clock ceiling of `budget`.
///
/// no longer called from pass 1 (`extract_entities_with_tree`) or
/// pass 2 (`scope_resolve.rs`'s reparse loop) -- both use
/// [`is_pathological_large_file`], a deterministic content-shape predicate,
/// as their sole give-up decision instead. See that function's doc comment
/// for why a hybrid (predicate ahead of this function, this function kept as
/// fallback) was tried and rejected: measured directly against
/// dotnet-runtime at two chunk sizes, the hybrid still reproduced 's
/// chunk-boundary edge-count nondeterminism, so any file still able to reach
/// this wall-clock race keeps the underlying bug alive regardless of what
/// the predicate catches. Kept working, not deleted: a future caller that
/// genuinely needs a bounded-wall-clock parse (as opposed to a
/// give-up-or-not classification decided ahead of time, immune to
/// scheduling) still has this available.
///
/// Returns `None` if the language is unset (same contract as [`parse_tree`])
/// or if the parse blew through `budget`, in which case the caller must treat
/// the file as unparseable -- the exact same `(Vec::new(), None)` shape
/// [`CodeParserPlugin::extract_entities_with_tree`] already returns for any
/// other unparseable file, so callers need no new handling for this case.
///
/// this runs the parse on a supervisor thread and races it against
/// `budget` with `recv_timeout`, rather than tree-sitter's own
/// `parse_with_options` + `progress_callback` cancellation mechanism (what an
/// earlier revision of this function used, mirroring the pass-2 reparse loop
/// this budget was first built for in). That callback-based read API
/// turned out to have a correctness bug independent of timing: on at least
/// one small, fast-to-parse but adversarially-crafted file in the
/// dotnet-runtime corpus (`EncryptedXmlSample5.xml`, 5,586 bytes -- an XML
/// decryption-transform-chain fixture, not a large or slow one), it returned
/// a *completed* tree containing a node with `end_byte() = 5630`, past the
/// end of the 5,586-byte input -- verified by isolating the same content
/// through the plain `Parser::parse` (this function's read path) instead,
/// which parses it correctly (`root_kind=document`, no error, 29 entities,
/// matching every other well-formed file's contract) in under a millisecond.
/// Nothing here changed what the *progress_callback* budget mechanism itself
/// does for pass 2's reparse loop (untouched); this function no longer uses
/// it, at all, so pass 1 (every file, not just the >20k-file chunked-repo
/// reparse subset) cannot hit that bug either.
///
/// A supervisor thread per call means every pass-1 file pays one thread
/// spawn+join even on the overwhelming majority of files that never approach
/// `budget` -- measured net win regardless (see the C# pathology section above): thread spawn/join is microseconds, the
/// pathology it bounds was tens of seconds on a single file.
pub fn parse_tree_within_budget(
    config: &'static languages::LanguageConfig,
    content: &str,
    budget: std::time::Duration,
) -> Option<tree_sitter::Tree> {
    let language = (config.get_language)()?;
    let content_owned = content.to_string();
    let (tx, rx) = std::sync::mpsc::channel::<Option<tree_sitter::Tree>>();

    // `Parser` and `Tree` are both `Send` (tree-sitter's own unsafe impls).
    // The spawned thread is intentionally allowed to outlive this call on
    // the timeout path below -- there is no `join` to wait on, so a
    // pathological file's still-running parse is abandoned, not aborted;
    // it consumes one background thread's CPU until tree-sitter's own parse
    // completes, same as it always would have, just off the critical path.
    let spawned = std::thread::Builder::new().spawn(move || {
        let mut parser = tree_sitter::Parser::new();
        let _ = parser.set_language(&language);
        let tree = parser.parse(content_owned.as_bytes(), None);
        let _ = tx.send(tree);
    });
    if spawned.is_err() {
        // Thread-spawn failure (resource exhaustion): fall back to a direct,
        // unbounded parse on the calling thread rather than silently losing
        // the file's entities -- same behavior as before this budget existed.
        return parse_tree(config, content);
    }

    rx.recv_timeout(budget).unwrap_or_default()
}

/// The shape threshold [`is_pathological_large_file`] classifies on: the
/// longest single `\n`-delimited run in a file's content, in bytes.
///
/// ("Memory attribution" section):
/// measured directly with `examples/parse_time_probe.rs` (sequential,
/// zero-contention, single-file-at-a-time -- the wall-clock time a file
/// actually costs to parse, isolated from anything [`PARSE_TIME_BUDGET`]'s
/// concurrent-supervisor-thread race adds on top) across dotnet-runtime,
/// linux, llvm-project, elasticsearch, TypeScript-monster, and tiptap's
/// whole >128KiB-file populations:
///
/// - **The one confirmed pathological file**: dotnet-runtime's
///   `EncryptedXmlSample4.xml`, 9.5MB total, one embedded `<CipherValue>`
///   payload forming a single 8,441,855-byte line (not deep tag-nesting --
///   only ~1,245 open tags total) -- **92.265s** solo parse, 9.2x over
///   `PARSE_TIME_BUDGET`.
/// - **Every other large-line file measured parses fast, regardless of
///   line length** -- and line length alone does *not* cleanly separate
///   these from the pathological file the way an earlier revision of this
///   fix assumed: TypeScript-monster ships
///   `.../should-be-able-to-return-the-file-size-when-a-JS-file-is-too-large-to-load-into-text.js`,
///   a deliberate torture fixture whose single line is 4,194,306 bytes --
///   only 2x below the pathological XML's line length -- yet it parses in
///   **52 milliseconds**. Several other TypeScript-monster fixtures are
///   ~100% one line (`codeFixClassImplementInterfaceNoTruncationProperties.ts`,
///   `excessivelyLargeArrayLiteralCompletions.ts`, both >99.9% single-line)
///   and still parse in single-digit milliseconds. Total file size doesn't
///   separate the populations either -- dotnet-runtime's legitimately-slow
///   `hugeexpr1.cs` cluster is up to 24MB, *larger* than the 9.5MB
///   pathological file. **The pathology is grammar-specific (XML's
///   scanner on this input), not a generic function of content shape** --
///   this predicate is a coarse, conservative proxy for it, not a proof.
///
/// [`PATHOLOGICAL_LINE_THRESHOLD`] is set at the geometric mean of the
/// widest legitimate line measured (TypeScript-monster's 4,194,306 bytes)
/// and the one confirmed pathological line (8,441,855 bytes): **6 MiB**,
/// giving both populations a ~1.4x margin -- thinner than an ideal
/// discriminator would have, disclosed rather than overstated. Because the
/// margin is thin and the underlying pathology is grammar-dependent rather
/// than structurally proven, this predicate is deliberately *not* the sole
/// line of defense: see [`is_pathological_large_file`]'s doc comment for
/// why [`parse_tree_within_budget`]'s wall-clock ceiling stays in place as
/// a fallback for whatever this predicate doesn't catch.
pub const PATHOLOGICAL_LINE_THRESHOLD: u64 = 6 * 1024 * 1024;

/// Longest single `\n`-delimited run in `content`, in bytes. O(n), one pass
/// over the bytes already in hand, no allocation -- see
/// [`is_pathological_large_file`] for why this is cheap enough to run
/// unconditionally rather than gating it behind a coarser pre-check.
fn max_line_len(content: &str) -> usize {
    content
        .as_bytes()
        .split(|&b| b == b'\n')
        .map(<[u8]>::len)
        .max()
        .unwrap_or(0)
}

/// Deterministic, pure-per-file pre-filter that removes the one
/// *confirmed* source of [`parse_tree_within_budget`]'s wall-clock race from
/// pass-1's and pass-2's large-file give-up decision, before that race ever
/// starts.
///
/// ## The bug this targets
///
/// The give-up decision used to be entirely the outcome of racing a
/// spawned-thread parse against [`PARSE_TIME_BUDGET`]'s 10s wall clock.
/// Which files finished inside that window depended on how many *other*
/// parses -- including other budget-racing supervisor threads -- were in
/// flight on other threads at the same moment, itself a function of
/// `SCOPE_RESOLVE_FILE_CHUNK_SIZE` (a different chunk size batches `rayon`
/// work differently, changing how many large files land in the same chunk
/// together, changing contention). This was not hypothetical: shrinking
/// dotnet-runtime's chunk size from 5,000 to 1,000 files moved
/// the corpus's resolved edge count by exactly +1, reproducibly, with no
/// other change to the input.
///
/// ## The fix, and its honest scope
///
/// `EncryptedXmlSample4.xml` is dotnet-runtime's single heaviest
/// contributor to that contention -- a 92-second supervisor thread pinning
/// a core in *every* chunk it lands in, every build, deterministically
/// present regardless of outcome (it always exceeds budget; the doc comment
/// on [`PATHOLOGICAL_LINE_THRESHOLD`] has the measurement). This function
/// removes it from the race entirely: content this returns `true` for is
/// classified unparseable *before any parse is attempted and before any
/// thread is spawned* -- a pure function of the file's own bytes, identical
/// on every call, on every machine, under any concurrent load, forever.
/// Removing dotnet-runtime's one heaviest, always-present contention source
/// is expected to remove the specific +1-edge nondeterminism
/// measured (that borderline file's own solo parse time is nowhere near
/// 10s -- see `parse_time_probe`'s measurements
/// -- so its flip was contention-driven, not intrinsic).
///
/// This is *not* a claim that every conceivable pathological file is now
/// classified without a clock: [`PATHOLOGICAL_LINE_THRESHOLD`]'s doc
/// comment shows the underlying pathology is grammar-specific, not a
/// provable function of content shape, so [`parse_tree_within_budget`]
/// stays in place as a fallback at both call sites for whatever this
/// coarse predicate doesn't catch -- disclosed residual risk, not silently
/// dropped protection.
pub fn is_pathological_large_file(content: &str) -> bool {
    content.len() as u64 > PATHOLOGICAL_LINE_THRESHOLD
        && max_line_len(content) as u64 > PATHOLOGICAL_LINE_THRESHOLD
}

/// Parse `content`, optionally reusing `old_tree` for an incremental reparse.
///
/// The caller is responsible for having already applied the matching
/// [`tree_sitter::InputEdit`]s to `old_tree`; passing an un-edited or stale
/// tree yields a wrong parse, which is exactly why sem-core's own entry points
/// never do this. It is public so callers that *do* track edits (and the
/// incremental benchmark) can measure and use it.
pub fn parse_tree_incremental(
    config: &'static languages::LanguageConfig,
    content: &str,
    old_tree: Option<&tree_sitter::Tree>,
) -> Option<tree_sitter::Tree> {
    let language = (config.get_language)()?;

    PARSER_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let parser = cache.entry(config.id).or_insert_with(|| {
            let mut p = tree_sitter::Parser::new();
            let _ = p.set_language(&language);
            p
        });

        parser.parse(content.as_bytes(), old_tree)
    })
}

fn has_non_comment_content(node: tree_sitter::Node, source: &[u8]) -> bool {
    let mut worklist = Vec::new();
    let mut cursor = node.walk();
    worklist.extend(node.children(&mut cursor));

    while let Some(node) = worklist.pop() {
        if is_comment_node(node.kind()) {
            continue;
        }

        if node.child_count() == 0 {
            let start = node.start_byte();
            let end = node.end_byte();
            if start < end
                && end <= source.len()
                && source[start..end].iter().any(|b| !b.is_ascii_whitespace())
            {
                return true;
            }
            continue;
        }

        let mut cursor = node.walk();
        worklist.extend(node.children(&mut cursor));
    }

    false
}

fn is_comment_node(kind: &str) -> bool {
    matches!(
        kind,
        "comment" | "line_comment" | "block_comment" | "doc_comment" | "tag_comment"
    )
}

fn shebang_line(content: &str) -> Option<&str> {
    content
        .strip_prefix("#!")
        .map(|rest| rest.lines().next().unwrap_or(""))
}

impl SemanticParserPlugin for CodeParserPlugin {
    fn id(&self) -> &str {
        "code"
    }

    fn extensions(&self) -> &[&str] {
        get_all_code_extensions()
    }

    /// Content-addressed: identical `(content, file_path)` is served from the
    /// process-local cache instead of re-parsing. See [`crate::parser::cache`]
    /// for the budget and the env switches that turn it off.
    ///
    /// `extract_entities_with_tree` is deliberately *not* cached — it hands the
    /// caller the `Tree`, which is neither cheap to keep nor `Sync`.
    ///
    /// This is also the one seam an installed
    /// [`FastExtractor`](crate::parser::fast_extractor::FastExtractor) sits
    /// behind: it is the entities-only API, so a parser with no tree-sitter
    /// `Tree` to hand back can answer it in full. `extract_entities_with_tree`
    /// is deliberately *not* routed through the fast path — its callers (pass
    /// 1 of `EntityGraph::build`) need the tree itself, and a fast path there
    /// would trade a parallel parse for a serial pass-2 re-parse. A decline
    /// (`None`) falls through to tree-sitter with no observable difference
    /// beyond timing.
    fn extract_entities(&self, content: &str, file_path: &str) -> Vec<SemanticEntity> {
        cache::get_or_extract(
            "code",
            file_path,
            content,
            || match fast_extractor::try_extract(file_path, content) {
                Some(entities) => entities,
                None => self.extract_entities_with_tree(content, file_path).0,
            },
        )
    }

    fn extract_entities_with_tree(
        &self,
        content: &str,
        file_path: &str,
    ) -> (Vec<SemanticEntity>, Option<tree_sitter::Tree>) {
        let Some(config) = language_config_for_content(content, file_path) else {
            return (Vec::new(), None);
        };

        // gave pass 1 a wall-clock ceiling for large files (a single
        // pathological file could otherwise pin pass 1 for tens of seconds).
        // replaced it outright with a deterministic, pure-per-file
        // predicate -- see `is_pathological_large_file`'s doc comment for why
        // a hybrid (predicate-then-budget-fallback) was tried first and
        // measurement disproved it: with the fallback still in place, the
        // exact dotnet-runtime chunk-boundary edge-count flip found
        // still reproduced (981,283 at a 5,000-file chunk vs 981,284 at a
        // 1,000-file chunk, stable across repeat runs at each size) --
        // proof the flipping file was never `EncryptedXmlSample4.xml`, and
        // that *any* file still going through `parse_tree_within_budget`'s
        // wall-clock race keeps the bug alive regardless of the predicate.
        // With the fallback removed entirely, both chunk sizes produce
        // identical entities/edges. No
        // thread is spawned on this path any more for any file: flagged
        // content is rejected before a parse is attempted, and every other
        // file -- including every large-but-healthy file the old budget
        // mechanism used to race against a 10s clock -- goes through the
        // same plain, unconditional `parse_tree` a small file always has.
        let tree = if is_pathological_large_file(content) {
            None
        } else {
            parse_tree(config, content)
        };
        let Some(tree) = tree else {
            return (Vec::new(), None);
        };

        let entities = extract_entities(&tree, file_path, config, content);
        (entities, Some(tree))
    }

    /// Also content-addressed — it otherwise re-parses the file from scratch.
    fn structural_hash_content(&self, content: &str, file_path: &str) -> Option<String> {
        cache::get_or_structural_hash("code", file_path, content, || {
            let config = language_config_for_content(content, file_path)?;
            let tree = parse_tree(config, content)?;
            let shebang = shebang_line(content);
            if shebang.is_none() && !has_non_comment_content(tree.root_node(), content.as_bytes()) {
                return Some(String::new());
            }
            let structural = structural_hash(tree.root_node(), content.as_bytes());
            match shebang {
                Some(shebang) => Some(content_hash(&format!("shebang:{shebang}\n{structural}"))),
                None => Some(structural),
            }
        })
    }
}

use crate::parser::registry::detect_ext_from_content;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_java_entity_extraction() {
        let code = r#"
package com.example;

import java.util.List;

public class UserService {
    private String name;

    public UserService(String name) {
        this.name = name;
    }

    public List<User> getUsers() {
        return db.findAll();
    }

    public void createUser(User user) {
        db.save(user);
    }
}

interface Repository<T> {
    T findById(String id);
    List<T> findAll();
}

enum Status {
    ACTIVE,
    INACTIVE,
    DELETED
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "UserService.java");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        let types: Vec<&str> = entities.iter().map(|e| e.entity_type.as_str()).collect();
        eprintln!(
            "Java entities: {:?}",
            names.iter().zip(types.iter()).collect::<Vec<_>>()
        );

        assert!(
            names.contains(&"UserService"),
            "Should find class UserService, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Repository"),
            "Should find interface Repository, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Status"),
            "Should find enum Status, got: {:?}",
            names
        );

        // A field is named by its declarator, not its type: `private String name;`
        // is the field `name`, not `String`.
        let field = entities
            .iter()
            .find(|e| e.entity_type == "field")
            .expect("should extract the field entity");
        assert_eq!(
            field.name, "name",
            "field should be named by its declarator, got: {:?}",
            field.name
        );
    }

    #[test]
    fn test_java_nested_methods() {
        let code = r#"
public class Calculator {
    public int add(int a, int b) {
        return a + b;
    }

    public int subtract(int a, int b) {
        return a - b;
    }
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "Calculator.java");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!(
            "Java nested: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type, &e.parent_id))
                .collect::<Vec<_>>()
        );

        assert!(
            names.contains(&"Calculator"),
            "Should find Calculator class"
        );
        assert!(
            names.contains(&"add"),
            "Should find add method, got: {:?}",
            names
        );
        assert!(
            names.contains(&"subtract"),
            "Should find subtract method, got: {:?}",
            names
        );

        // Methods should have Calculator as parent
        let add = entities.iter().find(|e| e.name == "add").unwrap();
        assert!(add.parent_id.is_some(), "add should have parent_id");
    }

    #[test]
    fn test_c_entity_extraction() {
        let code = r#"
#include <stdio.h>

struct Point {
    int x;
    int y;
};

enum Color {
    RED,
    GREEN,
    BLUE
};

typedef struct {
    char name[50];
    int age;
} Person;

void greet(const char* name) {
    printf("Hello, %s!\n", name);
}

int add(int a, int b) {
    return a + b;
}

int main() {
    greet("world");
    return 0;
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "main.c");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        let types: Vec<&str> = entities.iter().map(|e| e.entity_type.as_str()).collect();
        eprintln!(
            "C entities: {:?}",
            names.iter().zip(types.iter()).collect::<Vec<_>>()
        );

        assert!(
            names.contains(&"greet"),
            "Should find greet function, got: {:?}",
            names
        );
        assert!(
            names.contains(&"add"),
            "Should find add function, got: {:?}",
            names
        );
        assert!(
            names.contains(&"main"),
            "Should find main function, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Point"),
            "Should find Point struct, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Color"),
            "Should find Color enum, got: {:?}",
            names
        );
    }

    #[test]
    fn test_c_function_locals_not_extracted() {
        let code = r#"
int global_count = 0;
int helper(void);

int main(void) {
    int local = helper();
    const char *message = "hello";
    return local + global_count;
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "main.c");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"global_count"), "got: {:?}", names);
        assert!(names.contains(&"helper"), "got: {:?}", names);
        assert!(names.contains(&"main"), "got: {:?}", names);
        assert!(!names.contains(&"local"), "got: {:?}", names);
        assert!(!names.contains(&"message"), "got: {:?}", names);
    }

    #[test]
    fn test_cpp_entity_extraction() {
        let code = "namespace math {\nclass Vector3 {\npublic:\n    float length() const { return 0; }\n};\n}\nvoid greet() {}\n";
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "main.cpp");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"math"), "got: {:?}", names);
        assert!(names.contains(&"Vector3"), "got: {:?}", names);
        assert!(names.contains(&"greet"), "got: {:?}", names);
    }

    #[test]
    fn test_cpp_function_locals_not_extracted() {
        let code = r#"
int global_value = 1;
int helper();

int main() {
    int local = helper();
    auto lambda = []() {
        int lambda_local = 3;
        return lambda_local;
    };
    return local + lambda();
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "main.cpp");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"global_value"), "got: {:?}", names);
        assert!(names.contains(&"helper"), "got: {:?}", names);
        assert!(names.contains(&"main"), "got: {:?}", names);
        assert!(!names.contains(&"local"), "got: {:?}", names);
        assert!(!names.contains(&"lambda"), "got: {:?}", names);
        assert!(!names.contains(&"lambda_local"), "got: {:?}", names);
    }

    #[test]
    fn test_ruby_entity_extraction() {
        let code = "module Auth\n  class User\n    def greet\n      \"hi\"\n    end\n  end\nend\ndef helper(x)\n  x * 2\nend\n";
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "auth.rb");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"Auth"), "got: {:?}", names);
        assert!(names.contains(&"User"), "got: {:?}", names);
        assert!(names.contains(&"helper"), "got: {:?}", names);
    }

    #[test]
    fn test_csharp_entity_extraction() {
        let code = "namespace MyApp {\npublic class User {\n    public string GetName() { return \"\"; }\n}\npublic enum Role { Admin, User }\n}\n";
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "Models.cs");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"MyApp"), "got: {:?}", names);
        assert!(names.contains(&"User"), "got: {:?}", names);
        assert!(names.contains(&"Role"), "got: {:?}", names);
    }

    #[test]
    fn test_swift_entity_extraction() {
        let code = r#"
import Foundation

typealias Handler = (Int) -> Void

prefix operator ~~~

class UserService {
    var name: String

    init(name: String) {
        self.name = name
    }

    deinit {
        print("freed")
    }

    func getUsers() -> [User] {
        return db.findAll()
    }
}

struct Point {
    var x: Double
    var y: Double

    subscript(index: Int) -> Double {
        return x + y + Double(index)
    }
}

enum Status {
    case active
    case inactive
    case deleted
}

protocol Repository {
    associatedtype Canvas
    func findById(id: String) -> Canvas?
    func findAll() -> [Canvas]
}

func helper(x: Int) -> Int {
    return x * 2
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "UserService.swift");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!(
            "Swift entities: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type))
                .collect::<Vec<_>>()
        );

        assert!(
            names.contains(&"UserService"),
            "Should find class UserService, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Point"),
            "Should find struct Point, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Status"),
            "Should find enum Status, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Repository"),
            "Should find protocol Repository, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Canvas"),
            "Should find associatedtype Canvas, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Handler"),
            "Should find typealias Handler, got: {:?}",
            names
        );
        assert!(
            names.contains(&"~~~"),
            "Should find custom operator ~~~, got: {:?}",
            names
        );
        assert!(
            names.contains(&"init"),
            "Should find initializer init, got: {:?}",
            names
        );
        assert!(
            names.contains(&"deinit"),
            "Should find deinitializer deinit, got: {:?}",
            names
        );
        assert!(
            names.contains(&"subscript"),
            "Should find subscript, got: {:?}",
            names
        );
        assert!(
            names.contains(&"helper"),
            "Should find function helper, got: {:?}",
            names
        );

        let handler = entities.iter().find(|e| e.name == "Handler").unwrap();
        assert_eq!(handler.entity_type, "type");
        assert!(handler.parent_id.is_none());

        let operator = entities.iter().find(|e| e.name == "~~~").unwrap();
        assert_eq!(operator.entity_type, "operator");
        assert!(operator.parent_id.is_none());

        let user_service = entities.iter().find(|e| e.name == "UserService").unwrap();
        assert_eq!(user_service.entity_type, "class");

        let initializer = entities.iter().find(|e| e.name == "init").unwrap();
        assert_eq!(initializer.entity_type, "init");
        assert_eq!(
            initializer.parent_id.as_deref(),
            Some(user_service.id.as_str())
        );
        assert_eq!(
            initializer.id,
            "UserService.swift::class::UserService::init"
        );

        let deinitializer = entities.iter().find(|e| e.name == "deinit").unwrap();
        assert_eq!(deinitializer.entity_type, "deinit");
        assert_eq!(
            deinitializer.parent_id.as_deref(),
            Some(user_service.id.as_str())
        );
        assert_eq!(
            deinitializer.id,
            "UserService.swift::class::UserService::deinit"
        );

        let point = entities.iter().find(|e| e.name == "Point").unwrap();
        assert_eq!(point.entity_type, "struct");

        let subscript = entities.iter().find(|e| e.name == "subscript").unwrap();
        assert_eq!(subscript.entity_type, "subscript");
        assert_eq!(subscript.parent_id.as_deref(), Some(point.id.as_str()));
        assert_eq!(subscript.id, "UserService.swift::struct::Point::subscript");

        let status = entities.iter().find(|e| e.name == "Status").unwrap();
        assert_eq!(status.entity_type, "enum");

        let repository = entities.iter().find(|e| e.name == "Repository").unwrap();
        assert_eq!(repository.entity_type, "protocol");
        assert_eq!(repository.id, "UserService.swift::protocol::Repository");

        let canvas = entities.iter().find(|e| e.name == "Canvas").unwrap();
        assert_eq!(canvas.entity_type, "associatedtype");
        assert_eq!(canvas.parent_id.as_deref(), Some(repository.id.as_str()));
        assert_eq!(canvas.id, "UserService.swift::protocol::Repository::Canvas");
    }

    #[test]
    fn test_swift_multi_binding_property_extraction() {
        let code = r#"
struct Point {
    var x, y: Int
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "Point.swift");
        let point = entities.iter().find(|e| e.name == "Point").unwrap();
        let properties: Vec<_> = entities
            .iter()
            .filter(|e| e.entity_type == "property")
            .collect();

        assert_eq!(
            properties
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            vec!["x", "y"]
        );
        assert!(properties
            .iter()
            .all(|property| property.parent_id.as_deref() == Some(point.id.as_str())));
        assert_eq!(properties[0].content, "var x: Int");
        assert_eq!(properties[1].content, "var y: Int");
    }

    #[test]
    fn test_swift_multi_binding_property_content_is_per_binding() {
        let typed_code = r#"
struct Types {
    var x: Int, y: String
}
"#;
        let plugin = CodeParserPlugin;
        let typed_entities = plugin.extract_entities(typed_code, "Types.swift");
        let typed_properties: Vec<_> = typed_entities
            .iter()
            .filter(|e| e.entity_type == "property")
            .collect();
        assert_eq!(typed_properties[0].content, "var x: Int");
        assert_eq!(typed_properties[1].content, "var y: String");

        let mixed_code = r#"
struct Mixed {
    var x, y: Int, z: String
}
"#;
        let mixed_entities = plugin.extract_entities(mixed_code, "Mixed.swift");
        let mixed_properties: Vec<_> = mixed_entities
            .iter()
            .filter(|e| e.entity_type == "property")
            .collect();
        assert_eq!(mixed_properties[0].content, "var x: Int");
        assert_eq!(mixed_properties[1].content, "var y: Int");
        assert_eq!(mixed_properties[2].content, "var z: String");

        let generic_code = r#"
struct GenericTypes {
    var lookup: Dictionary<String, Int>, count: Int
}
"#;
        let generic_entities = plugin.extract_entities(generic_code, "GenericTypes.swift");
        let generic_properties: Vec<_> = generic_entities
            .iter()
            .filter(|e| e.entity_type == "property")
            .collect();
        assert_eq!(
            generic_properties[0].content,
            "var lookup: Dictionary<String, Int>"
        );
        assert_eq!(generic_properties[1].content, "var count: Int");

        let initializer_code = r#"
struct Initializers {
    var a = Foo(), b = Bar()
}
"#;
        let initializer_entities = plugin.extract_entities(initializer_code, "Initializers.swift");
        let initializer_properties: Vec<_> = initializer_entities
            .iter()
            .filter(|e| e.entity_type == "property")
            .collect();
        assert!(initializer_properties[0].content.contains("Foo()"));
        assert!(!initializer_properties[0].content.contains("Bar()"));
        assert!(initializer_properties[1].content.contains("Bar()"));
        assert!(!initializer_properties[1].content.contains("Foo()"));

        let constants_code = r#"
struct Constants {
    let first, second, third: Int
}
"#;
        let constants_entities = plugin.extract_entities(constants_code, "Constants.swift");
        let constants_properties: Vec<_> = constants_entities
            .iter()
            .filter(|e| e.entity_type == "property")
            .collect();
        assert_eq!(
            constants_properties
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second", "third"]
        );
        assert_eq!(constants_properties[0].content, "let first: Int");
        assert_eq!(constants_properties[1].content, "let second: Int");
        assert_eq!(constants_properties[2].content, "let third: Int");

        let semicolon_code = r#"
struct Semicolons {
    var left, right: Int; var next: Int
}
"#;
        let semicolon_entities = plugin.extract_entities(semicolon_code, "Semicolons.swift");
        let semicolon_properties: Vec<_> = semicolon_entities
            .iter()
            .filter(|e| e.entity_type == "property")
            .collect();
        assert_eq!(semicolon_properties[0].content, "var left: Int");
        assert_eq!(semicolon_properties[1].content, "var right: Int");
        assert_eq!(semicolon_properties[2].content, "var next: Int");
    }

    #[test]
    fn test_swift_body_locals_not_extracted_as_properties() {
        let code = r#"
class Cache {
    var stored: Int

    var computed: Int {
        let computedLocal = stored + 1
        func computedNested() -> Int {
            return computedLocal
        }
        return computedNested()
    }

    var explicit: Int {
        get {
            let getterLocal = stored
            func getterNested() -> Int {
                return getterLocal
            }
            return getterNested()
        }
    }

    init(seed: Int) {
        let initial = seed
        self.stored = initial
    }

    func value() -> Int {
        let doubled = stored * 2
        var offset = doubled + 1
        func nested() -> Int {
            let insideNested = offset
            return insideNested
        }
        return nested()
    }

    subscript(index: Int) -> Int {
        let shifted = index + stored
        func subscriptNested() -> Int {
            return shifted
        }
        return subscriptNested()
    }

    deinit {
        let closing = stored
        _ = closing
    }
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "Cache.swift");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"Cache"), "got: {:?}", names);
        assert!(names.contains(&"stored"), "got: {:?}", names);
        assert!(names.contains(&"computed"), "got: {:?}", names);
        assert!(names.contains(&"explicit"), "got: {:?}", names);
        assert!(names.contains(&"init"), "got: {:?}", names);
        assert!(names.contains(&"value"), "got: {:?}", names);
        assert!(names.contains(&"computedNested"), "got: {:?}", names);
        assert!(names.contains(&"getterNested"), "got: {:?}", names);
        assert!(names.contains(&"nested"), "got: {:?}", names);
        assert!(names.contains(&"subscriptNested"), "got: {:?}", names);
        assert!(names.contains(&"subscript"), "got: {:?}", names);
        assert!(names.contains(&"deinit"), "got: {:?}", names);
        assert!(!names.contains(&"Int"), "got: {:?}", names);

        for local in [
            "computedLocal",
            "getterLocal",
            "initial",
            "doubled",
            "offset",
            "insideNested",
            "shifted",
            "closing",
        ] {
            assert!(
                !names.contains(&local),
                "{local} should not be an entity. Got: {:?}",
                names
            );
        }
    }

    #[test]
    fn test_swift_suppressed_multi_binding_initializers_are_traversed() {
        let code = r#"
func outer() {
    let a = { func innerA() -> Int { 1 } },
        b = { func innerB() -> Int { 2 } }
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "Locals.swift");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"outer"), "got: {:?}", names);
        assert!(names.contains(&"innerA"), "got: {:?}", names);
        assert!(names.contains(&"innerB"), "got: {:?}", names);
        assert!(
            !names.contains(&"a"),
            "local binding should stay suppressed: {:?}",
            names
        );
        assert!(
            !names.contains(&"b"),
            "local binding should stay suppressed: {:?}",
            names
        );
    }

    #[test]
    fn test_swift_conditional_compilation_inside_struct() {
        let code = r#"
import ArgumentParser

public struct TuistCommand: AsyncParsableCommand {
    public init() {}

    public static var configuration: CommandConfiguration {
        let comment = "brace in string }"
        let multiline = """
        brace in multiline }
        escaped \"""
        """
        /* brace in comment } */
        CommandConfiguration(commandName: "tuist")
    }

    #if os(macOS)
        public static var groupedSubcommands: [ParsableCommand.Type] {
            [InstallCommand.self]
        }
    #else
        public static var groupedSubcommands: [ParsableCommand.Type] {
            []
        }
    #endif

    public func run() async throws {}
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "TuistCommand.swift");
        eprintln!(
            "Swift conditional entities: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type, &e.parent_id))
                .collect::<Vec<_>>()
        );

        let command = entities
            .iter()
            .find(|e| e.name == "TuistCommand")
            .expect("Should recover TuistCommand struct");
        assert_eq!(command.entity_type, "struct");
        assert!(command.parent_id.is_none());

        let renamed_code = code.replace("TuistCommand", "RenamedCommand");
        let renamed_entities = plugin.extract_entities(&renamed_code, "TuistCommand.swift");
        let renamed_command = renamed_entities
            .iter()
            .find(|e| e.name == "RenamedCommand")
            .expect("Should recover renamed command struct");
        assert_eq!(command.structural_hash, renamed_command.structural_hash);

        for member in ["init", "configuration", "run"] {
            let entity = entities
                .iter()
                .find(|e| e.name == member)
                .unwrap_or_else(|| panic!("Should find {member}"));
            assert_eq!(entity.parent_id.as_deref(), Some(command.id.as_str()));
        }

        let grouped_subcommands: Vec<_> = entities
            .iter()
            .filter(|e| e.name == "groupedSubcommands")
            .collect();
        assert_eq!(grouped_subcommands.len(), 2);
        assert!(grouped_subcommands
            .iter()
            .all(|entity| entity.parent_id.as_deref() == Some(command.id.as_str())));
    }

    #[test]
    fn test_swift_conditional_compilation_with_interpolated_brace_string() {
        let plugin = CodeParserPlugin;
        for (container_name, code) in [
            (
                "Config",
                r#"
class Config {
    let tpl = "prefix \("}") suffix"
#if DEBUG
    func dump() { print(tpl) }
#endif
    func render() -> String { return tpl }
}

struct Tail { let q: Int }
"#,
            ),
            (
                "RawConfig",
                r##"
class RawConfig {
    let tpl = #"prefix \#("{") suffix"#
#if DEBUG
    func dump() { print(tpl) }
#endif
    func render() -> String { return tpl }
}
"##,
            ),
            (
                "MultilineConfig",
                r#"
class MultilineConfig {
    let tpl = """
    prefix \("}") suffix
    """
#if DEBUG
    func dump() { print(tpl) }
#endif
    func render() -> String { return tpl }
}
"#,
            ),
            (
                "ClosureConfig",
                r#"
class ClosureConfig {
    let tpl = "prefix \(["}"].map { $0 }.joined()) suffix"
#if DEBUG
    func dump() { print(tpl) }
#endif
    func render() -> String { return tpl }
}
"#,
            ),
        ] {
            let file_path = format!("{container_name}.swift");
            let entities = plugin.extract_entities(code, &file_path);
            let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
            let container = entities
                .iter()
                .find(|e| e.name == container_name)
                .unwrap_or_else(|| {
                    panic!("Should recover {container_name}, got: {names:?}");
                });
            assert_eq!(container.entity_type, "class");
            assert!(container.parent_id.is_none());

            for member in ["tpl", "dump", "render"] {
                let entity = entities
                    .iter()
                    .find(|e| e.name == member)
                    .unwrap_or_else(|| {
                        panic!("Should find {member} in {container_name}, got: {names:?}");
                    });
                assert_eq!(entity.parent_id.as_deref(), Some(container.id.as_str()));
            }
        }
    }

    #[test]
    fn test_elixir_entity_extraction() {
        let code = r#"
defmodule MyApp.Accounts do
  def create_user(attrs) do
    %User{}
    |> User.changeset(attrs)
    |> Repo.insert()
  end

  defp validate(attrs) do
    # private helper
    :ok
  end

  defmacro is_admin(user) do
    quote do
      unquote(user).role == :admin
    end
  end

  defguard is_positive(x) when is_integer(x) and x > 0
end

defprotocol Printable do
  def to_string(data)
end

defimpl Printable, for: Integer do
  def to_string(i), do: Integer.to_string(i)
end
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "accounts.ex");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        let types: Vec<&str> = entities.iter().map(|e| e.entity_type.as_str()).collect();
        eprintln!(
            "Elixir entities: {:?}",
            names.iter().zip(types.iter()).collect::<Vec<_>>()
        );

        assert!(
            names.contains(&"MyApp.Accounts"),
            "Should find module, got: {:?}",
            names
        );
        assert!(
            names.contains(&"create_user"),
            "Should find def, got: {:?}",
            names
        );
        assert!(
            names.contains(&"validate"),
            "Should find defp, got: {:?}",
            names
        );
        assert!(
            names.contains(&"is_admin"),
            "Should find defmacro, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Printable"),
            "Should find defprotocol, got: {:?}",
            names
        );

        // Verify nesting: create_user should have MyApp.Accounts as parent
        let create_user = entities.iter().find(|e| e.name == "create_user").unwrap();
        assert!(
            create_user.parent_id.is_some(),
            "create_user should be nested under module"
        );
    }

    #[test]
    #[cfg(feature = "lang-clojure")]
    fn test_clojure_entity_extraction() {
        let code = r#"
(ns my.app.core
  (:require [clojure.string :as str]))

(def my-var 42)

(def ^:private secret "hunter2")

(defonce connection (atom nil))

(defn greet
  "Returns a greeting string."
  [name]
  (str "Hello, " name "!"))

(defmacro unless [pred & body]
  `(when (not ~pred) ~@body))

(defprotocol Greeter
  (greet! [this name]))

(defrecord Person [name age])

(defmulti area :shape)

(defmethod area :circle [{:keys [radius]}]
  (* Math/PI radius radius))

(defmethod area :rectangle [{:keys [width height]}]
  (* width height))
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "core.clj");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!(
            "Clojure entities: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type))
                .collect::<Vec<_>>()
        );

        assert!(
            !names.contains(&"my.app.core"),
            "Should not extract ns form as entity, got: {:?}",
            names
        );
        assert!(
            names.contains(&"my-var"),
            "Should find def, got: {:?}",
            names
        );
        assert!(
            names.contains(&"secret"),
            "Should strip ^:private metadata from name, got: {:?}",
            names
        );
        assert!(
            names.contains(&"connection"),
            "Should find defonce, got: {:?}",
            names
        );
        assert!(
            names.contains(&"greet"),
            "Should find defn, got: {:?}",
            names
        );
        assert!(
            names.contains(&"unless"),
            "Should find defmacro, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Greeter"),
            "Should find defprotocol, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Person"),
            "Should find defrecord, got: {:?}",
            names
        );
        assert!(
            names.contains(&"area"),
            "Should find defmulti, got: {:?}",
            names
        );
        // defmethods get dispatch-qualified names so two methods on the same multimethod are distinct
        assert!(
            names.contains(&"area/:circle"),
            "Should find defmethod area :circle, got: {:?}",
            names
        );
        assert!(
            names.contains(&"area/:rectangle"),
            "Should find defmethod area :rectangle, got: {:?}",
            names
        );
        let ids: Vec<&str> = entities.iter().map(|e| e.id.as_str()).collect();
        assert!(
            ids.iter().collect::<std::collections::HashSet<_>>().len() == ids.len(),
            "All entity IDs must be unique, got: {:?}",
            ids
        );
    }

    #[test]
    #[cfg(feature = "lang-clojure")]
    fn test_clojure_defn_private() {
        let code = r#"
(ns my.app)

(defn- private-helper [x]
  (* x 2))
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "app.clj");
        let entity = entities
            .iter()
            .find(|e| e.name == "private-helper")
            .expect("Should extract defn- as a function entity");
        assert_eq!(entity.entity_type, "function");
    }

    #[test]
    #[cfg(feature = "lang-clojure")]
    fn test_clojure_predicate_and_bang_functions() {
        let code = r#"
(ns my.app.validators)

(defn empty? [coll]
  (= 0 (count coll)))

(defn reset! [state new-val]
  (compare-and-set! state @state new-val))
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "validators.clj");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"empty?"),
            "Should extract predicate fn empty?, got: {:?}",
            names
        );
        assert!(
            names.contains(&"reset!"),
            "Should extract bang fn reset!, got: {:?}",
            names
        );
        let empty_entity = entities.iter().find(|e| e.name == "empty?").unwrap();
        let reset_entity = entities.iter().find(|e| e.name == "reset!").unwrap();
        assert_eq!(empty_entity.entity_type, "function");
        assert_eq!(reset_entity.entity_type, "function");
    }

    #[test]
    #[cfg(feature = "lang-clojure")]
    fn test_clojure_dynamic_vars_and_equality_fns() {
        let code = r#"
(ns my.app.core)

(def *db* (atom nil))

(defn not= [a b]
  (not (= a b)))
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "core.clj");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"*db*"),
            "Should extract dynamic var *db*, got: {:?}",
            names
        );
        assert!(
            names.contains(&"not="),
            "Should extract fn not=, got: {:?}",
            names
        );
        let db_entity = entities.iter().find(|e| e.name == "*db*").unwrap();
        let noteq_entity = entities.iter().find(|e| e.name == "not=").unwrap();
        assert_eq!(db_entity.entity_type, "var");
        assert_eq!(noteq_entity.entity_type, "function");
    }

    #[test]
    #[cfg(feature = "lang-clojure")]
    fn test_clojure_deftype_definterface_defstruct() {
        let code = r#"
(ns my.app)

(deftype MyType [field])

(definterface IFoo
  (foo [this]))

(defstruct point :x :y)
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "app.clj");
        let by_name = |name: &str| entities.iter().find(|e| e.name == name);

        assert!(
            by_name("MyType").is_some(),
            "Should extract deftype, got: {:?}",
            entities.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
        assert_eq!(by_name("MyType").unwrap().entity_type, "type");

        assert!(
            by_name("IFoo").is_some(),
            "Should extract definterface, got: {:?}",
            entities.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
        assert_eq!(by_name("IFoo").unwrap().entity_type, "interface");

        assert!(
            by_name("point").is_some(),
            "Should extract defstruct, got: {:?}",
            entities.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
        assert_eq!(by_name("point").unwrap().entity_type, "struct");
    }

    #[test]
    #[cfg(feature = "lang-clojure")]
    fn test_clojure_cljc_extension() {
        let code = r#"
(ns my.app.shared)

(defn platform-key [] :default)

(def shared-value 99)
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "shared.cljc");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"platform-key"),
            "Should extract defn from .cljc, got: {:?}",
            names
        );
        assert!(
            names.contains(&"shared-value"),
            "Should extract def from .cljc, got: {:?}",
            names
        );
    }

    #[test]
    #[cfg(feature = "lang-clojure")]
    fn test_clojure_defmethod_non_keyword_dispatch() {
        let code = r#"
(ns my.app)

(defmulti process identity)

(defmethod process nil [_] :nothing)

(defmethod process "string" [s] s)

(defmethod process 42 [n] n)
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "app.clj");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"process"),
            "Should extract defmulti, got: {:?}",
            names
        );
        assert!(
            names.contains(&"process/nil"),
            "Should extract defmethod with nil dispatch, got: {:?}",
            names
        );
        assert!(
            names.contains(&"process/\"string\""),
            "Should extract defmethod with string dispatch, got: {:?}",
            names
        );
        assert!(
            names.contains(&"process/42"),
            "Should extract defmethod with integer dispatch, got: {:?}",
            names
        );
        let ids: Vec<&str> = entities.iter().map(|e| e.id.as_str()).collect();
        assert!(
            ids.iter().collect::<std::collections::HashSet<_>>().len() == ids.len(),
            "All entity IDs must be unique, got: {:?}",
            ids
        );
    }

    #[test]
    fn test_bash_entity_extraction() {
        let code = r#"#!/bin/bash

greet() {
    echo "Hello, $1!"
}

function deploy {
    echo "deploying..."
}

# not a function
echo "main script"
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "deploy.sh");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        let types: Vec<&str> = entities.iter().map(|e| e.entity_type.as_str()).collect();
        eprintln!(
            "Bash entities: {:?}",
            names.iter().zip(types.iter()).collect::<Vec<_>>()
        );

        assert!(
            names.contains(&"greet"),
            "Should find greet(), got: {:?}",
            names
        );
        assert!(
            names.contains(&"deploy"),
            "Should find function deploy, got: {:?}",
            names
        );
        assert_eq!(
            entities.len(),
            2,
            "Should only find functions, got: {:?}",
            names
        );
    }

    #[test]
    fn test_entity_byte_offsets_slice_source_exactly() {
        // Byte offsets must let a consumer slice the exact original bytes of an
        // entity out of the source given only file_path + the span (#requested
        // by a sem-core user: pull exact content from git by file + entity id).
        let code =
            "import os\n\ndef first(a):\n    return a + 1\n\ndef second(b):\n    return b * 2\n";
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "demo.py");
        let bytes = code.as_bytes();
        let funcs: Vec<_> = entities
            .iter()
            .filter(|e| e.entity_type == "function")
            .collect();
        assert_eq!(funcs.len(), 2, "expected 2 functions, got {:?}", funcs);
        for e in funcs {
            let sb = e.start_byte.expect("function entity must carry start_byte");
            let eb = e.end_byte.expect("function entity must carry end_byte");
            let sliced = std::str::from_utf8(&bytes[sb..eb]).unwrap();
            assert!(
                sliced.starts_with(&format!("def {}", e.name)),
                "bytes[{sb}..{eb}] = {sliced:?} should be the body of {}",
                e.name
            );
        }
    }

    #[test]
    #[cfg(feature = "lang-lua")]
    fn test_lua_entity_extraction() {
        let code = r#"local M = {}

function greet(name)
    return "hello " .. name
end

local function helper(x)
    return x * 2
end

function M.compute(a, b)
    return helper(a) + helper(b)
end

function M:method(v)
    return v
end

return M
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "demo.lua");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        // global, local, table (dot) and method (colon) forms all extract
        assert!(
            names.contains(&"greet"),
            "global function, got: {:?}",
            names
        );
        assert!(
            names.contains(&"helper"),
            "local function, got: {:?}",
            names
        );
        assert!(
            names.contains(&"M.compute"),
            "table function, got: {:?}",
            names
        );
        assert!(
            names.contains(&"M:method"),
            "method function, got: {:?}",
            names
        );
        assert_eq!(entities.len(), 4, "only functions, got: {:?}", names);
    }

    #[test]
    #[cfg(feature = "lang-fish")]
    fn test_fish_entity_extraction() {
        let code = r#"function greet
    echo "hello $argv[1]"
end

# the config.fish pattern: definitions wrapped in a top-level guard
if status is-interactive
    function fish_prompt
        set_color green
        echo -n (prompt_pwd) '> '
    end
end

function notify --on-event fish_command_finished --description "ping on done"
    greet $argv
end
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "config.fish");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"greet"), "plain function, got: {:?}", names);
        assert!(
            names.contains(&"fish_prompt"),
            "function inside a top-level if block, got: {:?}",
            names
        );
        assert!(
            names.contains(&"notify"),
            "function with option flags, got: {:?}",
            names
        );
        assert_eq!(entities.len(), 3, "only functions, got: {:?}", names);
    }

    #[test]
    fn test_typescript_entity_extraction() {
        // Existing language should still work
        let code = r#"
export function hello(): string {
    return "hello";
}

export class Greeter {
    greet(name: string): string {
        return `Hello, ${name}!`;
    }
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "test.ts");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"hello"), "Should find hello function");
        assert!(names.contains(&"Greeter"), "Should find Greeter class");
    }

    #[test]
    fn test_same_line_typescript_overload_ids_are_unique() {
        let code = "function f(a: number): void {}; function f(a: string): void {}\n";
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "over.ts");
        let overloads: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|entity| entity.name == "f" && entity.entity_type == "function")
            .collect();
        let ids: Vec<&str> = overloads.iter().map(|entity| entity.id.as_str()).collect();

        assert_eq!(
            overloads.len(),
            2,
            "expected both overloads, got: {entities:?}"
        );
        assert_eq!(
            ids,
            vec!["over.ts::function::f@L1#1", "over.ts::function::f@L1#2"]
        );
    }

    #[test]
    fn test_same_line_duplicate_parent_ids_are_propagated_to_children() {
        let code = "class C { m(){ return 1 } } class C { m(){ return 2 } }\n";
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "c.ts");
        let classes: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|entity| entity.name == "C" && entity.entity_type == "class")
            .collect();
        let methods: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|entity| entity.name == "m" && entity.entity_type == "method")
            .collect();

        assert_eq!(classes.len(), 2, "expected both classes, got: {entities:?}");
        assert_eq!(methods.len(), 2, "expected both methods, got: {entities:?}");
        assert_eq!(classes[0].id, "c.ts::class::C@L1#1");
        assert_eq!(classes[1].id, "c.ts::class::C@L1#2");
        assert_eq!(methods[0].parent_id.as_deref(), Some("c.ts::class::C@L1#1"));
        assert_eq!(methods[1].parent_id.as_deref(), Some("c.ts::class::C@L1#2"));
        assert_eq!(methods[0].id, "c.ts::class::C@L1#1::m");
        assert_eq!(methods[1].id, "c.ts::class::C@L1#2::m");
    }

    #[test]
    fn test_module_typescript_entity_extraction() {
        let code = r#"
export function hello(): string {
    return "hello";
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "test.mts");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"hello"), "Should find hello function");
    }

    #[test]
    fn test_commonjs_typescript_entity_extraction() {
        let code = r#"
export class Greeter {
    greet(name: string): string {
        return `Hello, ${name}!`;
    }
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "test.cts");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"Greeter"), "Should find Greeter class");
        assert!(names.contains(&"greet"), "Should find greet method");
    }

    #[test]
    fn test_typescript_generator_function_entity_extraction() {
        let code = r#"
export async function* streamUsers(): AsyncGenerator<string> {
    yield "alice";
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "stream.ts");
        let stream = entities.iter().find(|e| e.name == "streamUsers");

        assert!(
            stream.is_some(),
            "Should find generator function, got: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type))
                .collect::<Vec<_>>()
        );
        assert_eq!(stream.unwrap().entity_type, "function");
    }

    #[test]
    fn test_javascript_generator_function_entity_extraction() {
        let code = r#"
export function* ids() {
    yield 1;
    yield 2;
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "ids.js");
        let ids = entities.iter().find(|e| e.name == "ids");

        assert!(
            ids.is_some(),
            "Should find generator function, got: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type))
                .collect::<Vec<_>>()
        );
        assert_eq!(ids.unwrap().entity_type, "function");
    }

    #[test]
    fn test_nested_functions_typescript() {
        let code = r#"
function outer() {
    function inner() {
        return 42;
    }
    return inner();
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "nested.ts");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!(
            "Nested TS: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type, &e.parent_id))
                .collect::<Vec<_>>()
        );

        assert!(
            names.contains(&"outer"),
            "Should find outer, got: {:?}",
            names
        );
        assert!(
            names.contains(&"inner"),
            "Should find inner, got: {:?}",
            names
        );

        let inner = entities.iter().find(|e| e.name == "inner").unwrap();
        assert!(inner.parent_id.is_some(), "inner should have parent_id");
    }

    #[test]
    fn test_typescript_nested_anonymous_class_fields() {
        let code = r#"
class L1 {
  L2 = class {
    L3 = class {
      L4 = class {
        method() { return 1; }
      };
    };
  };
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "a.ts");
        let find = |name: &str| {
            entities.iter().find(|e| e.name == name).unwrap_or_else(|| {
                panic!(
                    "missing {name}; got: {:?}",
                    entities
                        .iter()
                        .map(|e| (&e.name, &e.entity_type, &e.parent_id))
                        .collect::<Vec<_>>()
                )
            })
        };

        let l1 = find("L1");
        assert_eq!(l1.entity_type, "class");
        let l1_id = l1.id.clone();

        let l2 = find("L2");
        assert_eq!(l2.entity_type, "field");
        assert_eq!(l2.parent_id.as_deref(), Some(l1_id.as_str()));
        let l2_id = l2.id.clone();

        let l3 = find("L3");
        assert_eq!(l3.entity_type, "field");
        assert_eq!(l3.parent_id.as_deref(), Some(l2_id.as_str()));
        let l3_id = l3.id.clone();

        let l4 = find("L4");
        assert_eq!(l4.entity_type, "field");
        assert_eq!(l4.parent_id.as_deref(), Some(l3_id.as_str()));
        let l4_id = l4.id.clone();

        let method = find("method");
        assert_eq!(method.entity_type, "method");
        assert_eq!(method.parent_id.as_deref(), Some(l4_id.as_str()));
        assert_eq!(method.id, "a.ts::class::L1::L2::L3::L4::method");
    }

    #[test]
    fn test_nested_functions_python() {
        let code = "def outer():\n    def inner():\n        return 42\n    return inner()\n";
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "nested.py");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"outer"), "got: {:?}", names);
        assert!(names.contains(&"inner"), "got: {:?}", names);

        let inner = entities.iter().find(|e| e.name == "inner").unwrap();
        assert!(inner.parent_id.is_some(), "inner should have parent_id");
    }

    #[test]
    fn test_nested_functions_rust() {
        let code = "fn outer() {\n    fn inner() -> i32 {\n        42\n    }\n    inner();\n}\n";
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "nested.rs");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"outer"), "got: {:?}", names);
        assert!(names.contains(&"inner"), "got: {:?}", names);

        let inner = entities.iter().find(|e| e.name == "inner").unwrap();
        assert!(inner.parent_id.is_some(), "inner should have parent_id");
    }

    #[test]
    fn test_rust_impl_blocks_unique_names() {
        let code = r#"
trait Greeting {
    fn greet(&self) -> String;
}

struct Person;
struct Robot;
struct Cat;

impl Greeting for Person {
    fn greet(&self) -> String { "Hello".to_string() }
}

impl Greeting for Robot {
    fn greet(&self) -> String { "Beep".to_string() }
}

impl Greeting for Cat {
    fn greet(&self) -> String { "Meow".to_string() }
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "impls.rs");
        let impl_entities: Vec<&_> = entities
            .iter()
            .filter(|e| e.entity_type == "impl")
            .collect();
        let names: Vec<&str> = impl_entities.iter().map(|e| e.name.as_str()).collect();

        assert_eq!(
            impl_entities.len(),
            3,
            "Should find 3 impl blocks, got: {:?}",
            names
        );
        assert!(names.contains(&"Greeting for Person"), "got: {:?}", names);
        assert!(names.contains(&"Greeting for Robot"), "got: {:?}", names);
        assert!(names.contains(&"Greeting for Cat"), "got: {:?}", names);
    }

    #[test]
    fn test_nested_functions_go() {
        // Go doesn't have named nested functions, but has nested type/var declarations
        let code = "package main\n\nfunc outer() {\n    var x int = 42\n    _ = x\n}\n";
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "nested.go");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"outer"), "got: {:?}", names);
    }

    #[test]
    fn test_renamed_function_same_structural_hash() {
        let code_a = "def get_card():\n    return db.query('cards')\n";
        let code_b = "def get_card_1():\n    return db.query('cards')\n";

        let plugin = CodeParserPlugin;
        let entities_a = plugin.extract_entities(code_a, "a.py");
        let entities_b = plugin.extract_entities(code_b, "b.py");

        assert_eq!(entities_a.len(), 1, "Should find one entity in a");
        assert_eq!(entities_b.len(), 1, "Should find one entity in b");
        assert_eq!(entities_a[0].name, "get_card");
        assert_eq!(entities_b[0].name, "get_card_1");

        // Structural hash should match since only the name differs
        assert_eq!(
            entities_a[0].structural_hash, entities_b[0].structural_hash,
            "Renamed function with identical body should have same structural_hash"
        );

        // Content hash should differ (it includes the name)
        assert_ne!(
            entities_a[0].content_hash, entities_b[0].content_hash,
            "Content hash should differ since raw content includes the name"
        );
    }

    #[test]
    fn test_swift_renamed_operator_same_structural_hash() {
        let plugin = CodeParserPlugin;
        let entities_a = plugin.extract_entities("prefix operator ~~~\n", "a.swift");
        let entities_b = plugin.extract_entities("prefix operator !!!\n", "b.swift");

        assert_eq!(entities_a.len(), 1, "Should find one entity in a");
        assert_eq!(entities_b.len(), 1, "Should find one entity in b");
        assert_eq!(entities_a[0].name, "~~~");
        assert_eq!(entities_b[0].name, "!!!");
        assert_eq!(entities_a[0].entity_type, "operator");
        assert_eq!(entities_b[0].entity_type, "operator");
        assert_eq!(
            entities_a[0].structural_hash, entities_b[0].structural_hash,
            "Renamed operator with otherwise identical declaration should have same structural_hash"
        );
        assert_ne!(
            entities_a[0].content_hash, entities_b[0].content_hash,
            "Content hash should differ since raw content includes the operator token"
        );
    }

    #[test]
    fn test_swift_synthesized_names_disambiguate_overloads() {
        let plugin = CodeParserPlugin;
        let code = r#"
struct Matrix {
    subscript(row: Int) -> Double {
        return Double(row)
    }

    subscript(row: Int, column: Int) -> Double {
        return Double(row + column)
    }
}

class Builder {
    init(value: Int) {}
    init(text: String) {}
}
"#;

        let entities = plugin.extract_entities(code, "Overloads.swift");

        let subscript_ids: Vec<&str> = entities
            .iter()
            .filter(|e| e.entity_type == "subscript")
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(subscript_ids.len(), 2);
        assert_ne!(subscript_ids[0], subscript_ids[1]);
        assert!(subscript_ids.iter().all(|id| id.contains("@L")));

        let init_ids: Vec<&str> = entities
            .iter()
            .filter(|e| e.entity_type == "init")
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(init_ids.len(), 2);
        assert_ne!(init_ids[0], init_ids[1]);
        assert!(init_ids.iter().all(|id| id.contains("@L")));
    }

    #[test]
    fn test_hcl_entity_extraction() {
        let code = r#"
region = "eu-west-1"

variable "image_id" {
  type = string
}

resource "aws_instance" "web" {
  ami = var.image_id

  lifecycle {
    create_before_destroy = true
  }
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "main.tf");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        let types: Vec<&str> = entities.iter().map(|e| e.entity_type.as_str()).collect();
        eprintln!(
            "HCL entities: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type, &e.parent_id))
                .collect::<Vec<_>>()
        );

        assert!(
            names.contains(&"region"),
            "Should find top-level attribute, got: {:?}",
            names
        );
        assert!(
            names.contains(&"variable.image_id"),
            "Should find variable block, got: {:?}",
            names
        );
        assert!(
            names.contains(&"resource.aws_instance.web"),
            "Should find resource block, got: {:?}",
            names
        );
        assert!(
            names.contains(&"resource.aws_instance.web.lifecycle"),
            "Should find nested lifecycle block with qualified name, got: {:?}",
            names
        );
        assert!(
            !names.contains(&"ami"),
            "Should skip nested attributes inside blocks, got: {:?}",
            names
        );
        assert!(
            !names.contains(&"create_before_destroy"),
            "Should skip nested attributes inside nested blocks, got: {:?}",
            names
        );

        let lifecycle = entities
            .iter()
            .find(|e| e.name == "resource.aws_instance.web.lifecycle")
            .unwrap();
        assert!(
            lifecycle.parent_id.is_some(),
            "lifecycle should be nested under resource"
        );
        assert!(
            types.contains(&"attribute"),
            "Should preserve attribute entity type for top-level attributes"
        );
    }

    #[test]
    fn test_kotlin_entity_extraction() {
        let code = r#"
class UserService {
    val name: String = ""

    fun greet(): String {
        return "Hello, $name"
    }

    companion object {
        fun create(): UserService = UserService()
    }
}

interface Repository {
    fun findById(id: Int): Any?
}

object AppConfig {
    val version = "1.0"
}

fun topLevel(x: Int): Int = x * 2
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "App.kt");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!(
            "Kotlin entities: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type))
                .collect::<Vec<_>>()
        );
        assert!(names.contains(&"UserService"), "got: {:?}", names);
        assert!(names.contains(&"greet"), "got: {:?}", names);
        assert!(names.contains(&"Repository"), "got: {:?}", names);
        assert!(names.contains(&"findById"), "got: {:?}", names);
        assert!(names.contains(&"AppConfig"), "got: {:?}", names);
        assert!(names.contains(&"topLevel"), "got: {:?}", names);
    }

    #[test]
    fn test_xml_entity_extraction() {
        let code = r#"<?xml version="1.0" encoding="UTF-8"?>
<project>
    <groupId>com.example</groupId>
    <artifactId>my-app</artifactId>
    <dependencies>
        <dependency>
            <groupId>junit</groupId>
            <artifactId>junit</artifactId>
        </dependency>
    </dependencies>
    <build>
        <plugins>
            <plugin>
                <groupId>org.apache.maven</groupId>
            </plugin>
        </plugins>
    </build>
</project>
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "pom.xml");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!(
            "XML entities: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type))
                .collect::<Vec<_>>()
        );
        assert!(names.contains(&"project"), "got: {:?}", names);
        assert!(names.contains(&"dependencies"), "got: {:?}", names);
        assert!(names.contains(&"build"), "got: {:?}", names);
    }

    #[test]
    fn test_arrow_callback_scope_boundary_typescript() {
        // Arrow function callbacks: locals are suppressed, but inner
        // class/function declarations are still extracted. Nested callbacks
        // also suppress their locals.
        let code = r#"
const activeQueues = [
  { queue: queues.fooQueue, processor: foo.process },
];

activeQueues.forEach((handler: any) => {
  const queue = handler.queue;
  let retries = 0;

  class QueueHandler {
    handle() { return queue; }
  }

  function createHandler() {
    return new QueueHandler();
  }

  queue.process((job) => {
    const orderId = job.data.orderId;
    return orderId;
  });
});

function handleFailure(job: any, err: any) {
  console.error('failed', err);
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "process.ts");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        let top_level: Vec<&str> = entities
            .iter()
            .filter(|e| e.parent_id.is_none())
            .map(|e| e.name.as_str())
            .collect();

        // Top-level entities preserved
        assert!(top_level.contains(&"activeQueues"), "got: {:?}", top_level);
        assert!(top_level.contains(&"handleFailure"), "got: {:?}", top_level);

        // Declarations inside callback extracted
        assert!(names.contains(&"QueueHandler"), "got: {:?}", names);
        assert!(names.contains(&"handle"), "got: {:?}", names);
        assert!(names.contains(&"createHandler"), "got: {:?}", names);

        // Locals inside callbacks suppressed
        assert!(!names.contains(&"queue"), "got: {:?}", names);
        assert!(!names.contains(&"retries"), "got: {:?}", names);
        assert!(!names.contains(&"orderId"), "got: {:?}", names);
    }

    #[test]
    fn test_top_level_iife_wrapper_still_extracts_typescript_entities() {
        let code = r#"
function factory() {
  class Foo {
    method(): number {
      return 1;
    }
  }

  function bar(): Foo {
    return new Foo();
  }
}

factory();
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "wrapped.ts");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"factory"),
            "Should find top-level wrapper function, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Foo"),
            "Should find class inside top-level wrapper, got: {:?}",
            names
        );
        assert!(
            names.contains(&"bar"),
            "Should find function inside top-level wrapper, got: {:?}",
            names
        );
    }

    #[test]
    fn test_top_level_iife_still_extracts_typescript_entities() {
        let code = r#"
(() => {
  class Foo {
    method(): number {
      return 1;
    }
  }

  function bar(): Foo {
    return new Foo();
  }
})();
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "iife.ts");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"Foo"),
            "Should find class inside top-level IIFE, got: {:?}",
            names
        );
        assert!(
            names.contains(&"bar"),
            "Should find function inside top-level IIFE, got: {:?}",
            names
        );
    }

    #[test]
    fn test_function_locals_not_extracted_as_nested_entities_typescript() {
        let code = r#"
export default function foo() {
  const x = 1;
  return x;
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "default-export.ts");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"foo"),
            "Should find exported function, got: {:?}",
            names
        );
        assert!(
            !names.contains(&"x"),
            "Local inside function should not be extracted as an entity, got: {:?}",
            names
        );
    }

    #[test]
    fn test_function_expression_scope_boundary_typescript() {
        // Function expressions: assigned to variables, or used as callback
        // arguments. Locals are suppressed in all cases.
        let code = r#"
const foo = function namedExpr(x: number) {
  const inner = x + 1;
  return inner;
};

const bar = function(y: number) {
  const local = y * 2;
  return local;
};

const items = [1, 2, 3];

items.forEach(function process(item) {
  const doubled = item * 2;
  console.log(doubled);
});
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "funexpr.ts");
        let top_level: Vec<&str> = entities
            .iter()
            .filter(|e| e.parent_id.is_none())
            .map(|e| e.name.as_str())
            .collect();
        let find = |name: &str| entities.iter().find(|e| e.name == name).unwrap();
        let all_names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        // Top-level declarations preserved, and const-assigned function
        // expressions are promoted from variable to function.
        assert!(top_level.contains(&"foo"), "got: {:?}", top_level);
        assert!(top_level.contains(&"bar"), "got: {:?}", top_level);
        assert!(top_level.contains(&"items"), "got: {:?}", top_level);
        assert_eq!(find("foo").entity_type, "function");
        assert_eq!(find("bar").entity_type, "function");
        assert_eq!(find("items").entity_type, "variable");

        // Locals inside function expressions suppressed
        assert!(!all_names.contains(&"inner"), "got: {:?}", all_names);
        assert!(!all_names.contains(&"local"), "got: {:?}", all_names);
        assert!(!all_names.contains(&"doubled"), "got: {:?}", all_names);

        // Named function expression used as callback argument not extracted
        assert!(!top_level.contains(&"process"), "got: {:?}", top_level);
    }

    #[test]
    fn test_variable_assigned_arrow_extracts_inner_entities() {
        // Arrow function assigned to a variable: inner class/function
        // declarations should be extracted, locals should be suppressed.
        let code = r#"
const handler = () => {
  class Inner {
    run() { return 1; }
  }

  function make() {
    return new Inner();
  }

  const local = 42;
};
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "assigned.ts");
        let handler = entities.iter().find(|e| e.name == "handler").unwrap();
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert_eq!(handler.entity_type, "function");
        assert!(names.contains(&"handler"), "got: {:?}", names);
        assert!(names.contains(&"Inner"), "got: {:?}", names);
        assert!(names.contains(&"run"), "got: {:?}", names);
        assert!(names.contains(&"make"), "got: {:?}", names);
        assert!(!names.contains(&"local"), "got: {:?}", names);
    }

    #[test]
    fn test_variable_assigned_function_expression_extracts_inner_entities() {
        // Function expression assigned to a variable: same behavior.
        let code = r#"
const handler = function() {
  class Inner {}
  function make() { return new Inner(); }
  const local = 42;
};
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "funexpr-inner.ts");
        let handler = entities.iter().find(|e| e.name == "handler").unwrap();
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert_eq!(handler.entity_type, "function");
        assert!(names.contains(&"handler"), "got: {:?}", names);
        assert!(names.contains(&"Inner"), "got: {:?}", names);
        assert!(names.contains(&"make"), "got: {:?}", names);
        assert!(!names.contains(&"local"), "got: {:?}", names);
    }

    #[test]
    fn test_let_assigned_arrow_stays_variable_typescript() {
        let code = r#"
let handler = () => {
  return 42;
};
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "let-assigned.ts");
        let handler = entities.iter().find(|e| e.name == "handler").unwrap();

        assert_eq!(handler.entity_type, "variable");
    }

    #[test]
    fn test_const_assigned_arrow_promoted_to_function_javascript() {
        let code = r#"
const handler = () => {
  return 42;
};
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "handler.js");
        let handler = entities.iter().find(|e| e.name == "handler").unwrap();

        assert_eq!(handler.entity_type, "function");
    }

    #[test]
    fn test_js_ts_multi_declarator_promotes_each_const_initializer() {
        let code = r#"
const value = 1, handler = () => value;
const first = () => 1, second = 2;
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "sample.ts");
        let find = |name: &str| {
            entities.iter().find(|e| e.name == name).unwrap_or_else(|| {
                panic!(
                    "missing {name}; got: {:?}",
                    entities
                        .iter()
                        .map(|e| (&e.name, &e.entity_type))
                        .collect::<Vec<_>>()
                )
            })
        };

        assert_eq!(find("value").entity_type, "variable");
        assert_eq!(find("handler").entity_type, "function");
        assert_eq!(find("first").entity_type, "function");
        assert_eq!(find("second").entity_type, "variable");
    }

    #[test]
    fn test_suppressed_multi_declarator_traverses_skipped_initializers() {
        let code = r#"
function wrapper() {
  const holder = class {
    run() { return 1; }
  }, handler = () => {
    class Inner {
      go() { return 2; }
    }
  }, value = 1;
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "sample.ts");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        let find = |name: &str| {
            entities.iter().find(|e| e.name == name).unwrap_or_else(|| {
                panic!(
                    "missing {name}; got: {:?}",
                    entities
                        .iter()
                        .map(|e| (&e.name, &e.entity_type, &e.parent_id))
                        .collect::<Vec<_>>()
                )
            })
        };

        assert_eq!(find("wrapper").entity_type, "function");
        assert_eq!(find("handler").entity_type, "function");
        assert!(names.contains(&"run"), "got: {:?}", names);
        assert!(names.contains(&"Inner"), "got: {:?}", names);
        assert!(names.contains(&"go"), "got: {:?}", names);
        assert!(!names.contains(&"holder"), "got: {:?}", names);
        assert!(!names.contains(&"value"), "got: {:?}", names);
    }

    #[test]
    fn test_go_var_declaration() {
        let code = r#"package featuremgmt

type FeatureFlag struct {
	Name        string
	Description string
	Stage       string
}

var standardFeatureFlags = []FeatureFlag{
	{
		Name:        "panelTitleSearch",
		Description: "Search for dashboards using panel title",
		Stage:       "PublicPreview",
	},
}

func GetFlags() []FeatureFlag {
	return standardFeatureFlags
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "flags.go");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        let types: Vec<&str> = entities.iter().map(|e| e.entity_type.as_str()).collect();
        eprintln!(
            "Go entities: {:?}",
            names.iter().zip(types.iter()).collect::<Vec<_>>()
        );

        assert!(
            names.contains(&"FeatureFlag"),
            "Should find type FeatureFlag, got: {:?}",
            names
        );
        assert!(
            names.contains(&"standardFeatureFlags"),
            "Should find var standardFeatureFlags, got: {:?}",
            names
        );
        assert!(
            names.contains(&"GetFlags"),
            "Should find func GetFlags, got: {:?}",
            names
        );
    }

    #[test]
    fn test_go_grouped_var_declaration() {
        let code = r#"package test

var (
	simple = 42
	flags = []string{"a", "b"}
)

const (
	x = 1
	y = 2
)

func main() {}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "test.go");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        let types: Vec<&str> = entities.iter().map(|e| e.entity_type.as_str()).collect();
        eprintln!(
            "Go grouped entities: {:?}",
            names.iter().zip(types.iter()).collect::<Vec<_>>()
        );

        assert!(
            names.contains(&"flags") || names.contains(&"simple"),
            "Should find grouped var, got: {:?}",
            names
        );
        assert!(
            names.contains(&"x"),
            "Should find grouped const x, got: {:?}",
            names
        );
        assert!(
            names.contains(&"main"),
            "Should find func main, got: {:?}",
            names
        );
    }

    #[test]
    fn test_dart_entity_extraction() {
        let code = r#"
import 'dart:math';

class Calculator {
  final String name;

  Calculator(this.name);

  Calculator.withDefault() : name = 'default';

  factory Calculator.create(String name) {
    return Calculator(name);
  }

  int add(int a, int b) {
    return a + b;
  }

  int get doubleAdd => add(1, 1) * 2;

  set label(String value) {
    // no-op
  }

  int operator +(Calculator other) {
    return 0;
  }
}

mixin Loggable {
  void log(String message) {
    print(message);
  }
}

extension StringExt on String {
  bool get isBlank => trim().isEmpty;
}

enum Status {
  active,
  inactive;

  String display() => name.toUpperCase();
}

typedef Callback = void Function(int);

int add(int a, int b) {
  return a + b;
}

extension type Wrapper(int value) implements int {}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "calculator.dart");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!(
            "Dart entities: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type, &e.parent_id))
                .collect::<Vec<_>>()
        );

        // Top-level declarations
        assert!(
            names.contains(&"Calculator"),
            "Should find class, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Loggable"),
            "Should find mixin, got: {:?}",
            names
        );
        assert!(
            names.contains(&"StringExt"),
            "Should find extension, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Status"),
            "Should find enum, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Callback"),
            "Should find typedef, got: {:?}",
            names
        );
        assert!(
            names.contains(&"add"),
            "Should find top-level function, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Wrapper"),
            "Should find extension type, got: {:?}",
            names
        );

        // Class members with correct types
        let add_method = entities
            .iter()
            .find(|e| e.name == "add" && e.parent_id.is_some());
        assert!(
            add_method.is_some(),
            "Should find add method inside Calculator"
        );
        assert_eq!(add_method.unwrap().entity_type, "method");

        // Named constructor gets distinct name from unnamed constructor
        let unnamed_ctor = entities
            .iter()
            .find(|e| e.name == "Calculator" && e.entity_type == "constructor");
        assert!(unnamed_ctor.is_some(), "Should find unnamed constructor");
        let named_ctor = entities.iter().find(|e| e.name == "Calculator.withDefault");
        assert!(
            named_ctor.is_some(),
            "Should find named constructor Calculator.withDefault, got: {:?}",
            names
        );
        assert_eq!(named_ctor.unwrap().entity_type, "constructor");
        assert_ne!(
            unnamed_ctor.unwrap().id,
            named_ctor.unwrap().id,
            "Named and unnamed constructors must have different entity IDs"
        );

        // Factory constructor
        let factory_ctor = entities.iter().find(|e| e.name == "Calculator.create");
        assert!(
            factory_ctor.is_some(),
            "Should find factory constructor Calculator.create, got: {:?}",
            names
        );
        assert_eq!(factory_ctor.unwrap().entity_type, "constructor");

        // Getter, setter, operator
        let getter = entities.iter().find(|e| e.name == "doubleAdd");
        assert!(getter.is_some(), "Should find getter doubleAdd");
        assert_eq!(getter.unwrap().entity_type, "getter");

        let setter = entities.iter().find(|e| e.name == "label");
        assert!(setter.is_some(), "Should find setter label");
        assert_eq!(setter.unwrap().entity_type, "setter");

        let operator = entities.iter().find(|e| e.name == "operator +");
        assert!(operator.is_some(), "Should find operator +");
        assert_eq!(operator.unwrap().entity_type, "method");

        // Mixin members have parent
        let log_method = entities.iter().find(|e| e.name == "log");
        assert!(log_method.is_some(), "Should find log in Loggable");
        assert!(
            log_method.unwrap().parent_id.is_some(),
            "log should have parent_id"
        );

        // Entity type mapping
        let callback = entities.iter().find(|e| e.name == "Callback").unwrap();
        assert_eq!(callback.entity_type, "type", "typedef should map to 'type'");

        let loggable = entities.iter().find(|e| e.name == "Loggable").unwrap();
        assert_eq!(loggable.entity_type, "mixin");

        let ext = entities.iter().find(|e| e.name == "StringExt").unwrap();
        assert_eq!(ext.entity_type, "extension");

        let wrapper = entities.iter().find(|e| e.name == "Wrapper").unwrap();
        assert_eq!(wrapper.entity_type, "extension");
    }

    #[test]
    #[cfg(feature = "lang-sql")]
    fn test_sql_entity_extraction() {
        let code = r#"
CREATE TABLE users (id INT PRIMARY KEY, name TEXT);
CREATE VIEW active_users AS SELECT * FROM users WHERE active;
CREATE FUNCTION add(a INT, b INT) RETURNS INT AS $$ BEGIN RETURN a + b; END; $$ LANGUAGE plpgsql;
CREATE INDEX idx_name ON users(name);
CREATE TYPE mood AS ENUM ('sad', 'happy');
CREATE SCHEMA myapp;
CREATE MATERIALIZED VIEW mv AS SELECT 1;
CREATE TABLE billing.invoices (id INT);
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "schema.sql");
        let by_name = |n: &str| entities.iter().find(|e| e.name == n);

        // object_reference names (incl. schema-qualified)
        assert_eq!(
            by_name("users").map(|e| e.entity_type.as_str()),
            Some("table")
        );
        assert_eq!(
            by_name("active_users").map(|e| e.entity_type.as_str()),
            Some("view")
        );
        assert_eq!(
            by_name("add").map(|e| e.entity_type.as_str()),
            Some("function")
        );
        assert_eq!(
            by_name("mood").map(|e| e.entity_type.as_str()),
            Some("type")
        );
        assert_eq!(by_name("mv").map(|e| e.entity_type.as_str()), Some("view"));
        assert_eq!(
            by_name("billing.invoices").map(|e| e.entity_type.as_str()),
            Some("table"),
            "schema-qualified table name should be preserved"
        );

        // CREATE INDEX / SCHEMA name a bare identifier, not the ON-table
        assert_eq!(
            by_name("idx_name").map(|e| e.entity_type.as_str()),
            Some("index"),
            "index should be named idx_name, not the table it indexes"
        );
        assert_eq!(
            by_name("myapp").map(|e| e.entity_type.as_str()),
            Some("schema")
        );
    }

    #[test]
    fn test_dart_top_level_function_includes_body() {
        let code = r#"
int add(int a, int b) {
  return a + b;
}

String greet(String name) => 'Hello, $name!';
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "funcs.dart");
        eprintln!(
            "Dart top-level: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type, &e.content))
                .collect::<Vec<_>>()
        );

        let add_fn = entities.iter().find(|e| e.name == "add").unwrap();
        assert!(
            add_fn.content.contains("return a + b"),
            "Top-level function content should include the body, got: {:?}",
            add_fn.content
        );

        let greet_fn = entities.iter().find(|e| e.name == "greet").unwrap();
        assert!(
            greet_fn.content.contains("Hello"),
            "Expression body should be included, got: {:?}",
            greet_fn.content
        );

        // Body changes should produce different content_hash
        let code_v2 = r#"
int add(int a, int b) {
  return a * b;
}

String greet(String name) => 'Hello, $name!';
"#;
        let entities_v2 = plugin.extract_entities(code_v2, "funcs.dart");
        let add_v2 = entities_v2.iter().find(|e| e.name == "add").unwrap();
        assert_ne!(
            add_fn.content_hash, add_v2.content_hash,
            "Body change should produce different content_hash"
        );

        // Unchanged function should keep the same hash
        let greet_v2 = entities_v2.iter().find(|e| e.name == "greet").unwrap();
        assert_eq!(
            greet_fn.content_hash, greet_v2.content_hash,
            "Unchanged function should keep the same content_hash"
        );
    }

    #[test]
    fn test_dart_renamed_named_constructor_same_structural_hash() {
        let code_a = r#"
class Foo {
  Foo.fromJson(Map<String, dynamic> json) {
    print(json);
  }
}
"#;
        let code_b = r#"
class Foo {
  Foo.fromMap(Map<String, dynamic> json) {
    print(json);
  }
}
"#;
        let plugin = CodeParserPlugin;
        let entities_a = plugin.extract_entities(code_a, "a.dart");
        let entities_b = plugin.extract_entities(code_b, "b.dart");

        let ctor_a = entities_a
            .iter()
            .find(|e| e.name == "Foo.fromJson")
            .unwrap();
        let ctor_b = entities_b.iter().find(|e| e.name == "Foo.fromMap").unwrap();

        assert_eq!(
            ctor_a.structural_hash, ctor_b.structural_hash,
            "Renamed named constructor with identical body should have same structural_hash"
        );
        assert_ne!(
            ctor_a.content_hash, ctor_b.content_hash,
            "Content hash should differ since raw content includes the name"
        );
    }

    #[test]
    fn test_dart_top_level_getter_setter() {
        let code = r#"
int _value = 0;

int get currentValue {
  return _value;
}

set currentValue(int v) {
  _value = v;
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "accessors.dart");
        eprintln!(
            "Dart top-level accessors: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type, &e.content))
                .collect::<Vec<_>>()
        );

        let getter = entities
            .iter()
            .find(|e| e.name == "currentValue" && e.entity_type == "getter");
        assert!(
            getter.is_some(),
            "Should find top-level getter, got: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type))
                .collect::<Vec<_>>()
        );
        assert!(
            getter.unwrap().content.contains("return _value"),
            "Top-level getter content should include the body"
        );
        assert!(
            getter.unwrap().parent_id.is_none(),
            "Top-level getter should have no parent"
        );

        // tree-sitter-dart 0.2.0 parses top-level setters as function_signature
        // (treating `set` as a type_identifier). setter_signature is only
        // produced inside class_member → method_signature.
        let setter = entities
            .iter()
            .find(|e| e.name == "currentValue" && e.entity_type == "function");
        assert!(
            setter.is_some(),
            "Should find top-level setter as function, got: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type))
                .collect::<Vec<_>>()
        );
        assert!(
            setter.unwrap().content.contains("_value = v"),
            "Top-level setter content should include the body"
        );
    }

    #[test]
    fn test_dart_field_entity_type() {
        let code = r#"
class Config {
  final String name;
  static const int maxRetries = 3;
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "config.dart");
        eprintln!(
            "Dart fields: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type, &e.parent_id))
                .collect::<Vec<_>>()
        );

        let name_field = entities
            .iter()
            .find(|e| e.name == "name" && e.parent_id.is_some());
        assert!(
            name_field.is_some(),
            "Should find field 'name', got: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type))
                .collect::<Vec<_>>()
        );
        assert_eq!(name_field.unwrap().entity_type, "field");

        let max_retries = entities.iter().find(|e| e.name == "maxRetries");
        assert!(
            max_retries.is_some(),
            "Should find field 'maxRetries', got: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type))
                .collect::<Vec<_>>()
        );
        assert_eq!(max_retries.unwrap().entity_type, "field");
    }

    #[test]
    fn test_dart_identifier_list_fields() {
        // identifier_list produces bare identifier children (no "name" field),
        // unlike initialized_identifier_list which wraps each in an
        // initialized_identifier node with a "name" field.
        let code = r#"
abstract class Shape {
  abstract double x, y;
  abstract String label;
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "shape.dart");
        eprintln!(
            "Dart identifier_list fields: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type, &e.parent_id))
                .collect::<Vec<_>>()
        );

        let x_field = entities.iter().find(|e| e.name == "x");
        assert!(
            x_field.is_some(),
            "Should find field 'x' from identifier_list, got: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type))
                .collect::<Vec<_>>()
        );
        assert_eq!(x_field.unwrap().entity_type, "field");
        assert!(
            x_field.unwrap().parent_id.is_some(),
            "field 'x' should be nested under Shape"
        );

        let label_field = entities.iter().find(|e| e.name == "label");
        assert!(
            label_field.is_some(),
            "Should find field 'label' from single-element identifier_list, got: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type))
                .collect::<Vec<_>>()
        );
        assert_eq!(label_field.unwrap().entity_type, "field");
    }

    #[test]
    fn test_ocaml_entity_extraction() {
        let code = r#"
type color = Red | Green | Blue

type point = {
  x : float;
  y : float;
}

exception Not_found of string

let greet name =
  Printf.printf "Hello, %s!\n" name

let add a b = a + b

let version = "1.0"

let color_to_string = function
  | Red -> "red"
  | Blue -> "blue"

let inc = fun x -> x + 1

module MyModule = struct
  let helper x = x * 2
end

module type Printable = sig
  val to_string : 'a -> string
end

external caml_input : in_channel -> bytes -> int -> int -> int = "caml_input"

class point_class x_init = object
  val mutable x = x_init
  method get_x = x
end

class type measurable = object
  method measure : float
end
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "example.ml");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!(
            "OCaml entities: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type))
                .collect::<Vec<_>>()
        );

        let find = |name: &str| {
            entities
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("Should find {}, got: {:?}", name, names))
        };

        assert_eq!(find("color").entity_type, "type");
        assert_eq!(find("point").entity_type, "type");
        assert_eq!(find("Not_found").entity_type, "exception");
        assert_eq!(find("greet").entity_type, "function");
        assert_eq!(find("add").entity_type, "function");
        assert_eq!(find("version").entity_type, "value");
        assert_eq!(find("color_to_string").entity_type, "function");
        assert_eq!(find("inc").entity_type, "function");
        assert_eq!(find("MyModule").entity_type, "module");
        assert_eq!(find("Printable").entity_type, "module_type");
        assert_eq!(find("caml_input").entity_type, "external");
        assert_eq!(find("point_class").entity_type, "class");
        assert_eq!(find("measurable").entity_type, "class_type");
    }

    #[test]
    fn test_ocaml_nested_module_entities() {
        let code = r#"
module Outer = struct
  let x = 42

  module Inner = struct
    let y = 0
  end
end
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "nested.ml");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!(
            "OCaml nested: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type, &e.parent_id))
                .collect::<Vec<_>>()
        );

        let find = |name: &str| {
            entities
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("Should find {}, got: {:?}", name, names))
        };

        let outer = find("Outer");
        let x = find("x");
        let inner = find("Inner");
        let y = find("y");

        assert_eq!(outer.entity_type, "module");
        assert_eq!(x.entity_type, "value");
        assert_eq!(inner.entity_type, "module");
        assert_eq!(y.entity_type, "value");

        assert!(
            x.parent_id.as_ref().is_some_and(|p| p == &outer.id),
            "x should be nested under Outer"
        );
        assert!(
            inner.parent_id.as_ref().is_some_and(|p| p == &outer.id),
            "Inner should be nested under Outer"
        );
        assert!(
            y.parent_id.as_ref().is_some_and(|p| p == &inner.id),
            "y should be nested under Inner"
        );
    }

    #[test]
    fn test_ocaml_interface_entity_extraction() {
        let code = r#"
type t

val create : string -> t
val to_string : t -> string

exception Invalid_input of string

module type Serializable = sig
  val serialize : t -> string
end
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "example.mli");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!(
            "OCaml interface entities: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type))
                .collect::<Vec<_>>()
        );

        let find = |name: &str| {
            entities
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("Should find {}, got: {:?}", name, names))
        };

        assert_eq!(find("t").entity_type, "type");
        assert_eq!(find("create").entity_type, "val");
        assert_eq!(find("to_string").entity_type, "val");
        assert_eq!(find("Invalid_input").entity_type, "exception");
        assert_eq!(find("Serializable").entity_type, "module_type");
    }

    #[test]
    fn test_ocaml_mutual_recursion_let() {
        let code = r#"
let rec even n = (n = 0) || odd (n - 1)
and odd n = (n <> 0) && even (n - 1)

let rec ping x = pong (x - 1)
and pong x = if x <= 0 then 0 else ping (x - 1)
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "mutual.ml");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!(
            "OCaml mutual let: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type))
                .collect::<Vec<_>>()
        );

        let find = |name: &str| {
            entities
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("Should find {}, got: {:?}", name, names))
        };

        assert_eq!(find("even").entity_type, "function");
        assert_eq!(find("odd").entity_type, "function");
        assert_eq!(find("ping").entity_type, "function");
        assert_eq!(find("pong").entity_type, "function");
    }

    #[test]
    fn test_ocaml_mutual_recursion_module() {
        let code = r#"
module rec A : sig val x : int end = struct
  let x = B.y + 1
end
and B : sig val y : int end = struct
  let y = 0
end
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "mutual_mod.ml");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!(
            "OCaml mutual module: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type, &e.parent_id))
                .collect::<Vec<_>>()
        );

        let find = |name: &str| {
            entities
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("Should find {}, got: {:?}", name, names))
        };

        let a = find("A");
        let b = find("B");
        assert_eq!(a.entity_type, "module");
        assert_eq!(b.entity_type, "module");

        let x = find("x");
        let y = find("y");
        assert!(
            x.parent_id.as_ref().is_some_and(|p| p == &a.id),
            "x should be nested under A"
        );
        assert!(
            y.parent_id.as_ref().is_some_and(|p| p == &b.id),
            "y should be nested under B"
        );
    }

    #[test]
    fn test_ocaml_destructured_let() {
        let code = r#"
let (a, b) = (1, 2)

let { x; y } = point

let simple = 42
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "destruct.ml");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!(
            "OCaml destructured: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type))
                .collect::<Vec<_>>()
        );

        let find = |name: &str| {
            entities
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("Should find {}, got: {:?}", name, names))
        };

        assert_eq!(find("a").entity_type, "value");
        assert_eq!(find("b").entity_type, "value");
        assert_eq!(find("x").entity_type, "value");
        assert_eq!(find("y").entity_type, "value");
        assert_eq!(find("simple").entity_type, "value");
    }

    #[test]
    fn test_ocaml_mutual_recursion_class() {
        let code = r#"
class foo = object
  method x = 1
end
and bar = object
  method y = 2
end
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "classes.ml");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!(
            "OCaml mutual class: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type))
                .collect::<Vec<_>>()
        );

        let find = |name: &str| {
            entities
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("Should find {}, got: {:?}", name, names))
        };

        assert_eq!(find("foo").entity_type, "class");
        assert_eq!(find("bar").entity_type, "class");
    }

    #[test]
    fn test_perl_entity_extraction() {
        let code = r#"package Foo::Bar;

use strict;
use warnings;

sub hello {
    my ($self, $name) = @_;
    print "Hello, $name!\n";
}

sub _private_helper {
    return 42;
}

1;
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "Foo/Bar.pm");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"Foo::Bar"), "got: {:?}", names);
        assert!(names.contains(&"hello"), "got: {:?}", names);
        assert!(names.contains(&"_private_helper"), "got: {:?}", names);

        let find = |name: &str| {
            entities
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("Should find {}, got: {:?}", name, names))
        };

        assert_eq!(find("Foo::Bar").entity_type, "package");
        assert_eq!(find("hello").entity_type, "function");
        assert_eq!(find("_private_helper").entity_type, "function");
    }

    #[test]
    fn test_fortran_entity_extraction() {
        let code = r#"module math_utils
  implicit none
contains
  function add(a, b) result(c)
    integer, intent(in) :: a, b
    integer :: c
    c = a + b
  end function add

  subroutine greet()
    print *, "hello"
  end subroutine greet
end module math_utils

program main
  implicit none
  print *, "hello"
end program main
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "test.f90");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"math_utils"), "got: {:?}", names);
        assert!(names.contains(&"add"), "got: {:?}", names);
        assert!(names.contains(&"greet"), "got: {:?}", names);
        assert!(names.contains(&"main"), "got: {:?}", names);

        let find = |name: &str| {
            entities
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("Should find {}, got: {:?}", name, names))
        };

        assert_eq!(find("math_utils").entity_type, "module");
        assert_eq!(find("add").entity_type, "function");
        assert_eq!(find("greet").entity_type, "subroutine");
        assert_eq!(find("main").entity_type, "program");

        // Nested entities have parent
        assert!(find("add").parent_id.is_some());
        assert!(find("greet").parent_id.is_some());
    }

    #[test]
    fn test_scala_entity_extraction() {
        let code = r#"
package com.example

import scala.collection.mutable

class UserService(val name: String) {
  def getUsers(): List[User] = db.findAll()

  def createUser(user: User): Unit = db.save(user)

  private def validate(user: User): Boolean = true
}

object UserService {
  def apply(name: String): UserService = new UserService(name)

  val DefaultName: String = "default"
}

trait Repository[T] {
  def findById(id: String): Option[T]
  def findAll(): List[T]
}

case class User(id: String, name: String)

type UserId = String
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "UserService.scala");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!(
            "Scala entities: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type))
                .collect::<Vec<_>>()
        );

        assert!(
            names.contains(&"UserService"),
            "Should find class UserService, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Repository"),
            "Should find trait Repository, got: {:?}",
            names
        );
        assert!(
            names.contains(&"getUsers"),
            "Should find method getUsers, got: {:?}",
            names
        );
        assert!(
            names.contains(&"createUser"),
            "Should find method createUser, got: {:?}",
            names
        );

        // Methods should be nested under class
        let get_users = entities.iter().find(|e| e.name == "getUsers").unwrap();
        assert!(
            get_users.parent_id.is_some(),
            "getUsers should have parent_id"
        );
    }

    #[test]
    fn test_scala3_entity_extraction() {
        let code = r#"
package com.example

enum Color:
  case Red, Green, Blue

enum Planet(mass: Double, radius: Double):
  case Mercury extends Planet(3.303e+23, 2.4397e6)
  case Venus   extends Planet(4.869e+24, 6.0518e6)

object Main:
  def main(args: Array[String]): Unit =
    println("Hello, World!")

trait Greeter:
  def greet(name: String): String

given Greeter with
  def greet(name: String): String = s"Hello, $name!"

extension (s: String)
  def shout: String = s.toUpperCase + "!"

type Predicate[A] = A => Boolean
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "Main.scala");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        eprintln!(
            "Scala 3 entities: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.entity_type))
                .collect::<Vec<_>>()
        );

        assert!(
            names.contains(&"Color"),
            "Should find enum Color, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Planet"),
            "Should find enum Planet, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Main"),
            "Should find object Main, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Greeter"),
            "Should find trait Greeter, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Predicate"),
            "Should find type alias Predicate, got: {:?}",
            names
        );
    }

    #[test]
    fn test_zig_entity_extraction() {
        let code = r#"
const std = @import("std");

pub const Point = struct {
    x: i32,
    y: i32,
};

pub const Color = enum {
    red,
    green,
    blue,
};

const Person = struct {
    name: []const u8,
    age: u32,
};

pub fn greet(name: []const u8) void {
    std.debug.print("Hello, {s}!\n", .{name});
}

fn add(a: i32, b: i32) i32 {
    return a + b;
}

pub fn main() !void {
    greet("world");
}

test "basic addition" {
    const result = add(2, 3);
    _ = result;
}
"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "main.zig");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        let types: std::collections::HashMap<&str, &str> = entities
            .iter()
            .map(|e| (e.name.as_str(), e.entity_type.as_str()))
            .collect();

        assert!(
            names.contains(&"greet"),
            "Should find greet, got: {:?}",
            names
        );
        assert!(names.contains(&"add"), "Should find add, got: {:?}", names);
        assert!(
            names.contains(&"main"),
            "Should find main, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Point"),
            "Should find Point, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Color"),
            "Should find Color, got: {:?}",
            names
        );
        assert!(
            names.contains(&"Person"),
            "Should find Person, got: {:?}",
            names
        );

        assert_eq!(types["greet"], "function");
        assert_eq!(types["add"], "function");
        assert_eq!(types["Point"], "struct");
        assert_eq!(types["Color"], "enum");
        assert_eq!(types["Person"], "struct");
    }

    #[test]
    #[cfg(feature = "lang-edn")]
    fn test_edn_deps_edn_map_entries() {
        let code = r#"{:deps {org.clojure/clojure {:mvn/version "1.11.0"}}
 :paths ["src" "resources"]
 :aliases {:dev {:extra-deps {cider/cider-nrepl {:mvn/version "0.28.5"}}}}}"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "deps.edn");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        let types: std::collections::HashMap<&str, &str> = entities
            .iter()
            .map(|e| (e.name.as_str(), e.entity_type.as_str()))
            .collect();

        assert!(
            names.contains(&":deps"),
            "Should find :deps, got: {:?}",
            names
        );
        assert!(
            names.contains(&":paths"),
            "Should find :paths, got: {:?}",
            names
        );
        assert!(
            names.contains(&":aliases"),
            "Should find :aliases, got: {:?}",
            names
        );
        assert_eq!(
            names.len(),
            3,
            "Should have exactly 3 entries, got: {:?}",
            names
        );
        assert_eq!(types[":deps"], "entry");
        assert_eq!(types[":paths"], "entry");
        assert_eq!(types[":aliases"], "entry");
    }

    #[test]
    #[cfg(feature = "lang-edn")]
    fn test_edn_nested_map_values_not_extracted() {
        // Inner map entries (inside :aliases) must not leak as top-level entities.
        let code = r#"{:a {:b 1 :c 2} :d 3}"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "config.edn");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&":a"), "Should find :a, got: {:?}", names);
        assert!(names.contains(&":d"), "Should find :d, got: {:?}", names);
        assert!(!names.contains(&":b"), "Inner :b should not be extracted");
        assert!(!names.contains(&":c"), "Inner :c should not be extracted");
        assert_eq!(names.len(), 2);
    }

    #[test]
    #[cfg(feature = "lang-edn")]
    fn test_edn_non_map_top_level_forms_not_extracted() {
        // A bare vector at the top level has no meaningful name and yields no entities.
        let code = r#"["alpha" "beta"]"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "data.edn");
        assert_eq!(entities.len(), 0);
    }

    #[test]
    #[cfg(feature = "lang-edn")]
    fn test_edn_symbol_keys_extracted() {
        let code = r#"{foo 1 bar 2}"#;
        let plugin = CodeParserPlugin;
        let entities = plugin.extract_entities(code, "sym.edn");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"foo"), "Should find foo, got: {:?}", names);
        assert!(names.contains(&"bar"), "Should find bar, got: {:?}", names);
    }
}
