// In-sandbox TS/JS runtime of a sem witness run.
//
//   node /opt/witness/witness_rt.mjs /harness/harness.ts
//
// Same contract as witness_rt.py. The runner has spliced into /work:
//   _witness_inj(<param>) at the declared source parameter, or _witness_src(<expr>),
//   _witness_sink(<arg>, i) around every argument of the declared sink call,
//   _witness_probe("<key>", [<params>]) at the start of every path function.
// The harness (`export default async function main(W)`) is bundled with
// esbuild; an import that does not resolve becomes a stub module (an
// external contract). The canary is generated here and never given to the
// harness.
import { createRequire, builtinModules } from 'node:module';
import { randomBytes } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

const require = createRequire(import.meta.url);
const esbuild = require('/opt/witness/node_modules/esbuild');
const TASK = JSON.parse(fs.readFileSync('/harness/task.json', 'utf8'));
const WORK = '/work';
const HARNESS_DIR = '/harness';
const OUT = '/out/result.json';
// generated when the entry call starts: no harness code that runs before it can see it
let CANARY = null;
const realExit = process.exit.bind(process);

const state = {
  in_entry: false, entry_done: false, entry_calls: 0, injections: 0, inject_attempts: 0,
  sink_hits: [], sink_seen: 0, leaks: [], stubbed_modules: [], path_hits: {}, path_seen: {},
  error: null, entry_error: null, registry: [],
};
let injectMode = 'append';

// ------------------------------------------------------------- canary search
function has(v, depth = 0, seen = new Set()) {
  if (!CANARY || depth > 4 || v == null) return false;
  const t = typeof v;
  if (t === 'string') return v.includes(CANARY);
  if (t === 'number' || t === 'boolean' || t === 'bigint' || t === 'symbol') return false;
  if (t === 'function') return false;
  if (v instanceof URL) return v.href.includes(CANARY);
  if (v instanceof Uint8Array) return Buffer.from(v).toString('latin1').includes(CANARY);
  if (v && v[STUB]) return false;
  if (seen.has(v)) return false;
  seen.add(v);
  try {
    if (typeof Request !== 'undefined' && v instanceof Request) return v.url.includes(CANARY);
    if (v instanceof Map) return [...v.entries()].slice(0, 200).some(([a, b]) => has(a, depth + 1, seen) || has(b, depth + 1, seen));
    if (Array.isArray(v) || v instanceof Set) return [...v].slice(0, 200).some((x) => has(x, depth + 1, seen));
    for (const k of Object.keys(v).slice(0, 200)) {
      if (k.includes(CANARY) || has(v[k], depth + 1, seen)) return true;
    }
  } catch { return false; }
  return false;
}

// ------------------------------------------------------------- hooks spliced into repo code
function taint(v, depth = 0) {
  if (typeof v === 'string') return injectMode === 'replace' ? CANARY : injectMode === 'prepend' ? CANARY + v : v + CANARY;
  if (v == null || (v && v[STUB])) return CANARY;
  if (depth > 3 || typeof v !== 'object') return v;
  if (Array.isArray(v)) return v.map((x) => taint(x, depth + 1));
  if (Object.getPrototypeOf(v) === Object.prototype || Object.getPrototypeOf(v) === null) {
    const o = {};
    for (const [k, x] of Object.entries(v)) o[k] = (typeof x === 'string' || (x && typeof x === 'object')) ? taint(x, depth + 1) : x;
    return o;
  }
  return v;
}
globalThis._witness_inj = (v) => {
  if (!state.in_entry) return v;
  state.inject_attempts++;
  const t = taint(v);
  if (has(t)) state.injections++;
  return t;
};
globalThis._witness_src = (v) => {
  if (!state.in_entry) return v;
  if (v && typeof v.then === 'function' && !v[STUB]) return v.then((x) => globalThis._witness_inj(x));
  return globalThis._witness_inj(v);
};

