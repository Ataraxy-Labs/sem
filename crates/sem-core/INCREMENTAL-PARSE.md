# Incremental reparse: measured, and not taken

## The question

A merge compares a base against two sides that each differ from it by a small
number of edit regions. That is the exact shape tree-sitter's incremental
parser exists for: hand it the old `Tree`, apply the `InputEdit`s, and it
reuses every subtree the edit did not touch.

So: should sem-core thread old trees through its API so weave's merges get
incremental reparses?

## The measurement

`benches/incremental.rs` builds a base tree, derives the single `InputEdit`
that turns base into the edited side (common prefix/suffix trimmed), and
compares two ways of getting to the new entities. It asserts the incremental
parse produces an S-expression identical to a full parse first, so the numbers
are comparing like with like.

Apple silicon, `--release`, criterion medians, `--warm-up-time 1
--measurement-time 3 --sample-size 30`. The edit is one region of a few bytes,
0.001-0.07% of the file — the friendliest case incremental parsing will ever
see.

| fixture | full parse | incr. parse | speedup | full parse+walk | incr. parse+walk | speedup |
|---|---:|---:|---:|---:|---:|---:|
| python-small | 86.2 µs | 7.60 µs | 11x | 147 µs | 68.7 µs | 2.15x |
| python-medium | 872 µs | 12.0 µs | 73x | 1.49 ms | 632 µs | 2.36x |
| python-large | 6.87 ms | 44.2 µs | 155x | 11.80 ms | 4.97 ms | 2.37x |
| typescript-small | 77.1 µs | 5.33 µs | 14x | 174 µs | 87.2 µs | 1.99x |
| typescript-medium | 825 µs | 9.84 µs | 84x | 1.68 ms | 846 µs | 1.99x |
| typescript-large | 6.41 ms | 42.1 µs | 152x | 13.13 ms | 6.74 ms | 1.95x |
| rust-small | 70.4 µs | 5.12 µs | 14x | 137 µs | 70.2 µs | 1.95x |
| rust-medium | 648 µs | 12.6 µs | 51x | 1.29 ms | 654 µs | 1.97x |
| rust-large | 5.17 ms | 74.9 µs | 69x | 10.38 ms | 5.27 ms | 1.97x |

Read the two speedup columns together. The parse gets 11-155x faster. The thing
callers actually ask for gets **1.95-2.37x** faster, in every language, at every
size — because the entity walk is untouched and it is roughly half the work.

## The verdict: no

Incremental parsing is a real and large win **on the parse phase** — up to 155x
on these fixtures. It is a ~2x win **on the thing sem-core is actually asked
for**, which is `Vec<SemanticEntity>`:

* `extract_entities` parses *and then walks the whole tree*. The walk is
  roughly half the total cost (see `split/parse` vs `split/walk` in
  `parse_profile`) and it does not get cheaper just because the parse did.
  Amdahl caps the end-to-end gain at 2x, and the measurements sit right on that
  cap — 1.95-2.37x — regardless of language or file size.
* The content-addressed cache gets 118-241x on the case that actually recurs,
  and the two do not compose: a cache hit skips the walk too, which incremental
  parsing cannot.

Put concretely, for the 4.5kL TypeScript fixture: a full parse + walk is
13.1 ms, an incremental parse + walk is 6.7 ms, and a cache hit is 0.114 ms —
the cache is 59x better than incremental parsing on the same file, for a
fraction of the plumbing.

## What it would have cost

Threading old trees through would touch the API in ways that are hard to walk
back:

1. **`SemanticParserPlugin::extract_entities(&self, content, file_path)` has
   nowhere to put a tree.** A new trait method taking
   `(old_tree, edits, new_content)` would have to be implemented — or correctly
   defaulted — by all eleven plugins, four of which are not tree-sitter based
   at all.
2. **Someone has to own the old tree between calls.** `tree_sitter::Tree` is
   not `Sync`, so it cannot live in the same process-global table the entity
   cache uses. It would need thread-local storage keyed by path, with an
   explicit lifetime — which is a cache with an invalidation problem, exactly
   what content-addressing was chosen to avoid.
3. **Someone has to produce the `InputEdit`s.** sem-core is handed two strings,
   not a diff. Deriving edits means diffing base against the side (prefix/suffix
   trimming gives one region; a real multi-region diff costs more), and a wrong
   or stale edit silently yields a *wrong parse* rather than a slow one. That is
   a correctness cliff sitting under a 2x speedup.
4. **weave's three calls are three different blobs, not a chain.** `base`,
   `ours` and `theirs` are siblings. Only `base -> ours` and `base -> theirs`
   are edit-related, so at best two of the three parses could go incremental,
   and only if the caller kept the base tree alive across them.

## What was delivered instead

The primitive, exposed and measured, so this decision can be revisited with
numbers rather than re-derived:

```rust
sem_core::parser::plugins::code::parse_tree_incremental(
    config,
    new_content,
    Some(&already_edited_old_tree),
)
```

Its doc comment states the caller's obligation (apply the `InputEdit`s to the
old tree first). `parse_tree` is the same function with `None`, so there is one
parse path, not two.

## When to revisit

If the entity walk ever gets much cheaper — or gains its own incremental mode
that revisits only the changed subtrees — the ratio flips and this becomes
worth the plumbing. The bench is committed and will say so.
