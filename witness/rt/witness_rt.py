"""In-sandbox Python runtime of a sem witness run.

    python3 /opt/witness/witness_rt.py /harness/harness.py

The runner has already spliced into the repo copy (/work):
  - at the declared source: `_witness_inj(<param>)` (parameter sources) or
    `_witness_src(<expr>)` (expression sources);
  - at the declared sink call: every argument wrapped as `_witness_sink(<arg>, i)`.

This runtime generates a fresh canary that the harness never sees, defines
those hooks, gives the harness a small API (`W`), runs the harness, and
writes what it observed to /out/result.json. The verdict is the runner's.

Observation rules (all inside the one `W.entry(...)` call):
  - injected: the source hook fired and put the canary into the value;
  - sink hit: a sink argument contained the canary;
  - the stack at the sink hit holds the entry frame and no frame of the
    harness, of this runtime's stubs, or of unittest.mock between them;
  - leak: the canary was passed into a function defined by the harness
    (data travelling through harness code is not a repo path);
  - path functions: each one, at call or return, held the canary in its
    arguments, locals or return value.
"""
import asyncio
import builtins
import importlib.abc
import importlib.machinery
import importlib.util
import json
import os
import secrets
import sys
import sysconfig
import threading
import traceback
import types

# the runtime's own references: a harness may replace external contracts
# such as os.path.exists, which must not change how the runtime works
_exists, _join, _abspath, _normpath, _fspath = os.path.exists, os.path.join, os.path.abspath, os.path.normpath, os.fspath
_open = open
TASK = json.load(_open("/harness/task.json"))
WORK = "/work"
HARNESS_DIR = "/harness"
RT_FILE = os.path.abspath(__file__)
STDLIB = sysconfig.get_paths()["stdlib"]
OUT = "/out/result.json"

# lowercase alnum (survives quoting, joins, f-strings); generated only when the
# entry call starts, so no harness code that runs before it can ever see it
_CANARY = None

_state = {
    "in_entry": False,
    "entry_done": False,
    "entry_calls": 0,
    "entry_frame": None,
    "injections": 0,
    "inject_attempts": 0,
    "sink_hits": [],
    "sink_seen": 0,
    "leaks": [],
    "stubbed_modules": [],
    "path_hits": {},
    "path_seen": {},
    "error": None,
    "entry_error": None,
}
_mode = {"inject": "append"}


# ------------------------------------------------------------------ canary search
def _has(v, depth=0, seen=None):
    if _CANARY is None:
        return False
    if seen is None:
        seen = set()
    if depth > 4 or id(v) in seen:
        return False
    if isinstance(v, str):
        return _CANARY in v
    if isinstance(v, (bytes, bytearray)):
        return _CANARY.encode() in v
    if isinstance(v, (int, float, bool, type(None))):
        return False
    seen.add(id(v))
    try:
        if isinstance(v, os.PathLike):
            return _CANARY in _fspath(v)
        if isinstance(v, dict):
            return any(_has(k, depth + 1, seen) or _has(x, depth + 1, seen) for k, x in list(v.items())[:200])
        if isinstance(v, (list, tuple, set, frozenset)):
            return any(_has(x, depth + 1, seen) for x in list(v)[:200])
        if isinstance(v, _Stub):
            return False
        d = getattr(v, "__dict__", None)
        if isinstance(d, dict) and _has(d, depth + 1, seen):
            return True
        if type(v).__module__ in ("urllib.parse", "pathlib", "yarl", "httpx", "requests.models"):
            return _CANARY in str(v)
    except Exception:
        return False
    return False


