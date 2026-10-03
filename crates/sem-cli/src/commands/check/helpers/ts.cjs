#!/usr/bin/env node
// sem check: TypeScript checker helper, run with the project's own `typescript`.
//
//   node ts.cjs --project tsconfig.json --out RESULT.json
//               [--state-in STATE.json.gz] [--state-out STATE.json.gz] [--full]
//
// cwd is the project root. The verdict always equals what `tsc --pretty false`
// prints for the same project: either every file is checked (mode "full"), or
// only the files a change can affect are, with every other file's diagnostics
// carried over from a state written by an earlier run (mode "incremental").
//
// The state is self-validating. It records the content hash of every source
// file of the program (node_modules declarations and lib files included), the
// module each import of each file resolved to, each file's module format, the
// compiler options, every config file of the tsconfig chain and the TS version.
// The change set is computed against that record, never taken on trust from
// git, so a state from any tree is a correct starting point: whatever differs
// is either rechecked or forces the full check.
//
// Incremental method (early cutoff by public interface, per exported name):
// - seeds: repo files whose content changed or that are new; importers of
//   removed files; files whose imports now resolve elsewhere;
// - a file's interface = its d.ts (Program.emit with forceDtsEmit), union
//   members and type-literal properties sorted (their order is a check-order
//   artifact); per exported name, closed over local references and over the
//   names it uses from other modules;
// - an importer joins the recheck set only if it uses a changed name
//   (namespace imports, `export *`, import() types, dynamic import/require,
//   module augmentation, JS/JSON = uses everything).
// Full check instead (and why) when: no usable state, TS version or helper
// changed, any tsconfig of the chain or the compiler options changed, any
// non-repo program file (node_modules, lib) changed, a module format changed,
// a file that reaches the global scope (script file, `declare global`,
// module augmentation) changed its interface, or a package.json field that
// steers module resolution changed.
"use strict";
const fs = require("fs");
const path = require("path");
const crypto = require("crypto");
const zlib = require("zlib");

const HELPER_VERSION = "sem-check-ts/2";
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
const lap = (name, t0) => (timings[name] = Math.round((ms() - t0) * 10) / 10);

const OUT = arg("--out");
function finish(obj, code) {
  obj.timings = timings;
  obj.totalMs = Math.round(ms());
  fs.writeFileSync(OUT, JSON.stringify(obj));
  process.exit(code || 0);
}

let ts;
try {
  ts = require(require.resolve("typescript", { paths: [cwd] }));
} catch (e) {
  finish({ verdict: "undecided", error: "typescript is not installed in this project (require.resolve('typescript') failed from " + cwd + ")" }, 0);
}

const sha = (s) => crypto.createHash("sha1").update(s).digest("hex");
const rel = (f) => path.relative(cwd, f).split(path.sep).join("/");
const isRepo = (r) => !r.startsWith("..") && !path.isAbsolute(r) && !r.includes("node_modules/");
// A non-repo file's identity independent of where the checkout is (every
// clone and worktree has its own node_modules, or links one): its path from
// the first node_modules segment on, else its path relative to the root.
const extKey = (f) => {
  const r = rel(f);
  const i = r.indexOf("node_modules/");
  return i >= 0 ? r.slice(i) : r;
};
const keyOf = (f) => {
  const r = rel(f);
  return isRepo(r) ? r : extKey(f);
};
const fmtHost = { getCurrentDirectory: () => cwd, getCanonicalFileName: (f) => f, getNewLine: () => "\n" };
const fmt = (d) => ts.formatDiagnostic(d, fmtHost).replace(/\n$/, "");

// ---- config ----------------------------------------------------------------

const projectArg = arg("--project", "tsconfig.json");
const configPath = path.resolve(cwd, projectArg);
let pc;
{
  const t = ms();
  const host = {
    ...ts.sys,
    onUnRecoverableConfigFileDiagnostic: (d) => {
      finish({ verdict: "fail", mode: "full", reasons: ["config: unrecoverable"], diagnostics: [fmt(d)], errors: 1, tsVersion: ts.version }, 0);
    },
  };
  pc = ts.getParsedCommandLineOfConfigFile(configPath, {}, host);
  lap("config", t);
}
const configFiles = [configPath, ...((pc.options.configFile && pc.options.configFile.extendedSourceFiles) || [])];
const configHashes = {};
for (const f of configFiles) {
  try {
    configHashes[rel(f)] = sha(fs.readFileSync(f, "utf8"));
  } catch {
    configHashes[rel(f)] = "missing";
  }
}
if (!pc.fileNames.length && (pc.projectReferences || []).length) {
  finish({ verdict: "undecided", error: `${rel(configPath)} is a solution (references only, no files of its own): its full check is \`tsc -b\`; point ts.project at a project config, or check it with a "commands" entry`, tsVersion: ts.version }, 0);
}
const optionsKey = sha(
  // paths inside the options (pathsBasePath, rootDir, outDir, ...) are
  // absolute: compare them relative to the checkout
  JSON.stringify(pc.options, (k, v) =>
    k === "configFile" || k === "configFilePath" ? undefined : typeof v === "string" && v.startsWith(cwd) ? "<root>" + v.slice(cwd.length) : v,
  ) +
    JSON.stringify(pc.projectReferences || []),
);

