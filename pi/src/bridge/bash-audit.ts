import { isCodeFilePath, auditWriteCommand } from "./write-audit.ts";

/**
 * Pure classifier for pi-sem's bash audit wrapper (no process spawning).
 *
 * Deliberately conservative: rules only ever match a statement's LEADING
 * command word(s) (after stripping an env-var prefix and normalizing an
 * absolute-path binary to its basename), a single level of $(...)/backtick
 * substitution, or (for "write") a redirect/verb pattern anywhere in the
 * statement, never scan mid-command generally beyond that, so a false
 * positive costs at most one audit-log entry by default (and one refusal
 * when PI_SEM_STRICT=1).
 *
 * NOT caught (deliberately, to keep this a "few regexes" classifier rather
 * than a shell parser -- a real adversary can defeat every rule below):
 *  - nested command substitution, e.g. `$(echo $(cat f))` -- only one level
 *    of $(...)/`...` is inspected
 *  - a variable holding a command name, e.g. `CMD=cat; $CMD f.txt`
 *  - `xargs cat`, `find . -exec cat {} \;`, or any command that itself
 *    invokes a read verb as a sub-process argument rather than via -c/-e
 *  - a read/write verb inside a heredoc BODY's own text content (the body
 *    itself is stripped before classification -- see stripHeredocsAndHereStrings
 *    -- but a verb appearing only as literal text inside that body, e.g. the
 *    word "cat" mentioned in a comment being written, was never a command
 *    anyway and correctly isn't classified)
 *  - `python3 script.py` / `node script.js` running arbitrary file reads or
 *    writes internally -- only the inline `-c`/`-e` forms are classified
 *  - a `python -c '...'`/`node -e '...'` script's own file-write calls
 *    (e.g. `open(path, "w")`) are not parsed -- the whole invocation is
 *    already flagged "indirect" regardless of what the embedded script
 *    does, which is as far as this classifier goes without a real
 *    interpreter for that language
 *  - shell aliases, functions, or `env` used as an indirection wrapper
 *    (`env cat f.txt` is not normalized the way `/bin/cat` is)
 *  - a `sed` in-place flag written via a variable (e.g. `FLAG=-i; sed $FLAG
 *    ...`) -- both spellings recognized literally in the statement text
 *    (`-i`, `--in-place`) are caught, but an indirect flag is not
 *  - a flag that consumes a separate-token VALUE ahead of the trigger flag,
 *    e.g. `node -r foo -e "..."` -- the leading-flag scan stops at the
 *    first token that doesn't itself start with `-` (here, `foo`), so a
 *    trigger flag appearing after such a value-bearing flag is not seen.
 *    A flag that is merely ANOTHER leading `-`/`--` token (with no
 *    separate-token value of its own), e.g. `bash --posix -c "..."`, does
 *    NOT break the scan and so is correctly still caught.
 *  - a backslash-escaped command (`\cat f.txt`, defeats shell aliasing),
 *    the `command`/`builtin` prefix (`command cat f.txt`), bare `&`
 *    backgrounding as a statement separator (`sleep 1 & cat f.txt`),
 *    subshell parens (`( cat f.txt )`), or piping into a second-stage
 *    shell (`echo 'cat f.txt' | sh`) -- none of these are normalized or
 *    split on; each defeats every rule below (a later review's findings,
 *    disclosed, not yet fixed)
 *  - `base64 -d | sh`-style decode-then-execute chains -- no verb rule for
 *    decode utilities, and only one level of substitution is inspected
 *    (ox-review-3 item 8 -- disclosed, not yet fixed)
 *  - a write's TARGET PATH extraction (below) is best-effort: quoted paths
 *    with embedded spaces, `2>&1`-style fd duplication (deliberately
 *    excluded, see REDIRECT_PATTERN), and multi-redirect statements only
 *    ever resolve to the LAST redirect target found
 */

export interface BashAuditMatch {
  ruleId: "read" | "search" | "find" | "list" | "indirect" | "write";
  statement: string;
  suggestedTool: string;
  /** Only set for ruleId "write" when a target path could be extracted (best-effort; see header). */
  path?: string;
  /** Only set alongside `path` -- whether that path's extension looks like a code file (see write-audit.ts's isCodeFilePath). */
  isCodeFile?: boolean;
}

