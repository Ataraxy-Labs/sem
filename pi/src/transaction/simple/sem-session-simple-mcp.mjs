#!/usr/bin/env node
process.env.SEM_KEEP_INDEX_WARM = "0";
import {hasMutations, refreshIndex} from "./warm-index.mjs";
import { boundedSource } from "./bounded-source.mjs";
import readline from "node:readline";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { createHash } from "node:crypto";
import { generateEdits } from "./edit-program.mjs";
import { readBoundedFiles } from "./program-files.mjs";
import { editFailed } from "./transaction-gate.mjs";
import { boundedInteger, inScope, candidatePage, compactEditReceipt, packDefinitions, compactDefinition } from "./plan-policy.mjs";
import { focusedCheck } from "./container-check-v8.mjs";
import { toolResult } from "./compact-response.mjs";
import { selectEntity, invalidateChanged, acknowledgeDefinitions } from "./repair-contract.mjs";
import { matchInventory, measuredLookup } from "./discovery-inventory.mjs";
import { ContextReceipts } from "./context-receipts.mjs";
import { ExactCode } from "./exact-code.mjs";
import { applyExact } from "./exact-weave.mjs";
import { normalizeEdits } from "./normalize-edits-simple.mjs";
import { searchPreviews } from "./search-previews.mjs";
import { pythonModuleHint } from "./import-hints.mjs";
const contextReceipts = new ContextReceipts();
import { buildSemApi } from "../../codemode/api.ts";
import { performWeaveEdit } from "../../tools/weave-edit.ts";

const runFile = promisify(execFile);
const argument = (name) => {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : undefined;
};
let hostedState = null;
let planImportAuthority = new Map();
let planImportFiles = new Map();
let planCalls = 0;
let transactionCalls = 0;
const programEntities = new Map();
const programFiles = new Map();
const programSnapshots = new Map();

function resolvePythonImportFile(fromFile, moduleName) {
  const match = moduleName.match(/^(\.*)(.*)$/);
  const dots = match[1].length;
  const tail = match[2].split(".").filter(Boolean);
  if (dots === 0) return null;
  const base = path.dirname(fromFile).split(path.sep);
  const kept = base.slice(0, Math.max(0, base.length - (dots - 1)));
  return [...kept, ...tail].join("/") + ".py";
}

function relativePythonModule(targetFile, sourceFile) {
  const target = path.dirname(targetFile).split("/");
  const source = sourceFile.replace(/\.py$/, "").split("/");
  let common = 0;
  while (common < target.length && common < source.length && target[common] === source[common]) common++;
  const dots = ".".repeat(target.length - common + 1);
  return dots + source.slice(common).join(".");
}

async function hostedValidation(cwd) {
  const image = argument("--validation-image") ?? process.env.SEM_HOSTED_VALIDATION_IMAGE;
  const scriptPath = argument("--validation-script");
  const scriptSource = scriptPath
    ? await fs.readFile(scriptPath, "utf8")
    : process.env.SEM_HOSTED_VALIDATION_SCRIPT;
  if (!image || !scriptSource) return null;
  const temporary = await fs.mkdtemp(path.join(os.tmpdir(), "sem-hosted-check-"));
  const patchFile = path.join(temporary, "candidate.diff");
  const scriptFile = path.join(temporary, "evaluate.sh");
  const container = hostedState?.image === image
    ? hostedState.container
    : `sem_check_${process.pid}_${Date.now()}`;
  const options = { maxBuffer: 20 * 1024 * 1024 };
  let patchBytes = 0;
  let containerPatchBytes = null;
  try {
    await runFile("git", ["add", "-N", "--", "."], { cwd, ...options });
    const { stdout: patch } = await runFile("git", ["diff", "--no-ext-diff", "--binary", "HEAD"], { cwd, ...options });
    patchBytes = Buffer.byteLength(patch);
    await fs.writeFile(patchFile, patch);
    await fs.writeFile(scriptFile, scriptSource, { mode: 0o755 });
    if (!hostedState || hostedState.container !== container) {
      await runFile("docker", ["run", "-d", "--name", container, "--network", "none", image, "tail", "-f", "/dev/null"], options);
      hostedState = { image, container, hasPatch: false };
    } else if (hostedState.hasPatch) {
      try {
        await runFile("docker", ["exec", "-u", "root", container, "bash", "-lc", "cd /testbed && git apply -R /tmp/candidate.diff"], options);
      } catch {
        await runFile("docker", ["exec", "-u", "root", container, "bash", "-lc", "cd /testbed && git reset --hard HEAD"], options);
      }
    }
    await runFile("docker", ["cp", patchFile, `${container}:/tmp/candidate.diff`], options);
    const size = await runFile("docker", ["exec", container, "wc", "-c", "/tmp/candidate.diff"], options);
    containerPatchBytes = Number.parseInt(size.stdout, 10);
    await runFile("docker", ["exec", "-u", "root", container, "bash", "-lc", "cd /testbed && git apply /tmp/candidate.diff"], options);
    hostedState.hasPatch = true;
    await runFile("docker", ["cp", scriptFile, `${container}:/tmp/evaluate.sh`], options);
    try {
      const result = await runFile("docker", ["exec", "-u", "root", container, "bash", "/tmp/evaluate.sh"], options);
      return { pass: /OMNIGRIL_EXIT_CODE=0(?:\s|$)/.test(result.stdout), runner: "hosted", stdout: result.stdout.slice(-12000), stderr: result.stderr.slice(-12000) };
    } catch (error) {
      const stdout = String(error.stdout ?? "");
      const stderr = String(error.stderr ?? "");
      let testLogs = "";
      try {
        const logs = await runFile("docker", ["exec", "-u", "root", container, "bash", "-lc", "find /root/.cache/bazel -path '*/testlogs/*/test.log' -type f -mmin -10 -print0 2>/dev/null | xargs -0 -r tail -n 200"], options);
        testLogs = logs.stdout;
      } catch { /* non-Bazel runners already report failures on stdout/stderr */ }
      return { pass: false, runner: "hosted", exit_code: error.code ?? null, stdout: stdout.slice(-12000), stderr: stderr.slice(-12000), test_logs: testLogs.slice(-12000) };
    }
  } catch (error) {
    return { pass: null, runner: "hosted", patch_bytes: patchBytes, container_patch_bytes: containerPatchBytes, error: String(error.stderr ?? error.message ?? error).slice(-12000) };
  } finally {
    await fs.rm(temporary, { recursive: true, force: true });
  }
}

