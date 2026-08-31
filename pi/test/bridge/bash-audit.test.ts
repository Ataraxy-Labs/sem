import { test } from "node:test";
import assert from "node:assert/strict";
import { classifyBashCommand, auditBashCommand } from "../../src/bridge/bash-audit.ts";

// Spec for the bash audit wrapper's classifier (pure, no process spawning).
// Keep the rule set small and deliberately conservative -- it only matches
// a statement's LEADING command word, never scans mid-command, so it's
// cheap to reason about and cheap to be wrong about (a false positive only
// costs one audit-log entry by default; see auditBashCommand's strict
// behavior below for the one place a false positive costs more).

test("classifyBashCommand: matches leading cat/head/tail/less/more as a read", () => {
  for (const cmd of ["cat foo.ts", "head -n 20 foo.ts", "tail -f log.txt", "less foo.ts", "more foo.ts"]) {
    const matches = classifyBashCommand(cmd);
    assert.equal(matches.length, 1, `expected exactly one match for "${cmd}"`);
    assert.equal(matches[0]?.ruleId, "read");
  }
});

test("classifyBashCommand: matches any plain sed (no -i) as a read -- without -i, sed never touches the file, only prints to stdout", () => {
  assert.equal(classifyBashCommand("sed -n '1,20p' foo.ts")[0]?.ruleId, "read");
  assert.equal(classifyBashCommand("sed 's/a/b/' foo.ts")[0]?.ruleId, "read", "sed without -i only prints the transform to stdout; the file on disk is untouched, so this is a read");
});

test("classifyBashCommand: sed WITH -i is a genuine in-place edit -- classified write, not read (ox-review-3 finding #19)", () => {
  const matches1 = classifyBashCommand("sed -i 's/a/b/' foo.ts");
  assert.equal(matches1.length, 1);
  assert.equal(matches1[0]?.ruleId, "write");
  assert.equal(matches1[0]?.path, "foo.ts");

  const matches2 = classifyBashCommand("sed -i.bak 's/a/b/' foo.ts");
  assert.equal(matches2.length, 1, "GNU-style -i with a suffix attached must still be recognized as in-place");
  assert.equal(matches2[0]?.ruleId, "write");
});

test("classifyBashCommand: sed --in-place (GNU long-form flag) is recognized as a genuine in-place edit -- classified write, not read (fixes the prior honest-label misclassification)", () => {
  // Previously only the literal `-i` short flag was recognized, so
  // `sed --in-place ...` fell through to the plain-sed READ rule --
  // actively MISCLASSIFIED, not just uncaught (neither the write rule's
  // guard nor the read rule's negated guard recognized the long form).
  // hasSedInPlaceFlag now recognizes both spellings wherever `-i` is
  // recognized.
  const matches = classifyBashCommand("sed --in-place s/a/b/ f.txt");
  assert.equal(matches.length, 1);
  assert.equal(matches[0]?.ruleId, "write");
  assert.equal(matches[0]?.path, "f.txt");

  const matchesWithSuffix = classifyBashCommand("sed --in-place=.bak s/a/b/ f.txt");
  assert.equal(matchesWithSuffix.length, 1, "the suffixed long form (--in-place=.bak) must also be recognized as in-place");
  assert.equal(matchesWithSuffix[0]?.ruleId, "write");
});

test("classifyBashCommand: matches leading awk as a read", () => {
  assert.equal(classifyBashCommand("awk '{print $1}' foo.ts")[0]?.ruleId, "read");
});

test("classifyBashCommand: matches leading grep/rg/ag as a search", () => {
  for (const cmd of ["grep -rn foo src/", "rg foo src/", "ag foo src/"]) {
    const matches = classifyBashCommand(cmd);
    assert.equal(matches.length, 1);
    assert.equal(matches[0]?.ruleId, "search");
  }
});

test("classifyBashCommand: matches leading find/fd as a find", () => {
  assert.equal(classifyBashCommand("find . -name '*.ts'")[0]?.ruleId, "find");
  assert.equal(classifyBashCommand("fd '*.ts'")[0]?.ruleId, "find");
});

