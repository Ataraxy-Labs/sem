import { createBashToolDefinition, createWriteToolDefinition, type ExtensionAPI, type ExtensionContext } from "@earendil-works/pi-coding-agent";
import { existsSync } from "node:fs";
import { resolve as resolvePath } from "node:path";
import { loadConfig } from "../src/config/load.ts";
import { FAIL_CLOSED_CONFIG } from "../src/config/default.ts";
import { resolveConfigCommands } from "../src/config/resolve-command.ts";
import { registerServerTools, type RegisterServerResult } from "../src/bridge/register.ts";
import { computeActiveTools } from "../src/bridge/tool-policy.ts";
import { getToolPayloadSummary } from "../src/bridge/tool-payload-size.ts";
import type { McpClient } from "../src/bridge/mcp-client.ts";
import type { PiSemConfig } from "../src/config/types.ts";
import { registerWeaveEdit } from "../src/tools/weave-edit.ts";
import { registerSemOutline } from "../src/tools/sem-outline.ts";
import { registerSemRead } from "../src/tools/sem-read.ts";
import { registerSemFind } from "../src/tools/sem-find.ts";
import { registerSemGrep } from "../src/tools/sem-grep.ts";
import { registerSemCallers } from "../src/tools/sem-callers.ts";
import { registerSemGraph } from "../src/tools/sem-graph.ts";
import { registerSemPath } from "../src/tools/sem-path.ts";
import { registerSemHotspots } from "../src/tools/sem-hotspots.ts";
import { registerSemCochange } from "../src/tools/sem-cochange.ts";
import { registerSemCode, SEM_CODE_TOOL_NAME } from "../src/codemode/tool.ts";
import { auditBashCommand, type BashAuditMatch } from "../src/bridge/bash-audit.ts";
import { auditWriteCommand, type WriteAuditEntry } from "../src/bridge/write-audit.ts";
import type { WriteAuditCall } from "../src/codemode/api.ts";

const WEAVE_EDIT_TOOL_NAME = "weave_edit";
const SEM_OUTLINE_TOOL_NAME = "sem_outline";
const SEM_READ_TOOL_NAME = "sem_read";
const SEM_FIND_TOOL_NAME = "sem_find";
const SEM_GREP_TOOL_NAME = "sem_grep";
const SEM_CALLERS_TOOL_NAME = "sem_callers";
const SEM_GRAPH_TOOL_NAME = "sem_graph";
const SEM_PATH_TOOL_NAME = "sem_path";
const SEM_HOTSPOTS_TOOL_NAME = "sem_hotspots";
const SEM_COCHANGE_TOOL_NAME = "sem_cochange";

// PI_SEM_MODE=code replaces the whole curated multi-tool surface above with
// ONE tool (sem_code, src/codemode/) that runs a short sandboxed script
// against a typed `sem` API -- see src/codemode/DESIGN.md. Unset/anything
// else keeps today's per-lookup tool set byte-for-byte; bash/write policy
// is unaffected either way.
const CODE_MODE = process.env.PI_SEM_MODE === "code" || process.env.PI_SEM_PURE === "1";
// Pure codespace is code mode's DEFAULT identity: no active builtins at
// all, [sem_code] is the entire tool set. The agent lives in the code
// space with one tool: ask (blast/why/where/explain), act
// (edit/rename/add), verify (check), and report a receipt (the entity
// diff) instead of a claim. sem.write is refused in this mode (api.ts) --
// sem.add is the one creation door; sem.check needs no bash builtin
// (pinned by test/bridge/extension-pure-mode.test.ts).
//
// PI_SEM_PURE=0 is the explicit opt-out (restores the bash/write
// builtins next to sem_code -- benchmark-comparability arms and legacy
// workflows); PI_SEM_PURE=1 additionally FORCES code mode on when
// PI_SEM_MODE is unset. Tools mode is untouched by this flag either way.
const PURE_MODE = CODE_MODE && process.env.PI_SEM_PURE !== "0";

