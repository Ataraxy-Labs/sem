//! `sem hook tool`: the sem+weave workflow as a pre-tool hook, the same for
//! every agent harness that speaks the Claude Code hook format (Claude Code,
//! Codex) and for pi through its bash wrapper.
//!
//! Whatever tool the agent reaches for, code is read by sem and changed by
//! weave:
//!
//! ```text
//! cat / head / sed -n / nl FILE     ->  sem find --read FILE [--lines A-B]
//! grep / rg PATTERN [PATHS]          ->  sem grep PATTERN [PATHS]
//! sed -i / perl -pi 's/lit/lit/g'    ->  weave edit (old/new on each file)
//! apply_patch (Codex)                ->  weave edit, one old/new per hunk
//! builds and tests                   ->  sem check --run (Claude, pi), or the
//!                                        sem_check tool, which blocks (Codex)
//! any other shell write              ->  refused: use weave_edit
//! ```
//!
//! Each decision is logged to `<git dir>/sem-run/coverage/<session>.jsonl`
//! (or `$SEM_COVERAGE_DIR`),
//! so a session can be checked for how much of its code access went through
//! sem and weave. Silent on anything it does not recognise: the hook must
//! never break a tool call it does not understand.

use std::io::Read;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::{json, Value};

/// Commands that only filter output piped into them.
const FILTERS: &[&str] = &["head", "tail", "grep", "egrep", "rg", "sed", "awk", "cut", "sort", "uniq", "wc", "tr", "cat", "less", "more", "column"];

pub fn tool() {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    let Ok(event) = serde_json::from_str::<Value>(&input) else { return };
    if event["hook_event_name"].as_str() != Some("PreToolUse") {
        return;
    }
    let cwd = event["cwd"].as_str().map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let ctx = Ctx {
        sem: std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "sem".into()),
        weave: std::env::var("WEAVE_BIN").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "weave".into()),
        session: event["session_id"].as_str().unwrap_or("").to_string(),
        codex: event["transcript_path"].as_str().is_some_and(|p| p.contains("/.codex/")) || event["turn_id"].is_string(),
        cwd,
    };
    let name = event["tool_name"].as_str().unwrap_or("");
    let input = &event["tool_input"];
    let out = match name {
        "Bash" => bash(&ctx, input),
        "apply_patch" => apply_patch(&ctx, input),
        n if n.starts_with("mcp__sem") || n.starts_with("mcp__weave") || n.starts_with("sem_") || n.starts_with("weave_") => {
            ctx.log(if n.contains("weave") { "weave" } else { "sem" }, true, n);
            None
        }
        "Read" | "Edit" | "Write" | "MultiEdit" | "Grep" | "Glob" | "NotebookEdit" => {
            ctx.log("native", false, name);
            None
        }
        _ => None,
    };
    if let Some(v) = out {
        println!("{v}");
    }
}

struct Ctx {
    sem: String,
    weave: String,
    session: String,
    codex: bool,
    cwd: PathBuf,
}

impl Ctx {
    fn log(&self, kind: &str, routed: bool, detail: &str) {
        let dir = match std::env::var("SEM_COVERAGE_DIR").ok().filter(|d| !d.is_empty()) {
            Some(d) => PathBuf::from(d),
            None => match run_dir(&self.cwd) {
                Some(d) => d.join("coverage"),
                None => return,
            },
        };
        let _ = std::fs::create_dir_all(&dir);
        let id = if self.session.is_empty() { "none" } else { &self.session };
        let line = json!({"kind": kind, "routed": routed, "detail": detail.chars().take(200).collect::<String>()}).to_string();
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(format!("{id}.jsonl"))) {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// `<git common dir>/sem-run`.
fn run_dir(cwd: &Path) -> Option<PathBuf> {
    let o = std::process::Command::new("git").args(["rev-parse", "--path-format=absolute", "--git-common-dir"]).current_dir(cwd).output().ok()?;
    o.status.success().then(|| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()).join("sem-run"))
}

fn allow(input: &Value, command: String) -> Value {
    let mut ti = input.clone();
    ti["command"] = Value::String(command);
    json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "allow", "updatedInput": ti}})
}

fn deny(why: String) -> Value {
    json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": why}})
}

