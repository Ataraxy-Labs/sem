import { extname } from "node:path";

/**
 * Pure classifier for pi-sem's write audit wrapper (no process spawning, no
 * fs access -- callers resolve `targetExists` themselves so this stays
 * trivially unit-testable, matching bash-audit.ts's shape).
 */

export interface WriteAuditEntry {
  path: string;
  bytes: number;
  isCodeFile: boolean;
}

/**
 * Extensions matching sem-core's tree-sitter grammars (the `grammar-all`
 * feature set), used only as a cheap, LOCAL heuristic for "is this a code
 * file weave_edit/sem_* could operate on" -- this is NOT a call into `sem`
 * itself. Conservative and documented, not authoritative: asking `sem`
 * directly would be the real answer, but this audit wrapper deliberately
 * avoids that (keeps every write synchronous and cheap; a wrong guess costs
 * at most an audit-log entry under default/non-strict mode).
 *
 * Deliberately NOT exhaustive:
 *  - does not distinguish `.h` (C vs C++) or disambiguate any other
 *    multi-language extension
 *  - does not handle extensionless files (Makefile, Dockerfile) even
 *    though some of those have tree-sitter grammars sem uses
 *  - treats any matching extension as "code" even for generated/vendored
 *    files sem itself would likely skip (node_modules, .min.js, etc.) --
 *    this classifier never looks at the path's directory, only its
 *    extension
 */
const CODE_EXTENSIONS = new Set([
  ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs",
  ".py", ".go", ".rs", ".java",
  ".c", ".h", ".cpp", ".cc", ".cxx", ".hpp", ".hxx",
  ".rb", ".cs", ".php",
  ".f90", ".f", ".f95",
  ".swift", ".ex", ".exs",
  ".sh", ".bash",
  ".tf", ".hcl",
  ".kt", ".kts",
  ".xml", ".dart",
  ".pl", ".pm",
  ".ml", ".mli",
  ".scala", ".zig", ".nix",
  ".svelte", ".erb",
  ".hs", ".elm",
  ".clj", ".cljs", ".cljc", ".edn",
  ".d", ".sql", ".lua", ".fish",
]);

export function isCodeFilePath(path: string): boolean {
  return CODE_EXTENSIONS.has(extname(path).toLowerCase());
}

export interface WriteAuditDecision {
  entry: WriteAuditEntry;
  refuse: boolean;
  refusalMessage?: string;
}

/**
 * `targetExists` and `contentBytes` are resolved by the caller (fs access
 * and byte-length live in extensions/pi-sem.ts, which has the real cwd).
 * Refuses only when ALL THREE hold: strict mode, the target is a code
 * file, and the target already exists -- a write to a brand-new file is
 * always allowed, code file or not.
 */
export function auditWriteCommand(
  path: string,
  contentBytes: number,
  targetExists: boolean,
  strict: boolean,
): WriteAuditDecision {
  const isCodeFile = isCodeFilePath(path);
  const entry: WriteAuditEntry = { path, bytes: contentBytes, isCodeFile };

  if (!strict || !isCodeFile || !targetExists) {
    return { entry, refuse: false };
  }

  return {
    entry,
    refuse: true,
    refusalMessage:
      "pi-sem: use weave_edit for existing entities; write is for new files." +
      " (PI_SEM_STRICT is set; unset it or set PI_SEM_STRICT=0 to audit instead of refuse.)",
  };
}
