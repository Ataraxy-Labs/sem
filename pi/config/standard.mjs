// The sem+weave standard for pi: the same five tools Claude Code and Codex
// get (sem and weave MCP, by their own names and descriptions) and bash,
// which pi-sem routes through `sem hook tool`. Use with PI_SEM_MODE=standard.
const sem = process.env.SEM_BIN || "sem";
const weave = process.env.PI_SEM_WEAVE_MCP_BIN || "weave-mcp";

export default {
  servers: [
    { id: "sem", command: sem, args: ["mcp"], tools: { sem_find: true, sem_grep: true, sem_impact: true, sem_check: true } },
    { id: "weave", command: weave, args: [], env: { SEM_BIN: sem }, tools: { weave_edit: true } },
  ],
  sessionPolicy: { activeBuiltins: ["bash"] },
};