test("classifyBashCommand: matches leading ls/tree as a list", () => {
  assert.equal(classifyBashCommand("ls -la src/")[0]?.ruleId, "list");
  assert.equal(classifyBashCommand("tree src/")[0]?.ruleId, "list");
});

test("classifyBashCommand: a cat heredoc WRITE is classified write (its redirect target), never read -- the heredoc body is stripped first, not just excluded from the read rule (ox-review-3 finding #18 item 7)", () => {
  const matches = classifyBashCommand("cat <<EOF > out.txt\nhello\nEOF");
  assert.equal(matches.length, 1);
  assert.equal(matches[0]?.ruleId, "write");
  assert.equal(matches[0]?.path, "out.txt");
});

test("classifyBashCommand: ordinary, unrelated commands never match (no false positives on the happy path)", () => {
  for (const cmd of ["npm test", "git status", "echo hello", "node script.js", "mkdir -p dist", "npm run build"]) {
    assert.deepEqual(classifyBashCommand(cmd), [], `expected no match for "${cmd}"`);
  }
});

test("classifyBashCommand: a compound command reports one match per matching statement", () => {
  const matches = classifyBashCommand("find . -name '*.ts' | grep foo");
  assert.equal(matches.length, 2);
  assert.deepEqual(
    matches.map((m) => m.ruleId).sort(),
    ["find", "search"],
  );
});

test("classifyBashCommand: only the matching statement in a compound command is reported, not the whole command", () => {
  const matches = classifyBashCommand("cd src && cat file.ts");
  assert.equal(matches.length, 1);
  assert.equal(matches[0]?.ruleId, "read");
  assert.match(matches[0]?.statement ?? "", /cat file\.ts/);
});

test("classifyBashCommand: each match names a suggested tool", () => {
  const [match] = classifyBashCommand("cat foo.ts");
  assert.ok(match?.suggestedTool && match.suggestedTool.length > 0);
});

test("auditBashCommand: audit-only (strict=false) never refuses, even with a match", () => {
  const decision = auditBashCommand("cat foo.ts", false);
  assert.equal(decision.matches.length, 1);
  assert.equal(decision.refuse, false);
  assert.equal(decision.refusalMessage, undefined);
});

test("auditBashCommand: strict=true refuses on a match, naming the offending statement and the suggested tool", () => {
  const decision = auditBashCommand("cat foo.ts", true);
  assert.equal(decision.refuse, true);
  assert.ok(decision.refusalMessage);
  assert.match(decision.refusalMessage, /cat foo\.ts/);
  const [firstMatch] = decision.matches;
  assert.ok(firstMatch);
  const [firstWord] = firstMatch.suggestedTool.split(" ");
  assert.ok(firstWord);
  assert.match(decision.refusalMessage, new RegExp(firstWord));
});

test("auditBashCommand: strict=true never refuses a command with no match", () => {
  const decision = auditBashCommand("npm test", true);
  assert.equal(decision.matches.length, 0);
  assert.equal(decision.refuse, false);
});

test("classifyBashCommand: an absolute-path binary is classified the same as its bare name", () => {
  assert.equal(classifyBashCommand("/bin/cat foo.ts")[0]?.ruleId, "read");
  assert.equal(classifyBashCommand("/usr/bin/grep foo src/")[0]?.ruleId, "search");
  assert.equal(classifyBashCommand("/usr/local/bin/find . -name '*.ts'")[0]?.ruleId, "find");
});

test("classifyBashCommand: an env-var-prefixed command is classified the same as the bare command", () => {
  assert.equal(classifyBashCommand("FOO=1 cat foo.ts")[0]?.ruleId, "read");
  assert.equal(classifyBashCommand("FOO=1 BAR=2 grep foo src/")[0]?.ruleId, "search");
});

test("classifyBashCommand: env prefix and absolute path combine", () => {
  assert.equal(classifyBashCommand("FOO=1 /bin/cat foo.ts")[0]?.ruleId, "read");
});

test("classifyBashCommand: an env-var-looking substring that isn't actually a leading assignment still matches its own bare-name rule (guards against a namespace collision, not a real bypass)", () => {
  // "cat" itself is unaffected by the env-prefix strip (no leading KEY=value).
  assert.equal(classifyBashCommand("cat foo.ts")[0]?.ruleId, "read");
});

