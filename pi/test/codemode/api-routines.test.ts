import { test } from "node:test";
import assert from "node:assert/strict";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { buildSemApi, type RoutineReplay, type RoutineSaveResult, type SemApiDeps } from "../../src/codemode/api.ts";
import { runInSandbox } from "../../src/codemode/sandbox.ts";

/**
 * Routines: reason once, run many (DESIGN-routines.md). A routine is THE
 * SCRIPT THAT JUST WORKED, saved as an ordinary repo file
 * (.sem/routines/<name>.mjs) with its inputs lifted to params, and replayed
 * later in the same sandbox with no new authority. These tests pin the
 * whole loop: save (substitution, header, overwrite refusal), list, replay
 * (round-trip, param variation, telemetry), and the honest failure modes
 * (missing name, stale repo, depth guard, save-inside-replay no-op).
 */

function makeTree(files: Record<string, string>): string {
  const dir = mkdtempSync(join(tmpdir(), "sem-routines-"));
  for (const [name, content] of Object.entries(files)) {
    mkdirSync(join(dir, name, ".."), { recursive: true });
    writeFileSync(join(dir, name), content);
  }
  return dir;
}

const api = (dir: string, deps: Partial<SemApiDeps> = {}) => buildSemApi({ cwd: dir, semBin: "sem", ...deps });

const routineFile = (dir: string, name: string) => join(dir, ".sem", "routines", `${name}.mjs`);

