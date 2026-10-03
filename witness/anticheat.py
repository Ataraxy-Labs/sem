"""Static anti-cheat checks on a proposed harness, before it runs.

The harness never receives the canary (the runtime generates it and injects
it at the declared source itself), so a cheat must either learn the canary or
route data through harness code. These rules close the routes a static check
can see; the runtime closes the rest (stack check at the sink, profiler leak
check for Python, one entry call, no recording stubs).

A harness may only: import repo modules, construct objects, provide
external contracts (stub modules / fakes), choose an injection mode, and call
ONE entry point. It may not name a hop or sink function of the claimed path
(only the source function may be named), introspect frames or memory, start
threads/processes, or touch the runtime's hooks.
"""
import ast
import re

PY_BANNED_MODULES = {
    "gc", "inspect", "ctypes", "sys", "builtins", "importlib", "threading", "multiprocessing", "subprocess",
    "signal", "unittest", "mock", "pytest", "traceback", "faulthandler", "tracemalloc", "code", "codeop", "pdb",
    "atexit", "concurrent", "_thread", "pty", "socket", "runpy", "pickle", "marshal", "dis", "types", "weakref",
    "witness_rt", "__main__", "resource", "mmap", "selectors", "asyncio.subprocess",
}
PY_BANNED_CALLS = {"eval", "exec", "compile", "__import__", "getattr", "setattr", "delattr", "globals", "locals",
                   "vars", "breakpoint", "input", "memoryview", "id"}
PY_BANNED_OS = {"system", "popen", "fork", "forkpty", "kill", "killpg", "execv", "execve", "execl", "execlp", "execvp",
                "execvpe", "spawnl", "spawnv", "spawnve", "posix_spawn", "posix_spawnp", "read", "pipe", "dup", "dup2",
                "open", "fdopen", "_exit", "abort"}
ALLOWED_DUNDER = {"__init__", "__name__", "__enter__", "__exit__", "__aenter__", "__aexit__", "__call__"}
INTROSPECTION_ATTRS = re.compile(r"^(f|gi|cr|ag|tb)_|^co_|^func_")


def forbidden_names(task):
    """Names of the path's hop and sink functions, unless shared with the
    source function (which the harness may call)."""
    pfs = task.get("pathFunctions", [])
    src = {p["name"] for p in pfs if p["role"] == "source"}
    return sorted({p["name"] for p in pfs if p["role"] in ("hop", "sink")} - src - {"__init__", "main", "<module>"})


def repo_tops(task):
    """Top-level importable names of the repo, from the task's file paths."""
    tops = set()
    files = [task["source"]["file"], task["sink"]["file"]] + [p["file"] for p in task.get("pathFunctions", [])]
    for f in files:
        parts = f.split("/")
        tops.add(parts[0].removesuffix(".py"))
        if parts[0] in ("src", "lib") and len(parts) > 1:
            tops.add(parts[1].removesuffix(".py"))
    return tops


