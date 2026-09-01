import vm from "node:vm";

/**
 * Confines an arbitrary JS string to a `node:vm` context whose only globals
 * are `sem` (whatever capability-bearing object the caller hands in) and a
 * `console` with a captured `log`. See DESIGN.md's "Sandbox contract".
 *
 * The hard part, proven empirically (not assumed) before writing this file:
 * a HOST-REALM function object exposed directly into a vm context still
 * carries `.constructor` -> the host's real `Function` intrinsic, and
 * `codeGeneration: {strings:false}` on the *sandbox's* context does NOT
 * block a call through that chain (the intrinsic being invoked belongs to
 * the unrestricted host context, not this one). Verified with a throwaway
 * probe: a plain exposed function leaked `process.env` via
 * `sem.x.constructor.constructor(str)()` even with codeGeneration disabled.
 *
 * The fix that DOES close it (also verified empirically): never expose a
 * host function object directly. Instead, for every capability, compile a
 * trampoline function *inside* the target vm context via
 * `vm.compileFunction(..., {parsingContext: context, contextExtensions})`
 * -- its own `.constructor` chain is then context-native, so
 * `codeGeneration:{strings:false}` correctly blocks
 * `trampoline.constructor.constructor(str)`. The trampoline explicitly
 * constructs a context-native `new Promise(...)` and `new Error(...)`
 * around the host call's result/rejection (never returning `.then()`'s own
 * result, which inherits the host promise's species) and round-trips the
 * resolved value through `JSON.stringify`/`JSON.parse` -- both run as
 * context-native code inside the trampoline, so the value handed back to
 * the sandboxed script is rebuilt entirely out of this context's own
 * intrinsics, with zero leftover host-realm prototype anywhere in it.
 *
 * A second, unrelated vector surfaced the same way: with an ARROW-function
 * wrapper (`(async () => {...})()`), top-level `this` inside sandboxed code
 * lexically inherits the vm context's own exotic global-object template --
 * and empirically, `this.constructor` reached an intrinsic NOT blocked by
 * codeGeneration:{strings:false} and NOT fixed by patching any prototype in
 * `this`'s own chain (its real resolution didn't go through the chain
 * `Object.getPrototypeOf(this)` exposes to mutation). The fix: wrap in a
 * strict-mode, non-arrow `async function` IIFE instead -- called with no
 * receiver under `"use strict"`, top-level `this` is a hard `undefined`,
 * and `undefined.constructor` throws before reaching anything at all.
 *
 * Known, disclosed gap (not claimed as closed): a script that busy-loops
 * synchronously *after* its first `await` cannot be preempted from this
 * thread by any mechanism here -- `vm.Script`'s own `timeout` only bounds
 * the initial synchronous burst, and a `setTimeout`-based race can't fire
 * while the same single thread is pegged by a synchronous loop. Closing
 * that fully needs a separate, terminable worker thread; out of scope for
 * this pass (not exercised by a RED test -- the async-hang test here uses
 * an unresolved promise, not a busy loop, which the outer race does catch).
 *
 * A THIRD vector, closed in this pass (see `sanitizeReturnValue` below):
 * the value a script *returns* may carry a sandbox-authored `toJSON()` --
 * calling that via host-realm `JSON.stringify(value)` after the race above
 * resolves is exactly the disclosed gap's shape (a synchronous busy loop,
 * now hiding inside a serializer call instead of the script body), except
 * it ran on the HOST thread with *no* timeout bound at all, not even the
 * "only the initial burst" one. Proven empirically (throwaway vm probes)
 * before writing this: `vm.Script`'s native `timeout` DOES forcibly
 * interrupt a flat, non-async script that calls a busy `toJSON()` -- e.g.
 * `(function(){ return JSON.stringify(v) })()` -- but that protection
 * evaporates the instant the call is reached through *any* await/`.then()`
 * boundary, even one with zero real host-side async work (a bare
 * `Promise.resolve()` microtask is enough to fall outside the
 * timeout-watched region -- confirmed by direct measurement, not
 * assumption). So the fix is to run stringification as its OWN separate,
 * synchronous `runInContext` call in the *same* context the value belongs
 * to, chained onto nothing -- never appended onto the raced
 * script-execution promise via `await`/`.then()`, which would silently
 * lose the protection. This closes the vector for exactly the case the
 * disclosed gap above doesn't already cover (no preceding real await);
 * a value whose busy `toJSON()` is only reached *after* the script's own
 * code crosses a real host-boundary `await` remains part of that same
 * disclosed, pre-existing limitation -- not newly introduced here, and
 * not made worse by this fix.
 */

