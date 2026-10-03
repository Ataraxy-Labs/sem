"""Exact Python call tracer for `sem system` (dynamic layer).

Put this directory on PYTHONPATH and set SYSTRACE_OUT to a directory; every
Python process then records each distinct (caller, call-site line, callee)
it executes — `call` events for Python callees, `c_call` for native ones —
and writes them as JSON lines to $SYSTRACE_OUT/py-<pid>.jsonl.
"""
import atexit
import json
import os
import sys
import threading

_OUT = os.environ.get("SYSTRACE_OUT")

if _OUT:
    _seen = set()
    _buf = []
    _lock = threading.Lock()
    _pid = os.getpid()

    def _name(co):
        return getattr(co, "co_qualname", co.co_name)

    def _prof(frame, event, arg):
        if event == "call":
            back = frame.f_back
            if back is None:
                return
            co = frame.f_code
            # module and class bodies run at import / class creation: not calls
            if not (co.co_flags & 0x1):  # CO_OPTIMIZED
                return
            bc = back.f_code
            key = (bc.co_filename, bc.co_firstlineno, _name(bc), back.f_lineno,
                   co.co_filename, co.co_firstlineno, _name(co))
        elif event == "c_call":
            co = frame.f_code
            callee = getattr(arg, "__qualname__", None) or getattr(arg, "__name__", "?")
            key = (co.co_filename, co.co_firstlineno, _name(co), frame.f_lineno,
                   "<builtin>", 0, str(callee))
        else:
            return
        if key not in _seen:
            _seen.add(key)
            _buf.append(key)
            if len(_buf) >= 20000:
                _flush()

    def _flush():
        global _pid
        with _lock:
            if not _buf:
                return
            rows = list(_buf)
            del _buf[:]
        path = os.path.join(_OUT, "py-%d.jsonl" % os.getpid())
        try:
            with open(path, "a") as f:
                for (cf, cl, cn, sl, tf, tl, tn) in rows:
                    f.write(json.dumps({
                        "caller_file": cf, "caller_line": cl, "caller": cn, "site_line": sl,
                        "callee_file": tf, "callee_line": tl, "callee": tn,
                    }) + "\n")
        except OSError:
            pass

    def _after_fork():
        # a forked child starts with the parent's buffer; keep it, dedupe later
        pass

    try:
        os.makedirs(_OUT, exist_ok=True)
    except OSError:
        pass
    atexit.register(_flush)
    sys.setprofile(_prof)
    threading.setprofile(_prof)