/** Statements are split on these operators, then trimmed. `||` must come before `|`. */
const STATEMENT_SPLIT = /\|\||&&|;|\||\n/;

/** Leading `KEY=value ` assignments (`FOO=1 BAR=2 cmd`), stripped before matching. */
const ENV_PREFIX = /^(?:[A-Za-z_][A-Za-z0-9_]*=(?:'[^']*'|"[^"]*"|\S*)\s+)+/;

/** A single level of `$(...)` or `` `...` `` substitution content. */
const SUBSTITUTION = /\$\(([^()]*)\)|`([^`]*)`/g;

/**
 * A `>`/`>>` redirect NOT preceded by `-` (excludes `->`), NOT followed by
 * `=` (excludes `>=`), and NOT followed by `&` (excludes `2>&1`/`1>&2` fd
 * duplication, which never names a real file target). `&> file`/`&>> file`
 * (redirect both streams) IS matched -- the `&` sits before the `>`, which
 * this pattern doesn't look at, so the `>` itself still matches normally.
 */
const REDIRECT_PATTERN = /(?<!-)>{1,2}(?!=)(?!&)\s*(['"]?)([^\s'"]+)\1/g;

/**
 * Strips heredoc bodies and here-string tokens from the WHOLE command text,
 * before statement splitting -- both constructs can contain characters
 * STATEMENT_SPLIT treats as separators (`;`, `&&`, `|`, newlines), so this
 * must run first or a heredoc body gets shredded into bogus "statements".
 *
 * Here-strings (`<<< word` / `<<< 'quoted'` / `<<< "quoted"`) are a single
 * token, removed outright. Heredocs (`<<WORD`, `<<-WORD`, `<<~WORD`, with an
 * optionally quoted delimiter) keep whatever follows the marker on the SAME
 * line (e.g. `> out.txt` in `cat <<EOF > out.txt`) and strip everything from
 * the next line through the terminator line (a line consisting of just the
 * delimiter, optionally indented for `<<-`).
 *
 * This closes ox-review-3 finding #18 item 7: the previous approach (skip
 * the WHOLE statement if it merely contained the substring "<<") meant
 * appending `<<< x` to ANY statement -- `head -100 file <<< x`, `cat foo.ts
 * <<< x` -- evaded classification entirely, a universal evasion suffix
 * disguised as a heredoc-write guard.
 */
function stripHeredocsAndHereStrings(command: string): string {
  let result = command.replace(/<<<\s*(?:'[^']*'|"[^"]*"|\S+)/g, "");
  result = result.replace(/<<[-~]?(['"]?)(\w+)\1([^\n]*)\n[\s\S]*?^[ \t]*\2[ \t]*$\n?/gm, "$3");
  // Cosmetic only: stripping a token can leave a double space behind
  // (`cat <<EOF > out.txt` -> `cat  > out.txt`) -- collapse horizontal
  // whitespace runs, never touching newlines (STATEMENT_SPLIT still needs
  // those as statement separators).
  return result.replace(/[ \t]{2,}/g, " ");
}

function stripEnvPrefix(statement: string): string {
  return statement.replace(ENV_PREFIX, "");
}

/** `/bin/cat foo.txt` -> `cat foo.txt`; `env`-based indirection is NOT unwrapped (see header). */
function normalizeLeadingBinary(statement: string): string {
  return statement.replace(/^\/(?:[^\s/]+\/)*([^\s/]+)/, "$1");
}

/** Strip env-prefix then normalize an absolute-path binary, for matching only (reported statement stays raw). */
function normalizeForMatch(statement: string): string {
  return normalizeLeadingBinary(stripEnvPrefix(statement));
}

/**
 * Recognizes sed's in-place flag in EITHER spelling: the short `-i`
 * (optionally with an attached suffix, `-i.bak`) or the GNU long form
 * `--in-place` (optionally `--in-place=.bak`). Previously only `-i` was
 * recognized, which meant `sed --in-place ...` fell through to the plain-
 * sed READ rule -- a MISCLASSIFICATION (reported as a read) rather than
 * merely uncaught, since neither the write rule's guard nor the read
 * rule's negated guard recognized the long form (ox-review-3 follow-up).
 */
function hasSedInPlaceFlag(statement: string): boolean {
  return /(^|\s)-i\b/.test(statement) || /(^|\s)--in-place(\b|=)/.test(statement);
}

/**
 * True when `rest` (the tokens after the interpreter name) carries the
 * trigger letter (`c` for sh/bash/zsh/dash/python, `e` for node) in a
 * leading run of flag tokens -- either combined into one cluster
 * (`-lc`) or split across separate tokens (`-l -c`). Scans tokens from the
 * start and stops at the first non-flag token (the interpreter's actual
 * argument, e.g. the `-c` STRING itself, must not be scanned for stray
 * letters).
 */
function hasIndirectTriggerFlag(rest: string, triggerLetter: string): boolean {
  const tokens = rest.trim().split(/\s+/);
  for (const token of tokens) {
    if (!token.startsWith("-")) break;
    if (token.slice(1).includes(triggerLetter)) return true;
  }
  return false;
}

/**
 * `sh|bash|zsh|dash|python(3)|node` followed by a leading run of flag
 * tokens carrying the trigger letter -- combined cluster (`bash -lc "..."`)
 * or split across separate tokens (`bash -l -c "..."`, `sh -e -c "..."`).
 * The split form was previously a documented gap (ox-review-3 follow-up):
 * `bash -lc` was caught but `bash -l -c` was not, even though both invoke
 * an opaque interpreted string identically.
 */
function classifyIndirectShell(normalized: string): boolean {
  const match = /^(sh|bash|zsh|dash|python3?|node)\s*(.*)$/.exec(normalized);
  if (!match) return false;
  const interpreter = match[1] ?? "";
  const rest = match[2] ?? "";
  const trigger = interpreter === "node" ? "e" : "c";
  return hasIndirectTriggerFlag(rest, trigger);
}

/**
 * Best-effort write TARGET extraction for a statement already matched by
 * one of the "write" rules below. Not attempted for anything else -- this
 * is deliberately narrow, not a general shell-argument parser.
 */
function extractWriteTargetPath(statement: string): string | undefined {
  REDIRECT_PATTERN.lastIndex = 0;
  const redirectMatches = [...statement.matchAll(REDIRECT_PATTERN)];
  if (redirectMatches.length > 0) {
    // Last redirect in the statement wins -- the common single-redirect
    // case has exactly one match; a multi-redirect statement's earlier
    // targets are a documented gap (see module header).
    return redirectMatches[redirectMatches.length - 1]?.[2];
  }
  const normalized = normalizeForMatch(statement);
  if (/^sed\b/.test(normalized) && hasSedInPlaceFlag(statement)) {
    const tokens = statement.trim().split(/\s+/);
    return tokens[tokens.length - 1];
  }
  if (/^tee\b/.test(normalized)) {
    const tokens = statement.trim().split(/\s+/).slice(1).filter((t) => !t.startsWith("-"));
    return tokens[0];
  }
  if (/^(cp|mv|truncate)\b/.test(normalized)) {
    // .slice(1) drops the leading command word itself -- without it, a
    // flags-only invocation with no real path argument (`truncate --help`)
    // would fall through to returning "truncate" itself as the "path"
    // once every flag token got filtered out.
    const tokens = statement.trim().split(/\s+/).slice(1).filter((t) => !t.startsWith("-"));
    return tokens[tokens.length - 1];
  }
  return undefined;
}

interface ClassificationRule {
  ruleId: BashAuditMatch["ruleId"];
  pattern: RegExp;
  suggestedTool: string;
  /** Extra predicate; when it returns false the rule does not apply. Receives the RAW (unnormalized) statement. */
  guard?: (statement: string) => boolean;
}

// Order matters: first match wins, one match max per statement. WRITE rules
// come first so a statement that's both read-shaped and write-shaped
// (`cat <<EOF > out.txt`, which after heredoc-stripping becomes
// `cat > out.txt`) is classified by its write side -- the security-relevant
// half -- not misreported as a plain read.
const RULES: ClassificationRule[] = [
  {
    ruleId: "write",
    // `cmd > file` / `cmd >> file`, any leading command -- ox-review-3
    // finding #19: bash-side writes (redirection, sed -i, tee, cp, mv,
    // truncate) were never classified at all, so PI_SEM_STRICT's write
    // protection was fully bypassable through bash.
    pattern: REDIRECT_PATTERN,
    suggestedTool: "weave_edit for an existing entity, or the write tool for a new file, instead of a bash redirect",
  },
  {
    ruleId: "write",
    // The positive counterpart of the read rule's "sed without -i" guard
    // below -- WITH -i (short OR --in-place long form), sed genuinely
    // edits the file in place.
    guard: hasSedInPlaceFlag,
    pattern: /^sed\b/,
    suggestedTool: "weave_edit for an existing entity instead of bash sed -i/--in-place",
  },
  {
    ruleId: "write",
    pattern: /^(tee|cp|mv|truncate)\b/,
    suggestedTool: "weave_edit for an existing entity, or the write tool for a new file, instead of bash tee/cp/mv/truncate",
  },
  {
    ruleId: "read",
    // No heredoc/redirect guard needed here anymore: heredoc bodies are
    // stripped from the WHOLE command before statement splitting (see
    // stripHeredocsAndHereStrings), and a real `>`/`>>` redirect is caught
    // by the "write" rules above, which run first (first-match-wins) --
    // `cat file > out.txt` is classified "write", never reaching this rule.
    pattern: /^(cat|head|tail|less|more)\b/,
    suggestedTool: "sem_read (or sem_context) instead of bash cat/head/tail/less",
  },
  {
    ruleId: "read",
    // Plain `sed` (an edit) must NOT match; only sed WITHOUT an in-place
    // flag (short -i OR long --in-place) is a read (covers `sed -n ...` and
    // any other read-only use).
    guard: (statement) => !hasSedInPlaceFlag(statement),
    pattern: /^sed\b/,
    suggestedTool: "sem_read (or sem_context) instead of bash sed (no -i)",
  },
  {
    ruleId: "read",
    pattern: /^awk\b/,
    suggestedTool: "sem_read (or sem_context) instead of bash awk",
  },
  {
    ruleId: "search",
    pattern: /^(grep|rg|ag)\b/,
    suggestedTool: "sem_outline's text= (or sem_grep once available) instead of bash grep/rg/ag",
  },
  {
    ruleId: "find",
    pattern: /^(find|fd)\b/,
    suggestedTool: "sem_find (once available) instead of bash find/fd",
  },
  {
    ruleId: "list",
    pattern: /^(ls|tree)\b/,
    suggestedTool: "sem_outline (or sem_find once available) instead of bash ls/tree",
  },
  {
    ruleId: "indirect",
    // `sh -c "..."` / `bash -c "..."` / `python(3) -c "..."` / `node -e "..."`
    // run an opaque interpreted string -- classified as bypass-shaped
    // regardless of what the string contains (auditing its contents would
    // require actually parsing that language). The trigger letter (`c` for
    // sh/bash/zsh/dash/python, `e` for node) is matched whether it appears
    // in a combined CLUSTER (`bash -lc "..."`, `sh -ic "..."`, `bash -xc
    // "..."`) or split across separate leading flag tokens (`bash -l -c
    // "..."`, `sh -e -c "..."` -- previously a documented gap, ox-review-3
    // follow-up), so `bash -lc` and `bash -l -c` are treated identically.
    // A false positive here only ever costs one audit-log entry, so
    // matching any leading flag run CONTAINING the trigger letter is the
    // conservative direction, even though a cluster like `-norc` (not a
    // real bash short-flag combination, but syntactically matches) would
    // also trip it.
    pattern: /^(sh|bash|zsh|dash|python3?|node)\b/,
    guard: (statement) => classifyIndirectShell(normalizeForMatch(statement)),
    suggestedTool: "an inline interpreter -c/-e invocation can run arbitrary reads/writes opaquely -- prefer sem_read/sem_outline/weave_edit directly",
  },
];