function frames() {
  const e = {};
  Error.captureStackTrace(e);
  return String(e.stack).split('\n').slice(1).map((l) => l.trim());
}
function frameKind(line) {
  if (line.includes('__witness_entry__')) return 'entry';
  if (line.includes('/opt/witness/')) return 'runtime';
  // unmapped bundle lines are esbuild's own module-init helpers (__init, __require): every
  // repo / harness / stub line is source-mapped to its own path
  if (line.includes('/tmp/witness-bundle.cjs')) return 'node';
  if (line.includes(HARNESS_DIR + '/')) return 'harness';
  if (line.includes(WORK + '/')) return line.includes('/node_modules/') ? 'dep' : 'repo';
  if (line.includes('node:') || line.includes('<anonymous>') || /^at (async )?[\w.<>$\[\] ]*$/.test(line)) return 'node';
  return 'other';
}
globalThis._witness_sink = (v, i) => {
  if (!state.in_entry) return v;
  state.sink_seen++;
  if (has(v)) {
    const fr = frames();
    while (fr.length && frameKind(fr[0]) === "runtime") fr.shift(); // the hook itself
    const between = [];
    let entry = false;
    for (const l of fr) {
      const k = frameKind(l);
      if (k === 'entry') { entry = true; break; }
      between.push([k, l]);
    }
    const bad = between.filter(([k]) => !['repo', 'dep', 'node'].includes(k));
    state.sink_hits.push({ arg: i, entryOnStack: entry, badFrames: bad.map(([k, l]) => `${k} ${l}`).slice(0, 10), stack: between.map(([k, l]) => `${k} ${l}`).slice(0, 30) });
  }
  return v;
};
globalThis._witness_probe = (key, args) => {
  if (!state.in_entry) return;
  state.path_seen[key] = true;
  if (!state.path_hits[key] && has(args)) state.path_hits[key] = true;
};

// ------------------------------------------------------------- stubs for absent externals
const STUB = Symbol('witness-stub');
function register(name, args) {
  for (const a of args) {
    if (typeof a === 'function' && !a[STUB]) state.registry.push({ key: name, fn: a });
    else if (a && typeof a === 'object' && !a[STUB]) {
      for (const [k, x] of Object.entries(a)) if (typeof x === 'function' && !x[STUB]) state.registry.push({ key: k, fn: x });
    }
  }
}
function passthrough(fn, name) {
  return new Proxy(fn, {
    get(t, k) {
      if (k === STUB) return false;
      if (k in t) return t[k];
      if (k === 'then') return undefined;
      return makeStub(`${name}(fn).${String(k)}`);
    },
  });
}
function makeStub(name) {
  const target = function () {};
  return new Proxy(target, {
    get(_, k) {
      if (k === STUB) return true;
      if (k === 'then') return undefined; // awaiting a stub gives the stub
      if (k === '__esModule') return true;
      if (k === Symbol.iterator) return function* () {};
      if (k === Symbol.asyncIterator) return async function* () {};
      if (k === Symbol.toPrimitive) return () => '';
      if (k === 'toString' || k === 'valueOf' || k === 'toJSON') return () => '';
      if (k === 'prototype') return target.prototype;
      return makeStub(`${name}.${String(k)}`);
    },
    apply(_, __, args) {
      register(name, args);
      // a decorator / handler registration keeps the function (callable as
      // itself; unknown properties are stubs so builder chains go on)
      if (args.length === 1 && typeof args[0] === 'function' && !args[0][STUB]) return passthrough(args[0], name);
      return makeStub(`${name}()`);
    },
    construct(_, args) { register(name, args); return makeStub(`new ${name}`); },
    set() { return true; },
    has() { return true; },
  });
}
globalThis._witness_stub_module = (name) => { state.stubbed_modules.push(name); return makeStub(name); };