fn quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,@+".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Shell words, POSIX-style quoting; None on unbalanced quotes.
fn words(s: &str) -> Option<Vec<String>> {
    let (mut out, mut cur, mut quote, mut any) = (Vec::new(), String::new(), None::<char>, false);
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match quote {
            Some('\'') if c == '\'' => quote = None,
            Some('"') if c == '"' => quote = None,
            Some('"') if c == '\\' && it.peek().is_some_and(|n| "\"\\$`".contains(*n)) => cur.push(it.next()?),
            Some(_) => cur.push(c),
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    any = true
                }
                '\\' => {
                    cur.push(it.next()?);
                    any = true
                }
                c if c.is_whitespace() => {
                    if any || !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                        any = false;
                    }
                }
                _ => cur.push(c),
            },
        }
    }
    if quote.is_some() {
        return None;
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    Some(out)
}

/// Split on top-level separators outside quotes: (segment, separator).
fn split_top(cmd: &str) -> Vec<(String, String)> {
    let b: Vec<char> = cmd.chars().collect();
    let (mut out, mut cur, mut quote, mut i) = (Vec::new(), String::new(), None::<char>, 0);
    while i < b.len() {
        let c = b[i];
        if let Some(q) = quote {
            cur.push(c);
            if c == '\\' && q == '"' && i + 1 < b.len() {
                cur.push(b[i + 1]);
                i += 1;
            } else if c == q {
                quote = None;
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
            cur.push(c);
        } else if (c == '&' || c == '|') && b.get(i + 1) == Some(&c) {
            out.push((std::mem::take(&mut cur), format!("{c}{c}")));
            i += 1;
        } else if c == ';' || c == '\n' {
            out.push((std::mem::take(&mut cur), c.to_string()));
        } else {
            cur.push(c);
        }
        i += 1;
    }
    out.push((cur, String::new()));
    out
}

/// Pipeline stages, splitting only on a single `|` outside quotes.
fn pipes(seg: &str) -> Vec<String> {
    let b: Vec<char> = seg.chars().collect();
    let (mut out, mut cur, mut quote, mut i) = (Vec::new(), String::new(), None::<char>, 0);
    while i < b.len() {
        let c = b[i];
        if let Some(q) = quote {
            cur.push(c);
            if c == '\\' && i + 1 < b.len() {
                cur.push(b[i + 1]);
                i += 1;
            } else if c == q {
                quote = None;
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
            cur.push(c);
        } else if c == '\\' && i + 1 < b.len() {
            cur.push(c);
            cur.push(b[i + 1]);
            i += 1;
        } else if c == '|' {
            out.push(std::mem::take(&mut cur).trim().to_string());
        } else {
            cur.push(c);
        }
        i += 1;
    }
    out.push(cur.trim().to_string());
    out
}

fn first_word(stage: &str) -> String {
    words(stage).and_then(|w| w.into_iter().next()).unwrap_or_default()
}

fn quiet(seg: &str) -> String {
    static RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r"\s+2>\s*/dev/null|\s+2>&1").unwrap());
    RE.replace_all(seg, "").into_owned()
}