# ------------------------------------------------------------------ hooks spliced into repo code
def _taint(v, depth=0):
    """The value at the declared source, with the canary put into it."""
    m = _mode["inject"]
    if isinstance(v, str):
        return {"replace": _CANARY, "prepend": _CANARY + v}.get(m, v + _CANARY)
    if isinstance(v, bytes):
        return _taint(v.decode("utf-8", "replace")).encode()
    if v is None or isinstance(v, _Stub):
        return _CANARY
    if depth > 3:
        return v
    if isinstance(v, dict):
        return {k: _taint(x, depth + 1) for k, x in v.items()}
    if isinstance(v, list):
        return [_taint(x, depth + 1) for x in v]
    if isinstance(v, tuple):
        return tuple(_taint(x, depth + 1) for x in v)
    d = getattr(v, "__dict__", None)
    if isinstance(d, dict):
        for k, x in list(d.items()):
            if isinstance(x, (str, dict, list, tuple)) or x is None:
                try:
                    object.__setattr__(v, k, _taint(x, depth + 1))
                except Exception:
                    pass
        return v
    return v


def _witness_inj(v):
    if not _state["in_entry"]:
        return v
    _state["inject_attempts"] += 1
    t = _taint(v)
    if _has(t):
        _state["injections"] += 1
    return t


def _witness_src(v):
    if not _state["in_entry"]:
        return v
    if asyncio.iscoroutine(v):
        async def _await():
            return _witness_src(await v)
        return _await()
    return _witness_inj(v)


def _frame_kind(fn):
    fn = _abspath(fn)
    if fn == RT_FILE:
        return "runtime"
    if fn.startswith(HARNESS_DIR + "/"):
        return "harness"
    if fn.startswith(WORK + "/"):
        return "repo"
    if "unittest/mock" in fn or fn.endswith("/mock.py"):
        return "mock"
    if "site-packages" in fn or "dist-packages" in fn:
        return "dep"
    if fn.startswith(STDLIB) or fn.startswith("<frozen"):
        return "stdlib"
    return "other"


def _stack_check():
    """Frames between the sink hook and the entry frame: no harness, mock,
    runtime-stub or unknown code may sit between them."""
    f = sys._getframe(2)  # the repo frame holding the sink call
    frames = []
    found_entry = False
    while f is not None:
        if f is _state["entry_frame"]:
            found_entry = True
            break
        frames.append((_frame_kind(f.f_code.co_filename), f.f_code.co_filename, f.f_lineno, f.f_code.co_name))
        f = f.f_back
    # the runtime's own async trampolines (`W.entry`'s `_run`, `_witness_src`'s
    # `_await`) are allowed; any other runtime frame (a stub) is not
    bad = [x for x in frames if x[0] not in ("repo", "stdlib", "dep") and not (x[0] == "runtime" and x[3] in ("_run", "_await"))]
    # async entry: the coroutine runs under the event loop, not under the
    # entry frame; accept when the outermost repo coroutine was started by it
    if not found_entry and _state.get("async_entry"):
        found_entry = _state.get("entry_task") is not None and asyncio.current_task() is _state["entry_task"]
    return found_entry, bad, frames


def _witness_sink(v, i):
    if not _state["in_entry"]:
        return v
    _state["sink_seen"] += 1
    if _has(v):
        ok_entry, bad, frames = _stack_check()
        _state["sink_hits"].append({
            "arg": i,
            "entryOnStack": ok_entry,
            "badFrames": [f"{k} {fn}:{ln} {nm}" for k, fn, ln, nm in bad][:10],
            "stack": [f"{k} {fn}:{ln} {nm}" for k, fn, ln, nm in frames][:30],
        })
    return v


_FINISHING = [False]
_GUARDED = ("/proc", "/dev/mem", "/dev/kmem", "/opt/witness", "/out")


