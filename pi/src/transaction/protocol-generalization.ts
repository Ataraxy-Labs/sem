import path from "node:path";

/** Normalize an agent-supplied search path to the repository-relative form
 * expected by sem. Empty paths and the repository root intentionally become
 * undefined so callers do not accidentally filter every result out. */
export function normalizeRepositoryPath(cwd: string, requestedPath?: string): string | undefined {
  if (requestedPath == null || String(requestedPath).trim() === "" || requestedPath === ".") {
    return undefined;
  }
  const root = path.resolve(cwd);
  const absolute = path.resolve(root, String(requestedPath));
  const relative = path.relative(root, absolute);
  if (relative === "" || relative === ".") return undefined;
  if (relative === ".." || relative.startsWith(`..${path.sep}`) || path.isAbsolute(relative)) {
    throw new Error(`search path escapes repository: ${requestedPath}`);
  }
  return relative.split(path.sep).join("/");
}

interface PlanCoverageInput {
  definitions: unknown[];
  requestedNames: string[];
  resolvedNames: Set<string>;
  matches: { results?: Array<{ hits?: unknown[] }> } | null;
}

/**
 * Partition a large semantic mutation without splitting edits that target the
 * same file.  Entity locators in one file are order-sensitive: an earlier edit
 * can move the entity used by a later edit.  Across distinct files, however,
 * batches are independent and can be committed incrementally.  Keeping this
 * policy pure makes the transaction server's retry boundary deterministic.
 */
export function partitionTransactionEdits<T extends { file: string }>(
  edits: T[],
  maxDistinctFiles = 6,
  maxEdits = 10,
): T[][] {
  if (!Number.isInteger(maxDistinctFiles) || maxDistinctFiles < 1) {
    throw new Error("maxDistinctFiles must be a positive integer");
  }
  if (!Number.isInteger(maxEdits) || maxEdits < 1) {
    throw new Error("maxEdits must be a positive integer");
  }

  const byFile = new Map<string, T[]>();
  for (const edit of edits) {
    const group = byFile.get(edit.file);
    if (group) group.push(edit);
    else byFile.set(edit.file, [edit]);
  }

  const batches: T[][] = [];
  let batch: T[] = [];
  let files = 0;
  for (const group of byFile.values()) {
    if (batch.length > 0 && (files >= maxDistinctFiles || batch.length + group.length > maxEdits)) {
      batches.push(batch);
      batch = [];
      files = 0;
    }
    batch.push(...group);
    files++;
  }
  if (batch.length > 0) batches.push(batch);
  return batches;
}

export const SESSION_ROUTING_PROTOCOL = "ataraxy.session-route.v1";

export interface SessionRouteInput {
  task: string;
  files?: string[];
  indexWarm?: boolean;
}

export interface SessionRouteDecision {
  protocol: typeof SESSION_ROUTING_PROTOCOL;
  mode: "native" | "structural";
  attach_structural_tools: boolean;
  confidence: "high" | "medium";
  reason: string;
  signals: string[];
  fallback: "native";
  index: "warm" | "cold" | "unknown";
}

