#!/usr/bin/env node
import readline from "node:readline";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { buildSemApi } from "../codemode/api.ts";
import { performWeaveEdit } from "../tools/weave-edit.ts";

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
  allow_signature_change: { type: "boolean" },
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

async function normalizeEdits(rawEdits, cwd, api) {
  const creates = [];
  const edits = [];
  const textEdits = [];
  const composed = new Map();
  for (let raw of rawEdits ?? []) {
    if (raw.op === "delete" && typeof raw.old === "string" && typeof raw.new !== "string") {
      raw = { ...raw, new: "" };
    }
    let file = raw.file ?? raw.path ?? raw.entity?.file;
    if (!file) throw new Error("each edit needs file or path");
    if (raw.operation === "create") {
      creates.push({ file, content: raw.content });
      continue;
    }
    if (raw.entity?.name && typeof raw.old !== "string" && !raw.range) {
      // Canonicalize model-facing aliases/casing against the file outline.
      // Python compatibility aliases such as MCPTool/McpTool otherwise make
      // a correct member edit fail the identity guard before any write.
      let normalizedEntity = raw.entity;
      try {
        const outline = await api.outline(file);
        const requestedName = raw.entity.name.toLowerCase();
        const requestedParent = raw.entity.parent_name?.toLowerCase();
        const canonical = (outline.entities ?? []).find((item) =>
          item.name?.toLowerCase() === requestedName &&
          (!requestedParent || item.parent_name?.toLowerCase() === requestedParent),
        );
        if (canonical) {
          normalizedEntity = {
            ...raw.entity,
            name: canonical.name,
            entity_type: canonical.type,
            ...(canonical.parent_name ? { parent_name: canonical.parent_name } : {}),
          };
        }
      } catch { /* underlying edit still reports a precise locator error */ }
      edits.push({ ...raw, entity: normalizedEntity, file, op: raw.op ?? raw.operation ?? "replace", claim: false });
      continue;
    }
    let absolute = path.resolve(cwd, file);
    const root = `${path.resolve(cwd)}${path.sep}`;
    if (!absolute.startsWith(root)) throw new Error(`edit path escapes repository: ${file}`);
    let source = await fs.readFile(absolute, "utf8").catch(() => "");
    if (typeof raw.old === "string" && !uniqueFlexibleText(source, raw.old)) {
      const needle = raw.old.split("\n").find((line) => line.trim().length >= 8)?.trim();
      if (needle) {
        const searched = await api.grep([needle], { path: ".", literal: true, limit: 40 });
        const candidates = [];
        for (const group of searched.results ?? []) {
          for (const hit of group.hits ?? []) {
            if (candidates.some((item) => item.file === hit.file)) continue;
            const candidateSource = await fs.readFile(path.resolve(cwd, hit.file), "utf8").catch(() => "");
            if (uniqueFlexibleText(candidateSource, raw.old)) candidates.push({ file: hit.file, source: candidateSource });
          }
        }
        if (candidates.length === 1) {
          file = candidates[0].file;
          source = candidates[0].source;
          absolute = path.resolve(cwd, file);
        }
      }
    }
    const outline = await api.outline(file);
    let startLine;
    let endLine;
    let replacement;
    if (raw.range) {
      startLine = raw.range.start_line;
      endLine = raw.range.end_line;
      replacement = raw.content;
    } else if (typeof raw.old === "string" && typeof raw.new === "string") {
      for (const match of raw.new.matchAll(/self\.(\_[A-Za-z_]\w*)\.[A-Za-z_]\w*/g)) {
        const field = match[1].replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
        const guardedInSource = new RegExp(`(?:not\\s+self\\.${field}|self\\.${field}\\s+is\\s+None)`).test(source);
        const guardedInReplacement = new RegExp(`(?:if\\s+self\\.${field}|not\\s+self\\.${field}|hasattr\\s*\\(\\s*self[^)]*["']${field}["']|getattr\\s*\\(\\s*self\\s*,\\s*["']${field}["'])`).test(raw.new);
        if (guardedInSource && !guardedInReplacement) {
          throw new Error(`replacement dereferences nullable self.${match[1]} without preserving its indexed null guard`);
        }
      }
      const resolved = uniqueFlexibleText(source, raw.old);
      if (!resolved) {
        throw new Error(`old text must occur exactly once in ${file}`);
      }
      raw = { ...raw, old: resolved.text };
      startLine = source.slice(0, resolved.index).split("\n").length;
      endLine = startLine + resolved.text.split("\n").length - 1;
      replacement = raw.new;
    } else {
      throw new Error(`edit for ${file} needs entity, range+content, or exact old+new`);
    }
    const enclosing = (outline.entities ?? [])
      .filter((item) => item.start_line <= startLine && item.end_line >= endLine)
      .sort((a, b) => (a.end_line - a.start_line) - (b.end_line - b.start_line))[0];
    if (!enclosing) {
      const selected = raw.range
        ? source.split("\n").slice(startLine - 1, endLine).join("\n")
        : raw.old;
      if (!selected || !uniqueFlexibleText(source, selected)) {
        throw new Error(`parser-orphan text must occur exactly once in ${file}:${startLine}-${endLine}`);
      }
      textEdits.push({
        file,
        old: selected,
        new: raw.op === "insert_before"
          ? replacement + selected
          : raw.op === "insert_after"
            ? selected + replacement
            : replacement,
      });
      continue;
    }
    const lines = source.split("\n");
    const entitySource = lines.slice(enclosing.start_line - 1, enclosing.end_line).join("\n");
    const entityKey = [file, enclosing.name, enclosing.type, enclosing.parent_name ?? ""].join("\0");
    const prior = composed.get(entityKey);
    let content = prior?.content ?? entitySource;
    if (raw.range) {
      const selected = lines.slice(startLine - 1, endLine).join("\n");
      const first = content.indexOf(selected);
      if (first < 0 || content.indexOf(selected, first + 1) >= 0) {
        throw new Error(`range source must occur exactly once in ${file} entity ${enclosing.name}`);
      }
      content = content.slice(0, first) + replacement + content.slice(first + selected.length);
    } else {
      const resolved = uniqueFlexibleText(content, raw.old);
      if (!resolved) {
        throw new Error(`old text must occur exactly once in ${file} entity ${enclosing.name}: ${raw.old.slice(0, 120)}`);
      }
      const inserted = raw.op === "insert_before"
        ? replacement + resolved.text
        : raw.op === "insert_after"
          ? resolved.text + replacement
          : replacement;
      content = content.slice(0, resolved.index) + inserted + content.slice(resolved.index + resolved.text.length);
    }
    composed.set(entityKey, {
      file,
      entity: {
        name: enclosing.name,
        entity_type: enclosing.type,
        ...(enclosing.parent_name ? { parent_name: enclosing.parent_name } : {}),
      },
      op: "replace",
      content,
      claim: false,
      allow_signature_change: raw.allow_signature_change ?? false,
    });
  }
  edits.push(...composed.values());
  return { creates, edits, textEdits };
}