export interface SandboxOptions {
  /** The object exposed as the sandbox's only capability-bearing global, `sem`. */
  sem: unknown;
  /** Wall-clock budget in ms. Default 60_000. */
  timeoutMs?: number;
  /** Max sem.* calls per run, `log` exempt. Default 200. */
  maxCalls?: number;
  /** ~token cap on captured output (console.log + sem.log), ~4 chars/token. Default 8000. */
  outputCapTokens?: number;
  /**
   * Shared with the SAME `sem` object's `SemApiDeps.cancellation` (the
   * caller constructs ONE `createRunCancellation()` per run and threads it
   * into both `buildSemApi` and here) -- `runInSandbox` revokes it the
   * instant its own wall-clock timeout fires, so every `sem.*` trampoline,
   * and api.ts's own write/edit mutators, refuse any FURTHER call from
   * that point on. Optional: a caller that doesn't pass one gets a fresh,
   * private cancellation cell (still closes the trampoline-level hole,
   * just without api.ts's own deps.cancellation seeing the SAME revoke).
   */
  cancellation?: RevocableRunCancellation;
}

export interface SandboxError {
  message: string;
  line?: number;
}

/** One `sem.*` call that actually reached the real host function (call-cap rejections, which never reach it, are not recorded here -- they're reflected in `callCount` only). */
export interface CallRecord {
  /** The `sem.*` property name called, e.g. "read", "edit". Never "log" -- that's exempt from tracking, same as from the call cap. */
  fn: string;
  ok: boolean;
  /** The host function's own thrown/rejected message, present iff !ok. */
  error?: string;
  /**
   * True iff this call's merge backstop actually PERFORMED a merge over
   * another agent's concurrent changes (edit/rename only; attached via
   * the SUB_CALLS protocol by api.ts, which is where "merge" has
   * meaning -- this file stays domain-agnostic). Absent otherwise.
   */
  merged?: boolean;
}

/**
 * One sem.* call that RAN TO COMPLETION, with the value it produced.
 *
 * P6 (2026-09-02 transcript study): every verb throws on a soft failure and
 * an uncaught throw used to discard the entire script's output, so 206
 * scripts across 147 of 327 runs (45%) lost ~518 already-completed calls to
 * one bad guess later in the script -- a tax that scaled with how well the
 * agent followed this tool's own "one script per turn" advice. Retaining
 * each completed call's value costs nothing (the value is already in memory,
 * already JSON-round-tripped by the trampoline) and makes every remaining
 * throw survivable.
 */
export interface CompletedCall {
  fn: string;
  value: unknown;
}

export interface SandboxResult {
  ok: boolean;
  output: string;
  truncated: boolean;
  callCount: number;
  /** Every sem.* call that reached the host function, in order, with its outcome -- the basis for tool.ts's apiCalls/edits reporting. */
  calls: CallRecord[];
  /**
   * How many sem.* call ATTEMPTS were refused specifically because the run
   * had already RESOLVED (revoked) by the time they reached the
   * trampoline's cancellation check -- see createRunCancellation.
   * `cancellation.revoke()` now fires on EVERY exit path (success, error,
   * AND timeout, not just timeout -- a fix-shape correction to
   * the authority-on-failure audit), so this counts a call refused for
   * ANY of those reasons, not only "refused after cancel due to timeout"
   * (its narrower, pre-fix meaning; renamed from `revokedCalls` to match).
   * Closes the CASCADING case: a SECOND sem.* call chained in a `.then()`
   * after a first, already-in-flight call settles now gets refused rather
   * than silently succeeding, because the cell is revoked by the time it
   * runs. Does NOT retroactively stop a call that had ALREADY passed this
   * check before revoke() fired -- see `pendingAtResolve` for that case.
   * Best-effort, not a complete tally: only attempts that happened to
   * resume before this function finished returning are counted here; a
   * zombie continuation that resumes later (after a real async wait) is
   * still refused when it does, just invisibly to this particular result.
   */
  refusedAfterResolve: number;
  /**
   * How many sem.* calls were STARTED (past the trampoline's cancellation/
   * call-cap checks, into the real host function) but had not yet settled
   * at the moment this run finished returning -- i.e. a script that never
   * awaited them (fire-and-forget: `sem.write(...).then(...)`) and itself
   * returned/resolved first. Authority-on-failure finding (see
   * test/codemode/authority-on-failure.test.ts): revoking on resolution
   * (see `refusedAfterResolve`) does NOT retroactively stop a call that
   * already passed the trampoline's check before revoke() fired -- that
   * call keeps running and its mutation lands for real, invisibly, after
   * "done" was already reported. This field (renamed from
   * `outstandingCalls` to match the fix-shape naming) is the
   * honest signal for exactly that case: a nonzero count means the
   * model's own report of success may still have real, unconfirmed side
   * effects landing after the fact. Best-effort like `refusedAfterResolve`:
   * a snapshot at return time, not a guarantee the count won't still
   * change by a microtask in either direction.
   */
  pendingAtResolve: number;
  /** Every sem.* call that COMPLETED, in order, with its own result -- see CompletedCall. Present on every exit path; the error paths are what it exists for. */
  completed: CompletedCall[];
  error?: SandboxError;
  value?: unknown;
}

