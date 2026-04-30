# JSON Semantic Diff — Behaviour Spec

## What is a JSON entity?

An entity is a single key-value pair anywhere inside a JSON object.
It has:
- A **JSON Pointer path** as its stable identity within the file (e.g. `/scripts/build`)
- A **parent** — the enclosing entity (or none for top-level keys)
- **content** — the raw `"key": value` text, used for content hashing
- **structural_hash** — a hash of the *value only* (key name stripped), used to detect renames

---

## What we extract entities from

| JSON structure | Extract entities? | Recurse into children? |
|---|---|---|
| Root object `{ }` | No (root itself is not an entity) | Yes — all top-level keys become entities |
| Object value `"key": { }` | Yes (the key is an entity) | Yes — recurse into the nested object |
| Array value `"key": [ ]` | Yes (the key is an entity) | **No** — array elements have no stable key name |
| Scalar value `"key": "val"` (string, number, boolean, or `null`) | Yes | N/A |
| Root is an array `[ ]` | — | File produces no entities at all |

---

## Entity types

| Value type | `entity_type` |
|---|---|
| String, number, boolean, null | `property` |
| Object `{ }` | `object` |
| Array `[ ]` | `array` |

Note: the `entity_type` field is set on each entity but is **not** part of the
entity ID. Two entities at the same JSON Pointer path with different value types
(e.g. scalar → object) share the same ID and are matched as the same entity.

---

## Display format

Changes are displayed with the **full ancestor chain** as context:

```
⊕ property   scripts::build              [added]
∆ property   jest::config::testTimeout   [modified]
```

The `parent_name` field on a change holds the full `::`-joined chain of ancestor
names (e.g. `"jest::config"` for an entity at `/jest/config/testTimeout`). The
entity's own name is **not** included in `parent_name` — the terminal formatter
combines `parent_name` and `entity_name` to produce the display path.

For `Renamed` and `Moved` changes, `entity_name` and `parent_name` always
reflect the **after** state, while `old_entity_name` carries the before key
(when changed) and `old_parent_id` carries the before parent (when changed).
Display format:

| Change type | Display |
|---|---|
| Renamed (same parent, key changed) | `parent_name::old_entity_name -> entity_name` |
| Moved (parent changed, key unchanged) | `parent_name::entity_name`, footer `moved from <old_parent_name>` |
| Moved (parent changed, key also changed) | `parent_name::old_entity_name -> entity_name`, footer `moved from <old_parent_name>` |

`<old_parent_name>` is derived from the `old_parent_id` field by resolving
the ID against the **before** entity set and reading that entity's `name`.
For top-level entities (no parent), the footer is omitted.

---

## Parent suppression

Object entities (`entity_type = "object"`) act as **containers**. When any child
changes, the parent object is **not** reported as a separate change — only the
children are. The full display path (`parent::child`) gives sufficient context.

This keeps output focused on what actually changed for large files.

```json
// before                                  // after
{ "scripts": { "build": "tsc" } }         { "scripts": { "build": "webpack" } }
```
→ `scripts::build` **Modified**
(Not: `scripts Modified` + `scripts::build Modified`)

Same rule applies to Add/Delete — when a whole object section is added or removed,
only its leaf children are reported, not the container object itself.

---

## Child move suppression

When a child entity moves only because its parent was renamed (and the child
itself is otherwise unchanged), the child move is **suppressed**. Only the
parent rename is reported.

