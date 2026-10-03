#!/usr/bin/env node
// TypeScript / JavaScript call-site front-end for `sem system`, using the
// TypeScript checker as the resolver. Writes one JSON line per call site in
// the same shape as sem's call pipeline (`SEM_CALLS_SITES`):
//
//   {"file","at","line","call":true,"defs":[{"file","line","name"}]|null,
//    "unknown":reason|null,"callee"}
//
// defs = the declaration the checker resolves the call to (repo file,
// node_modules .d.ts, or lib.*.d.ts); defs null + unknown null = external
// (the target is outside the input); unknown = the checker has no target.
//
// Usage: node ts-sites.mjs <repo root> [--no-deps] > sites.jsonl
//   --no-deps: base layer — no lib.*.d.ts, no @types, bare module
//   specifiers unresolved (only the repo's own files are in the input).
// Needs the `typescript` package resolvable (e.g. NODE_PATH=$(npm root -g)).
import { createRequire } from "node:module";
import fs from "node:fs";
import path from "node:path";

const require = createRequire(import.meta.url);
const ts = require("typescript");

const root = path.resolve(process.argv[2] || ".");
const noDeps = process.argv.includes("--no-deps");
const SKIP = new Set(["node_modules", ".git", "dist", "build", ".sem-system", "coverage", ".next", "vendor", "target"]);
const TEST = /(^|\/)(tests?|specs?|__tests__|__mocks__|e2e|fixtures?|cypress|playwright|benchmarks?)(\/|$)|[._-](test|spec)\.[cm]?[jt]sx?$/i;

function walk(dir, out) {
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    if (e.isDirectory()) {
      if (!SKIP.has(e.name) && !e.name.startsWith(".")) walk(path.join(dir, e.name), out);
    } else if (/\.(tsx?|mts|cts|jsx?|mjs|cjs)$/.test(e.name) && !e.name.endsWith(".d.ts") && !e.name.endsWith(".min.js")) {
      out.push(path.join(dir, e.name));
    }
  }
  return out;
}

const files = walk(root, []).filter((f) => !TEST.test(path.relative(root, f)));

// compiler options: the root tsconfig when there is one, forced to no-emit
let options = {};
const cfgPath = ts.findConfigFile(root, ts.sys.fileExists, "tsconfig.json");
if (cfgPath && path.dirname(cfgPath) === root) {
  const cfg = ts.readConfigFile(cfgPath, ts.sys.readFile);
  options = ts.parseJsonConfigFileContent(cfg.config || {}, ts.sys, root).options;
}
options = {
  ...options,
  allowJs: true,
  checkJs: false,
  noEmit: true,
  skipLibCheck: true,
  jsx: options.jsx ?? ts.JsxEmit.Preserve,
  target: options.target ?? ts.ScriptTarget.ES2022,
  module: options.module ?? ts.ModuleKind.ESNext,
  moduleResolution: options.moduleResolution ?? ts.ModuleResolutionKind.Node10,
  experimentalDecorators: true,
  esModuleInterop: true,
};
if (noDeps) {
  options = { ...options, noLib: true, types: [], typeRoots: [] };
}

const host = ts.createCompilerHost(options, true);
if (noDeps) {
  const bare = (s) => !s.startsWith(".") && !s.startsWith("/") && !(options.paths && Object.keys(options.paths).some((p) => new RegExp("^" + p.replace("*", ".*") + "$").test(s)));
  host.resolveModuleNameLiterals = (lits, containing, redirected, opts) =>
    lits.map((lit) => {
      const s = lit.text;
      if (bare(s)) return { resolvedModule: undefined };
      const r = ts.resolveModuleName(s, containing, opts, host);
      const rm = r.resolvedModule;
      if (rm && rm.resolvedFileName.includes("/node_modules/")) return { resolvedModule: undefined };
      return r;
    });
}
const program = ts.createProgram({ rootNames: files, options, host });
const checker = program.getTypeChecker();

function squash(text) {
  let out = "";
  let depth = 0;
  for (const c of text) {
    if (c === "(") { if (depth === 0) out += "("; depth++; }
    else if (c === ")") { depth = Math.max(0, depth - 1); if (depth === 0) out += ")"; }
    else if (/\s/.test(c)) continue;
    else if (depth === 0) out += c;
    if (out.length >= 200) break;
  }
  return out;
}

function rel(f) {
  const r = path.relative(root, f);
  if (!r.startsWith("..")) return r.split(path.sep).join("/");
  if (f.includes("/node_modules/typescript/lib/")) return "<tslib>/" + path.basename(f);
  return f;
}