def _audit(event, args):
    """No code in the sandbox may read process memory or the runtime (the
    canary lives there), whatever path spelling or cwd it uses."""
    if event in ("gc.get_objects", "gc.get_referrers", "gc.get_referents"):
        raise PermissionError(f"{event} is not allowed in a witness run")
    if event in ("open", "os.listdir", "os.scandir", "os.chdir") and args and isinstance(args[0], (str, bytes, os.PathLike)):
        try:
            p = os.path.realpath(_fspath(args[0]) if not isinstance(args[0], bytes) else args[0].decode())
        except Exception:
            return
        if p.startswith(_GUARDED) and not (_FINISHING[0] and p.startswith("/out")):
            raise PermissionError(f"{p}: not readable in a witness run")


sys.addaudithook(_audit)

builtins._witness_inj = _witness_inj
builtins._witness_src = _witness_src
builtins._witness_sink = _witness_sink


# ------------------------------------------------------------------ path-function tracing
_PATH = {}  # (abs file, def line) -> key
for pf in TASK.get("pathFunctions", []):
    _PATH[(_join(WORK, pf["file"]), int(pf["line"]))] = pf["key"]


def _path_key(code):
    k = _PATH.get((code.co_filename, code.co_firstlineno))
    if k is None and code.co_filename.startswith(WORK + "/"):
        # decorated defs: co_firstlineno is the first decorator line
        for (fn, line), key in _PATH.items():
            if fn == code.co_filename and code.co_firstlineno <= line <= code.co_firstlineno + 6 and code.co_name == key.rsplit(".", 1)[-1].rsplit("::", 1)[-1]:
                _PATH[(code.co_filename, code.co_firstlineno)] = key
                return key
    return k


_harness_calls = {}  # id(frame) -> names of arguments that held the canary at call


def _harness_globals_have():
    g = _state.get("harness_globals")
    if not g:
        return False
    return any(_has(v) for k, v in list(g.items()) if not k.startswith("__") and k != "W" and not isinstance(v, types.ModuleType))


def _profile(frame, event, arg):
    if not _state["in_entry"] or event not in ("call", "return"):
        return
    code = frame.f_code
    fn = code.co_filename
    if fn.startswith(HARNESS_DIR + "/"):
        # harness code may RECEIVE the canary (a contract stub such as
        # `os.path.exists = lambda p: True`, a fake logger) but must never
        # EMIT it: not in a return value, not into an argument object it
        # was given clean, not into its module globals
        try:
            loc = dict(frame.f_locals)
            if event == "call":
                _harness_calls[id(frame)] = {k for k, v in loc.items() if _has(v)}
                return
            pre = _harness_calls.pop(id(frame), set())
            where = f"{fn}:{frame.f_lineno} {code.co_name}"
            if _has(arg):
                _state["leaks"].append(f"returned the canary: {where}")
            for k, v in loc.items():
                if k not in pre and _has(v):
                    _state["leaks"].append(f"put the canary into `{k}`: {where}")
                    break
            if _harness_globals_have():
                _state["leaks"].append(f"stored the canary in a module global: {where}")
        except Exception:
            pass
        return
    if event == "call" and fn.startswith(WORK + "/"):
        back = frame.f_back
        if back is not None and back.f_code.co_filename.startswith(HARNESS_DIR + "/"):
            try:
                if _has(dict(frame.f_locals)):
                    _state["leaks"].append(f"harness code passed the canary into repo code: {fn}:{frame.f_lineno} {code.co_name}")
            except Exception:
                pass
    key = _path_key(code)
    if key is None:
        return
    _state["path_seen"][key] = True
    if _state["path_hits"].get(key):
        return
    try:
        if _has(dict(frame.f_locals)) or (event == "return" and _has(arg)):
            _state["path_hits"][key] = True
    except Exception:
        pass


