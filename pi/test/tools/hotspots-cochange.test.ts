import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { performSemHotspots } from "../../src/tools/sem-hotspots.ts";
import { performSemCochange } from "../../src/tools/sem-cochange.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
// Real git history, not a fixture -- same convention api-graph.test.ts's
// own hotspots()/cochange()/history() test uses, since both bottom out in
// `sem log --json` over this repo's actual commits.
const REPO_ROOT = join(__dirname, "..", "..");

test("sem_hotspots returns well-shaped rows from this repo's own commit history", async () => {
  const outcome = await performSemHotspots({ limit: 5 }, { cwd: REPO_ROOT, semBin: "sem" });
  assert.equal(outcome.isError, false, outcome.text);

  const details = outcome.details as { total?: number; hotspots?: Array<{ entity: string; file: string; commits: number }> };
  assert.ok((details.total ?? 0) <= 5);
  if ((details.total ?? 0) > 0) {
    const first = details.hotspots![0]!;
    assert.ok(first.entity);
    assert.ok(first.file);
    assert.equal(typeof first.commits, "number");
    assert.match(outcome.text, new RegExp(`${first.entity.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")} \\(${first.file.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\) — \\d+ commit`));
  }
});

test("sem_cochange returns only pairs actually involving the requested entity", async () => {
  const hot = await performSemHotspots({ limit: 5 }, { cwd: REPO_ROOT, semBin: "sem" });
  const details = hot.details as { hotspots?: Array<{ entity: string }> };
  const targetEntity = details.hotspots?.[0]?.entity;
  if (!targetEntity) return; // no history in this checkout shape -- nothing to assert against

  const outcome = await performSemCochange({ entity: targetEntity, limit: 5 }, { cwd: REPO_ROOT, semBin: "sem" });
  assert.equal(outcome.isError, false, outcome.text);
  const cochangeDetails = outcome.details as { pairs?: Array<{ a: { entity: string }; b: { entity: string } }> };
  for (const p of cochangeDetails.pairs ?? []) {
    assert.ok(p.a.entity === targetEntity || p.b.entity === targetEntity);
  }
});

test("sem_cochange reports zero co-changing entities as a plain success, not an error", async () => {
  const outcome = await performSemCochange({ entity: "totallyMissingEntityXyz123" }, { cwd: REPO_ROOT, semBin: "sem" });
  assert.equal(outcome.isError, false, outcome.text);
  assert.match(outcome.text, /no entities found that consistently change alongside/);
});