/// `cat|head|sed -n|nl FILE...` (possibly piped into filters) as a sem read.
fn read_segment(ctx: &Ctx, seg: &str) -> Option<String> {
    let seg = quiet(seg);
    let mut stages = pipes(&seg);
    if stages[0].contains(['>', '<', '`']) || stages[0].contains("$(") {
        return None;
    }
    let w = words(&stages[0])?;
    if stages[1..].iter().any(|s| !FILTERS.contains(&first_word(s).as_str())) {
        return None;
    }
    let (mut files, mut lines): (Vec<String>, Option<String>) = (Vec::new(), None);
    match w.first()?.as_str() {
        "cat" | "nl" => files = w[1..].iter().filter(|a| !a.starts_with('-')).cloned().collect(),
        "head" => {
            let mut n = 10;
            let mut i = 1;
            while i < w.len() {
                let a = &w[i];
                if a == "-n" && i + 1 < w.len() {
                    n = w[i + 1].parse().unwrap_or(n);
                    i += 1;
                } else if let Some(k) = a.strip_prefix("-n").or(a.strip_prefix('-')).and_then(|k| k.parse().ok()) {
                    n = k;
                } else if !a.starts_with('-') {
                    files.push(a.clone());
                }
                i += 1;
            }
            lines = Some(format!("1-{n}"));
        }
        "sed" if w.len() >= 4 && w[1] == "-n" => {
            static P: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r"^(\d+)(?:,(\d+|\$))?p$").unwrap());
            let m = P.captures(&w[2])?;
            let a = m.get(1)?.as_str();
            lines = Some(match m.get(2).map(|x| x.as_str()) {
                Some("$") => format!("{a}-"),
                Some(b) => format!("{a}-{b}"),
                None => format!("{a}-{a}"),
            });
            files = w[3..].to_vec();
        }
        _ => return None,
    }
    if files.is_empty() || files.iter().any(|f| f.starts_with('-') || f.contains(['*', '?'])) {
        return None;
    }
    // `cat f | sed -n 'A,Bp'` and `cat f | head -n N`: line ranges of f
    if lines.is_none() && files.len() == 1 && stages.len() > 1 {
        static S: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r"^sed -n '?(\d+),(\d+)p'?$|^head -n ?(\d+)$|^head -(\d+)$").unwrap());
        if let Some(m) = S.captures(&stages[1]) {
            lines = Some(match (m.get(1), m.get(2), m.get(3).or(m.get(4))) {
                (Some(a), Some(b), _) => format!("{}-{}", a.as_str(), b.as_str()),
                (_, _, Some(n)) => format!("1-{}", n.as_str()),
                _ => return None,
            });
            stages.remove(1);
        }
    }
    let mut cmd = format!("SEM_SESSION={} {} find --read {}", quote(&ctx.session), quote(&ctx.sem), files.iter().map(|f| quote(f)).collect::<Vec<_>>().join(" "));
    if let Some(l) = lines {
        cmd.push_str(&format!(" --lines {l}"));
    }
    for s in &stages[1..] {
        cmd.push_str(&format!(" | {s}"));
    }
    Some(cmd)
}

/// `grep|rg PATTERN [PATHS]` as `sem grep`.
fn grep_segment(ctx: &Ctx, seg: &str) -> Option<String> {
    let seg = quiet(seg);
    let stages = pipes(&seg);
    if stages[0].contains(['>', '`']) || stages[0].contains("$(") {
        return None;
    }
    let w = words(&stages[0])?;
    let tool = w.first()?.as_str();
    if !matches!(tool, "grep" | "egrep" | "rg") || stages[1..].iter().any(|s| !FILTERS.contains(&first_word(s).as_str())) {
        return None;
    }
    let (mut pats, mut paths, mut flags) = (Vec::<String>::new(), Vec::<String>::new(), Vec::<&str>::new());
    let (mut fixed, mut word, mut extended) = (false, false, tool != "grep");
    let mut i = 1;
    while i < w.len() {
        let a = w[i].as_str();
        match a {
            "-e" | "--regexp" if i + 1 < w.len() => {
                pats.push(w[i + 1].clone());
                i += 2;
                continue;
            }
            "-g" | "--glob" | "-t" | "--type" | "--include" | "--exclude" | "--exclude-dir" => {
                i += 2;
                continue;
            }
            "--line-number" | "--recursive" | "--no-heading" | "--color=never" | "--with-filename" => {}
            "--extended-regexp" => extended = true,
            "--ignore-case" => flags.push("-i"),
            "--files-with-matches" => flags.push("-l"),
            "--fixed-strings" => fixed = true,
            a if a.starts_with("--include=") || a.starts_with("--exclude") || a.starts_with("--glob=") => {}
            a if a.starts_with("--") => return None,
            a if a.starts_with('-') && a.len() > 1 => {
                for ch in a[1..].chars() {
                    match ch {
                        'i' => flags.push("-i"),
                        'l' => flags.push("-l"),
                        'F' => fixed = true,
                        'w' => word = true,
                        'E' => extended = true,
                        'r' | 'R' | 'n' | 'H' | 's' | 'I' | 'P' => {}
                        _ => return None, // context lines, counts, inversion: not a plain search
                    }
                }
            }
            _ if pats.is_empty() => pats.push(a.to_string()),
            _ => paths.push(a.to_string()),
        }
        i += 1;
    }
    if pats.is_empty() {
        return None;
    }
    let shape = |p: &str| {
        let mut p = if fixed { regex::escape(p) } else { p.to_string() };
        if !fixed && !extended {
            // grep's basic regex: \| \( \) \+ \? \{ \} are the operators
            p = Regex::new(r"\\([|()+?{}])").unwrap().replace_all(&p, "$1").into_owned();
        }
        if word {
            p = format!(r"\b(?:{p})\b");
        }
        p
    };
    flags.dedup();
    let mut cmd = format!("{} grep", quote(&ctx.sem));
    for f in &flags {
        cmd.push_str(&format!(" {f}"));
    }
    if pats.len() == 1 {
        cmd.push_str(&format!(" {}", quote(&shape(&pats[0]))));
    } else {
        for p in &pats {
            cmd.push_str(&format!(" -e {}", quote(&shape(p))));
        }
    }
    for p in paths.iter().filter(|p| p.as_str() != ".") {
        cmd.push_str(&format!(" {}", quote(p)));
    }
    for s in &stages[1..] {
        cmd.push_str(&format!(" | {s}"));
    }
    Some(cmd)
}