function attachWriteFields(match: BashAuditMatch, statement: string): BashAuditMatch {
  if (match.ruleId !== "write") return match;
  const path = extractWriteTargetPath(statement);
  if (path === undefined) return match;
  return { ...match, path, isCodeFile: isCodeFilePath(path) };
}

function classifyLeadingCommand(rawStatement: string): BashAuditMatch | undefined {
  const normalized = normalizeForMatch(rawStatement);
  const rule = RULES.find((r) => r.pattern.test(normalized) && (r.guard?.(rawStatement) ?? true));
  if (!rule) return undefined;
  return attachWriteFields({ ruleId: rule.ruleId, statement: rawStatement, suggestedTool: rule.suggestedTool }, rawStatement);
}

/** Only a rule's own bare leading-command patterns are tried here -- not the "indirect" -c/-e rule itself (a substitution nested inside another -c/-e is not chased). */
const SUBSTITUTION_INNER_RULES = RULES.filter((r) => r.ruleId !== "indirect");

function classifySubstitutions(rawStatement: string): BashAuditMatch | undefined {
  SUBSTITUTION.lastIndex = 0;
  let match: RegExpExecArray | null;
  while ((match = SUBSTITUTION.exec(rawStatement))) {
    const inner = (match[1] ?? match[2] ?? "").trim();
    if (!inner) continue;
    const normalizedInner = normalizeForMatch(inner);
    const rule = SUBSTITUTION_INNER_RULES.find((r) => r.pattern.test(normalizedInner) && (r.guard?.(inner) ?? true));
    if (!rule) continue;
    return {
      ruleId: "indirect",
      statement: rawStatement,
      suggestedTool: `command substitution runs a ${rule.ruleId} (${inner.split(/\s+/)[0]}) whose output is used elsewhere -- ${rule.suggestedTool}`,
    };
  }
  return undefined;
}