def check_python(text, task):
    v = []
    try:
        tree = ast.parse(text)
    except SyntaxError as e:
        return [f"syntax error: {e}"]
    bad_names = set(forbidden_names(task))
    tops = repo_tops(task)
    repo_bound = set()
    for n in ast.walk(tree):
        if isinstance(n, ast.Import):
            for a in n.names:
                if a.name.split(".")[0] in tops:
                    repo_bound.add((a.asname or a.name).split(".")[0])
        elif isinstance(n, ast.ImportFrom) and (n.level or (n.module or "").split(".")[0] in tops):
            for a in n.names:
                repo_bound.add(a.asname or a.name)
    for n in ast.walk(tree):
        targets = n.targets if isinstance(n, ast.Assign) else [n.target] if isinstance(n, (ast.AugAssign, ast.AnnAssign)) else n.targets if isinstance(n, ast.Delete) else []
        for t in targets:
            root = t
            while isinstance(root, (ast.Attribute, ast.Subscript)):
                root = root.value
            if t is not root and isinstance(root, ast.Name) and root.id in repo_bound:
                v.append(f"line {n.lineno}: assigns into repo module/object `{root.id}` (repo code may not be patched; only external contracts)")
    for n in ast.walk(tree):
        if isinstance(n, ast.Import):
            for a in n.names:
                if a.name.split(".")[0] in PY_BANNED_MODULES or a.name in PY_BANNED_MODULES:
                    v.append(f"line {n.lineno}: import of `{a.name}` is not allowed")
        elif isinstance(n, ast.ImportFrom):
            m = n.module or ""
            if m.split(".")[0] in PY_BANNED_MODULES or m in PY_BANNED_MODULES:
                v.append(f"line {n.lineno}: import from `{m}` is not allowed")
            for a in n.names:
                if a.name in bad_names:
                    v.append(f"line {n.lineno}: imports `{a.name}`, a hop/sink function of the claimed path")
                if a.name == "*":
                    v.append(f"line {n.lineno}: star import is not allowed")
        elif isinstance(n, ast.Name):
            if "witness" in n.id.lower() or n.id.startswith("_W") or n.id in ("__builtins__", "__loader__", "__spec__"):
                v.append(f"line {n.lineno}: name `{n.id}` is reserved")
            if n.id in bad_names:
                v.append(f"line {n.lineno}: names `{n.id}`, a hop/sink function of the claimed path")
        elif isinstance(n, ast.Attribute):
            a = n.attr
            if a.startswith("__") and a not in ALLOWED_DUNDER:
                v.append(f"line {n.lineno}: dunder attribute `{a}` is not allowed")
            if INTROSPECTION_ATTRS.search(a) or "witness" in a.lower() or a.startswith("_W"):
                v.append(f"line {n.lineno}: attribute `{a}` is not allowed")
            if a in bad_names:
                v.append(f"line {n.lineno}: names `{a}`, a hop/sink function of the claimed path")
            if isinstance(n.value, ast.Name) and n.value.id == "os" and a in PY_BANNED_OS:
                v.append(f"line {n.lineno}: os.{a} is not allowed")
            if isinstance(n.value, ast.Name) and n.value.id == "W" and a.startswith("_"):
                v.append(f"line {n.lineno}: W.{a} is private")
        elif isinstance(n, ast.Call) and isinstance(n.func, ast.Name) and n.func.id in PY_BANNED_CALLS:
            v.append(f"line {n.lineno}: call of `{n.func.id}` is not allowed")
        elif isinstance(n, ast.Constant) and isinstance(n.value, str):
            s = n.value
            if "/proc" in s or "/out" == s[:4] or "/opt/witness" in s or "witness_rt" in s or "/dev/mem" in s:
                v.append(f"line {n.lineno}: string `{s[:40]}` points at the runtime or process memory")
        elif isinstance(n, (ast.Global, ast.Nonlocal)):
            pass
    calls = [n for n in ast.walk(tree) if isinstance(n, ast.Call) and isinstance(n.func, ast.Attribute)
             and isinstance(n.func.value, ast.Name) and n.func.value.id == "W" and n.func.attr == "entry"]
    if len(calls) != 1:
        v.append(f"the harness must call W.entry exactly once (found {len(calls)})")
    # W.entry inside a loop / comprehension could still run once (the
    # runtime refuses a second call); nothing else to check here
    return sorted(set(v))


TS_BANNED = [
    (r"_witness|__witness", "reserved runtime names"),
    (r"\bglobalThis\b|\bglobal\s*\.|\bglobal\s*\[", "global object access"),
    (r"\bprocess\s*\.\s*(?!env\b)\w+|\bprocess\s*\[", "process access other than process.env"),
    (r"\brequire\s*\(|\bimport\s*\(", "dynamic require/import"),
    (r"\beval\b|\bFunction\s*\(|new\s+Function\b", "eval / Function"),
    (r"\.stack\b|captureStackTrace|prepareStackTrace", "stack introspection"),
    (r"['\"](node:)?(inspector|v8|vm|worker_threads|child_process|cluster|async_hooks|module|repl|perf_hooks|trace_events)['\"]", "banned built-in module"),
    (r"\bset(Timeout|Interval|Immediate)\b|queueMicrotask|process\.nextTick", "deferred execution"),
    (r"\bProxy\b|\bReflect\b|defineProperty|__proto__|\.prototype\b|setPrototypeOf|getOwnPropertyDescriptor", "meta-object access"),
    (r"/proc|/opt/witness|/out/", "runtime or process-memory paths"),
    (r"\bclass\s+\w*\s*(extends|\{)", "class definitions"),
    (r"=>", "arrow functions (use W.returns / W.resolves / W.fake)"),
    (r"\bget\s+\w+\s*\(|\bset\s+\w+\s*\(", "accessors"),
]


