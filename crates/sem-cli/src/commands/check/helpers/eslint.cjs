#!/usr/bin/env node
// sem check: ESLint helper, run with the project's own `eslint`.
//
//   node eslint.cjs --opts OPTS.json --out RESULT.json
//                   [--state-in STATE] [--state-out STATE] [--full]
//
// OPTS: { patterns, extensions, maxWarnings, quiet, configFile, noEslintrc,
//         reportUnusedDisableDirectives, importers: {file: [importer, ...]},
//         unresolved: [[importer, specifier], ...] }
//
// The verdict equals `eslint <patterns>` on the whole project. Most rules see
// one file; a file's messages then change only when the file does. Rules of
// the import family resolve a file's imports, so its messages also depend on
// the files it imports: their importers are relinted too. Everything else
// that can change a message forces the full run, and says so: no state, a
// different ESLint/plugin/TypeScript version, a changed config or ignore file
// (or a file a config file requires), a changed package.json or lockfile,
// type-aware linting (parserOptions.project/projectService: a file's messages
// depend on the whole program), a rule of a plugin not known to be per-file,
// whole-graph import rules (no-cycle, no-unused-modules), and an unresolved
// import that may name a changed file.
//
// The state is self-validating: every linted file's content hash is recorded
// and re-hashed; new files are found by walking the patterns the way ESLint
// enumerates them.
"use strict";
const fs = require("fs");
const path = require("path");
const crypto = require("crypto");
const zlib = require("zlib");

const HELPER_VERSION = "sem-check-eslint/1";
const T0 = process.hrtime.bigint();
const ms = () => Number(process.hrtime.bigint() - T0) / 1e6;
const cwd = process.cwd();
const argv = process.argv.slice(2);
const arg = (k, d) => {
  const i = argv.indexOf(k);
  return i < 0 ? d : argv[i + 1];
};
const flag = (k) => argv.includes(k);
const timings = {};
const lap = (n, t) => (timings[n] = Math.round((ms() - t) * 10) / 10);
const OUT = arg("--out");
function finish(o) {
  o.timings = timings;
  o.totalMs = Math.round(ms());
  fs.writeFileSync(OUT, JSON.stringify(o));
  process.exit(0);
}
const opts = JSON.parse(fs.readFileSync(arg("--opts"), "utf8"));
const sha = (s) => crypto.createHash("sha1").update(s).digest("hex");
const rel = (f) => path.relative(cwd, f).split(path.sep).join("/");
const readHash = (r) => {
  try {
    return sha(fs.readFileSync(path.join(cwd, r)));
  } catch {
    return null;
  }
};

let eslintPkg, eslintVersion;
try {
  const pj = require.resolve("eslint/package.json", { paths: [cwd] });
  eslintVersion = JSON.parse(fs.readFileSync(pj, "utf8")).version;
  eslintPkg = require(require.resolve("eslint", { paths: [cwd] }));
} catch (e) {
  finish({ verdict: "undecided", error: "eslint is not installed in this project (" + String(e).split("\n")[0] + ")" });
}

// Packages whose version can change a message: eslint, its plugins, parsers,
// shareable configs, and typescript (the TS parser's).
function versions() {
  const out = { eslint: eslintVersion };
  // the node_modules Node resolves from: the nearest one up from the root
  let nm = path.join(cwd, "node_modules");
  for (let d = cwd; ; d = path.dirname(d)) {
    if (fs.existsSync(path.join(d, "node_modules"))) {
      nm = path.join(d, "node_modules");
      break;
    }
    if (path.dirname(d) === d) break;
  }
  const add = (name) => {
    try {
      out[name] = JSON.parse(fs.readFileSync(path.join(nm, name, "package.json"), "utf8")).version;
    } catch {}
  };
  let entries = [];
  try {
    entries = fs.readdirSync(nm);
  } catch {}
  for (const e of entries) {
    if (e.startsWith("@")) {
      let sub = [];
      try {
        sub = fs.readdirSync(path.join(nm, e));
      } catch {}
      for (const s of sub) if (/eslint/.test(e + "/" + s)) add(e + "/" + s);
    } else if (/eslint/.test(e) || e === "typescript" || e === "prettier") add(e);
  }
  return out;
}

