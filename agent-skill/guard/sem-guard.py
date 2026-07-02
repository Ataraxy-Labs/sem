#!/usr/bin/env python3
"""PreToolUse hard gate: the agent always uses sem for code, never grep/read/sed.

Denied (with a redirect reason the model acts on):
  - Grep on code files            -> sem_entities text=/query=, sem_impact, sem_context
  - Read of a code file           -> sem_context / sem_entities; retry lane stays open
                                     for the mechanical Read-before-Edit requirement:
                                     the SECOND Read of the same path is allowed.
  - Bash grep/rg/ag/ack on code   -> sem_entities (piped filters like `cargo test | grep` pass)
  - Bash sed/awk touching code    -> Edit tool (after sem_context)
  - Bash cat/head/tail on code    -> sem_context / sem_entities

Always allowed: non-code files (md/toml/json/yaml/...), files outside a git repo
(sem needs git), pipe filtering, and anything when SEM_GUARD=0 is set.
"""
import sys, json, re, os, time

CODE_EXTS = {
    "rs", "py", "ts", "tsx", "js", "jsx", "mjs", "cjs", "go", "java", "kt",
    "kts", "c", "h", "cpp", "hpp", "cc", "hh", "cxx", "rb", "php", "swift",
    "scala", "cs", "lua", "zig", "ex", "exs", "hs", "ml", "mli", "vue",
    "svelte", "dart", "m", "mm",
}
STATE = os.path.expanduser("~/.claude/hooks/.sem-guard-state.json")
RETRY_WINDOW = 900  # seconds a Read-deny stays retryable


def ext_of(path):
    base = os.path.basename(path or "")
    return base.rsplit(".", 1)[1].lower() if "." in base else ""


def is_code(path):
    return ext_of(path) in CODE_EXTS


def in_git_repo(path, cwd="."):
    p = os.path.expanduser(path or ".")
    if not os.path.isabs(p):
        p = os.path.join(os.path.expanduser(cwd), p)
    d = os.path.abspath(p)
    if not os.path.isdir(d):
        d = os.path.dirname(d)
    while d and d != "/":
        if os.path.exists(os.path.join(d, ".git")):
            return True
        d = os.path.dirname(d)
    return False


def deny(reason):
    print(json.dumps({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    }))
    sys.exit(0)


def load_state():
    try:
        with open(STATE) as f:
            return json.load(f)
    except Exception:
        return {}


def save_state(s):
    try:
        now = time.time()
        s = {k: v for k, v in s.items() if now - v < RETRY_WINDOW}
        with open(STATE, "w") as f:
            json.dump(s, f)
    except Exception:
        pass


def grep_redirect(pattern):
    if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]{2,}", pattern or ""):
        return (
            f'"{pattern}" is a code symbol; grep on code is disabled. Use:\n'
            f"  - mcp__sem__sem_context entity_name=\"{pattern}\" (body + callers/callees, one call)\n"
            f"  - mcp__sem__sem_impact (blast radius)\n"
            f"  - mcp__sem__sem_entities query=\"...\" (find by intent)"
        )
    return (
        "grep on code files is disabled. Use mcp__sem__sem_entities with "
        f"text=\"{pattern}\" (exact substring, entity-addressed hits) or query=\"...\" "
        "(intent search). For regex, pass a distinctive literal chunk as text=. "
        "If you are genuinely searching non-code files, re-run scoped to them "
        "(e.g. glob *.md)."
    )


def handle_grep(inp, cwd):
    glob = inp.get("glob") or ""
    path = inp.get("path") or ""
    if glob and not is_code(glob):
        return  # explicitly scoped to non-code
    if path and os.path.isfile(os.path.expanduser(path)) and not is_code(path):
        return
    ftype = inp.get("type") or ""
    if ftype and ftype not in CODE_EXTS:
        return
    if not in_git_repo(path or ".", cwd):
        return
    deny(grep_redirect(inp.get("pattern") or ""))