// ---- program ---------------------------------------------------------------

const t = ms();
const program = ts.createProgram({
  rootNames: pc.fileNames,
  options: pc.options,
  projectReferences: pc.projectReferences,
  configFileParsingDiagnostics: ts.getConfigFileParsingDiagnostics(pc),
});
lap("program", t);
const opts = program.getCompilerOptions();
const emitsDeclarations = !opts.noEmit && !!(opts.declaration || opts.composite);

const all = program.getSourceFiles();
const repoFiles = [];
const extHashes = {};
for (const sf of all) {
  const r = rel(sf.fileName);
  if (isRepo(r)) repoFiles.push(sf);
  else {
    // two files with one key (nested installs of one package): both hashes count
    const k = extKey(sf.fileName);
    extHashes[k] = extHashes[k] ? [extHashes[k], sha(sf.text)].sort().join(",") : sha(sf.text);
  }
}
const bySrc = new Map(repoFiles.map((sf) => [rel(sf.fileName), sf]));

// ---- module resolution, as the program resolved it -------------------------

let resCache;
function resolveSpec(sf, spec, mode) {
  try {
    if (typeof program.getResolvedModule === "function") {
      const r = program.getResolvedModule(sf, spec, mode);
      if (r) return r.resolvedModule ? r.resolvedModule.resolvedFileName : null;
    }
  } catch {}
  try {
    if (typeof ts.getResolvedModule === "function") {
      const r = ts.getResolvedModule(sf, spec, mode);
      if (r !== undefined) return r ? r.resolvedFileName : null;
    }
  } catch {}
  resCache = resCache || ts.createModuleResolutionCache(cwd, (f) => f, opts);
  const r = ts.resolveModuleName(spec, sf.fileName, opts, ts.sys, resCache, undefined, mode);
  return r.resolvedModule ? r.resolvedModule.resolvedFileName : null;
}
function modeOf(sf, node) {
  try {
    return ts.getModeForUsageLocation(sf, node, opts);
  } catch {
    return undefined;
  }
}
// [[specifier, resolved repo-relative path | "?"], ...] in source order
function importsOf(sf) {
  const out = [];
  for (const n of sf.imports || []) {
    const f = resolveSpec(sf, n.text, modeOf(sf, n));
    out.push([n.text, f ? keyOf(f) : "?"]);
  }
  for (const m of sf.moduleAugmentations || []) {
    if (!ts.isStringLiteral(m)) continue;
    const f = resolveSpec(sf, m.text, undefined);
    out.push(["augment:" + m.text, f ? keyOf(f) : "?"]);
  }
  for (const r of sf.referencedFiles || []) out.push(["ref:" + r.fileName, keyOf(path.resolve(path.dirname(sf.fileName), r.fileName))]);
  return out;
}
function revOf(importsByFile) {
  const rev = new Map();
  for (const [src, imps] of Object.entries(importsByFile)) {
    for (const [, tgt] of imps) {
      if (tgt === "?" || !isRepo(tgt)) continue;
      if (!rev.has(tgt)) rev.set(tgt, new Set());
      rev.get(tgt).add(src);
    }
  }
  return rev;
}

// files whose declarations reach the global scope or other modules without an import
function globalAffecting(sf) {
  if (sf.isDeclarationFile && !ts.isExternalModule(sf)) return true;
  if (!ts.isExternalModule(sf) && !sf.fileName.endsWith(".json")) return true;
  return !!(sf.moduleAugmentations && sf.moduleAugmentations.length);
}

// ---- d.ts signatures -------------------------------------------------------

function emitDts(sf) {
  let text = null;
  const res = program.emit(
    sf,
    (fileName, data) => {
      if (/\.d\.[cm]?ts$/.test(fileName)) text = data;
    },
    undefined,
    /*emitOnlyDts*/ true,
    undefined,
    /*forceDtsEmit*/ true,
  );
  return { text, err: (res.diagnostics || []).length > 0 };
}