const tools = new Map([
  ["sem_plan", {
    description: "One bounded structural planning query. Exact names are resolved and their full entities are read internally; regex patterns return matching source lines. No handles are exposed or required.",
    schema: object({
      entity_names: { type: "array", items: { type: "string" }, maxItems: 12, description: "Exact or likely function/class/method names. Unqualified names are allowed and may return several definitions." },
      regex_patterns: { type: "array", items: { type: "string" }, maxItems: 6, description: "Source regexes for concepts whose entity name is unknown. Matching lines are automatically expanded to their enclosing entities." },
      path: { type: "string" },
      prefer_tests: { type: "boolean", default: false, description: "Rank test files first when the task explicitly targets tests, fixtures, or test compatibility." },
      max_entities: { type: "integer", minimum: 1, maximum: 16, default: 10 },
      budget_per_entity: { type: "integer", minimum: 300, maximum: 2500, default: 1800 },
    }),
    async run(params, cwd) {
      if (planCalls >= 1) throw new Error("transaction protocol permits exactly one sem_plan call per session");
      if (transactionCalls > 0) throw new Error("sem_plan must run before any weave_transaction call");
      planCalls++;
      const started = performance.now();
      const api = buildSemApi({ cwd, semBin: "sem" });
      // Enforce bounds here as well as in the advertised schema. Some MCP
      // clients do not validate model-generated arguments before dispatch.
      const max = Math.min(params.max_entities ?? 10, 16);
      const budget = Math.min(params.budget_per_entity ?? 1800, 2500);
      const preferTests = params.prefer_tests === true;
      const definitions = [];
      const resolvedNames = new Set();
      const seen = new Set();
      const names = (params.entity_names ?? params.names ?? params.entities ?? []).slice(0, 12);
      const patterns = (params.regex_patterns ?? params.patterns ?? []).slice(0, 6);
      const patternReserve = patterns.length > 0 ? Math.min(4, Math.floor(max / 2)) : 0;
      const nameLimit = max - patternReserve;
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
      const findNameCandidate = async (name) => {
        try {
          let found = await api.find(name);
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
          if ((found.hits?.length ?? 0) === 0 && leaf) {
            try {
              const cli = await runFile("sem", ["find", leaf, "--json"], {
                cwd,
                maxBuffer: 4 * 1024 * 1024,
              });
              const rows = JSON.parse(cli.stdout || "[]");
              const cliHits = rows.filter((hit) =>
                hit.name?.toLowerCase() === leaf.toLowerCase(),
              ).map((hit) => ({
                ...hit,
                h: {
                  name: hit.name,
                  file: hit.file,
                  entity_type: hit.type,
                },
              }));
              if (cliHits.length > 0) found = { ...found, hits: cliHits };
            } catch { /* structural grep fallback remains available */ }
          }
          if ((found.hits?.length ?? 0) === 0 && /[.:]/.test(name)) {
            found = await api.find(leaf);
            const scoped = (found.hits ?? []).filter((hit) =>
              (hit.name ?? hit.h?.name)?.toLowerCase() === leaf?.toLowerCase() &&
              (!expectedParent || hit.parent_name === expectedParent || hit.h?.parent_name === expectedParent),
            );
            if (scoped.length > 0) found = { ...found, hits: scoped };
            if ((found.hits?.length ?? 0) === 0 && expectedParent) {
              const parentFound = await api.find(expectedParent);
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
          const ranked = rankedHits(found, preferTests).sort((a, b) =>
            discoveryPathRank(a.file, preferTests) - discoveryPathRank(b.file, preferTests) ||
            (matchedFiles.get(b.file) ?? 0) - (matchedFiles.get(a.file) ?? 0) ||
            a.file.localeCompare(b.file)
          );
          return { name, expectedParent, ranked: ranked.slice(0, 2), error: null };
        } catch (error) {
          return { name, expectedParent: null, ranked: [], error: String(error instanceof Error ? error.message : error).slice(-500) };
        }
      };
      // Cold SEM indexes serialize their first few accesses. Parallel finds
      // can therefore all return empty during index startup; exact discovery
      // is important enough to spend the ~3 seconds on deterministic order.
      const nameCandidates = [];
      for (const name of names) nameCandidates.push(await findNameCandidate(name));
      // Retry misses once after the successful lookups have warmed the index.
      for (let i = 0; i < nameCandidates.length; i++) {
        if (nameCandidates[i].ranked.length === 0) {
          nameCandidates[i] = await findNameCandidate(nameCandidates[i].name);
        }
      }
      // Hydrate names round-robin so ambiguous definitions cannot consume the
      // entire budget before later explicitly requested symbols are visited.
      for (let ordinal = 0; ordinal < 2 && definitions.length < nameLimit; ordinal++) {
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
      for (const definition of [...definitions]) {
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
      for (const { hit, pattern } of matchCandidates) {
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
              truncated: false,
              budget,
              content: lines.slice(selected.start_line - 1, selected.end_line).join("\n").slice(0, budget),
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
        if (!symbol || !file) return;
        const values = authorityFileCandidates.get(symbol) ?? new Set();
        values.add(file);
        authorityFileCandidates.set(symbol, values);
      };
      for (const definition of definitions) recordAuthorityFile(definition.entity?.name, definition.file);
      for (const location of symbolLocations) {
        recordAuthority(location.requested_name, location.file);
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
      return {
        definitions,
        resolutions: nameCandidates.map(({ name, expectedParent, ranked, error }) => ({
          requested_name: name,
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
        member_inventory: memberInventory,
        exact_callsites: exactCallsites,
        invariants,
        matches: matches ? {
          total_patterns: matches.total_patterns,
          ran: matches.ran,
          omitted: matches.omitted,
          results: (matches.results ?? []).map((group) => ({
            pattern: group.pattern,
            total: group.total,
            hits: rankedHits(group, preferTests).slice(0, 8).map(({ file, line, text }) => ({ file, line, text })),
          })),
        } : null,
        unresolved_names: names.filter((name) => !resolvedNames.has(name)),
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
    description: "Apply all entity edits as one atomic Weave batch, then run one focused repository check. Submit complete replacement entities returned from sem_plan.",
    schema: object({
      creates: { type: "array", items: create, maxItems: 10, description: "Complete contents for genuinely new files reported absent by sem_plan." },
      imports: { type: "array", items: importEdit, maxItems: 20, description: "Imports for existing files. Use this instead of guessing exact text in file headers that sem_plan did not return." },
      remove_imports: { type: "array", items: importEdit, maxItems: 20, description: "Complete import statements or unique import-specifier lines to remove atomically from existing files." },
      edits: { type: "array", items: flexibleEdit, maxItems: 30, description: "Existing-file changes as entity edits, 1-based line ranges, or exact old/new text. The service resolves ranges/text to enclosing entities before Weave applies them." },
      validation_cmd: { type: "string", description: "Optional focused repository test or typecheck command. Prefer the narrow target covering the edited code over the generic detected runner." },
    }),
    async run(params, cwd) {
      if (planCalls !== 1) throw new Error("weave_transaction requires one successful sem_plan call first");
      if (transactionCalls >= 2) throw new Error("transaction protocol permits one implementation and at most one repair");
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
            { cwd, semBin: "sem" },
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
      let check = await hostedValidation(cwd);
      if (!check) {
        const validationCmd = params.validation_cmd
          ?.replace(/^\.venv\/bin\/pytest\b/, "pytest")
          .replace(/^\.venv\/bin\/python\s+-m\s+pytest\b/, "python -m pytest");
        try {
          check = await api.check(validationCmd ? { cmd: validationCmd } : {});
        } catch (error) {
          const rejected = String(error instanceof Error ? error.message : error);
          if (validationCmd) {
            try {
              check = {
                ...await api.check({}),
                requested_cmd_rejected: validationCmd,
                requested_cmd_error: rejected.slice(-2000),
              };
            } catch (fallbackError) {
              check = {
                pass: null,
                stage: "unavailable",
                requested_cmd_rejected: validationCmd,
                error: String(fallbackError instanceof Error ? fallbackError.message : fallbackError).slice(-2000),
              };
            }
          } else {
            check = { pass: null, stage: "unavailable", error: rejected.slice(-2000) };
          }
        }
      }
      const checkedAt = performance.now();
      return {
        created,
        imported: [...importSnapshots.keys()],
        exact_text_edits: normalized.textEdits.length,
        edit: outcome.details ?? outcome,
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
      reply.result = { content: [{ type: "text", text: JSON.stringify(value) }], structuredContent: value };
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
