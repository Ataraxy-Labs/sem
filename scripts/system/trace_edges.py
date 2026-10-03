#!/usr/bin/env python3
"""Turn raw runtime traces into `sem system build --trace` edges.

  trace_edges.py cpuprof DIR...            V8 .cpuprofile files (sampled stacks)
  trace_edges.py callgrind FILE...         valgrind callgrind.out files (exact)
  trace_edges.py jsonl FILE_OR_DIR...      py-/go- tracer JSON lines (exact)

Each writes normalized JSON lines to stdout:
  {"caller_file","caller_line","caller","site_line","callee_file","callee_line","callee"}
`site_line` is 0 when the tracer knows no call-site line (sampled stacks).

--map FROM=TO (repeatable) rewrites path prefixes (container paths to
world-relative ones: the repo root to "", a dependency root to its
`.sem-system/links/<i>/<name>` link); paths matching no rule become
"<external>" and edges with both ends external are dropped.
"""
import argparse
import glob
import json
import os
import re
import sys


IMPORT_MACHINERY = ("<frozen importlib._bootstrap>", "<frozen importlib._bootstrap_external>")


def rewrite(path, rules):
    if path and path.startswith("<frozen ") and path.endswith(">"):
        # CPython's frozen stdlib modules: "<frozen posixpath>" -> posixpath.py
        # under the rule for "<frozen>" (the stdlib root), if any
        mod = path[len("<frozen "):-1]
        for frm, to in rules:
            if frm == "<frozen>":
                return to.rstrip("/") + "/" + mod.replace(".", "/") + ".py"
        return "<external>"
    if not path or path.startswith("<"):
        return path or "<external>"
    if path.startswith("file://"):
        path = path[len("file://"):]
    for frm, to in rules:
        if "*" in frm:
            # a `*` matches one path segment (registry hashes, rustc commit ids)
            m = re.match(re.escape(frm.rstrip("/")).replace(r"\*", "[^/]+") + r"(/|$)", path)
            if m:
                rest = path[m.end():].lstrip("/")
                return (to.rstrip("/") + "/" + rest) if to else rest
            continue
        if path == frm or path.startswith(frm.rstrip("/") + "/"):
            rest = path[len(frm):].lstrip("/")
            return (to.rstrip("/") + "/" + rest) if to else rest
    return "<external>"


def emit(rows, rules, out):
    seen = set()
    n = 0
    for r in rows:
        if r.get("callee_file") in IMPORT_MACHINERY:
            continue  # an import statement, not a call
        cf = rewrite(r["caller_file"], rules)
        tf = rewrite(r["callee_file"], rules)
        if cf == "<external>" and tf in ("<external>", "<builtin>"):
            continue
        key = (cf, r.get("caller_line", 0), r.get("site_line", 0), tf, r.get("callee_line", 0), r.get("callee", ""))
        if key in seen:
            continue
        seen.add(key)
        r = dict(r, caller_file=cf, callee_file=tf)
        out.write(json.dumps(r) + "\n")
        n += 1
    return n


def cpuprof_rows(paths):
    files = []
    for p in paths:
        files += glob.glob(os.path.join(p, "*.cpuprofile")) if os.path.isdir(p) else [p]
    skip = {"(root)", "(program)", "(idle)", "(garbage collector)"}
    for f in files:
        try:
            prof = json.load(open(f))
        except Exception:
            continue
        nodes = {n["id"]: n for n in prof.get("nodes", [])}
        parent = {}
        for n in nodes.values():
            for c in n.get("children", []):
                parent[c] = n["id"]
        for nid, n in nodes.items():
            pid = parent.get(nid)
            if pid is None:
                continue
            a, b = nodes[pid]["callFrame"], n["callFrame"]
            if a["functionName"] in skip or b["functionName"] in skip:
                continue
            burl = b.get("url") or ""
            yield {
                "caller_file": a.get("url") or "<external>",
                "caller_line": a.get("lineNumber", -1) + 1,
                "caller": a.get("functionName") or "(anonymous)",
                "site_line": 0,
                "callee_file": burl if burl else "<builtin>",
                "callee_line": b.get("lineNumber", -1) + 1,
                "callee": b.get("functionName") or "(anonymous)",
            }


def callgrind_rows(paths):
    files = []
    for p in paths:
        files += glob.glob(os.path.join(p, "callgrind.out*")) if os.path.isdir(p) else [p]
    for f in files:
        fl = fn = cfl = cfn = None
        fn_line = {}
        pending = None
        with open(f, errors="replace") as fh:
            for line in fh:
                line = line.rstrip("\n")
                if pending is not None:
                    # the cost line after calls=: "<caller line> <cost>"
                    parts = line.split()
                    if parts and parts[0].lstrip("+-").isdigit():
                        site = int(parts[0])
                        caller_file = fl
                        yield {
                            "caller_file": caller_file or "<external>",
                            "caller_line": fn_line.get((fl, fn), 0),
                            "caller": fn or "",
                            "site_line": site,
                            "callee_file": pending[0] or "<external>",
                            "callee_line": pending[1],
                            "callee": pending[2] or "",
                        }
                    pending = None
                    continue
                if line.startswith("fl="):
                    fl = line[3:]
                elif line.startswith(("fi=", "fe=")):
                    pass  # inlined file switch: keep the function's own file
                elif line.startswith("fn="):
                    fn = line[3:]
                    cfl = None
                elif line.startswith(("cfl=", "cfi=")):
                    cfl = line[4:]
                elif line.startswith("cfn="):
                    cfn = line[4:]
                elif line.startswith("calls="):
                    parts = line[6:].split()
                    target = int(parts[1]) if len(parts) > 1 and parts[1].isdigit() else 0
                    pending = (cfl or fl, target, cfn)
                    cfl = None
                elif line and line[0].isdigit() and fn is not None and (fl, fn) not in fn_line:
                    fn_line[(fl, fn)] = int(line.split()[0])


def jsonl_rows(paths):
    files = []
    for p in paths:
        files += glob.glob(os.path.join(p, "*.jsonl")) if os.path.isdir(p) else [p]
    for f in files:
        with open(f, errors="replace") as fh:
            for line in fh:
                try:
                    yield json.loads(line)
                except Exception:
                    continue


def short_name(n):
    # Go "pkg/path.(*T).Method" / Rust "crate::m::T::f" / Python "C.f" -> last segment
    n = re.sub(r"\[.*?\]", "", n or "")
    n = n.split("<")[0]
    return re.split(r"[./:]", n.rstrip(")"))[-1] if n else n


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("mode", choices=["cpuprof", "callgrind", "jsonl"])
    ap.add_argument("paths", nargs="+")
    ap.add_argument("--map", action="append", default=[])
    a = ap.parse_args()
    rules = []
    for m in a.map:
        frm, _, to = m.partition("=")
        rules.append((frm, to))
    rules.sort(key=lambda r: -len(r[0]))
    rows = {"cpuprof": cpuprof_rows, "callgrind": callgrind_rows, "jsonl": jsonl_rows}[a.mode](a.paths)

    def named(rs):
        for r in rs:
            r["callee"] = short_name(r.get("callee", ""))
            r["caller"] = short_name(r.get("caller", ""))
            yield r

    n = emit(named(rows), rules, sys.stdout)
    print(f"{n} edges", file=sys.stderr)


if __name__ == "__main__":
    main()