let printer;
function canonicalDts(text) {
  if (text == null) return null;
  printer = printer || ts.createPrinter({ removeComments: false, newLine: ts.NewLineKind.LineFeed });
  const sf = ts.createSourceFile("sig.d.ts", text, ts.ScriptTarget.Latest, true);
  const f = ts.factory;
  const key = (n) => printer.printNode(ts.EmitHint.Unspecified, n, sf);
  const byKey = (a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0);
  const tr = (ctx) => {
    const visit = (node) => {
      node = ts.visitEachChild(node, visit, ctx);
      if (ts.isUnionTypeNode(node)) {
        const sorted = node.types.slice().map((x) => [key(x), x]).sort(byKey).map((x) => x[1]);
        return f.updateUnionTypeNode(node, f.createNodeArray(sorted));
      }
      if (ts.isTypeLiteralNode(node)) {
        const props = node.members.filter(ts.isPropertySignature).map((m) => [key(m), m]).sort(byKey).map((x) => x[1]);
        const rest = node.members.filter((m) => !ts.isPropertySignature(m));
        return f.updateTypeLiteralNode(node, f.createNodeArray([...rest, ...props]));
      }
      return node;
    };
    return (root) => ts.visitNode(root, visit);
  };
  const out = ts.transform(sf, [tr]);
  const text2 = printer.printFile(out.transformed[0]);
  out.dispose();
  return text2;
}

function signatureOf(sf) {
  if (sf.fileName.endsWith(".json")) return { sig: sf.text, dtsErr: false };
  if (sf.isDeclarationFile) return { sig: sf.text, dtsErr: false };
  const { text, err } = emitDts(sf);
  return { sig: canonicalDts(text), dtsErr: err };
}

// ---- per-name interface tables ---------------------------------------------

const ALL = Symbol("ALL");
const EMPTY = new Set();
let nameResCache;
// `mode`: the resolution mode (node16/nodenext ESM vs CJS) of the importing file
function resolveFrom(fromFile, spec, mode) {
  nameResCache = nameResCache || ts.createModuleResolutionCache(cwd, (f) => f, opts);
  const r = ts.resolveModuleName(spec, path.join(cwd, fromFile), opts, ts.sys, nameResCache, undefined, mode);
  const f = r.resolvedModule && r.resolvedModule.resolvedFileName;
  if (!f) return "UNRESOLVED:" + spec;
  const rf = rel(f);
  return isRepo(rf) ? rf : "EXT:" + spec;
}