# ------------------------------------------------------------------ stubs for absent externals
class _Stub:
    """An absent external module / object: any attribute is a stub, a call
    returns a stub, except a call with one callable argument, which returns
    it (a decorator or handler registration keeps the function). It records
    no arguments, so nothing passed to it can come back out."""

    def __init__(self, name="stub"):
        object.__setattr__(self, "_n", name)

    def __getattr__(self, k):
        if k.startswith("__") and k.endswith("__"):
            raise AttributeError(k)
        return _Stub(f"{self._n}.{k}")

    def __call__(self, *a, **kw):
        if len(a) == 1 and not kw and callable(a[0]) and not isinstance(a[0], _Stub):
            return a[0]
        return _Stub(f"{self._n}()")

    def __mro_entries__(self, bases):
        return (_StubBase,)

    def __iter__(self):
        return iter(())

    def __bool__(self):
        return True

    def __repr__(self):
        return f"<stub {self._n}>"

    def __getitem__(self, k):
        return _Stub(f"{self._n}[]")

    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False

    async def __aenter__(self):
        return self

    async def __aexit__(self, *a):
        return False

    def __await__(self):
        if False:
            yield
        return self

    def __or__(self, o):
        return self

    __ror__ = __or__


class _StubBase:
    """Base class standing in for a stubbed external class."""

    def __init__(self, *a, **kw):
        for k, v in kw.items():
            try:
                setattr(self, k, v)
            except Exception:
                pass

    def __init_subclass__(cls, **kw):
        pass

    def __class_getitem__(cls, k):
        return cls

    def __getattr__(self, k):
        if k.startswith("__"):
            raise AttributeError(k)
        return _Stub(f"{type(self).__name__}.{k}")


class _StubModule(types.ModuleType):
    def __getattr__(self, k):
        if k.startswith("__") and k.endswith("__"):
            raise AttributeError(k)
        return _Stub(f"{self.__name__}.{k}")


_explicit = {}


class _StubFinder(importlib.abc.MetaPathFinder, importlib.abc.Loader):
    def find_spec(self, name, path, target=None):
        if name in _explicit:
            return importlib.util.spec_from_loader(name, self, is_package=True)
        for f in sys.meta_path:
            if f is self:
                continue
            try:
                spec = f.find_spec(name, path, target) if hasattr(f, "find_spec") else None
            except Exception:
                spec = None
            if spec is not None:
                return None
        top = name.split(".")[0]
        if _exists(_join(WORK, top)) or _exists(_join(WORK, top + ".py")):
            return None  # a repo module that failed: let the import error surface
        # never stub the standard library or private/platform modules
        # (`_winapi`, `msvcrt`): code probes them with try/except ImportError
        if top in sys.stdlib_module_names or top.startswith("_"):
            return None
        # only repo or harness code gets stubs: an installed library probing
        # an optional dependency (pandas -> pyarrow) must see ImportError
        f = sys._getframe(1)
        while f is not None and (f.f_code.co_filename.startswith("<frozen") or f.f_code.co_filename == RT_FILE):
            f = f.f_back
        if f is not None and _frame_kind(f.f_code.co_filename) not in ("repo", "harness"):
            return None
        _state["stubbed_modules"].append(name)
        return importlib.util.spec_from_loader(name, self, is_package=True)

    def create_module(self, spec):
        if spec.name in _explicit:
            m = _StubModule(spec.name)
            for k, v in _explicit[spec.name].items():
                setattr(m, k, v)
            m.__path__ = []
            return m
        m = _StubModule(spec.name)
        m.__path__ = []
        return m

    def exec_module(self, module):
        pass


class _ExplicitFinder(_StubFinder):
    """First on the path: a module the harness provided with W.stub_module
    wins over an installed one (an external contract, e.g. an incompatible
    installed version)."""

    def find_spec(self, name, path, target=None):
        if name in _explicit:
            return importlib.util.spec_from_loader(name, self, is_package=True)
        return None


sys.meta_path.insert(0, _ExplicitFinder())
sys.meta_path.append(_StubFinder())


# ------------------------------------------------------------------ repo code patched by the harness?
_BUILTIN_NAMES = set(dir(builtins))