/**
 * Opt-in protocol: a host function normally counts as exactly ONE
 * `CallRecord` in `calls[]` per script-level invocation (this file stays
 * domain-agnostic -- it never special-cases a function by name). But a
 * host function that itself performs N logically-separate sub-operations
 * in one call (api.ts's batch `edit(request[])`, one outcome per entity)
 * can attach N `CallRecord`s to its OWN return value as a non-enumerable,
 * symbol-keyed property -- this file records those instead of its usual
 * single generic one. Deliberately NOT a wrapper object around the return
 * value: the value a direct caller (a test calling `api.edit(...)`
 * directly, bypassing the sandbox entirely) gets back must stay byte-for-
 * byte what it always was. A symbol key is invisible to `JSON.stringify`
 * (never leaks into what the sandboxed script itself sees) and invisible
 * to `Object.keys`/spread/`for...in` (never disturbs a direct caller's own
 * use of the value either).
 */
export const SUB_CALLS: unique symbol = Symbol.for("sem_code.sandbox.subCalls");

function readSubCalls(value: unknown): CallRecord[] | undefined {
  if (typeof value !== "object" || value === null) return undefined;
  const sub = (value as Record<PropertyKey, unknown>)[SUB_CALLS];
  return Array.isArray(sub) ? (sub as CallRecord[]) : undefined;
}

/**
 * Closes the "zombie script" hole: when the
 * wall-clock timeout race wins, `runInSandbox` returns `{ok:false, timed
 * out}` immediately -- but the sandboxed script's OWN promise chain is
 * still pending and keeps running in the background (JS promises can't be
 * forcibly cancelled). Proven empirically before fixing: a script that
 * `await`s something slower than the timeout, then calls `sem.write(...)`,
 * lands that write on disk ~300ms AFTER the tool already reported "timed
 * out" to the caller -- a "cancelled" run that still mutates state.
 *
 * `RunCancellation` is a SingleWriterCell-shaped split: `revoke()` is the
 * writer (only `runInSandbox`'s own timeout handler calls it, the instant
 * the race resolves that way), `isRevoked()` is the read-only view every
 * `sem.*` trampoline checks FIRST -- before even the call-cap check --
 * and that api.ts's own long-running mutators (`write`/`edit`) can check
 * AGAIN immediately before their actual disk/coordination step, narrowing
 * (never fully eliminating -- no cooperative single-thread model can) the
 * window between "already committed to an in-flight host call" and
 * "timeout fires". Once revoked, every FUTURE sem.* call attempt throws
 * "run cancelled: timeout" instead of ever reaching its host function.
 */
export interface RunCancellation {
  readonly isRevoked: () => boolean;
}

interface RevocableRunCancellation extends RunCancellation {
  readonly revoke: () => void;
}

export function createRunCancellation(): RevocableRunCancellation {
  let revoked = false;
  return {
    isRevoked: () => revoked,
    revoke: () => {
      revoked = true;
    },
  };
}

const CANCELLED_MESSAGE = "run cancelled: this run has already finished (timed out, errored, or returned)";

const DEFAULT_TIMEOUT_MS = 60_000;
const DEFAULT_MAX_CALLS = 200;
const DEFAULT_OUTPUT_CAP_TOKENS = 8_000;
const CHARS_PER_TOKEN = 4;
const SANDBOX_FILENAME = "sandbox.js";
/** The wrapper adds exactly this many lines before the caller's own code -- see WRAPPER_PREFIX below. */
const WRAPPER_PREFIX_LINES = 2;