fn heavy(cmd: &str) -> bool {
    static RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"\b(cargo\s+(test|build|check|nextest|clippy|run)|go\s+(test|build|vet)|pytest|python3?\s+-m\s+(pytest|unittest)|npm\s+(test|run)|yarn\s+(test|jest|build|tsc)|pnpm\s+(test|run)|npx\s+(jest|vitest|tsc|mocha)|mvn|mvnw|gradle|gradlew|make|bench-exec|dotnet\s+(test|build)|swift\s+(test|build)|tox|nox)\b").unwrap()
    });
    RE.is_match(cmd)
}

/// A whole command that writes files through the shell.
fn shell_write(cmd: &str) -> bool {
    static EDIT: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(^|[;&|]\s*)(sudo\s+)?(sed\s+(-[a-zA-Z]*i|--in-place)|perl\s+-[a-zA-Z]*[pi]|tee|truncate|patch\s)").unwrap()
    });
    static REDIRECT: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r"(^|[^<>&0-9])>{1,2}\s*[\w./-]+\.\w+").unwrap());
    static SCRIPT: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r#"open\([^)]*,\s*['"][wa]|\.write_text\("#).unwrap());
    EDIT.is_match(cmd) || REDIRECT.is_match(&cmd.replace("/dev/null", "")) || (cmd.contains("python") && SCRIPT.is_match(cmd))
}

/// `sed -i 's/A/B/[g]' files` or `perl -pi -e ...` with literal A and B, as a
/// weave edit batch over the files that contain A.
fn literal_substitution(ctx: &Ctx, cmd: &str) -> Option<String> {
    if split_top(cmd).len() > 1 || pipes(cmd).len() > 1 || cmd.contains('`') || cmd.contains("$(") {
        return None;
    }
    let w = words(cmd)?;
    let (mut expr, mut files, mut inplace) = (None::<String>, Vec::new(), false);
    let mut i = 1;
    match w.first()?.as_str() {
        "sed" => {
            while i < w.len() {
                let a = &w[i];
                if a.starts_with("-i") {
                    inplace = true;
                    if a == "-i" && w.get(i + 1).is_some_and(|n| n.is_empty() || n.starts_with('.')) {
                        i += 1;
                    }
                } else if a == "-e" {
                    expr = Some(w.get(i + 1)?.clone());
                    i += 1;
                } else if a == "-E" || a == "-r" {
                } else if a.starts_with('-') {
                    return None;
                } else if expr.is_none() {
                    expr = Some(a.clone());
                } else {
                    files.push(a.clone());
                }
                i += 1;
            }
        }
        "perl" => {
            while i < w.len() {
                let a = &w[i];
                if a.starts_with('-') {
                    if a.contains('i') {
                        inplace = true;
                    }
                    if a.ends_with('e') {
                        expr = Some(w.get(i + 1)?.clone());
                        i += 1;
                    }
                } else {
                    files.push(a.clone());
                }
                i += 1;
            }
        }
        _ => return None,
    }
    let expr = expr?;
    if !inplace || expr.len() < 4 || !expr.starts_with('s') {
        return None;
    }
    let d = expr.chars().nth(1)?;
    let parts: Vec<&str> = expr[2..].split(d).collect();
    let [old, new, fl] = parts[..] else { return None };
    if old.is_empty() || old.contains(|c: char| ".*[]^$+?(){}|\\".contains(c)) || new.contains(['\\', '&', '$']) || !fl.chars().all(|c| c == 'g') {
        return None;
    }
    let mut edits = Vec::new();
    for f in files {
        let path = ctx.cwd.join(&f);
        let text = std::fs::read_to_string(&path).ok()?;
        if text.contains(old) {
            if fl.is_empty() && text.lines().any(|l| l.matches(old).count() > 1) {
                return None;
            }
            edits.push(json!({"file": f, "old": old, "new": new, "all": !fl.is_empty()}));
        }
    }
    (!edits.is_empty()).then(|| format!("{} edit {}", quote(&ctx.weave), quote(&Value::Array(edits).to_string())))
}

