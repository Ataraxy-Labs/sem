import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const piRoot = join(dirname(fileURLToPath(import.meta.url)), "..");

export default {
  servers: [{
    id: "structural-transaction",
    command: process.execPath,
    args: ["--experimental-strip-types", join(piRoot, "src/transaction/server.mjs")],
    requestTimeoutMs: 600_000,
    tools: { sem_plan: true, weave_transaction: true },
  }],
  sessionPolicy: { activeBuiltins: [] },
  systemPromptAddendum: `Structural transaction protocol (mandatory):
- Call sem_plan once for discovery and complete source hydration, including the original request in task. If coverage.status is empty, make one broader recovery call using repository-relative paths.
- Read coverage before editing: transaction means structural evidence is complete; hybrid means evidence is partial; native_fallback means no structural evidence was found.
- Treat returned symbol locations, imports, member inventory, exact callsites, and invariants as authoritative. Never invent private adapters, members, imports, or helper names.
- Preserve existing context objects and null guards.
- sem_plan is read-only. Use structural_file_coverage.unindexed_files to limit native fallback precisely; never treat an unindexed file as covered.
- Call weave_transaction once with revision_digest copied exactly from sem_plan.revision.digest, the complete change, and a focused validation command. The service commits it in bounded file-affinity batches.
- If it reports retry_scope, preserve completed_files and repair only failed_files plus remaining_files. Never resend completed files.
- Only when validation reports an actionable code failure, call weave_transaction again with revision_digest copied from receipt.revision_after.digest and the minimal compiler-reported repair, with at most two repair calls. No other tools are available.`,
};