def _harness_made(v):
    """A function / class / bound method whose code lives in the harness."""
    f = getattr(v, "__func__", v)
    code = getattr(f, "__code__", None)
    if code is not None:
        return code.co_filename.startswith(HARNESS_DIR + "/")
    if isinstance(v, type):
        mod = sys.modules.get(getattr(v, "__module__", ""), None)
        return getattr(v, "__module__", "") == "__witness_harness__" or (mod is not None and getattr(mod, "__file__", "") and mod.__file__.startswith(HARNESS_DIR + "/"))
    return False


def _repo_patches():
    """Bindings in repo modules (and their classes) that hold harness code, or
    module-level names shadowing a builtin that the module itself does not
    define in its source: the harness patched repo code. Checked at W.entry."""
    out = []
    # values the harness provided as an external module (W.stub_module) are
    # contracts the repo imported, not patches of repo code
    provided = {id(v) for attrs in _explicit.values() for v in attrs.values()}
    for name, m in list(sys.modules.items()):
        f = getattr(m, "__file__", None) or ""
        if not f.startswith(WORK + "/"):
            continue
        try:
            src = _open(f).read()
        except Exception:
            src = ""
        for k, v in list(vars(m).items()):
            if k.startswith("__"):
                continue
            if id(v) in provided:
                continue
            if _harness_made(v):
                out.append(f"{name}.{k}")
            elif k in _BUILTIN_NAMES and k not in src:
                out.append(f"{name}.{k} (shadows a builtin)")
            elif isinstance(v, type) and getattr(v, "__module__", None) == name:
                for ck, cv in list(vars(v).items()):
                    if _harness_made(cv):
                        out.append(f"{name}.{k}.{ck}")
    return out[:20]


def _is_repo_module(name):
    """Is `name` (or a package above it) a module of the repo, under the repo
    root or any repo directory that is (or may be put) on sys.path (src
    layouts: /work/src/pkg)?"""
    parts = name.split(".")
    roots = {WORK} | {p for p in sys.path if isinstance(p, str) and p.startswith(WORK)}
    roots |= {_join(WORK, d) for d in ("src", "lib", "python", "app", "packages")}
    for r in roots:
        for i in range(1, len(parts) + 1):
            base = _join(r, *parts[:i])
            if _exists(base + ".py") or _exists(_join(base, "__init__.py")) or (i == 1 and _exists(base) and _exists(_join(base, parts[1] + ".py") if len(parts) > 1 else base)):
                return True
    # any module file of that dotted path anywhere in the repo tree
    tail = _join(*parts)
    for dirpath, dirnames, filenames in os.walk(WORK):
        dirnames[:] = [d for d in dirnames if d not in ("node_modules", ".git", ".venv", "venv")]
        if dirpath.endswith("/" + tail) or (parts[-1] + ".py" in filenames and dirpath.endswith("/" + _join(*parts[:-1])) if len(parts) > 1 else False):
            return True
    return False


# pure computation is not an external boundary: a harness may fake the
# filesystem, network, env or clock (os.path.exists is a filesystem
# predicate), but not path / url / json semantics
import posixpath as _posixpath, json as _json_mod, urllib.parse as _uparse
_FS_PREDICATES = {"exists", "lexists", "isfile", "isdir", "islink", "ismount", "getsize", "getmtime", "getatime",
                  "getctime", "samefile", "sameopenfile", "samestat", "realpath", "expanduser", "expandvars"}
_PURE_SNAP = [(m, {k: getattr(m, k) for k in dir(m) if not k.startswith("_") and callable(getattr(m, k))
                   and not (m is _posixpath and k in _FS_PREDICATES)}) for m in (_posixpath, _json_mod, _uparse)]


def _pure_patches():
    return [f"{m.__name__}.{k} replaced (pure function, not a contract)" for m, snap in _PURE_SNAP for k, v in snap.items()
            if getattr(m, k, None) is not v]


