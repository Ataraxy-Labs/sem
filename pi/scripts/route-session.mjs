#!/usr/bin/env node
/** Stable pre-session admission command for any coding-agent adapter.
 * Task text is read from stdin to avoid shell quoting and process-list leaks.
 * The command always emits a usable decision; internal failures fail open to
 * native mode rather than preventing the agent from starting. */

import { execFileSync } from "node:child_process";
import process from "node:process";
import { decideSessionRoute, SESSION_ROUTING_PROTOCOL } from "../src/transaction/protocol-generalization.ts";

async function readStdin() {
  let value = "";
  for await (const chunk of process.stdin) value += chunk;
  return value;
}

function trackedFiles() {
  try {
    return execFileSync("git", ["ls-files", "-co", "--exclude-standard", "-z"], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
      maxBuffer: 16 * 1024 * 1024,
    }).split("\0").filter(Boolean);
  } catch {
    return [];
  }
}

try {
  const input = await readStdin();
  let request = { task: input, files: trackedFiles(), indexWarm: undefined };
  if (input.trimStart().startsWith("{")) {
    const envelope = JSON.parse(input);
    if (typeof envelope.task !== "string") throw new Error("route request requires string field: task");
    if (envelope.files != null && (!Array.isArray(envelope.files) || envelope.files.some((file) => typeof file !== "string"))) {
      throw new Error("route request files must be an array of strings");
    }
    request = {
      task: envelope.task,
      files: envelope.files ?? trackedFiles(),
      indexWarm: typeof envelope.index_warm === "boolean" ? envelope.index_warm : undefined,
    };
  }
  const decision = decideSessionRoute({
    ...request,
    indexWarm: request.indexWarm ?? (process.env.SEM_INDEX_WARM === "1" ? true : process.env.SEM_INDEX_WARM === "0" ? false : undefined),
  });
  process.stdout.write(`${JSON.stringify(decision)}\n`);
} catch (error) {
  process.stdout.write(`${JSON.stringify({
    protocol: SESSION_ROUTING_PROTOCOL,
    mode: "native",
    attach_structural_tools: false,
    confidence: "high",
    reason: "routing failed; native fallback selected",
    signals: ["router-error"],
    fallback: "native",
    index: "unknown",
    error: String(error instanceof Error ? error.message : error).slice(0, 500),
  })}\n`);
}
