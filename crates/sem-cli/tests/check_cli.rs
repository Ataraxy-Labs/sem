//! `sem check` end to end on fixture projects. The property every case asserts:
//! sem check's verdict (and, for TypeScript and ESLint, its diagnostics) equals
//! the full tool's on the same tree — whatever mode it chose. Mode assertions
//! then pin down that the incremental path is actually taken where it should
//! be, and the full check (with the right reason) where it must be.
//!
//! The TypeScript, ESLint and vitest cases run the pinned tools that
//! `scripts/check-fixture-tools.sh` installs into `crates/target/check-tools`
//! (or `$SEM_CHECK_TOOLS`, a node_modules directory). Without them those cases
//! are skipped with a message, unless `SEM_CHECK_REQUIRE_TOOLS=1`.
// The fixtures link pinned Node toolchains in with symlinks; these tests run on Unix CI.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn tools() -> Option<PathBuf> {
    let p = std::env::var("SEM_CHECK_TOOLS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/check-tools/node_modules"));
    if p.join("typescript/package.json").exists() {
        return Some(p.canonicalize().unwrap());
    }
    if std::env::var("SEM_CHECK_REQUIRE_TOOLS").as_deref() == Ok("1") {
        panic!("fixture tools missing at {} (run scripts/check-fixture-tools.sh)", p.display());
    }
    eprintln!("SKIP: fixture tools missing at {} (run scripts/check-fixture-tools.sh)", p.display());
    None
}

struct Repo {
    dir: tempfile::TempDir,
    store: tempfile::TempDir,
}

impl Repo {
    fn new(files: &[(&str, &str)], tools: Option<&Path>) -> Repo {
        let dir = tempfile::tempdir().unwrap();
        let r = Repo { dir, store: tempfile::tempdir().unwrap() };
        for (p, c) in files {
            r.write(p, c);
        }
        r.write(".gitignore", "node_modules\n");
        if let Some(t) = tools {
            std::os::unix::fs::symlink(t, r.path().join("node_modules")).unwrap();
        }
        r.git(&["init", "-q", "-b", "main"]);
        r.git(&["config", "user.email", "t@example.com"]);
        r.git(&["config", "user.name", "T"]);
        r.commit("init");
        r
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }
    fn write(&self, p: &str, c: &str) {
        let f = self.path().join(p);
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(f, c).unwrap();
    }
    fn remove(&self, p: &str) {
        fs::remove_file(self.path().join(p)).unwrap();
    }
    fn git(&self, args: &[&str]) -> String {
        let o = Command::new("git").current_dir(self.path()).args(args).output().unwrap();
        assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }
    fn commit(&self, msg: &str) -> String {
        self.git(&["add", "--all", "--", "."]);
        self.git(&["-c", "commit.gpgsign=false", "commit", "-q", "--allow-empty", "-m", msg]);
        self.git(&["rev-parse", "HEAD"])
    }
    fn sem(&self, args: &[&str]) -> (i32, Value) {
        let o: Output = Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(self.path())
            .env("SEM_CHECK_CACHE_DIR", self.store.path())
            .args(["check", "--json"])
            .args(args)
            .output()
            .unwrap();
        let v: Value = serde_json::from_slice(&o.stdout)
            .unwrap_or_else(|e| panic!("sem check output is not JSON ({e}): {}\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)));
        (o.status.code().unwrap_or(-1), v)
    }
}

fn checker<'a>(v: &'a Value, name: &str) -> &'a Value {
    v["checkers"].as_array().unwrap().iter().find(|c| c["name"] == name).unwrap_or_else(|| panic!("no {name} checker in {v:#}"))
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().map(|x| x.as_str().unwrap().to_string()).collect()).unwrap_or_default()
}

// ---- TypeScript --------------------------------------------------------------

/// `tsc --pretty false` on the repo, as sorted diagnostics (one per entry,
/// continuation lines joined).
fn tsc(repo: &Repo) -> Vec<String> {
    let o = Command::new("node").current_dir(repo.path()).args(["node_modules/typescript/bin/tsc", "--pretty", "false"]).output().unwrap();
    let mut out: Vec<String> = Vec::new();
    for line in String::from_utf8_lossy(&o.stdout).lines() {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with(' ') && !out.is_empty() {
            let last = out.last_mut().unwrap();
            last.push('\n');
            last.push_str(line);
        } else {
            out.push(line.to_string());
        }
    }
    out.sort();
    out
}

/// Run `sem check --checkers ts` and assert it equals tsc; return the ts result.
fn ts_case(repo: &Repo, base: Option<&str>, label: &str) -> Value {
    let mut args = vec!["--checkers", "ts"];
    if let Some(b) = base {
        args.push("--base");
        args.push(b);
    }
    let (code, v) = repo.sem(&args);
    let c = checker(&v, "ts").clone();
    let full = tsc(repo);
    assert_eq!(strs(&c["diagnostics"]), full, "[{label}] sem check diagnostics differ from tsc's\n{c:#}");
    let want = if full.is_empty() { "pass" } else { "fail" };
    assert_eq!(c["verdict"], want, "[{label}] verdict\n{c:#}");
    assert_eq!(code, if full.is_empty() { 0 } else { 1 }, "[{label}] exit code");
    c
}

