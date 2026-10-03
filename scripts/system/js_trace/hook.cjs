// Runtime half of the JS/TS tracer: `__systrace()` is called first thing in
// every instrumented function; it reads the stack (source-mapped when the
// runner installs source maps, as ts-jest does) and records each distinct
// (caller frame, callee frame) once per process to $SYSTRACE_OUT/js-<pid>.jsonl.
// State lives on `process` so that every VM context (jest's per-file
// globals) shares one set.
"use strict";
const fs = require("fs");
const path = require("path");

const st = process.__systrace_state || (process.__systrace_state = { seen: new Set(), buf: [], hooked: false });

function flush() {
  const dir = process.env.SYSTRACE_OUT;
  if (!dir || !st.buf.length) return;
  try {
    fs.mkdirSync(dir, { recursive: true });
    fs.appendFileSync(path.join(dir, `js-${process.pid}.jsonl`), st.buf.join("\n") + "\n");
  } catch (_) {}
  st.buf.length = 0;
}

const FRAME = /at (?:(.*?) \()?(?:file:\/\/)?([^():\s][^():]*):(\d+):(\d+)\)?$/;

const known = new Map();
function exists(f) {
  if (!known.has(f)) {
    let ok = false;
    try { ok = fs.statSync(f).isFile(); } catch (_) {}
    known.set(f, ok);
  }
  return known.get(f);
}

// down-levelled async/generator helpers emitted inline (target < es2017)
const HELPER = /(^|\.)(__awaiter|__generator|step|fulfilled|rejected|verb|adopt)$/;

function parse(line) {
  const m = FRAME.exec(line.trim());
  if (!m) return null;
  // source-mapped frames may be relative to the working directory; a
  // relative path that names no file there (a dependency's own source map,
  // `../../src/x.ts`) is not a repo frame
  let file = m[2];
  if (!path.isAbsolute(file)) {
    file = path.resolve(process.cwd(), file);
    if (!exists(file)) file = "<external>/" + m[2];
  }
  return { name: m[1] || "", file, line: +m[3] };
}

function __systrace() {
  const limit = Error.stackTraceLimit;
  Error.stackTraceLimit = 14;
  const stack = new Error().stack || "";
  Error.stackTraceLimit = limit;
  const lines = stack.split("\n");
  // [0] "Error", [1] __systrace, [2] callee, then the caller — past
  // down-levelled async/generator machinery (`Generator.next`, tslib's
  // __awaiter, `new Promise`) and the wrapper frame on the callee's own line
  const callee = parse(lines[2] || "");
  if (!callee) return;
  let caller = null;
  let callerIdx = -1;
  for (let i = 3; i < lines.length; i++) {
    const f = parse(lines[i]);
    if (!f) continue;
    if (/\/node_modules\/tslib\/|\/node_modules\/regenerator-runtime\/|\/@babel\/runtime\//.test(f.file)) continue;
    if (HELPER.test(f.name) || f.name === "new Promise" || f.name === "Generator.next") continue;
    if (f.file === callee.file && f.line === callee.line) continue;
    caller = f;
    callerIdx = i;
    break;
  }
  if (!caller) return;
  // Source-mapped stacks (jest) name each frame after the function called
  // at that position, not the one running there: shift names by one.
  if (/__systrace$/.test(callee.name)) {
    const above = parse(lines[callerIdx + 1] || "");
    callee.name = caller.name;
    caller.name = above ? above.name : "";
  }
  const key = `${caller.file}:${caller.line}>${callee.file}:${callee.line}`;
  if (st.seen.has(key)) return;
  st.seen.add(key);
  st.buf.push(JSON.stringify({
    caller_file: caller.file, caller_line: 0, caller: caller.name, site_line: caller.line,
    callee_file: callee.file, callee_line: callee.line, callee: callee.name,
  }));
  // write at once: test runners sandbox `process` (jest), so an exit hook
  // may never fire; each distinct edge is written only once anyway
  flush();
}

globalThis.__systrace = __systrace;
if (!st.hooked) {
  st.hooked = true;
  process.on("exit", flush);
}
module.exports = __systrace;