export type ServerStatus = RegisterServerResult;

/**
 * Starts every configured MCP server, registers its allowlisted tools plus
 * the native weave_edit tool, and locks pi's tool set down to exactly
 * `activeBuiltins + whatever registered`. FAIL-CLOSED by construction: each
 * server's registration is wrapped in its own try/catch so one server's
 * exception (registerServerTools should never throw post-fix, but this is
 * defense in depth against a future regression) can never skip the
 * `pi.setActiveTools(...)` call at the end -- that call happens exactly
 * once, unconditionally, as the last statement in this function, so pi's
 * full default toolset is never left silently active no matter how many
 * (or how badly) servers fail.
 *
 * CODE_MODE bug fix (real fail-open, found by a real-session audit: 54.4%
 * of code-mode sessions, 100% of the SWE-bench-style task suite, actually
 * called a bridged tool like weave_impact): the bridged-MCP-server loop
 * below is now SKIPPED ENTIRELY in code mode,
 * not merely excluded from the tool names passed to setActiveTools. See
 * that skip's own comment for the mechanism this closes -- it was not a
 * pi-core setActiveTools gating failure, it was this module's own
 * computeActiveTools call always including every registered server tool
 * regardless of mode.
 */
export async function startServersAndRegisterTools(
  pi: ExtensionAPI,
  ctx: ExtensionContext,
  config: PiSemConfig,
  /** CODE_MODE only: folds every sem.write() call's audit classification into the SAME write-audit log/session-summary the builtin `write` tool wrapper feeds below. Optional -- omitting it (as every pre-existing caller does) just means sem.write() calls aren't logged there, matching prior behavior exactly. */
  onCodeModeWriteAudit?: (entry: WriteAuditCall) => void,
): Promise<{ clients: McpClient[]; statuses: ServerStatus[]; activeTools: string[] }> {
  const clients: McpClient[] = [];
  const statuses: ServerStatus[] = [];

  // Resolve every server command exactly once, up front, so the bridged-tools
  // loop below and the weaveServer lookup (which feeds live coordination in
  // BOTH modes) always agree on the binary. See resolve-command.ts for the
  // vanished-hardcoded-path incident this guards against. The fallback
  // warning is silent in -p/--mode json (ctx.ui.notify limitation, same as
  // the bypass-tool warning's documented gap).
  config = resolveConfigCommands(config, (message) => ctx.ui.notify(message, "warning"));

  // CODE_MODE never starts the bridged MCP servers at all, rather than
  // starting them and relying on setActiveTools to hide their tools --
  // that reliance was the real bug (see below). Nothing needs them in code
  // mode: sem_code shells out to the `sem` binary directly (buildSemApi's
  // semBin), and registerSemCode's own Coordinator spawns weave-mcp itself,
  // lazily, on the first sem.edit() call (getCoordinator() below,
  // independent of any client started here) -- so skipping this loop loses
  // no functionality, it just stops registering tools nothing in code mode
  // needs or should expose.
  //
  // THE BUG this closes: computeActiveTools (tool-policy.ts) always folds
  // `results.flatMap(r => r.registeredToolNames)` into its returned list,
  // with no way for a caller to exclude it -- so even when this call site
  // passed `extras = [SEM_CODE_TOOL_NAME]` for code mode, the `statuses`
  // this loop had already populated (every allowlisted bridged tool name,
  // e.g. sem_impact/sem_diff/weave_impact) rode along too. `pi.setActiveTools`
  // was therefore never actually called with `[sem_code]` alone -- it was
  // called with `[...activeBuiltins, sem_impact, sem_diff, weave_impact,
  // ..., sem_code]`, and pi's real gating (verified against
  // node_modules/@earendil-works/pi-coding-agent/dist/core/agent-session.js:
  // setActiveToolsByName replaces `agent.state.tools` wholesale with
  // exactly the names it's given) worked exactly as documented on the
  // WRONG list. This was never a pi-core "doesn't reliably gate an
  // already-registered tool" failure -- it was this file handing pi-core a
  // list that still had the bridged names in it.
  if (!CODE_MODE) {
    for (const serverConfig of config.servers) {
      let outcome: { client: McpClient; result: RegisterServerResult };
      try {
        outcome = await registerServerTools(pi, serverConfig, (serverId, chunk) => {
          // Keep stderr out of the transcript; surface only on failure below.
          void serverId;
          void chunk;
        });
      } catch (err) {
        // Defense in depth: registerServerTools is documented to always
        // resolve, never throw, but if that contract ever regresses this
        // catch keeps the loop -- and the setActiveTools call below -- going
        // regardless.
        const startError = err instanceof Error ? err : new Error(String(err));
        statuses.push({ serverId: serverConfig.id, registeredToolNames: [], startError });
        ctx.ui.notify(`pi-sem: server "${serverConfig.id}" failed unexpectedly (${startError.message}); its tools are unavailable.`, "warning");
        continue;
      }

      clients.push(outcome.client);
      statuses.push(outcome.result);
      if (outcome.result.startError) {
        ctx.ui.notify(
          `pi-sem: server "${serverConfig.id}" failed to start (${outcome.result.startError.message}); its tools are unavailable.`,
          "warning",
        );
      }
    }
  }

  // weave_edit, sem_outline, sem_read, sem_find, sem_grep, sem_callers,
  // sem_graph, sem_path, sem_hotspots, and sem_cochange are native pi tools
  // (L3), not MCP-bridged ones -- they shell out to `sem` directly (and,
  // for weave_edit's coordination, talk to the same weave-mcp binary the
  // "weave" bridge server above uses). Wire weave_edit to that exact binary
  // rather than letting it fall back to $PATH, so both paths always agree
  // on which weave-mcp is running. All ten are unconditional: they work
  // even if every MCP server failed (weave_edit just skips live
  // coordination). L2's allowlist.json (e01584b) already dropped the
  // bridged sem_entities/sem_context now that sem_outline/sem_read cover
  // that ground natively; sem_callers (6008ffe) replaces the bridged
  // weave_dependents the same way -- one door each. sem_graph/sem_path/
  // sem_hotspots/sem_cochange (ab611d8) are tools-mode-only additions --
  // code mode already has this ground covered via the sem API's own
  // graph/path/hotspots/cochange methods (src/codemode/sem-api.d.ts), so
  // they're wired only in the `else` branch below, not alongside sem_code.
  const weaveServer = config.servers.find((s) => s.id === "weave");
  if (CODE_MODE) {
    // One sandboxed tool over a typed sem API instead of the ten below --
    // see src/codemode/DESIGN.md. edit()'s live coordination talks to the
    // same weave-mcp binary the "weave" bridge server above uses, same
    // convention as registerWeaveEdit.
    // `pure` passed explicitly from this module's own load-time constant so
    // the tool surface (activeBuiltins above) and sem.write's behavior can
    // never disagree about which mode this session is in.
    registerSemCode(pi, { pure: PURE_MODE, weaveMcpCommand: weaveServer?.command, weaveMcpArgs: weaveServer?.args, onWriteAudit: onCodeModeWriteAudit });
  } else {
    registerWeaveEdit(pi, {
      weaveMcpCommand: weaveServer?.command,
      weaveMcpArgs: weaveServer?.args,
    });
    registerSemOutline(pi);
    registerSemRead(pi);
    registerSemFind(pi);
    registerSemGrep(pi);
    registerSemCallers(pi);
    registerSemGraph(pi);
    registerSemPath(pi);
    registerSemHotspots(pi);
    registerSemCochange(pi);
  }

  const activeTools = computeActiveTools(
    // Pure mode: the codespace is the whole surface -- activeBuiltins
    // forced empty regardless of config, so the active set is exactly
    // [sem_code].
    PURE_MODE ? { ...config.sessionPolicy, activeBuiltins: [] } : config.sessionPolicy,
    statuses,
    CODE_MODE
      ? [SEM_CODE_TOOL_NAME]
      : [
          WEAVE_EDIT_TOOL_NAME,
          SEM_OUTLINE_TOOL_NAME,
          SEM_READ_TOOL_NAME,
          SEM_FIND_TOOL_NAME,
          SEM_GREP_TOOL_NAME,
          SEM_CALLERS_TOOL_NAME,
          SEM_GRAPH_TOOL_NAME,
          SEM_PATH_TOOL_NAME,
          SEM_HOTSPOTS_TOOL_NAME,
          SEM_COCHANGE_TOOL_NAME,
        ],
  );
  pi.setActiveTools(activeTools);

  return { clients, statuses, activeTools };
}