fn bash(ctx: &Ctx, input: &Value) -> Option<Value> {
    let cmd = input["command"].as_str()?;
    if let Some(edit) = literal_substitution(ctx, cmd) {
        ctx.log("edit", true, cmd);
        return Some(allow(input, edit));
    }
    if heavy(cmd) && !cmd.contains("check --run") && !input["run_in_background"].as_bool().unwrap_or(false) {
        ctx.log("check", true, cmd);
        // run in the agent's own shell, where its environment is (an MCP server
        // is started with a stripped environment: virtualenvs, project
        // variables and PATH additions would be missing there)
        let idle = if Regex::new(r"\s(-q|--quiet|-qq)\b").unwrap().is_match(cmd) { 0 } else { 300 };
        return Some(allow(input, format!("{} check --run {} --idle {idle}", quote(&ctx.sem), quote(cmd))));
    }
    if shell_write(cmd) {
        ctx.log("edit", false, cmd);
        return Some(deny("Change files with the weave_edit tool (entity + content, entity + old/new, old/new on the whole file, rename, into; content alone creates a file), not the shell.".into()));
    }
    let mut changed = false;
    let mut out = String::new();
    for (seg, sep) in split_top(cmd) {
        let core = seg.trim();
        match (!core.is_empty()).then(|| read_segment(ctx, core).or_else(|| grep_segment(ctx, core))).flatten() {
            Some(new) => {
                out.push_str(&seg[..seg.len() - seg.trim_start().len()]);
                out.push_str(&new);
                out.push_str(&seg[seg.trim_end().len()..]);
                changed = true;
            }
            None => out.push_str(&seg),
        }
        out.push_str(&sep);
    }
    if changed {
        ctx.log("read", true, cmd);
        return Some(allow(input, out));
    }
    static READS: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r"(^|[;&|]\s*)(cat|head|tail|sed\s+-n|nl|less|grep|rg|awk)\b").unwrap());
    static LISTS: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r"(^|[;&|]\s*)(ls|find\s|rg\s+--files|git\s+ls-files|tree)\b").unwrap());
    let kind = if READS.is_match(cmd) { "read" } else if LISTS.is_match(cmd) { "listing" } else { "shell" };
    ctx.log(kind, false, cmd);
    None
}