const TS_FILES: &[(&str, &str)] = &[
    ("package.json", r#"{"name":"fx","private":true,"workspaces":["packages/*"]}"#),
    (
        "tsconfig.json",
        r#"{
  "compilerOptions": {
    "target": "ES2022", "module": "ESNext", "moduleResolution": "bundler", "strict": true,
    "noEmit": true, "skipLibCheck": true, "lib": ["ES2022"], "types": [],
    "paths": { "@fx/core": ["./packages/core/src/index.ts"] }
  },
  "include": ["packages"]
}"#,
    ),
    ("packages/core/package.json", r#"{"name":"@fx/core"}"#),
    ("packages/core/src/index.ts", "export * from \"./math\";\nexport { greet } from \"./greet\";\n"),
    (
        "packages/core/src/math.ts",
        "export function add(a: number, b: number): number {\n  return a + b;\n}\nexport function mul(a: number, b: number): number {\n  return a * b;\n}\n",
    ),
    ("packages/core/src/greet.ts", "export function greet(name: string): string {\n  return \"hi \" + name;\n}\n"),
    ("packages/app/package.json", r#"{"name":"@fx/app"}"#),
    (
        "packages/app/src/main.ts",
        "import { add, greet } from \"@fx/core\";\nexport const total: number = add(1, 2);\nexport const msg: string = greet(\"x\");\n",
    ),
    ("packages/app/src/other.ts", "import { mul } from \"@fx/core\";\nexport const m: number = mul(2, 3);\n"),
    ("packages/app/src/globals.d.ts", "declare const APP_VERSION: string;\n"),
    ("packages/app/src/version.ts", "export const v: string = APP_VERSION;\n"),
    ("packages/app/src/win.ts", "export class App {\n  run(): void {}\n}\ndeclare global {\n  var appInstance: App;\n}\n"),
    ("packages/app/src/useWin.ts", "export const r: void = appInstance.run();\n"),
    ("packages/app/src/res.ts", "import { k } from \"./dir\";\nexport const kk: number = k;\n"),
    ("packages/app/src/dir/index.ts", "export const k = 1;\n"),
    ("packages/app/src/pwa.ts", "export const helper = (n: number): number => n;\ndeclare global {\n  interface PromptChoice {\n    outcome: \"accepted\" | \"dismissed\";\n  }\n}\n"),
    ("packages/app/src/usePwa.ts", "export const c: PromptChoice = { outcome: \"accepted\" };\n"),
];

#[test]
fn typescript_verdicts_equal_tsc_in_every_case() {
    let Some(t) = tools() else { return };
    let repo = Repo::new(TS_FILES, Some(&t));
    let c0 = repo.git(&["rev-parse", "HEAD"]);

    // bootstrap: no state anywhere -> full
    let c = ts_case(&repo, None, "bootstrap");
    assert_eq!(c["mode"], "full");
    assert!(strs(&c["reasons"])[0].starts_with("no-state"), "{c:#}");

    // the same tree again: incremental, nothing rechecked
    let c = ts_case(&repo, None, "unchanged");
    assert_eq!(c["mode"], "incremental");
    assert_eq!(c["filesRecheckedCount"], 0);

    // body-only change: only the file itself is rechecked
    repo.write(
        "packages/core/src/math.ts",
        "export function add(a: number, b: number): number {\n  return b + a;\n}\nexport function mul(a: number, b: number): number {\n  return a * b;\n}\n",
    );
    let c1 = repo.commit("body only");
    let c = ts_case(&repo, Some(&c0), "body-only");
    assert_eq!(c["mode"], "incremental", "{c:#}");
    assert_eq!(strs(&c["filesRechecked"]), vec!["packages/core/src/math.ts"]);
    assert_eq!(c["state"]["from"], "base");

    // interface change: add's dependents are rechecked (and fail); other.ts,
    // which uses only mul, is not (name-level cutoff)
    repo.write(
        "packages/core/src/math.ts",
        "export function add(a: number, b: string): number {\n  return a + b.length;\n}\nexport function mul(a: number, b: number): number {\n  return a * b;\n}\n",
    );
    let c2 = repo.commit("interface change");
    let c = ts_case(&repo, Some(&c1), "interface change");
    assert_eq!(c["mode"], "incremental", "{c:#}");
    let re = strs(&c["filesRechecked"]);
    assert!(re.contains(&"packages/app/src/main.ts".to_string()), "{re:?}");
    assert!(!re.contains(&"packages/app/src/other.ts".to_string()), "{re:?}");
    assert_eq!(c["verdict"], "fail");

    // fix it back; then a type error introduced in a dependent file only
    repo.write(
        "packages/core/src/math.ts",
        "export function add(a: number, b: number): number {\n  return b + a;\n}\nexport function mul(a: number, b: number): number {\n  return a * b;\n}\n",
    );
    let c3 = repo.commit("revert");
    let c = ts_case(&repo, Some(&c2), "revert");
    assert_eq!(c["mode"], "incremental");
    repo.write(
        "packages/app/src/main.ts",
        "import { add, greet } from \"@fx/core\";\nexport const total: string = add(1, 2);\nexport const msg: string = greet(\"x\");\n",
    );
    let c4 = repo.commit("dependent error");
    let c = ts_case(&repo, Some(&c3), "dependent error");
    assert_eq!(c["mode"], "incremental");
    assert_eq!(strs(&c["filesRechecked"]), vec!["packages/app/src/main.ts"]);
    assert_eq!(c["verdict"], "fail");
    repo.write(
        "packages/app/src/main.ts",
        "import { add, greet } from \"@fx/core\";\nexport const total: number = add(1, 2);\nexport const msg: string = greet(\"x\");\n",
    );
    let c5 = repo.commit("fix dependent");
    ts_case(&repo, Some(&c4), "fix dependent");

    // global declaration: a script file's interface changed -> full
    repo.write("packages/app/src/globals.d.ts", "declare const APP_VERSION: number;\n");
    let c6 = repo.commit("global");
    let c = ts_case(&repo, Some(&c5), "global declaration");
    assert_eq!(c["mode"], "full", "{c:#}");
    assert!(strs(&c["reasons"]).iter().any(|r| r.starts_with("global-interface: packages/app/src/globals.d.ts")), "{c:#}");
    assert_eq!(c["verdict"], "fail");
    repo.write("packages/app/src/globals.d.ts", "declare const APP_VERSION: string;\n");
    let c7 = repo.commit("global back");
    ts_case(&repo, Some(&c6), "global back");

    // tsconfig change -> full
    let cfg = fs::read_to_string(repo.path().join("tsconfig.json")).unwrap().replace("\"strict\": true,", "\"strict\": true, \"noUnusedLocals\": true,");
    repo.write("tsconfig.json", &cfg);
    let c8 = repo.commit("tsconfig");
    let c = ts_case(&repo, Some(&c7), "tsconfig");
    assert_eq!(c["mode"], "full");
    assert!(strs(&c["reasons"]).iter().any(|r| r.starts_with("config: tsconfig.json")), "{c:#}");

    // new file with an error: rechecked, nothing else
    repo.write("packages/app/src/added.ts", "import { greet } from \"@fx/core\";\nexport const n: number = greet(\"y\");\n");
    let c9 = repo.commit("new file");
    let c = ts_case(&repo, Some(&c8), "new file");
    assert_eq!(c["mode"], "incremental");
    assert_eq!(strs(&c["filesRechecked"]), vec!["packages/app/src/added.ts"]);
    assert_eq!(c["verdict"], "fail");

    // deleted file: its importers are rechecked and fail
    repo.remove("packages/app/src/added.ts");
    repo.remove("packages/core/src/greet.ts");
    let c10 = repo.commit("deleted file");
    let c = ts_case(&repo, Some(&c9), "deleted file");
    assert_eq!(c["mode"], "incremental", "{c:#}");
    let re = strs(&c["filesRechecked"]);
    assert!(re.contains(&"packages/core/src/index.ts".to_string()), "{re:?}");
    assert_eq!(c["verdict"], "fail");

    // `declare global` inside a module: a body-only edit stays incremental...
    repo.write("packages/app/src/win.ts", "export class App {\n  run(): void {\n    let n = 1;\n    n++;\n  }\n}\ndeclare global {\n  var appInstance: App;\n}\n");
    let c11 = repo.commit("global module body");
    let c = ts_case(&repo, Some(&c10), "global module, body only");
    assert_eq!(c["mode"], "incremental", "{c:#}");
    assert_eq!(strs(&c["filesRechecked"]), vec!["packages/app/src/win.ts"]);
    // ...an interface edit reaches the global scope: full
    repo.write("packages/app/src/win.ts", "export class App {\n  run(): number {\n    return 1;\n  }\n}\ndeclare global {\n  var appInstance: App;\n}\n");
    let c12 = repo.commit("global module interface");
    let c = ts_case(&repo, Some(&c11), "global module, interface");
    assert_eq!(c["mode"], "full", "{c:#}");
    assert!(strs(&c["reasons"]).iter().any(|r| r.starts_with("global-interface: packages/app/src/win.ts")), "{c:#}");
    assert_eq!(c["verdict"], "fail");

    // a module's exports change but its `declare global` block does not: no
    // global change, so no full check...
    repo.write("packages/app/src/pwa.ts", "export const helper = (n: string): string => n;\ndeclare global {\n  interface PromptChoice {\n    outcome: \"accepted\" | \"dismissed\";\n  }\n}\n");
    let c12a = repo.commit("pwa export");
    let c = ts_case(&repo, Some(&c12), "global module, export only");
    assert_eq!(c["mode"], "incremental", "{c:#}");
    assert_eq!(strs(&c["filesRechecked"]), vec!["packages/app/src/pwa.ts"]);
    // ...while a change inside the block, with no export changing, is one
    repo.write("packages/app/src/pwa.ts", "export const helper = (n: string): string => n;\ndeclare global {\n  interface PromptChoice {\n    outcome: \"yes\" | \"no\";\n  }\n}\n");
    let c12b = repo.commit("pwa global block");
    let c = ts_case(&repo, Some(&c12a), "global block only");
    assert_eq!(c["mode"], "full", "{c:#}");
    assert!(strs(&c["reasons"]).iter().any(|r| r.starts_with("global-interface: packages/app/src/pwa.ts")), "{c:#}");
    assert_eq!(c["verdict"], "fail");
    let c12 = c12b;

    // an unchanged file whose import now resolves to a new file is rechecked
    repo.write("packages/app/src/dir.ts", "export const k = \"s\";\n");
    let _c13 = repo.commit("shadowing file");
    let c = ts_case(&repo, Some(&c12), "resolution changed");
    assert_eq!(c["mode"], "incremental", "{c:#}");
    assert!(strs(&c["filesRechecked"]).contains(&"packages/app/src/res.ts".to_string()), "{c:#}");
    assert!(strs(&c["diagnostics"]).iter().any(|d| d.starts_with("packages/app/src/res.ts")), "{c:#}");

    // uncommitted edits are what is checked (base = HEAD)
    repo.write("packages/core/src/greet.ts", "export function greet(name: string): string {\n  return \"hi \" + name;\n}\n");
    repo.remove("packages/app/src/dir.ts");
    repo.write("packages/app/src/pwa.ts", "export const helper = (n: string): string => n;\ndeclare global {\n  interface PromptChoice {\n    outcome: \"accepted\" | \"dismissed\";\n  }\n}\n");
    repo.write("packages/app/src/win.ts", "export class App {\n  run(): void {}\n}\ndeclare global {\n  var appInstance: App;\n}\n");
    let c = ts_case(&repo, None, "dirty tree");
    assert_eq!(c["verdict"], "pass");
    assert_eq!(repo.sem(&["--checkers", "ts"]).1["head"]["dirty"], true);
}

#[test]
fn typescript_certificate_and_state_digests() {
    let Some(t) = tools() else { return };
    let repo = Repo::new(TS_FILES, Some(&t));
    let (code, v) = repo.sem(&["--checkers", "ts"]);
    assert_eq!(code, 0, "{v:#}");
    let cert = &v["certificate"];
    assert_eq!(cert["schema"], "sem-check-certificate/1");
    assert_eq!(cert["inputs"]["tree"], v["head"]["tree"]);
    assert_eq!(cert["inputs"]["baseTree"], v["base"]["tree"]);
    assert_eq!(cert["checkers"][0]["mode"], "full");
    assert!(cert["checkers"][0]["stateOut"].as_str().is_some_and(|s| s.len() == 64));
    assert!(cert["digest"].as_str().is_some_and(|s| s.len() == 40));
    assert!(cert["checkers"][0]["toolVersion"] == "5.9.3");
    // a second run reads the state the first wrote
    let (_, v2) = repo.sem(&["--checkers", "ts"]);
    assert_eq!(v2["certificate"]["checkers"][0]["stateIn"], cert["checkers"][0]["stateOut"]);
    // --no-cache: no state read or written, full
    let (_, v3) = repo.sem(&["--checkers", "ts", "--no-cache"]);
    assert_eq!(checker(&v3, "ts")["mode"], "full");
    assert!(checker(&v3, "ts")["state"]["out"].is_null());
}

/// A state is shared by every checkout of the tree: a clone elsewhere, with its
/// own node_modules link, starts incremental from it (a land queue's lander
/// worktree and the submitter's clone are different checkouts).
#[test]
fn states_are_shared_across_checkouts() {
    let Some(t) = tools() else { return };
    let repo = Repo::new(TS_FILES, Some(&t));
    let c = ts_case(&repo, None, "first checkout");
    assert_eq!(c["mode"], "full");
    let other = tempfile::tempdir().unwrap();
    let dst = other.path().join("nested/clone");
    fs::create_dir_all(dst.parent().unwrap()).unwrap();
    let o = Command::new("git").args(["clone", "-q"]).arg(repo.path()).arg(&dst).output().unwrap();
    assert!(o.status.success());
    std::os::unix::fs::symlink(&t, dst.join("node_modules")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_sem"))
        .current_dir(&dst)
        .env("SEM_CHECK_CACHE_DIR", repo.store.path())
        .args(["check", "--json", "--checkers", "ts"])
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let c = checker(&v, "ts");
    assert_eq!(c["mode"], "incremental", "{c:#}");
    assert_eq!(c["filesRecheckedCount"], 0, "{c:#}");
    assert_eq!(c["state"]["from"], "base");
}

#[test]
fn typescript_external_declarations_changing_forces_full() {
    let Some(t) = tools() else { return };
    // a real (copied, not symlinked) node_modules package whose d.ts we edit
    let repo = Repo::new(
        &[
            ("package.json", r#"{"name":"fx","private":true}"#),
            ("tsconfig.json", r#"{"compilerOptions":{"strict":true,"noEmit":true,"module":"ESNext","moduleResolution":"bundler","types":[],"lib":["ES2022"]},"include":["src"]}"#),
            ("src/a.ts", "import { dep } from \"dep\";\nexport const x: number = dep;\n"),
        ],
        None,
    );
    fs::create_dir_all(repo.path().join("node_modules/dep")).unwrap();
    std::os::unix::fs::symlink(t.join("typescript"), repo.path().join("node_modules/typescript")).unwrap();
    repo.write("node_modules/dep/package.json", r#"{"name":"dep","types":"index.d.ts"}"#);
    repo.write("node_modules/dep/index.d.ts", "export declare const dep: number;\n");
    let c = ts_case(&repo, None, "bootstrap");
    assert_eq!(c["mode"], "full");
    repo.write("node_modules/dep/index.d.ts", "export declare const dep: string;\n");
    let c = ts_case(&repo, None, "external d.ts changed");
    assert_eq!(c["mode"], "full", "{c:#}");
    assert!(strs(&c["reasons"]).iter().any(|r| r.starts_with("external:")), "{c:#}");
    assert_eq!(c["verdict"], "fail");
}

#[test]
fn tsgo_backend_when_the_project_uses_tsgo() {
    let Some(t) = tools() else { return };
    if !t.join(".bin/tsgo").exists() {
        return;
    }
    let repo = Repo::new(
        &[
            ("package.json", r#"{"name":"fx","private":true,"scripts":{"typecheck":"tsgo"}}"#),
            ("tsconfig.json", r#"{"compilerOptions":{"strict":true,"noEmit":true,"module":"ESNext","moduleResolution":"bundler","types":[]},"include":["src"]}"#),
            ("src/a.ts", "export const x: number = 1;\n"),
        ],
        Some(&t),
    );
    let tsgo = |r: &Repo| {
        let o = Command::new(t.join(".bin/tsgo")).current_dir(r.path()).args(["--pretty", "false"]).output().unwrap();
        let mut d: Vec<String> = String::from_utf8_lossy(&o.stdout).lines().filter(|l| !l.trim().is_empty()).map(String::from).collect();
        d.sort();
        d
    };
    let (code, v) = repo.sem(&["--checkers", "ts"]);
    let c = checker(&v, "ts");
    assert_eq!(c["tool"], "tsgo", "{c:#}");
    assert_eq!(c["mode"], "full");
    assert_eq!(code, 0);
    assert_eq!(strs(&c["filesRechecked"]), vec!["src/a.ts"]);
    repo.write("src/b.ts", "export const y: string = 1;\n");
    let (code, v) = repo.sem(&["--checkers", "ts"]);
    let c = checker(&v, "ts");
    assert_eq!(code, 1);
    assert_eq!(strs(&c["diagnostics"]), tsgo(&repo));
    // the project's tsc instead, on request: same verdict here
    let (code, v) = repo.sem(&["--checkers", "ts", "--ts-backend", "tsc"]);
    assert_eq!(code, 1);
    assert_eq!(checker(&v, "ts")["tool"], "tsc");
}

// ---- ESLint ------------------------------------------------------------------

/// `eslint .` on the repo, in sem check's message format, sorted; and whether it passed.
fn eslint(repo: &Repo) -> (bool, Vec<String>) {
    let o = Command::new("node").current_dir(repo.path()).args(["node_modules/eslint/bin/eslint.js", "-f", "json", "."]).output().unwrap();
    let v: Value = serde_json::from_slice(&o.stdout).unwrap_or(Value::Null);
    let mut out = Vec::new();
    for f in v.as_array().into_iter().flatten() {
        let rel = Path::new(f["filePath"].as_str().unwrap()).strip_prefix(repo.path().canonicalize().unwrap()).unwrap().to_string_lossy().to_string();
        for m in f["messages"].as_array().into_iter().flatten() {
            out.push(format!(
                "{rel}:{}:{}: {} {}: {}",
                m["line"].as_u64().unwrap_or(0),
                m["column"].as_u64().unwrap_or(0),
                if m["severity"] == 2 { "error" } else { "warning" },
                m["ruleId"].as_str().unwrap_or(if m["fatal"] == true { "fatal" } else { "-" }),
                m["message"].as_str().unwrap()
            ));
        }
    }
    out.sort();
    (o.status.success(), out)
}

fn lint_case(repo: &Repo, base: Option<&str>, label: &str) -> Value {
    let mut args = vec!["--checkers", "lint"];
    if let Some(b) = base {
        args.push("--base");
        args.push(b);
    }
    let (_, v) = repo.sem(&args);
    let c = checker(&v, "lint").clone();
    let (ok, full) = eslint(repo);
    let mut got = strs(&c["diagnostics"]);
    got.sort();
    assert_eq!(got, full, "[{label}] sem check messages differ from eslint's\n{c:#}");
    assert_eq!(c["verdict"], if ok { "pass" } else { "fail" }, "[{label}] verdict\n{c:#}");
    c
}

const LINT_FILES: &[(&str, &str)] = &[
    ("package.json", r#"{"name":"lintfx","private":true,"type":"module"}"#),
    (
        "eslint.config.js",
        "import importPlugin from \"eslint-plugin-import\";\nexport default [{ files: [\"**/*.js\"], plugins: { import: importPlugin },\n  languageOptions: { sourceType: \"module\", ecmaVersion: 2022, globals: { console: \"readonly\" } },\n  rules: { \"no-unused-vars\": \"error\", \"import/named\": \"error\" } }];\n",
    ),
    ("src/a.js", "export const foo = 1;\nexport const bar = 2;\n"),
    ("src/b.js", "import { foo } from \"./a.js\";\nconsole.log(foo);\n"),
    ("src/c.js", "const x = 1;\nexport default x;\n"),
];

#[test]
fn eslint_verdicts_equal_eslint_in_every_case() {
    let Some(t) = tools() else { return };
    let repo = Repo::new(LINT_FILES, Some(&t));
    let c0 = repo.git(&["rev-parse", "HEAD"]);
    let c = lint_case(&repo, None, "bootstrap");
    assert_eq!(c["mode"], "full");
    assert_eq!(c["verdict"], "pass");

    // a per-file finding in a file nobody imports
    repo.write("src/c.js", "const x = 1;\nconst unused = 2;\nexport default x;\n");
    let c1 = repo.commit("unused var");
    let c = lint_case(&repo, Some(&c0), "unused var");
    assert_eq!(c["mode"], "incremental", "{c:#}");
    assert_eq!(strs(&c["filesRechecked"]), vec!["src/c.js"]);
    assert_eq!(c["verdict"], "fail");

    // an export removed: the importer's import/named finding appears although
    // the importer did not change (import rules resolve imports)
    repo.write("src/c.js", "const x = 1;\nexport default x;\n");
    repo.write("src/a.js", "export const foo2 = 1;\nexport const bar = 2;\n");
    let c2 = repo.commit("export renamed");
    let c = lint_case(&repo, Some(&c1), "export renamed");
    assert_eq!(c["mode"], "incremental", "{c:#}");
    assert!(strs(&c["filesRechecked"]).contains(&"src/b.js".to_string()), "{c:#}");
    assert_eq!(c["verdict"], "fail");

    // config change -> full
    repo.write("src/a.js", "export const foo = 1;\nexport const bar = 2;\n");
    let cfg = fs::read_to_string(repo.path().join("eslint.config.js")).unwrap().replace("\"no-unused-vars\": \"error\"", "\"no-unused-vars\": \"warn\"");
    repo.write("eslint.config.js", &cfg);
    let c3 = repo.commit("config");
    let c = lint_case(&repo, Some(&c2), "config");
    assert_eq!(c["mode"], "full");
    assert!(strs(&c["reasons"]).iter().any(|r| r.starts_with("config: eslint.config.js")), "{c:#}");

    // a new file is found and linted
    repo.write("src/d.js", "const y = 3;\n");
    let c4 = repo.commit("new file");
    let c = lint_case(&repo, Some(&c3), "new file");
    assert_eq!(c["mode"], "incremental", "{c:#}");
    assert!(strs(&c["filesRechecked"]).contains(&"src/d.js".to_string()));

    // new files no config matches (docs, data) are neither linted nor a reason
    // to give up: the flat config decides which files are lint targets
    repo.write("README.md", "# lint fixture\n");
    repo.write("src/data.json", "{\"a\": 1}\n");
    let _c5 = repo.commit("non-js files");
    let c = lint_case(&repo, Some(&c4), "non-js files");
    assert_eq!(c["mode"], "incremental", "{c:#}");
    assert_eq!(c["filesRecheckedCount"], 0, "{c:#}");
}

#[test]
fn eslint_type_aware_config_forces_full() {
    let Some(t) = tools() else { return };
    let repo = Repo::new(
        &[
            ("package.json", r#"{"name":"lintfx","private":true,"type":"module"}"#),
            ("eslint.config.js", "export default [{ files: [\"**/*.js\"], languageOptions: { parserOptions: { project: true } }, rules: { \"no-unused-vars\": \"error\" } }];\n"),
            ("src/a.js", "export const foo = 1;\n"),
        ],
        Some(&t),
    );
    lint_case(&repo, None, "bootstrap");
    repo.write("src/a.js", "export const foo = 2;\n");
    let c = lint_case(&repo, None, "edit");
    assert_eq!(c["mode"], "full", "{c:#}");
    assert!(strs(&c["reasons"]).iter().any(|r| r.starts_with("type-aware")), "{c:#}");
}

// ---- tests (vitest) ------------------------------------------------------------

fn vitest_passes(repo: &Repo) -> bool {
    Command::new("node")
        .current_dir(repo.path())
        .env("CI", "true")
        .args(["node_modules/vitest/vitest.mjs", "run", "--maxWorkers=2", "--minWorkers=1"])
        .output()
        .unwrap()
        .status
        .success()
}

fn tests_case(repo: &Repo, base: Option<&str>, label: &str) -> Value {
    let mut args = vec!["--checkers", "tests"];
    if let Some(b) = base {
        args.push("--base");
        args.push(b);
    }
    let (_, v) = repo.sem(&args);
    let c = checker(&v, "tests").clone();
    let ok = vitest_passes(repo);
    assert_eq!(c["verdict"], if ok { "pass" } else { "fail" }, "[{label}] verdict differs from `vitest run`\n{c:#}");
    c
}

const VITEST_FILES: &[(&str, &str)] = &[
    ("package.json", r#"{"name":"testfx","private":true,"type":"module","scripts":{"test":"vitest"}}"#),
    (".sem/check.json", r#"{"tests":{"args":["--maxWorkers=2","--minWorkers=1"]}}"#),
    ("src/math.js", "export const add = (a, b) => a + b;\n"),
    ("src/str.js", "export const up = (s) => s.toUpperCase();\n"),
    ("test/math.test.js", "import { test, expect } from \"vitest\";\nimport { add } from \"../src/math.js\";\ntest(\"add\", () => expect(add(1, 2)).toBe(3));\n"),
    ("test/str.test.js", "import { test, expect } from \"vitest\";\nimport { up } from \"../src/str.js\";\ntest(\"up\", () => expect(up(\"a\")).toBe(\"A\"));\n"),
];

#[test]
fn affected_tests_verdicts_equal_vitest() {
    let Some(t) = tools() else { return };
    if !t.join("vitest/package.json").exists() {
        return;
    }
    let repo = Repo::new(VITEST_FILES, Some(&t));
    let c0 = repo.git(&["rev-parse", "HEAD"]);
    let c = tests_case(&repo, None, "bootstrap");
    assert_eq!(c["mode"], "full");

    repo.write("src/math.js", "export const add = (a, b) => b + a;\n");
    let c1 = repo.commit("math body");
    let c = tests_case(&repo, Some(&c0), "math body");
    assert_eq!(c["mode"], "incremental", "{c:#}");
    assert_eq!(strs(&c["filesRechecked"]), vec!["test/math.test.js"]);

    repo.write("src/str.js", "export const up = (s) => s.toLowerCase();\n");
    let c2 = repo.commit("str broken");
    let c = tests_case(&repo, Some(&c1), "str broken");
    assert_eq!(c["mode"], "incremental", "{c:#}");
    assert_eq!(strs(&c["filesRechecked"]), vec!["test/str.test.js"]);
    assert_eq!(c["verdict"], "fail");

    // a doc change runs nothing and carries the base's results (still failing)
    repo.write("README.md", "# fx\n");
    let c3 = repo.commit("docs");
    let c = tests_case(&repo, Some(&c2), "docs");
    assert_eq!(c["mode"], "incremental");
    assert_eq!(c["filesRecheckedCount"], 0);
    assert_eq!(c["verdict"], "fail");

    // package.json -> all tests
    repo.write("src/str.js", "export const up = (s) => s.toUpperCase();\n");
    repo.write("package.json", r#"{"name":"testfx","private":true,"type":"module","scripts":{"test":"vitest run"}}"#);
    let c4 = repo.commit("manifest");
    let c = tests_case(&repo, Some(&c3), "manifest");
    assert_eq!(c["mode"], "full");
    assert!(strs(&c["reasons"]).iter().any(|r| r == "runner input: package.json"), "{c:#}");
    assert_eq!(c["verdict"], "pass");

    // a state from any tree but the base is never used for tests
    repo.write("src/math.js", "export const add = (a, b) => a + b;\n");
    let c = tests_case(&repo, Some(&c0), "base without state");
    assert_eq!(c["mode"], "full", "{c:#}");
    let _ = c4;
}

// ---- generic checkers ------------------------------------------------------------

#[test]
fn configured_commands_pass_fail_and_undecided() {
    let repo = Repo::new(&[(".sem/check.json", r#"{"commands":["test -f ok.txt"]}"#)], None);
    let (code, v) = repo.sem(&["--checkers", "cmd"]);
    assert_eq!(code, 1, "{v:#}");
    assert_eq!(checker(&v, "cmd")["mode"], "full");
    repo.write("ok.txt", "");
    let (code, _) = repo.sem(&["--checkers", "cmd"]);
    assert_eq!(code, 0);
    // a checker that cannot run: could not decide
    let (code, v) = repo.sem(&["--checkers", "ts"]);
    assert_eq!(code, 2, "{v:#}");
    assert_eq!(v["verdict"], "undecided");
    let (code, _) = repo.sem(&["--checkers", "nope"]);
    assert_eq!(code, 2);
}

fn have_go() -> bool {
    Command::new("go").arg("version").output().is_ok_and(|o| o.status.success())
}

#[test]
fn go_scopes_to_affected_packages_and_matches_full() {
    if !have_go() {
        eprintln!("SKIP: no go toolchain");
        return;
    }
    let repo = Repo::new(
        &[
            ("go.mod", "module fx\n\ngo 1.21\n"),
            ("a/a.go", "package a\n\nfunc Add(x, y int) int { return x + y }\n"),
            ("a/a_test.go", "package a\n\nimport \"testing\"\n\nfunc TestAdd(t *testing.T) {\n\tif Add(1, 2) != 3 {\n\t\tt.Fatal(\"add\")\n\t}\n}\n"),
            ("b/b.go", "package b\n\nimport \"fx/a\"\n\nfunc Twice(x int) int { return a.Add(x, x) }\n"),
            ("c/c.go", "package c\n\nfunc C() int { return 1 }\n"),
        ],
        None,
    );
    let c0 = repo.git(&["rev-parse", "HEAD"]);
    let full_ok = |r: &Repo| {
        Command::new("sh").current_dir(r.path()).args(["-c", "go vet ./... && go test ./..."]).output().unwrap().status.success()
    };
    let (_, v) = repo.sem(&["--checkers", "go"]);
    let g = checker(&v, "go");
    assert_eq!(g["mode"], "full");
    assert_eq!(g["verdict"], "pass");

    repo.write("b/b.go", "package b\n\nimport \"fx/a\"\n\nfunc Twice(x int) int { return a.Add(x, x) * 1 }\n");
    let c1 = repo.commit("b");
    let (_, v) = repo.sem(&["--checkers", "go", "--base", &c0]);
    let g = checker(&v, "go");
    assert_eq!(g["mode"], "incremental", "{g:#}");
    assert_eq!(strs(&g["filesRechecked"]), vec!["fx/b"]);
    assert_eq!(g["verdict"] == "pass", full_ok(&repo));

    repo.write("a/a.go", "package a\n\nfunc Add(x, y int) int { return x - y }\n");
    let c2 = repo.commit("a broken");
    let (code, v) = repo.sem(&["--checkers", "go", "--base", &c1]);
    let g = checker(&v, "go");
    assert_eq!(g["mode"], "incremental", "{g:#}");
    assert_eq!(strs(&g["filesRechecked"]), vec!["fx/a", "fx/b"]);
    assert_eq!(code, 1);
    assert!(!full_ok(&repo));

    repo.write("go.mod", "module fx\n\ngo 1.22\n");
    repo.write("a/a.go", "package a\n\nfunc Add(x, y int) int { return x + y }\n");
    repo.commit("go.mod");
    let (_, v) = repo.sem(&["--checkers", "go", "--base", &c2]);
    let g = checker(&v, "go");
    assert_eq!(g["mode"], "full");
    assert!(strs(&g["reasons"]).iter().any(|r| r == "module input: go.mod"), "{g:#}");
    assert_eq!(g["verdict"] == "pass", full_ok(&repo));
}

#[test]
fn commands_with_inputs_carry_when_none_changed() {
    let repo = Repo::new(
        &[
            (".sem/check.json", r#"{"commands":[{"run":"test -s src/app.txt","inputs":["src/**"]},"true"]}"#),
            ("src/app.txt", "x\n"),
        ],
        None,
    );
    let (_, v) = repo.sem(&["--checkers", "cmd"]);
    assert_eq!(checker(&v, "cmd")["mode"], "full");
    repo.write("README.md", "docs\n");
    let (code, v) = repo.sem(&["--checkers", "cmd"]);
    let c = checker(&v, "cmd");
    assert_eq!((code, c["mode"].as_str()), (0, Some("incremental")), "{c:#}");
    assert_eq!(strs(&c["filesRechecked"]), vec!["true"]);
    repo.write("src/app.txt", "");
    let (code, v) = repo.sem(&["--checkers", "cmd"]);
    assert_eq!(code, 1, "{v:#}");
}

fn have(bin: &str, arg: &str) -> bool {
    Command::new(bin).arg(arg).output().is_ok_and(|o| o.status.success())
}

#[test]
fn cargo_carries_when_only_other_languages_changed() {
    let repo = Repo::new(
        &[
            ("Cargo.toml", "[package]\nname = \"fx\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"),
            ("src/main.rs", "fn main() {}\n"),
            ("web/app.ts", "export const x = 1;\n"),
        ],
        None,
    );
    repo.write(".gitignore", "node_modules\ntarget\n");
    let (_, v) = repo.sem(&["--checkers", "cargo"]);
    assert_eq!(checker(&v, "cargo")["verdict"], "pass", "{v:#}");
    repo.commit("lock");
    let (_, v) = repo.sem(&["--checkers", "cargo"]);
    assert_eq!(checker(&v, "cargo")["verdict"], "pass");
    repo.write("web/app.ts", "export const x = 2;\n");
    let (_, v) = repo.sem(&["--checkers", "cargo"]);
    assert_eq!(checker(&v, "cargo")["mode"], "incremental", "{v:#}");
    repo.write("src/main.rs", "fn main() { let x: u8 = \"no\"; }\n");
    let (code, v) = repo.sem(&["--checkers", "cargo"]);
    let c = checker(&v, "cargo");
    assert_eq!((code, c["mode"].as_str()), (1, Some("full")), "{c:#}");
    assert!(strs(&c["reasons"]).iter().any(|r| r == "input changed: src/main.rs"), "{c:#}");
}

#[test]
fn cpp_recompiles_units_that_include_a_change_and_matches_full() {
    if !have("cc", "--version") {
        eprintln!("SKIP: no C compiler");
        return;
    }
    let repo = Repo::new(
        &[
            ("inc/math.h", "int add(int a, int b);\n"),
            ("src/math.c", "#include \"math.h\"\nint add(int a, int b) { return a + b; }\n"),
            ("src/main.c", "#include \"math.h\"\nint main(void) { return add(1, 2); }\n"),
            ("src/other.c", "int other(void) { return 1; }\n"),
        ],
        None,
    );
    let db: Vec<Value> = ["math", "main", "other"]
        .iter()
        .map(|n| serde_json::json!({ "directory": repo.path(), "file": format!("src/{n}.c"), "arguments": ["cc", "-Iinc", "-c", format!("src/{n}.c"), "-o", format!("{n}.o")] }))
        .collect();
    repo.write("compile_commands.json", &serde_json::to_string(&db).unwrap());
    repo.commit("db");
    let (_, v) = repo.sem(&["--checkers", "cpp"]);
    assert_eq!(checker(&v, "cpp")["verdict"], "pass", "{v:#}");
    repo.write("src/other.c", "int other(void) { return 2; }\n");
    let (_, v) = repo.sem(&["--checkers", "cpp"]);
    let c = checker(&v, "cpp");
    assert_eq!(c["mode"], "incremental");
    assert_eq!(strs(&c["filesRechecked"]), vec!["src/other.c"]);
    repo.write("src/other.c", "int other(void) { return 1; }\n");
    repo.write("inc/math.h", "int add(int a, int b, int c);\n");
    let (code, v) = repo.sem(&["--checkers", "cpp"]);
    let c = checker(&v, "cpp").clone();
    let mut got = strs(&c["filesRechecked"]);
    got.sort();
    assert_eq!(got, vec!["src/main.c", "src/math.c"], "{c:#}");
    assert_eq!(code, 1);
    let (_, full) = repo.sem(&["--checkers", "cpp", "--full"]);
    assert_eq!(strs(&checker(&full, "cpp")["diagnostics"]), strs(&c["diagnostics"]));
}

#[test]
fn pytest_reruns_only_tests_that_can_load_a_change() {
    if !have("pytest", "--version") {
        eprintln!("SKIP: no pytest");
        return;
    }
    let repo = Repo::new(
        &[
            ("pytest.ini", "[pytest]\npythonpath = .\n"),
            ("calc.py", "def add(a, b):\n    return a + b\n"),
            ("words.py", "def shout(s):\n    return s.upper()\n"),
            ("tests/test_calc.py", "from calc import add\n\ndef test_add():\n    assert add(1, 2) == 3\n"),
            ("tests/test_words.py", "import words\n\ndef test_shout():\n    assert words.shout('a') == 'A'\n"),
        ],
        None,
    );
    repo.write(".gitignore", "node_modules\n__pycache__/\n.pytest_cache/\n");
    repo.commit("ignore");
    let (_, v) = repo.sem(&["--checkers", "pytest"]);
    assert_eq!(checker(&v, "pytest")["verdict"], "pass", "{v:#}");
    repo.write("calc.py", "def add(a, b):\n    return a - b\n");
    let (code, v) = repo.sem(&["--checkers", "pytest"]);
    let c = checker(&v, "pytest");
    assert_eq!(c["mode"], "incremental", "{c:#}");
    assert_eq!(strs(&c["filesRechecked"]), vec!["tests/test_calc.py"]);
    assert_eq!(code, 1);
}