const SKIP_DIRS = new Set(["node_modules", ".git"]);
const LOCKFILES = ["package-lock.json", "yarn.lock", "pnpm-lock.yaml", "bun.lockb", "bun.lock", "npm-shrinkwrap.json"];
const CONFIG_RE = /(^|\/)(\.eslintrc(\.(js|cjs|yaml|yml|json))?|\.eslintignore|eslint\.config\.(js|mjs|cjs|ts|mts|cts)|\.prettierrc(\.(js|cjs|mjs|json|json5|yaml|yml|toml))?|prettier\.config\.(js|cjs|mjs)|\.editorconfig|\.browserslistrc|package\.json|tsconfig(\.[^/]*)?\.json)$/;

// Every repo file an ESLint config can read (config/ignore files, package.json
// files, prettier/editorconfig/browserslist/tsconfig files, lockfiles), plus
// local files the configs require. Walked once per run.
function walkAll(dir, out, dotOk) {
  let ents = [];
  try {
    ents = fs.readdirSync(path.join(cwd, dir), { withFileTypes: true });
  } catch {
    return;
  }
  for (const e of ents) {
    const r = dir ? dir + "/" + e.name : e.name;
    if (e.isDirectory()) {
      if (SKIP_DIRS.has(e.name)) continue;
      walkAll(r, out, dotOk);
    } else if (e.isFile()) out.push(r);
  }
}
const t0 = ms();
const allFiles = [];
walkAll("", allFiles, true);
lap("walk", t0);

// [size, mtimeMs, sha1] per file; a hash is recomputed only when size or
// mtime moved (what git's index does), or when the mtime is too close to the
// state's own write time to be trusted.
let prevStat = {};
let prevWritten = 0;
const stats = {};
function statOf(r) {
  if (stats[r]) return stats[r];
  let st;
  try {
    st = fs.statSync(path.join(cwd, r));
  } catch {
    return (stats[r] = null);
  }
  const p = prevStat[r];
  const racy = st.mtimeMs >= prevWritten - 2000;
  if (p && p[0] === st.size && p[1] === st.mtimeMs && !racy) return (stats[r] = p);
  return (stats[r] = [st.size, st.mtimeMs, readHash(r)]);
}
const hashOf = (r) => {
  const s = statOf(r);
  return s ? s[2] : null;
};

