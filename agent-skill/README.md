# @ataraxy-labs/sem-skill

One-command setup of [sem](https://github.com/Ataraxy-Labs/sem) (entity-level
code intelligence) for coding agents.

```bash
npx @ataraxy-labs/sem-skill
```

This:

1. Installs the **sem skill** into `~/.claude/skills/sem/` so the agent knows
   when and how to reach for sem (impact, context, orient, diff, blame, log)
   instead of grep for structural code questions.
2. Registers the **sem MCP server** (`sem mcp`) at user scope, so the
   `sem_impact` / `sem_context` / `sem_entities` / ... tools are available in
   every session.

It's idempotent, re-run it any time. It needs the sem CLI on PATH
(`npm i -g @ataraxy-labs/sem` or see the
[install docs](https://github.com/Ataraxy-Labs/sem#install)); if sem isn't
installed yet, the skill and MCP registration still go in and work once it is.

Restart the agent session afterward to load the MCP tools.

Skill content originally contributed by @linhlban150612 (sem PR #376).