test("classifyBashCommand: sh -c / bash -c / python -c / node -e are classified as indirect, regardless of PATH or absolute-path form", () => {
  for (const cmd of [
    "sh -c 'cat foo.ts'",
    "bash -c 'grep foo bar.ts'",
    "python -c 'print(1)'",
    "python3 -c 'print(1)'",
    "node -e 'console.log(1)'",
  ]) {
    const matches = classifyBashCommand(cmd);
    assert.equal(matches.length, 1, `expected exactly one match for "${cmd}"`);
    assert.equal(matches[0]?.ruleId, "indirect");
  }
});

test("classifyBashCommand: node without -e (a normal script invocation) is not classified as indirect", () => {
  assert.equal(classifyBashCommand("node script.js").length, 0);
});

test("classifyBashCommand: combined short-flag clusters containing the trigger letter are classified as indirect, same as bare -c/-e (ox-review-3)", () => {
  for (const cmd of [
    'bash -lc "cat secret.env"', // login shell + command -- the most common real-world -c form
    'sh -lc "cat secret.env"',
    'bash -ic "cat secret.env"', // interactive + command
    'bash -xc "cat secret.env"', // xtrace + command
    'zsh -lc "cat secret.env"',
    'dash -lc "cat secret.env"',
    'python3 -ic "print(1)"',
    'node -ie "console.log(1)"',
  ]) {
    const matches = classifyBashCommand(cmd);
    assert.equal(matches.length, 1, `expected exactly one match for "${cmd}"`);
    assert.equal(matches[0]?.ruleId, "indirect");
  }
});

test("classifyBashCommand: a flag cluster with no trigger letter is not classified as indirect", () => {
  assert.equal(classifyBashCommand("bash -x script.sh").length, 0);
  assert.equal(classifyBashCommand("sh -l script.sh").length, 0);
});

test("classifyBashCommand: the trigger flag split across SEPARATE tokens is now caught, same rule family as the combined cluster", () => {
  // bash -lc "..." (combined, one token) was already caught; bash -l -c
  // "..." (space-separated, two tokens) was a documented gap -- now closed
  // by scanning the leading RUN of flag tokens for the trigger letter,
  // stopping at the first non-flag token.
  for (const cmd of ['bash -l -c "cat secret.env"', 'sh -e -c "cat secret.env"']) {
    const matches = classifyBashCommand(cmd);
    assert.equal(matches.length, 1, `expected exactly one match for "${cmd}"`);
    assert.equal(matches[0]?.ruleId, "indirect");
  }
});

test("classifyBashCommand: a $(...) or backtick substitution containing a read verb is classified as indirect", () => {
  assert.equal(classifyBashCommand("echo $(cat secret.txt)")[0]?.ruleId, "indirect");
  assert.equal(classifyBashCommand("echo `cat secret.txt`")[0]?.ruleId, "indirect");
  assert.equal(classifyBashCommand("VAR=$(grep foo bar.ts)").length, 1, "env-assignment RHS substitution is still inspected");
});

test("classifyBashCommand: a $(...) substitution with no read verb inside does not match", () => {
  assert.equal(classifyBashCommand("echo $(date)").length, 0);
  assert.equal(classifyBashCommand("echo $(npm test)").length, 0);
});

test("classifyBashCommand: leading-command classification takes priority over a substitution match in the same statement", () => {
  // cat itself is the leading command AND there happens to be a nested
  // substitution reference -- only one match should be reported.
  const matches = classifyBashCommand("cat $(echo foo.ts)");
  assert.equal(matches.length, 1);
  assert.equal(matches[0]?.ruleId, "read");
});

// --- ox-review-3 finding #18 item 7: heredoc/here-string stripping
// (previously a blanket "skip the whole rule if this statement contains
// <<" guard turned appending `<<< x` into a universal evasion suffix for
// the read rule specifically) ---

test("classifyBashCommand: a here-string no longer evades the read rule", () => {
  for (const cmd of ["head -100 file.ts <<< x", "cat foo.ts <<< x", "tail -5 log.txt <<< ''"]) {
    const matches = classifyBashCommand(cmd);
    assert.equal(matches.length, 1, `expected a match for "${cmd}"`);
    assert.equal(matches[0]?.ruleId, "read");
  }
});

