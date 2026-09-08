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
- Call sem_plan exactly once for discovery and complete source hydration.
- Treat returned symbol locations, imports, member inventory, exact callsites, and invariants as authoritative. Never invent private adapters, members, imports, or helper names.
- Preserve existing context objects and null guards.
- Call weave_transaction once with the complete atomic change and focused validation command.
- Only when validation reports an actionable code failure, call weave_transaction once more to repair it. No other tools are available.`,
};