test("save writes .sem/routines/<name>.mjs: JSON header line + source with param values substituted", async () => {
  const dir = makeTree({});
  try {
    const source = 'const a = await sem.find("Widget");\nconst n = 3;\nreturn `${\'Widget\'}:${n}`;';
    const sem = api(dir, { scriptSource: source });
    const r = (await sem.routine.save("widget-scan", { params: { target: "Widget", limit: 3 }, description: "scan a widget" })) as RoutineSaveResult;
    assert.equal(r.saved, true);
    assert.equal(r.file, ".sem/routines/widget-scan.mjs");
    // Both quote styles substituted; the number word-bounded.
    assert.deepEqual(r.substitutions, { target: 2, limit: 1 });
    assert.deepEqual(r.warnings, []);
    const raw = readFileSync(routineFile(dir, "widget-scan"), "utf8");
    const [header, ...body] = raw.split("\n");
    const parsed = JSON.parse(header!.replace("// sem:routine ", ""));
    assert.equal(parsed.description, "scan a widget");
    assert.deepEqual(parsed.params, { target: "Widget", limit: 3 });
    assert.equal(body.join("\n"), "const a = await sem.find(params.target);\nconst n = params.limit;\nreturn `${params.target}:${n}`;");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a param whose example value never appears in the source is reported as a warning, not silently dropped", async () => {
  const dir = makeTree({});
  try {
    const sem = api(dir, { scriptSource: 'return "hello";' });
    const r = (await sem.routine.save("greet", { params: { name: "Ada" } })) as RoutineSaveResult;
    assert.equal(r.saved, true);
    assert.deepEqual(r.substitutions, { name: 0 });
    assert.equal(r.warnings!.length, 1);
    assert.match(r.warnings![0]!, /"name"/);
    assert.match(r.warnings![0]!, /will not vary/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("save refuses to overwrite an existing routine without { update: true }", async () => {
  const dir = makeTree({});
  try {
    const sem = api(dir, { scriptSource: "return 1;" });
    await sem.routine.save("thing", {});
    await assert.rejects(sem.routine.save("thing", {}), (err: Error) => {
      assert.match(err.message, /already exists/);
      assert.match(err.message, /update: true/);
      return true;
    });
    const sem2 = api(dir, { scriptSource: "return 2;" });
    const r = (await sem2.routine.save("thing", { update: true })) as RoutineSaveResult;
    assert.equal(r.saved, true);
    assert.match(readFileSync(routineFile(dir, "thing"), "utf8"), /return 2;/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("save refuses outside a sem_code run (no script source) and refuses a path-shaped name", async () => {
  const dir = makeTree({});
  try {
    await assert.rejects(api(dir).routine.save("x", {}), /no script source/);
    await assert.rejects(api(dir, { scriptSource: "return 1;" }).routine.save("../evil", {}), /not a valid routine name/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("routines() lists names + descriptions + param keys; empty repo lists []", async () => {
  const dir = makeTree({});
  try {
    assert.deepEqual(await api(dir).routines(), []);
    const sem = api(dir, { scriptSource: 'return "hi";' });
    await sem.routine.save("b-second", { params: { x: 1 }, description: "two" });
    await sem.routine.save("a-first", { description: "one" });
    assert.deepEqual(await api(dir).routines(), [
      { name: "a-first", description: "one", params: [] },
      { name: "b-second", description: "two", params: ["x"] },
    ]);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a missing routine refuses and lists what IS available (next-call rule)", async () => {
  const dir = makeTree({});
  try {
    const sem = api(dir, { scriptSource: "return 1;" });
    await sem.routine.save("real-one", {});
    await assert.rejects(sem.routine("imagined"), (err: Error) => {
      assert.match(err.message, /no routine named "imagined"/);
      assert.match(err.message, /real-one/);
      return true;
    });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("round-trip through the real sandbox: run A saves the working script, run B replays it with a different param -- no re-exploration, telemetry split intact", async () => {
  const dir = makeTree({
    "Cargo.toml": '[package]\nname = "fixture"\n',
    "src/lib.rs": "pub fn top() {}\n",
  });
  try {
    // RUN A: the model "reasoned" its way to a working script, then saved it.
    const scriptA = [
      'const r = await sem.add({ module: "checksum", content: "pub fn c() {}\\n" });',
      'await sem.routine.save("add-module", { params: { module: "checksum" }, description: "add + wire a module" });',
      "return r.file;",
    ].join("\n");
    const runA = await runInSandbox(scriptA, { sem: api(dir, { scriptSource: scriptA }) });
    assert.equal(runA.ok, true, runA.error?.message ?? "run A failed");
    assert.equal(runA.value, "src/checksum.rs");
    // The save is a gated call like any other, under its dotted name.
    assert.ok(runA.calls.some((c) => c.fn === "routine.save" && c.ok), "save must be recorded as routine.save");

    // RUN B: fresh api (new "session"), same repo. A fresh session's api()
    // carries no memory of run A's save (routine-trust-boundary DESIGN --
    // sem.routine.save's session-scoped trust leg is per buildSemApi()
    // instance, same as `handles`/`changes`), so this replay is otherwise
    // untrusted; vetting it via .sem/routines.trust is exactly the
    // human-in-the-loop path the design intends for a routine that outlives
    // its saving session, and keeps this test's own subject (round-trip
    // replay mechanics, not the trust gate) isolated from that gate.
    writeFileSync(join(dir, ".sem", "routines.trust"), "add-module\n");
    const routineLog: RoutineReplay[] = [];
    const scriptB = 'return await sem.routine("add-module", { module: "digest" });';
    const runB = await runInSandbox(scriptB, { sem: api(dir, { scriptSource: scriptB, routineLog }) });
    assert.equal(runB.ok, true, runB.error?.message ?? "run B failed");
    assert.equal(runB.value, "src/digest.rs", "the replay's return value is the routine's own return value");
    assert.match(readFileSync(join(dir, "src/digest.rs"), "utf8"), /pub fn c/);
    assert.match(readFileSync(join(dir, "src/lib.rs"), "utf8"), /mod digest;/, "replayed add must wire the module like any direct call");
    // Telemetry: the outer run records ONE plain "routine" call (the
    // apiCallSequence vocabulary is untouched)...
    assert.deepEqual(runB.calls.map((c) => c.fn), ["routine"]);
    // ...and the replay's inner work lands in routineLog for details.routines.
    assert.equal(routineLog.length, 1);
    assert.equal(routineLog[0]!.name, "add-module");
    assert.ok(routineLog[0]!.apiCalls >= 1, "replayed sem.* calls must be counted");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("routine.save inside a replay is an honest no-op -- every saved script ends in its own save call", async () => {
  const dir = makeTree({});
  try {
    const source = ['const out = { n: params.n };', 'const s = await sem.routine.save("self", { params: { n: 7 } });', "return { out, saved: s.saved, note: s.note };"].join("\n");
    const sem = api(dir, { scriptSource: source });
    await sem.routine.save("self", { params: { n: 7 } });
    const before = readFileSync(routineFile(dir, "self"), "utf8");
    const result = (await sem.routine("self", { n: 9 })) as { out: { n: number }; saved: boolean; note: string };
    assert.deepEqual(result.out, { n: 9 });
    assert.equal(result.saved, false);
    assert.match(result.note, /no-op inside a routine replay/);
    assert.equal(readFileSync(routineFile(dir, "self"), "utf8"), before, "the file must not be rewritten by its own replay");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("depth guard: a routine cannot call another routine", async () => {
  const dir = makeTree({});
  try {
    const semInner = api(dir, { scriptSource: "return 1;" });
    await semInner.routine.save("inner", {});
    const semOuter = api(dir, { scriptSource: 'return await sem.routine("inner");' });
    await semOuter.routine.save("outer", {});
    await assert.rejects(semOuter.routine("outer"), (err: Error) => {
      assert.match(err.message, /cannot call other routines/);
      return true;
    });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("stale routine: the repo drifted, replay fails with the normal entity refusal wrapped in re-explore + re-save advice -- never a crash, never a wrong edit", async () => {
  const dir = makeTree({
    "math.ts": "export function add(a: number, b: number): number {\n  return a + b;\n}\n",
  });
  try {
    const source = 'await sem.edit({ file: "math.ts", entity: { name: "add" }, op: "replace", content: "export function add(a: number, b: number): number {\\n  return b + a;\\n}" });\nreturn "done";';
    const sem = api(dir, { scriptSource: source });
    await sem.routine.save("flip-add", {});
    // Vetted (routine-trust-boundary DESIGN): this test's subject is
    // STALENESS handling, a concern distinct from provenance trust -- an
    // untrusted routine's sem.edit is refused for TRUST reasons before it
    // ever reaches the staleness check below (see the dedicated
    // test/codemode/routine-trust.test.ts for that gate itself), so
    // "flip-add" is vetted here via .sem/routines.trust to isolate the
    // staleness path the way this test always meant to.
    writeFileSync(join(dir, ".sem", "routines.trust"), "flip-add\n");
    // The repo moves on: `add` is renamed out from under the routine.
    writeFileSync(join(dir, "math.ts"), "export function sum(a: number, b: number): number {\n  return a + b;\n}\n");
    await assert.rejects(api(dir).routine("flip-add"), (err: Error) => {
      assert.match(err.message, /sem\.routine\("flip-add"\)/);
      assert.match(err.message, /may be stale/);
      assert.match(err.message, /sem\.routine\.save\("flip-add", \{ update: true/);
      return true;
    });
    assert.match(readFileSync(join(dir, "math.ts"), "utf8"), /function sum/, "a stale replay must not have edited anything");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("pure mode: save + replay work identically (a routine is just a script; pure is the default identity)", async () => {
  const dir = makeTree({});
  try {
    const source = "return 21 * 2;";
    const sem = api(dir, { pure: true, scriptSource: source });
    const saved = (await sem.routine.save("doubler", { params: { x: 21 }, description: "doubles x" })) as RoutineSaveResult;
    assert.equal(saved.saved, true);
    assert.deepEqual(saved.substitutions, { x: 1 });
    assert.equal(await api(dir, { pure: true }).routine("doubler", { x: 50 }), 100);
    assert.equal(await api(dir, { pure: true }).routine("doubler"), 42, "saved example params are the replay defaults");
    assert.ok(existsSync(routineFile(dir, "doubler")));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