function dtsTable(file, entry) {
  const text = entry.sig;
  const t = { text, err: !!entry.dtsErr, global: !!entry.global, locals: new Map(), imports: new Map(), exports: new Map(), stars: [], modules: new Set(), opaque: false };
  if (text == null || entry.dtsErr) {
    t.opaque = true;
    return t;
  }
  if (file.endsWith(".json")) {
    t.locals.set("<json>", { text, ids: new Set(), itypes: [] });
    t.opaque = true;
    return t;
  }
  const sf = ts.createSourceFile("t.d.ts", text, ts.ScriptTarget.Latest, true);
  // A script file reaches the global scope with every declaration: it is
  // compared as a whole. A module reaches it only through `declare global`
  // and `declare module "x"` blocks: those become the local `<global>`, whose
  // change (closed over what the blocks refer to) is a global change.
  if (entry.global && !ts.isExternalModule(sf)) t.opaque = true;
  // a declaration file's imports resolve in its source file's module format
  const fileMode = bySrc.has(file) ? bySrc.get(file).impliedNodeFormat : undefined;
  const mod = (spec) => {
    const m = resolveFrom(file, spec, fileMode);
    t.modules.add(m);
    return m;
  };
  const addLocal = (name, node) => {
    const ids = new Set();
    const itypes = [];
    const walk = (n) => {
      if (ts.isIdentifier(n)) ids.add(n.text);
      else if (ts.isImportTypeNode(n) && ts.isLiteralTypeNode(n.argument) && ts.isStringLiteral(n.argument.literal)) {
        const m = mod(n.argument.literal.text);
        let q = n.qualifier;
        while (q && ts.isQualifiedName(q)) q = q.left;
        itypes.push(m + "#" + (q ? q.text : "*"));
      }
      ts.forEachChild(n, walk);
    };
    walk(node);
    const prev = t.locals.get(name);
    const txt = node.getText(sf);
    if (prev) {
      prev.text += "\n" + txt;
      for (const i of ids) prev.ids.add(i);
      prev.itypes.push(...itypes);
    } else t.locals.set(name, { text: txt, ids, itypes });
  };
  const flags = (n) => (ts.getCombinedModifierFlags ? ts.getCombinedModifierFlags(n) : 0);
  const isExported = (n) => flags(n) & ts.ModifierFlags.Export;
  const isDefault = (n) => flags(n) & ts.ModifierFlags.Default;
  for (const st of sf.statements) {
    if (ts.isImportDeclaration(st)) {
      if (!ts.isStringLiteral(st.moduleSpecifier)) {
        t.opaque = true;
        continue;
      }
      const m = mod(st.moduleSpecifier.text);
      const c = st.importClause;
      if (!c) continue;
      if (c.name) t.imports.set(c.name.text, m + "#default");
      if (c.namedBindings) {
        if (ts.isNamespaceImport(c.namedBindings)) t.imports.set(c.namedBindings.name.text, m + "#*");
        else for (const el of c.namedBindings.elements) t.imports.set(el.name.text, m + "#" + (el.propertyName || el.name).text);
      }
    } else if (ts.isExportDeclaration(st)) {
      if (st.moduleSpecifier) {
        if (!ts.isStringLiteral(st.moduleSpecifier)) {
          t.opaque = true;
          continue;
        }
        const m = mod(st.moduleSpecifier.text);
        if (!st.exportClause) t.stars.push(m);
        else if (ts.isNamespaceExport(st.exportClause)) t.exports.set(st.exportClause.name.text, "R:" + m + "#*");
        else for (const el of st.exportClause.elements) t.exports.set(el.name.text, "R:" + m + "#" + (el.propertyName || el.name).text);
      } else if (st.exportClause && ts.isNamedExports(st.exportClause)) {
        for (const el of st.exportClause.elements) t.exports.set(el.name.text, "L:" + (el.propertyName || el.name).text);
      }
    } else if (ts.isExportAssignment(st)) {
      if (st.isExportEquals) {
        t.opaque = true;
        continue;
      }
      if (ts.isIdentifier(st.expression)) t.exports.set("default", "L:" + st.expression.text);
      else {
        addLocal("<default-expr>", st);
        t.exports.set("default", "L:<default-expr>");
      }
    } else if (ts.isImportEqualsDeclaration(st)) {
      if (ts.isExternalModuleReference(st.moduleReference) && ts.isStringLiteral(st.moduleReference.expression)) {
        t.imports.set(st.name.text, mod(st.moduleReference.expression.text) + "#*");
        if (isExported(st)) t.exports.set(st.name.text, "L:" + st.name.text);
      } else t.opaque = true;
    } else if (ts.isVariableStatement(st)) {
      for (const d of st.declarationList.declarations) {
        if (!ts.isIdentifier(d.name)) {
          t.opaque = true;
          continue;
        }
        addLocal(d.name.text, st);
        if (isExported(st)) t.exports.set(d.name.text, "L:" + d.name.text);
      }
    } else if (
      ts.isFunctionDeclaration(st) ||
      ts.isClassDeclaration(st) ||
      ts.isInterfaceDeclaration(st) ||
      ts.isTypeAliasDeclaration(st) ||
      ts.isEnumDeclaration(st) ||
      ts.isModuleDeclaration(st)
    ) {
      if (ts.isModuleDeclaration(st) && (!ts.isIdentifier(st.name) || st.flags & ts.NodeFlags.GlobalAugmentation)) {
        addLocal("<global>", st); // declare module "x" / declare global
        continue;
      }
      const name = st.name ? st.name.text : "<anon-default>";
      addLocal(name, st);
      if (isExported(st)) t.exports.set(isDefault(st) ? "default" : name, "L:" + name);
    } else if (st.kind === ts.SyntaxKind.EmptyStatement) {
      // nothing
    } else t.opaque = true;
  }
  return t;
}