/**
 * Tool names known to be registered by pi PACKAGES (not by a model
 * provider -- corrected from an earlier, wrong assumption; see below) that
 * duplicate bash/write/read-shaped capability outside pi-sem's audit
 * wrapper. Each name IS routed through pi.registerTool normally (visible in
 * pi.getAllTools(), with an accurate sourceInfo) -- these are not invisible
 * to pi's registry the way an earlier version of this check assumed.
 * What actually breaks the "only this package's curated tools plus
 * bash/write are active" guarantee is that a call to one of these still
 * reaches the model even when pi-sem's setActiveTools excludes it from the
 * active set (confirmed empirically: openai-codex/gpt-5.6-sol calling
 * exec_command with pi-sem's lockdown active and `--no-builtin-tools` set).
 *
 * Originally (wrongly) keyed on model PROVIDER ("openai-codex"). ox-review-3
 * corrected this: probing a real session's pi.getAllTools() showed these
 * tools' sourceInfo naming the actual cause -- `source: "npm:@howaboua/
 * pi-codex-conversion"`, `origin: "package"` (from ~/.pi/agent/settings.json's
 * `packages` list) -- present or absent based on whether that PACKAGE is
 * installed, independent of which provider/model is active. Provider was
 * neither necessary nor sufficient: a different provider with the package
 * still installed would have been silently unflagged; openai-codex without
 * the package installed would have been wrongly flagged.
 *
 * Deliberately a small, observation-only list, not a heuristic: add an
 * entry only after actually seeing it registered (see README.md "Known
 * limitations"). exec_command/write_stdin/apply_patch/web_run/imagegen/
 * view_image and this package's OWN alternate "exec"/"wait" code-mode pair
 * (unrelated to pi-sem's own PI_SEM_MODE=code) all come from
 * @howaboua/pi-codex-conversion, confirmed via
 * dist/adapter/activation/tool-set.js's exported name constants.
 */