const object = (properties, required = []) => ({ type: "object", properties, required, additionalProperties: false });
const entity = object({
  name: { type: "string" },
  file: { type: "string" },
  entity_type: { type: "string" },
  parent_name: { type: "string" },
  ordinal: { type: "integer", minimum: 0 },
}, ["name"]);
const edit = object({
  file: { type: "string" },
  entity,
  op: { type: "string", enum: ["replace", "insert_after", "insert_before", "delete"] },
  content: { type: "string" },
  allow_signature_change: { type: "boolean", description: "Set true only when a replacement intentionally renames an entity or changes its structural kind, such as replacing a declaration with a compatibility macro." },
}, ["file", "entity", "op"]);
const create = object({
  file: { type: "string" },
  content: { type: "string" },
}, ["file", "content"]);
const importEdit = object({
  file: { type: "string" },
  statement: { type: "string", description: "Complete language import statement or import spec accepted by sem.addImport." },
}, ["file", "statement"]);
const flexibleEdit = object({
  file: { type: "string" },
  path: { type: "string", description: "Alias for file." },
  entity,
  op: { type: "string", enum: ["replace", "insert_after", "insert_before", "delete"] },
  operation: { type: "string", enum: ["replace", "create"] },
  content: { type: "string" },
  range: object({ start_line: { type: "integer", minimum: 1 }, end_line: { type: "integer", minimum: 1 } }, ["start_line", "end_line"]),
  old: { type: "string", description: "Exact text to replace inside one entity." },
  new: { type: "string", description: "Replacement for old." },
  allow_signature_change: { type: "boolean", description: "Required true when the requested edit intentionally renames an entity or changes its kind/signature. Applies to old/new edits as well as full replacements. Without this explicit intent, the identity guard rolls back the batch." },
});

