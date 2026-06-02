use std::path::Path;
use std::process::{Command, Output};

struct TempRepo {
    repo: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl TempRepo {
    fn new() -> Self {
        let repo = tempfile::tempdir().expect("create temp repo");
        let home = tempfile::tempdir().expect("create temp home");

        run_git(repo.path(), &["init", "-q"]);
        run_git(repo.path(), &["config", "user.name", "Test"]);
        run_git(repo.path(), &["config", "user.email", "test@example.com"]);

        Self { repo, home }
    }

    fn path(&self) -> &Path {
        self.repo.path()
    }

    fn run_sem(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .args(args)
            .current_dir(self.repo.path())
            .env("HOME", self.home.path())
            .output()
            .expect("run sem")
    }
}

fn run_git(repo: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn commit_file(repo: &TempRepo, path: &str, content: &str) {
    std::fs::write(repo.path().join(path), content).expect("write file");
    run_git(repo.path(), &["add", path]);
    run_git(repo.path(), &["commit", "-qm", "init"]);
}

#[test]
fn markdown_verbose_diff_uses_fence_longer_than_source_backticks() {
    let repo = TempRepo::new();
    commit_file(
        &repo,
        "app.py",
        "def foo():\n    s = \"plain\"\n    return s\n",
    );
    std::fs::write(
        repo.path().join("app.py"),
        "def foo():\n    s = \"X``` Y ``` Z\"\n    return s\n",
    )
    .expect("write modified file");

    let output = repo.run_sem(&["diff", "--format", "markdown", "-v"]);
    assert!(
        output.status.success(),
        "sem failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf-8");
    let lines: Vec<&str> = stdout.lines().collect();
    let opening = lines
        .iter()
        .position(|line| *line == "````diff")
        .expect("diff block should open with a 4-backtick fence");
    let closing = lines[opening + 1..]
        .iter()
        .position(|line| *line == "````")
        .map(|offset| opening + 1 + offset)
        .expect("diff block should close with a matching 4-backtick fence");

    assert!(lines[opening + 1..closing]
        .iter()
        .any(|line| line.contains("```")));
    assert!(!lines.iter().any(|line| *line == "```diff"), "{stdout}");
}

#[test]
fn terminal_verbose_diff_escapes_source_control_bytes_with_color_never() {
    let repo = TempRepo::new();
    commit_file(&repo, "app.py", "def foo():\n    return 1\n");
    std::fs::write(
        repo.path().join("app.py"),
        "def foo():\n    s = \"\u{1b}[31mRED\u{1b}[0m\"\n    return s\n",
    )
    .expect("write modified file");

    let output = repo.run_sem(&["diff", "-v", "--color", "never"]);
    assert!(
        output.status.success(),
        "sem failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !output.stdout.contains(&0x1b),
        "stdout contains raw ESC bytes: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf-8");
    assert!(stdout.contains("\\u{1b}[31mRED\\u{1b}[0m"), "{stdout}");
}

#[test]
#[cfg(not(windows))]
fn terminal_unsupported_file_warning_escapes_file_path_control_bytes() {
    let repo = TempRepo::new();
    let file_name = "bad\u{1b}[31m.txt";
    commit_file(&repo, file_name, "alpha\nbeta\n");
    std::fs::write(repo.path().join(file_name), "alpha\nchanged\n").expect("write modified file");

    let output = repo.run_sem(&["diff", "-v", "--color", "never"]);
    assert!(
        output.status.success(),
        "sem failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !output.stdout.contains(&0x1b),
        "stdout contains raw ESC bytes: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf-8");
    assert!(stdout.contains("bad\\u{1b}[31m.txt"), "{stdout}");
    assert!(
        stdout.contains("used line-based chunking (unsupported file extension)"),
        "{stdout}"
    );
}
