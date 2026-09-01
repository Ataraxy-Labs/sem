import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import ts from "typescript";
import { buildSemApi } from "../../src/codemode/api.ts";
import { runInSandbox } from "../../src/codemode/sandbox.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const DTS_PATH = join(__dirname, "../../src/codemode/sem-api.d.ts");

/**
 * P5 (transcript study 2026-09-02, §2 and §7): `sem-api.d.ts` declared
 * `changed(): ChangedResult` and `more(handle): MoreResult` SYNCHRONOUS,
 * while sandbox.ts's `compileValueTrampoline` wraps EVERY exposed verb in
 * `new Promise(...)`. 204 of 266 `changed()` calls in a 327-run drive were
 * written without `await` -- correctly, per the published types -- and 155
 * came back as `{}` across 146 of 327 runs (45%): the verb whose job is
 * "what did I change" silently answered "nothing" in nearly half of all
 * runs.
 *
 * The d.ts is the model's ONLY contract for this API, so the invariant this
 * file pins is the one that makes an un-awaited call impossible to write
 * "correctly": every callable the sandbox exposes is declared
 * Promise-returning, and every callable the d.ts declares actually exists.
 * `log` is the single, deliberate exception -- it goes through
 * `compileVoidTrampoline`, returns nothing, and is exempt from the call
 * gate entirely.
 */

const SYNC_BY_DESIGN = new Set(["log"]);

interface DeclaredMember {
  /** "changed", "routine", "routine.save", ... */
  path: string;
  returnsPromise: boolean;
}

function semDeclarationMembers(): DeclaredMember[] {
  const source = readFileSync(DTS_PATH, "utf8");
  const sourceFile = ts.createSourceFile("sem-api.d.ts", source, ts.ScriptTarget.Latest, true);

  let semType: ts.TypeLiteralNode | undefined;
  for (const statement of sourceFile.statements) {
    if (!ts.isVariableStatement(statement)) continue;
    for (const decl of statement.declarationList.declarations) {
      if (ts.isIdentifier(decl.name) && decl.name.text === "sem" && decl.type && ts.isTypeLiteralNode(decl.type)) {
        semType = decl.type;
      }
    }
  }
  assert.ok(semType, "sem-api.d.ts must declare `const sem: { ... }` as an inline type literal");

  const returnsPromise = (type: ts.TypeNode | undefined): boolean =>
    type !== undefined && ts.isTypeReferenceNode(type) && ts.isIdentifier(type.typeName) && type.typeName.text === "Promise";

  const members: DeclaredMember[] = [];
  const walk = (typeLiteral: ts.TypeLiteralNode, prefix: string): void => {
    for (const member of typeLiteral.members) {
      if (ts.isMethodSignature(member) && member.name && ts.isIdentifier(member.name)) {
        members.push({ path: `${prefix}${member.name.text}`, returnsPromise: returnsPromise(member.type) });
        continue;
      }
      if (ts.isCallSignatureDeclaration(member)) {
        // e.g. `routine: { (name, params?): Promise<unknown>; save(...): ... }`
        members.push({ path: prefix.replace(/\.$/, ""), returnsPromise: returnsPromise(member.type) });
        continue;
      }
      if (ts.isPropertySignature(member) && member.name && ts.isIdentifier(member.name) && member.type && ts.isTypeLiteralNode(member.type)) {
        walk(member.type, `${prefix}${member.name.text}.`);
      }
    }
  };
  walk(semType, "");
  return members;
}

test("sem-api.d.ts declares EVERY sem.* verb as Promise-returning (P5: the trampoline is always async)", () => {
  const members = semDeclarationMembers();
  assert.ok(members.length > 20, `expected the full verb surface, parsed only ${members.length}`);

  const sync = members.filter((m) => !m.returnsPromise && !SYNC_BY_DESIGN.has(m.path)).map((m) => m.path);
  assert.deepEqual(
    sync,
    [],
    `these sem.* verbs are declared synchronous in sem-api.d.ts but run through sandbox.ts's Promise trampoline, ` +
      `so an un-awaited call (which the types say is correct) returns a bare Promise that serializes to {}: ${sync.join(", ")}`,
  );
});

test("every verb the sem API actually exposes is declared in sem-api.d.ts", () => {
  const declared = new Set(semDeclarationMembers().map((m) => m.path));
  const api = buildSemApi({ cwd: process.cwd(), semBin: "sem" }) as unknown as Record<string, unknown>;

  const missing: string[] = [];
  for (const [name, value] of Object.entries(api)) {
    if (typeof value !== "function") continue;
    if (!declared.has(name)) missing.push(name);
    for (const [prop, propValue] of Object.entries(value as unknown as Record<string, unknown>)) {
      if (typeof propValue === "function" && !declared.has(`${name}.${prop}`)) missing.push(`${name}.${prop}`);
    }
  }
  assert.deepEqual(missing, [], `undeclared in sem-api.d.ts: ${missing.join(", ")}`);
});

test("the sandbox really does hand back a Promise for a verb the d.ts once called synchronous", async () => {
  const api = buildSemApi({ cwd: process.cwd(), semBin: "sem" });
  const result = await runInSandbox(
    `const unawaited = sem.changed();
     return { isThenable: typeof unawaited === "object" && unawaited !== null && typeof unawaited.then === "function", serialized: JSON.stringify(unawaited) };`,
    { sem: api },
  );
  assert.equal(result.ok, true, result.error?.message ?? "sandbox run failed");
  const value = result.value as { isThenable: boolean; serialized: string };
  assert.equal(value.isThenable, true, "sem.changed() inside the sandbox must be a Promise");
  assert.equal(value.serialized, "{}", "an un-awaited sem.* call serializes to {} -- exactly the P5 failure the types must prevent");
});