function uniqueFlexibleText(source, requested) {
  const first = source.indexOf(requested);
  if (first >= 0) {
    return source.indexOf(requested, first + 1) < 0
      ? { index: first, text: requested }
      : null;
  }
  const pieces = requested.split(/\s+/).filter(Boolean);
  if (pieces.length < 2) return null;
  const pattern = pieces
    .map((piece) => piece.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"))
    .join("\\s*");
  const matches = [...source.matchAll(new RegExp(pattern, "g"))];
  if (matches.length !== 1) return null;
  let index = matches[0].index;
  let text = matches[0][0];
  // The tokenized fallback starts at the first non-whitespace token. When
  // the requested anchor includes indentation, consume the actual
  // whitespace-only prefix too; otherwise inserting an indented replacement
  // after the retained prefix doubles the first line's indentation.
  if (/^[ \t]+/.test(requested)) {
    const lineStart = source.lastIndexOf("\n", index - 1) + 1;
    const prefix = source.slice(lineStart, index);
    if (/^[ \t]*$/.test(prefix)) {
      index = lineStart;
      text = prefix + text;
    }
  }
  return { index, text };
}

function discoveryPathRank(file, preferTests = false) {
  const normalized = `/${file.toLowerCase()}/`;
  const isTest = /\/(test|tests|spec|specs|fixtures)\//.test(normalized)
    || /(?:^|\/)(?:test_[^/]+|[^/]+_test)\.(?:py|go|c|cc|cpp|h|hpp|rs)$|(?:\.spec|\.test)\.[^/]+$/.test(file.toLowerCase());
  let rank = 5;
  if (/\/(src|lib|pkg|packages|app|core)\//.test(normalized)) rank -= 4;
  if (isTest) rank += preferTests ? -8 : 8;
  if (/\/(docs|doc|examples|example|samples|sample|contributing)\//.test(normalized)) rank += 8;
  return rank;
}

function rankedHits(group, preferTests = false) {
  return [...(group.hits ?? [])].sort((a, b) =>
    discoveryPathRank(a.file, preferTests) - discoveryPathRank(b.file, preferTests) || a.file.localeCompare(b.file) || a.line - b.line
  );
}

function codeWithoutDocumentation(content) {
  let triple = null;
  let blockComment = false;
  const kept = [];
  for (const line of String(content ?? "").split("\n")) {
    const trimmed = line.trim();
    if (triple) {
      if (trimmed.includes(triple)) triple = null;
      continue;
    }
    const delimiter = trimmed.includes('"""') ? '"""' : trimmed.includes("'''") ? "'''" : null;
    if (delimiter) {
      if (trimmed.split(delimiter).length % 2 === 0) triple = delimiter;
      continue;
    }
    if (blockComment) {
      if (trimmed.includes("*/")) blockComment = false;
      continue;
    }
    if (trimmed.startsWith("/*")) {
      if (!trimmed.includes("*/", 2)) blockComment = true;
      continue;
    }
    if (/^(?:#(?!\s*include)|\/\/|\*)/.test(trimmed)) continue;
    kept.push(line);
  }
  return kept.join("\n");
}

// Batch composition lives in a separately tested module.

const tools = new Map([
  ["sem_plan", {
    description: "Batch code discovery: resolve names and references structurally, or read known small files using files when imports/configuration/test layout are needed. Prefer one query containing related targets. Follow up only for missing context. Unresolved names do not prove absence.",
    schema: object({
      entity_names: { type: "array", items: { type: "string" }, maxItems: 12, description: "Exact or likely function/class/method names. Unqualified names are allowed and may return several definitions." },
      expand_context: {type:"boolean",default:false,description:"Opt in to loading enclosing bodies for regex matches and related types. Default returns match locators and reads only explicitly requested files or entity_names; use explicit reads for selected targets."},
      regex_patterns: { type: "array", items: { type: "string" }, maxItems: 6, description: "Source regexes for concepts whose entity name is unknown. Returns compact matching lines; set expand_context=true to also read enclosing entities." },
      match_offset: {type:"integer", minimum:0, maximum:10000, description:"Page offset for compact match locators, independent of definition bodies."},
      path: { type: "string" },
      context_epoch: {type:"string",minLength:1,maxLength:80,description:"Optional acknowledgement that all previous sem_plan source in this epoch remains in your context. Reuse the ID to avoid repeat bodies; change it after compaction/context loss. Omit for full source."},
      ack_sequence: {type:"integer",minimum:0,description:"Echo the context_receipt.ack_sequence from the last successfully received sem_plan response to acknowledge its source. Do not acknowledge lost responses."},
      refresh_source: {type:"boolean",description:"Return full source even when acknowledged; clears epoch receipts."},
      known_receipts: {type:"array",items:{type:"string"},maxItems:128,description:"Receipts of definitions already available in your context. Only explicitly acknowledged unchanged bodies are omitted; changed bodies are returned fully."},
      files: {type:"array",items:{type:"string"},maxItems:8,description:"Optional known repository-relative files. Explicit bounded text fallback, total 48KB shared with entity payloads. Useful for imports, configuration and small test files; check truncated."},
      candidate_offset: { type: "integer", minimum: 0, default: 0, description: "Per-name pagination offset from resolutions.next_offset. Keep query names, patterns and bounds unchanged when paging." },
      prefer_tests: { type: "boolean", default: false, description: "Rank test files first when the task explicitly targets tests, fixtures, or test compatibility." },
      max_entities: { type: "integer", minimum: 1, maximum: 64, default: 24, description: "Up to 64 short definitions; full definition payloads share a 48KB byte budget. Oversized payloads return explicit deferred locators." },
      budget_per_entity: { type: "integer", minimum: 300, maximum: 10000, default: 1800, description: "Read budget per entity; raise for long definitions. The shared 48KB payload cap still applies. See effective_budget_per_entity and truncation flags." },
    }),
    async run(params, cwd) {
      planCalls++;
      const started = performance.now();
      const api = buildSemApi({ cwd, semBin: "sem" });
      // Enforce bounds here as well as in the advertised schema. Some MCP
      // clients do not validate model-generated arguments before dispatch.
      const max = boundedInteger(params.max_entities, 24, 1, 64);
      const budget = boundedInteger(params.budget_per_entity, 1800, 300, 10000);
      const offset = boundedInteger(params.candidate_offset, 0, 0, 10000);
      const preferTests = params.prefer_tests === true;
      const definitions = [];
      const resolvedNames = new Set();
      const seen = new Set();
      const names = (params.entity_names ?? params.names ?? params.entities ?? []).slice(0, 12);
      const patterns = (params.regex_patterns ?? params.patterns ?? []).slice(0, 6);
      const patternReserve = params.expand_context === true && patterns.length > 0 ? Math.min(4, Math.floor(max / 2)) : 0;
      const nameLimit = max - patternReserve;
      const pageSize = Math.max(1, Math.floor(nameLimit / Math.max(1, names.length)));
      const matches = patterns.length > 0
        ? await api.grep(patterns, { path: params.path === "." ? undefined : params.path, limit: 60 })
        : null;
      const grepDone = performance.now();
      const matchedFiles = new Map();
      for (const group of matches?.results ?? (matches ? [matches] : [])) {
        for (const hit of group.hits ?? []) {
          matchedFiles.set(hit.file, (matchedFiles.get(hit.file) ?? 0) + 1);
        }
      }
      const lookupTimings = [];
      const lookup = measuredLookup(async name => {
        const cli = await runFile("sem", ["find", name, "--json"], {cwd, timeout:15000, maxBuffer:4*1024*1024});
        const rows = JSON.parse(cli.stdout || "[]");
        if (!Array.isArray(rows)) throw new Error("Invalid sem find result");
        return {hits: rows.map(hit => ({...hit, h:{name:hit.name,file:hit.file,entity_type:hit.type,parent_name:hit.parent_name}}))};
      }, lookupTimings);
      const findNameCandidate = async (name) => {
        try {
          let found = await lookup(name);
          let expectedParent = null;
          const parts = name.split(/::|\./);
          const leaf = parts.pop();
          if (/[.:]/.test(name)) {
            expectedParent = parts.pop() ?? null;
          }
          // `find` may include fuzzy semantic matches. Exact identity must win
          // before path heuristics, otherwise a well-named production symbol
          // can hydrate an unrelated class from a preferred src directory.
          const exactHits = (found.hits ?? []).filter((hit) =>
            (hit.name ?? hit.h?.name)?.toLowerCase() === leaf?.toLowerCase() &&
            (!expectedParent || hit.parent_name === expectedParent || hit.h?.parent_name === expectedParent),
          );
          found = { ...found, hits: exactHits };
          if ((found.hits?.length ?? 0) === 0 && /[.:]/.test(name)) {
            found = await lookup(leaf);
            const scoped = (found.hits ?? []).filter((hit) =>
              (hit.name ?? hit.h?.name)?.toLowerCase() === leaf?.toLowerCase() &&
              (!expectedParent || hit.parent_name === expectedParent || hit.h?.parent_name === expectedParent),
            );
            if (scoped.length > 0) found = { ...found, hits: scoped };
            if ((found.hits?.length ?? 0) === 0 && expectedParent) {
              const parentFound = await lookup(expectedParent);
              const structuralHits = [];
              for (const parentHit of rankedHits(parentFound, preferTests).slice(0, 3)) {
                const outline = await api.outline(parentHit.file);
                for (const item of outline.entities ?? []) {
                  if (item.name !== leaf || item.parent_name !== expectedParent) continue;
                  structuralHits.push({
                    ...item,
                    file: parentHit.file,
                    line: item.start_line,
                    h: {
                      name: item.name,
                      file: parentHit.file,
                      entity_type: item.type,
                      parent_name: item.parent_name,
                    },
                  });
                }
              }
              found = { hits: structuralHits };
            }
          }
          const ranked = rankedHits(found, preferTests).filter(hit => inScope(hit.file, params.path)).sort((a, b) =>
            discoveryPathRank(a.file, preferTests) - discoveryPathRank(b.file, preferTests) ||
            (matchedFiles.get(b.file) ?? 0) - (matchedFiles.get(a.file) ?? 0) ||
            a.file.localeCompare(b.file)
          );
          const page = candidatePage(ranked, offset, pageSize);
          return { name, expectedParent, ranked: page.hits, total: page.total,
            next_offset: page.next_offset,
            candidate_locations: ranked.slice(0, 120).map(hit => ({ file: hit.file, name: hit.name, type: hit.type })), error: null };
        } catch (error) {
          return { name, expectedParent: null, ranked: [], error: String(error instanceof Error ? error.message : error).slice(-500) };
        }
      };
      // Cold SEM indexes serialize their first few accesses. Parallel finds
      // can therefore all return empty during index startup; exact discovery
      // is important enough to spend the ~3 seconds on deterministic order.
      const nameCandidates = [];
      for (const name of names) nameCandidates.push(await findNameCandidate(name));
      // Hydrate names round-robin so ambiguous definitions cannot consume the
      // entire budget before later explicitly requested symbols are visited.
      for (let ordinal = 0; ordinal < pageSize && definitions.length < nameLimit; ordinal++) {
        for (const { name, expectedParent, ranked } of nameCandidates) {
          if (definitions.length >= nameLimit) break;
          try {
            const hit = ranked[ordinal];
            if (!hit) continue;
            const key = `${hit.file}:${hit.name}:${hit.start_line}`;
            if (seen.has(key)) continue;
            const read = await api.read(hit.h, { full: true, budget });
            if (expectedParent && read.entity?.parent_name !== expectedParent) continue;
            resolvedNames.add(name);
            seen.add(key);
            definitions.push(read);
          } catch { /* one unreadable candidate must not discard the plan */ }
        }
      }
      const missedNames = nameCandidates.filter((candidate) => !resolvedNames.has(candidate.name));
      if (missedNames.length > 0 && definitions.length < nameLimit) {
        const leafToCandidates = new Map();
        for (const candidate of missedNames) {
          const leaf = candidate.name.split(/::|\./).pop();
          if (!leaf) continue;
          const list = leafToCandidates.get(leaf) ?? [];
          list.push(candidate);
          leafToCandidates.set(leaf, list);
        }
        try {
          const fallbackMatches = await api.grep([...leafToCandidates.keys()], { path: params.path === "." ? undefined : params.path, literal: true, limit: 40 });
          for (const group of fallbackMatches.results ?? []) {
            for (const candidate of leafToCandidates.get(group.pattern) ?? []) {
              if (definitions.length >= nameLimit || resolvedNames.has(candidate.name)) continue;
              for (const hit of rankedHits(group, preferTests)) {
                const outline = await api.outline(hit.file);
                const enclosing = (outline.entities ?? [])
                  .filter((item) => item.start_line <= hit.line && item.end_line >= hit.line)
                  .sort((a, b) => (a.end_line - a.start_line) - (b.end_line - b.start_line))[0];
                if (!enclosing || (candidate.expectedParent && enclosing.parent_name !== candidate.expectedParent)) continue;
                const key = `${hit.file}:${enclosing.name}:${enclosing.start_line}`;
                if (seen.has(key)) continue;
                definitions.push(await api.read({
                  name: enclosing.name,
                  file: hit.file,
                  entity_type: enclosing.type,
                  ...(enclosing.parent_name ? { parent_name: enclosing.parent_name } : {}),
                }, { full: true, budget }));
                seen.add(key);
                resolvedNames.add(candidate.name);
                break;
              }
            }
          }
        } catch { /* regex hydration below still provides a final fallback */ }
      }
      // Expand directly referenced public types before broad regex hydration.
      // This keeps a one-shot plan structurally complete (for example,
      // FieldTree -> Subfields) without another model round trip.
      const referenceNames = [];
      for (const definition of (params.expand_context === true ? [...definitions] : [])) {
        if (definition.entity?.parent_name) referenceNames.push(definition.entity.parent_name);
        referenceNames.push(...(codeWithoutDocumentation(definition.content).match(/\b[A-Z][A-Za-z0-9_]{2,}\b/g) ?? []));
      }
      const languageTypes = new Set(["Array", "Exclude", "Function", "Iterator", "Map", "Object", "Promise", "ReadonlyArray", "Record", "Reflect", "Set", "Signal", "Symbol"]);
      const referenceCounts = new Map();
      for (const name of referenceNames) referenceCounts.set(name, (referenceCounts.get(name) ?? 0) + 1);
      const structuralType = /(?:Config|Context|Credential|Manager|Params|Provider|Result|Scheme|Service|Tool)$/;
      const references = [...new Set(referenceNames)]
        .filter((name) => !languageTypes.has(name))
        .sort((a, b) =>
          (structuralType.test(b) ? 1 : 0) - (structuralType.test(a) ? 1 : 0) ||
          (referenceCounts.get(b) ?? 0) - (referenceCounts.get(a) ?? 0) ||
          a.length - b.length || a.localeCompare(b)
        )
        .slice(0, 32);
      const referenceResults = await Promise.all(references.map(async (reference) => {
        try { return await api.find(reference); } catch { return null; }
      }));
      const symbolLocations = referenceResults.flatMap((found, index) => {
        if (!found) return [];
        const hit = rankedHits(found).sort((a, b) =>
          discoveryPathRank(a.file) - discoveryPathRank(b.file) ||
          ((a.end_line ?? a.start_line) - a.start_line) - ((b.end_line ?? b.start_line) - b.start_line)
        )[0];
        return hit ? [{ requested_name: references[index], name: hit.name, file: hit.file, entity_type: hit.type }] : [];
      });
      for (const found of referenceResults) {
        if (definitions.length >= nameLimit) break;
        try {
          if (!found) continue;
          const hit = rankedHits(found).sort((a, b) =>
            ((a.end_line ?? a.start_line) - a.start_line) - ((b.end_line ?? b.start_line) - b.start_line)
          )[0];
          if (!hit || discoveryPathRank(hit.file, preferTests) > 5 || (hit.end_line ?? hit.start_line) - hit.start_line > 60) continue;
          const key = `${hit.file}:${hit.name}:${hit.start_line}`;
          if (seen.has(key)) continue;
          seen.add(key);
          definitions.push(await api.read({
            name: hit.name,
            file: hit.file,
            entity_type: hit.type,
          }, { full: true, budget }));
        } catch { /* many capitalized words are not symbols */ }
      }
      const namesDone = performance.now();
      const rankedMatchGroups = [...(matches?.results ?? (matches ? [matches] : []))]
        .sort((a, b) => (a.total ?? a.hits?.length ?? 0) - (b.total ?? b.hits?.length ?? 0));
      const matchCandidates = rankedMatchGroups.flatMap((group) =>
        rankedHits(group, preferTests).map((hit) => ({ hit, pattern: group.pattern ?? "", groupTotal: group.total ?? group.hits?.length ?? 0 })),
      ).sort((a, b) =>
        discoveryPathRank(a.hit.file, preferTests) - discoveryPathRank(b.hit.file, preferTests) ||
        (/^\s*(?:export\s+)?(?:async\s+)?(?:func(?:tion)?|class|interface|type)\b/.test(b.hit.text) ? 1 : 0) -
          (/^\s*(?:export\s+)?(?:async\s+)?(?:func(?:tion)?|class|interface|type)\b/.test(a.hit.text) ? 1 : 0) ||
        (matchedFiles.get(b.hit.file) ?? 0) - (matchedFiles.get(a.hit.file) ?? 0) ||
        a.groupTotal - b.groupTotal ||
        a.hit.file.localeCompare(b.hit.file) ||
        a.hit.line - b.hit.line
      );
      const hydratedPerFile = new Map();
      const hydratedPerFilePattern = new Set();
      for (const { hit, pattern } of (params.expand_context === true ? matchCandidates : [])) {
        if (definitions.length >= max) break;
        if ((hydratedPerFile.get(hit.file) ?? 0) >= 3) continue;
        const filePattern = `${hit.file}\0${pattern}`;
        if (hydratedPerFilePattern.has(filePattern)) continue;
        try {
          const outline = await api.outline(hit.file);
          const enclosing = (outline.entities ?? [])
            .filter((item) => item.start_line <= hit.line && item.end_line >= hit.line)
            .sort((a, b) => (a.end_line - a.start_line) - (b.end_line - b.start_line))[0];
          if (!enclosing) continue;
          let selected = enclosing;
          let selectedParent = false;
          if (enclosing.parent_name) {
            const parentEntity = (outline.entities ?? []).find(
              (candidate) => candidate.name === enclosing.parent_name,
            );
            if (parentEntity && parentEntity.type === "variable" && parentEntity.end_line - parentEntity.start_line <= 80) {
              selected = parentEntity;
              selectedParent = true;
            }
          }
          const key = `${hit.file}:${selected.name}:${selected.start_line}`;
          if (seen.has(key)) continue;
          seen.add(key);
          if (selectedParent) {
            const lines = (await fs.readFile(path.resolve(cwd, hit.file), "utf8")).split("\n");
            definitions.push({
              file: hit.file,
              entity: {
                name: selected.name,
                type: selected.type,
                parent_name: selected.parent_name ?? null,
                start_line: selected.start_line,
                end_line: selected.end_line,
                file: hit.file,
              },
              range_source: "structural-parent-slice",
              related: [],
              budget,
              ...boundedSource(lines.slice(selected.start_line - 1, selected.end_line).join("\n"), budget),
            });
          } else {
            definitions.push(await api.read({
              name: selected.name,
              file: hit.file,
              entity_type: selected.type,
              ...(selected.parent_name ? { parent_name: selected.parent_name } : {}),
            }, { full: true, budget }));
          }
          hydratedPerFile.set(hit.file, (hydratedPerFile.get(hit.file) ?? 0) + 1);
          hydratedPerFilePattern.add(filePattern);
        } catch { /* a broad match may not belong to a parseable entity */ }
      }
      const importsByFile = await Promise.all(
        [...new Set(definitions.map((definition) => definition.file))].slice(0, 16).map(async (file) => {
          const source = await fs.readFile(path.resolve(cwd, file), "utf8").catch(() => "");
          const imports = source.split("\n").slice(0, 140).filter((line) =>
            /^\s*(?:from\s+\S+\s+import\b|import\s+\S|#\s*include\b|use\s+\S)/.test(line)
          ).slice(0, 30);
          return { file, imports };
        }),
      );
      // Give the model an explicit, compact method allow-list for every
      // hydrated class. Full class reads can be truncated and plausible API
      // names are a common source of otherwise-correct patches. Outlines are
      // structural index facts, so this adds reliability without source grep.
      const classDefinitions = definitions.filter((definition) =>
        ["class", "struct", "interface", "trait"].includes(definition.entity?.type),
      );
      const outlinesByFile = new Map(await Promise.all(
        [...new Set(classDefinitions.map((definition) => definition.file))].map(async (file) => {
          try { return [file, await api.outline(file)]; }
          catch { return [file, null]; }
        }),
      ));
      const memberInventory = (await Promise.all(classDefinitions.map(async (definition) => {
        const entity = definition.entity;
        const outline = outlinesByFile.get(definition.file);
        let members = (outline?.entities ?? [])
          .filter((item) => item.parent_name === entity.name)
          .sort((a, b) => a.start_line - b.start_line)
          .slice(0, 80)
          .map((item) => ({ name: item.name, type: item.type }));
        // Some parsers expose a decorated Python class as one opaque range.
        // Recover only member signatures from that already-bounded range;
        // bodies remain out of context and the structural class boundary is
        // still authoritative.
        if (members.length === 0 && definition.file.endsWith(".py")) {
          const source = await fs.readFile(path.resolve(cwd, definition.file), "utf8").catch(() => "");
          const classSource = source.split("\n").slice(entity.start_line - 1, entity.end_line).join("\n");
          members = [...classSource.matchAll(/^\s+(?:async\s+)?def\s+([A-Za-z_]\w*)\s*\(/gm)]
            .slice(0, 80)
            .map((match) => ({ name: match[1], type: "function" }));
        }
        return { file: definition.file, class_name: entity.name, members };
      }))).filter((item) => item.members.length > 0);
      const invariants = definitions.some((definition) => /\bProxy(?:Handler)?\b|\bownKeys\b|getOwnPropertyDescriptor/.test(definition.content ?? ""))
        ? [
            "Proxy ownKeys must return unique domain keys and must not blindly expose callable-target keys such as name or length.",
            "Descriptors synthesized for virtual proxy keys must be configurable to satisfy ECMAScript proxy invariants.",
            "When a capability applies to all object-shaped values, prefer the hydrated shared public type alias over duplicating it at one use site.",
          ]
        : [];
      if (memberInventory.length > 0) {
        invariants.push("Calls on hydrated classes must use exact member names from member_inventory; never substitute a plausible synonym for an indexed method.");
      }
      const privateNames = [...new Set(definitions
        .map((definition) => definition.entity?.name)
        .concat(memberInventory.flatMap((item) => item.members.map((member) => member.name)))
        .filter((name) => typeof name === "string" && /^_[^_]/.test(name) && name.length > 4))];
      const exactCallsites = [];
      const callsiteSources = [...definitions.map((definition) => ({
        file: definition.file,
        entity: definition.entity,
        content: String(definition.content ?? ""),
      }))];
      for (const inventory of memberInventory) {
        const classDefinition = classDefinitions.find((definition) =>
          definition.file === inventory.file && definition.entity?.name === inventory.class_name,
        );
        if (!classDefinition) continue;
        const source = await fs.readFile(path.resolve(cwd, inventory.file), "utf8").catch(() => "");
        callsiteSources.push({
          file: inventory.file,
          entity: classDefinition.entity,
          content: source.split("\n").slice(classDefinition.entity.start_line - 1, classDefinition.entity.end_line).join("\n"),
        });
      }
      for (const definition of callsiteSources) {
        for (const [offset, line] of definition.content.split("\n").entries()) {
          const called = privateNames.find((name) =>
            line.includes(`${name}(`) && !new RegExp(`\\bdef\\s+${name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\s*\\(`).test(line),
          );
          if (!called) continue;
          exactCallsites.push({
            callee: called,
            file: definition.file,
            enclosing_entity: definition.entity?.name ?? null,
            line: (definition.entity?.start_line ?? 1) + offset,
            text: line,
          });
          if (exactCallsites.length >= 40) break;
        }
        if (exactCallsites.length >= 40) break;
      }
      if (exactCallsites.length > 0) {
        invariants.push("When extracting or deleting a hydrated helper, use its exact indexed name and an exact_callsite anchor; preserve the hydrated behavior rather than inventing a renamed predecessor.");
      }
      const authorityCandidates = new Map();
      const authorityFileCandidates = new Map();
      const recordAuthority = (symbol, moduleName) => {
        if (!symbol || !moduleName) return;
        const values = authorityCandidates.get(symbol) ?? new Set();
        values.add(moduleName.replace(/\.py$/, "").split(/[./]/).filter(Boolean).pop());
        authorityCandidates.set(symbol, values);
      };
      const recordAuthorityFile = (symbol, file) => {
        if (!symbol || !pythonModuleHint(file)) return;
        const values = authorityFileCandidates.get(symbol) ?? new Set();
        values.add(file);
        authorityFileCandidates.set(symbol, values);
      };
      for (const definition of definitions) recordAuthorityFile(definition.entity?.name, definition.file);
      for (const location of symbolLocations) {
        recordAuthority(location.requested_name, pythonModuleHint(location.file));
        recordAuthorityFile(location.requested_name, location.file);
      }
      for (const item of importsByFile) {
        for (const line of item.imports) {
          const match = line.match(/^\s*from\s+([.\w]+)\s+import\s+([A-Za-z_]\w*)/);
          if (match) {
            recordAuthority(match[2], match[1]);
            recordAuthorityFile(match[2], resolvePythonImportFile(item.file, match[1]));
          }
        }
      }
      planImportAuthority = new Map([...authorityCandidates]
        .filter(([, values]) => values.size === 1)
        .map(([symbol, values]) => [symbol, [...values][0]]));
      planImportFiles = new Map([...authorityFileCandidates]
        .filter(([, values]) => values.size === 1)
        .map(([symbol, values]) => [symbol, [...values][0]]));
      const packed = packDefinitions(definitions);
      const fileContext = await readBoundedFiles(cwd, params.files ?? [], Math.max(0,48000-packed.full_definition_bytes));
      for (const file of new Set([...packed.definitions.map(d=>d.file),...fileContext.filter(d=>!d.error).map(d=>d.file)])) {
        const source=await fs.readFile(path.resolve(cwd,file),'utf8');
        const hash=createHash('sha256').update(source).digest('hex');
        if(programSnapshots.get(file)!==hash) {
          for(const [key,d] of programEntities)if(d.file===file)programEntities.delete(key);
          programFiles.delete(file);
        }
        programSnapshots.set(file,hash);
        for(const d of packed.definitions.filter(d=>d.file===file&&!d.truncated&&typeof d.content==='string')) {
          const observed=source.split('\n').slice(d.entity.start_line-1,d.entity.end_line).join('\n');
          if(observed.trimEnd()===d.content.trimEnd())programEntities.set(JSON.stringify([file,d.entity.name,d.entity.parent_name,d.entity.start_line]),d);
        }
        for(const d of fileContext.filter(d=>d.file===file&&!d.truncated))if(d.content===source)programFiles.set(file,d);
      }
      const contextView=contextReceipts.presentBundle({definitions:packed.definitions.map(compactDefinition),files:fileContext},params);
      return {
        program_cache: {entities:programEntities.size,files:programFiles.size},
        files: contextView.files,
        coverage: "partial",
        body_policy: "Explicit files and entity_names are read. Regex matches are locators unless expand_context=true; inspect selected source before editing.",
        definition_payload_bytes: packed.full_definition_bytes,
        definition_byte_budget: packed.byte_budget,
        effective_budget_per_entity: budget,
        deferred_definitions: packed.deferred,
        warning: "Bounded search is not proof of absence or completeness. Follow next_offset or narrow path for missing definitions. Read truncated entities again with a larger budget or narrower symbol.",
        definitions: contextView.definitions,
        context_receipt: contextView.context_receipt,
        resolutions: nameCandidates.map(({ name, expectedParent, ranked, error, total, next_offset, candidate_locations }) => ({
          requested_name: name,
          total_candidates: total ?? null,
          next_offset: next_offset ?? null,
          candidate_locations: candidate_locations ?? [],
          expected_parent: expectedParent,
          error,
          candidates: ranked.map((hit) => ({
            name: hit.name ?? hit.h?.name ?? null,
            parent_name: hit.parent_name ?? hit.h?.parent_name ?? null,
            file: hit.file,
          })),
        })),
        symbol_locations: symbolLocations,
        imports_by_file: importsByFile.filter((item) => item.imports.length > 0),
        import_authority: Object.fromEntries(planImportAuthority),
        import_authority_scope: "Python module hints only; not compiler-resolved import authority",
        member_inventory: memberInventory,
        exact_callsites: exactCallsites,
        invariants,
        matches: matches ? {
          total_patterns: matches.total_patterns,
          ran: matches.ran,
          omitted: matches.omitted,
          text_policy: 'Matching line previews: 400 UTF-8 bytes each, 12000 shared. Truncated text retains file/line retrieval locations; match coverage is separate from text completeness.',
          results: searchPreviews((matches.results ?? []).map(group => matchInventory(group, rankedHits(group, preferTests),
            boundedInteger(params.match_offset, 0, 0, 10000)))),
        } : null,
        unresolved_names: names.filter((name) => !resolvedNames.has(name)),
        lookup_timings: lookupTimings,
        timings_ms: {
          grep: Math.round(grepDone - started),
          names_and_reads: Math.round(namesDone - grepDone),
          hydrate_matches: Math.round(performance.now() - namesDone),
          total: Math.round(performance.now() - started),
        },
      };
    },
  }],
  ["weave_transaction", {
    description: "Apply a batch of structural edits and run focused validation. For small changes prefer {file,old,new} with a unique source anchor: the backend composes changes into enclosing entities and validates them. Do not resend an entire unchanged function to change a few tokens. Use entity+op for deletion/insertion and content for substantial rewrites.",
    schema: object({
      creates: { type: "array", items: create, maxItems: 10, description: "Complete contents for genuinely new files reported absent by sem_plan." },
      imports: { type: "array", items: importEdit, maxItems: 20, description: "Imports for existing files. Use this instead of guessing exact text in file headers that sem_plan did not return." },
      remove_imports: { type: "array", items: importEdit, maxItems: 20, description: "Complete import statements or unique import-specifier lines to remove atomically from existing files." },
      edits: { type: "array", items: flexibleEdit, maxItems: 64, description: "Prefer {file,old,new} for small changes. Include entity with exact entity_type/parent to scope repeated anchors; old must be unique inside that entity. Without entity, old must be unique in the file. Use entity+op:delete for removal, or entity/content for large rewrites. Struct and impl selectors are distinct." },
      validation_cmd: { type: "string", description: "Optional single project check command. This offline checker supports pytest, cargo, bazel, bazelisk, go, npm, pnpm, yarn, make, gradle and ./gradlew. It does not support python scripts, python -c, shell operators or heredocs; do not retry those with alternative quoting. Unsupported checks remain unverified." },
    }),
    async run(params, cwd) {
      if (params.validation_cmd?.startsWith('evidence:')) {
        if (['edits','creates','imports','remove_imports'].some(key=>params[key]?.length)) {
          throw new Error('Read validation evidence separately from edits');
        }
        // Inspecting retained evidence neither consumes an edit attempt nor
        // mutates source. The evidence itself is not a fresh validation pass.
        return {check:await focusedCheck(buildSemApi({cwd,semBin:'sem'}),params.validation_cmd)};
      }
      transactionCalls++;
      const started = performance.now();
      const api = buildSemApi({ cwd, semBin: "sem" });
      const normalized = await normalizeEdits(params.edits, cwd, api);
      const normalizedAt = performance.now();
      const requestedCreates = [...(params.creates ?? []), ...normalized.creates];
      const created = [];
      const importSnapshots = new Map();
      try {
        for (const item of requestedCreates) {
          await api.add({ file: item.file, content: item.content });
          created.push(item.file);
        }
        for (const item of [...(params.imports ?? []), ...(params.remove_imports ?? []), ...normalized.textEdits]) {
          const absolute = path.resolve(cwd, item.file);
          if (!importSnapshots.has(item.file)) {
            importSnapshots.set(item.file, await fs.readFile(absolute, "utf8"));
          }
        }
        for (const item of params.imports ?? []) {
          let statement = item.statement;
          const match = statement.match(/^(\s*from\s+)([.\w]+)(\s+import\s+)([A-Za-z_]\w*)/);
          if (match) {
            const authoritative = planImportAuthority.get(match[4]);
            const authoritativeFile = planImportFiles.get(match[4]);
            const moduleName = authoritativeFile && item.file.endsWith(".py")
              ? relativePythonModule(item.file, authoritativeFile)
              : null;
            const pieces = match[2].split(".");
            const current = pieces[pieces.length - 1];
            if (moduleName || (authoritative && current !== authoritative)) {
              if (!moduleName) pieces[pieces.length - 1] = authoritative;
              statement = match[1] + (moduleName ?? pieces.join(".")) + match[3] + match[4] + statement.slice(match[0].length);
            }
          }
          await api.addImport(item.file, statement);
        }
        for (const item of params.remove_imports ?? []) {
          const absolute = path.resolve(cwd, item.file);
          const source = await fs.readFile(absolute, "utf8");
          const statement = item.statement.trim();
          const candidates = source.split("\n");
          const matches = candidates.flatMap((line, index) =>
            line.trim() === statement ? [index] : []
          );
          if (matches.length !== 1) {
            throw new Error(`import removal must match exactly one line in ${item.file}: ${statement}`);
          }
          candidates.splice(matches[0], 1);
          await fs.writeFile(absolute, candidates.join("\n"));
        }
        for (const item of normalized.textEdits) {
          const absolute = path.resolve(cwd, item.file);
          const source = await fs.readFile(absolute, "utf8");
          const resolved = uniqueFlexibleText(source, item.old);
          if (!resolved) throw new Error(`parser-orphan text must occur exactly once in ${item.file}`);
          await fs.writeFile(
            absolute,
            source.slice(0, resolved.index) + item.new + source.slice(resolved.index + resolved.text.length),
          );
        }
      } catch (error) {
        for (const [file, source] of importSnapshots) {
          await fs.writeFile(path.resolve(cwd, file), source).catch(() => {});
        }
        for (const file of created) await fs.unlink(path.resolve(cwd, file)).catch(() => {});
        throw error;
      }
      const createdAt = performance.now();
      let outcome = { text: "no existing entities edited", details: { applied: 0 } };
      if (normalized.edits.length > 0) {
        try {
          outcome = await performWeaveEdit(
            { edits: normalized.edits, atomic: true },
            { cwd, semBin: "sem", checkDependents: false },
          );
        } catch (error) {
          for (const [file, source] of importSnapshots) {
            await fs.writeFile(path.resolve(cwd, file), source).catch(() => {});
          }
          for (const file of created) await fs.unlink(path.resolve(cwd, file)).catch(() => {});
          throw error;
        }
        const detail = outcome.details ?? {};
        if (detail.rolledBack || detail.failed > 0 || detail.succeeded === 0) {
          for (const [file, source] of importSnapshots) {
            await fs.writeFile(path.resolve(cwd, file), source).catch(() => {});
          }
          for (const file of created) await fs.unlink(path.resolve(cwd, file)).catch(() => {});
        }
      }
      const editedAt = performance.now();
      if (editFailed(outcome)) {
        return {
          created: [], imported: [], edit: outcome.details ?? outcome,
          check: { pass: null, stage: "edit_failed", reason: "Validation skipped because the requested batch did not succeed. Inspect the edit diagnostics and repair; unchanged-code tests cannot validate this request." },
          timings_ms: { normalize: Math.round(normalizedAt-started), weave: Math.round(editedAt-createdAt), check: 0, total: Math.round(editedAt-started) },
        };
      }
      let check = await hostedValidation(cwd);
      if (!check) {
        const validationCmd = params.validation_cmd
          ?.replace(/^\.venv\/bin\/pytest\b/, "pytest")
          .replace(/^\.venv\/bin\/python\s+-m\s+pytest\b/, "python -m pytest");
        check = await focusedCheck(api, validationCmd);
      }
      const checkedAt = performance.now();
      return {
        created,
        imported: [...importSnapshots.keys()],
        exact_text_edits: normalized.textEdits.length,
        edit: compactEditReceipt(outcome),
        check,
        timings_ms: {
          normalize: Math.round(normalizedAt - started),
          create: Math.round(createdAt - normalizedAt),
          weave: Math.round(editedAt - createdAt),
          check: Math.round(checkedAt - editedAt),
          total: Math.round(checkedAt - started),
        },
      };
    },
  }],
]);

const send = (value) => process.stdout.write(`${JSON.stringify(value)}\n`);
// Opt-in: do not change existing benchmark tool inventories or routing.
if(process.env.SEM_EXACT_TOOLS === '1') {
  const exact=new ExactCode();
  tools.set('sem_exact', {
    description:'Deterministic explicit-scope source operations. Prefer query with an array of selectors to resolve AND read related symbols in one call; pass revision for an existing snapshot OR files to capture and query in one call. A selector with only file reads that entire captured file (48KB shared file budget, oversized files explicitly deferred); mix file and symbol selectors in one query. Names may be exact parser names or full lexical names such as Documenter.generate. qualified_name is derived from enclosing source ranges, not runtime dispatch. Missing files in query return missing_files and not_found without blocking available files; invalid paths and symlinks still fail. Narrow ambiguity with file/type or id. Unique matches return complete source once per ID; overlapping bodies may use content_source byte offsets into a full source in the SAME response; ambiguous matches return locators without choosing. capture/resolve/read remain available. apply accepts id plus either full content or a unique old/new substitution within the captured entity; replaces entities by snapshot ID with file-hash preconditions inside Weave; supports one entity per file per batch, optional validation_cmd, and requires recapture after mutation. Rollback is compensating, not repository-wide isolation. prepare is preview-only. Parser coverage is not semantic completeness. Snapshot IDs are not repository revisions.',
    schema:object({validation_cmd:{type:'string'},op:{type:'string',enum:['capture','resolve','read','query','prepare','apply']},selectors:{type:'array',minItems:1,maxItems:64,items:{type:'object',properties:{id:{type:'string'},name:{type:'string'},file:{type:'string'},type:{type:'string'}},additionalProperties:false}},files:{type:'array',items:{type:'string'},minItems:1,maxItems:64},revision:{type:'string'},name:{type:'string'},id:{type:'string'},edits:{type:'array',items:{type:'object',properties:{id:{type:'string'},content:{type:'string'},old:{type:'string'},new:{type:'string'},allow_signature_change:{type:'boolean'}},required:['id'],additionalProperties:false},minItems:1,maxItems:64}},['op']),
    async run(p,cwd) {
      switch(p.op) {
        case 'apply': {
          try { return await applyExact(exact,cwd,p.revision,p.edits,{validate:p.validation_cmd
            ?()=>focusedCheck(buildSemApi({cwd,semBin:'sem'}),p.validation_cmd):undefined}); }
          finally { await invalidateChanged(cwd,programSnapshots,programEntities,programFiles); }
        }
        case 'query': {
          if(Boolean(p.revision)===Boolean(p.files)) throw new Error('PROVIDE_REVISION_OR_FILES');
          const snapshot=p.files?await exact.capture(cwd,p.files,{allowMissing:true}):null;
          return {...exact.query(snapshot?.revision??p.revision,p.selectors),...(snapshot?{snapshot}: {})};
        }
        case 'capture': return exact.capture(cwd,p.files);
        case 'resolve': return exact.resolve(p.revision,p.name);
        case 'read': return exact.read(p.revision,p.id);
        case 'prepare': return exact.prepare(p.revision,p.edits);
        default: throw new Error('UNKNOWN_OPERATION');
      }
    },
  });
}
// Refresh cache validity after every direct transaction, including partial failures.
// Keep unaffected definitions; never treat pre-edit snapshots as current source.
const rawTransaction=tools.get('weave_transaction').run;
tools.get('weave_transaction').run=async(params,cwd)=>{
  let result;
  try { result = await rawTransaction(params,cwd); return result; }
  finally {
    await invalidateChanged(cwd,programSnapshots,programEntities,programFiles);
    if (process.env.SEM_KEEP_INDEX_WARM === '1' && hasMutations(params)) {
      const refresh = await refreshIndex(cwd);
      if (result) result.index_refresh = refresh;
      else if (!refresh.ok) process.stderr.write('Index refresh unavailable: '+JSON.stringify(refresh)+'\\n');
    }
  }
};
tools.set('weave_program', {
  description: 'Generate repetitive edits with a synchronous JavaScript function body using cached full entities and files from sem_plan. Return {edits,imports?,creates?,validation_cmd?}, with the same shapes as weave_transaction. No IO/imports. Identity/parse/rollback guards apply. After any transaction, changed-file cache entries are invalidated while unchanged entries remain; query changed files again if needed.',
  schema: object({code:{type:'string',maxLength:30000}},['code']),
  async run(params,cwd) {
    if(!programEntities.size&&!programFiles.size)throw new Error('Use sem_plan first to populate source data');
    const generated=await generateEdits(params.code,[...programEntities.values()],[...programFiles.values()]);
    for(const [file,hash] of programSnapshots) {
      const current=createHash('sha256').update(await fs.readFile(path.resolve(cwd,file))).digest('hex');
      if(current!==hash)throw new Error(`Source changed since planning: ${file}; query again before applying`);
    }
    return await tools.get('weave_transaction').run(generated,cwd);
  },
});
const lines = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
for await (const line of lines) {
  let message;
  try { message = JSON.parse(line); } catch { continue; }
  if (message.id === undefined) continue;
  const reply = { jsonrpc: "2.0", id: message.id };
  try {
    if (message.method === "initialize") {
      reply.result = { protocolVersion: message.params?.protocolVersion ?? "2025-06-18", capabilities: { tools: {} }, serverInfo: { name: "sem-session", version: "0.1.0" } };
    } else if (message.method === "tools/list") {
      reply.result = { tools: [...tools].map(([name, tool]) => ({ name, description: tool.description, inputSchema: tool.schema })) };
    } else if (message.method === "tools/call") {
      const tool = tools.get(message.params?.name);
      if (!tool) throw new Error(`unknown tool: ${message.params?.name}`);
      const value = await tool.run(message.params.arguments ?? {}, process.cwd());
      reply.result = toolResult(value);
    } else if (message.method === "ping") {
      reply.result = {};
    } else {
      throw new Error(`method not found: ${message.method}`);
    }
  } catch (error) {
    reply.error = { code: -32603, message: error instanceof Error ? error.message : String(error) };
  }
  send(reply);
}
if (hostedState) {
  await runFile("docker", ["rm", "-f", hostedState.container], { maxBuffer: 1024 * 1024 }).catch(() => {});
}
