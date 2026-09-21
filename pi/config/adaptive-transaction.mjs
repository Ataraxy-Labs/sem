import transaction from "./transaction.mjs";

export default {
  ...transaction,
  sessionPolicy: { activeBuiltins: ["bash", "write"] },
  systemPromptAddendum: `Adaptive structural transaction protocol:
- Use ordinary shell/write operations directly for environment setup, generated artifacts, data-only work, and obvious localized edits.
- For unfamiliar, cross-file, API, dependency, serialization, or structurally ambiguous source changes, call sem_plan once with the original request in task plus batched names and patterns.
- Inspect sem_plan.coverage. Use weave_transaction when recommended_mode=transaction. When recommended_mode=hybrid, use the structural evidence but verify missing context narrowly. When recommended_mode=native_fallback, use native tools; if recovery_allowed=true, one broader sem_plan call is permitted first.
- Treat returned symbol locations, imports, member inventory, exact callsites, and invariants as authoritative. Never invent private adapters, members, imports, or helper names.
- Prefer one atomic weave_transaction with a focused validation command. Use a repair transaction only for an actionable code failure.
- Do not repeat native searches for facts already supplied by sem_plan.`,
};