// {names, global}: the exported names whose declaration differs between the
// two tables (closed over local references and over imported names changed in
// their module), and whether what the file contributes to the global scope
// changed.
function changedExports(nw, od, ceOf) {
  if (!od) return { names: ALL, global: nw.global };
  if (nw.opaque || od.opaque) {
    // compared as a whole: unchanged iff the same d.ts text, emitted without
    // errors, referring to no module whose interface changed
    const whole = { names: ALL, global: nw.global || od.global };
    if (nw.text == null || nw.err || od.err || nw.text !== od.text) return whole;
    for (const m of new Set([...nw.modules, ...od.modules])) {
      if (m.startsWith("UNRESOLVED:")) return whole;
      if (m.startsWith("EXT:")) continue;
      const c = ceOf(m);
      if (c === ALL || c.size > 0) return whole;
    }
    return { names: new Set(), global: false };
  }
  const modChanged = (ref) => {
    const i = ref.lastIndexOf("#");
    const m = ref.slice(0, i),
      n = ref.slice(i + 1);
    if (m.startsWith("EXT:")) return false; // non-repo modules are validated by hash
    if (m.startsWith("UNRESOLVED:")) return true;
    const c = ceOf(m);
    if (c === ALL) return true;
    return n === "*" ? c.size > 0 : c.has(n);
  };
  const memo = new Map();
  const localChanged = (name) => {
    if (memo.has(name)) return memo.get(name);
    memo.set(name, false); // cycle guard; the fixed point below completes it
    const a = nw.locals.get(name),
      b = od.locals.get(name);
    let ch = false;
    if (!a || !b || a.text !== b.text) ch = true;
    else {
      for (const r of [...a.itypes, ...b.itypes])
        if (modChanged(r)) {
          ch = true;
          break;
        }
      if (!ch)
        for (const id of new Set([...a.ids, ...b.ids])) {
          if (id === name) continue;
          if (nw.locals.has(id) || od.locals.has(id)) {
            if (localChanged(id)) {
              ch = true;
              break;
            }
          }
          const ia = nw.imports.get(id),
            ib = od.imports.get(id);
          if (ia !== ib) {
            ch = true;
            break;
          }
          if (ia && modChanged(ia)) {
            ch = true;
            break;
          }
        }
    }
    memo.set(name, ch);
    return ch;
  };
  for (let round = 0; round < 50; round++) {
    const before = [...memo].filter(([, v]) => v).length;
    for (const k of memo.keys()) if (!memo.get(k)) memo.delete(k);
    for (const n of new Set([...nw.locals.keys(), ...od.locals.keys()])) localChanged(n);
    const after = [...memo].filter(([, v]) => v).length;
    if (after === before && round > 0) break;
  }
  const global = nw.locals.has("<global>") || od.locals.has("<global>") ? localChanged("<global>") : false;
  const starsKey = (x) => x.stars.slice().sort().join("|");
  if (starsKey(nw) !== starsKey(od)) return { names: ALL, global };
  const out = new Set();
  const exportChanged = (e) => {
    if (e.startsWith("R:")) return modChanged(e.slice(2));
    const l = e.slice(2);
    if (nw.locals.has(l) || od.locals.has(l)) return localChanged(l);
    const ia = nw.imports.get(l),
      ib = od.imports.get(l);
    if (ia !== ib) return true;
    return ia ? modChanged(ia) : true;
  };
  for (const n of new Set([...nw.exports.keys(), ...od.exports.keys()])) {
    const a = nw.exports.get(n),
      b = od.exports.get(n);
    if (!a || !b || a !== b || exportChanged(a)) out.add(n);
  }
  for (const m of nw.stars) {
    const c = ceOf(m);
    if (c === ALL) return { names: ALL, global };
    for (const n of c) if (!nw.exports.has(n)) out.add(n);
  }
  return { names: out, global };
}

// The names `sf` uses from module `target` (Set, or ALL).
function usedNames(sf, target) {
  const from = rel(sf.fileName);
  if (/\.[cm]?jsx?$/.test(from) || target.endsWith(".json") || /\.[cm]?jsx?$/.test(target)) return ALL;
  const used = new Set();
  let all = false,
    found = false;
  // the program's own resolution of this very import (its resolution mode included)
  const hit = (spec, lit) => {
    const f = resolveSpec(sf, spec, lit ? modeOf(sf, lit) : undefined);
    return !!f && keyOf(f) === target;
  };
  const visit = (n) => {
    if (all) return;
    if (ts.isImportDeclaration(n) && ts.isStringLiteral(n.moduleSpecifier) && hit(n.moduleSpecifier.text, n.moduleSpecifier)) {
      found = true;
      const c = n.importClause;
      if (c) {
        if (c.name) used.add("default");
        if (c.namedBindings) {
          if (ts.isNamespaceImport(c.namedBindings)) all = true;
          else for (const el of c.namedBindings.elements) used.add((el.propertyName || el.name).text);
        }
      }
    } else if (ts.isExportDeclaration(n) && n.moduleSpecifier && ts.isStringLiteral(n.moduleSpecifier) && hit(n.moduleSpecifier.text, n.moduleSpecifier)) {
      found = true;
      if (!n.exportClause || ts.isNamespaceExport(n.exportClause)) all = true;
      else for (const el of n.exportClause.elements) used.add((el.propertyName || el.name).text);
    } else if (
      ts.isImportEqualsDeclaration(n) &&
      ts.isExternalModuleReference(n.moduleReference) &&
      ts.isStringLiteral(n.moduleReference.expression) &&
      hit(n.moduleReference.expression.text, n.moduleReference.expression)
    ) {
      found = all = true;
    } else if (ts.isImportTypeNode(n) && ts.isLiteralTypeNode(n.argument) && ts.isStringLiteral(n.argument.literal) && hit(n.argument.literal.text, n.argument.literal)) {
      found = all = true;
    } else if (
      ts.isCallExpression(n) &&
      n.arguments.length &&
      ts.isStringLiteral(n.arguments[0]) &&
      (n.expression.kind === ts.SyntaxKind.ImportKeyword || (ts.isIdentifier(n.expression) && n.expression.text === "require")) &&
      hit(n.arguments[0].text, n.arguments[0])
    ) {
      found = all = true;
    } else if (ts.isModuleDeclaration(n) && ts.isStringLiteral(n.name) && hit(n.name.text, null)) {
      found = all = true;
    }
    ts.forEachChild(n, visit);
  };
  visit(sf);
  if (all || !found) return ALL; // no reference TS can see: be conservative
  return used;
}