test("classifyBashCommand: a here-string never mattered for rules with no heredoc guard (search), confirming it was never a UNIVERSAL evasion -- only the read rule's own guard was the hole", () => {
  const matches = classifyBashCommand("rg secrets . <<< ''");
  assert.equal(matches.length, 1);
  assert.equal(matches[0]?.ruleId, "search");
});

test("classifyBashCommand: a heredoc's own delimiter word doesn't leak into the classified statement", () => {
  const matches = classifyBashCommand("cat foo.ts <<EOF\nignored body text\nEOF");
  // No redirect target here (no `>`), so this is read: EOF/the body never
  // appear in the reported statement, and the body's own content (which
  // could coincidentally contain read/write verbs as plain text) is never
  // itself classified as a separate statement.
  assert.equal(matches.length, 1);
  assert.equal(matches[0]?.ruleId, "read");
  assert.ok(!matches[0]?.statement.includes("EOF"));
  assert.ok(!matches[0]?.statement.includes("ignored body"));
});

test("classifyBashCommand: <<- (indented heredoc) and <<~ (shell-comment-stripping heredoc) are both stripped", () => {
  for (const marker of ["<<-", "<<~"]) {
    const matches = classifyBashCommand(`cat ${marker}EOF > out.txt\n\tbody\n\tEOF`);
    assert.equal(matches.length, 1, `expected a match for marker "${marker}"`);
    assert.equal(matches[0]?.ruleId, "write");
    assert.equal(matches[0]?.path, "out.txt");
  }
});

test("classifyBashCommand: a quoted heredoc delimiter ('EOF' or \"EOF\") is recognized the same as a bare one", () => {
  for (const quote of ["'", '"']) {
    const matches = classifyBashCommand(`cat <<${quote}EOF${quote} > out.txt\nbody\nEOF`);
    assert.equal(matches.length, 1, `expected a match for quote ${quote}`);
    assert.equal(matches[0]?.ruleId, "write");
  }
});

// --- ox-review-3 finding #19: bash-side writes were never classified at
// all, so PI_SEM_STRICT's write protection was fully bypassable through
// bash (sed -i, echo/redirect, tee, cp, mv, truncate) ---

test("classifyBashCommand: a redirect (> or >>) is classified write, with the redirect target as path, regardless of the leading command", () => {
  for (const cmd of ["echo hi > out.ts", "echo hi >> out.ts", "printf x > out.ts", "some-random-cmd arg > out.ts"]) {
    const matches = classifyBashCommand(cmd);
    assert.equal(matches.length, 1, `expected a match for "${cmd}"`);
    assert.equal(matches[0]?.ruleId, "write");
    assert.equal(matches[0]?.path, "out.ts");
  }
});

test("classifyBashCommand: tee/cp/mv/truncate are classified write, with a best-effort target path", () => {
  assert.deepEqual(
    classifyBashCommand("tee out.ts").map((m) => [m.ruleId, m.path]),
    [["write", "out.ts"]],
  );
  assert.deepEqual(
    classifyBashCommand("cp src.ts dst.ts").map((m) => [m.ruleId, m.path]),
    [["write", "dst.ts"]],
  );
  assert.deepEqual(
    classifyBashCommand("mv src.ts dst.ts").map((m) => [m.ruleId, m.path]),
    [["write", "dst.ts"]],
  );
  assert.deepEqual(
    classifyBashCommand("truncate -s 0 out.ts").map((m) => [m.ruleId, m.path]),
    [["write", "out.ts"]],
  );
});

test("classifyBashCommand: write matches record isCodeFile the same way write-audit.ts's own classifier does", () => {
  assert.equal(classifyBashCommand("echo x > out.ts")[0]?.isCodeFile, true);
  assert.equal(classifyBashCommand("echo x > README.md")[0]?.isCodeFile, false);
});

test("classifyBashCommand: 2>&1 / 1>&2 fd duplication is NOT classified as a write -- no real file target", () => {
  assert.equal(classifyBashCommand("npm test 2>&1").length, 0);
  assert.equal(classifyBashCommand("some-cmd 1>&2").length, 0);
});