function isPlainCallable(value: unknown): value is (...args: unknown[]) => unknown {
  return typeof value === "function";
}

/**
 * Joins ALREADY-STRINGIFIED console.log/sem.log arguments. The actual
 * stringification now happens context-natively, inside the compiled
 * trampoline itself (see `compileVoidTrampoline`) -- this just joins the
 * resulting strings, same as before, but never calls host-realm
 * `JSON.stringify`/`String` on a sandbox-authored value. See the file
 * header's "third vector" note: the same class of escape as the return-
 * value `toJSON()` hang, just reached via a LOGGED value's `toJSON()`/
 * `toString()` instead of the script's own return value.
 */
function formatLogArgs(args: string[]): string {
  return args.join(" ");
}

function extractLine(message: string): number | undefined {
  const match = message.match(/sandbox\.js:(\d+):\d+/);
  if (!match) return undefined;
  const raw = Number(match[1]);
  return Number.isFinite(raw) ? Math.max(1, raw - WRAPPER_PREFIX_LINES) : undefined;
}

function toSandboxError(err: unknown): SandboxError {
  if (err instanceof Error) {
    const line = extractLine(err.stack ?? err.message);
    return line !== undefined ? { message: err.message, line } : { message: err.message };
  }
  return { message: String(err) };
}

interface OutputSink {
  append(text: string): void;
  readonly truncated: boolean;
  readonly text: string;
}

function createOutputSink(capChars: number): OutputSink {
  const chunks: string[] = [];
  let chars = 0;
  let truncated = false;
  return {
    append(text: string) {
      if (truncated) return;
      if (chars + text.length > capChars) {
        const remaining = Math.max(0, capChars - chars);
        if (remaining > 0) chunks.push(text.slice(0, remaining));
        chunks.push(`\n…output truncated at ~${Math.round(capChars / CHARS_PER_TOKEN)} tokens.`);
        chars = capChars;
        truncated = true;
        return;
      }
      chunks.push(text);
      chars += text.length;
    },
    get truncated() {
      return truncated;
    },
    get text() {
      return chunks.join("");
    },
  };
}

/**
 * Compiles a context-native trampoline around `hostFn` (a call INTO trusted
 * host code). `hostFn`'s own return value/rejection are sanitized before
 * they ever reach sandboxed script -- see the file-level comment for why.
 * Arguments flow host<-sandbox unsanitized (safe: nothing sensitive about
 * a sandbox-realm value reaching trusted host code).
 */
function compileValueTrampoline(context: vm.Context, hostFn: (...args: unknown[]) => unknown): (...args: unknown[]) => unknown {
  const src = `
    var args = Array.prototype.slice.call(arguments);
    return new Promise(function (resolve, reject) {
      Promise.resolve(__bridge.apply(null, args)).then(
        function (v) {
          try { resolve(v === undefined ? undefined : JSON.parse(JSON.stringify(v))); }
          catch (e) { reject(new Error("sem_code: could not serialize the result: " + e.message)); }
        },
        function (e) {
          reject(new Error(e && e.message ? String(e.message) : String(e)));
        }
      );
    });
  `;
  return vm.compileFunction(src, [], {
    parsingContext: context,
    contextExtensions: [{ __bridge: hostFn }],
  }) as (...args: unknown[]) => unknown;
}

/**
 * Fire-and-forget trampoline for `log`/`console.log`. No return value
 * crosses back, but the LOGGED ARGUMENTS themselves need the same care the
 * return-value fix above does: a logged object may carry its own
 * `toJSON()`/`toString()`, and calling that via host-realm `JSON.stringify`
 * is exactly the escape `sanitizeReturnValue` closes for return values --
 * so the stringification happens HERE, context-natively, as part of the
 * SAME synchronous trampoline call the sandboxed script's own `log(...)`
 * invocation makes (still covered by vm's native timeout for a call with
 * no preceding await, same mechanism as the return-value fix; a call
 * reached only after a real host-boundary await falls inside this file's
 * disclosed, pre-existing "busy loop after first await" gap either way --
 * unaffected by whether the stringify itself is context- or host-native).
 * `hostFn` receives already-joined STRINGS, never a raw sandbox value.
 */