def strip_ts_comments(text):
    """Blank out // and /* */ comments (not inside string literals), keeping
    line numbers."""
    out, i, n = [], 0, len(text)
    q = None
    while i < n:
        c = text[i]
        if q:
            out.append(c)
            if c == "\\" and i + 1 < n:
                out.append(text[i + 1])
                i += 2
                continue
            if c == q:
                q = None
            i += 1
        elif c in "'\"`":
            q = c
            out.append(c)
            i += 1
        elif text.startswith("//", i):
            j = text.find("\n", i)
            j = n if j < 0 else j
            i = j
        elif text.startswith("/*", i):
            j = text.find("*/", i + 2)
            j = n if j < 0 else j + 2
            out.append("".join(ch if ch == "\n" else " " for ch in text[i:j]))
            i = j
        else:
            out.append(c)
            i += 1
    return "".join(out)


# --rules v2 (exploratory): allowed forms, blanked out before the banned patterns run
TS_V2_ALLOWED = [
    r"^[ \t]*globalThis\.[A-Za-z][\w$]*\s*=\s*(?=W\.\w+\()",   # globalThis.fetch = W.fake(..)
    r"\bprocess\.chdir\s*\(\s*(['\"])[^'\"]*\1\s*\)",          # process.chdir('<literal>')
    r"\bprocess\.argv\b",                                       # CLI arguments contract
]


def check_ts(text, task, rules="v1"):
    text = strip_ts_comments(text)
    v = []
    if rules == "v2":
        for m in re.finditer(r"\bprocess\.chdir\s*\(\s*(['\"])([^'\"]*)\1", text):
            if "/proc" in m.group(2) or "/opt/witness" in m.group(2):
                v.append("process.chdir into the runtime or /proc")
        for pat in TS_V2_ALLOWED:
            text = re.sub(pat, lambda m: "" if m.group(0).lstrip().startswith("globalThis") else "PROCESS_OK", text, flags=re.M)
    for pat, why in TS_BANNED:
        for m in re.finditer(pat, text):
            line = text.count("\n", 0, m.start()) + 1
            v.append(f"line {line}: {why} (`{m.group(0)[:30]}`)")
    fns = re.findall(r"\bfunction\b", text)
    if len(fns) != 1 or not re.search(r"export\s+default\s+async\s+function\s+main\s*\(\s*W\b", text):
        v.append("the harness must define exactly one function: `export default async function main(W)`")
    # method shorthand in object literals defines a function: `foo(x) {`
    for m in re.finditer(r"(?<![\w.])(\w+)\s*\([^()]*\)\s*\{", text):
        if m.group(1) not in ("if", "for", "while", "switch", "catch", "main", "with"):
            line = text.count("\n", 0, m.start()) + 1
            v.append(f"line {line}: method definition `{m.group(1)}(..) {{` (harness code may not receive values)")
    for name in forbidden_names(task):
        for m in re.finditer(r"(?<![\w$])" + re.escape(name) + r"(?![\w$])", text):
            line = text.count("\n", 0, m.start()) + 1
            v.append(f"line {line}: names `{name}`, a hop/sink function of the claimed path")
    n_entry = len(re.findall(r"\bW\s*\.\s*entry\s*\(", text))
    if rules == "v2":
        n_entry += len(re.findall(r"\bW\s*\.\s*entryImport\s*\(", text))
        for m in re.finditer(r"\bW\s*\.\s*entryImport\s*\(([^)]*)\)", text):
            if not re.fullmatch(r"\s*(['\"])/work/[^'\"]+\1\s*", m.group(1)):
                v.append("W.entryImport takes one string literal path under /work")
    if n_entry != 1:
        v.append("the harness must call W.entry exactly once" + (" (or W.entryImport once)" if rules == "v2" else ""))
    return sorted(set(v))


def check(text, task, rules="v1"):
    return check_python(text, task) if task["lang"] == "python" else check_ts(text, task, rules)
