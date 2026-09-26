import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const piRoot = join(dirname(fileURLToPath(import.meta.url)), "..");

export default {
  servers: [{
    id: "simple-structural-transaction",
    command: process.execPath,
    args: ["--experimental-strip-types", join(piRoot, "src/transaction/simple/sem-session-simple-mcp.mjs")],
    env: { SEM_EXACT_TOOLS: "1" },
    requestTimeoutMs: 780_000,
    tools: { sem_plan: true, sem_exact: true, weave_transaction: true, weave_program: true },
  }],
  sessionPolicy: { activeBuiltins: [] },
  systemPromptAddendum: `Simple structural transaction policy (experimental):
- Read the source needed to make the change; batch related requests when useful. There is no fixed discovery or transaction call quota.
- Use sem_plan for discovery and sem_exact query for batched explicit-file or symbol reads. Parser coverage is not semantic completeness; resolve ambiguity explicitly and inspect missing context.
- Do not repeatedly fetch unchanged source already in context. Acknowledge only receipts you actually received; rotate context_epoch after context loss or use refresh_source=true.
- Use entity-scoped old/new edits for small substitutions. Preserve exact entity type and parent. Use sem_exact apply for one captured entity per file, weave_transaction for multiple targets, and weave_program for repetitive edits. Recapture changed files; do not reuse stale snapshots.
- Preserve public API argument order, existing helper contracts, context objects and null guards unless the task requires changing them. Inspect affected callers.
- Complete coherent cross-file edits, callers and regression tests before expensive validation. An early focused check is appropriate when it resolves a blocking uncertainty.
- After a failure, repair and run the narrow failing target. Finish with the required public regression gate on the final patch. Confirm added tests actually ran and disclose skipped or unavailable tests. Never weaken assertions or reuse earlier success as proof for a changed patch.
- Use validation_cmd for a single project test/build/check command, not shell chains. A receipt records what was checked; it does not prove arbitrary behavior or whole-repository correctness.`,
};
