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

### Optional: a live sem badge in your statusline

```bash
npx @ataraxy-labs/sem-skill --badge
```

Adds a small badge to your Claude Code statusline that shows sem working in real
time: how many structural queries this session, the last command **and the
entity it analyzed**, its latency, a sparkline of recent latencies, and a
rotating stat (distinct entities analyzed, top command)
(`⊕ sem ×12  impact validateToken 9ms  ▁▂▃▅▂  · 7 entities analyzed`). It picks
up sem whether the agent uses the MCP tools or the `sem` CLI, and falls back to
recent activity so it never gets stuck on "idle". It is opt-in and
non-destructive: it backs up your settings, and if you already have a statusline
it leaves it untouched and just tells you how to add the badge yourself. To
remove it, delete the `statusLine` key and the `mcp__sem__.*` and `Bash`
PostToolUse entries from `~/.claude/settings.json`.

It's idempotent, re-run it any time. It needs the sem CLI on PATH
(`npm i -g @ataraxy-labs/sem` or see the
[install docs](https://github.com/Ataraxy-Labs/sem#install)); if sem isn't
installed yet, the skill and MCP registration still go in and work once it is.

Restart the agent session afterward to load the MCP tools.

Skill content originally contributed by @linhlban150612 (sem PR #376).
