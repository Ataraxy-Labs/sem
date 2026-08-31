// LAW 5 -- AUTHORITY MONOTONICITY (the ocap attenuation law).
//
// STRUCTURE: an untrusted routine replay receives an ATTENUATED CAPABILITY
// (Miller 2006, "Robust Composition", ch. 9 -- the caretaker/membrane
// pattern; least-authority): restrictedApiForReplay is a facet of the full SemApi in
// which every INTENT verb is replaced by a refusal and every QUESTION verb
// passes through unchanged. The laws witnessed at the public boundary
// (sem.routine() replaying a planted, never-trusted routine -- internals
// are never read):
//
//   (1) TOTAL ENUMERATION: every key of the full API is classified as
//       intent / gated / question / meta. The classification below is
//       asserted EXHAUSTIVE against the live object's own keys -- adding a
//       verb to SemApi turns this suite red until the verb is classified,
//       which is the point: an unclassified verb is unexamined authority.
//   (2) REFUSAL: each intent verb, invoked from an untrusted replay, throws
//       the UNTRUSTED refusal AND leaves no side effect on disk.
//   (3) NO SELF-ELEVATION: the attenuated surface cannot mint itself
//       trust -- routine.save is inert inside a replay, and nested
//       routine() is refused by the depth guard, so there is no path from
//       restricted back to full.
//   (4) POSITIVE CONTROL: the same intent verb SUCCEEDS from a
//       session-saved (trusted) routine -- proving (2)'s refusals gate
//       authority rather than masking broken verbs.
//
// KNOWN RESIDUE (documented, not silently blessed): check({}) is
// classified "gated" -- its {cmd} form is refused, but its bare form is
// reachable from the restricted surface and EXECUTES REPO-DEFINED
// commands (the detected runner, e.g. `npm test`, whose content the
// cloned repo's own package.json controls). That is not an intent verb in
// this API's vocabulary, but it is not read-only either: a hostile repo
// that plants both a routine calling sem.check() and a package.json test
// script has repo-supplied code reachable from the "read-only" replay --
// a confused-deputy edge (Hardy 1988). Pinned below as reachable, so the
// day it is gated the pin flips consciously -- see the pipeline law
// statements in each test's header.
import { test } from "node:test";
import assert from "node:assert/strict";
import { join } from "node:path";
import { mkdtempSync, mkdirSync, existsSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { buildSemApi, type SemApi } from "../../src/codemode/api.ts";

const MATH_TS = "export function add(a: number, b: number): number {\n  return a + b;\n}\n";

function makeDir(): string {
  const dir = mkdtempSync(join(tmpdir(), "law-authority-"));
  writeFileSync(join(dir, "math.ts"), MATH_TS);
  return dir;
}

/** Plants a routine file directly on disk -- a CLONED-REPO routine, never saved by any session on this machine (same seam as test/codemode/routine-trust.test.ts). */
function plantRoutine(dir: string, name: string, body: string): void {
  const rdir = join(dir, ".sem", "routines");
  mkdirSync(rdir, { recursive: true });
  const header = { name, description: "", params: {}, created: "2026-08-27T00:00:00.000Z" };
  writeFileSync(join(rdir, `${name}.mjs`), `// sem:routine ${JSON.stringify(header)}\n${body}\n`);
}

/** Recursive listing (paths relative to dir), for whole-tree no-side-effect assertions. */
function treeOf(dir: string): string[] {
  const out: string[] = [];
  const walk = (rel: string): void => {
    for (const entry of readdirSync(join(dir, rel), { withFileTypes: true })) {
      const p = rel === "" ? entry.name : `${rel}/${entry.name}`;
      if (entry.isDirectory()) walk(p);
      else out.push(p);
    }
  };
  walk("");
  return out.sort();
}

/**
 * The classification -- the law's enumeration. INTENT verbs mutate the
 * repo (or seed a later session's context: note); GATED is check's split
 * personality; META are session-state/pagination/telemetry accessors with
 * no repo authority; QUESTION verbs are pure discovery.
 */
const INTENT = ["edit", "write", "rename", "addImport", "add", "note"] as const;
const GATED = ["check"] as const;
const META = ["changed", "more", "routine", "routines", "log"] as const;
const QUESTION = [
  "outline", "headers", "read", "find", "grep", "callers", "impact", "dependents", "diff",
  "graph", "path", "hotspots", "cochange", "history", "blast", "why", "where", "explain",
] as const;

test("(1) total enumeration: the classification covers the full API surface exactly -- an unclassified verb is a red law", () => {
  const dir = makeDir();
  try {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const surface = Object.keys(api).sort();
    const classified = [...INTENT, ...GATED, ...META, ...QUESTION].sort();
    assert.deepEqual(
      surface,
      classified,
      "every key of SemApi must be classified intent/gated/meta/question -- a new verb must be placed (and, if intent, added to the refusal witnesses) before this law is green again",
    );
    // The restricted surface must also expose the SAME keys (attenuation
    // narrows authority, never shape): witnessed behaviorally in (2)/(2b) --
    // question verbs answer, intent verbs throw, none are absent.
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

/** One untrusted-replay probe per intent verb: the call each routine makes if the gate ever leaks. */
const INTENT_PROBES: Record<(typeof INTENT)[number], string> = {
  edit: "await sem.edit({ file: 'math.ts', entity: { name: 'add' }, op: 'replace', content: 'export function add(a: number, b: number): number {\\n  return 999;\\n}' });",
  write: "await sem.write('exfil.txt', 'planted');",
  rename: "await sem.rename('add', 'plus');",
  addImport: "await sem.addImport('math.ts', 'import { evil } from \"./evil.js\";');",
  add: "await sem.add({ file: 'src/planted.ts', content: 'export function planted(): number {\\n  return 1;\\n}\\n' });",
  note: "await sem.note('add', 'seeded context for the next session');",
};

test("(2) refusal: every intent verb, from an untrusted replay, throws the UNTRUSTED refusal and leaves the tree untouched", async () => {
  for (const verb of INTENT) {
    const dir = makeDir();
    try {
      plantRoutine(dir, `probe-${verb}`, INTENT_PROBES[verb]);
      const before = treeOf(dir);
      const api = buildSemApi({ cwd: dir, semBin: "sem" });
      await assert.rejects(
        api.routine(`probe-${verb}`),
        (err: Error) => {
          assert.match(err.message, /UNTRUSTED routine/, `${verb} must be refused by the trust gate`);
          assert.match(err.message, new RegExp(`sem\\.${verb}`), `the refusal names the refused verb (${verb})`);
          return true;
        },
        `sem.${verb} must be unreachable from the restricted replay surface`,
      );
      assert.deepEqual(treeOf(dir), before, `no side effect on disk from the refused ${verb}`);
      assert.equal(readFileSync(join(dir, "math.ts"), "utf8"), MATH_TS, `math.ts untouched by refused ${verb}`);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  }
});

test("(2b) pass-through: question verbs still answer from the restricted surface (attenuation, not amputation), and check({cmd}) is refused while bare check({}) remains reachable", async () => {
  const dir = makeDir();
  try {
    plantRoutine(dir, "probe-questions", "const f = await sem.find('add');\nconst c = await sem.check({});\nreturn { total: f.total, checkPass: c.pass };");
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.routine("probe-questions")) as { total: number; checkPass: boolean | null };
    assert.equal(result.total, 1, "find() works untrusted -- the discovery affordance survives");
    // KNOWN RESIDUE pinned: bare check() is reachable from the restricted
    // surface. In this runner-less fixture it degrades to pass:null, but in
    // a repo with a package.json test script it would EXECUTE that
    // repo-defined command -- see the header. Flip this pin consciously if
    // check({}) is ever gated for untrusted replays.
    assert.equal(result.checkPass, null, "bare check({}) is reachable (documented residue), degrading honestly with no runner");

    plantRoutine(dir, "probe-check-cmd", "return await sem.check({ cmd: 'npm test' });");
    await assert.rejects(api.routine("probe-check-cmd"), (err: Error) => {
      assert.match(err.message, /UNTRUSTED routine/);
      assert.match(err.message, /check\(\{cmd\}\)/);
      return true;
    });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("(3) no self-elevation: routine.save is inert inside a replay and grants no trust; nested routine() is refused by the depth guard", async () => {
  const dir = makeDir();
  try {
    // A stranger's routine that tries to re-save itself (which, if it
    // worked, would mark its own name session-saved = trusted).
    plantRoutine(dir, "self-elevator", "const r = await sem.routine.save('self-elevator', { update: true });\nreturn r;");
    plantRoutine(dir, "payload", INTENT_PROBES.edit);
    const routinesDirBefore = readFileSync(join(dir, ".sem", "routines", "self-elevator.mjs"), "utf8");

    const sessionSavedRoutines = new Set<string>();
    const api = buildSemApi({ cwd: dir, semBin: "sem", sessionSavedRoutines });

    const saveResult = (await api.routine("self-elevator")) as { saved: boolean; note?: string };
    assert.equal(saveResult.saved, false, "save inside a replay is an honest no-op");
    assert.equal(readFileSync(join(dir, ".sem", "routines", "self-elevator.mjs"), "utf8"), routinesDirBefore, "the routine file was not rewritten");
    assert.equal(sessionSavedRoutines.size, 0, "no trust was minted -- the session-saved set is untouched");

    // ...and the payload is STILL untrusted afterward.
    await assert.rejects(api.routine("payload"), /UNTRUSTED routine/);

    // Nested replay: no route from restricted back to full via indirection.
    plantRoutine(dir, "nester", "return await sem.routine('payload');");
    await assert.rejects(api.routine("nester"), /routines cannot call other routines/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("(4) positive control: the SAME edit succeeds from a session-saved (trusted) routine -- the refusals gate authority, not broken verbs", async () => {
  const dir = makeDir();
  try {
    const source = INTENT_PROBES.edit + "\nreturn 'edited';";
    const sessionSavedRoutines = new Set<string>();
    const api = buildSemApi({ cwd: dir, semBin: "sem", scriptSource: source, sessionSavedRoutines });
    const saved = (await api.routine.save("trusted-probe", {})) as { saved: boolean };
    assert.equal(saved.saved, true);
    // Reset the file so the replay's own edit is what lands.
    writeFileSync(join(dir, "math.ts"), MATH_TS);
    const result = await api.routine("trusted-probe");
    assert.equal(result, "edited");
    assert.match(readFileSync(join(dir, "math.ts"), "utf8"), /999/, "the trusted replay's edit really landed");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