/// Codex's apply_patch, one weave old/new edit per hunk (or a new file), run
/// by weave; the native patch is then not applied.
fn apply_patch(ctx: &Ctx, input: &Value) -> Option<Value> {
    let text = input["command"].as_str().or(input["input"].as_str())?;
    let mut edits: Vec<Value> = Vec::new();
    let (mut cur, mut hunk): (Option<String>, Vec<String>) = (None, Vec::new());
    let flush = |cur: &Option<String>, hunk: &mut Vec<String>, edits: &mut Vec<Value>| -> bool {
        if let (Some(f), false) = (cur, hunk.is_empty()) {
            let old: String = hunk.iter().filter(|l| l.starts_with([' ', '-'])).map(|l| format!("{}\n", &l[1..])).collect();
            let new: String = hunk.iter().filter(|l| l.starts_with([' ', '+'])).map(|l| format!("{}\n", &l[1..])).collect();
            if old.trim().is_empty() {
                return false;
            }
            edits.push(json!({"file": f, "old": old, "new": new}));
        }
        hunk.clear();
        true
    };
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        if let Some(f) = l.strip_prefix("*** Update File: ") {
            if !flush(&cur, &mut hunk, &mut edits) {
                return None;
            }
            cur = Some(f.trim().to_string());
        } else if let Some(f) = l.strip_prefix("*** Add File: ") {
            if !flush(&cur, &mut hunk, &mut edits) {
                return None;
            }
            let mut body = String::new();
            i += 1;
            while i < lines.len() && !lines[i].starts_with("*** ") {
                body.push_str(lines[i].strip_prefix('+').unwrap_or(lines[i]));
                body.push('\n');
                i += 1;
            }
            edits.push(json!({"file": f.trim(), "content": body}));
            cur = None;
            continue;
        } else if l.starts_with("*** Delete File") || l.starts_with("*** Move to") {
            ctx.log("edit", false, "apply_patch delete/move");
            return None;
        } else if l.starts_with("@@") {
            if !flush(&cur, &mut hunk, &mut edits) {
                return None;
            }
        } else if l.starts_with("*** ") {
        } else if cur.is_some() && (l.starts_with([' ', '-', '+']) || l.is_empty()) {
            hunk.push(if l.is_empty() { " ".into() } else { l.to_string() });
        }
        i += 1;
    }
    if !flush(&cur, &mut hunk, &mut edits) || edits.is_empty() {
        ctx.log("edit", false, "apply_patch untranslatable");
        return None;
    }
    for e in &mut edits {
        if let Some(f) = e["file"].as_str() {
            if let Ok(rel) = Path::new(f).strip_prefix(&ctx.cwd) {
                e["file"] = Value::String(rel.to_string_lossy().into_owned());
            }
        }
    }
    let r = std::process::Command::new(&ctx.weave).args(["edit", &Value::Array(edits.clone()).to_string()]).current_dir(&ctx.cwd).output();
    ctx.log("edit", true, &format!("apply_patch: {} hunks", edits.len()));
    Some(match r {
        Ok(o) if o.status.success() => deny(format!(
            "Applied through weave; the change is in the files now, so do not apply this patch again.\n{}",
            String::from_utf8_lossy(&o.stdout).trim()
        )),
        Ok(o) => deny(format!(
            "weave refused this patch and nothing was written:\n{}\nRead the current text and send the patch again, or use weave_edit.",
            format!("{}{}", String::from_utf8_lossy(&o.stderr), String::from_utf8_lossy(&o.stdout)).trim()
        )),
        Err(e) => deny(format!("weave is not available ({e}); set WEAVE_BIN, or use weave_edit")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx {
        Ctx { sem: "sem".into(), weave: "weave".into(), session: "s".into(), codex: false, cwd: std::env::temp_dir() }
    }

    #[test]
    fn reads_and_searches_become_sem() {
        let c = ctx();
        assert_eq!(read_segment(&c, "sed -n '10,40p' src/x.py").unwrap(), "SEM_SESSION=s sem find --read src/x.py --lines 10-40");
        assert_eq!(read_segment(&c, "nl -ba a.py | sed -n '5,9p'").unwrap(), "SEM_SESSION=s sem find --read a.py --lines 5-9");
        assert_eq!(grep_segment(&c, r#"grep -n "^import\|^from" a.py b.py 2>/dev/null"#).unwrap(), "sem grep '^import|^from' a.py b.py");
        assert_eq!(grep_segment(&c, "rg -n -w Foo src | head -20").unwrap(), r"sem grep '\b(?:Foo)\b' src | head -20");
        assert!(grep_segment(&c, "grep -A3 foo a.py").is_none());
        assert!(read_segment(&c, "cat > x.py").is_none());
    }

    #[test]
    fn shell_writes_are_recognised() {
        assert!(shell_write("sed -i 's/a/b/' x.py"));
        assert!(shell_write("echo hi > notes.txt"));
        assert!(!shell_write("go test ./... 2>/dev/null"));
        assert!(heavy("go test ./... | tail -5") && !heavy("git status"));
    }
}