export function classifyBashCommand(command: string): BashAuditMatch[] {
  const stripped = stripHeredocsAndHereStrings(command);
  const matches: BashAuditMatch[] = [];
  for (const rawStatement of stripped.split(STATEMENT_SPLIT)) {
    const statement = rawStatement.trim();
    if (statement.length === 0) continue;
    const match = classifyLeadingCommand(statement) ?? classifySubstitutions(statement);
    if (match) matches.push(match);
  }
  return matches;
}

export interface BashAuditDecision {
  matches: BashAuditMatch[];
  refuse: boolean;
  refusalMessage?: string;
}

/**
 * `resolveTargetExists` lets the caller (extensions/pi-sem.ts, which has
 * real fs access) answer "does this write's target already exist" for a
 * "write"-classified match with an extracted `path` -- mirroring
 * write-audit.ts's own pure-classifier-plus-caller-resolved-existence
 * split. Omitted (the default for every existing call site and test), a
 * "write" match never auto-refuses under strict mode -- audit-only, same
 * as a match with no extractable path at all. When provided and the write
 * targets an existing code file, the refusal is delegated to
 * write-audit.ts's own `auditWriteCommand` so the message is the SAME text
 * the builtin `write` tool wrapper already gives (ox-review-3 finding #19's
 * explicit ask), not a separately-maintained copy that can drift.
 */
