import { alpha } from "./chain.ts";

// Local stand-ins for jest/vitest globals -- this fixture is never actually
// RUN, only parsed by sem's own entity extraction (verified empirically,
// throwaway probe: sem classifies a `test(...)` call site as entityType
// "test" purely by call shape, filename-independent, with a real "calls"
// edge to whatever it exercises -- not dependent on a real test runner
// being present). Declared locally so this file still type-checks in the
// repo's own tsc pass without pulling in @types/jest.
function test(name: string, fn: () => void): void {
  void name;
  void fn;
}
function expect(actual: unknown): { toBe: (expected: unknown) => void } {
  return { toBe: () => void actual };
}

test("alpha works", () => {
  expect(alpha()).toBe(1);
});