A child is "otherwise unchanged" when its key name and value content are the
same; only its `parent_id` changed. A child whose key was also renamed is
**not** suppressed. A child whose value also changed is governed by the
[parent rename + child value change](#parent-rename--child-value-change)
limitation below — its connection to the before entity is lost and it is
reported as Deleted + Added in the new parent path.

---

## Change detection — all cases

### Top-level scalar

```json
// before               // after
{ "name": "foo" }       { "name": "bar" }
```
→ `name` **Modified**

```json
{ "name": "foo" }       { }
```
→ `name` **Deleted**

```json
{ }                     { "name": "foo" }
```
→ `name` **Added**

```json
{ "timeout": 30 }       { "testTimeout": 30 }
```
→ `testTimeout` **Renamed** from `timeout` (structural_hash matches — same value, different key)

---

### Top-level object

```json
{ "scripts": { "build": "tsc" } }     { "scripts": { "build": "webpack" } }
```
→ `scripts::build` **Modified**

```json
{ "scripts": { "build": "tsc" } }     { }
```
→ `scripts::build` **Deleted**

```json
{ }                                     { "scripts": { "build": "tsc" } }
```
→ `scripts::build` **Added**

```json
{ "scripts": { "dev": "vite" } }     { "tasks": { "dev": "vite" } }
```
→ `tasks` **Renamed** from `scripts` (structural_hash of object value matches)
(`tasks::dev` is suppressed — `dev` only "moved" because its parent was renamed.)

---

### Nested scalar — rename

```json
// before                                    // after
{ "scripts": { "run": "node ." } }          { "scripts": { "start": "node ." } }
```
→ `scripts::start` **Renamed** from `run`

---

### Nested scalar — add/delete

```json
{ "scripts": { "build": "tsc" } }     { "scripts": { "build": "tsc", "test": "jest" } }
```
→ `scripts::test` **Added**

```json
{ "scripts": { "build": "tsc", "test": "jest" } }     { "scripts": { "build": "tsc" } }
```
→ `scripts::test` **Deleted**

---

### Parent rename + child also renamed

This case is governed by the
[Parent rename when content also changed](#parent-rename-when-content-also-changed)
limitation — the renamed child key changes the parent's structural_hash, so
the parent rename itself is not detected. The child move surfaces with both
`old_entity_name` and `old_parent_id` populated, conveying the rename.

---

### Scalar ↔ object type change

A key whose value changes from scalar to object (or vice versa) is reported
as **Modified** — same key path, different value. When the new value is an
object with children (or the old value was), those children are reported
separately as Added/Deleted. Container suppression does **not** apply across
a type transition — both the parent change and the child changes are visible
because the type change itself is meaningful.

```json
{ "build": "tsc" }                     { "build": { "command": "tsc" } }
```
→ `build` **Modified**
→ `build::command` **Added**

```json
{ "config": { "watch": true } }        { "config": "auto" }
```
→ `config` **Modified**
→ `config::watch` **Deleted**

The `entity_type` of the change reflects the **after** type (`object` becomes
`property` or vice versa).

---

### Deep nesting (3+ levels)

```json
// before
{
  "jest": {
    "config": {
      "testTimeout": 5000
    }
  }
}

// after
{
  "jest": {
    "config": {
      "testTimeout": 10000
    }
  }
}
```
→ `jest::config::testTimeout` **Modified**
(Intermediate container objects `jest` and `jest::config` are not reported separately.)

---

### Array value — always treated as opaque

```json
{ "deps": ["react", "vue"] }           { "deps": ["react", "vue", "lodash"] }
```
→ `deps` **Modified**
(No child entities. Array elements are not tracked.)

```json
{ "deps": [{"name": "react"}] }        { "deps": [{"name": "react-dom"}] }
```
→ `deps` **Modified**
(Array contains objects — we still do not recurse. The whole array is opaque.)

```json
{ "deps": [{"name": "react"}] }        { "dependencies": [{"name": "react"}] }
```
→ `dependencies` **Renamed** from `deps` (structural_hash of array content matches)

---

### Null and empty object values

```json
{ "key": null }                        { "key": "value" }
```
→ `key` **Modified**

```json
{ "key": {} }                          { "key": { "build": "tsc" } }
```
→ `key` **Modified**, `key::build` **Added**
(The precision guard preserves `key` because its declaration shape changed
from `{}` to `{...}`.)

---

## Matching algorithm (overview)

Entities in before/after are matched in three phases:

1. **Phase 1 — exact ID match.** Same entity ID in both sides. If `content_hash` differs → Modified, otherwise unchanged.
2. **Phase 2 — structural_hash match.** Unmatched entities are paired by equal `structural_hash` (same value, different ID). Used for rename and move detection.
3. **Phase 3 — fuzzy similarity.** Remaining unmatched entities are paired by Jaccard similarity above a threshold. Used to recover renames where both the key and value changed slightly.

Whatever remains unmatched after phase 3 is Deleted (before only) or Added (after only).

---

## Structural hash rules (rename detection)

The `structural_hash` is computed from the **value only** — the key name is stripped.
This is what allows rename detection.

| Before | After | content_hash | structural_hash |
|---|---|---|---|
| `"build": "tsc"` | `"compile": "tsc"` | different (key name changed) | **same** → Renamed |
| `"build": "tsc"` | `"build": "webpack"` | different | different → Modified |
| `"scripts": {"dev": "vite"}` | `"tasks": {"dev": "vite"}` | different | **same** → Renamed |
| `"scripts": {"dev": "vite"}` | `"scripts": {"dev": "rollup"}` | different | different → Modified |

### Tie-breaking on duplicate structural_hash

When multiple sibling keys share the same value (e.g. several flags all set to
`true`, or several scripts all running the same command), and one or more are
renamed, the spec **does not** guarantee a specific pairing between
identical-value before/after entities. Any pairing produces semantically
equivalent output (same set of names disappeared, same set of names appeared),
so callers MUST treat the result as equivalent regardless of which old name was
paired with which new name. Implementations are free to be stable across runs
on the same input but the spec does not require it.

---

## Entity ID format

IDs are stable across runs and unique within a file.

Format: `{file_path}::{json_pointer}`

Examples:
- `package.json::/name`
- `package.json::/scripts`
- `package.json::/scripts/build`
- `package.json::/deps`

Rules:
- The JSON Pointer is always the **full absolute path** from the root (e.g. `/scripts/build`, not just `/build`)
- Key names are JSON Pointer-escaped: `~` → `~0`, `/` → `~1`
- The entity type is **not** part of the ID — a key whose value changed type
  (scalar ↔ object) keeps the same ID and is matched as Modified
- The parent ID is **not** embedded in the child ID — the full pointer is sufficient to uniquely identify any entity

---

## Known limitations

### Parent rename when content also changed

When a parent object is renamed **and** any of its content also changes in
the same commit (a sibling added/removed, a child renamed, or a child value
changed), the parent rename itself cannot be detected. The implementation
falls back to whatever leaf-level matches Phase 2/3 can recover, then
container-suppresses the parent Deleted/Added entries.

The user can usually still infer the parent rename from a child's
`old_parent_id` (footer "moved from ...") and current `parent_name`.

#### Sub-case: sibling added/removed

```json
// before                       // after
{                               {
  "scripts": {                    "tasks": {
    "build": "tsc"                  "build": "tsc",
  }                                 "test": "jest"
}                               }
                                }
```

Output:
```
→ property   tasks::build   [moved]   moved from scripts
⊕ property   tasks::test    [added]
```

`build` matches by structural_hash → Moved (parent_id changed). `scripts`
Deleted and `tasks` Added are container-suppressed because `build`'s
`old_parent_id` is `scripts` and `test`'s `parent_id` is `tasks`.

#### Sub-case: child key also renamed

```json
{ "scripts": { "dev": "vite" } }    { "tasks": { "develop": "vite" } }
```

Output:
```
→ property   tasks::dev -> develop   [moved]   moved from scripts
```

The renamed child key changes the parent's structural_hash, so the parent
rename is missed. The child still matches by structural_hash (value `"vite"`
unchanged) and surfaces with both `old_entity_name` (the old key) and
`old_parent_id` (the old parent) populated.

#### Sub-case: child value also changed

```json
{ "scripts": { "dev": "vite" } }    { "tasks": { "dev": "rollup" } }
```

Output:
```
- property   scripts::dev   [deleted]
+ property   tasks::dev     [added]
```

Both the parent's structural_hash and the child's structural_hash differ;
no Phase 2 match is possible at either level. Phase 3 fuzzy matching may
recover the connection if the surrounding content is similar enough but is
not guaranteed.

---

## Edge cases

| Case | Behaviour |
|---|---|
| Key name contains `/` e.g. `"a/b": 1` | Pointer-escaped to `/a~1b`. Entity ID: `file::/a~1b` |
| Key name contains `~` e.g. `"a~b": 1` | Pointer-escaped to `/a~0b` |
| Root document is `[]` | No entities produced |
| Root document is a scalar `"hello"` | No entities produced |
| Empty object `{}` | No entities produced |
| Object with empty nested object `{"key": {}}` | One entity: `key` (type `object`, no children) |
| Object with `null` value `{"key": null}` | One entity: `key` (type `property`) |
| Same key, value type changes (scalar ↔ object ↔ array, any combination) | The key is reported as **Modified** (entity_type reflects the after value). Children of the side that is an object — old children if before was an object, new children if after is an object — are reported as Added or Deleted. Container suppression does not apply across a type transition. Arrays remain opaque (no children either side). See [Scalar ↔ object type change](#scalar--object-type-change). |