function compileVoidTrampoline(context: vm.Context, hostFn: (...args: unknown[]) => void): (...args: unknown[]) => void {
  const src = `
    var args = Array.prototype.slice.call(arguments);
    var formatted = args.map(function (a) {
      if (typeof a === "string") return a;
      try { return JSON.stringify(a); }
      catch (e) {
        try { return String(a); }
        catch (e2) { return "[unstringifiable]"; }
      }
    });
    __bridge.apply(null, formatted);
  `;
  return vm.compileFunction(src, [], {
    parsingContext: context,
    contextExtensions: [{ __bridge: hostFn }],
  }) as (...args: unknown[]) => void;
}

/**
 * Serializes `value` to JSON *inside the sandbox's own vm context*, as a
 * flat, non-async `vm.Script` bound by its own native `timeout` -- never
 * via host-realm `JSON.stringify(value)` called directly. See the file
 * header for the empirical proof this closes: a script's returned value
 * may still carry a sandbox-authored `toJSON()`/`valueOf()`, and running
 * it unprotected on the host thread hangs the whole process indefinitely
 * (there's no forcible-interrupt mechanism on the host side at all -- the
 * previous code's failure mode, not merely an unbounded one).
 *
 * A `vm.Script` timeout here is surfaced as a genuine failure (`ok:false`)
 * rather than silently degrading to `undefined`: it means sandboxed code
 * kept running past its budget, which the caller needs to see as the
 * failure it is. An ordinary non-serializable value (a bare `Function`,
 * a `Symbol`, or `JSON.stringify` returning `undefined` for a top-level
 * `undefined`/`Function`) is not a timeout and keeps the prior, quieter
 * degrade-to-`undefined` behavior -- only a genuine timed-out execution
 * gets treated as a run failure.
 */
function sanitizeReturnValue(
  context: vm.Context,
  globalHolder: Record<string, unknown>,
  value: unknown,
  timeoutMs: number,
): { ok: true; value: unknown } | { ok: false; error: unknown } {
  if (value === undefined) return { ok: true, value: undefined };

  const holderKey = "__sandboxSanitizeTarget";
  Object.defineProperty(globalHolder, holderKey, { value, configurable: true, enumerable: false });
  try {
    const sanitizeScript = new vm.Script(
      `(function () {\n  var __v = globalThis[${JSON.stringify(holderKey)}];\n  return JSON.stringify(__v === undefined ? null : __v);\n})()`,
      { filename: "sandbox-sanitize.js" },
    );
    const jsonText: unknown = sanitizeScript.runInContext(context, { timeout: Math.max(1, timeoutMs) });
    if (typeof jsonText !== "string") return { ok: true, value: undefined };
    return { ok: true, value: JSON.parse(jsonText) };
  } catch (err) {
    if (err instanceof Error && /timed out/.test(err.message)) {
      return {
        ok: false,
        error: new Error(
          "sem_code: timed out serializing the script's returned value -- a toJSON()/valueOf() on it ran past the timeout.",
        ),
      };
    }
    // Non-timeout failure (e.g. a top-level BigInt): degrade quietly, matching prior behavior for ordinary non-serializable values.
    return { ok: true, value: undefined };
  } finally {
    delete globalHolder[holderKey];
  }
}

