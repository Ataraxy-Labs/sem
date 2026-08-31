import { test } from "node:test";
import assert from "node:assert/strict";
import { resolveEntity, nearestNames, deriveEntity, type Entity, type RawEntity } from "../../src/tools/internal/entities.ts";

function entity(overrides: Partial<Entity>): Entity {
  return {
    name: "greet",
    type: "function",
    start_line: 1,
    end_line: 3,
    start_byte: 0,
    end_byte: 10,
    parent_id: null,
    parentName: null,
    byteRangeReliable: true,
    ...overrides,
  };
}

test("resolveEntity finds a unique match by name alone", () => {
  const entities = [entity({ name: "greet" }), entity({ name: "other" })];
  const result = resolveEntity(entities, { name: "greet" });
  assert.equal(result.kind, "found");
  assert.equal(result.kind === "found" && result.entity.name, "greet");
});

test("resolveEntity reports not-found with nearest names, never guessing", () => {
  const entities = [entity({ name: "greeting" }), entity({ name: "farewell" })];
  const result = resolveEntity(entities, { name: "greet" });
  assert.equal(result.kind, "not-found");
  assert.ok(result.kind === "not-found" && result.nearest.includes("greeting"));
});

test("resolveEntity refuses an ambiguous name with the full candidate list", () => {
  const entities = [
    entity({ name: "greet", parent_id: "f.ts::class::Alice", parentName: "Alice", start_line: 2, end_line: 4 }),
    entity({ name: "greet", parent_id: "f.ts::class::Bob", parentName: "Bob", start_line: 8, end_line: 10 }),
  ];
  const result = resolveEntity(entities, { name: "greet" });
  assert.equal(result.kind, "ambiguous");
  assert.equal(result.kind === "ambiguous" && result.candidates.length, 2);
});

test("resolveEntity disambiguates an ambiguous name via parent_name", () => {
  const alice = entity({ name: "greet", parent_id: "f.ts::class::Alice", parentName: "Alice", start_line: 2, end_line: 4 });
  const bob = entity({ name: "greet", parent_id: "f.ts::class::Bob", parentName: "Bob", start_line: 8, end_line: 10 });
  const result = resolveEntity([alice, bob], { name: "greet", parent_name: "Bob" });
  assert.equal(result.kind, "found");
  assert.equal(result.kind === "found" && result.entity.parentName, "Bob");
});

test("resolveEntity disambiguates an ambiguous name via entity_type", () => {
  const asFunction = entity({ name: "handler", type: "function", start_line: 1, end_line: 2 });
  const asVariable = entity({ name: "handler", type: "variable", start_line: 5, end_line: 5 });
  const result = resolveEntity([asFunction, asVariable], { name: "handler", entity_type: "variable" });
  assert.equal(result.kind, "found");
  assert.equal(result.kind === "found" && result.entity.type, "variable");
});

test("resolveEntity falls back to ordinal only when still ambiguous after type/parent filters", () => {
  const first = entity({ name: "overload", start_line: 1, end_line: 2 });
  const second = entity({ name: "overload", start_line: 10, end_line: 12 });
  const result = resolveEntity([second, first], { name: "overload", ordinal: 1 });
  assert.equal(result.kind, "found");
  assert.equal(result.kind === "found" && result.entity.start_line, 10);
});

test("resolveEntity reports ambiguous when ordinal is out of range instead of picking first", () => {
  const first = entity({ name: "overload", start_line: 1, end_line: 2 });
  const second = entity({ name: "overload", start_line: 10, end_line: 12 });
  const result = resolveEntity([first, second], { name: "overload", ordinal: 5 });
  assert.equal(result.kind, "ambiguous");
});

test("nearestNames is empty when nothing in the file is close to the query", () => {
  const entities = [entity({ name: "completelyUnrelatedIdentifierXyz" })];
  assert.deepEqual(nearestNames(entities, "gr"), []);
});

// sem versions disagree on whether non-code entities (markdown headings,
// toml/yaml sections, ...) carry a byte range at all — confirmed
// empirically against two live sem installs during this session: 0.23.0
// omits start_byte/end_byte for them entirely; 0.23.1 includes one. A test
// that shells out to whatever `sem` happens to be on PATH and asserts a
// fixed byteRangeReliable value is really pinning that live binary's
// version, not the transformation logic — these feed both CAPTURED shapes
// directly through deriveEntity(), the actual code every consumer
// (sem-outline's token estimate, weave-edit's splice safety) depends on,
// independent of which sem is installed.
test("deriveEntity: byteRangeReliable is true for the sem >=0.23.1 shape (byte range present, non-zero) — captured for a toml section", () => {
  const raw: RawEntity = { name: "server", type: "section", start_line: 1, end_line: 3, start_byte: 0, end_byte: 37, parent_id: null };
  const result = deriveEntity(raw);
  assert.equal(result.byteRangeReliable, true);
  assert.equal(result.start_byte, 0);
  assert.equal(result.end_byte, 37);
});

test("deriveEntity: byteRangeReliable is false for the sem <=0.23.0 shape (byte range fields entirely absent) — captured for a markdown heading", () => {
  const raw: RawEntity = { name: "Section One", type: "heading", start_line: 5, end_line: 9, parent_id: "notes.md::heading::Notes" };
  const result = deriveEntity(raw);
  assert.equal(result.byteRangeReliable, false);
  assert.equal(result.start_byte, undefined);
  assert.equal(result.end_byte, undefined);
});

test("deriveEntity: byteRangeReliable is false when sem reports an explicit 0/0 range (a third observed 'no real range' shape, distinct from omitting the fields)", () => {
  const raw: RawEntity = { name: "add", type: "function", start_line: 1, end_line: 3, start_byte: 0, end_byte: 0, parent_id: null };
  const result = deriveEntity(raw);
  assert.equal(result.byteRangeReliable, false);
});

test("deriveEntity: parentName is derived from parent_id's final ::-segment regardless of byte-range shape", () => {
  const raw: RawEntity = { name: "greet", type: "method", start_line: 2, end_line: 4, start_byte: 12, end_byte: 40, parent_id: "sample.ts::class::Alice" };
  const result = deriveEntity(raw);
  assert.equal(result.parentName, "Alice");
});
