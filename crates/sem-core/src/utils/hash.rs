use std::hash::Hasher;
use tree_sitter::Node;
use xxhash_rust::xxh3::Xxh3;

pub fn content_hash(content: &str) -> String {
    content_hash_bytes(content.as_bytes())
}

pub fn content_hash_bytes(content: &[u8]) -> String {
    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(content))
}

pub fn short_hash(content: &str, length: usize) -> String {
    let hash = content_hash(content);
    hash[..length.min(hash.len())].to_string()
}

/// Compute a structural hash from a tree-sitter AST node.
/// Strips comments and normalizes whitespace so formatting-only changes
/// produce the same hash. Uses streaming xxHash64 to avoid intermediate
/// string allocations.
pub fn structural_hash(node: Node, source: &[u8]) -> String {
    let mut hasher = Xxh3::new();
    hash_structural_tokens(node, source, &mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Compute a structural hash that excludes tokens within a given byte range.
/// Used to strip the entity name from the hash so that renames of otherwise
/// identical entities produce the same hash, enabling Phase 2 rename detection.
pub fn structural_hash_excluding_range(
    node: Node,
    source: &[u8],
    exclude_start: usize,
    exclude_end: usize,
) -> String {
    let mut hasher = Xxh3::new();
    hash_structural_tokens_excluding(node, source, &mut hasher, exclude_start, exclude_end);
    format!("{:016x}", hasher.finish())
}

/// Iteratively hash tokens from the AST, skipping comments.
/// Hashes both node types (structure) and leaf text (content) so that
/// structurally different ASTs with identical leaf tokens produce different hashes.
/// Zero allocations: hashes directly from source byte slices.
fn hash_structural_tokens(root: Node, source: &[u8], hasher: &mut Xxh3) {
    let mut worklist = vec![root];
    // One cursor reused for the whole traversal instead of `node.walk()` per
    // internal node, and children are pushed in place instead of collected into
    // a fresh Vec per node — the two per-node allocations the old code paid.
    let mut cursor = root.walk();
    while let Some(node) = worklist.pop() {
        let kind = node.kind();

        if is_comment_node(kind) {
            continue;
        }

        if node.child_count() == 0 {
            // Leaf node: hash its text directly from the source buffer
            let start = node.start_byte();
            let end = node.end_byte();
            if start < end && end <= source.len() {
                let bytes = &source[start..end];
                // Trim whitespace manually to avoid allocation
                let trimmed = trim_bytes(bytes);
                if !trimmed.is_empty() {
                    hasher.write(trimmed);
                    hasher.write(b" ");
                }
            }
        } else {
            // Hash the node type to capture structure, not just leaf content.
            // e.g. `x = foo(bar)` vs `foo(bar) = x` have same leaves but different structure.
            hasher.write(kind.as_bytes());
            hasher.write(b":");
            // Push children in source order, then reverse just the appended
            // slice so `pop()` yields them in source order — identical traversal
            // (and identical hash) to the previous `children.rev()` push.
            push_children_reversed(&mut cursor, node, &mut worklist);
        }
    }
}

/// Like `hash_structural_tokens` but skips any leaf node whose byte range
/// overlaps the excluded range (the entity name).
fn hash_structural_tokens_excluding(
    root: Node,
    source: &[u8],
    hasher: &mut Xxh3,
    exclude_start: usize,
    exclude_end: usize,
) {
    let mut worklist = vec![root];
    let mut cursor = root.walk();
    while let Some(node) = worklist.pop() {
        let kind = node.kind();

        if is_comment_node(kind) {
            continue;
        }

        if node.child_count() == 0 {
            let start = node.start_byte();
            let end = node.end_byte();
            // Skip leaf nodes that overlap the excluded range
            if start < exclude_end && end > exclude_start {
                continue;
            }
            if start < end && end <= source.len() {
                let bytes = &source[start..end];
                let trimmed = trim_bytes(bytes);
                if !trimmed.is_empty() {
                    hasher.write(trimmed);
                    hasher.write(b" ");
                }
            }
        } else {
            hasher.write(kind.as_bytes());
            hasher.write(b":");
            push_children_reversed(&mut cursor, node, &mut worklist);
        }
    }
}

/// Push `node`'s children onto `worklist` such that a subsequent `pop()`
/// sequence yields them in source (first-to-last) order, without allocating a
/// per-node Vec. Children are appended in source order, then the appended tail
/// is reversed in place. Equivalent to the previous
/// `node.children(&mut cursor).collect::<Vec<_>>().into_iter().rev()`.
#[inline]
fn push_children_reversed<'a>(
    cursor: &mut tree_sitter::TreeCursor<'a>,
    node: Node<'a>,
    worklist: &mut Vec<Node<'a>>,
) {
    let base = worklist.len();
    cursor.reset(node);
    if cursor.goto_first_child() {
        loop {
            worklist.push(cursor.node());
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    worklist[base..].reverse();
}

/// Sibling of [`push_children_reversed`] for [`structural_and_semantic_hash`]'s
/// worklist, which additionally carries each pushed child's parent (always
/// `node`, since these are exactly `node`'s children) so `is_semantic_leaf`
/// never needs to call the expensive whole-tree-anchored `Node::parent()`.
/// Same traversal order and zero-extra-allocation shape as
/// `push_children_reversed`; see that function's doc comment.
#[inline]
fn push_children_reversed_with_parent<'a>(
    cursor: &mut tree_sitter::TreeCursor<'a>,
    node: Node<'a>,
    worklist: &mut Vec<(Node<'a>, Option<Node<'a>>)>,
) {
    let base = worklist.len();
    cursor.reset(node);
    if cursor.goto_first_child() {
        loop {
            worklist.push((cursor.node(), Some(node)));
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    worklist[base..].reverse();
}

/// Trim leading/trailing ASCII whitespace from a byte slice without allocating.
#[inline]
fn trim_bytes(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |p| p + 1);
    &bytes[start..end]
}

fn is_comment_node(kind: &str) -> bool {
    matches!(
        kind,
        "comment" | "line_comment" | "block_comment" | "doc_comment" | "tag_comment"
    )
}

// ---------------------------------------------------------------------------
// kappa: a second, semantic identity hash (spike; see crates/sem-core)
// ---------------------------------------------------------------------------

/// Compute `structural_hash` and kappa (the semantic hash) together in one
/// tree walk, so kappa doesn't cost a second traversal over the same subtree.
///
/// `exclude_range`, when given, is the byte range of the entity's name token
/// (mirrors `structural_hash_excluding_range`'s use for rename detection).
/// It is honored on the **structural** side only, exactly matching today's
/// `compute_structural_hash` behavior — that half of this function must stay
/// byte-for-byte identical to calling `structural_hash`/
/// `structural_hash_excluding_range` directly; `tests/kappa.rs` and
/// `hash::tests` pin that equivalence.
///
/// Kappa never excludes the name range: a rename is a semantic change and
/// kappa is deliberately rename-*sensitive* (unlike `structural_hash`,
/// which is rename-invariant by design so `differ.rs` can detect renames).
/// The full inclusion/exclusion spec.
pub fn structural_and_semantic_hash(
    node: Node,
    source: &[u8],
    exclude_range: Option<(usize, usize)>,
) -> (String, String) {
    let mut struct_hasher = Xxh3::new();
    let mut kappa_hasher = Xxh3::new();
    // worklist entries carry the node's parent (`None` only for
    // `node` itself) instead of leaving `is_semantic_leaf` to call
    // `Node::parent()` on demand. `Node::parent()` is NOT O(1): tree-sitter's
    // `ts_node_parent` (src/node.c) starts at `ts_tree_root_node` and walks
    // *down* from the whole tree's root looking for the child whose subtree
    // contains the target node -- i.e. it costs O(depth of the node from the
    // tree root), not O(depth from this function's own `node` argument. This
    // traversal already knows every node's parent for free (it's the node
    // whose children were just pushed), so threading it through avoids the
    // call entirely.
    //
    // This is a genuine algorithmic fix, not a micro-optimization: entities
    // are extracted once per nesting level, each hashing its own full
    // subtree, so a corpus with deeply nested declarations pays this walk
    // once per (entity level x keyword-leaf x that leaf's absolute depth) --
    // cubic in nesting depth. Confirmed via a synthetic C# fixture with N
    // levels of nested struct declarations (dotnet-runtime ships a real one
    // at N=500, `NestedGenericStructs.cs`): extraction time scaled ~N^2.9-3.0
    // (13ms at N=50, 2.09s at N=300, 9.5s at N=500), matching this
    // mechanism's predicted O(depth^3) exactly, and per-phase timing showed
    // 9.48s of that 9.5s inside this function specifically. See
    // `crates/sem-core/"C# pathology" section.
    let mut worklist: Vec<(Node, Option<Node>)> = vec![(node, None)];
    let mut cursor = node.walk();
    while let Some((n, known_parent)) = worklist.pop() {
        let kind = n.kind();

        if is_comment_node(kind) {
            continue;
        }

        if n.child_count() == 0 {
            let start = n.start_byte();
            let end = n.end_byte();
            if start < end && end <= source.len() {
                let bytes = &source[start..end];
                let trimmed = trim_bytes(bytes);
                if !trimmed.is_empty() {
                    let excluded = exclude_range.is_some_and(|(es, ee)| start < ee && end > es);
                    if !excluded {
                        struct_hasher.write(trimmed);
                        struct_hasher.write(b" ");
                    }
                    if is_semantic_leaf(n, trimmed, source, known_parent) {
                        kappa_hasher.write(trimmed);
                        kappa_hasher.write(b" ");
                    }
                }
            }
        } else {
            struct_hasher.write(kind.as_bytes());
            struct_hasher.write(b":");
            // Kappa only hashes *named* node kinds: tree-sitter's anonymous
            // internal-node wrappers (rare, grammar-specific) don't correspond
            // to anything a typed AST would expose as a node kind.
            if n.is_named() {
                kappa_hasher.write(kind.as_bytes());
                kappa_hasher.write(b":");
            }
            push_children_reversed_with_parent(&mut cursor, n, &mut worklist);
        }
    }
    (
        format!("{:016x}", struct_hasher.finish()),
        format!("{:016x}", kappa_hasher.finish()),
    )
}

/// The classic CST punctuation/delimiter set kappa drops even though these
/// are anonymous leaves. Braces/parens/brackets/semicolons/commas are pure
/// grouping and statement-termination syntax with no representation in a
/// typed AST (oxc, babel, swc, ...) — a `Function` node has no "had a
/// trailing comma" field. This is the exact set the task spec calls out.
const KAPPA_PUNCTUATION: &[&[u8]] = &[b"{", b"}", b"(", b")", b"[", b"]", b";", b","];

/// v1.1: named parent node kinds where the grammar mixes one or more
/// discriminator keywords directly in among other, non-keyword children (so
/// the "pure keyword bag" wrapper test below can never fire — the parent
/// has non-keyword payload children too, e.g. a name or parameter list) and
/// each such keyword is part of what says which of several mutually-
/// exclusive declaration/definition kinds this is, because the grammar
/// assigns *all* of the alternatives the same named node kind.
///
/// Every anonymous keyword-shaped child of a table-listed parent kind is
/// included, at *any* position, not just a leading one: v1.1 first modeled
/// this as "only the first child counts" (true for `let`/`const`, which
/// never have a second keyword sibling), but `method_definition`'s
/// modifiers can stack (`static async foo() {}` has both `static` and
/// `async` as anonymous children before the name) — a position check would
/// silently miss the second one. Position-independence is safe here
/// specifically *because* the table is curated per node kind: every
/// anonymous keyword-shaped child ever produced under these particular
/// kinds is a real modifier/discriminator, by construction of the grammar
/// rule that produces them — there's no "innocent" bare keyword sharing the
/// slot that inclusion could spuriously latch onto.
///
/// Found by sweeping every "sibling" language for the same failure shape as
/// `let`/`const` (v1.1), then a second pass triggered by real
/// collisions `kappa_stats` (collision analysis) surfaced on the
/// TypeScript-monster corpus:
///
/// - `lexical_declaration` (TS/JS): `choice('let', 'const')` followed by
///   `variable_declarator`(s) — `let x = 1` and `const x = 1` are both
///   `lexical_declaration` nodes; only the keyword differs.
/// - `property_declaration` (Kotlin): `choice('val', 'var')` followed by a
///   `variable_declaration` (the *name*, confusingly — not to be conflated
///   with TS/JS's `variable_declaration`) — same shape, same reason.
/// - `method_definition` (TS/JS): a plain method, a getter (`get foo()`), a
///   setter (`set foo(v)`), a static method (`static foo()`), and an async
///   method (`async foo()`) are all `method_definition` nodes whose
///   `get`/`set`/`static`/`async` keywords are anonymous non-sole children
///   — found via `kappa_stats` on the TypeScript-monster corpus merging
/// `foo {}` and `get foo {}` under one kappa (v1.1's
///   collision analysis).
/// - `public_field_definition` (TS): `readonly x = 1`/`abstract x: T`/
///   `x = 1` are all `public_field_definition` nodes — same shape as the
///   `method_definition` case, found the same way.
/// - `method_signature` (TS): the interface/ambient-context sibling of
///   `method_definition` — `get foo(): T;` vs `foo(): T;` inside an
///   `interface`, same shape.
/// - `property_signature` (TS): `public_field_definition`'s interface/
///   object-type-literal sibling — `readonly length: number;` vs `length:
///   number;` inside an `interface` or a `type` object-type literal —
///   found on a second look at the same TS-monster `kappa_stats` sample
///   that caught `public_field_definition` (a 2152-entity group merging
///   `length: number` and `readonly length: number`, still present after
///   the `public_field_definition` fix alone).
/// - `function_declaration`, `function_expression`, `generator_function`,
///   `generator_function_declaration`, `arrow_function` (TS/JS): each has
///   an `async` keyword as an anonymous non-sole child when present
///   (`async function f() {}` vs `function f() {}`, `async () => {}` vs
///   `() => {}`, `async function* f() {}` vs `function* f() {}`, ...) — the
///   `kappa_stats` sample that caught `method_definition` above also
///   caught `async function fn1() {}` merged with plain `function fn1()
///   {}` under one kappa on the TS-monster corpus, which is what triggered
///   checking every other "keyword modifies a callable" node kind in the
///   grammar, not just the one the first sample happened to show.
/// - `export_statement` (TS/JS): `export default Foo;`, `export as
///   namespace Foo;`, and `export = foo;` are all `export_statement`
///   nodes whose `default`/`as`+`namespace`/`=` tokens are anonymous
///   non-sole children (`=` is a symbolic anonymous leaf already included
///   by rule 4 regardless, but `default`/`as`/`namespace` are keyword-
///   shaped) — found the same way, a `kappa_stats` sample merging `export
///   as namespace Foo;` and `export default Foo;` under one kappa.
/// - `function_definition` (Python): the exact same `async` shape as TS/JS
///   above, in a completely different grammar — `async def f():` and `def
///   f():` are both `function_definition` nodes. This one wasn't found on
///   the TS-monster corpus at all: it's the fix for a real false merge
///   `kappa_stats` surfaced on **django** (a Python corpus) --
///   `def __call__(self, **kwargs): ...` and `async def __call__(self,
///   **kwargs): ...` (two behaviorally different methods) sharing one
///   kappa in `tests/signals/tests.py`. The original per-language sweep
/// (v1.1) checked Python's `global`/`nonlocal` and missed
///   `async def` entirely; running the corpus analysis against a Python
///   repo, not just TypeScript ones, is what caught it.
///
/// This entire cluster (every table entry after `property_declaration`)
/// was found by the SAME "kappa_stats on a real corpus, sample collision
/// groups, eyeball, check the parse tree" loop, applied repeatedly across
/// languages until the sample stopped turning up new node kinds — not from
/// a first-principles enumeration of any one grammar. `export { Foo }`
/// (`export_clause`), `import ... from '...'` (imports aren't a
/// kappa-bearing entity kind in this codebase's extractor —),
/// and `abstract class C {}` (its own node kind, `abstract_class_
/// declaration`, distinct from `class_declaration`) were checked and don't
/// need this treatment.
///
/// Everything else swept turned out NOT to need this table, because the
/// grammar already resolves the ambiguity another way — see v1.1
/// ("Sibling-gap sweep") for the full per-candidate verdict table:
/// JS/TS `var` gets its own node kind (`variable_declaration`, distinct
/// from `lexical_declaration`); a generator's `*` is a symbolic (non-
/// keyword-shaped) anonymous leaf so rule 4 already includes it regardless
/// of position; Go `:=`/`var`/`const` each get their own node kind; Python
/// `global`/`nonlocal` each get their own node kind; Rust `mut` is a
/// *named* leaf (`mutable_specifier`) so rule 1 already includes it; Swift
/// `let`/`var`, `override`, and C#/PHP's per-modifier keywords (`readonly`,
/// `public`, ...) are each the sole child of their own small named wrapper
/// node, so the pre-existing "sole child" rule (generalized below) already
/// includes them.
const KAPPA_LEADING_KEYWORD_DISCRIMINATOR_PARENTS: &[&str] = &[
    "lexical_declaration",
    "property_declaration",
    "method_definition",
    "public_field_definition",
    "method_signature",
    "property_signature",
    "function_declaration",
    "function_expression",
    "generator_function",
    "generator_function_declaration",
    "arrow_function",
    "export_statement",
    "function_definition",
];

/// True when `node` is an anonymous discriminator keyword of a parent
/// listed in `KAPPA_LEADING_KEYWORD_DISCRIMINATOR_PARENTS`.
///
/// `known_parent` is `node`'s parent, already in hand from the caller's
/// traversal -- see `structural_and_semantic_hash`'s doc comment for why this
/// must not fall back to calling `Node::parent()` itself.
fn is_leading_keyword_discriminator(known_parent: Option<Node>) -> bool {
    known_parent.is_some_and(|parent| {
        parent.is_named() && KAPPA_LEADING_KEYWORD_DISCRIMINATOR_PARENTS.contains(&parent.kind())
    })
}

/// True when every child of `parent` is itself an anonymous keyword-shaped
/// leaf (ASCII alnum/underscore text, no children of its own). This is the
/// general form of "parent is a tree-sitter bare-words wrapper" — the
/// keyword(s) *are* the node's entire semantic payload, because there is
/// nothing else under it the node-kind hash could be standing in for.
///
/// Subsumes the original v1 "sole child of a named parent" rule (a lone
/// keyword child trivially satisfies "all children are keyword-shaped"),
/// and additionally covers grammars that group *several* alternative
/// keywords as flat siblings under one wrapper kind instead of giving each
/// one its own single-child wrapper — e.g. tree-sitter-java's `modifiers`
/// node, whose children are a flat `repeat1(choice('public', 'private',
/// 'final', 'static', ...))`: `public final` and `private final` both
/// produce a `modifiers` node with two anonymous keyword children, so
/// neither keyword is a "sole child" and v1's rule silently dropped both,
/// collapsing `public final int x` and `private final int x` to the same
/// kappa. (tree-sitter-c-sharp's `modifier`/tree-sitter-php's
/// `visibility_modifier`+`readonly_modifier`/tree-sitter-swift's
/// `value_binding_pattern` instead give each keyword its own single-child
/// wrapper, so they never needed this generalization — only the "sole
/// child" case, i.e. n=1, which this rule still covers.)
///
/// Known residual gap (documented, not fixed, v1.1): if a
/// grammar mixes a non-keyword sibling into the same wrapper (e.g. Java
/// `modifiers` holding an `@Override` annotation alongside `public`), this
/// predicate returns `false` for the whole node and none of its keywords
/// are included — same as v1's behavior, just not made worse.
fn is_pure_keyword_bag_parent(parent: Node, source: &[u8]) -> bool {
    if !parent.is_named() || parent.child_count() == 0 {
        return false;
    }
    let mut cursor = parent.walk();
    let all_keyword_shaped = parent.children(&mut cursor).all(|child| {
        child.child_count() == 0 && !child.is_named() && is_keyword_shaped_leaf_text(child, source)
    });
    all_keyword_shaped
}

/// Whether `leaf`'s trimmed source text is entirely ASCII alphanumeric/
/// underscore (the "looks like a bare word/keyword" test), re-derived from
/// `source` for a sibling node instead of reusing an already-trimmed slice.
fn is_keyword_shaped_leaf_text(leaf: Node, source: &[u8]) -> bool {
    let start = leaf.start_byte();
    let end = leaf.end_byte();
    if start >= end || end > source.len() {
        return false;
    }
    let trimmed = trim_bytes(&source[start..end]);
    !trimmed.is_empty()
        && trimmed
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Decide whether a leaf token is semantically meaningful enough for kappa.
/// Precise spec (also documented, which a reimplementation
/// against a different parser should follow instead of this tree-sitter-
/// specific code):
///
/// 1. Named leaves (tree-sitter's `is_named()`) are always included --
///    these are identifiers, literals, and similar grammar productions: the
///    tokens a typed AST exposes as node fields (`Identifier.name`,
///    `Literal.value`, ...).
/// 2. Anonymous leaves that are pure punctuation/delimiters
/// (`KAPPA_PUNCTUATION`) are always excluded.
/// 3. Anonymous leaves whose text is entirely ASCII alphanumeric/underscore
///    look like keywords (`function`, `class`, `return`, `if`, ...) and are
///    excluded, UNLESS either:
///    (3a) the leaf is the *leading discriminator* of a
/// `KAPPA_LEADING_KEYWORD_DISCRIMINATOR_PARENTS` parent (v1.1) — e.g.
///    TS/JS `let`/`const` in a `lexical_declaration`; or
///    (3b) the leaf's parent is a "pure keyword bag" (v1.1, generalizing
///    v1's "sole child of a named parent" rule) — every child of the
///    parent is itself an anonymous keyword-shaped leaf, so the keyword(s)
///    are the node's entire semantic payload and the parent's node-kind
///    alone can't tell them apart. Covers both a single bare-word choice
///    (tree-sitter-typescript's `predefined_type: $ => choice('any',
///    'boolean', 'string', ...)`, `accessibility_modifier: $ =>
///    choice('public', 'private', 'protected')`) and a flat list of several
///    (tree-sitter-java's `modifiers: $ => repeat1(choice('public',
///    'private', 'final', 'static', ...))`).
/// 4. Everything else anonymous is a symbolic operator/punctuator that DOES
///    carry meaning (`+`, `==`, `=>`, `...`, `:`, `?`, `.`, ...) and is
///    included.
///
/// `known_parent` is `node`'s parent, already known by the caller's
/// traversal -- see `structural_and_semantic_hash`'s doc comment for why this
/// must not fall back to calling `Node::parent()` itself.
fn is_semantic_leaf(node: Node, trimmed: &[u8], source: &[u8], known_parent: Option<Node>) -> bool {
    if node.is_named() {
        return true;
    }
    if KAPPA_PUNCTUATION.contains(&trimmed) {
        return false;
    }
    let looks_like_keyword = trimmed
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'_');
    if looks_like_keyword {
        if is_leading_keyword_discriminator(known_parent) {
            return true;
        }
        return known_parent.is_some_and(|parent| is_pure_keyword_bag_parent(parent, source));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_hash_deterministic() {
        let h1 = content_hash("hello world");
        let h2 = content_hash("hello world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_content_hash_hex_format() {
        let h = content_hash("test");
        assert_eq!(h.len(), 16); // xxHash64 = 8 bytes = 16 hex chars
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_content_hash_bytes_matches_string_hash() {
        assert_eq!(content_hash_bytes(b"test"), content_hash("test"));
    }

    #[test]
    fn test_short_hash() {
        let h = short_hash("test", 8);
        assert_eq!(h.len(), 8);
    }

    // -----------------------------------------------------------------------
    // v1.1 sibling-gap sweep, at the direct parser + hash level.
    //
    // `CodeParserPlugin` doesn't currently surface Kotlin `property_declaration`
    // nodes as standalone entities at all (a separate, pre-existing gap:
    // `extract_name` in `entity_extractor.rs` only reads a `name` field, and
    // Kotlin's grammar nests the identifier one level deeper, under a
    // `variable_declaration` child, not directly on `property_declaration`),
    // so `tests/kappa.rs`'s Kotlin coverage uses `function_declaration`
    // instead. The `val`/`var` discriminator fix itself is proven here,
    // directly against the real tree-sitter-kotlin-ng parser and the real
    // `structural_and_semantic_hash`, bypassing entity extraction entirely.
    // -----------------------------------------------------------------------

    #[cfg(feature = "lang-kotlin")]
    fn parse_kotlin(source: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .unwrap();
        parser.parse(source, None).unwrap()
    }

    #[cfg(feature = "lang-kotlin")]
    fn kotlin_property_kappa(source: &str) -> String {
        let tree = parse_kotlin(source);
        let root = tree.root_node();
        let property = root
            .named_child(0)
            .filter(|n| n.kind() == "property_declaration")
            .unwrap_or_else(|| panic!("expected a top-level property_declaration in {source:?}"));
        let (_struct_hash, kappa) = structural_and_semantic_hash(property, source.as_bytes(), None);
        kappa
    }

    #[cfg(feature = "lang-kotlin")]
    #[test]
    fn kotlin_val_vs_var_differ() {
        // v1.1 Rule A (leading-keyword discriminator): Kotlin's
        // `property_declaration` puts `val`/`var` directly alongside a
        // sibling `variable_declaration` (the name), the same
        // `choice(kw) + other-children` shape as TS/JS `lexical_declaration`
        // -- found by the sibling-gap sweep, not suggested by the task, and
        // fixed by the same table-driven rule.
        assert_ne!(
            kotlin_property_kappa("val x = 1"),
            kotlin_property_kappa("var x = 1"),
            "v1.1: Kotlin `val x = 1` and `var x = 1` must no longer collide"
        );
    }

    #[cfg(feature = "lang-kotlin")]
    #[test]
    fn kotlin_val_declaration_formatting_invariance_still_holds() {
        assert_eq!(
            kotlin_property_kappa("val x = 1"),
            kotlin_property_kappa("val   x   =   1"),
            "extra whitespace around `val x = 1` must not change kappa"
        );
    }

    #[cfg(feature = "lang-csharp")]
    fn csharp_class_kappa(source: &str) -> String {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        let root = tree.root_node();
        let class = root
            .named_child(0)
            .filter(|n| n.kind() == "class_declaration")
            .unwrap_or_else(|| panic!("expected a top-level class_declaration in {source:?}"));
        let (_struct_hash, kappa) = structural_and_semantic_hash(class, source.as_bytes(), None);
        kappa
    }

    #[cfg(feature = "lang-csharp")]
    #[test]
    fn csharp_public_vs_private_readonly_already_differ() {
        // Sibling-gap sweep, "no fix needed": tree-sitter-c-sharp gives each
        // modifier its own single-child `modifier` wrapper node (unlike
        // Java's flat `modifiers` list), so the pre-existing "sole child of
        // a named parent" rule already included `public`/`private`/
        // `readonly` individually -- confirmed unaffected by v1.1.
        assert_ne!(
            csharp_class_kappa("class C { public readonly int x; }"),
            csharp_class_kappa("class C { private readonly int x; }"),
            "C# `public readonly` vs `private readonly` must differ \
             (already true pre-v1.1)"
        );
    }
}