// ---- diagnostics -----------------------------------------------------------

function fileDiags(sf) {
  const e = { diags: program.getSemanticDiagnostics(sf).map(fmt) };
  if (emitsDeclarations && !sf.isDeclarationFile) e.decl = program.getDeclarationDiagnostics(sf).map(fmt);
  return e;
}

// Mirrors tsc's emitFilesAndReportErrors: syntactic first; options and global
// only when there are none; semantic only when nothing before it; declaration
// diagnostics only when nothing at all. Every entry is tsc's own text.
function verdict(configDiags, syntactic, optionsDiags, globalDiags, perFile) {
  const out = configDiags.map(fmt);
  const cfgLen = out.length;
  out.push(...syntactic.map(fmt));
  if (out.length === cfgLen) {
    out.push(...optionsDiags.map(fmt));
    out.push(...globalDiags.map(fmt));
    if (out.length === cfgLen) {
      for (const f of Object.keys(perFile).sort()) out.push(...perFile[f].diags);
      if (out.length === cfgLen && emitsDeclarations) for (const f of Object.keys(perFile).sort()) out.push(...(perFile[f].decl || []));
    }
  }
  const errors = out.filter((t) => /^(?:[^\n]*\(\d+,\d+\): )?error TS\d+: /.test(t)).length;
  return { diagnostics: out.sort(), errors };
}

// ---- state -----------------------------------------------------------------

function readState(p) {
  if (!p || !fs.existsSync(p)) return null;
  try {
    const buf = fs.readFileSync(p);
    const txt = buf[0] === 0x1f && buf[1] === 0x8b ? zlib.gunzipSync(buf).toString("utf8") : buf.toString("utf8");
    const s = JSON.parse(txt);
    return { s, digest: crypto.createHash("sha256").update(buf).digest("hex") };
  } catch (e) {
    return { s: null, digest: null, error: String(e) };
  }
}
function writeState(p, s) {
  const buf = zlib.gzipSync(Buffer.from(JSON.stringify(s)), { level: 1 });
  const tmp = p + ".tmp" + process.pid;
  fs.writeFileSync(tmp, buf);
  fs.renameSync(tmp, p);
  return crypto.createHash("sha256").update(buf).digest("hex");
}

// ---- main ------------------------------------------------------------------

const tSyn = ms();
const configDiags = ts.getConfigFileParsingDiagnostics(pc);
const syntactic = program.getSyntacticDiagnostics();
const optionsDiags = program.getOptionsDiagnostics();
lap("syntactic+options", tSyn);

const tImp = ms();
const newImports = {};
for (const sf of repoFiles) newImports[rel(sf.fileName)] = importsOf(sf);
lap("imports", tImp);