function declOf(decl) {
  const sf = decl.getSourceFile();
  const { line } = sf.getLineAndCharacterOfPosition(decl.getStart(sf));
  let name = "";
  if (decl.name && decl.name.getText) name = decl.name.getText(sf);
  else if (ts.isConstructorDeclaration(decl)) name = "constructor";
  return { file: rel(sf.fileName), line: line + 1, name };
}

// leftmost identifier of a callee: a.b.c() -> a
function rootIdent(expr) {
  let e = expr;
  while (e) {
    if (ts.isPropertyAccessExpression(e) || ts.isElementAccessExpression(e)) e = e.expression;
    else if (ts.isCallExpression(e) || ts.isNonNullExpression(e) || ts.isParenthesizedExpression(e) || ts.isAsExpression(e)) e = e.expression;
    else break;
  }
  return e && ts.isIdentifier(e) ? e : undefined;
}

function typeNameRoot(t) {
  if (!t) return undefined;
  if (ts.isTypeReferenceNode(t)) {
    let n = t.typeName;
    while (ts.isQualifiedName(n)) n = n.left;
    return n;
  }
  return undefined;
}

function importedFromOutside(ident, depth = 0) {
  const sym = checker.getSymbolAtLocation(ident);
  if (!sym) return true; // a global the input does not declare (stdlib)
  for (const d of sym.declarations || []) {
    // a parameter / variable whose declared type comes from outside the input
    if (depth < 3 && (ts.isParameter(d) || ts.isVariableDeclaration(d) || ts.isPropertyDeclaration(d)) && d.type) {
      const root = typeNameRoot(d.type);
      if (root && importedFromOutside(root, depth + 1)) return true;
    }
    let n = d;
    while (n && !ts.isImportDeclaration(n) && !ts.isVariableDeclaration(n)) n = n.parent;
    if (n && ts.isImportDeclaration(n)) {
      const spec = n.moduleSpecifier.text;
      if (!spec.startsWith(".") && !spec.startsWith("/")) return true;
    }
    if (n && ts.isVariableDeclaration(n) && n.initializer && ts.isCallExpression(n.initializer)) {
      const c = n.initializer;
      if (ts.isIdentifier(c.expression) && c.expression.text === "require" && c.arguments[0] && ts.isStringLiteral(c.arguments[0])) {
        const spec = c.arguments[0].text;
        if (!spec.startsWith(".") && !spec.startsWith("/")) return true;
      }
    }
    const sf = d.getSourceFile();
    if (sf.isDeclarationFile || sf.fileName.includes("/node_modules/")) return true;
  }
  return false;
}

const out = [];
for (const sf of program.getSourceFiles()) {
  if (sf.isDeclarationFile || sf.fileName.includes("/node_modules/") || !files.includes(sf.fileName)) continue;
  const file = rel(sf.fileName);
  const visit = (node) => {
    if (ts.isCallExpression(node) || ts.isNewExpression(node)) {
      const callee = node.expression;
      let defs = null;
      let unknown = null;
      let sig;
      try { sig = checker.getResolvedSignature(node); } catch { sig = undefined; }
      let decl = sig && sig.declaration;
      if (!decl || ts.isJSDocSignature?.(decl)) {
        const sym = checker.getSymbolAtLocation(ts.isPropertyAccessExpression(callee) ? callee.name : callee);
        const target = sym && sym.flags & ts.SymbolFlags.Alias ? checker.getAliasedSymbol(sym) : sym;
        const d = target && (target.valueDeclaration || (target.declarations || [])[0]);
        if (d) decl = d;
      }
      if (decl) {
        defs = [declOf(decl)];
      } else {
        const id = rootIdent(callee);
        const t = checker.getTypeAtLocation(callee);
        if (id && importedFromOutside(id)) unknown = null; // external
        else if (callee.kind === ts.SyntaxKind.SuperKeyword) unknown = "super outside the input";
        else if (t.flags & ts.TypeFlags.Any) unknown = "callee type unknown (any)";
        else unknown = "no declaration";
        if (unknown === null && defs === null) {
          // external: leave both null
        }
      }
      const pos = node.getStart(sf);
      const { line } = sf.getLineAndCharacterOfPosition(pos);
      out.push(JSON.stringify({ file, at: pos, line: line + 1, call: true, defs, unknown, callee: squash(callee.getText(sf)) }));
    }
    ts.forEachChild(node, visit);
  };
  visit(sf);
}
process.stdout.write(out.join("\n") + (out.length ? "\n" : ""));
process.stderr.write(`ts-sites: ${out.length} call sites in ${files.length} files${noDeps ? " (no deps)" : ""}\n`);