export async function runInSandbox(code: string, options: SandboxOptions): Promise<SandboxResult> {
  const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  const maxCalls = options.maxCalls ?? DEFAULT_MAX_CALLS;
  const capChars = (options.outputCapTokens ?? DEFAULT_OUTPUT_CAP_TOKENS) * CHARS_PER_TOKEN;

  const sink = createOutputSink(capChars);
  let callCount = 0;
  let refusedAfterResolve = 0;
  let pendingAtResolve = 0;
  const calls: CallRecord[] = [];
  const completed: CompletedCall[] = [];
  const cancellation = options.cancellation ?? createRunCancellation();

  // Build the context with a throwaway placeholder global object first --
  // vm.createContext needs *some* object to contextify before we can
  // compile trampolines against it (compileFunction needs a live context).
  const globalHolder: { sem?: unknown; console?: unknown; [key: string]: unknown } = {};
  const context = vm.createContext(globalHolder, { codeGeneration: { strings: false, wasm: false } });

  const rawSem = (options.sem ?? {}) as Record<string, unknown>;
  const sandboxedSem: Record<string, unknown> = {};
  for (const name of Object.keys(rawSem)) {
    const member = rawSem[name];
    if (!isPlainCallable(member)) continue;
    if (name === "log") {
      sandboxedSem[name] = compileVoidTrampoline(context, (...args: unknown[]) => {
        // Context-native stringification (see compileVoidTrampoline) hands
        // this closure already-joined strings, never a raw sandbox value.
        sink.append(formatLogArgs(args as string[]) + "\n");
      });
      continue;
    }
    const hostFn = member;
    // The full gate every sem.* call passes through -- cancellation first,
    // then the call-cap, then in-flight accounting -- factored so a
    // callable member's own callable PROPERTIES (the one current case:
    // `routine` with `routine.save`) go through the identical pipeline as
    // the member itself, under their dotted name. Stays generic: no member
    // is special-cased by name here.
    const gate = (fnName: string, target: (...args: unknown[]) => unknown) => async (...args: unknown[]) => {
      // Checked FIRST, before even the call-cap: a call attempted after
      // the run's own timeout has already fired must never reach the host
      // function at all -- see createRunCancellation's doc comment.
      if (cancellation.isRevoked()) {
        refusedAfterResolve++;
        throw new Error(CANCELLED_MESSAGE);
      }
      if (callCount >= maxCalls) {
        throw new Error(`sem_code: call limit reached (max ${maxCalls} sem.* calls per run) -- batch fewer, larger calls.`);
      }
      callCount++;
      // Marks this call IN FLIGHT for as long as the host call hasn't
      // settled -- the window a fire-and-forget (unawaited) sem.* call
      // occupies if the script itself returns first. Decremented in
      // `finally` so both the success and failure paths below still count
      // it correctly.
      pendingAtResolve++;
      try {
        const result = await target(...args);
        // Retained BEFORE the sub-call bookkeeping below, so a batch verb's
        // one return value is kept whole rather than once per sub-outcome.
        if (result !== undefined) completed.push({ fn: fnName, value: result });
        const subCalls = readSubCalls(result);
        if (subCalls !== undefined) {
          for (const sub of subCalls) calls.push(sub);
        } else {
          calls.push({ fn: fnName, ok: true });
        }
        return result;
      } catch (err) {
        calls.push({ fn: fnName, ok: false, error: err instanceof Error ? err.message : String(err) });
        throw err;
      } finally {
        pendingAtResolve--;
      }
    };
    const trampoline = compileValueTrampoline(context, gate(name, hostFn)) as ((...args: unknown[]) => unknown) & Record<string, unknown>;
    for (const [propName, propValue] of Object.entries(member as unknown as Record<string, unknown>)) {
      if (!isPlainCallable(propValue)) continue;
      trampoline[propName] = compileValueTrampoline(context, gate(`${name}.${propName}`, propValue));
    }
    // Freeze the trampoline function itself so sandboxed code can't graft
    // properties onto it (the bare functions were implicitly sealed by
    // never being extended; now that properties are legitimate, sealing
    // must be explicit).
    Object.freeze(trampoline);
    sandboxedSem[name] = trampoline;
  }
  Object.freeze(sandboxedSem);

  const sandboxedConsole = {
    log: compileVoidTrampoline(context, (...args: unknown[]) => {
      // Same context-native stringification as sem.log above.
      sink.append(formatLogArgs(args as string[]) + "\n");
    }),
  };
  Object.freeze(sandboxedConsole);

  // Lock the global bindings themselves (not just the objects they point
  // at) so a script can't do `sem = {}` and replace the whole capability.
  Object.defineProperty(globalHolder, "sem", { value: sandboxedSem, writable: false, configurable: false, enumerable: true });
  Object.defineProperty(globalHolder, "console", { value: sandboxedConsole, writable: false, configurable: false, enumerable: true });

  // A plain (non-arrow) async-function IIFE in strict mode, called with no
  // explicit receiver, gives top-level `this` a hard `undefined` -- closing
  // an escape vector an arrow-function wrapper does NOT close. Proven
  // empirically: with an arrow wrapper, `this` inside sandboxed code
  // resolved to the vm context's own exotic global-object template, whose
  // `.constructor` chain reached an unrestricted intrinsic even after every
  // prototype-level mitigation tried here -- codeGeneration:{strings:false}
  // included. Switching to strict-mode `this === undefined` closes it
  // completely: `undefined.constructor` throws before reaching anything.
  const wrapped = `"use strict";\n(async function () {\n${code}\n})()`;

  const executionStartedAt = Date.now();
  let scriptResultPromise: Promise<unknown>;
  try {
    const script = new vm.Script(wrapped, { filename: SANDBOX_FILENAME });
    // vm's own timeout covers only the synchronous burst up to the first
    // await -- e.g. a `while(true){}` before any await -- see file header.
    scriptResultPromise = Promise.resolve(script.runInContext(context, { timeout: timeoutMs }));
  } catch (err) {
    // Nothing could have started (the script never ran), but revoke
    // anyway for consistency with every other exit path below.
    cancellation.revoke();
    return { ok: false, output: sink.text, truncated: sink.truncated, callCount, calls, completed, refusedAfterResolve, pendingAtResolve, error: toSandboxError(err) };
  }

  let outcome: { ok: true; value: unknown } | { ok: false; error: unknown; timedOut: boolean };
  try {
    const timeoutError = new Error(`sem_code: timed out after ${timeoutMs}ms.`);
    const timeoutPromise = new Promise<never>((_resolve, reject) => {
      setTimeout(() => {
        // Revoke BEFORE rejecting: any zombie continuation that resumes
        // from this instant on must see itself as cancelled the moment it
        // next tries a sem.* call, not some indeterminate time later.
        cancellation.revoke();
        reject(timeoutError);
      }, timeoutMs).unref?.();
    });
    const value = await Promise.race([scriptResultPromise, timeoutPromise]);
    outcome = { ok: true, value };
    // Authority-on-failure fix: revoke
    // on EVERY resolution, not just timeout -- a normal, on-time return is
    // still a resolution. This can't retroactively stop a call already
    // past the trampoline's check (pendingAtResolve covers that), but it
    // DOES refuse any SECOND, cascading call chained in a `.then()` after
    // the first settles, rather than letting it silently succeed too.
    cancellation.revoke();
  } catch (err) {
    // Best-effort, not a guarantee: give an already-queued zombie
    // continuation (a microtask-only resumption, not a real wall-clock
    // wait) one tick to hit the revocation check above, so a FAST zombie's
    // refused attempt is reflected in refusedAfterResolve below. A zombie that
    // resumes after a real async wait can't be observed before this
    // function returns, by construction -- it still gets refused when it
    // DOES resume, just invisibly to this particular result.
    await new Promise((resolve) => setImmediate(resolve));
    // `timedOut` must read isRevoked() BEFORE the unconditional revoke()
    // below -- otherwise a genuine (non-timeout) script error would be
    // misreported as a timeout.
    outcome = { ok: false, error: err, timedOut: cancellation.isRevoked() };
    // Same fix as the success path above: a genuine script error is ALSO
    // a resolution, and must revoke too, not just the timeout branch.
    cancellation.revoke();
  }

  if (!outcome.ok) {
    const timeoutSuffix = outcome.timedOut && refusedAfterResolve > 0 ? ` ${refusedAfterResolve} call(s) refused after cancel.` : "";
    const sandboxError = toSandboxError(outcome.error);
    return {
      ok: false,
      output: sink.text,
      truncated: sink.truncated,
      callCount,
      calls,
      completed,
      refusedAfterResolve,
      pendingAtResolve,
      error: timeoutSuffix ? { ...sandboxError, message: sandboxError.message + timeoutSuffix } : sandboxError,
    };
  }

  // Sanitize INSIDE the sandbox context, bound by its own remaining share of
  // the wall-clock budget -- never via host-realm JSON.stringify(value)
  // directly. See `sanitizeReturnValue`'s doc comment for why: the value
  // may still carry a sandbox-authored toJSON()/valueOf(), and a host-side
  // call to it has no timeout bound at all.
  const remainingMs = Math.max(25, timeoutMs - (Date.now() - executionStartedAt));
  const sanitized = sanitizeReturnValue(context, globalHolder, outcome.value, remainingMs);
  if (!sanitized.ok) {
    // Already revoked above (this run already resolved ok:true before
    // sanitization was even attempted) -- no-op call, kept for clarity
    // that this exit path is also covered, not an oversight.
    cancellation.revoke();
    return { ok: false, output: sink.text, truncated: sink.truncated, callCount, calls, completed, refusedAfterResolve, pendingAtResolve, error: toSandboxError(sanitized.error) };
  }

  return { ok: true, output: sink.text, truncated: sink.truncated, callCount, calls, completed, refusedAfterResolve, pendingAtResolve, value: sanitized.value };
}