// pure computation is not an external boundary: a harness may fake the
// filesystem, network, env or clock, but not path/url/util semantics
const PURE = ['path', 'url', 'util', 'querystring', 'string_decoder', 'punycode'];
const pureSnapshot = PURE.map((m) => { const mod = require(m); return [m, mod, Object.fromEntries(Object.keys(mod).map((k) => [k, mod[k]]))]; });
function purePatches() {
  const out = [];
  for (const [m, mod, snap] of pureSnapshot) for (const k of Object.keys(snap)) if (mod[k] !== snap[k]) out.push(`${m}.${k} replaced (pure function, not a contract)`);
  return out;
}

// ------------------------------------------------------------- the harness API (never the canary)
const W = Object.freeze({
  injectMode(m) { if (!['append', 'prepend', 'replace'].includes(m)) throw new Error(m); injectMode = m; },
  stub(name = 'stub') { return makeStub(name); },
  // a fake external object: `W.fake({ query: W.returns([]) })`; other props are stubs
  fake(obj) {
    return new Proxy(obj, { get(t, k) { if (k === 'then') return undefined; return k in t ? t[k] : makeStub(String(k)); } });
  },
  returns(v) { const f = () => v; return f; },
  resolves(v) { const f = async () => v; return f; },
  // a function a stubbed framework received (handler, procedure, route) by key
  find(key, nth = 0) {
    const m = state.registry.filter((r) => r.key === key || r.fn.name === key);
    if (!m[nth]) throw new Error(`no registered function '${key}' (have: ${[...new Set(state.registry.map((r) => r.key))].slice(0, 40).join(', ')})`);
    return m[nth].fn;
  },
  async entry(fn, ...args) {
    if (state.entry_calls) throw new Error('W.entry may be called once');
    state.entry_calls++;
    state.repo_patches = purePatches();
    CANARY = 'wq' + randomBytes(8).toString('hex');
    state.in_entry = true;
    const __witness_entry__ = async () => await fn(...args);
    try { return await __witness_entry__(); } catch (e) { state.entry_error = String(e && e.stack || e).slice(0, 800); return undefined; } finally { state.in_entry = false; state.entry_done = true; }
  },
});

// ------------------------------------------------------------- bundling
// workspace packages: package.json `name` -> directory, found under /work
let WS = null;
function workspaces() {
  if (WS) return WS;
  WS = new Map();
  const walk = (d, depth) => {
    if (depth > 5) return;
    let ents = [];
    try { ents = fs.readdirSync(d, { withFileTypes: true }); } catch { return; }
    if (ents.some((e) => e.name === 'package.json' && e.isFile())) {
      try { const j = JSON.parse(fs.readFileSync(path.join(d, 'package.json'), 'utf8')); if (j.name && !WS.has(j.name)) WS.set(j.name, { dir: d, pkg: j }); } catch {}
    }
    for (const e of ents) if (e.isDirectory() && !e.name.startsWith('.') && e.name !== 'node_modules' && e.name !== 'dist' && e.name !== 'build') walk(path.join(d, e.name), depth + 1);
  };
  walk(WORK, 0);
  return WS;
}
function workspaceTarget(spec) {
  const parts = spec.split('/');
  const name = spec.startsWith('@') ? parts.slice(0, 2).join('/') : parts[0];
  const sub = parts.slice(spec.startsWith('@') ? 2 : 1).join('/');
  const w = workspaces().get(name);
  if (!w) return [];
  if (sub) return [path.join(w.dir, sub)];
  const out = [];
  const ex = w.pkg.exports;
  const pick = (e) => (typeof e === 'string' ? e : e && (e.types && String(e.types).endsWith('.ts') ? e.types : (e.import || e.require || e.default)));
  if (ex) { const dot = typeof ex === 'string' ? ex : ex['.'] !== undefined ? pick(ex['.']) : pick(ex); if (typeof dot === 'string') out.push(path.join(w.dir, dot)); }
  for (const f of [w.pkg.source, w.pkg.types, w.pkg.module, w.pkg.main]) if (typeof f === 'string') out.push(path.join(w.dir, f));
  out.push(path.join(w.dir, 'src/index'), path.join(w.dir, 'index'));
  return out;
}