test("classifyBashCommand: &> (redirect both streams) IS classified write -- it does name a real file target", () => {
  const matches = classifyBashCommand("some-cmd &> out.txt");
  assert.equal(matches.length, 1);
  assert.equal(matches[0]?.ruleId, "write");
  assert.equal(matches[0]?.path, "out.txt");
});

test("classifyBashCommand: -> (an arrow, e.g. a Rust closure or TS generic bound) is NOT mistaken for a redirect", () => {
  assert.equal(classifyBashCommand("cat foo.rs").length, 1); // sanity: cat itself still matches
  assert.equal(classifyBashCommand("rg 'fn foo() -> Result<()>' src/").length, 1); // matches as search, not write
  assert.equal(classifyBashCommand("rg 'fn foo() -> Result<()>' src/")[0]?.ruleId, "search");
});

test("auditBashCommand: with a resolveTargetExists resolver, strict mode refuses a write to an EXISTING code file, reusing write-audit.ts's own refusal text", () => {
  const decision = auditBashCommand("sed -i 's/a/b/' src/existing.ts", true, () => true);
  assert.equal(decision.refuse, true);
  assert.ok(decision.refusalMessage);
  assert.match(decision.refusalMessage, /use weave_edit for existing entities; write is for new files/);
});

test("auditBashCommand: with a resolver, strict mode never refuses a write to a NEW file", () => {
  const decision = auditBashCommand("echo x > src/new.ts", true, () => false);
  assert.equal(decision.refuse, false);
});

test("auditBashCommand: with a resolver, strict mode never refuses a write to an existing NON-code file", () => {
  const decision = auditBashCommand("echo x > README.md", true, () => true);
  assert.equal(decision.refuse, false);
});

test("auditBashCommand: WITHOUT a resolver (the default, matching every pre-existing call site), a write match is audit-only even under strict -- never auto-refuses without fs access to check existence", () => {
  const decision = auditBashCommand("sed -i 's/a/b/' src/existing.ts", true);
  assert.equal(decision.matches[0]?.ruleId, "write");
  assert.equal(decision.refuse, false);
});

test("auditBashCommand: a write match with no extractable path is audit-only, never refuses (can't determine risk without a target)", () => {
  // truncate with only a flag and no path token left after filtering --
  // contrived, but exercises the "path stays undefined" branch honestly.
  const decision = auditBashCommand("truncate --help", true, () => true);
  if (decision.matches[0]?.ruleId === "write") {
    assert.equal(decision.matches[0]?.path, undefined);
    assert.equal(decision.refuse, false);
  }
});

// --- Full evasion matrix, passes 2-3 combined: every case ox's reviews
// raised, re-verified together in one table so "what's caught" and "what's
// documented-uncaught" can't silently drift apart from each other or from
// the module header's own gap list. ---