const nowManifests = manifestsOf();
const loaded = flag("--full") ? null : readState(arg("--state-in"));
const old = loaded && loaded.s;
const reasons = [];
if (flag("--full")) reasons.push("full: requested");
else if (!loaded) reasons.push("no-state: no earlier check state to start from");
else if (!old) reasons.push("no-state: the state file could not be read (" + loaded.error + ")");
else {
  if (old.helper !== HELPER_VERSION) reasons.push(`helper-version: ${old.helper} -> ${HELPER_VERSION}`);
  if (old.tsVersion !== ts.version) reasons.push(`ts-version: ${old.tsVersion} -> ${ts.version}`);
  if (old.project !== rel(configPath)) reasons.push(`config: project ${old.project} -> ${rel(configPath)}`);
  for (const f of new Set([...Object.keys(old.config || {}), ...Object.keys(configHashes)]))
    if ((old.config || {})[f] !== configHashes[f]) reasons.push(`config: ${f}`);
  if (old.options !== optionsKey && !reasons.some((r) => r.startsWith("config:"))) reasons.push("config: compiler options");
  const extChanged = [];
  for (const f of new Set([...Object.keys(old.ext || {}), ...Object.keys(extHashes)])) if ((old.ext || {})[f] !== extHashes[f]) extChanged.push(f);
  if (extChanged.length)
    reasons.push(`external: ${extChanged.length} non-repo program file(s) differ (${extChanged.slice(0, 3).join(", ")}${extChanged.length > 3 ? ", ..." : ""})`);
  const fmtChanged = repoFiles.map((sf) => rel(sf.fileName)).filter((f) => old.files[f] && old.files[f].fmt !== (bySrc.get(f).impliedNodeFormat || 0));
  if (fmtChanged.length) reasons.push(`module-format: ${fmtChanged.slice(0, 3).join(", ")}`);
  // diagnostics in non-repo files name them relative to the checkout
  if (Object.keys(old.outside || {}).length && old.root !== cwd) reasons.push("external-diagnostics: the state's non-repo diagnostics were recorded in another checkout");
  const was = old.manifests || {};
  for (const f of new Set([...Object.keys(was), ...Object.keys(nowManifests)]))
    if (was[f] !== nowManifests[f]) reasons.push(`manifest: ${f} (a field that steers module resolution or module format changed)`);
}

function manifestKey(f) {
  try {
    const j = JSON.parse(fs.readFileSync(path.join(cwd, f), "utf8"));
    const keys = ["name", "type", "main", "module", "types", "typings", "typesVersions", "exports", "imports", "browser"];
    return sha(JSON.stringify(keys.map((k) => j[k])));
  } catch {
    return "missing";
  }
}
// every package.json from a repo program file up to the project root
function manifestsOf() {
  const seen = new Set();
  for (const sf of repoFiles) {
    let d = path.dirname(sf.fileName);
    while (true) {
      const r = rel(d);
      if (r.startsWith("..")) break;
      const p = (r ? r + "/" : "") + "package.json";
      if (seen.has(p)) break;
      seen.add(p);
      if (!r) break;
      d = path.dirname(d);
    }
  }
  const out = {};
  for (const p of seen) if (fs.existsSync(path.join(cwd, p))) out[p] = manifestKey(p);
  return out;
}

const newFiles = {};
let mode = "full";
let recheck;
let interfaceChanged = [];

function fullCheck() {
  const tc = ms();
  for (const sf of all) {
    const r = rel(sf.fileName);
    const e = fileDiags(sf);
    if (bySrc.has(r)) newFiles[r] = e;
    else if (e.diags.length || (e.decl && e.decl.length)) outside[r] = e;
  }
  lap("check-all", tc);
  recheck = new Set(bySrc.keys());
}
const outside = {};

if (!reasons.length) {
  const r = sliceByName();
  if (r.escalate.length) reasons.push(...[...new Set(r.escalate)].map((f) => `global-interface: ${f}`));
  else {
    mode = "incremental";
    recheck = r.R;
    interfaceChanged = r.EI;
    Object.assign(outside, old.outside || {});
  }
}
if (mode === "full") {
  for (const k of Object.keys(newFiles)) delete newFiles[k];
  fullCheck();
}