function configInputs(also) {
  const m = {};
  for (const f of also || []) m[f] = readHash(f);
  for (const f of allFiles) if (CONFIG_RE.test(f) || LOCKFILES.includes(f)) m[f] = hashOf(f);
  // local modules config files require (eslintrc in CJS)
  for (const id of Object.keys(require.cache)) {
    const r = rel(id);
    if (!r.startsWith("..") && !r.includes("node_modules/")) m[r] = readHash(r);
  }
  // local modules a flat config imports (best effort: relative specifiers, transitively)
  const queue = allFiles.filter((f) => /(^|\/)eslint\.config\.[cm]?[jt]s$/.test(f));
  const seen = new Set(queue);
  while (queue.length) {
    const f = queue.shift();
    m[f] = readHash(f);
    let text = "";
    try {
      text = fs.readFileSync(path.join(cwd, f), "utf8");
    } catch {}
    for (const mm of text.matchAll(/(?:from\s*|import\s*\(\s*|require\s*\(\s*|import\s+)["'](\.{1,2}\/[^"']+)["']/g)) {
      const base = path.posix.normalize(path.posix.join(path.posix.dirname(f), mm[1]));
      for (const c of [base, ...[".js", ".mjs", ".cjs", ".ts", ".json"].map((x) => base + x), base + "/index.js"])
        if (fs.existsSync(path.join(cwd, c)) && fs.statSync(path.join(cwd, c)).isFile() && !seen.has(c)) {
          seen.add(c);
          queue.push(c);
        }
    }
  }
  return m;
}

const KNOWN_PER_FILE = new Set([
  "react", "react-hooks", "jsx-a11y", "@typescript-eslint", "prettier", "testing-library", "jest", "jest-dom",
  "vitest", "@vitest", "unicorn", "simple-import-sort", "promise", "sonarjs", "flowtype", "eslint-comments",
  "@eslint-community/eslint-comments", "react-refresh", "regexp", "security", "no-only-tests", "mocha", "cypress",
  "@stylistic", "@stylistic/js", "@stylistic/ts", "@stylistic/jsx", "perfectionist", "jsdoc", "react-native",
  "vue", "svelte", "solid", "qwik", "playwright", "chai-friendly", "unused-imports", "es", "es-x", "functional",
]);
const CROSS_FILE = new Set(["import", "import-x", "n", "node"]);
const WHOLE_GRAPH = new Set(["import/no-cycle", "import/no-unused-modules", "import-x/no-cycle", "import-x/no-unused-modules"]);
const sevOff = (v) => {
  const s = Array.isArray(v) ? v[0] : v;
  return s === 0 || s === "off";
};
const pluginOf = (rule) => (rule.includes("/") ? rule.slice(0, rule.lastIndexOf("/")) : null);

// What one file's config makes its messages depend on.
function classify(cfg) {
  const out = { typeAware: false, crossFile: false, wholeGraph: [], unknown: [] };
  if (!cfg) return out;
  const po = (cfg.languageOptions && cfg.languageOptions.parserOptions) || cfg.parserOptions || {};
  if (po.project || po.projectService || po.programs) out.typeAware = true;
  for (const [rule, v] of Object.entries(cfg.rules || {})) {
    if (sevOff(v)) continue;
    if (WHOLE_GRAPH.has(rule)) out.wholeGraph.push(rule);
    const p = pluginOf(rule);
    if (!p) continue;
    if (CROSS_FILE.has(p)) out.crossFile = true;
    else if (!KNOWN_PER_FILE.has(p)) out.unknown.push(rule);
  }
  return out;
}

const fmt = (r, m) => `${r}:${m.line || 0}:${m.column || 0}: ${m.severity === 2 ? "error" : "warning"} ${m.ruleId || (m.fatal ? "fatal" : "-")}: ${m.message}`;
function entryOf(res) {
  const r = rel(res.filePath);
  let msgs = res.messages;
  if (opts.quiet) msgs = msgs.filter((m) => m.severity === 2);
  return [r, { h: hashOf(r), msgs: msgs.map((m) => fmt(r, m)), e: msgs.filter((m) => m.severity === 2).length, w: msgs.filter((m) => m.severity === 1).length }];
}

(async () => {
  const major = Number(eslintVersion.split(".")[0]);
  const hasFlat = allFiles.some((f) => /^eslint\.config\.[cm]?[jt]s$/.test(f));
  let flat = major >= 9 ? process.env.ESLINT_USE_FLAT_CONFIG !== "false" : hasFlat || process.env.ESLINT_USE_FLAT_CONFIG === "true";
  let ESLintClass = eslintPkg.ESLint;
  if (eslintPkg.loadESLint) ESLintClass = await eslintPkg.loadESLint({ useFlatConfig: flat });
  else if (flat && major === 8) {
    try {
      ESLintClass = require(require.resolve("eslint/use-at-your-own-risk", { paths: [cwd] })).FlatESLint;
    } catch {}
  }
  const o = { cwd };
  if (opts.configFile) o.overrideConfigFile = opts.configFile;
  if (!flat) {
    if (opts.extensions && opts.extensions.length) o.extensions = opts.extensions;
    if (opts.noEslintrc) o.useEslintrc = false;
    if (opts.reportUnusedDisableDirectives) o.reportUnusedDisableDirectives = opts.reportUnusedDisableDirectives;
  }
  const eslint = new ESLintClass(o);
  const patterns = opts.patterns && opts.patterns.length ? opts.patterns : ["."];
  const exts = flat ? null : opts.extensions && opts.extensions.length ? opts.extensions : [".js"];

  // Would `eslint <patterns>` lint this file?
  const underPatterns = (f) => patterns.some((p) => p === "." || f === p || f.startsWith(p.replace(/\/$/, "") + "/") || (p.includes("*") && false));
  const patternsSimple = patterns.every((p) => !/[*?{[]/.test(p));
  async function lintable(f) {
    if (!underPatterns(f)) return false;
    if (!flat) {
      if (!exts.some((x) => f.endsWith(x))) return false;
      if (f.split("/").some((seg) => seg.startsWith("."))) return false; // eslintrc ignores dotfiles
    }
    if (await eslint.isPathIgnored(path.join(cwd, f))) return false;
    if (flat) {
      try {
        return (await eslint.calculateConfigForFile(path.join(cwd, f))) !== undefined;
      } catch {
        return false;
      }
    }
    return true;
  }

  const vers = versions();
  const state = (() => {
    if (flag("--full")) return null;
    const p = arg("--state-in");
    if (!p || !fs.existsSync(p)) return null;
    try {
      const buf = fs.readFileSync(p);
      const s = JSON.parse((buf[0] === 0x1f && buf[1] === 0x8b ? zlib.gunzipSync(buf) : buf).toString("utf8"));
      return { s, digest: crypto.createHash("sha256").update(buf).digest("hex") };
    } catch {
      return null;
    }
  })();
  if (state && state.s) {
    prevStat = state.s.stat || {};
    prevWritten = state.s.written || 0;
  }
  const optsKey = sha(JSON.stringify({ patterns, exts, flat, configFile: opts.configFile, noEslintrc: opts.noEslintrc, quiet: opts.quiet, rudd: opts.reportUnusedDisableDirectives }));
  const reasons = [];
  let mode = "full";
  let files = {};
  let relint = [];
  const tc = ms();
  let cfgIn = null;
  if (flag("--full")) reasons.push("full: requested");
  else if (!state) reasons.push("no-state: no earlier lint state to start from");
  else if (!patternsSimple) reasons.push("patterns: glob patterns are linted in full");
  else {
    const s = state.s;
    if (s.helper !== HELPER_VERSION) reasons.push("helper-version");
    if (s.opts !== optsKey) reasons.push("options: lint options changed");
    for (const k of new Set([...Object.keys(s.versions || {}), ...Object.keys(vers)]))
      if ((s.versions || {})[k] !== vers[k]) reasons.push(`version: ${k} ${(s.versions || {})[k]} -> ${vers[k]}`);
  }
  // config inputs are hashed after ESLint has loaded the configs (require.cache)
  let candidates = [];
  if (!reasons.length) {
    const s = state.s;
    // which files changed since the state (any file: lint target or not)
    const changed = new Set();
    const fresh = [];
    for (const f of allFiles) {
      if (!prevStat[f]) fresh.push(f);
      else if (hashOf(f) !== prevStat[f][2]) changed.add(f);
    }
    const now = new Set(allFiles);
    for (const f of Object.keys(prevStat)) if (!now.has(f)) changed.add(f);
    const known = new Set(Object.keys(s.files));
    // load configs for the files we will look at, then hash config inputs
    const added = [];
    for (const f of fresh) if (await lintable(f)) added.push(f);
    candidates = [...new Set([...[...changed].filter((f) => known.has(f)), ...added])].filter((f) => fs.existsSync(path.join(cwd, f)));
    const removed = [...changed].filter((f) => !fs.existsSync(path.join(cwd, f)));
    // any path that changed at all (lint target or not): its importers may resolve differently
    const touched = new Set([...changed, ...fresh]);
    cfgIn = configInputs(Object.keys(s.config || {}));
    for (const f of new Set([...Object.keys(s.config || {}), ...Object.keys(cfgIn)])) if ((s.config || {})[f] !== cfgIn[f]) reasons.push(`config: ${f}`);
    if (!reasons.length) {
      const importersNew = opts.importers || {};
      const importersOld = s.importers || {};
      const importers = new Set();
      for (const f of touched) {
        for (const i of importersNew[f] || []) importers.add(i);
        for (const i of importersOld[f] || []) importers.add(i);
      }
      // an import nobody could resolve may name a touched file
      for (const [src, spec] of opts.unresolved || []) {
        if (!spec.startsWith(".")) continue;
        const base = path.posix.normalize(path.posix.join(path.posix.dirname(src), spec));
        for (const f of touched)
          if (f === base || f.startsWith(base + ".") || f.startsWith(base + "/index.")) reasons.push(`unresolved-import: ${src} imports ${spec}, which may be ${f}`);
      }
      const set = new Set(candidates);
      const lintNow = [];
      for (const f of [...candidates, ...importers]) {
        if (!fs.existsSync(path.join(cwd, f))) continue;
        if (!known.has(f) && !set.has(f)) continue; // not a lint target
        if (lintNow.includes(f)) continue;
        lintNow.push(f);
      }
      // what the rules of the files involved depend on
      let cross = false;
      for (const f of lintNow) {
        let cfg;
        try {
          cfg = await eslint.calculateConfigForFile(path.join(cwd, f));
        } catch (e) {
          reasons.push(`config: cannot compute the config of ${f}`);
          break;
        }
        const c = classify(cfg);
        if (c.typeAware) {
          reasons.push(`type-aware: ${f} is linted with type information (parserOptions.project/projectService): its messages depend on the whole program`);
          break;
        }
        if (c.wholeGraph.length) {
          reasons.push(`whole-graph-rule: ${c.wholeGraph[0]} depends on the whole import graph`);
          break;
        }
        if (c.unknown.length) {
          reasons.push(`unknown-rule: ${c.unknown[0]} (plugin not known to read only the file it lints)`);
          break;
        }
        if (c.crossFile) cross = true;
      }
      if (!reasons.length) {
        relint = cross ? lintNow : lintNow.filter((f) => set.has(f));
        files = Object.assign({}, s.files);
        for (const f of removed) delete files[f];
        if (relint.length) {
          const res = await eslint.lintFiles(relint.map((f) => path.join(cwd, f)));
          for (const r of res) {
            const [k, e] = entryOf(r);
            files[k] = e;
          }
        }
        mode = "incremental";
      }
    }
  }
  if (mode === "full") {
    const res = await eslint.lintFiles(patterns);
    files = {};
    for (const r of res) {
      const [k, e] = entryOf(r);
      files[k] = e;
    }
    relint = Object.keys(files);
    cfgIn = cfgIn || configInputs();
  }
  lap("lint", tc);
  let errors = 0,
    warnings = 0;
  const diags = [];
  for (const f of Object.keys(files).sort()) {
    errors += files[f].e;
    warnings += files[f].w;
    diags.push(...files[f].msgs);
  }
  const max = typeof opts.maxWarnings === "number" ? opts.maxWarnings : -1;
  const tooMany = max >= 0 && warnings > max;
  const result = {
    verdict: errors > 0 || tooMany ? "fail" : "pass",
    mode,
    reasons: mode === "full" ? reasons : [relint.length ? "per-file rules: changed files" + (relint.length > candidates.length ? " and the files that import them (import rules resolve imports)" : "") + " relinted" : "nothing lintable changed"],
    eslintVersion,
    configSystem: flat ? "flat" : "eslintrc",
    relinted: relint.sort(),
    lintedFiles: Object.keys(files).length,
    errors,
    warnings,
    maxWarnings: max,
    diagnostics: diags.concat(tooMany ? [`ESLint found too many warnings (maximum: ${max}).`] : []),
    stateIn: state && state.digest,
  };
  const out = arg("--state-out");
  if (out) {
    const stat = {};
    for (const f of allFiles) {
      const st = statOf(f);
      if (st) stat[f] = st;
    }
    const buf = zlib.gzipSync(Buffer.from(JSON.stringify({ helper: HELPER_VERSION, opts: optsKey, versions: vers, config: cfgIn, files, stat, written: Date.now(), importers: opts.importers || {} })), { level: 1 });
    fs.writeFileSync(out, buf);
    result.stateOut = crypto.createHash("sha256").update(buf).digest("hex");
  }
  finish(result);
})().catch((e) => finish({ verdict: "undecided", error: "eslint failed: " + (e && e.stack ? e.stack.split("\n").slice(0, 3).join(" ") : String(e)) }));