test("evasion matrix: passes 2-3, caught vs documented-uncaught, re-verified together", () => {
  const CAUGHT: Array<{ label: string; cmd: string; ruleId: string }> = [
    { label: "absolute-path binary", cmd: "/bin/cat foo.ts", ruleId: "read" },
    { label: "env-var-prefixed command", cmd: "FOO=1 cat foo.ts", ruleId: "read" },
    { label: "combined short-flag cluster (bash -lc)", cmd: 'bash -lc "cat secret.env"', ruleId: "indirect" },
    { label: "trigger flag split across separate tokens (bash -l -c)", cmd: 'bash -l -c "cat secret.env"', ruleId: "indirect" },
    { label: "a long-form flag ahead of -c that doesn't itself break the leading-flag scan", cmd: 'bash --posix -c "cat secret.env"', ruleId: "indirect" },
    { label: "sed --in-place (GNU long-form flag, no longer misclassified as read)", cmd: "sed --in-place s/a/b/ foo.ts", ruleId: "write" },
    { label: "$(...) substitution containing a read verb", cmd: "echo $(cat secret.txt)", ruleId: "indirect" },
    { label: "plain sed without -i (prints to stdout, a read)", cmd: "sed 's/a/b/' foo.ts", ruleId: "read" },
    { label: "sed WITH -i (genuine in-place edit)", cmd: "sed -i 's/a/b/' foo.ts", ruleId: "write" },
    { label: "redirect > (write)", cmd: "echo x > out.ts", ruleId: "write" },
    { label: "redirect >> (append write)", cmd: "echo x >> out.ts", ruleId: "write" },
    { label: "tee (write)", cmd: "tee out.ts", ruleId: "write" },
    { label: "cp destination (write)", cmd: "cp a.ts b.ts", ruleId: "write" },
    { label: "mv destination (write)", cmd: "mv a.ts b.ts", ruleId: "write" },
    { label: "truncate (write)", cmd: "truncate -s0 out.ts", ruleId: "write" },
    { label: "here-string no longer evades the read rule", cmd: "cat foo.ts <<< x", ruleId: "read" },
    { label: "heredoc-with-redirect correctly write, not evaded", cmd: "cat <<EOF > out.txt\nbody\nEOF", ruleId: "write" },
  ];

  const UNCAUGHT: Array<{ label: string; cmd: string }> = [
    // NOTE on this list's own accuracy: several cases originally guessed
    // here turned out NOT to be zero-match when actually run, and moved
    // elsewhere as they were fixed or found to be actively wrong rather
    // than simply uncaught -- verified empirically before being asserted,
    // not assumed from prose:
    //  - nested substitution using cat as the innermost verb: the
    //    substitution regex still finds an unnested INNER $(...) by
    //    retrying at later string positions (a nested case only stays
    //    uncaught when NEITHER level names a recognized verb)
    //  - `find -exec cat`: matches on its own leading word regardless of
    //    what -exec runs
    //  - `sed --in-place`: was worse than uncaught (MISCLASSIFIED as read);
    //    now fixed (CAUGHT, see above) and no longer even a gap
    //  - `bash -l -c` / `bash --posix -c`: the flag-split fix now catches
    //    both (CAUGHT, see above) -- any leading run of `-`-prefixed
    //    tokens is scanned for the trigger letter, not just a single
    //    combined cluster
    { label: "nested command substitution where NEITHER level names a recognized verb", cmd: "$(basename $(dirname x))" },
    { label: "a variable holding a command name", cmd: "CMD=cat; $CMD f.txt" },
    { label: "xargs invoking a read verb", cmd: "xargs cat" },
    { label: "xargs invoking a read verb, piped in", cmd: "echo f.txt | xargs cat" },
    { label: "a plain script file run without -c/-e", cmd: "python3 script.py" },
    { label: "env-based indirection", cmd: "env cat f.txt" },
    { label: "a flag that consumes a separate-token value ahead of the trigger flag", cmd: 'node -r foo -e "1"' },
    { label: "a backslash-escaped command", cmd: "\\cat f.txt" },
    { label: "the command/builtin prefix", cmd: "command cat f.txt" },
    { label: "bare & backgrounding as a statement separator", cmd: "sleep 1 & cat secret.txt" },
    { label: "subshell parens", cmd: "( cat f.txt )" },
    { label: "piping into a second-stage shell", cmd: "echo 'cat f.txt' | sh" },
    { label: "decode-then-exec chain", cmd: "base64 -d p.b64 | sh" },
  ];

  const failures: string[] = [];
  for (const { label, cmd, ruleId } of CAUGHT) {
    const matches = classifyBashCommand(cmd);
    if (matches.length === 0) failures.push(`CAUGHT case "${label}" (${JSON.stringify(cmd)}) unexpectedly matched nothing`);
    else if (matches[0]?.ruleId !== ruleId) failures.push(`CAUGHT case "${label}" matched ruleId ${matches[0]?.ruleId}, expected ${ruleId}`);
  }
  for (const { label, cmd } of UNCAUGHT) {
    const matches = classifyBashCommand(cmd);
    if (matches.length > 0) failures.push(`UNCAUGHT case "${label}" (${JSON.stringify(cmd)}) unexpectedly matched ${JSON.stringify(matches)} -- this gap may have been silently closed; update the module header's disclosed-gaps list if so`);
  }
  assert.deepEqual(failures, [], `evasion matrix drifted:\n${failures.join("\n")}`);
});
