"""Splice a witness task's source / sink / probe hooks into copies of repo files.

Pure text edits at the byte spans `sem dataflow --witness` emitted; nothing is
executed. Returns {repo-relative path: instrumented text}.

  source param  ->  `<p> = _witness_inj(<p>)` first in the body that binds it
  source expr   ->  `_witness_src(<expr>)`
  sink call     ->  every argument `a` becomes `_witness_sink(a, i)`
  path fns (TS) ->  `_witness_probe("<key>", [<params>])` first in the body
                    (Python path functions are traced by the runtime's profiler)
"""
import ast
import json
from pathlib import Path


class InstrumentError(Exception):
    pass


def _edits_for(task):
    """(file, offset, rank, text) insertions; ranks order same-offset inserts
    (outer wrappers open first and close last)."""
    lang = task["lang"]
    ed = []
    sk = task["sink"]
    for i, a in enumerate(sk["contract"]["args"]):
        s, e = a["span"]
        ed.append((sk["file"], s, 0, "_witness_sink("))
        ed.append((sk["file"], e, 9, f", {i})"))
    src = task["source"]
    c = src["contract"]
    if c["kind"] == "expr":
        s, e = c["span"]
        ed.append((src["file"], s, 1, "_witness_src("))
        ed.append((src["file"], e, 8, ")"))
    else:
        p = c["param"]
        ed += _body_insert(lang, src["file"], c["body"], f"{p} = _witness_inj({p})", rank=2)
    if lang == "ts":
        for pf in task.get("pathFunctions", []):
            if not pf.get("body") or pf["role"] == "source" and c["kind"] == "param" and pf["file"] == src["file"] and pf["line"] == c.get("fnLine"):
                # the source function: injection proves it; a probe before
                # injection would see the uninjected value anyway
                continue
            args = ", ".join(pf["params"])
            ed += _body_insert(lang, pf["file"], pf["body"], f"_witness_probe({json.dumps(pf['key'])}, [{args}])", rank=3)
    return ed


def _body_insert(lang, file, body, stmt, rank):
    k = body["kind"]
    if k == "py-block":
        if body["sameLine"]:
            return [(file, body["at"], rank, stmt + "; ")]
        return [(file, body["lineStart"], rank, " " * body["col"] + stmt + "\n")]
    if k == "ts-block":
        return [(file, body["at"], rank, stmt + ";")]
    if k == "ts-expr":
        s, e = body["span"]
        return [(file, s, rank, "(" + stmt + ", "), (file, e, 10 - rank, ")")]
    raise InstrumentError(f"unknown body kind {k}")


def instrument(task, root: Path) -> dict:
    by_file = {}
    for f, off, rank, text in _edits_for(task):
        by_file.setdefault(f, []).append((off, rank, text))
    out = {}
    for f, eds in by_file.items():
        raw = (root / f).read_bytes()
        eds.sort(key=lambda x: (x[0], x[1]))
        parts, last = [], 0
        for off, _, text in eds:
            if off < last or off > len(raw):
                raise InstrumentError(f"{f}: overlapping or out-of-range edit at {off}")
            parts.append(raw[last:off])
            parts.append(text.encode())
            last = off
        parts.append(raw[last:])
        new = b"".join(parts).decode("utf-8", "replace")
        if task["lang"] == "python":
            try:
                ast.parse(new)  # parse only; nothing runs
            except SyntaxError as e:
                raise InstrumentError(f"{f}: instrumented file does not parse: {e}")
        out[f] = new
    return out
