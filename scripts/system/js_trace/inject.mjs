#!/usr/bin/env node
// Instrument JS/TS sources for `sem system`'s dynamic layer: every function
// body with a block gets a call to the tracer hook right after its `{`, on
// the same line, so recorded line numbers match the untouched source.
// Expression-bodied arrows (`x => x + 1`) are left alone (not recorded).
// Constructors of derived classes are skipped (their first statement may
// have to be `super(...)`).
//
// Usage: node inject.mjs <dir> [<dir>...]   (run on a scratch copy; needs
// the `typescript` package, e.g. NODE_PATH=$(npm root -g))
import { createRequire } from "node:module";
import fs from "node:fs";
import path from "node:path";

const require = createRequire(import.meta.url);
const ts = require("typescript");
const SKIP = new Set(["node_modules", ".git", "dist", "build", "coverage", ".sem-system", ".next"]);
const TEST = /(^|\/)(tests?|specs?|__tests__|__mocks__|e2e|fixtures?)(\/|$)|[._-](test|spec)\.[cm]?[jt]sx?$/i;

function walk(dir, out) {
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) {
      if (!SKIP.has(e.name) && !e.name.startsWith(".")) walk(p, out);
    } else if (/\.(tsx?|mts|cts|jsx?|mjs|cjs)$/.test(e.name) && !e.name.endsWith(".d.ts") && !e.name.endsWith(".min.js")) {
      out.push(p);
    }
  }
  return out;
}

let files = 0, fns = 0;
for (const root of process.argv.slice(2)) {
  for (const f of walk(path.resolve(root), [])) {
    if (TEST.test(path.relative(root, f))) continue;
    const src = fs.readFileSync(f, "utf8");
    const isTs = /\.(tsx?|mts|cts)$/.test(f);
    const kind = f.endsWith("x") ? (isTs ? ts.ScriptKind.TSX : ts.ScriptKind.JSX) : isTs ? ts.ScriptKind.TS : ts.ScriptKind.JS;
    const sf = ts.createSourceFile(f, src, ts.ScriptTarget.Latest, true, kind);
    const hook = isTs
      ? "(globalThis as any).__systrace && (globalThis as any).__systrace();"
      : "globalThis.__systrace && globalThis.__systrace();";
    const inserts = [];
    const visit = (n) => {
      const body = n.body;
      const isFn = ts.isFunctionDeclaration(n) || ts.isFunctionExpression(n) || ts.isMethodDeclaration(n) ||
        ts.isArrowFunction(n) || ts.isGetAccessorDeclaration(n) || ts.isSetAccessorDeclaration(n) || ts.isConstructorDeclaration(n);
      if (isFn && body && ts.isBlock(body)) {
        let ok = true;
        if (ts.isConstructorDeclaration(n)) {
          const cls = n.parent;
          const derived = cls && cls.heritageClauses && cls.heritageClauses.some((h) => h.token === ts.SyntaxKind.ExtendsKeyword);
          if (derived) ok = false;
        }
        const first = body.statements[0];
        if (first && ts.isExpressionStatement(first) && ts.isStringLiteral(first.expression)) ok = false; // "use strict"
        if (n.asteriskToken && ts.isFunctionDeclaration(n) === false && ts.isMethodDeclaration(n) === false) ok = ok; // generators fine
        if (ok) inserts.push(body.getStart(sf) + 1);
      }
      ts.forEachChild(n, visit);
    };
    visit(sf);
    if (!inserts.length) continue;
    inserts.sort((a, b) => b - a);
    let out = src;
    for (const at of inserts) out = out.slice(0, at) + hook + out.slice(at);
    fs.writeFileSync(f, out);
    files++;
    fns += inserts.length;
  }
}
process.stderr.write(`js inject: ${fns} functions in ${files} files\n`);