function escapeRe(s) { return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'); }
// ESM source of a stub module: default export and every name `importer`
// imports from `spec` (named imports, re-exports, `ns.member` of a
// namespace import), all reading the same stub
function stubModuleSource(spec, importer) {
  const names = new Set();
  let text = '';
  try { text = fs.readFileSync(importer, 'utf8'); } catch {}
  const from = `from\\s*['"]${escapeRe(spec)}['"]`;
  const named = new RegExp(`(?:import|export)\\s+(?:type\\s+)?(?:[\\w$]+\\s*,\\s*)?\\{([^}]*)\\}\\s*${from}`, 'g');
  for (const m of text.matchAll(named)) {
    for (const part of m[1].split(',')) {
      const nm = part.trim().replace(/^type\s+/, '').split(/\s+as\s+/)[0].trim();
      if (/^[A-Za-z_$][\w$]*$/.test(nm) && nm !== 'default') names.add(nm);
    }
  }
  const ns = new RegExp(`import\\s+\\*\\s+as\\s+([\\w$]+)\\s*${from}`, 'g');
  for (const m of text.matchAll(ns)) {
    for (const a of text.matchAll(new RegExp(`\\b${escapeRe(m[1])}\\.([A-Za-z_$][\\w$]*)`, 'g'))) if (a[1] !== 'default') names.add(a[1]);
  }
  // the JSX automatic runtime's imports are synthesized by the bundler, not
  // written in the importer
  if (/jsx-(dev-)?runtime$/.test(spec)) for (const nm of ['jsx', 'jsxs', 'jsxDEV', 'Fragment']) names.add(nm);
  const lines = [`const s = globalThis._witness_stub_module(${JSON.stringify(spec)});`, 'export default s;'];
  for (const nm of names) lines.push(`export const ${nm} = s[${JSON.stringify(nm)}];`);
  return lines.join('\n');
}

const builtins = new Set([...builtinModules, ...builtinModules.map((m) => 'node:' + m)]);
const stubPlugin = {
  name: 'witness-stubs',
  setup(build) {
    build.onResolve({ filter: /.*/ }, async (args) => {
      if (args.pluginData && args.pluginData.inner) return undefined;
      if (builtins.has(args.path)) return { path: args.path, external: true };
      if (args.kind === 'entry-point') return undefined;
      const r = await build.resolve(args.path, { resolveDir: args.resolveDir, kind: args.kind, importer: args.importer, pluginData: { inner: true } });
      if (r.errors.length === 0 && !r.external) return r;
      // a package of this repo's own workspace (monorepo; no node_modules
      // links in the sandbox): resolve it to its directory
      const ws = workspaceTarget(args.path);
      for (const cand of ws) {
        const r2 = await build.resolve(cand, { resolveDir: WORK, kind: args.kind, importer: args.importer, pluginData: { inner: true } });
        if (r2.errors.length === 0 && !r2.external) return r2;
      }
      // one stub module per (specifier, importer): its named exports are
      // the names that importer takes from it
      return { path: args.path + '?from=' + args.importer, namespace: 'witness-stub', pluginData: { spec: args.path, importer: args.importer } };
    });
    build.onLoad({ filter: /.*/, namespace: 'witness-stub' }, (args) => {
      const { spec, importer } = args.pluginData;
      return { contents: stubModuleSource(spec, importer), loader: 'js', resolveDir: WORK };
    });
    // repo code: its own __dirname / __filename / import.meta.url (the
    // bundle lives in /tmp, so the bundler's would point there)
    build.onLoad({ filter: /^\/work\/.*\.(m|c)?(t|j)sx?$/ }, (args) => {
      if (args.path.includes('/node_modules/')) return undefined;
      let src = fs.readFileSync(args.path, 'utf8');
      const ext = path.extname(args.path).replace('.', '');
      const loader = { mts: 'ts', cts: 'ts', mjs: 'js', cjs: 'js' }[ext] || ext;
      const url = 'file://' + args.path;
      src = src.replace(/\bimport\.meta\.url\b/g, JSON.stringify(url))
        .replace(/\bimport\.meta\.dirname\b/g, JSON.stringify(path.dirname(args.path)))
        .replace(/\bimport\.meta\.filename\b/g, JSON.stringify(args.path));
      // CommonJS-style globals, unless the module defines its own
      if (!/(const|let|var)\s+__dirname\b/.test(src)) src = src.replace(/\b__dirname\b/g, JSON.stringify(path.dirname(args.path)));
      if (!/(const|let|var)\s+__filename\b/.test(src)) src = src.replace(/\b__filename\b/g, JSON.stringify(args.path));
      return { contents: src, loader, resolveDir: path.dirname(args.path) };
    });
    // non-code assets imported by code
    build.onLoad({ filter: /\.(css|scss|sass|less|svg|png|jpg|jpeg|gif|webp|woff2?|ttf|md|html)$/ }, (args) => ({
      contents: `module.exports = ${JSON.stringify(path.basename(args.path))};`, loader: 'js',
    }));
  },
};

async function main() {
  let harness = process.argv[2];
  // W.entryImport('/work/x') (exploratory, --rules v2): the runtime, not the
  // harness, writes the thunk that loads the module inside the entry call
  const hsrc = fs.readFileSync(harness, 'utf8');
  if (/\bW\s*\.\s*entryImport\s*\(/.test(hsrc)) {
    const re = /\bW\s*\.\s*entryImport\s*\(\s*(['"])(\/work\/[^'"]+)\1\s*\)/g;
    const out = hsrc.replace(re, (m, q, p) => `W.entry(function __witness_entry__import() { return require(${JSON.stringify(p)}); })`);
    fs.mkdirSync('/tmp/wh/harness', { recursive: true });
    harness = '/tmp/wh/harness/' + path.basename(harness);
    fs.writeFileSync(harness, out);
  }
  const outfile = '/tmp/witness-bundle.cjs';
  try {
    await esbuild.build({
      entryPoints: [harness], bundle: true, platform: 'node', format: 'cjs', target: 'node22',
      outfile, sourcemap: 'inline', logLevel: 'silent', plugins: [stubPlugin], ignoreAnnotations: true, treeShaking: false,
      jsx: 'automatic', absWorkingDir: WORK, nodePaths: [path.join(WORK, 'node_modules')],
      loader: { '.json': 'json', '.txt': 'text' },
      tsconfig: TASK.tsconfig ? path.join(WORK, TASK.tsconfig) : undefined,
      define: { 'import.meta.url': '"file:///work/index.js"' },
    });
    process.setSourceMapsEnabled(true);
    const mod = require(outfile);
    const fn = mod.default || mod.main;
    if (typeof fn !== 'function') throw new Error('harness must `export default async function main(W)`');
    await fn(W);
  } catch (e) {
    state.error = String(e && (e.stack || e.message) || e).slice(-2000);
  }
  finish();
}

function redact(s) { return typeof s === 'string' && CANARY ? s.split(CANARY).join('<CANARY>') : s; }
function finish() {
  const res = { ...state, registry: [...new Set(state.registry.map((r) => r.key))].slice(0, 60) };
  res.error = redact(res.error);
  res.entry_error = redact(res.entry_error);
  res.sink_hits = res.sink_hits.map((h) => ({ ...h, badFrames: h.badFrames.map(redact), stack: h.stack.map(redact) }));
  res.stubbed_modules = [...new Set(res.stubbed_modules)].slice(0, 80);
  fs.mkdirSync('/out', { recursive: true });
  fs.writeFileSync(OUT, JSON.stringify(res, null, 1));
  realExit(0);
}
process.on('unhandledRejection', () => {});
// repo code calling process.exit (a CLI's `.catch(() => process.exit(1))`)
// must not lose the observations: record and finish
process.exit = (code) => {
  state.error = (state.error || '') + ` process.exit(${code}) called`;
  finish();
};
setTimeout(() => { state.error = (state.error || '') + ' timeout(60s)'; finish(); }, 60000).unref();
main();