export function auditBashCommand(
  command: string,
  strict: boolean,
  resolveTargetExists?: (path: string) => boolean,
): BashAuditDecision {
  const matches = classifyBashCommand(command);
  if (matches.length === 0 || !strict) return { matches, refuse: false };

  const first = matches[0];
  if (first === undefined) return { matches, refuse: false };

  if (first.ruleId === "write") {
    if (first.path === undefined) return { matches, refuse: false };
    const targetExists = resolveTargetExists?.(first.path) ?? false;
    const writeDecision = auditWriteCommand(first.path, 0, targetExists, strict);
    if (!writeDecision.refuse) return { matches, refuse: false };
    // Prefix with the offending bash statement for context, then reuse the
    // builtin write wrapper's own refusalMessage's core guidance text
    // verbatim (its own leading "pi-sem: " is stripped so the two prefixes
    // don't double up) -- this is the "same message as the write wrapper"
    // ox-review-3 asked for, guaranteed by sharing the string, not by
    // separately maintaining matching prose.
    return {
      matches,
      refuse: true,
      refusalMessage: `pi-sem: "${first.statement}" -- ${writeDecision.refusalMessage?.replace(/^pi-sem: /, "")}`,
    };
  }

  return {
    matches,
    refuse: true,
    refusalMessage:
      `pi-sem: "${first.statement}" looks like a file ${first.ruleId}` +
      ` -- use ${first.suggestedTool}.` +
      ` (PI_SEM_STRICT is set; unset it or set PI_SEM_STRICT=0 to audit instead of refuse.)`,
  };
}