export const BYPASS_TOOL_NAMES = new Set<string>(["exec_command", "write_stdin", "apply_patch", "web_run", "imagegen", "view_image", "exec", "wait"]);

export function packageBypassWarning(pi: ExtensionAPI): string | undefined {
  const bypassTools = pi.getAllTools().filter((tool) => BYPASS_TOOL_NAMES.has(tool.name));
  if (bypassTools.length === 0) return undefined;
  const packages = [...new Set(bypassTools.map((tool) => tool.sourceInfo?.source ?? "an installed package"))].sort();
  const toolNames = [...new Set(bypassTools.map((tool) => tool.name))].sort();
  return (
    `pi-sem: ${packages.join(", ")} registered tool(s) that bypass pi-sem's tool policy (${toolNames.join(", ")}). ` +
    `The "only this package's curated tools plus bash/write are active" guarantee is not held for this session -- ` +
    `run "pi --no-extensions -e <pi-sem>" (an explicit -e still loads pi-sem itself) or uninstall the package for a locked-down session.`
  );
}

export default function piSemExtension(pi: ExtensionAPI) {
  let clients: McpClient[] = [];
  let statuses: ServerStatus[] = [];
  let config: PiSemConfig | undefined;
  let started = false;
  let bypassWarning: string | undefined;
  let bashAuditMatches: BashAuditMatch[] = [];
  let writeAuditEntries: WriteAuditEntry[] = [];
  const unknownToolNamesSeen = new Set<string>();

  pi.on("session_start", async (_event, ctx) => {
    if (started) return;
    started = true;

    // Fail-closed applies here too, not just inside startServersAndRegisterTools:
    // loadConfig() can throw (a malformed PI_SEM_CONFIG's readFile/JSON.parse/
    // dynamic import all have unguarded throw points) *before* any fail-closed
    // machinery runs at all. An unguarded throw here rejects the whole
    // session_start handler, which pi's runner swallows silently, leaving the
    // session on pi's completely untouched default toolset. Never let that
    // happen -- but also never fall back to DEFAULT_CONFIG here: a config we
    // couldn't read is a config whose intended allowlist we don't know, and
    // DEFAULT_CONFIG's full curated bridged-tool surface is a BROADER grant
    // than an operator who explicitly set (now-unreadable) PI_SEM_CONFIG ever
    // actually made -- the same ambient-authority shape the code-mode
    // fail-open (83ea54f) closed. Fall back to FAIL_CLOSED_CONFIG (no bridged
    // MCP-server tools; native tools + bash/write remain, see its own doc
    // comment in src/config/default.ts) and notify, instead of propagating OR
    // silently granting the broader default.
    try {
      config = await loadConfig();
    } catch (err) {
      const loadError = err instanceof Error ? err : new Error(String(err));
      config = FAIL_CLOSED_CONFIG;
      ctx.ui.notify(
        `pi-sem: failed to load PI_SEM_CONFIG (${loadError.message}); falling back to a fail-closed config -- no bridged MCP-server tools, native tools + bash/write only.`,
        "warning",
      );
    }
    const outcome = await startServersAndRegisterTools(pi, ctx, config, (entry) => {
      // Same write-audit path the builtin `write` tool wrapper below feeds
      // (writeAuditEntries + pi.appendEntry) -- so a sem.write() force-
      // bypass shows up in the session summary the same way a builtin
      // write's strict-mode audit entry does, just tagged by its extra
      // `forced`/`refused` fields rather than a separate log stream.
      writeAuditEntries.push(entry);
      pi.appendEntry("pi-sem-write-audit", {
        source: "sem.write",
        path: entry.path,
        bytes: entry.bytes,
        isCodeFile: entry.isCodeFile,
        targetExists: entry.targetExists,
        strict: entry.strict,
        refused: entry.refused,
        forced: entry.forced,
      });
    });
    clients = outcome.clients;
    statuses = outcome.statuses;

    // Wrap pi's real bash tool with the pi-sem audit policy: classify every
    // command first; log matches as a session entry; refuse outright when
    // PI_SEM_STRICT=1. Execution otherwise delegates untouched to the
    // built-in bash implementation.
    const realBash = createBashToolDefinition(ctx.cwd);
    pi.registerTool({
      name: "bash",
      label: "bash",
      description: realBash.description,
      // NOTE: bashToolSystemPromptContribution exists in dist/core/tools/bash.d.ts
      // but is NOT re-exported from the package root in 0.84.2 (so it cannot be
      // imported here), and createBashToolDefinition stamps those exact values
      // onto the definition it returns -- so read them from there instead.
      promptSnippet: realBash.promptSnippet,
      promptGuidelines: realBash.promptGuidelines ? [...realBash.promptGuidelines] : undefined,
      parameters: realBash.parameters,
      async execute(toolCallId, params, signal, onUpdate, execCtx) {
        const strict = process.env.PI_SEM_STRICT === "1";
        const decision = auditBashCommand(params.command, strict, (path) => existsSync(resolvePath(ctx.cwd, path)));
        if (decision.matches.length > 0) {
          bashAuditMatches.push(...decision.matches);
          pi.appendEntry("pi-sem-bash-audit", {
            command: params.command,
            matches: decision.matches,
            strict,
            refused: decision.refuse,
          });
        }
        if (decision.refuse) throw new Error(decision.refusalMessage ?? "pi-sem: bash command refused by policy.");
        return realBash.execute(toolCallId, params, signal, onUpdate, execCtx);
      },
      // renderCall/renderResult deliberately omitted: with both absent, a tool
      // named "bash" inherits pi's built-in bash renderer automatically.
    });

    // Wrap pi's real write tool the same way: classify every write first
    // (path, byte count, whether the target looks like a code file sem can
    // parse, whether the target already exists on disk); log an entry
    // unconditionally; under PI_SEM_STRICT=1 refuse writes that would
    // overwrite an EXISTING code file (weave_edit is the entity-aware path
    // for those) while writes to brand-new files are always allowed, code
    // file or not. Execution otherwise delegates untouched to the built-in
    // write implementation.
    const realWrite = createWriteToolDefinition(ctx.cwd);
    pi.registerTool({
      name: "write",
      label: "write",
      description: realWrite.description,
      promptSnippet: realWrite.promptSnippet,
      promptGuidelines: realWrite.promptGuidelines ? [...realWrite.promptGuidelines] : undefined,
      parameters: realWrite.parameters,
      async execute(toolCallId, params, signal, onUpdate, execCtx) {
        const strict = process.env.PI_SEM_STRICT === "1";
        const absolutePath = resolvePath(ctx.cwd, params.path);
        const targetExists = existsSync(absolutePath);
        const contentBytes = Buffer.byteLength(params.content, "utf8");
        const decision = auditWriteCommand(params.path, contentBytes, targetExists, strict);
        writeAuditEntries.push(decision.entry);
        pi.appendEntry("pi-sem-write-audit", {
          path: decision.entry.path,
          bytes: decision.entry.bytes,
          isCodeFile: decision.entry.isCodeFile,
          targetExists,
          strict,
          refused: decision.refuse,
        });
        if (decision.refuse) throw new Error(decision.refusalMessage ?? "pi-sem: write refused by policy.");
        return realWrite.execute(toolCallId, params, signal, onUpdate, execCtx);
      },
      // renderCall/renderResult deliberately omitted: with both absent, a tool
      // named "write" inherits pi's built-in write renderer automatically.
    });

    // Checked here (not on model_select): which bypass tools are registered
    // is determined by which PACKAGES are installed (fixed at extension-load
    // time, before any session_start ever fires -- confirmed empirically:
    // pi.getAllTools() already lists exec_command et al. inside a
    // session_start handler), not by which model/provider is currently
    // active. A provider switch doesn't change this extension's own
    // BYPASS_TOOL_NAMES membership, so there is nothing to re-check on
    // model_select.
    bypassWarning = packageBypassWarning(pi);
    if (bypassWarning) ctx.ui.notify(bypassWarning, "warning");
  });

  pi.on("before_agent_start", (event) => {
    // config.systemPromptAddendum describes the tools-mode loop (outline ->
    // choose -> read -> edit -> check impact) BY TOOL NAME -- sem_outline,
    // sem_read, weave_edit, etc. None of those tools are registered when
    // CODE_MODE is on (only sem_code is); injecting it there would actively
    // mislead the model about tools it doesn't have, on top of wasting
    // tokens. Real pi's ExtensionRunner fans out to EVERY before_agent_start
    // handler registered by this extension (verified against
    // @earendil-works/pi-coding-agent's runner.js: it iterates
    // ext.handlers.get("before_agent_start") and threads each handler's
    // systemPrompt into the next), and registerSemCode registers its OWN
    // before_agent_start handler (CODE_MODE_ADDENDUM + sem-api.d.ts) exactly
    // when CODE_MODE is on -- so this handler must stay out of the way in
    // that mode, or both addenda land in the same prompt. Code mode should
    // see only its own.
    if (CODE_MODE) return;
    const addendum = config?.systemPromptAddendum;
    if (!addendum) return;
    return { systemPrompt: `${event.systemPrompt}\n\n${addendum}` };
  });

  // Safety net for a call whose name isn't in pi.getAllTools() AT ALL --
  // truly invisible to pi's own registry, unlike BYPASS_TOOL_NAMES above
  // (those ARE registered normally; packageBypassWarning catches them
  // proactively at session_start). This catches something more exotic: a
  // provider that hands the model a tool never routed through
  // pi.registerTool in the first place, including a package's bypass tool
  // not yet added to BYPASS_TOOL_NAMES. Logged once per tool name per
  // session; this handler never blocks or mutates the call.
  pi.on("tool_call", (event, ctx) => {
    if (unknownToolNamesSeen.has(event.toolName)) return;
    const isKnown = pi.getAllTools().some((tool) => tool.name === event.toolName);
    if (isKnown) return;
    unknownToolNamesSeen.add(event.toolName);
    ctx.ui.notify(
      `pi-sem: model called "${event.toolName}", which pi-sem never registered -- it bypassed pi's tool registry (see the package-bypass warning above if one was shown).`,
      "warning",
    );
  });

  pi.on("session_shutdown", async () => {
    await Promise.all(clients.map((client) => client.stop()));
    clients = [];
    statuses = [];
    started = false;
    bypassWarning = undefined;
    bashAuditMatches = [];
    writeAuditEntries = [];
    unknownToolNamesSeen.clear();
  });

  pi.registerCommand("pi-sem", {
    description: "Show pi-sem MCP server status and active tools",
    handler: async (_args, ctx) => {
      const lines: string[] = [];
      lines.push(`mode: ${CODE_MODE ? "code (PI_SEM_MODE=code)" : "tools (default)"}`);
      for (const status of statuses) {
        if (status.startError) {
          lines.push(`${status.serverId}: FAILED (${status.startError.message})`);
        } else {
          lines.push(`${status.serverId}: ${status.registeredToolNames.length} tool(s) — ${status.registeredToolNames.join(", ")}`);
        }
      }
      lines.push(`active tools: ${pi.getActiveTools().join(", ")}`);

      const transforms: string[] = [];
      for (const server of config?.servers ?? []) {
        for (const [mcpToolName, override] of Object.entries(server.tools)) {
          if (override === true || !override.transform) continue;
          const displayName = override.name ?? mcpToolName;
          transforms.push(`${displayName} (${server.id}) -> "${override.transform}"`);
        }
      }
      if (transforms.length > 0) lines.push(`result transforms: ${transforms.join(", ")}`);

      if (bypassWarning) lines.push(bypassWarning);
      if (unknownToolNamesSeen.size > 0) {
        lines.push(`unregistered tool calls seen this session: ${[...unknownToolNamesSeen].join(", ")}`);
      }
      if (bashAuditMatches.length > 0) {
        lines.push(`bash policy-bypass events this session: ${bashAuditMatches.length}`);
      }
      if (writeAuditEntries.length > 0) {
        const toCodeFiles = writeAuditEntries.filter((entry) => entry.isCodeFile).length;
        lines.push(`write audit events this session: ${writeAuditEntries.length} (${toCodeFiles} to code files)`);
      }
      const payloadSummary = getToolPayloadSummary(pi);
      lines.push(`tool definitions: ${payloadSummary.toolCount} active, ~${payloadSummary.totalChars} chars, ~${payloadSummary.totalTokens} tokens`);
      ctx.ui.notify(lines.join("\n"), "info");
    },
  });
}