const DATA_ARTIFACT = /\b(?:jsonl?|csv|parquet|dataset|records?|aggregate|report|plan files?|output_data|input_data)\b/i;
const ENVIRONMENT_TASK = /\b(?:install|configure|service|daemon|container|docker|network|permission|package manager|systemd|compiler installation)\b/i;
const SOURCE_TASK = /\b(?:bug|fix|implement|source|codebase|repository|function|method|class|module|package|compile|test suite|refactor)\b/i;
const EXPLICIT_SOURCE_PATH = /(?:^|[\s'"`(/])(?:\/?[\w.-]+\/)*(?:[\w.-]+\.(?:py|pyi|js|jsx|ts|tsx|java|kt|kts|go|rs|c|h|cc|cpp|cxx|hpp|cs|rb|php|swift|scala|scm|sh))\b/i;
const CROSS_ENTITY_SIGNALS: Array<[string, RegExp]> = [
  ["multi-file", /\b(?:multi[- ]file|multiple (?:source )?files|across (?:the )?(?:codebase|repository|modules?|packages?))\b/i],
  ["references", /\b(?:all callers?|call sites?|all references?|all implementations?|all subclasses?|dependents?)\b/i],
  ["migration", /\b(?:rename|move|migrate|refactor)\b.*\b(?:across|throughout|every|all|callers?|references?|codebase|repository)\b/is],
  ["contract", /\b(?:interface|protocol|public api|schema)\b.*\b(?:change|migration|update|implementations?|consumers?)\b/is],
  ["architecture", /\b(?:dependency graph|module graph|architecture|cross[- ]module|cross[- ]package)\b/i],
];

/**
 * Select the session substrate before an agent starts.
 *
 * Structural startup has a fixed cost, so ambiguous work deliberately routes
 * native. This is also the universal failure mode: adapters can always execute
 * the task when Sem is absent, stale, or unsupported.
 */
export function decideSessionRoute({ task, files = [], indexWarm }: SessionRouteInput): SessionRouteDecision {
  const text = String(task ?? "").trim();
  const route = (
    mode: SessionRouteDecision["mode"],
    confidence: SessionRouteDecision["confidence"],
    reason: string,
    signals: string[],
  ): SessionRouteDecision => ({
    protocol: SESSION_ROUTING_PROTOCOL,
    mode,
    attach_structural_tools: mode === "structural",
    confidence,
    reason,
    signals,
    fallback: "native",
    index: indexWarm === true ? "warm" : indexWarm === false ? "cold" : "unknown",
  });

  const cross = CROSS_ENTITY_SIGNALS.filter(([, pattern]) => pattern.test(text)).map(([name]) => name);
  if (cross.length > 0) {
    const supported = files.length === 0 || files.some((file) => LANGUAGE_BY_EXTENSION[path.extname(file).toLowerCase()]);
    if (!supported) return route("native", "high", "repository has no supported source files", ["unsupported-repository", ...cross]);
    return route("structural", "high", "cross-entity work is explicit", cross);
  }
  if (DATA_ARTIFACT.test(text) && !SOURCE_TASK.test(text)) {
    return route("native", "high", "data/artifact task has no source-change signal", ["data-artifact"]);
  }
  if (ENVIRONMENT_TASK.test(text) && !SOURCE_TASK.test(text)) {
    return route("native", "high", "environment task has no source-change signal", ["environment"]);
  }
  if (EXPLICIT_SOURCE_PATH.test(text)) {
    return route("native", "high", "source edit is explicitly localized", ["explicit-source-path"]);
  }
  if (SOURCE_TASK.test(text)) {
    const supported = files.length === 0 || files.some((file) => LANGUAGE_BY_EXTENSION[path.extname(file).toLowerCase()]);
    if (supported) return route("structural", "medium", "source task requires repository discovery", ["unlocalized-source"]);
  }
  return route("native", "medium", "structural startup is not justified by available evidence", []);
}

const LANGUAGE_BY_EXTENSION: Record<string, { language: string; mutation: "high" | "medium" | "guarded" }> = {
  ".ts": { language: "typescript", mutation: "high" }, ".tsx": { language: "typescript", mutation: "high" },
  ".js": { language: "javascript", mutation: "high" }, ".jsx": { language: "javascript", mutation: "high" },
  ".mjs": { language: "javascript", mutation: "high" }, ".cjs": { language: "javascript", mutation: "high" },
  ".py": { language: "python", mutation: "high" }, ".go": { language: "go", mutation: "high" },
  ".rs": { language: "rust", mutation: "high" },
  ".java": { language: "java", mutation: "medium" }, ".kt": { language: "kotlin", mutation: "medium" },
  ".kts": { language: "kotlin", mutation: "medium" }, ".cs": { language: "csharp", mutation: "medium" },
  ".rb": { language: "ruby", mutation: "medium" }, ".php": { language: "php", mutation: "medium" },
  ".swift": { language: "swift", mutation: "medium" },
  ".c": { language: "c", mutation: "guarded" }, ".h": { language: "cpp", mutation: "guarded" },
  ".cc": { language: "cpp", mutation: "guarded" }, ".cpp": { language: "cpp", mutation: "guarded" },
  ".cxx": { language: "cpp", mutation: "guarded" }, ".hpp": { language: "cpp", mutation: "guarded" },
  ".scala": { language: "scala", mutation: "guarded" }, ".ex": { language: "elixir", mutation: "guarded" },
  ".exs": { language: "elixir", mutation: "guarded" }, ".dart": { language: "dart", mutation: "guarded" },
  ".zig": { language: "zig", mutation: "guarded" }, ".hs": { language: "haskell", mutation: "guarded" },
  ".ml": { language: "ocaml", mutation: "guarded" }, ".mli": { language: "ocaml", mutation: "guarded" },
  ".clj": { language: "clojure", mutation: "guarded" }, ".cljs": { language: "clojure", mutation: "guarded" },
  ".cljc": { language: "clojure", mutation: "guarded" }, ".edn": { language: "clojure", mutation: "guarded" },
  ".lua": { language: "lua", mutation: "guarded" }, ".d": { language: "d", mutation: "guarded" },
  ".elm": { language: "elm", mutation: "guarded" }, ".nix": { language: "nix", mutation: "guarded" },
  ".f": { language: "fortran", mutation: "guarded" }, ".f90": { language: "fortran", mutation: "guarded" },
  ".f95": { language: "fortran", mutation: "guarded" }, ".f03": { language: "fortran", mutation: "guarded" },
  ".f08": { language: "fortran", mutation: "guarded" }, ".pl": { language: "perl", mutation: "guarded" },
  ".pm": { language: "perl", mutation: "guarded" }, ".bsl": { language: "bsl", mutation: "guarded" },
  ".os": { language: "bsl", mutation: "guarded" },
  ".sh": { language: "bash", mutation: "guarded" }, ".fish": { language: "fish", mutation: "guarded" },
  ".sql": { language: "sql", mutation: "guarded" }, ".psql": { language: "sql", mutation: "guarded" },
  ".tf": { language: "hcl", mutation: "guarded" }, ".hcl": { language: "hcl", mutation: "guarded" },
  ".xml": { language: "xml", mutation: "guarded" },
};

export function describeRepositoryCapabilities(
  files: string[],
  runner: { kind: string; typecheckCmd?: string[]; testCmd?: string[] } | null,
  task = "",
) {
  const counts = new Map<string, { files: number; mutation: "high" | "medium" | "guarded" }>();
  for (const file of files) {
    const extension = path.extname(file).toLowerCase();
    const capability = LANGUAGE_BY_EXTENSION[extension];
    if (!capability) continue;
    const current = counts.get(capability.language) ?? { files: 0, mutation: capability.mutation };
    current.files++;
    counts.set(capability.language, current);
  }
  const languages = [...counts].map(([language, value]) => ({ language, ...value }))
    .sort((a, b) => b.files - a.files || a.language.localeCompare(b.language));
  const lowerTask = task.toLowerCase();
  const environmentSignals = /\b(?:install|configure|container|docker|service|daemon|network|permission|filesystem|system package)\b/;
  const dataSignals = /\b(?:jsonl|csv|aggregate|dataset|artifact|report|calculate|transform data|output file)\b/;
  const sourceSignals = /\b(?:bug|function|method|class|api|implementation|refactor|rename|caller|compile|typecheck|test failure)\b/;
  const taskKind = environmentSignals.test(lowerTask) && !sourceSignals.test(lowerTask)
    ? "environment"
    : dataSignals.test(lowerTask) && !sourceSignals.test(lowerTask)
      ? "data"
      : task.length > 0
        ? "source"
        : "unknown";
  const dominantMutation = languages[0]?.mutation ?? "guarded";
  const validationEcosystems = new Set<string>();
  for (const file of files) {
    const base = path.posix.basename(file);
    if (base === "Cargo.toml") validationEcosystems.add("cargo");
    else if (base === "go.mod") validationEcosystems.add("go");
    else if (base === "package.json") validationEcosystems.add("npm");
    else if (["pyproject.toml", "pytest.ini", "setup.cfg"].includes(base)) validationEcosystems.add("pytest");
    else if (base === "pom.xml") validationEcosystems.add("maven");
    else if (["build.gradle", "build.gradle.kts"].includes(base)) validationEcosystems.add("gradle");
    else if (base.endsWith(".sln") || base.endsWith(".csproj")) validationEcosystems.add("dotnet");
    else if (base === "Package.swift") validationEcosystems.add("swift");
    else if (base === "composer.json") validationEcosystems.add("composer");
    else if (["Gemfile", "Rakefile", ".rspec"].includes(base)) validationEcosystems.add("ruby");
    else if (base === "Makefile") validationEcosystems.add("make");
  }
  const ecosystems = [...validationEcosystems].sort();
  return {
    task_kind: taskKind,
    languages,
    dominant_mutation_confidence: dominantMutation,
    validation: runner ? {
      available: true,
      runner: runner.kind,
      typecheck: runner.typecheckCmd?.join(" ") ?? null,
      test: runner.testCmd?.join(" ") ?? null,
      ecosystems,
      scope: ecosystems.length > 1 ? "ambiguous" : "repository",
    } : { available: false, runner: null, typecheck: null, test: null, ecosystems, scope: ecosystems.length > 1 ? "ambiguous" : "unavailable" },
    semantic_transaction_recommended: taskKind !== "environment" && taskKind !== "data" && languages.length > 0,
  };
}

export function describePlanCoverage({ definitions, requestedNames, resolvedNames, matches }: PlanCoverageInput) {
  const definitionCount = definitions.length;
  const requestedCount = requestedNames.length;
  const resolvedCount = resolvedNames.size;
  const matchHits = (matches?.results ?? []).reduce(
    (total, group) => total + (group.hits?.length ?? 0),
    0,
  );
  const unresolvedCount = Math.max(0, requestedCount - resolvedCount);
  const evidenceCount = definitionCount + matchHits;
  const status = evidenceCount === 0
    ? "empty"
    : unresolvedCount > 0
      ? "partial"
      : "complete";
  return {
    status,
    recommended_mode: status === "complete" ? "transaction" : status === "partial" ? "hybrid" : "native_fallback",
    recovery_allowed: status === "empty",
    requested_entities: requestedCount,
    resolved_entities: resolvedCount,
    unresolved_entities: unresolvedCount,
    hydrated_entities: definitionCount,
    structural_matches: matchHits,
    capabilities: {
      structural_discovery: evidenceCount > 0 ? "available" : "unavailable",
      entity_replacement: definitionCount > 0 ? "available" : "unavailable",
      exact_text_edit: "available",
      validation: "runtime_detected",
    },
  };
}