# ------------------------------------------------------------------ the harness API
class _W:
    """What a harness may use. It never exposes the canary."""

    def __init__(self):
        object.__setattr__(self, "_frozen", True)

    def __setattr__(self, k, v):
        raise AttributeError("W is read-only")

    @staticmethod
    def inject_mode(mode):
        """How the canary goes into a string at the source: 'append'
        (default: value + canary), 'prepend', or 'replace'."""
        if mode not in ("append", "prepend", "replace"):
            raise ValueError(mode)
        _mode["inject"] = mode

    @staticmethod
    def stub_module(name, **attrs):
        """Provide an external module (absent from the sandbox) with these
        attributes; the rest is stubbed. Refuses repo modules."""
        if _is_repo_module(name):
            raise ValueError(f"{name} is a repo module: only external contracts may be stubbed")
        if name in sys.modules:
            raise ValueError(f"{name} is already imported")
        _explicit[name] = attrs

    @staticmethod
    def stub(name="stub"):
        """A permissive stand-in object for an external dependency."""
        return _Stub(name)

    @staticmethod
    def add_path(rel):
        """Put a repo directory on sys.path (e.g. 'src')."""
        p = _normpath(_join(WORK, rel))
        if not p.startswith(WORK):
            raise ValueError(rel)
        sys.path.insert(0, p)

    @staticmethod
    def entry(fn, *args, **kwargs):
        """Call the entry point once. Sources and sinks count only inside."""
        if _state["entry_calls"]:
            raise RuntimeError("W.entry may be called once")
        _state["entry_calls"] += 1
        globals()["_CANARY"] = "wq" + secrets.token_hex(8)
        _state["repo_patches"] = _repo_patches() + _pure_patches()
        _state["entry_frame"] = sys._getframe(0)
        _state["in_entry"] = True
        sys.setprofile(_profile)
        threading.setprofile(_profile)
        try:
            r = fn(*args, **kwargs)
            if asyncio.iscoroutine(r) or isinstance(r, asyncio.Future):
                _state["async_entry"] = True

                async def _run():
                    _state["entry_task"] = asyncio.current_task()
                    return await r

                r = asyncio.run(_run())
            return r
        except BaseException as e:  # the sink may have fired before
            _state["entry_error"] = "".join(traceback.format_exception_only(type(e), e))[-800:]
            return None
        finally:
            sys.setprofile(None)
            threading.setprofile(None)
            _state["in_entry"] = False
            _state["entry_done"] = True


W = _W()


def _redact(s):
    return s.replace(_CANARY, "<CANARY>") if isinstance(s, str) and _CANARY else s


def main():
    sys.path.insert(0, WORK)
    for extra in TASK.get("pythonPath", []):
        sys.path.insert(0, _join(WORK, extra))
    os.chdir(WORK)
    path = sys.argv[1]
    g = {"__name__": "__witness_harness__", "__file__": path, "W": W}
    _state["harness_globals"] = g
    try:
        code = compile(_open(path).read(), path, "exec")
        exec(code, g)
        main_fn = g.get("main")
        if callable(main_fn):
            main_fn()
    except BaseException as e:
        _state["error"] = "".join(traceback.format_exception(type(e), e, e.__traceback__))[-2000:]
    res = {k: v for k, v in _state.items() if k not in ("entry_frame", "entry_task", "harness_globals")}
    res.setdefault("repo_patches", [])
    res["entry_error"] = _redact(res["entry_error"])
    res["error"] = _redact(res["error"])
    res["sink_hits"] = [{k: ([_redact(x) for x in v] if isinstance(v, list) else v) for k, v in h.items()} for h in res["sink_hits"]]
    res["stubbed_modules"] = sorted(set(res["stubbed_modules"]))[:80]
    _FINISHING[0] = True
    os.makedirs("/out", exist_ok=True)
    with _open(OUT, "w") as f:
        json.dump(res, f, indent=1)


if __name__ == "__main__":
    main()