def handle_read(inp):
    fp = inp.get("file_path") or ""
    if not is_code(fp) or not in_git_repo(fp):
        return
    state = load_state()
    key = os.path.abspath(os.path.expanduser(fp))
    if key in state and time.time() - state[key] < RETRY_WINDOW:
        # Active editing window: the retry proved edit intent, so keep this
        # path readable (refreshed on each read) instead of re-denying every
        # Read while the file is being worked on.
        state[key] = time.time()
        save_state(state)
        return
    state[key] = time.time()
    save_state(state)
    deny(
        f"Reading {os.path.basename(fp)} directly is disabled. To understand code use "
        f"mcp__sem__sem_context (entity_name, one call, body + callers) or "
        f"mcp__sem__sem_entities (path=\"{fp}\") to list what's inside. "
        f"ONLY if you are about to Edit this exact file (Edit requires a prior Read): "
        f"call Read again with the same path and it will be allowed."
    )


SEARCHERS = {"grep", "egrep", "fgrep", "rg", "ag", "ack"}
READERS = {"cat", "head", "tail", "less", "more"}


SEPARATORS = {"|", "||", "&&", ";", ";;", "&"}


def split_segments(tokens):
    """Returns (separator_before, tokens) pairs; first segment has sep None."""
    segs, cur, sep = [], [], None
    for t in tokens:
        if t in SEPARATORS:
            if cur:
                segs.append((sep, cur))
            cur, sep = [], t
        else:
            cur.append(t)
    if cur:
        segs.append((sep, cur))
    return segs


def seg_cmd(seg):
    for t in seg:
        if "=" in t and re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", t):
            continue  # env assignment prefix
        if t in {"sudo", "command", "nice", "time"}:
            continue
        return t
    return ""


def code_file_args(seg, cwd):
    return [t for t in seg[1:] if not t.startswith("-") and is_code(t)
            and in_git_repo(t, cwd)]


def handle_bash(inp, cwd):
    cmd = inp.get("command") or ""
    if "SEM_GUARD=0" in cmd:
        return
    try:
        import shlex
        lex = shlex.shlex(cmd, posix=True, punctuation_chars=True)
        lex.whitespace_split = True
        tokens = list(lex)
    except Exception:
        return
    segs = split_segments(tokens)
    for sep, seg in segs:
        c = os.path.basename(seg_cmd(seg))
        if c in SEARCHERS:
            if sep == "|":
                continue  # filtering piped output is fine
            args = [t for t in seg[1:] if not t.startswith("-")]
            file_args = args[1:] if args else []
            if file_args and all(not is_code(a) and ext_of(a) for a in file_args):
                continue  # explicitly non-code targets
            target = file_args[0] if file_args else "."
            if not in_git_repo(target, cwd):
                continue
            deny(grep_redirect(args[0] if args else ""))
        elif c in {"sed", "awk"} and code_file_args(seg, cwd):
            deny(
                "sed/awk on code files is disabled. To modify code use the Edit tool "
                "(after mcp__sem__sem_context to understand it); to extract code use "
                "mcp__sem__sem_context or mcp__sem__sem_entities."
            )
        elif c in READERS and code_file_args(seg, cwd):
            deny(
                "Dumping code files via cat/head/tail is disabled. Use "
                "mcp__sem__sem_context (entity body + callers in one call) or "
                "mcp__sem__sem_entities (path=...) instead."
            )


def main():
    data = json.load(sys.stdin)
    if os.environ.get("SEM_GUARD") == "0":
        return
    # Session-wide kill switch: hooks inherit the parent process env, so a
    # mid-session `export SEM_GUARD=0` can't reach them. Touching this file
    # disables the guard immediately (e.g. for controlled benchmarks).
    if os.path.exists(os.path.expanduser("~/.claude/hooks/.sem-guard-off")):
        return
    tool = data.get("tool_name", "")
    inp = data.get("tool_input") or {}
    cwd = data.get("cwd") or "."
    if tool == "Grep":
        handle_grep(inp, cwd)
    elif tool == "Read":
        handle_read(inp)
    elif tool == "Bash":
        handle_bash(inp, cwd)


if __name__ == "__main__":
    try:
        main()
    except Exception:
        pass  # never block on internal errors
    sys.exit(0)