function sliceByName() {
  const oldFiles = old.files;
  const revNew = revOf(newImports);
  const revOld = revOf(Object.fromEntries(Object.entries(oldFiles).map(([f, e]) => [f, e.imports || []])));
  const R = new Set();
  const queue = [];
  const escalate = [];
  const enqueue = (f) => {
    if (bySrc.has(f) && !R.has(f)) {
      R.add(f);
      queue.push(f);
    }
  };
  // seeds
  const removed = Object.keys(oldFiles).filter((f) => !bySrc.has(f));
  for (const [f, sf] of bySrc) {
    const o = oldFiles[f];
    if (!o || o.h !== sha(sf.text)) enqueue(f);
    else if (JSON.stringify(o.imports || []) !== JSON.stringify(newImports[f])) enqueue(f);
  }
  for (const f of removed) for (const i of revOld.get(f) || []) enqueue(i);

  const CE = new Map();
  const tables = new Map();
  const dependents = new Map();
  for (const f of removed) {
    CE.set(f, ALL);
    if (oldFiles[f].global) escalate.push(f);
  }
  const ceOf = (m) => CE.get(m) || EMPTY;
  const nonEmpty = (c) => c === ALL || c.size > 0;
  const td = ms();
  const importerCheck = (x) => {
    const c = ceOf(x);
    if (!nonEmpty(c)) return;
    for (const i of revNew.get(x) || []) {
      if (R.has(i) || !bySrc.has(i)) continue;
      const used = usedNames(bySrc.get(i), x);
      if (used === ALL ? true : c === ALL ? used.size > 0 : [...used].some((n) => c.has(n))) enqueue(i);
    }
  };
  const evaluate = (x) => {
    const tb = tables.get(x);
    const r = changedExports(tb.nw, tb.od, ceOf);
    if (r.global) escalate.push(x);
    const next = r.names;
    const prev = CE.get(x);
    const grew = next === ALL ? prev !== ALL : prev !== ALL && [...next].some((n) => !(prev || EMPTY).has(n));
    if (!prev || grew) {
      const merged = next === ALL || prev === ALL ? ALL : new Set([...(prev || []), ...next]);
      CE.set(x, merged);
      if (grew || (!prev && nonEmpty(merged))) {
        importerCheck(x);
        for (const d of dependents.get(x) || []) evaluate(d);
      }
    }
  };
  for (const f of removed) importerCheck(f);
  while (queue.length && !escalate.length) {
    const f = queue.shift();
    const sf = bySrc.get(f);
    const e = signatureOf(sf);
    e.global = globalAffecting(sf);
    newFiles[f] = e;
    const o = oldFiles[f];
    const nw = dtsTable(f, e);
    const od = o ? dtsTable(f, o) : null;
    tables.set(f, { nw, od });
    for (const tb of [nw, od])
      if (tb)
        for (const m of tb.modules) {
          if (!dependents.has(m)) dependents.set(m, new Set());
          dependents.get(m).add(f);
        }
    evaluate(f);
  }
  lap("dts-propagate", td);
  if (escalate.length) return { escalate };
  const tc = ms();
  for (const sf of repoFiles) {
    const f = rel(sf.fileName);
    if (R.has(f)) Object.assign(newFiles[f], fileDiags(sf));
  }
  lap("check-slice", tc);
  const EI = [...CE].filter(([, c]) => nonEmpty(c)).map(([f, c]) => (c === ALL ? f + " *" : f + " " + [...c].sort().join(","))).sort();
  return { escalate, R, EI };
}

// ---- verdict ---------------------------------------------------------------

const globalDiags = program.getGlobalDiagnostics();
const perFile = {};
const files = {};
for (const [f, sf] of bySrc) {
  const e = newFiles[f] || old.files[f];
  files[f] = e;
  if (e.diags.length || (e.decl && e.decl.length)) perFile[f] = e;
}
for (const [f, e] of Object.entries(outside)) perFile[f] = e;
const v = verdict(configDiags, syntactic, optionsDiags, globalDiags, perFile);

const result = {
  verdict: v.errors > 0 ? "fail" : "pass",
  mode,
  reasons: mode === "full" ? reasons : ["early cutoff: only files a change can affect were rechecked"],
  tsVersion: ts.version,
  node: process.version,
  helper: HELPER_VERSION,
  project: rel(configPath),
  programFiles: bySrc.size,
  externalFiles: Object.keys(extHashes).length,
  recheck: [...recheck].sort(),
  interfaceChanged,
  diagnostics: v.diagnostics,
  errors: v.errors,
  stateIn: loaded && loaded.digest,
};

// ---- next state: signatures for every file that has none yet -----------------

const stateOut = arg("--state-out");
if (stateOut) {
  const tw = ms();
  const outFiles = {};
  for (const [f, sf] of bySrc) {
    let e = files[f];
    const fresh = newFiles[f];
    if (!fresh || fresh.sig === undefined) {
      const reuse = mode === "incremental" && old.files[f] && !fresh;
      if (reuse) e = old.files[f];
      else {
        const s = signatureOf(sf);
        e = Object.assign({}, e, s, { global: globalAffecting(sf) });
      }
    }
    outFiles[f] = {
      h: sha(sf.text),
      fmt: sf.impliedNodeFormat || 0,
      global: e.global,
      sig: e.sig,
      dtsErr: e.dtsErr,
      imports: newImports[f],
      diags: e.diags,
      decl: e.decl,
    };
  }
  lap("signatures", tw);
  const tws = ms();
  result.stateOut = writeState(stateOut, {
    helper: HELPER_VERSION,
    tsVersion: ts.version,
    root: cwd,
    project: rel(configPath),
    config: configHashes,
    options: optionsKey,
    manifests: nowManifests,
    files: outFiles,
    ext: extHashes,
    outside,
  });
  lap("write-state", tws);
}
finish(result, 0);
